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

/// 错误分类 —— **唯一口径**。所有「该不该重试 / 该怎么退避」都从这里派生。
///
/// 表：`402/2012` 并发上限 · `409/2011` thread 忙 · `429/1429` 限流 ·
/// `0/408/5xx/Transport` 瞬态 · 其余硬失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// 限流：全局熔断，不按任务计次。
    RateLimit,
    /// 并发满：只对本任务退避（等槽位）。
    Concurrency,
    /// thread 忙：只对本任务退避。
    ThreadBusy,
    /// 网络 / 408 / 5xx：按 `attempts` 重试。
    Transient,
    /// 400/401/403/404 / 2xx 非 JSON：重试没有意义。
    Hard,
}

/// 先查 `biz_code`，再查 HTTP `code`；都没有 ⇒ `Hard`。
///
/// ⚠️ **业务码只在命中已知表时才优先**：上游可能在 429 响应里塞一个
/// 无关的 `code: 1`，若直接 `biz_code.or(http_code)` 会把 429 盖成 Hard。
pub fn classify_error(error: &LovartError) -> ErrorClass {
    match error {
        LovartError::Transport { .. } => ErrorClass::Transient,
        LovartError::Upstream(upstream) => {
            let by_biz = upstream.biz_code.and_then(classify_code);
            let by_http = upstream.code.as_number().and_then(classify_code);
            by_biz.or(by_http).unwrap_or(ErrorClass::Hard)
        }
    }
}

fn classify_code(code: i64) -> Option<ErrorClass> {
    match code {
        402 | 2012 => Some(ErrorClass::Concurrency),
        409 | 2011 => Some(ErrorClass::ThreadBusy),
        429 | 1429 => Some(ErrorClass::RateLimit),
        0 | 408 => Some(ErrorClass::Transient),
        500..=599 => Some(ErrorClass::Transient),
        _ => None,
    }
}

/// 是否「值得再试」—— `ErrorClass` 的派生谓词，**不再维护第二份码表**。
///
/// ⚠️ **用户主动取消不算瞬态** —— 那种情况必须立刻结束，不能重试。
/// 该判断由调用方负责（见 `task::is_transient`）。
pub fn is_retryable_error(error: &LovartError) -> bool {
    matches!(
        classify_error(error),
        ErrorClass::RateLimit
            | ErrorClass::Concurrency
            | ErrorClass::ThreadBusy
            | ErrorClass::Transient
    )
}
