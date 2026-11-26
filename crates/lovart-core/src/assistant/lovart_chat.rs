//! Lovart 自有通道 —— 对话的**唯一**上游实现（`docs/plan/23`）。
//!
//! ── 为什么这里只有协议适配 ────────────────────────────────────────────
//! 组装 prompt、判失败、落盘都在 `assistant::send`；本文件只做"把一段文字和几张图
//! 变成一段回复"：设模式 → 上传附件 → 提交 → 轮询 → 取 `items[].text`。
//! 上层拿到的错误只有两类，判据是**拿到 `thread_id` 没有**（不是 2xx）：
//! `NotSent`（没提交上，可重试）/ `Accepted`（提交上了，钱可能花了，禁重试）。
//!
//! ── project 归对话独有（用户裁定 2026-11-26）──────────────────────────
//! 「各归各的，图片归图片，chat 归 chat」⇒ 自己持 [`ProjectCache`]，
//! 缓存方式与出图**逐字一致**（进程内、按 `accessKey`、不落盘）。

use std::path::Path;
use std::sync::atomic::AtomicUsize;

use serde_json::Value;

use crate::lovart::client::{self, ClientDeps, Mode};
use crate::lovart::contract::SendInput;
use crate::lovart::project::{self, ProjectCache};
use crate::lovart::task::{self, AutoConfirmDecision};

use super::{ChatCompletion, CompleteError, UpstreamKind};

/// 生产实现：与出图**同一个网关、同一套 AK/SK**，但走对话语义。
pub struct LovartCompletion {
    deps: ClientDeps,
    /// 对话专用 project（不与 `engine::scheduler::RealPorts` 共享）。
    project: ProjectCache,
}

impl LovartCompletion {
    pub fn new(deps: ClientDeps) -> Self {
        Self {
            deps,
            project: ProjectCache::new(),
        }
    }

    /// 附件 = 本机路径 → 逐个上传 CDN，拿 URL。任一张读不到/传不上 ⇒ 未提交。
    fn upload_all(&self, paths: &[String]) -> Result<Vec<String>, CompleteError> {
        let mut urls = Vec::with_capacity(paths.len());
        for path in paths {
            let bytes = std::fs::read(path).map_err(|error| CompleteError::NotSent {
                kind: UpstreamKind::Io,
                why: format!("读不了要上传的图 {path}：{error}"),
            })?;
            let name = Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("image.png")
                .to_string();
            let url = client::upload_file(&self.deps, &bytes, &name).map_err(|error| {
                CompleteError::NotSent {
                    kind: UpstreamKind::Io,
                    why: format!("上传 {name} 失败：{error}"),
                }
            })?;
            urls.push(url);
        }
        Ok(urls)
    }
}

impl ChatCompletion for LovartCompletion {
    fn complete(&self, prompt: &str, attachments: &[String]) -> Result<String, CompleteError> {
        // 与出图同一语义：Fast = 扣积分（`Mode::from_spend(true)` 的唯一映射）。
        // 服务端是账号级持久设置 ⇒ 每次提交前都要设。
        client::set_mode(&self.deps, Mode::from_spend(true)).map_err(|error| {
            CompleteError::NotSent {
                kind: UpstreamKind::Io,
                why: format!("设不了生成模式：{error}"),
            }
        })?;

        let urls = self.upload_all(attachments)?;

        let input = SendInput {
            prompt: prompt.to_string(),
            project_id: String::new(), // `send_chat_with_project` 会填
            attachments: if urls.is_empty() { None } else { Some(urls) },
            // CHAT 类别恒不下发 tool_config（上游只收 IMAGE / VIDEO 的偏好类别）
            tool_config: None,
        };

        let (thread_id, _project_id) =
            project::send_chat_with_project(&self.project, &self.deps, &input).map_err(|error| {
                CompleteError::NotSent {
                    kind: UpstreamKind::Http,
                    why: format!("提交失败：{error}"),
                }
            })?;

        // ⚠️ 判据就在这里：**没拿到 thread_id = 未提交**（没提交上就不该扣钱）。
        let thread_id = require_thread_id(thread_id)?;

        // 从这里开始已经提交：任何失败都只能是 `Accepted`（钱可能花了）。
        let confirmed = AtomicUsize::new(0);
        let result = task::poll_thread(
            &self.deps,
            &thread_id,
            &|| AutoConfirmDecision::Disabled, // 对话没有工具，不该出现确认；真出现就报错，不静默扣
            &confirmed,
        )
        // ⚠️ 这里**不猜**失败种类：`poll_thread` 的 Err 可能是超时、网络、也可能是
        // `pending_confirmation`（需人工确认，任务根本没继续跑）。把它们统一说成
        // "超时"就是让文案撒谎；统一归到 `Envelope`（协议层没走完），原文放进 `why`。
        .map_err(|error| CompleteError::Accepted {
            kind: UpstreamKind::Envelope,
            why: format!("等待回复失败（thread={thread_id}）：{error}"),
        })?;

        let text = extract_text(&result);
        if text.trim().is_empty() {
            return Err(CompleteError::Accepted {
                kind: UpstreamKind::Envelope,
                why: format!("回复里没有文本：{}", snapshot(&result, 300)),
            });
        }
        Ok(text)
    }
}

/// **判据本体**：拿到 `thread_id` 才算提交成功（拿到之后的一切失败都不能重试）。
///
/// 单独成函数是为了让"提交边界"可单测、可探针注入 —— 它决定"钱花没花"，
/// 不能埋在一个 200 行的 `complete` 中间。
fn require_thread_id(thread_id: String) -> Result<String, CompleteError> {
    if thread_id.is_empty() {
        return Err(CompleteError::NotSent {
            kind: UpstreamKind::Envelope,
            why: "上游没返回 thread_id".to_string(),
        });
    }
    Ok(thread_id)
}

/// 回复正文 = `data.items[].text` 拼接（与参照实现 `extractLovartText` 一致）。
///
/// 缺失 `items` 或全无 `text` ⇒ 空串（**由调用方判空并报错**，这里不编兜底文案）。
fn extract_text(result: &Value) -> String {
    result
        .as_object()
        .and_then(|object| object.get("items"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// 上游原文快照（截断），只为让错误里看得见"上游到底回了什么"。
fn snapshot(value: &Value, max: usize) -> String {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "<不可序列化>".to_string());
    if text.chars().count() <= max {
        text
    } else {
        text.chars().take(max).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 取文本_拼接全部_items() {
        let result = json!({ "items": [{ "text": "第一段" }, { "text": "第二段" }] });
        assert_eq!(extract_text(&result), "第一段第二段");
    }

    #[test]
    fn 提交无_thread_id_算未发出() {
        let error = require_thread_id(String::new()).expect_err("空 thread_id 必须算未提交");
        assert!(matches!(error, CompleteError::NotSent { .. }));
        assert_eq!(require_thread_id("t-1".into()).unwrap(), "t-1");
    }

    #[test]
    fn 取文本_没有_items_返回空串由上层判错() {
        assert_eq!(extract_text(&json!({})), "");
        assert_eq!(extract_text(&json!({ "items": [{}, { "text": null }] })), "");
    }
}
