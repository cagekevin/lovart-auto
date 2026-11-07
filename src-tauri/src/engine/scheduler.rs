//! `scheduler` — 排队生图调度器（对应 TS `src/queue.ts`）。
//!
//! ── 结构（TS 侧 2026-10-01 收口：状态即队列）────────────────────────
//! **真相 = state**。「该做什么」全部从记录派生，不再另立队列与计数器：
//!
//! | 原先散落的 5 处状态 | 现在 |
//! |---|---|
//! | `pendingQueue` / `recheckQueue` / `finalQueue` | **一个 `queue`**（待处理 id 列表）|
//! | `slotsInUse` 计数器 | **派生**：占槽状态计数 |
//! | `readyAt`（内存 Map） | **`TaskRecord.ready_at`**（落盘，重启不丢）|
//! | 「未完成」两套口径 | **一份** `is_unfinished` |
//!
//! 关键洞察：槽位之所以要计数器，是因为「**正在提交**」那段没有状态，
//! 只能靠 `+= 1` 预占。把预占变成**显式状态 `Submitting`** 后，槽位计数自然派生。
//!
//! ── 业务规则 ────────────────────────────────────────────────────────
//! 1. **并发上限是「服务端槽位」**（账号级 8 个），不是 worker 数；
//! 2. **超时 ≠ 失败**：轮询超时只释放 worker，任务转 `Timeout` 保留 threadId，收尾再查；
//! 3. **下载失败 ≠ 重跑生成**：生成成功后下载失败 = `DownloadFailed`，只记账待补下载；
//! 4. **「服务端并发满」是排队，不是失败**：按任务各自退避重投，不计入 `max_attempts`；
//! 5. **参数变更检测**：id 只哈希提示词文本，改了模型/比例/尺寸/参考图 ⇒ 终态记录重置重跑。
//!
//! ── 与 TS 的一处**结构性差异**：并发模型 ────────────────────────────
//! TS 是单线程 async —— 所有状态变更天然串行。Rust 用**线程池**，
//! 所以 `Ctx` 包在 `Mutex` 里，并且**只在「取件 / 写状态」这两小段持锁**，
//! 网络 I/O 一律在锁外。TS 那边"忘了 await 就并发写"这类问题在这里不可能发生。

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::engine::download::{download_artifact, slugify, DownloadedFile};
use crate::engine::intake;
use crate::engine::session::SessionState;
use crate::engine::state::{
    is_unfinished, row as status_row, slots_in_use, Job as JobKind, PersistOutcome, RecordPatch,
    StateStore, TaskRecord, TaskStatus,
};

use crate::lovart::client::{set_mode, ClientDeps};
use crate::lovart::contract::SendInput;
use crate::lovart::errors::{classify_error, is_retryable_error, ErrorClass, ErrType, LovartError};
use crate::lovart::project::{send_chat_with_project, ProjectCache};
use crate::lovart::prompt::{build_lovart_prompt, build_lovart_tool_config, PromptTask};
use crate::lovart::task::{extract_artifacts, poll_thread, AutoConfirmDecision};

/// 「服务端并发满 / thread 忙」的退避基数：5s → 10s → 20s → 40s → 60s 封顶。
const BUSY_BACKOFF_BASE_MS: u64 = 5_000;

/// 402/409 退避封顶（ms）。
const BUSY_BACKOFF_MAX_MS: u64 = 60_000;

/// 429 熔断基数 / 封顶（ms）。
const RATE_LIMIT_BACKOFF_BASE_MS: u64 = 60_000;
const RATE_LIMIT_BACKOFF_MAX_MS: u64 = 900_000;

/// 累计限流等待告警阈值（ms）。
const RATE_LIMIT_TOTAL_WAIT_ALERT_MS: u64 = 6 * 3600 * 1000;

/// 无活可干时的空转上限（ms）。
const IDLE_POLL_MS: u64 = 1_000;

// ── 端口（外部副作用出口）──────────────────────────────────────────────

/// 调度器的**外部副作用出口**。
///
/// 为什么要开这个口：调度器若直接调真实实现，就**除了真跑一批图，无法验证任何调度行为**。
/// 槽位上限、超时递减、下载失败不重跑这些规则，此前只能靠人肉观察终端确认，
/// 不可回归（DEBT TD-04-01）。开了端口后，测试注入假实现即可在**零网络**下断言这些规则。
///
/// 生产实现是 [`RealPorts`]，生产路径零变化。
pub trait Ports: Send + Sync {
    fn set_mode(&self, deps: &ClientDeps, unlimited: bool) -> Result<(), LovartError>;
    fn send(&self, deps: &ClientDeps, input: &SendInput) -> Result<(String, String), LovartError>;
    fn poll(
        &self,
        deps: &ClientDeps,
        thread_id: &str,
        task_timeout: Duration,
        allow: &dyn Fn() -> AutoConfirmDecision,
        confirmed: &AtomicUsize,
    ) -> Result<Value, LovartError>;
    fn extract_artifacts(&self, result: &Value) -> Result<Vec<String>, LovartError>;
    fn download(
        &self,
        url: &str,
        output_dir: &Path,
        file_base: &str,
        retries: u32,
    ) -> Result<DownloadedFile, LovartError>;
}

/// 生产实现（端口缺省值）。
pub struct RealPorts {
    /// project 缓存必须**跨任务共享** —— 每任务建一个 project 会造出一堆孤儿。
    project_cache: ProjectCache,
}

impl Default for RealPorts {
    fn default() -> Self {
        Self::new()
    }
}

impl RealPorts {
    pub fn new() -> Self {
        Self {
            project_cache: ProjectCache::new(),
        }
    }
}

impl Ports for RealPorts {
    fn set_mode(&self, deps: &ClientDeps, unlimited: bool) -> Result<(), LovartError> {
        set_mode(deps, unlimited)
    }

    fn send(&self, deps: &ClientDeps, input: &SendInput) -> Result<(String, String), LovartError> {
        send_chat_with_project(&self.project_cache, deps, input)
    }

    fn poll(
        &self,
        deps: &ClientDeps,
        thread_id: &str,
        task_timeout: Duration,
        allow: &dyn Fn() -> AutoConfirmDecision,
        confirmed: &AtomicUsize,
    ) -> Result<Value, LovartError> {
        let mut scoped = deps.clone();
        scoped.task_timeout = task_timeout;
        poll_thread(&scoped, thread_id, allow, confirmed)
    }

    fn extract_artifacts(&self, result: &Value) -> Result<Vec<String>, LovartError> {
        extract_artifacts(result)
    }

    fn download(
        &self,
        url: &str,
        output_dir: &Path,
        file_base: &str,
        retries: u32,
    ) -> Result<DownloadedFile, LovartError> {
        // 取消标志由调用方在 `Ctx` 里持有；这里用一个不取消的占位 ——
        // 真实取消由 worker 循环在外层检查（下载本身有 60s 超时兜底）。
        let never = std::sync::atomic::AtomicBool::new(false);
        download_artifact(url, output_dir, file_base, &never, retries)
    }
}

// ── 选项与上下文 ──────────────────────────────────────────────────────

pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

/// 会话封口时的回调（GUI 用它把「本会话跑完」变成告警与日志）；CLI 不传。
pub type SessionEndFn = Arc<dyn Fn(&SchedulerSummary) + Send + Sync>;

/// 这一批要处理的**是什么** —— 两种意图，两种语义。
///
/// ── 为什么要分开（2026-10-01 系统性修）────────────────────────────
/// 原先只有一种输入：`Vec<PromptTask>`，也就是**从提示词文件解析出来的声明**，
/// 语义是「**以文件为准**」（声明里垫图为空 ⇒ 记录里的垫图地址被**清空**）。
///
/// 而 GUI 的「重跑 / 接着查 / 确认」要做的是**派工**（"处理这条已有记录"）——
/// 它没有文件，于是**适配层**（`gui.rs`）伪造了一份单行文件，字段只挑
/// `prompt / model / ratio / size` 四个。缺了垫图 + 「没有就清空」⇒ **记录被覆盖**
/// （实测：点「确认」后垫图消失）。「重跑」那条路更早就在硬扛这个缺陷 ——
/// 它靠"参数对不上 ⇒ 重置"的**副作用**来重置，代价是垫图必然被清，
/// 只好加一条闸「**用了垫图就不许重跑**」。
///
/// ⇒ 根子是**两种意图共用一种载体**。这里把它们分开：
/// **派工不经过文件**，字段一律以**记录**为准（记录才是真相，见 [`SchedulerOptions::batch`]）。
#[derive(Debug, Clone)]
pub enum Batch {
    /// 用户声明的一份提示词（文件解析而来）：**以文件为准**，按它同步记录的参数与垫图。
    Declared(Vec<PromptTask>),
    /// 系统派工：**只处理这些 id**，字段以**记录**为准，**不做任何同步**。
    Records(Vec<String>),
}

/// 自动确认策略 —— **一个机制，三件事**：开关（全局热更新）、计数（本批）、许可（本批）。
///
/// 为什么不拆成三个字段散在各处：它们回答的是同一个问题「这次能不能花钱」。
/// 拆开后必然出现"计数改了、许可忘了"这类漂移，且花钱策略会渗进排队上下文（`Ctx`）。
///
/// ⚠️ 协议层（`lovart::task`）不认识这个类型，只收 `allow` 回调 + 计数器引用 ——
///    花钱策略属于调度层，协议层只管"怎么发请求"。
pub struct AutoConfirm {
    /// 全局开关：与 `GuiState.auto_confirm` 共享同一个原子量 ⇒ 切换立即生效。
    enabled: Arc<AtomicBool>,
    /// 本批已自动确认次数（`confirm` 成功那一刻 +1，与轮询收场无关）。
    confirmed: AtomicUsize,
    /// 本批已发放过许可的任务 id（内存，不落盘；批次结束随 `Shared` 一起消失）。
    attempted: Mutex<HashSet<String>>,
}

impl AutoConfirm {
    pub fn new(enabled: Arc<AtomicBool>) -> Arc<Self> {
        Arc::new(Self {
            enabled,
            confirmed: AtomicUsize::new(0),
            attempted: Mutex::new(HashSet::new()),
        })
    }

    /// 永远关闭 —— 测试 / 默认值用。
    pub fn disabled() -> Arc<Self> {
        Self::new(Arc::new(AtomicBool::new(false)))
    }

    /// 「此刻能确认吗」：读开关 + 原子占位，**一步完成**。
    ///
    /// 顺序是刻意的：先判开关（关 ⇒ `Disabled`，不碰集合），再占位。
    /// 关掉开关后，已用过许可的任务会报"未开启"——对应的是用户最后一个动作。
    pub fn decide(&self, id: &str) -> AutoConfirmDecision {
        if !self.enabled.load(Ordering::Relaxed) {
            return AutoConfirmDecision::Disabled;
        }
        let mut attempted = self
            .attempted
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if attempted.insert(id.to_string()) {
            AutoConfirmDecision::Allow
        } else {
            AutoConfirmDecision::AlreadyUsed
        }
    }

    /// 给协议层用的计数器（确认成功时由 `resolve_pending` +1）。
    pub fn confirmed(&self) -> &AtomicUsize {
        &self.confirmed
    }

    pub fn count(&self) -> usize {
        self.confirmed.load(Ordering::Relaxed)
    }

    /// 会话开始时清零「本会话已自动确认」计数。
    ///
    /// 计数器的语义是**每次会话**，而调度器常驻 ⇒ 不重置的话第二会话的告警
    /// 会把第一会话的条数也算进去。
    pub fn reset(&self) {
        self.confirmed.store(0, Ordering::Relaxed);
    }
}

/// 调度器的全部可调数值 —— **一处定义，两个入口共用**。
///
/// 为什么不是一个字段一个字段地摊在 [`SchedulerOptions`] 里：
/// 原先 `run.rs` 与 `gui.rs` 各写一份
/// `SchedulerOptions { task_timeout: …, recheck_timeout: …, /* ×11 */ }`，
/// 加字段必漏一边（`docs/plan/07` R1-6）。收进一个结构体后，构造点只多一行 `tuning`。
///
/// ⚠️ 不要给它加 `Default` 就顺手给 `SchedulerOptions` 也加 —— 后者有
/// `state` / `batch` / `deps`，没有合理默认值，加了就会有人跑出一个空批次。
#[derive(Debug, Clone)]
pub struct SchedulerTuning {
    pub concurrency: usize,
    pub task_timeout: Duration,
    pub recheck_timeout: Duration,
    pub max_timeouts: u32,
    pub download_retries: u32,
    pub max_attempts: u32,
    pub max_busy_retries: u32,
    /// 全局任务间隔（提交，含重试）。
    pub task_interval: Duration,
    /// 「服务端并发满 / thread 忙」的退避基数（测试缝）。
    pub busy_backoff_base: Duration,
    /// 429 熔断基数（测试缝）。
    pub rate_limit_backoff_base: Duration,
}

impl Default for SchedulerTuning {
    fn default() -> Self {
        // 用 `Settings::default()` 而不是读环境：测试要确定，生产走 `from_settings`。
        Self::from_settings(&crate::engine::settings::Settings::default())
    }
}

impl SchedulerTuning {
    /// 从一份**已解析**的设置得到调参。测试可传构造出来的 `Settings`，不碰真实 `.env`。
    pub fn from_settings(settings: &crate::engine::settings::Settings) -> Self {
        Self {
            concurrency: settings.concurrency,
            task_timeout: settings.task_timeout(),
            recheck_timeout: settings.recheck_timeout(),
            max_timeouts: settings.max_timeouts,
            download_retries: settings.download_retries,
            max_attempts: settings.max_attempts,
            max_busy_retries: settings.max_busy_retries,
            task_interval: settings.task_interval(),
            busy_backoff_base: Duration::from_millis(BUSY_BACKOFF_BASE_MS),
            rate_limit_backoff_base: Duration::from_millis(RATE_LIMIT_BACKOFF_BASE_MS),
        }
    }
}

pub struct SchedulerOptions {
    pub deps: ClientDeps,
    pub batch: Batch,
    pub state: StateStore,
    pub output_dir: PathBuf,
    /// 全部可调数值（并发 / 超时 / 次数 / 退避基数）。
    pub tuning: SchedulerTuning,
    /// 重置无 threadId 的 Failed 并重跑。
    pub retry_failed: bool,
    /// 自动确认策略（全局开关 + 本批计数 + 本批许可）。
    pub auto_confirm: Arc<AutoConfirm>,
    pub log: LogFn,
    /// 外部副作用出口；缺省用 [`RealPorts`]。
    pub ports: Option<Arc<dyn Ports>>,
    /// 会话封口时写报告 CSV；`None` = 不写（`--no-report`）。
    pub report_file: Option<PathBuf>,
}

/// 队列里的一件活。
///
/// ⚠️ `kind` 的**真源是状态登记表**（`engine::state::STATUS_TABLE` 的 `job` 列），
/// 这里只是搬运。原先本模块自带一份 `enum JobKind` + 一份穷尽 `match`，
/// 而 `run_final` 里还有两处 `if status == …` 各答一半**同一个问题**
/// （「这一条再跑一次会怎样」）—— 2026-10-01 普查把这四处并成一列。
/// 别名 `JobKind` 只是本模块沿用的叫法，不是第二个类型。
#[derive(Debug, Clone)]
struct Job {
    kind: JobKind,
    id: String,
}

/// 取件三态：有活 / 等多久 / 队列空。
enum TakeOutcome {
    Job(Job),
    Wait(u64),
    Empty,
}

/// 429 熔断的日志载荷（锁内只改状态，日志由 `Shared` 在锁外打）。
struct RateLimitEvent {
    wait: Duration,
    level: u32,
    total_wait: Duration,
    alert: bool,
}

/// 全局限速闸门：**一个闸门只在一处**。
///
/// - `task_interval`：所有 worker 共享的提交间隔（含重试）；
/// - `breaker_until`：429 全局熔断，期间不提交；
/// - `on_rate_limited` / `on_success`：唯一的状态变更入口。
struct SubmitGate {
    last_submit_at: Option<Instant>,
    task_interval: Duration,
    breaker_until: Option<Instant>,
    breaker_level: u32,
    rate_limit_total_wait: Duration,
    backoff_base: Duration,
}

impl SubmitGate {
    fn new(task_interval: Duration, backoff_base: Duration) -> Self {
        Self {
            last_submit_at: None,
            task_interval,
            breaker_until: None,
            breaker_level: 0,
            rate_limit_total_wait: Duration::ZERO,
            backoff_base,
        }
    }

    /// 间隔 + 熔断，一步判定；`Err(wait_ms)` = 现在不能提交。
    fn try_claim(&mut self, now: Instant) -> Result<(), u64> {
        if let Some(until) = self.breaker_until {
            let remaining = until.saturating_duration_since(now);
            if !remaining.is_zero() {
                return Err(remaining.as_millis() as u64);
            }
        }
        if let Some(last) = self.last_submit_at {
            let elapsed = now.saturating_duration_since(last);
            if elapsed < self.task_interval {
                return Err((self.task_interval - elapsed).as_millis() as u64);
            }
        }
        self.last_submit_at = Some(now);
        Ok(())
    }

    /// 只读熔断剩余（给 Recheck 用：轮询也吃 API 预算，熔断期间必须停）。
    fn breaker_remaining(&self, now: Instant) -> u64 {
        self.breaker_until
            .map(|until| until.saturating_duration_since(now).as_millis() as u64)
            .unwrap_or(0)
    }

    /// 锁内只改状态；日志由 `Shared` 在锁外打。
    fn on_rate_limited(&mut self, now: Instant) -> RateLimitEvent {
        let level = self.breaker_level + 1;
        let wait = exponential_backoff(
            level,
            self.backoff_base,
            Duration::from_millis(RATE_LIMIT_BACKOFF_MAX_MS),
        );
        self.breaker_until = Some(now + wait);
        self.breaker_level = level;
        self.rate_limit_total_wait += wait;
        RateLimitEvent {
            wait,
            level,
            total_wait: self.rate_limit_total_wait,
            alert: self.rate_limit_total_wait
                >= Duration::from_millis(RATE_LIMIT_TOTAL_WAIT_ALERT_MS),
        }
    }

    /// 成功提交：只有在熔断已到期时才清零。
    ///
    /// ⚠️ 闸门串行化的是**认领**，不是**请求完成**：worker A 在 T 时刻 429 开熔断，
    /// worker B 在 T-ε 认领、POST 超过 1s 才成功，会在 T+1s 调到这里。若直接清零，
    /// 会把 A 的升级抹掉（level 永远停在 60s 不再涨）。所以熔断未到期时忽略这次成功。
    fn on_success(&mut self, now: Instant) {
        if self.breaker_until.is_some_and(|until| until > now) {
            return;
        }
        self.breaker_level = 0;
        self.rate_limit_total_wait = Duration::ZERO;
        self.breaker_until = None;
    }
}

/// 指数退避 + jitter，**唯一的公式实现**。
///
/// ⚠️ 先 jitter 再 `min(max)`：否则 60s/900s 的「封顶」实际会变成 78s/1170s。
fn exponential_backoff(level: u32, base: Duration, max: Duration) -> Duration {
    let factor = 1u32 << level.saturating_sub(1).min(20);
    let raw = base.saturating_mul(factor);
    Duration::from_millis(jitter(raw.as_millis() as u64)).min(max)
}

struct Ctx {
    deps: ClientDeps,
    ports: Arc<dyn Ports>,
    state: StateStore,
    output_dir: PathBuf,
    concurrency: usize,
    task_timeout: Duration,
    recheck_timeout: Duration,
    max_timeouts: u32,
    download_retries: u32,
    max_attempts: u32,
    max_busy_retries: u32,
    busy_backoff_base: Duration,
    task_by_id: BTreeMap<String, PromptTask>,
    /// 唯一的待处理列表。**工作类型由记录的 `status` 派生**，不再分三个队列。
    queue: Vec<String>,
    /// 已取走、还没收尾的 id。**「界面是否持有它」的唯一判据**
    /// （`held_ids` = queue ∪ processing），不参与并发槽计算（那是 `slots_in_use`）。
    processing: std::collections::BTreeSet<String>,
    /// 已删除的 id（墓碑）。在飞任务收尾时若命中这里，**静默丢弃**写回 ——
    /// 否则「取消全部未完成」会刷一串「状态更新失败（内部 bug）」假告警。
    removed: std::collections::BTreeSet<String>,
    /// 全局提交闸门（间隔 + 429 熔断）。
    gate: SubmitGate,
    /// 本批是否已经记过「排队模式已设置」—— **只影响日志**，不影响行为。
    /// 每条提交前都会重设（见 [`Shared::ensure_mode`]），这里只是避免刷屏。
    mode_logged: bool,
    /// 是否对「失败且无 threadId」的记录重置重跑（`--retry-failed` 的一次性快照）。
    retry_failed: bool,
    /// 会话封口时写报告；`None` = 不写。
    report_file: Option<PathBuf>,
    /// 会话统计（统计窗口，不参与调度）。放 Ctx 而不是 `Shared` 的另一把锁里：
    /// 封口必须与「队列空」判定在同一把锁内，两把锁就会出现「刚判空又投喂」的窗口。
    session: SessionState,
}

impl Ctx {
    /// 按 id 集合生成会话摘要（封口与 `shutdown` 兜底共用）。
    ///
    /// 逐状态计数**由表生成** —— 不手抄具名字段，加状态时不会静默漏计。
    fn summarize_ids(
        &mut self,
        ids: &std::collections::BTreeSet<String>,
    ) -> SchedulerSummary {
        let records: Vec<TaskRecord> = self
            .state
            .all()
            .into_iter()
            .filter(|record| ids.contains(&record.id))
            .cloned()
            .collect();
        let mut counts = std::collections::BTreeMap::new();
        for status in crate::engine::state::all() {
            counts.insert(
                crate::engine::state::json_key(status),
                records
                    .iter()
                    .filter(|record| record.status == status)
                    .count(),
            );
        }
        SchedulerSummary {
            total: ids.len(),
            counts,
            unfinished: records
                .iter()
                .filter(|record| is_unfinished(record.status))
                .count(),
            persist_failed: !self.state.flush().is_ok(),
            records,
        }
    }
}

/// 启动修复：把「已认领、没拿到 threadId」的残留改回 `pending`。
///
/// `submitting` 在状态表里 `job` 为空 ⇒ 不会被取件；没有 threadId 也无处续查，
/// 界面会永远显示「提交中」。**只改状态、不入队**，所以不花一分钱（09 §3.8）。
fn fix_orphan_submitting(ctx: &mut Ctx) -> usize {
    let ids: Vec<String> = ctx
        .state
        .all()
        .into_iter()
        .filter(|record| record.status == TaskStatus::Submitting && record.thread_id.is_none())
        .map(|record| record.id.clone())
        .collect();
    for id in &ids {
        let _ = ctx.state.update(
            id,
            RecordPatch {
                status: Some(TaskStatus::Pending),
                error: Some(Some("上次提交未完成，重新提交".to_string())),
                ..Default::default()
            },
        );
    }
    if !ids.is_empty() {
        let _ = ctx.state.flush();
    }
    ids.len()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn error_text(error: &LovartError) -> String {
    error.message().to_string()
}

/// 会话封口时从锁内带出来的数据包 —— 锁外写报告用，避免持锁做磁盘 IO。
struct ClosedSession {
    session_id: String,
    summary: SchedulerSummary,
    report_file: Option<PathBuf>,
    tasks: BTreeMap<String, PromptTask>,
    output_dir: PathBuf,
}

/// 共享上下文。**只在取件 / 写状态这两小段持锁**，网络 I/O 一律在锁外。
struct Shared {
    ctx: Mutex<Ctx>,
    log: LogFn,
    /// 协议层取消（与 `ClientDeps.cancelled` 是同一份 `Arc`）：让在飞 HTTP 立刻跳出。
    /// CLI 的 Ctrl+C 处理器也置它。
    cancelled: Arc<AtomicBool>,
    /// 会话级停止：`stop_session` 置位、`enqueue` 复位。
    /// ⚠️ 它**不**让 worker 退出 —— 退出只看 `shutdown`（`docs/plan/09` §1.2.2）。
    stopped: AtomicBool,
    /// 进程级关停：worker 唯一的退出条件，永不复位。
    shutdown: AtomicBool,
    /// worker 等待 / 唤醒。**必须与 `ctx: Mutex<Ctx>` 配对**（§1.5.1）。
    wake: Condvar,
    /// worker 句柄（`shutdown` 时 join；`start` 里填充）。
    handles: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// 最近一次封口时生成的摘要（`shutdown` 优先取它，避免重复统计）。
    last_summary: Mutex<Option<SchedulerSummary>>,
    /// 会话封口回调（GUI 用它把「本会话跑完」变成告警与日志）；CLI 为 `None`。
    on_session_end: Option<SessionEndFn>,
    /// 自动确认策略（全局开关 + 本批计数 + 本批许可）。
    auto_confirm: Arc<AutoConfirm>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Ctx> {
        self.ctx.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn log(&self, message: &str) {
        (self.log)(message);
    }

    /// 会话是否已停止。协议层的 `cancelled` 也算：CLI 的 Ctrl+C 只置那一个。
    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst) || self.cancelled.load(Ordering::SeqCst)
    }

    fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// 关停调度器：worker 在下一个检查点退出。**幂等**。
    fn begin_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.wake.notify_all();
    }

    /// 取一件活；`None` = 已关停（worker 退出）。**唯一的等待点**。
    ///
    /// ⚠️ 判空与等待必须在**同一个 guard** 上：先返回 `Empty`、放开锁再 `wait`，
    /// 中间到达的唤醒会丢，worker 永久睡死（`docs/plan/09` §1.5.1）。
    fn next_job(&self) -> Option<Job> {
        let mut ctx = self.lock();
        loop {
            if self.is_shutdown() {
                return None;
            }
            match self.take_job(&mut ctx) {
                // 「在飞」由 `Ctx.processing` 记（`take_job` 取件时插入），
                // 不再另用一个计数器 —— 否则同一个事实有两份。
                TakeOutcome::Job(job) => return Some(job),
                TakeOutcome::Empty => {
                    // 没活 ⇒ 等下一次 `enqueue`。**不自动关停**：调度器是常驻的，
                    // 退出只看 `shutdown`（否则「启动完还没投喂」的瞬间 worker 就全走了）。
                    ctx = self.wake.wait(ctx).unwrap_or_else(|error| error.into_inner());
                }
                TakeOutcome::Wait(ms) => {
                    let (guard, _) = self
                        .wake
                        .wait_timeout(ctx, Duration::from_millis(ms.max(1)))
                        .unwrap_or_else(|error| error.into_inner());
                    ctx = guard;
                }
            }
        }
    }

    /// 一件活收尾：在飞数减一；会话若已无活则封口（写日志 + 报告 + 摘要）。
    ///
    /// ⚠️ 封口判定与「队列空」必须在**同一把锁**内，否则多个 worker 会同时
    /// 判定空闲、各写一份报告（`docs/plan/09` §1.5.6）。`SessionState::close`
    /// 用「会话号还在不在」做幂等。
    fn finish_job(&self, id: &str) {
        let closed = {
            let mut ctx = self.lock();
            ctx.processing.remove(id);
            Self::take_closed_if_idle(&mut ctx)
        };
        self.publish_session_end(closed);
        self.wake.notify_all();
    }

    /// 没有活就封口：**判定与封口在同一把锁内**（§1.5.6）。
    ///
    /// 两处调用：`finish_job`（正常跑完）与 `stop_session`（用户主动停）。
    fn close_session_if_idle(&self) {
        let closed = {
            let mut ctx = self.lock();
            Self::take_closed_if_idle(&mut ctx)
        };
        self.publish_session_end(closed);
    }

    /// 锁内：没有活就取走会话数据。`None` = 还有活 / 会话已封口（幂等）。
    fn take_closed_if_idle(ctx: &mut Ctx) -> Option<ClosedSession> {
        if !ctx.queue.is_empty() || !ctx.processing.is_empty() {
            return None;
        }
        let (session_id, ids) = ctx.session.close()?;
        // 会话结束 ⇒ 在飞任务必然已收尾 ⇒ 墓碑不再需要（不清会无限增长）。
        ctx.removed.clear();
        let summary = ctx.summarize_ids(&ids);
        Some(ClosedSession {
            session_id,
            summary,
            report_file: ctx.report_file.clone(),
            tasks: ctx.task_by_id.clone(),
            output_dir: ctx.output_dir.clone(),
        })
    }

    /// 锁外：日志 + 摘要 + 写报告（文件 IO 不占调度器锁）。
    fn publish_session_end(&self, closed: Option<ClosedSession>) {
        let Some(closed) = closed else {
            return;
        };
        self.log(&format!(
            "===== 会话结束：{}（共 {} 条，未完成 {}）=====",
            closed.session_id, closed.summary.total, closed.summary.unfinished
        ));
        *self
            .last_summary
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(closed.summary.clone());
        // 回调在**锁外、报告写完之后**：GUI 在这里把「本会话跑完」变成告警与日志。
        if let Some(callback) = &self.on_session_end {
            callback(&closed.summary);
        }
        if let Some(path) = closed.report_file {
            write_report_file(
                &path,
                &closed.summary,
                &closed.tasks,
                &closed.output_dir,
                &self.log,
            );
        }
        // 回调在锁外、报告写完之后：GUI 在这里把「本会话跑完」变成告警与日志。
        if let Some(callback) = &self.on_session_end {
            callback(&closed.summary);
        }
    }

    /// 提交前把服务端模式设回排队（`unlimited`，免费）。**每条提交都调**。
    ///
    /// ── 为什么不是「每批一次」────────────────────────────────────────
    /// 服务端模式是**账号级持久设置**，不是本进程的局部状态：手机、另一台电脑、
    /// AI Canvas 任何一处点了 fast，都会把它改掉，而 fast 是**扣积分**的。
    /// 一批能跑十几个小时（2026-11-03 那批 18:39 → 次日 08:55），只在开头设一次
    /// ⇒ 中途被改之后，后面每一条都在裸奔。
    ///
    /// 实测（2026-11-04 10:42 那批 13 张，`var/run.log` 50341-50483）：
    /// 前 4 张在免费队列里等了 26~28 分钟；模式在 10:42:42~10:47:55 之间被外部
    /// 改成 fast 后，后 9 张全部 66~70 秒出图 —— 那就是被扣积分的那 9 张。
    /// 当时本批只在 10:42:23 设过一次，之后一次都没设、也没校验。
    ///
    /// ── 代价 ────────────────────────────────────────────────────────
    /// 每条多 1 次 `/mode/set`（实测约 1.6s）。它发生在 `SubmitGate` 已 claim 之后，
    /// 而闸门本来就强制提交间隔（默认 6s）⇒ **这 1.6s 藏在原本的空等里，不增加墙钟时间**。
    /// 日志只在**首次成功**时打一行，其余静默（否则 1590 条就是 1590 行噪音）。
    ///
    /// ── 失败 ────────────────────────────────────────────────────────
    /// 返回 `Err` ⇒ 调用方**不提交**（见 [`run_submit`]）。宁可不发，也不在未知模式下扣钱。
    fn ensure_mode(&self, deps: &ClientDeps, ports: &dyn Ports) -> Result<(), LovartError> {
        // 网络 I/O 一律在锁外：这里只把结果搬进 `Ctx`。
        ports.set_mode(deps, true)?;
        let first = {
            let mut ctx = self.lock();
            let first = !ctx.mode_logged;
            ctx.mode_logged = true;
            first
        };
        if first {
            self.log("排队模式已设置（unlimited / 免费）—— 之后每条提交前都会重设");
        }
        Ok(())
    }

    /// 429：开全局熔断；日志在锁外打。
    fn on_rate_limited(&self) {
        let event = {
            let mut ctx = self.lock();
            ctx.gate.on_rate_limited(Instant::now())
        };
        self.log(&format!(
            "🚧 429 限流，全局暂停 {}s（第 {} 次）",
            event.wait.as_secs(),
            event.level
        ));
        if event.alert {
            self.log(&format!(
                "⚠️ 累计限流等待 {} 分钟，疑似配额耗尽，建议停止并检查账户",
                event.total_wait.as_secs() / 60
            ));
        }
    }

    /// 提交成功：熔断等级与累计等待归零（熔断未到期时忽略，见 `SubmitGate::on_success`）。
    fn on_success(&self) {
        self.lock().gate.on_success(Instant::now());
    }

    /// 写状态（不改变队列成员）。
    ///
    /// ── 为什么这里**必须**接住 `Err`（而不是 `let _ =`）──────────────────
    ///
    /// `StateStore::update` 的 `Err` 只有一种含义：**调用方传了一个不存在的 id**。
    /// 那是**代码 bug**，不是运行时状况（见 `StateError` 的注释：「绝不静默造记录」）。
    /// 静默掉它 = 让 bug 隐身 —— 任务会**永远停在旧状态**，而没有任何人知道为什么。
    ///
    /// ⚠️ 与「写盘失败」是**两条路**：写盘失败由 `StateStore` 自己的 `on_write_error`
    /// 上报（`PersistOutcome` 那套），**不在这里**。所以这里不能靠 `flush()` 兜底。
    ///
    /// ⚠️ 持锁期间调 `self.log` —— 因此 `log` 回调**不许回头锁 `ctx`**（会死锁）。
    /// 生产上的两个回调（CLI 的 println / GUI 的丢弃）都不碰它。
    fn update(&self, id: &str, patch: RecordPatch) {
        let mut ctx = self.lock();
        // 墓碑：这条已被「取消全部未完成」删掉，在飞任务收尾时就地丢弃。
        // 不在墓碑里又查不到记录 ⇒ 仍是内部 bug，照常告警。
        if ctx.removed.contains(id) {
            return;
        }
        if let Err(error) = ctx.state.update(id, patch) {
            self.log(&format!("状态更新失败（内部 bug，请报告）：{error}"));
        }
    }

    /// 把任务置为**终态**的唯一出口。
    ///
    /// **不做唤醒**：终态只改自己的记录；别的任务靠 `ready_at` + `TakeOutcome::Wait` 到期唤醒。
    fn finish(&self, id: &str, status: TaskStatus, mut patch: RecordPatch) {
        patch.status = Some(status);
        let mut ctx = self.lock();
        if ctx.removed.contains(id) {
            return;
        }
        // 收住错误、**不吞**（理由见 `Shared::update`）。持锁期间不调 `log`，
        // 放锁后再报 —— 免得 `log` 回调万一碰 `ctx` 就成了死锁。
        let problem = ctx
            .state
            .update(id, patch)
            .err()
            .map(|error| format!("状态更新失败（内部 bug，请报告）：{error}"));
        drop(ctx);
        if let Some(problem) = problem {
            self.log(&problem);
        }
    }

    /// 把任务放回待处理列表（非终态）。
    fn requeue(&self, id: &str, status: TaskStatus, mut patch: RecordPatch) {
        patch.status = Some(status);
        let mut ctx = self.lock();
        if ctx.removed.contains(id) {
            return;
        }
        let problem = ctx
            .state
            .update(id, patch)
            .err()
            .map(|error| format!("状态更新失败（内部 bug，请报告）：{error}"));
        // ⚠️ 停止时**只改状态、不入队**：否则在飞任务收尾会把刚清空的队列又填满，
        //    于是 `is_live` 恒真、会话永远封不了口（`docs/plan/09` §1.5.7）。
        // 退回队列 ⇒ 它不再是「已派出」的活。
        ctx.processing.remove(id);
        if !self.is_stopped() && !ctx.queue.iter().any(|queued| queued == id) {
            ctx.queue.push(id.to_string());
        }
        drop(ctx);
        if let Some(problem) = problem {
            self.log(&problem);
        }
    }

    /// 取件。返回三态：有活 / 等多久 / 队列空。
    ///
    /// 闸门只在这里：Submit 必须过 `SubmitGate`；被挡住时**跳过 Submit 继续找非 Submit 候选**，
    /// 避免 `Redownload` / `RecheckFinal` 被饿死。
    fn take_job(&self, ctx: &mut Ctx) -> TakeOutcome {
        let now_ms_value = now_ms();
        let slots = slots_in_use(&ctx.state);
        let concurrency = ctx.concurrency;
        let now = Instant::now();
        // 429 熔断期间，提交和轮询都停；Redownload 是 CDN 下载，不占 API 预算，不拦。
        let breaker_wait = ctx.gate.breaker_remaining(now);

        // 先收集候选再决定 —— 避免在遍历时持有对 `ctx.state` 的借用。
        let mut best: Option<(usize, u32, JobKind)> = None;
        let mut earliest_ready: Option<u64> = None;
        for (index, id) in ctx.queue.iter().enumerate() {
            let Some(record) = ctx.state.get(id) else {
                continue;
            };
            // 工作类型与取件优先级**都读状态表**（真源在 `engine::state`，
            // 本模块不再自带第二份 —— 原先 `kind_of` / `priority_of` 各是一份）。
            let Some(kind) = status_row(record.status).job else {
                continue; // `job` 为空（终态 / 提交中）残留在队列里：跳过
            };
            if record.ready_at > now_ms_value {
                let remaining = record.ready_at - now_ms_value;
                earliest_ready =
                    Some(earliest_ready.map_or(remaining, |value| value.min(remaining)));
                continue; // 还没到重试时间
            }
            if kind == JobKind::Submit && slots >= concurrency {
                continue; // 提交要占槽位
            }
            if breaker_wait > 0
                && matches!(
                    kind,
                    JobKind::Submit | JobKind::Recheck | JobKind::RecheckFinal
                )
            {
                continue; // 熔断期间提交和轮询都停；Redownload 是 CDN 下载，不拦
            }
            let priority = status_row(record.status).priority;
            if best.is_none_or(|(_, best_priority, _)| priority < best_priority) {
                best = Some((index, priority, kind));
            }
        }

        let Some((index, _, kind)) = best else {
            if ctx.queue.is_empty() {
                return TakeOutcome::Empty;
            }
            let wait = earliest_ready
                .unwrap_or(IDLE_POLL_MS)
                .min(IDLE_POLL_MS)
                .min(if breaker_wait > 0 {
                    breaker_wait
                } else {
                    u64::MAX
                });
            return TakeOutcome::Wait(wait.max(1));
        };

        // 提交要过全局闸门；被挡住就跳过它，继续找非 Submit 候选。
        if kind == JobKind::Submit {
            if let Err(wait) = ctx.gate.try_claim(now) {
                let mut fallback: Option<(usize, u32, JobKind)> = None;
                for (index, id) in ctx.queue.iter().enumerate() {
                    let Some(record) = ctx.state.get(id) else {
                        continue;
                    };
                    let Some(kind) = status_row(record.status).job else {
                        continue;
                    };
                    if kind == JobKind::Submit || record.ready_at > now_ms_value {
                        continue;
                    }
                    let priority = status_row(record.status).priority;
                    if fallback.is_none_or(|(_, p, _)| priority < p) {
                        fallback = Some((index, priority, kind));
                    }
                }
                if let Some((index, _, kind)) = fallback {
                    let id = ctx.queue.remove(index);
                    ctx.processing.insert(id.clone());
                    return TakeOutcome::Job(Job { kind, id });
                }
                // 闸门挡住时，也要为「更早到期的非 Submit 任务」醒来。
                let wait = earliest_ready
                    .map_or(wait, |value| wait.min(value))
                    .min(IDLE_POLL_MS);
                return TakeOutcome::Wait(wait.max(1));
            }
        }

        let id = ctx.queue.remove(index);
        // 记「已派出去」：界面是否持有它（进度条判据）与「还有没有活」都看这个集合。
        ctx.processing.insert(id.clone());
        // 预占：变成**显式状态**，槽位计数随之派生 —— 不再需要 `slots_in_use += 1`
        if kind == JobKind::Submit {
            let _ = ctx.state.update(
                &id,
                RecordPatch {
                    status: Some(TaskStatus::Submitting),
                    ..Default::default()
                },
            );
        }
        TakeOutcome::Job(Job { kind, id })
    }
}

// ── 摘要 ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SchedulerSummary {
    pub total: usize,
    /// **逐状态计数**（键 = `state::json_key`）—— 由状态登记表生成。
    ///
    /// ⚠️ 原先这里是 7 个具名字段（`done` / `failed` / `no_artifact` /
    /// `needs_confirm` / `download_failed` / `unresolved` / `pending`）：
    /// 那是**手抄的第二份状态清单**，加一个状态时它不报错、只会**静默漏计**。
    /// 现在表里有几行，这里就有几个键。
    pub counts: std::collections::BTreeMap<String, usize>,
    /// 「处理中」总数（`is_unfinished`）。它是**聚合**、不是某个状态，所以单列。
    pub unfinished: usize,
    /// 状态文件是否落盘失败（失败 ⇒ 本次进度可能续不上，必须让用户知道）。
    pub persist_failed: bool,
    /// 当前提示词文件对应的记录快照。
    pub records: Vec<TaskRecord>,
}

// ── 报告（会话封口时写）────────────────────────────────────────────────

/// 写报告：**先归档旧的，再覆盖**。
///
/// 主路径固定（`var/report.csv` 永远是最新一次会话）；归档只为「清空任务记录后
/// 还想翻历史」留一条后路（`docs/plan/08` §1.3.13）。
fn write_report_file(
    path: &Path,
    summary: &SchedulerSummary,
    tasks: &BTreeMap<String, PromptTask>,
    output_dir: &Path,
    log: &LogFn,
) {
    archive_previous_report(path, log);
    if let Err(error) = crate::run::write_report(path, &summary.records, tasks, output_dir) {
        log(&format!(
            "写报告失败（图已经出了，只是这份 CSV 没生成）：{error}"
        ));
    }
}

/// 覆盖前把旧报告挪进同级 `reports/`，用旧文件的 mtime 命名。
///
/// ⚠️ 失败**只告警**、不阻断：报告是副产品，不能因为它让一次跑批看起来失败。
fn archive_previous_report(path: &Path, log: &LogFn) {
    let Ok(meta) = std::fs::metadata(path) else {
        return; // 不存在 = 首次写，无需归档
    };
    let stamp = meta
        .modified()
        .ok()
        .map(chrono::DateTime::<chrono::Local>::from)
        .map(|time| time.format("%Y%m%d-%H%M%S").to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("reports");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        log(&format!("建不了报告归档目录 {}：{error}", dir.display()));
        return;
    }
    let target = dir.join(format!("report-{stamp}.csv"));
    if let Err(error) = std::fs::rename(path, &target) {
        log(&format!("归档旧报告失败（不影响本次报告）：{error}"));
    }
}

// ── 单任务处理 ────────────────────────────────────────────────────────

/// 下载一条记录的全部产物。**逐张隔离**：一张失败不影响其它张，
/// 失败的 URL 原样返回供记账补下。
fn download_all(
    urls: &[String],
    prompt_text: &str,
    id: &str,
    shared: &Shared,
) -> (Vec<String>, Vec<String>) {
    let (output_dir, ports, retries) = {
        let ctx = shared.lock();
        (
            ctx.output_dir.clone(),
            Arc::clone(&ctx.ports),
            ctx.download_retries,
        )
    };

    let mut files: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for (index, url) in urls.iter().enumerate() {
        let suffix = if urls.len() > 1 {
            format!("_{}", index + 1)
        } else {
            String::new()
        };
        let file_base = format!("{id}_{}{suffix}", slugify(prompt_text, 20));
        match ports.download(url, &output_dir, &file_base, retries) {
            // ⚠️ 记录里存**相对输出目录**的路径（产物总是直接落在 `output_dir` 下 ⇒ 就是文件名），
            //    不存绝对路径。绝对路径把"文件夹叫什么、放在哪"钉进数据里 ——
            //    **改一次名，历史记录里的图立刻全成破图**（2026-10-01 真发生过：仓库改名后
            //    `state.jsonl` 里 8 条记录全指向不存在的目录）。读取时按**当前**输出目录拼回来
            //    （`run::resolve_file`），搬运 / 改名都不再影响历史记录。
            //    `file_name()` 拿不到（理论不可能）⇒ 退回原值：不猜，也不把路径丢掉。
            Ok(downloaded) => files.push(
                downloaded
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| downloaded.path.to_string_lossy().to_string()),
            ),
            Err(error) => failed.push(format!("{url} :: {}", error_text(&error))),
        }
    }
    (files, failed)
}

fn run_submit(worker_id: usize, id: &str, shared: &Shared) {
    let (deps, ports, task, record, task_timeout) = {
        let ctx = shared.lock();
        let Some(task) = ctx.task_by_id.get(id).cloned() else {
            shared.log(&format!("[w{worker_id}] {id} 缺少任务定义，跳过"));
            return;
        };
        let Some(record) = ctx.state.get(id).cloned() else {
            shared.log(&format!("[w{worker_id}] {id} 缺少任务定义，跳过"));
            return;
        };
        (
            ctx.deps.clone(),
            Arc::clone(&ctx.ports),
            task,
            record,
            ctx.task_timeout,
        )
    };

    let has_refs = task.refs.as_ref().is_some_and(|refs| !refs.is_empty());
    let mode = match &task.refs {
        Some(refs) if !refs.is_empty() => format!("图生图({}图)", refs.len()),
        _ => "文生图".to_string(),
    };
    shared.log(&format!(
        "[w{worker_id}] 提交 {id} · {} · {} {} · {mode}",
        task.model, task.image_size, task.aspect_ratio
    ));

    // 服务端模式是**账号级**持久设置，随时会被别处改回 fast ⇒ **每条提交前都重设**。
    // 失败就不发（退回 pending，下次重投时再设一次），不在未知模式下花钱。
    if let Err(error) = shared.ensure_mode(&deps, ports.as_ref()) {
        handle_submit_error(worker_id, id, &error, &record, shared);
        return;
    }

    let tool_config = build_lovart_tool_config(&task.model);
    // 只传档位标签 + 比例，**不传像素**：像素会被上游当硬约束，长边算大后
    // 超出免费额度会触发 pending_confirmation（需人工批准才扣积分）。
    let attachments = task.refs.clone();
    let prompt = build_lovart_prompt(
        &task.model,
        &task.prompt,
        Some(task.image_size.as_str()),
        has_refs,
        &[format!("aspect_ratio: {}", task.aspect_ratio)],
    );
    let input = SendInput {
        prompt,
        project_id: String::new(),
        attachments,
        tool_config,
    };

    let sent = ports.send(&deps, &input);
    let (thread_id, project_id) = match sent {
        Ok(pair) => pair,
        Err(error) => {
            handle_submit_error(worker_id, id, &error, &record, shared);
            return;
        }
    };
    if thread_id.is_empty() {
        let error = LovartError::internal("上游未返回 thread_id", ErrType::Upstream);
        handle_submit_error(worker_id, id, &error, &record, shared);
        return;
    }
    // P6：校验通过后才算真正成功，避免异常路径误清零熔断。
    shared.on_success();

    // 提交成功**立刻落盘**：即使进程随后被杀，下次也能按 threadId 续轮询而不是重复提交。
    shared.update(
        id,
        RecordPatch {
            status: Some(TaskStatus::Running),
            thread_id: Some(Some(thread_id.clone())),
            project_id: Some(Some(project_id)),
            attempts: Some(record.attempts + 1),
            timeout_count: Some(0),
            ready_at: Some(0),
            error: Some(None),
            ..Default::default()
        },
    );
    shared.log(&format!(
        "[w{worker_id}] 已提交 {id} thread={thread_id}，进入轮询"
    ));
    poll_and_finish(worker_id, id, &thread_id, shared, task_timeout, false);
}

fn handle_submit_error(
    worker_id: usize,
    id: &str,
    error: &LovartError,
    record: &TaskRecord,
    shared: &Shared,
) {
    // 用户中断（Ctrl+C）：还没拿到 threadId，回退 pending，下次续跑重新提交。
    if shared.is_stopped() {
        shared.requeue(
            id,
            TaskStatus::Pending,
            RecordPatch {
                error: Some(Some("已中断".to_string())),
                ..Default::default()
            },
        );
        return;
    }

    match classify_error(error) {
        // 429 = 限流：全局熔断，**不按任务计次**。
        ErrorClass::RateLimit => {
            shared.on_rate_limited();
            shared.requeue(
                id,
                TaskStatus::Pending,
                RecordPatch {
                    ready_at: Some(0),
                    error: Some(Some(format!("限流，全局暂停中：{}", error_text(error)))),
                    ..Default::default()
                },
            );
            shared.log(&format!(
                "[w{worker_id}] ⏸️ {id} 提交撞限流，回待提交，等待全局熔断"
            ));
            return;
        }
        // 402/409 = 并发满 / thread 忙：只对本任务退避，不动全局。
        ErrorClass::Concurrency | ErrorClass::ThreadBusy => {
            let (busy, max_busy, base) = {
                let ctx = shared.lock();
                (
                    ctx.state.get(id).and_then(|r| Some(r.busy_retries)).unwrap_or(0) + 1,
                    ctx.max_busy_retries,
                    ctx.busy_backoff_base,
                )
            };
            if busy > max_busy {
                shared.finish(
                    id,
                    TaskStatus::Failed,
                    RecordPatch {
                        busy_retries: Some(busy),
                        error: Some(Some(format!(
                            "服务端持续并发满 {busy} 次，放弃：{}",
                            error_text(error)
                        ))),
                        ..Default::default()
                    },
                );
                shared.log(&format!(
                    "[w{worker_id}] ❌ {id} 服务端持续并发满 {busy} 次，放弃"
                ));
                return;
            }
            let wait = exponential_backoff(busy, base, Duration::from_millis(BUSY_BACKOFF_MAX_MS));
            shared.requeue(
                id,
                TaskStatus::Pending,
                RecordPatch {
                    busy_retries: Some(busy),
                    ready_at: Some(now_ms() + wait.as_millis() as u64),
                    error: Some(Some(format!(
                        "服务端并发满，排队重投：{}",
                        error_text(error)
                    ))),
                    ..Default::default()
                },
            );
            shared.log(&format!(
                "[w{worker_id}] ⏸️ {id} 服务端并发已满，{}s 后重投",
                wait.as_secs()
            ));
            return;
        }
        ErrorClass::Transient | ErrorClass::Hard => {}
    }

    // 提交阶段失败：按可重试性决定回到 pending 还是判失败。
    let attempts = record.attempts + 1;
    let max_attempts = shared.lock().max_attempts;
    if is_retryable_error(error) && attempts < max_attempts {
        shared.requeue(
            id,
            TaskStatus::Pending,
            RecordPatch {
                attempts: Some(attempts),
                error: Some(Some(error_text(error))),
                ..Default::default()
            },
        );
        shared.log(&format!(
            "[w{worker_id}] {id} 提交失败（可重试 {attempts}/{max_attempts}）：{}",
            error_text(error)
        ));
    } else {
        shared.finish(
            id,
            TaskStatus::Failed,
            RecordPatch {
                attempts: Some(attempts),
                error: Some(Some(error_text(error))),
                ..Default::default()
            },
        );
        shared.log(&format!("[w{worker_id}] {id} 提交失败：{}", error_text(error)));
    }
}

/// 退避抖动：`backoff * (1 + rand * 0.3)`。
fn jitter(backoff_ms: u64) -> u64 {
    // 不引 rand 依赖：用系统时间的低位当伪随机源（只需要"错开"，不需要统计质量）。
    let noise = now_ms() % 1000;
    let factor = 1.0 + (noise as f64 / 1000.0) * 0.3;
    (backoff_ms as f64 * factor).round() as u64
}

fn run_recheck(worker_id: usize, id: &str, shared: &Shared) {
    let (thread_id, recheck_timeout) = {
        let ctx = shared.lock();
        let Some(record) = ctx.state.get(id) else {
            return;
        };
        let Some(thread_id) = record.thread_id.clone() else {
            drop(ctx);
            shared.log(&format!("[w{worker_id}] {id} 缺少 threadId，回退为重新提交"));
            shared.requeue(
                id,
                TaskStatus::Pending,
                RecordPatch {
                    error: Some(Some("缺少 threadId".to_string())),
                    ..Default::default()
                },
            );
            return;
        };
        (thread_id, ctx.recheck_timeout)
    };
    shared.log(&format!("[w{worker_id}] 复查 {id}（thread={thread_id}）"));
    poll_and_finish(worker_id, id, &thread_id, shared, recheck_timeout, false);
}

/// 收尾处理（每条只跑一轮，避免无限循环）：
///   ① `Job::Redownload` → **只补下载，绝不重新生成**；
///   ② `Job::RecheckFinal` → 兜底复查一次，完成就取件，否则留到下次运行。
///
/// ⚠️ 分派依据是**状态表那一列**（`kind`），不是 `record.status` ——
/// 原先那两处 `if status == …` 是「再跑一次会怎样」的第三、第四份答案。
fn run_final(worker_id: usize, id: &str, kind: JobKind, shared: &Shared) {
    let (record, recheck_timeout) = {
        let ctx = shared.lock();
        let Some(record) = ctx.state.get(id).cloned() else {
            shared.log(&format!("[w{worker_id}] {id} 无记录，跳过收尾"));
            return;
        };
        (record, ctx.recheck_timeout)
    };

    if kind == JobKind::Redownload {
        let artifacts = record.artifacts.clone().unwrap_or_default();
        if artifacts.is_empty() {
            // 连产物 URL 都没有：只能重新生成。
            shared.requeue(
                id,
                TaskStatus::Pending,
                RecordPatch {
                    error: Some(Some("无产物 URL，需重新生成".to_string())),
                    ..Default::default()
                },
            );
            shared.log(&format!("[w{worker_id}] {id} 下载失败且无 URL，回退为重新提交"));
            return;
        }
        shared.log(&format!(
            "[w{worker_id}] ⬇️ 补下载 {id}（{} 个产物）",
            artifacts.len()
        ));
        let (files, failed) = download_all(&artifacts, &record.prompt, id, shared);
        if failed.is_empty() {
            shared.finish(
                id,
                TaskStatus::Done,
                RecordPatch {
                    files: Some(Some(files.clone())),
                    error: Some(None),
                    ..Default::default()
                },
            );
            shared.log(&format!(
                "[w{worker_id}] ✅ {id} 补下载完成，{} 张",
                files.len()
            ));
        } else {
            // 仍是 DownloadFailed（非终态）：不回队，留到下次运行的收尾再试
            shared.update(
                id,
                RecordPatch {
                    status: Some(TaskStatus::DownloadFailed),
                    files: Some(Some(files)),
                    error: Some(Some(format!(
                        "补下载仍失败（{}/{}）：{}",
                        failed.len(),
                        artifacts.len(),
                        failed[0]
                    ))),
                    ..Default::default()
                },
            );
            shared.log(&format!(
                "[w{worker_id}] ⬇️ {id} 补下载仍失败（{}/{}），保留记账",
                failed.len(),
                artifacts.len()
            ));
        }
        return;
    }

    if kind == JobKind::RecheckFinal {
        let Some(thread_id) = record.thread_id.clone() else {
            shared.requeue(
                id,
                TaskStatus::Pending,
                RecordPatch {
                    error: Some(Some("无 threadId，需重新提交".to_string())),
                    ..Default::default()
                },
            );
            return;
        };
        shared.log(&format!(
            "[w{worker_id}] 🕓 兜底复查 {id}（thread={thread_id}）"
        ));
        poll_and_finish(worker_id, id, &thread_id, shared, recheck_timeout, true);
        return;
    }

    shared.log(&format!(
        "[w{worker_id}] {id} 内部 bug：{kind:?} 不该进收尾（已跳过，请报告）"
    ));
}

/// 轮询到终态并取件。
///
/// `is_final_sweep` = 是否处于收尾兜底阶段（超时不再回队，避免死循环）。
fn poll_and_finish(
    worker_id: usize,
    id: &str,
    thread_id: &str,
    shared: &Shared,
    task_timeout: Duration,
    is_final_sweep: bool,
) {
    let (prompt_text, deps, ports) = {
        let ctx = shared.lock();
        (
            ctx.state.get(id).map(|r| r.prompt.clone()).unwrap_or_else(|| id.to_string()),
            ctx.deps.clone(),
            Arc::clone(&ctx.ports),
        )
    };

    // 自动确认：`allow` 在"即将确认"那一刻求值（开关热更新）；计数器与轮询收场无关。
    let auto = Arc::clone(&shared.auto_confirm);
    let allow = || auto.decide(id);
    let result = match ports.poll(&deps, thread_id, task_timeout, &allow, auto.confirmed()) {
        Ok(result) => result,
        Err(error) => {
            handle_poll_error(worker_id, id, &error, shared, is_final_sweep);
            return;
        }
    };

    let urls = match ports.extract_artifacts(&result) {
        Ok(urls) => urls,
        Err(error) => {
            handle_poll_error(worker_id, id, &error, shared, is_final_sweep);
            return;
        }
    };

    let (files, failed) = download_all(&urls, &prompt_text, id, shared);

    if failed.is_empty() {
        shared.finish(
            id,
            TaskStatus::Done,
            RecordPatch {
                artifacts: Some(Some(urls)),
                files: Some(Some(files.clone())),
                error: Some(None),
                ..Default::default()
            },
        );
        shared.log(&format!(
            "[w{worker_id}] ✅ {id} 完成，{} 张 → {}",
            files.len(),
            files.join(", ")
        ));
        return;
    }

    // 生成已成功 ⇒ 服务端槽位已释放（`DownloadFailed` 不占槽）⇒ 只记账待补下载
    shared.finish(
        id,
        TaskStatus::DownloadFailed,
        RecordPatch {
            artifacts: Some(Some(urls.clone())),
            files: Some(Some(files)),
            error: Some(Some(format!(
                "产物下载失败（{}/{}）：{}",
                failed.len(),
                urls.len(),
                failed[0]
            ))),
            ..Default::default()
        },
    );
    shared.log(&format!(
        "[w{worker_id}] ⬇️ {id} 生成成功但下载失败（{}/{}），已记账，收尾/下次补下载",
        failed.len(),
        urls.len()
    ));
}

/// 轮询阶段的错误分类：终态 / 超时续查 / 可重试 / 硬失败。
fn handle_poll_error(
    worker_id: usize,
    id: &str,
    error: &LovartError,
    shared: &Shared,
    is_final_sweep: bool,
) {
    // 用户中断（Ctrl+C）：保留 threadId，下次续跑直接复查，**不判失败**。
    if shared.is_stopped() {
        shared.update(
            id,
            RecordPatch {
                status: Some(if is_final_sweep {
                    TaskStatus::Unresolved
                } else {
                    TaskStatus::Running
                }),
                error: Some(Some("已中断（下次续跑复查）".to_string())),
                ..Default::default()
            },
        );
        shared.log(&format!("[w{worker_id}] {id} 已中断，保留 threadId 待下次复查"));
        return;
    }

    // 429 = 限流：开全局熔断；**不回 Pending**（会重复提交生成）。
    if classify_error(error) == ErrorClass::RateLimit {
        shared.on_rate_limited();
        if is_final_sweep {
            // 收尾只跑一轮：只落状态，不 requeue（否则可能死循环）。
            shared.update(
                id,
                RecordPatch {
                    status: Some(TaskStatus::Unresolved),
                    ready_at: Some(0),
                    error: Some(Some(format!("限流，全局暂停中：{}", error_text(error)))),
                    ..Default::default()
                },
            );
        } else {
            shared.requeue(
                id,
                TaskStatus::Running,
                RecordPatch {
                    ready_at: Some(0),
                    error: Some(Some(format!("限流，全局暂停中：{}", error_text(error)))),
                    ..Default::default()
                },
            );
        }
        shared.log(&format!(
            "[w{worker_id}] ⏸️ {id} 轮询撞限流，保持 {}，等待全局熔断",
            if is_final_sweep { "unresolved" } else { "running" }
        ));
        return;
    }

    let (attempts, max_attempts, max_timeouts) = {
        let ctx = shared.lock();
        (
            ctx.state.get(id).map(|r| r.attempts).unwrap_or(1),
            ctx.max_attempts,
            ctx.max_timeouts,
        )
    };

    match error.kind() {
        ErrType::NoArtifact => {
            shared.finish(
                id,
                TaskStatus::NoArtifact,
                RecordPatch {
                    error: Some(Some(error_text(error))),
                    ..Default::default()
                },
            );
            shared.log(&format!(
                "[w{worker_id}] ⚠️ {id} 无产物（多为内容审核）：{}",
                error_text(error)
            ));
            return;
        }
        ErrType::PendingConfirmation => {
            // 未开启 / 本批已用过许可 / 自动确认失败 —— 三种都转人工，原因原样写在 `error` 里。
            // ⚠️ 日志不能再说"不自动扣积分"：自动确认失败时那句话是假的。
            shared.finish(
                id,
                TaskStatus::NeedsConfirm,
                RecordPatch {
                    error: Some(Some(error_text(error))),
                    ..Default::default()
                },
            );
            shared.log(&format!(
                "[w{worker_id}] ⚠️ {id} 转人工：{}",
                error_text(error)
            ));
            return;
        }
        ErrType::Timeout => {
            // 超时 ≠ 失败，但**槽位保留必须有上限**，否则服务端卡住的任务会永久占坑。
            if is_final_sweep {
                // 兜底阶段不再回队（那条队列只跑一轮），留到下次运行。
                // **刻意不计入 `timeout_count`**：那一列语义是「占着槽位超时了几次」，
                // 而兜底阶段本就不占槽位；记进去会让计数每次运行都虚增，失去含义。
                shared.update(
                    id,
                    RecordPatch {
                        status: Some(TaskStatus::Unresolved),
                        error: Some(Some(error_text(error))),
                        ..Default::default()
                    },
                );
                shared.log(&format!(
                    "[w{worker_id}] 🕓 {id} 兜底复查仍超时，保留 unresolved（下次运行再查）"
                ));
                return;
            }
            let count = {
                let ctx = shared.lock();
                ctx.state.get(id).and_then(|r| Some(r.timeout_count)).unwrap_or(0) + 1
            };
            if count < max_timeouts {
                shared.requeue(
                    id,
                    TaskStatus::Timeout,
                    RecordPatch {
                        timeout_count: Some(count),
                        error: Some(Some(error_text(error))),
                        ..Default::default()
                    },
                );
                shared.log(&format!(
                    "[w{worker_id}] ⏳ {id} 轮询超时 {count}/{max_timeouts}，保留槽位待复查"
                ));
            } else {
                // 释放槽位（`Unresolved` 不占槽），转收尾
                shared.requeue(
                    id,
                    TaskStatus::Unresolved,
                    RecordPatch {
                        timeout_count: Some(count),
                        error: Some(Some(error_text(error))),
                        ..Default::default()
                    },
                );
                shared.log(&format!(
                    "[w{worker_id}] 🕓 {id} 超时达上限 {count}/{max_timeouts}，释放槽位（收尾兜底复查）"
                ));
            }
            return;
        }
        ErrType::Abort => {
            shared.finish(
                id,
                TaskStatus::Failed,
                RecordPatch {
                    error: Some(Some(error_text(error))),
                    ..Default::default()
                },
            );
            shared.log(&format!(
                "[w{worker_id}] {id} 被上游中止：{}",
                error_text(error)
            ));
            return;
        }
        _ => {}
    }

    if is_retryable_error(error) && attempts < max_attempts {
        shared.requeue(
            id,
            TaskStatus::Pending,
            RecordPatch {
                attempts: Some(attempts + 1),
                error: Some(Some(error_text(error))),
                ..Default::default()
            },
        );
        shared.log(&format!(
            "[w{worker_id}] {id} 轮询失败（可重试 {attempts}/{max_attempts}）：{}",
            error_text(error)
        ));
        return;
    }

    shared.finish(
        id,
        TaskStatus::Failed,
        RecordPatch {
            error: Some(Some(error_text(error))),
            ..Default::default()
        },
    );
    shared.log(&format!(
        "[w{worker_id}] ❌ {id} 失败：{}",
        error_text(error)
    ));
}

// ── 入口 ──────────────────────────────────────────────────────────────

/// 派工：把**这些已存在的记录**送进队列。
///
/// ⚠️ **一个字段都不写回记录**。派工没有"声明"可对齐 —— 记录里的
///    `prompt` / `model` / `ratio` / `size` / **垫图** / 指纹就是真相，
///    所以这条路上**没有任何同步**。这正是与声明路径的分界。
///
/// 返回的 [`PromptTask`] 由记录**无损投影**（[`TaskRecord::to_prompt_task`]），
/// 供 worker 取用（提交时要拿垫图地址当 attachments）。
///


// ── 常驻调度器 ────────────────────────────────────────────────────────

/// 起调度器需要的全部配置（一次性给全，之后不再读环境）。
pub struct SchedulerConfig {
    pub deps: ClientDeps,
    pub state: StateStore,
    pub output_dir: PathBuf,
    pub report_file: Option<PathBuf>,
    pub tuning: SchedulerTuning,
    pub retry_failed: bool,
    pub auto_confirm: Arc<AutoConfirm>,
    pub log: LogFn,
    pub ports: Option<Arc<dyn Ports>>,
    /// 会话封口回调（GUI 用）；CLI 传 `None`。
    pub on_session_end: Option<SessionEndFn>,
}

/// 一次投喂的结果（日志与告警用）。
pub struct EnqueueOutcome {
    /// 本次投喂的任务（报告 `tasks_by_id` 的原料）。
    pub tasks: Vec<PromptTask>,
    /// **恒为 0**（历史字段）。
    ///
    /// 原名「因参数变更而重置重跑的条数」，但投喂现在**一律新建**、
    /// 不再重置已有记录（见 `intake::sync_declared`），所以这个数永远是 0。
    /// 保留字段是为了不牵动调用方；日志里那半句因此永远不会出现。
    pub param_changed: usize,
    /// 本次是否开启了新会话（`false` = 追加到已有会话）。
    pub opened: bool,
    pub session_id: Option<String>,
    /// 被拒绝的条目（已结束 / 无记录等），由调用方转成给用户看的提示。
    pub rejected: Vec<intake::Rejection>,
}

/// 进程内调度器。`start` 之后一直活着，直到 `shutdown`。
///
/// ⚠️ **唯一**写 `state.jsonl` 的组件。`gui` / `run` 都不许直接
/// `StateStore::open` 写 —— 那会与在飞任务互相覆盖（不变量 I1）。
#[derive(Clone)]
pub struct Scheduler {
    shared: Arc<Shared>,
}

impl Scheduler {
    /// 起 worker 池并进入空闲。**不**自动接管未完成记录（用户点了才跑）。
    pub fn start(config: SchedulerConfig) -> Result<Self, LovartError> {
        let SchedulerConfig {
            deps,
            state,
            output_dir,
            report_file,
            tuning,
            retry_failed,
            auto_confirm,
            log,
            ports,
            on_session_end,
        } = config;
        let SchedulerTuning {
            concurrency,
            task_timeout,
            recheck_timeout,
            max_timeouts,
            download_retries,
            max_attempts,
            max_busy_retries,
            busy_backoff_base,
            task_interval,
            rate_limit_backoff_base,
        } = tuning;

        let cancelled = Arc::clone(&deps.cancelled);
        let mut ctx = Ctx {
            deps,
            ports: ports.unwrap_or_else(|| Arc::new(RealPorts::new())),
            state,
            output_dir,
            concurrency,
            // 值已由 `SchedulerTuning` 兜过底（Settings 层 + Default），这里不再 unwrap_or：
            // 多一层兜底 = 多一份默认值 = 迟早和上面对不上。
            task_timeout,
            recheck_timeout,
            max_timeouts: max_timeouts.max(1),
            download_retries: download_retries.max(1),
            max_attempts,
            max_busy_retries: max_busy_retries.max(1),
            busy_backoff_base,
            task_by_id: BTreeMap::new(),
            queue: Vec::new(),
            processing: std::collections::BTreeSet::new(),
            removed: std::collections::BTreeSet::new(),
            gate: SubmitGate::new(task_interval, rate_limit_backoff_base),
            mode_logged: false,
            retry_failed,
            report_file,
            session: SessionState::new(),
        };

        // 启动修复：`submitting` 残留（进程被杀）改回 `pending`。
        // **只改状态、不入队** —— 不消耗积分，不算自动接管（09 §3.8）。
        let fixed = fix_orphan_submitting(&mut ctx);
        if fixed > 0 {
            log(&format!(
                "启动修复：{fixed} 条「提交中」残留已改回待提交（不会自动开跑）"
            ));
        }

        let shared = Arc::new(Shared {
            ctx: Mutex::new(ctx),
            log,
            cancelled,
            stopped: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            wake: Condvar::new(),
            handles: Mutex::new(Vec::new()),
            last_summary: Mutex::new(None),
            on_session_end,
            auto_confirm,
        });

        let mut handles = Vec::new();
        for worker_id in 1..=concurrency {
            let worker_shared = Arc::clone(&shared);
            handles.push(std::thread::spawn(move || {
                worker_loop(worker_id, &worker_shared);
            }));
        }
        *shared
            .handles
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = handles;

        Ok(Self { shared })
    }

    fn lock(&self) -> MutexGuard<'_, Ctx> {
        self.shared.lock()
    }

    fn log(&self, message: &str) {
        self.shared.log(message);
    }

    /// 唯一投喂入口：建队 + 唤醒。
    ///
    /// ⚠️ 建队（记录创建 / 参数变更检测）与「队列空」判定在同一把锁内，
    /// 所以不存在「刚判定空闲、随后又被投喂」的窗口。
    pub fn enqueue(&self, batch: Batch) -> Result<EnqueueOutcome, LovartError> {
        let declared = matches!(batch, Batch::Declared(_));
        let (outcome, log_line) = {
            let mut ctx = self.lock();
            // 投喂即恢复：`stop_session` 置的两个标志在这里复位。
            // ⚠️ 不复位的话，停止一次之后所有「失败重试」都会被 `requeue` 丢掉
            //    （`requeue` 在 stopped 时不入队）⇒ 任务静默消失，不报错。
            self.shared.stopped.store(false, Ordering::SeqCst);
            self.shared.cancelled.store(false, Ordering::SeqCst);
            let mut new_ids: Vec<String> = Vec::new();
            let mut rejected: Vec<intake::Rejection> = Vec::new();
            let tasks = match batch {
                Batch::Declared(tasks) => tasks,
                // 派工：**一个字段都不写回记录** —— 只把 id 送进队列；
                // 任务由记录**无损投影**得到（含已上传的垫图 URL 与指纹）。
                Batch::Records(ids) => {
                    let (records, rejects) = intake::enqueue_records(
                        &mut ctx.state,
                        &ids,
                        &mut new_ids,
                        &self.shared.log,
                    );
                    rejected.extend(rejects);
                    records
                }
            };
            let retry_failed = ctx.retry_failed;
            // 声明路径：参数以本次投喂为准（创建 / 重置 / 同步 + 入队）。
            // ⚠️ 只对「声明」生效，派工进来会按"声明里没有垫图"覆盖记录。
            let param_changed = if declared {
                let (changed, rejects, spawned) = intake::sync_declared(
                    &mut ctx.state,
                    &mut new_ids,
                    &tasks,
                    retry_failed,
                    &self.shared.log,
                );
                rejected.extend(rejects);
                // 「再要一张」新建的记录也要进 `task_by_id`：报告里的行号/模式靠它。
                for extra in spawned {
                    ctx.task_by_id.insert(extra.id.clone(), extra);
                }
                changed
            } else {
                0
            };

            // 初始化落盘：写不进去就**别开跑** —— 跑了也续不上，等于白跑几小时。
            if let PersistOutcome::Failed {
                message,
                failed_writes,
            } = ctx.state.flush()
            {
                return Err(LovartError::transport(format!(
                    "状态文件写不进去（{message}，失败 {failed_writes} 次）：断点续跑会失效，已中止启动"
                )));
            }

            for task in &tasks {
                ctx.task_by_id.insert(task.id.clone(), task.clone());
            }
            let mut seen: std::collections::BTreeSet<String> = ctx.queue.iter().cloned().collect();
            for id in new_ids {
                if seen.insert(id.clone()) {
                    ctx.queue.push(id);
                } else {
                    // 已在队列里（同一条正在跑/排队）：再发一次 = 要**更多张** ⇒ 新 id 新建。
                    let source = tasks.iter().find(|task| task.id == id).cloned();
                    match source.and_then(|task| intake::spawn_extra(&mut ctx.state, &task)) {
                        Some(extra) => {
                            seen.insert(extra.id.clone());
                            ctx.task_by_id.insert(extra.id.clone(), extra.clone());
                            ctx.queue.push(extra.id);
                        }
                        None => rejected.push(intake::Rejection::new(
                            id.as_str(),
                            "已在队列里且序号用满，没有新建",
                        )),
                    }
                }
            }
            let ids: Vec<String> = tasks.iter().map(|task| task.id.clone()).collect();
            let opened = ctx.session.record(ids);
            let session_id = ctx.session.id().map(str::to_string);
            // 自动确认计数按**会话**算：新会话从 0 开始（否则告警会把上一会话的算进来）。
            if opened {
                self.shared.auto_confirm.reset();
            }
            // 自动确认计数按**会话**算：新会话从 0 开始（否则告警会把上一会话的算进来）。
            if opened {
                self.shared.auto_confirm.reset();
            }
            let queue_len = ctx.queue.len();
            let concurrency = ctx.concurrency;
            let log_line = format!(
                "任务 {} 条：待处理 {queue_len}，并发 {concurrency}{}",
                tasks.len(),
                if param_changed > 0 {
                    format!("（参数变更重跑 {param_changed} 条）")
                } else {
                    String::new()
                }
            );
            (
                EnqueueOutcome {
                    tasks,
                    param_changed,
                    opened,
                    session_id,
                    rejected,
                },
                log_line,
            )
        };
        self.log(&log_line);
        self.shared.wake.notify_all();
        Ok(outcome)
    }

    /// 有活：队列非空或有在飞任务。**会话封口判据**（不变量 I3）。
    pub fn is_live(&self) -> bool {
        let ctx = self.lock();
        !ctx.queue.is_empty() || !ctx.processing.is_empty()
    }

    /// 取一份 `ClientDeps` 克隆（参考图上传等锁外工作要用）。
    pub(crate) fn deps(&self) -> ClientDeps {
        self.lock().deps.clone()
    }

    /// 本会话已自动确认条数（看板与告警用）。
    pub fn auto_confirmed(&self) -> usize {
        self.shared.auto_confirm.count()
    }

    /// 当前被持有的 id（待处理 ∪ 在飞）。界面进度条的**唯一**判据。
    pub fn held_ids(&self) -> std::collections::BTreeSet<String> {
        let ctx = self.lock();
        let mut held: std::collections::BTreeSet<String> = ctx.queue.iter().cloned().collect();
        held.extend(ctx.processing.iter().cloned());
        held
    }

    /// 停止本会话：置标志 + 清队列，**不删记录**（可重跑）。
    ///
    /// ⚠️ worker **不退出**（退出只看 `shutdown`）；下一次 `enqueue` 会复位
    /// `stopped` 与协议层 `cancelled`，所以「停止后再提交」是正常路径。
    pub fn stop_session(&self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        self.shared.cancelled.store(true, Ordering::SeqCst);
        {
            let mut ctx = self.lock();
            ctx.queue.clear();
        }
        self.shared.wake.notify_all();
        // 停止即「这次会话到此为止」（08 §1.3.4）：没有在飞任务就当场封口写报告；
        // 还有在飞任务时封不了，它们收尾时会各自走 `close_session_if_idle`。
        self.shared.close_session_if_idle();
    }

    /// 重跑某一条：锁内重置为待提交（**垫图与指纹原样保留**）。
    ///
    /// 走调度器而不是外部 `StateStore::open` —— 后者是第二个写者（不变量 I1）。
    pub fn reset_for_retry(&self, id: &str) -> Result<(), LovartError> {
        let mut ctx = self.lock();
        if ctx.removed.contains(id) {
            return Ok(());
        }
        crate::engine::state::reset_for_retry(&mut ctx.state, id)
            .map_err(|error| LovartError::transport(format!("重置失败：{error}")))?;
        let _ = ctx.state.flush();
        Ok(())
    }

    /// 外部改一条记录（「确认」后推回 `running`）。锁内执行，保持单写者。
    pub fn update_record(&self, id: &str, patch: RecordPatch) -> Result<(), LovartError> {
        let mut ctx = self.lock();
        if ctx.removed.contains(id) {
            return Ok(());
        }
        ctx.state
            .update(id, patch)
            .map_err(|error| LovartError::transport(format!("更新失败：{error}")))?;
        let _ = ctx.state.flush();
        Ok(())
    }

    /// 删除指定 id 的记录（锁内，唯一写者）。在飞的会给它留墓碑。
    pub fn remove(&self, ids: &[String]) -> Result<usize, LovartError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut ctx = self.lock();
        let count = ctx
            .state
            .remove(ids)
            .map_err(|error| LovartError::transport(format!("删任务失败：{error}")))?;
        let set: std::collections::BTreeSet<String> = ids.iter().cloned().collect();
        ctx.queue.retain(|id| !set.contains(id));
        for id in &set {
            if ctx.processing.remove(id) {
                ctx.removed.insert(id.clone());
            }
            ctx.task_by_id.remove(id);
        }
        Ok(count)
    }

    /// 删除**已结束**的记录（锁内）。
    pub fn remove_finished(&self) -> Result<usize, LovartError> {
        let ids: Vec<String> = {
            let ctx = self.lock();
            ctx.state
                .all()
                .into_iter()
                // 与旧的「清空已完成」同一条判据（看板的 `finished` 组），
                // 不用 `is_terminal`：后者会把终态的 `needs_confirm` 也算进来。
                .filter(|record| crate::run::is_board_finished(record.status))
                .map(|record| record.id.clone())
                .collect()
        };
        self.remove(&ids)
    }

    /// 取消所有未完成：**锁内**删除看板未结束的记录（含终态的 `needs_confirm`），
    /// 返回删除条数。
    ///
    /// 与旧实现（先停批 → 等线程退出 → 再删）相比不再需要等待：写者只有调度器自己，
    /// 删除与 worker 的写入天然串行。在飞任务随后收尾时命中墓碑，静默丢弃（§2.5）。
    pub fn remove_open(&self) -> Result<usize, LovartError> {
        // 先停：在飞任务收尾时走 `stopped` 分支（只改状态、不入队）。
        self.shared.stopped.store(true, Ordering::SeqCst);
        self.shared.cancelled.store(true, Ordering::SeqCst);
        // 立刻叫醒阻塞在 `wait` 的 worker，让它们重新评估 `stopped`
        // （否则下面 `remove` 万一失败提前返回，worker 还睡着）。
        self.shared.wake.notify_all();
        // 只负责「挑哪些 id」（锁内），清理统一交给 [`Self::remove`] ——
        // 队列/在飞/墓碑/task_by_id 的摘除逻辑只有那一份。
        let ids: Vec<String> = {
            let ctx = self.lock();
            ctx.state
                .all()
                .into_iter()
                .filter(|record| crate::run::is_board_open(record.status))
                .map(|record| record.id.clone())
                .collect()
        };
        let removed = self.remove(&ids)?;
        if removed > 0 {
            // 取消全部 = 这次会话到此为止（下一条投喂开新会话）。
            // 不写报告：用户要的是「清空」，一份空报告没有意义。
            let mut ctx = self.lock();
            let _ = ctx.session.close();
        }
        self.shared.wake.notify_all();
        Ok(removed)
    }

    /// 等空闲（`queue` 空且在飞归零）。**只给 CLI 的一次性跑批用**。
    ///
    /// 用轮询而不是 `Condvar`：若持锁等待，worker 收尾时要抢同一把锁 ⇒ 死锁。
    pub(crate) fn wait_idle(&self) {
        loop {
            {
                let ctx = self.lock();
                if ctx.queue.is_empty() && ctx.processing.is_empty() {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 关停：worker 退出 → join → 统计会话 → 写报告。
    ///
    /// 幂等：第二次调用时 `handles` 已空、摘要已取走 ⇒ 返回空摘要。
    pub fn shutdown(&self) -> Result<SchedulerSummary, LovartError> {
        self.shared.begin_shutdown();
        let handles = {
            let mut slot = self
                .shared
                .handles
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            std::mem::take(&mut *slot)
        };
        for handle in handles {
            if handle.join().is_err() {
                self.log("调度器内部错误：worker 线程异常退出");
            }
        }

        let (summary, report_file, tasks, output_dir, already_written) = {
            let mut ctx = self.lock();
            let last = self
                .shared
                .last_summary
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            let already_written = last.is_some();
            let summary = match last {
                Some(summary) => summary,
                None => {
                    let ids = ctx.session.all_ids().clone();
                    ctx.summarize_ids(&ids)
                }
            };
            (
                summary,
                ctx.report_file.clone(),
                ctx.task_by_id.clone(),
                ctx.output_dir.clone(),
                already_written,
            )
        };
        // 封口已经写过报告就不再写：否则每跑一批都会在主文件之外多留一份归档副本。
        if !already_written {
            if let Some(path) = report_file {
                write_report_file(&path, &summary, &tasks, &output_dir, &self.shared.log);
            }
        }
        Ok(summary)
    }
}

/// 一次性跑完：起调度器 → 投喂 → 等空闲 → 关停。**CLI 专用**。
pub fn run_scheduler(options: SchedulerOptions) -> Result<SchedulerSummary, LovartError> {
    let SchedulerOptions {
        deps,
        batch,
        state,
        output_dir,
        tuning,
        retry_failed,
        auto_confirm,
        log,
        ports,
        report_file,
    } = options;
    let scheduler = Scheduler::start(SchedulerConfig {
        deps,
        state,
        output_dir,
        report_file,
        tuning,
        retry_failed,
        auto_confirm,
        log,
        ports,
        // CLI 自己打印摘要，不需要回调。
        on_session_end: None,
    })?;
    scheduler.enqueue(batch)?;
    scheduler.wait_idle();
    scheduler.shutdown()
}

fn worker_loop(worker_id: usize, shared: &Shared) {
    // ⚠️ 退出条件**只有** `next_job` 返回 `None`（= shutdown）。
    //    「队列空」不再等于退出 —— 那正是「运行中加任务」必须改掉的一条。
    while let Some(job) = shared.next_job() {
        match job.kind {
            JobKind::Submit => run_submit(worker_id, &job.id, shared),
            JobKind::Recheck => run_recheck(worker_id, &job.id, shared),
            // 收尾两件活（只补下载 / 兜底复查）走同一入口，进去按 `kind` 分派。
            JobKind::Redownload | JobKind::RecheckFinal => {
                run_final(worker_id, &job.id, job.kind, shared)
            }
        }
        shared.finish_job(&job.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flag(value: bool) -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(value))
    }

    #[test]
    fn a1_关闭时不占位() {
        let enabled = flag(false);
        let auto = AutoConfirm::new(Arc::clone(&enabled));
        assert_eq!(auto.decide("a"), AutoConfirmDecision::Disabled);
        // 关着的时候不该消耗许可：打开后同一任务仍然允许。
        enabled.store(true, Ordering::Relaxed);
        assert_eq!(auto.decide("a"), AutoConfirmDecision::Allow);
    }

    #[test]
    fn a2_首次允许_再次已用() {
        let auto = AutoConfirm::new(flag(true));
        assert_eq!(auto.decide("a"), AutoConfirmDecision::Allow);
        assert_eq!(auto.decide("a"), AutoConfirmDecision::AlreadyUsed);
    }

    #[test]
    fn a3_不同任务各自允许() {
        let auto = AutoConfirm::new(flag(true));
        assert_eq!(auto.decide("a"), AutoConfirmDecision::Allow);
        assert_eq!(auto.decide("b"), AutoConfirmDecision::Allow);
    }

    #[test]
    fn a4_关掉后已用许可的任务报未开启() {
        let enabled = flag(true);
        let auto = AutoConfirm::new(Arc::clone(&enabled));
        assert_eq!(auto.decide("a"), AutoConfirmDecision::Allow);
        enabled.store(false, Ordering::Relaxed);
        assert_eq!(auto.decide("a"), AutoConfirmDecision::Disabled);
    }

    #[test]
    fn a5_每批独立() {
        let a = AutoConfirm::new(flag(true));
        let b = AutoConfirm::new(flag(true));
        a.confirmed().fetch_add(1, Ordering::Relaxed);
        assert_eq!(a.count(), 1);
        assert_eq!(b.count(), 0, "两个批次不许共用计数");
    }

    #[test]
    fn a6_计数单调() {
        let auto = AutoConfirm::new(flag(true));
        assert_eq!(auto.count(), 0);
        auto.confirmed().fetch_add(1, Ordering::Relaxed);
        auto.confirmed().fetch_add(1, Ordering::Relaxed);
        assert_eq!(auto.count(), 2);
    }
}
