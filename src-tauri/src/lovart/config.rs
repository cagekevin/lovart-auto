//! 域内常量 —— 对应 TS `lovart/lovart_config.ts` 的非模型部分。
//!
//! 模型规格与别名表在 [`super::models`]（那张表要逐行比对，单独放）。

/// 所有端点共用的路径前缀。
pub const PATH_PREFIX: &str = "/v1/openapi";

/// 默认 base URL。
pub const DEFAULT_BASE_URL: &str = "https://lgw.lovart.ai";

/// 是否自动确认高成本操作（`pending_confirmation`）。
///
/// ⚠️ **必须是 `false`**：官方明确要求「Do NOT auto-confirm」。
/// 设为 `true` 会在不询问的情况下直接扣积分 —— 批量跑起来就是静默花钱。
/// 设为 `false` 后遇到 `pending_confirmation` 会抛 `PendingConfirmation`，
/// 由调度层记为 `needs_confirm` 跳过。
pub const AUTO_CONFIRM: bool = false;

/// 轮询起始间隔（ms）。
pub const POLL_INTERVAL_MS: u64 = 10_000;

/// 轮询间隔退避上限（ms）：10s → 20s → 40s → 60s 封顶。
///
/// 长任务若固定 3 秒轮询会打爆请求量触发限流。
pub const POLL_MAX_INTERVAL_MS: u64 = 60_000;

/// done 后复核等待（ms），防子任务误判 done。
pub const DONE_RECHECK_MS: u64 = 5_000;

/// 任务总超时（ms）：90 分钟。
///
/// ⚠️ **超时 ≠ 失败**：调度层会释放 worker、把任务记为 `timeout` 留待复查，
/// **绝不重新提交**。排队任务通常 30 分钟以上、最长不超过 1.5 小时，
/// 所以取上限本身，不留缓冲（留了也只是让 worker 干等）。
pub const TASK_TIMEOUT_MS: u64 = 90 * 60 * 1000;

/// 单次 HTTP 请求超时（ms）：60 秒。与任务总超时分离，避免一次查询挂住整个任务。
pub const REQUEST_TIMEOUT_MS: u64 = 60_000;

/// project 类型（上游枚举）。
pub const PROJECT_TYPE: i64 = 3;
