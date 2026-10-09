//! 请求体契约与安全取值 —— 对应 TS 的 `lovart/lovart_contract.ts`。
//!
//! Lovart 原生 send 体的字段与内部命名不一致（`projectId` ↔ `project_id`），
//! 这里统一做「字段别名 + 规范化」，避免散落在 client 各处。

use serde_json::{Map, Value};

/// 安全取 JSON 对象（非对象 / 数组 / null 返回 `None`）。
pub fn as_json_object(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

/// 安全取 JSON 数组（非数组返回空切片）。
pub fn as_json_array(value: &Value) -> &[Value] {
    match value.as_array() {
        Some(items) => items.as_slice(),
        None => &[],
    }
}

/// 安全取字符串字段（非字符串返回空串）。
pub fn as_string(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

/// **JS 真值判定** —— 对齐 TS 的 `if (result?.pending_confirmation)`。
///
/// ── 为什么需要它 ──────────────────────────────────────────────────
/// Rust 的 `Value::as_bool()` 只认 JSON `true`；TS 的 `if (x)` 认的是**真值**：
/// 对象 / 非空字符串 / 非零数字都算真。上游那个字段是**对象**时，两边就分道扬镳。
///
/// 具体栽过的地方：`/chat/result` 的 `pending_confirmation` 官方样例是**对象**
/// （`{"pending_confirmation": {...}}`，里面带花费估算）。用 `as_bool()` 读它得到
/// `false` ⇒ 把「待确认」当成「已完成」返回 ⇒ **确认闸门被绕过**，
/// 任务最后错落成「未产出」（2026-10-01 查出）。
///
/// ⚠️ 只用在**要对齐 TS 真值语义**的地方。判断"是不是布尔 `true`"仍应直接用 `as_bool()`。
pub fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        // JS：`0` 为假，其余数字为真（JSON 里没有 NaN）。
        Value::Number(number) => number.as_f64().is_some_and(|value| value != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// 一次 `/chat` 提交的输入。
#[derive(Debug, Clone, Default)]
pub struct SendInput {
    pub prompt: String,
    pub project_id: String,
    /// 已上传到 Lovart CDN 的附件 URL 列表。
    pub attachments: Option<Vec<String>>,
    /// 结构化路选模型：`{ prefer_tool_categories: { IMAGE: [...] } }`。
    pub tool_config: Option<Value>,
}

/// 规范化 send 请求体：内部 `project_id` → 上游同名（字段别名），
/// 并仅在非空时附 `attachments` / `tool_config`（metadata 提升）。
///
/// ⚠️ 「仅在非空时附」不是优化 —— 传 `attachments: []` 与不传是**两种语义**，
/// 上游可能把空数组当成"显式声明无附件"。
pub fn normalize_send_body(input: &SendInput) -> Value {
    let mut body = Map::new();
    body.insert("prompt".to_string(), Value::String(input.prompt.clone()));
    body.insert(
        "project_id".to_string(),
        Value::String(input.project_id.clone()),
    );
    if let Some(attachments) = &input.attachments {
        if !attachments.is_empty() {
            body.insert(
                "attachments".to_string(),
                Value::Array(
                    attachments
                        .iter()
                        .map(|url| Value::String(url.clone()))
                        .collect(),
                ),
            );
        }
    }
    if let Some(tool_config) = &input.tool_config {
        body.insert("tool_config".to_string(), tool_config.clone());
    }
    Value::Object(body)
}
