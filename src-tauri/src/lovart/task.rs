//! `lovart_task` — 轮询状态机 + 产物抽取（对应 TS `lovart/lovart_task.ts`）。
//!
//! ── 排队场景的三个必备条件 ──────────────────────────────────────────
//! 1. **超时分层**：总超时用 `deps.task_timeout`（90 分钟），不是 180 秒；
//! 2. **轮询容错**：瞬态错误（网络 / 408 / 429 / 5xx）**不冒泡**，退避后继续下一轮；
//!    只有确定性硬失败（400/401/403/404）才立刻判失败并透传上游文案；
//! 3. **间隔退避**：10s → 20s → 40s → 60s 封顶。长任务若固定 3 秒轮询会打爆请求量触发限流。
//!
//! 状态机：`done` → 取 result；`pending_confirmation` → 由调用方给的 `allow` 三态决定
//! （允许 / 未开启 / 本批已用过），允许则确认并继续，否则抛错；`abort` → 失败。
//!
//! ── 自动确认为什么在这里也要"一次性" ────────────────────────────────
//! 协议层只保证**同一次轮询调用内**不会反复确认（`allow` 背后的许可集合覆盖了这一点）；
//! 跨调用（超时复查 / 重试）由调度层的 `AutoConfirm` 集合保证。两层都不许省。
//!
//! ── Rust 比 TS 强的一处 ─────────────────────────────────────────────
//! **取消是醒着的**：TS 的 `sleep(interval)` 最长睡 60 秒，Ctrl+C 要等它睡完才响应；
//! 这里 [`sleep_or_abort`] 切成 ≤200ms 的小段，每段查一次取消标志
//! ⇒ 按 Ctrl+C 后最多 200ms 就抛出 `Abort`。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::sleep;
use std::time::Duration;

use serde_json::Value;

use super::client::{confirm_thread, get_result, get_status, ClientDeps};
use super::contract::{as_json_array, as_string, is_truthy};
use super::errors::{classify_error, is_retryable_error, ErrorClass, ErrType, LovartError};

/// 「此刻能自动确认吗」的三种答案。
///
/// 为什么不是 `bool`：`false` 有两种完全不同的原因（开关关 / 本批已用过），
/// 对用户是两句不同的话；压成 bool 就只能写一句含糊的兜底话。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoConfirmDecision {
    /// 开关开 && 本任务本批首次（返回时许可已占位）。
    Allow,
    /// 开关关。
    Disabled,
    /// 本任务本批已经用过许可。
    AlreadyUsed,
}

/// 处理一次 `pending_confirmation` —— **两处分支共用的唯一实现**。
///
/// `Ok(())` = 已确认，调用方应 `continue` 继续轮询；`Err` = 转人工。
/// 计数在 `confirm_thread` 返回成功的那一刻落，**不经过返回值** ——
/// 轮询后面超时 / 取消 / 报错都不会把这笔账弄丢。
fn resolve_pending(
    deps: &ClientDeps,
    thread_id: &str,
    allow: &dyn Fn() -> AutoConfirmDecision,
    confirmed: &AtomicUsize,
) -> Result<(), LovartError> {
    match allow() {
        // ⚠️ 这句文案是**黄金用例的一部分**（`fixtures/*/expected-state.jsonl`），
        //    默认关闭路径必须逐字节不变 —— 改它就得连黄金一起手改（README 有说明）。
        AutoConfirmDecision::Disabled => Err(LovartError::internal(
            "任务需人工确认（pending_confirmation），本程序不自动确认以免静默扣积分",
            ErrType::PendingConfirmation,
        )),
        // 安全阀：确认过还继续要确认 = 继续加钱的信号。手动再确认会**再扣一次**，必须说清。
        AutoConfirmDecision::AlreadyUsed => Err(LovartError::internal(
            "已自动确认过一次，上游仍要求确认；手动继续会再次扣积分",
            ErrType::PendingConfirmation,
        )),
        AutoConfirmDecision::Allow => match confirm_thread(deps, thread_id) {
            Ok(()) => {
                confirmed.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            // ⚠️ 确认失败**绝不重试**：请求可能已在服务端生效，重试 = 重复扣费风险。
            Err(error) => Err(LovartError::internal(
                format!(
                    "自动确认失败（{}），已转人工，不会重复确认",
                    error.message()
                ),
                ErrType::PendingConfirmation,
            )),
        },
    }
}

/// 取消检查粒度：睡这么久就醒一次看有没有被取消。
const CANCEL_POLL_MS: u64 = 200;

/// 取上游状态字符串。
///
/// ⚠️ **刻意不兜底**：以前是 `|| 'running'` —— 上游返回不认识的状态（字段缺失 / 改名 /
/// 非字符串）时被当成「还在跑」，于是一路轮询到 90 分钟超时，
/// **失败被伪装成进行中**（DEBT TD-02-01）。
///
/// 现在缺字段就抛。调用点在 `get_status` 的 `Result` **之外**，
/// 所以会直接冒到调度层按「无法判定」诚实判失败，不会静默续跑。
fn status_of(value: &Value) -> Result<String, LovartError> {
    let raw = value
        .as_object()
        .and_then(|object| object.get("status"))
        .map(as_string)
        .unwrap_or("");
    if raw.is_empty() {
        return Err(LovartError::internal(
            format!(
                "上游响应缺少 status 字段，无法判定任务状态：{}",
                truncate(&value.to_string(), 200)
            ),
            ErrType::Upstream,
        ));
    }
    Ok(raw.to_string())
}

fn truncate(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// 是否「值得再试」。
///
/// **用户主动取消不算** —— 那种情况必须立刻结束，不能重试。
/// 判定本身收口在 [`is_retryable_error`]（原先 TS 里这条规则有三份实现，DEBT TD-02-05）。
fn is_transient(error: &LovartError, deps: &ClientDeps) -> bool {
    if deps.is_cancelled() {
        return false;
    }
    is_retryable_error(error)
}

/// 可被取消打断的睡眠。
///
/// 切成 ≤[`CANCEL_POLL_MS`] 的小段，每段查一次取消标志 —— 见文件头「Rust 比 TS 强的一处」。
fn sleep_or_abort(deps: &ClientDeps, total: Duration) -> Result<(), LovartError> {
    let mut remaining = total;
    let step = Duration::from_millis(CANCEL_POLL_MS);
    while remaining > Duration::ZERO {
        if deps.is_cancelled() {
            return Err(LovartError::internal("已取消", ErrType::Abort));
        }
        let chunk = remaining.min(step);
        sleep(chunk);
        remaining -= chunk;
    }
    Ok(())
}

/// 退避一次（翻倍 + 封顶 + 可取消睡眠）。
fn backoff(
    deps: &ClientDeps,
    interval: &mut Duration,
    max_interval: Duration,
) -> Result<(), LovartError> {
    *interval = (*interval * 2).min(max_interval);
    sleep_or_abort(deps, *interval)
}

/// 轮询直到终态；返回 result（已含 items）。
///
/// `allow`：**在"即将确认"那一刻**求值（不是轮询开始时快照）⇒ 开关热更新双向即时；
/// `confirmed`：确认成功时 +1（由调用方提供，计数与轮询收场无关）。
pub fn poll_thread(
    deps: &ClientDeps,
    thread_id: &str,
    allow: &dyn Fn() -> AutoConfirmDecision,
    confirmed: &AtomicUsize,
) -> Result<Value, LovartError> {
    // 运行层防御：配置层已经把倒挂抬平，但 `ClientDeps` 是 pub，测试/未来调用方
    // 可能直接构造出 `max < interval` ⇒ 退避永远不增长（`09` 契约 N12）。
    let max_interval = deps.poll_max_interval.max(deps.poll_interval);
    let deadline = std::time::Instant::now() + deps.task_timeout;
    let mut interval = deps.poll_interval;

    while std::time::Instant::now() < deadline {
        let status = match get_status(deps, thread_id) {
            Ok(value) => value,
            Err(error) => {
                // 429 必须冒到调度层开全局熔断，不能在这里自己退避。
                if classify_error(&error) == ErrorClass::RateLimit {
                    return Err(error);
                }
                // 排队任务要轮询 1 小时以上，网络抖动是常态：
                // 瞬态错误退避后继续，**不结束任务**。
                if !is_transient(&error, deps) {
                    return Err(error);
                }
                backoff(deps, &mut interval, max_interval)?;
                continue;
            }
        };

        // ⚠️ 在 `Result` 之外判 —— 缺字段必须冒到调度层，不能在这里被当成瞬态吃掉。
        let state = status_of(&status)?;

        match state.as_str() {
            "abort" => {
                return Err(LovartError::internal("Lovart 任务被中止", ErrType::Abort));
            }
            "pending_confirmation" => {
                resolve_pending(deps, thread_id, allow, confirmed)?;
                continue;
            }
            "done" => {
                // 复核，防子任务未起跑误判 done。
                sleep_or_abort(deps, deps.done_recheck)?;
                let second = match get_status(deps, thread_id) {
                    Ok(value) => value,
                    Err(error) => {
                        if classify_error(&error) == ErrorClass::RateLimit {
                            return Err(error);
                        }
                        if !is_transient(&error, deps) {
                            return Err(error);
                        }
                        backoff(deps, &mut interval, max_interval)?;
                        continue;
                    }
                };
                let second_state = status_of(&second)?;
                if second_state == "done" || second_state == "abort" {
                    let result = match get_result(deps, thread_id) {
                        Ok(value) => value,
                        Err(error) => {
                            if classify_error(&error) == ErrorClass::RateLimit {
                                return Err(error);
                            }
                            if !is_transient(&error, deps) {
                                return Err(error);
                            }
                            backoff(deps, &mut interval, max_interval)?;
                            continue;
                        }
                    };
                    // ⚠️ 判据是 **JS 真值**（对齐 TS `if (result?.pending_confirmation)`），
                    //    **不是** `as_bool()` —— 上游这个字段是**对象**（官方样例里带花费估算）。
                    //    `as_bool()` 对对象返回 `false` ⇒ 会当成「已完成」返回、
                    //    **确认闸门被绕过**，任务错落成「未产出」（2026-10-01 查出的漏判）。
                    let pending = result
                        .as_object()
                        .and_then(|object| object.get("pending_confirmation"))
                        .is_some_and(is_truthy);
                    if pending {
                        resolve_pending(deps, thread_id, allow, confirmed)?;
                        continue;
                    }
                    return Ok(result);
                }
                // 子任务仍在跑，继续轮询
            }
            _ => {}
        }

        backoff(deps, &mut interval, max_interval)?;
    }

    // ⚠️ 文案**逐字对齐 TS**（含「按 pending 处理」这个说法）—— 它是 `state.jsonl` 里
    //    用户可见的 `error` 字段，改了就是改了黄金对账的口径。
    //    文案本身与实际状态（`timeout`，占槽待复查）不符 —— 这是**已知的债**；
    //    要改必须**连黄金用例一起改**（黄金已不能自动生成，见 README），不能单边顺手改。
    Err(LovartError::internal(
        "Lovart 任务轮询超时（任务可能仍在排队/生成中，按 pending 处理，稍后复查）",
        ErrType::Timeout,
    ))
}

/// 抽取产物 URL 列表（多 artifacts 归一并去重）；无产物抛 `NoArtifact`。
pub fn extract_artifacts(result: &Value) -> Result<Vec<String>, LovartError> {
    let mut urls: Vec<String> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    let items = result
        .as_object()
        .and_then(|object| object.get("items"))
        .map(as_json_array)
        .unwrap_or(&[]);
    for item in items {
        let artifacts = item
            .as_object()
            .and_then(|object| object.get("artifacts"))
            .map(as_json_array)
            .unwrap_or(&[]);
        for artifact in artifacts {
            let content = artifact
                .as_object()
                .and_then(|object| object.get("content"))
                .map(as_string)
                .unwrap_or("");
            if !content.is_empty() && seen.insert(content.to_string()) {
                urls.push(content.to_string());
            }
        }
    }

    if urls.is_empty() {
        // 错误信息按「上游说了什么」优先：失败原因 > agent 说明 > 兜底文案。
        // 上游常常给了原因而我们只报「没产出任何素材」，那等于把线索丢了。
        let reason = extract_failure_text(result);
        let agent = extract_assistant_text(result);
        let message = if !reason.is_empty() {
            reason
        } else if !agent.is_empty() {
            agent
        } else {
            "生成完成但未产出任何素材（上游未给出原因）".to_string()
        };
        return Err(LovartError::internal(message, ErrType::NoArtifact));
    }
    Ok(urls)
}

/// 抽取 Lovart Agent 的说明性文本（`items[].text` 非空项，空行连接）。
pub fn extract_assistant_text(result: &Value) -> String {
    let items = result
        .as_object()
        .and_then(|object| object.get("items"))
        .map(as_json_array)
        .unwrap_or(&[]);
    let texts: Vec<String> = items
        .iter()
        .filter_map(|item| item.as_object())
        .filter_map(|object| object.get("text"))
        .map(as_string)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .collect();
    texts.join("\n\n")
}

/// 抽取上游工具失败原因（`failures[].message`）——「上游为什么没产出」的原话。
pub fn extract_failure_text(result: &Value) -> String {
    let failures = result
        .as_object()
        .and_then(|object| object.get("failures"))
        .map(as_json_array)
        .unwrap_or(&[]);
    let mut messages: Vec<String> = Vec::new();
    for failure in failures {
        let message = failure
            .as_object()
            .and_then(|object| object.get("message"))
            .map(as_string)
            .unwrap_or("")
            .trim();
        if !message.is_empty() && !messages.iter().any(|existing| existing == message) {
            messages.push(message.to_string());
        }
    }
    messages.join("\n")
}

// 说明：TS 侧曾导出 `snapshotLovartResult` 与 `extractLovartChatText`，二者**从未被调用**。
//   - 前者：「无产物」的错误信息已由 failure_text / assistant_text 拼好，不需要原始 JSON 快照；
//   - 后者：只为 CHAT 模型服务，而本工具只出图，该场景不存在。
// 已删除（DEBT TD-02-05：幽灵预留 = 声明了却没人用的假能力）。
// **Rust 侧同样不建这两个** —— 移植不是"把 TS 的每个符号都搬过来"。
