//! 错误信封 —— 对应 TS 的 `lovart/lovart_errors.ts`。
//!
//! 错误契约：**上游返回什么文案就透传什么，绝不翻译 / 静默**。
//!
//! ── Rust 比 TS 强的一处（L1 类型层）────────────────────────────────
//! TS 用 `error.name === 'TypeError' || 'FetchError' || 'TimeoutError'` 这种
//! **字符串判据**来区分「传输层网络错误」和「上游返回的错误码」—— 改个名就失效，
//! 且三种名字本质是同一件事却要逐个列举。
//!
//! 这里由**类型**区分：`LovartError::Transport` 就是传输层，
//! `LovartError::Upstream` 就是上游/领域错误。不可能判错，也不可能漏列。

use std::fmt;

/// 失败类型（对应 TS `LOVART_ERR_TYPES`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrType {
    ProjectInvalid,
    NoArtifact,
    Abort,
    Timeout,
    UploadFailed,
    Upstream,
    PendingConfirmation,
}

impl ErrType {
    /// 与 TS 的字面量一致（写日志 / 对账用）。
    pub fn as_str(self) -> &'static str {
        match self {
            ErrType::ProjectInvalid => "project_invalid",
            ErrType::NoArtifact => "no_artifact",
            ErrType::Abort => "abort",
            ErrType::Timeout => "timeout",
            ErrType::UploadFailed => "upload_failed",
            ErrType::Upstream => "upstream",
            ErrType::PendingConfirmation => "pending_confirmation",
        }
    }
}

/// 上游码。对应 TS 的 `code: number | string`（`-1` = 内部标记）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrCode {
    /// 内部标记（TS 里的 `-1`）：非 HTTP 来源的领域错误。
    Internal,
    /// 数字码。**非 2xx 时这里是 HTTP 状态码**；信封里可能是业务码。
    Number(i64),
    /// 信封里的非数字码（原样保留，不丢信息）。
    Text(String),
}

impl ErrCode {
    pub fn as_number(&self) -> Option<i64> {
        match self {
            ErrCode::Number(value) => Some(*value),
            _ => None,
        }
    }
}

impl fmt::Display for ErrCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrCode::Internal => write!(formatter, "-1"),
            ErrCode::Number(value) => write!(formatter, "{value}"),
            ErrCode::Text(text) => write!(formatter, "{text}"),
        }
    }
}

/// 上游 / 领域错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamError {
    pub message: String,
    pub code: ErrCode,
    pub kind: ErrType,
    /// 上游信封里的**业务码**（`{code: 2012}`）。
    ///
    /// 为什么单独携带：非 2xx 时 `code` 是 HTTP 码，业务码只存在于响应体里。
    /// 不把它解出来的话，上层只能按 HTTP 码判重试性 ⇒
    /// **「并发满」（HTTP 402 + 业务码 2012）会被误判成确定性失败**。
    pub biz_code: Option<i64>,
}

/// 本域错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LovartError {
    /// 上游 / 领域错误：有信封码、有类型。
    Upstream(UpstreamError),
    /// **传输层**错误：连接失败 / DNS / TLS / 单请求超时。
    ///
    /// 与 `Upstream` 分开是刻意的 —— 见文件头「Rust 比 TS 强的一处」。
    Transport { message: String },
}

impl LovartError {
    /// 领域错误构造（对应 TS `new LovartError(message, code, type)`）。
    pub fn domain(message: impl Into<String>, code: ErrCode, kind: ErrType) -> Self {
        LovartError::Upstream(UpstreamError {
            message: message.into(),
            code,
            kind,
            biz_code: None,
        })
    }

    /// 内部标记（`-1`）的领域错误。
    pub fn internal(message: impl Into<String>, kind: ErrType) -> Self {
        Self::domain(message, ErrCode::Internal, kind)
    }

    /// 上游错误（HTTP 码）。
    pub fn upstream(message: impl Into<String>, http_status: u16) -> Self {
        Self::domain(
            message,
            ErrCode::Number(i64::from(http_status)),
            ErrType::Upstream,
        )
    }

    pub fn transport(message: impl Into<String>) -> Self {
        LovartError::Transport {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        match self {
            LovartError::Upstream(error) => &error.message,
            LovartError::Transport { message } => message,
        }
    }

    /// 失败类型。传输层错误一律归为 `Upstream`（与 TS 的默认值一致）。
    pub fn kind(&self) -> ErrType {
        match self {
            LovartError::Upstream(error) => error.kind,
            LovartError::Transport { .. } => ErrType::Upstream,
        }
    }

    pub fn code(&self) -> Option<&ErrCode> {
        match self {
            LovartError::Upstream(error) => Some(&error.code),
            LovartError::Transport { .. } => None,
        }
    }

    pub fn biz_code(&self) -> Option<i64> {
        match self {
            LovartError::Upstream(error) => error.biz_code,
            LovartError::Transport { .. } => None,
        }
    }

    /// 带业务码（对应 TS 里对 `error.bizCode = ...` 的赋值）。
    pub fn with_biz_code(mut self, biz_code: Option<i64>) -> Self {
        if let LovartError::Upstream(error) = &mut self {
            error.biz_code = biz_code;
        }
        self
    }
}

impl fmt::Display for LovartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LovartError::Upstream(error) => {
                write!(formatter, "{} (code={}, type={})", error.message, error.code, error.kind.as_str())
            }
            LovartError::Transport { message } => write!(formatter, "传输失败：{message}"),
        }
    }
}

impl std::error::Error for LovartError {}

/// 「服务端忙 / 并发满 / thread 忙」的码 —— 它们是**排队状态**，不是失败。
///
/// 表：`402/2012` 配额或并发上限 · `409/2011` thread 忙 · `429/1429` 限流。
///
/// ⚠️ 两类码混在一张表里：非 2xx 时 `code` 是 **HTTP 状态码**（402 / 409 / 429），
/// 业务码只存在于响应体里（2011 / 2012 / 1429）。[`is_busy_error`] 两边都查。
///
/// 原先只列了 `{402, 2012, 1429}` ⇒ **409 / 2011 / 429 会被当成普通可重试失败**，
/// 计入 `maxAttempts`（默认 3）。批量跑几百张时，撞三次限流任务就被判死，
/// 而它本该退避重投。
pub const BUSY_CODES: [i64; 6] = [402, 409, 429, 2011, 2012, 1429];

/// 该 HTTP 状态码是否「值得再试」。
///
/// ⚠️ **402 必须算可重试**：提交时 402 = 并发满 ⇒ 退避后重投。
/// 402 表示服务端槽位已满（账号级 8 个），是**瞬时状态**，不是确定性失败。
///
/// 网络层错误（0）· 402 · 408 · 429 · 5xx 视为瞬态；
/// 其余 4xx（400/401/403/404）是确定性硬失败，重试没有意义。
pub fn is_retryable_http_status(status: i64) -> bool {
    if status == 0 {
        return true;
    }
    status == 402 || status == 408 || status == 429 || (500..=599).contains(&status)
}

/// 是否「服务端忙 / 并发满 / thread 忙」—— **排队状态**，不是失败。
pub fn is_busy_error(error: &LovartError) -> bool {
    if let Some(biz_code) = error.biz_code() {
        if BUSY_CODES.contains(&biz_code) {
            return true;
        }
    }
    match error.code().and_then(ErrCode::as_number) {
        Some(code) => BUSY_CODES.contains(&code),
        None => false,
    }
}

/// 是否「值得再试」的瞬态错误 —— **唯一实现**。
///
/// 原先这条规则在 TS 里有**三份**实现，且字面量互有出入（DEBT TD-02-05 的母体：
/// 同一规则多处实现）。Rust 侧只有这一个。
///
/// ⚠️ **用户主动取消不算瞬态** —— 那种情况必须立刻结束，不能重试。
/// 该判断由调用方负责（见 `task::is_transient`）。
pub fn is_retryable_error(error: &LovartError) -> bool {
    match error {
        // 传输层（网络 / DNS / TLS / 单请求超时）一律瞬态。
        LovartError::Transport { .. } => true,
        LovartError::Upstream(upstream) => match upstream.code.as_number() {
            Some(code) => is_retryable_http_status(code),
            None => false,
        },
    }
}
