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
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::engine::download::{download_artifact, slugify, DownloadedFile};
use crate::engine::state::{
    clear, is_terminal, is_unfinished, new_record, reset_for_retry, row as status_row, set,
    slots_in_use, Clearable, Job as JobKind, PersistOutcome, RecordPatch, StateStore, TaskRecord,
    TaskStatus,
};

/// 垫图地址的补丁值：**有就给、没有就显式清空**。
///
/// 「没有就清空」是刻意的：换了提示词/参考图之后，旧地址必须消失 ——
/// 留着它，界面会拿**上一张垫图**冒充这一张，而用户正是靠这个地址去核对垫图的。
fn ref_urls_of(task: &PromptTask) -> Clearable<Vec<String>> {
    match task.refs.clone() {
        Some(urls) if !urls.is_empty() => set(urls),
        _ => clear(),
    }
}
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

/// 全局任务间隔默认值（ms）。
const TASK_INTERVAL_DEFAULT_MS: u64 = 1_000;

/// 429 熔断基数 / 封顶（ms）。
const RATE_LIMIT_BACKOFF_BASE_MS: u64 = 60_000;
const RATE_LIMIT_BACKOFF_MAX_MS: u64 = 900_000;

/// 累计限流等待告警阈值（ms）。
const RATE_LIMIT_TOTAL_WAIT_ALERT_MS: u64 = 6 * 3600 * 1000;

/// 无活可干时的空转上限（ms）。
const IDLE_POLL_MS: u64 = 1_000;

/// 取消检查粒度（ms）。
const CANCEL_POLL_MS: u64 = 200;

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
}

pub struct SchedulerOptions {
    pub deps: ClientDeps,
    pub batch: Batch,
    pub state: StateStore,
    pub output_dir: PathBuf,
    pub concurrency: usize,
    pub task_timeout: Option<Duration>,
    pub recheck_timeout: Option<Duration>,
    pub max_timeouts: Option<u32>,
    pub download_retries: Option<u32>,
    pub max_attempts: Option<u32>,
    pub max_busy_retries: Option<u32>,
    pub busy_backoff_base: Option<Duration>,
    /// 全局任务间隔（提交，含重试）。缺省 1000ms。
    pub task_interval: Option<Duration>,
    /// 429 熔断基数（测试缝）。缺省 60s。
    pub rate_limit_backoff_base: Option<Duration>,
    /// 重置无 threadId 的 Failed 并重跑。
    pub retry_failed: bool,
    /// 自动确认策略（全局开关 + 本批计数 + 本批许可）。
    pub auto_confirm: Arc<AutoConfirm>,
    pub log: LogFn,
    /// 外部副作用出口；缺省用 [`RealPorts`]。
    pub ports: Option<Arc<dyn Ports>>,
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
    /// 全局提交闸门（间隔 + 429 熔断）。
    gate: SubmitGate,
    /// 排队模式是否已设置（惰性初始化，正常跑批只调 1 次）。
    mode_ready: bool,
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

/// 共享上下文。**只在取件 / 写状态这两小段持锁**，网络 I/O 一律在锁外。
struct Shared {
    ctx: Mutex<Ctx>,
    log: LogFn,
    cancelled: Arc<AtomicBool>,
    /// 自动确认策略（全局开关 + 本批计数 + 本批许可）。
    auto_confirm: Arc<AutoConfirm>,
    /// `set_mode` 的 once 守卫：只让一个 worker 进去，网络 I/O 不持 `Ctx` 锁。
    mode_init: Mutex<()>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Ctx> {
        self.ctx.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn log(&self, message: &str) {
        (self.log)(message);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// 惰性设置排队模式：once 守卫，正常跑批只调 1 次。
    fn ensure_mode(&self, deps: &ClientDeps, ports: &dyn Ports) -> Result<(), LovartError> {
        if self.lock().mode_ready {
            return Ok(());
        }
        let _guard = self
            .mode_init
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.lock().mode_ready {
            return Ok(());
        }
        ports.set_mode(deps, true)?;
        self.lock().mode_ready = true;
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
        let problem = ctx
            .state
            .update(id, patch)
            .err()
            .map(|error| format!("状态更新失败（内部 bug，请报告）：{error}"));
        if !ctx.queue.iter().any(|queued| queued == id) {
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
    fn take_job(&self) -> TakeOutcome {
        let mut ctx = self.lock();
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

// ── 单任务处理 ────────────────────────────────────────────────────────

fn sleep_or_abort(shared: &Shared, total: Duration) -> Result<(), LovartError> {
    let mut remaining = total;
    let step = Duration::from_millis(CANCEL_POLL_MS);
    while remaining > Duration::ZERO {
        if shared.is_cancelled() {
            return Err(LovartError::internal("已取消", ErrType::Abort));
        }
        let chunk = remaining.min(step);
        std::thread::sleep(chunk);
        remaining -= chunk;
    }
    Ok(())
}

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

    // 服务端模式是持久设置：只在首次提交时惰性设置一次（once 守卫）。
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
    if shared.is_cancelled() {
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
    if shared.is_cancelled() {
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
/// 两条判据与声明路径**同源**（都读状态表）：终态不处理；`job` 为空的
/// `submitting` 回退待提交（它没拿到 threadId，无从续查）。
fn enqueue_records(
    state: &mut StateStore,
    ids: &[String],
    queue: &mut Vec<String>,
    log: &LogFn,
) -> Vec<PromptTask> {
    let mut tasks = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(record) = state.get(id).cloned() else {
            log(&format!("派工：{id} 没有记录，跳过"));
            continue;
        };
        if is_terminal(record.status) {
            continue;
        }
        if status_row(record.status).job.is_none() {
            if let Err(error) = state.update(
                id,
                RecordPatch {
                    status: Some(TaskStatus::Pending),
                    error: Some(Some("上次提交未完成，重新提交".to_string())),
                    ..Default::default()
                },
            ) {
                log(&format!("重置「上次提交未完成」失败（内部 bug，请报告）：{error}"));
            }
        }
        queue.push(id.clone());
        tasks.push(record.to_prompt_task());
    }
    tasks
}

pub fn run_scheduler(options: SchedulerOptions) -> Result<SchedulerSummary, LovartError> {
    let SchedulerOptions {
        deps,
        batch,
        mut state,
        output_dir,
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
        retry_failed,
        auto_confirm,
        log,
        ports,
    } = options;

    let mut queue: Vec<String> = Vec::new();
    let mut param_changed = 0usize;

    // ── 建队：两种意图，两条路（理由见 [`Batch`]）──────────────────────
    let declared = matches!(batch, Batch::Declared(_));
    let tasks: Vec<PromptTask> = match batch {
        Batch::Declared(tasks) => tasks,
        // 派工：**一个字段都不写回记录** —— 只把 id 送进队列；
        // 任务由记录**无损投影**得到（含已上传的垫图 URL 与指纹）。
        Batch::Records(ids) => enqueue_records(&mut state, &ids, &mut queue, &log),
    };
    let task_by_id: BTreeMap<String, PromptTask> = tasks
        .iter()
        .map(|task| (task.id.clone(), task.clone()))
        .collect();

    // ⚠️ 这个循环**只对"声明"生效**。派工若进来，就会按"声明里没有垫图"去覆盖记录
    //    —— 那正是 2026-10-01 那个 bug（点确认后垫图消失）。派工的 id 已入队。
    let to_sync: &[PromptTask] = if declared { &tasks } else { &[] };
    for task in to_sync {
        let existing = state.get(&task.id).cloned();
        let Some(existing) = existing else {
            state.create(new_record(task));
            queue.push(task.id.clone());
            continue;
        };

        // 参数（模型/比例/尺寸/参考图）被改过：以当前文件为准。
        // id 只哈希提示词文本，不检测的话终态记录会静默沿用旧参数/旧参考图的产物。
        let ref_signature = task.ref_signature.clone().unwrap_or_default();
        let same_params = existing.model == task.model
            && existing.aspect_ratio == task.aspect_ratio
            && existing.image_size == task.image_size
            && existing.ref_signature.clone().unwrap_or_default() == ref_signature;

        if !same_params && is_terminal(existing.status) {
            if let Err(error) = state.update(
                &task.id,
                RecordPatch {
                    status: Some(TaskStatus::Pending),
                    model: Some(task.model.clone()),
                    aspect_ratio: Some(task.aspect_ratio.clone()),
                    image_size: Some(task.image_size.clone()),
                    attempts: Some(0),
                    timeout_count: Some(0),
                    busy_retries: Some(0),
                    ready_at: Some(0),
                    thread_id: clear(),
                    // 一并清掉 30% 锚点：重跑会拿到**新** threadId，进度应重新计时。
                    submitted_at: clear(),
                    project_id: clear(),
                    artifacts: clear(),
                    files: clear(),
                    error: clear(),
                    ref_signature: if ref_signature.is_empty() {
                        clear()
                    } else {
                        set(ref_signature.clone())
                    },
                    ref_urls: ref_urls_of(task),
                    ..Default::default()
                },
            ) {
                log(&format!("参数变更重置失败（内部 bug，请报告）：{error}"));
            }
            queue.push(task.id.clone());
            param_changed += 1;
            continue;
        }

        let mut patch = RecordPatch::default();
        if !same_params {
            patch.model = Some(task.model.clone());
            patch.aspect_ratio = Some(task.aspect_ratio.clone());
            patch.image_size = Some(task.image_size.clone());
            patch.ref_signature = if ref_signature.is_empty() {
                clear()
            } else {
                set(ref_signature.clone())
            };
            // 垫图地址与指纹**必须同进同退**：只改指纹不改地址，界面就会拿旧垫图当新的
            // （图换了、显示的地址没换 ⇒ 用户对着错的图查）。见 `TaskRecord::ref_urls`。
            patch.ref_urls = ref_urls_of(task);
        }
        if existing.line != Some(task.line) {
            patch.line = Some(task.line);
        }
        if patch != RecordPatch::default() {
            if let Err(error) = state.update(&task.id, patch) {
                log(&format!("参数同步失败（内部 bug，请报告）：{error}"));
            }
        }

        if is_terminal(existing.status) {
            if retry_failed
                && existing.status == TaskStatus::Failed
                && existing.thread_id.is_none()
            {
                match reset_for_retry(&mut state, &task.id) {
                    Ok(_) => {
                        queue.push(task.id.clone());
                        log(&format!("[retry-failed] 🔁 {} 已重置为待提交", task.id));
                    }
                    Err(error) => {
                        log(&format!("[retry-failed] 重置失败（内部 bug）：{error}"));
                    }
                }
            } else if retry_failed
                && existing.status == TaskStatus::Failed
                && existing.thread_id.is_some()
            {
                log(&format!(
                    "⚠️ {} 失败但已有 threadId，跳过 --retry-failed",
                    task.id
                ));
            }
            continue;
        }

        // ⚠️ 判据**读表**：非终态里**只有 `submitting` 不进队列**（`job` 为空）。
        //    它是「已认领、还没拿到 threadId」的那个 —— 进程在上次提交中途被杀，
        //    没有 threadId 无从续查 ⇒ 只能重新提交。
        //    （提交可能已成功但没落盘 —— 这个窗口无法消除，只能缩到最小。）
        //    上面 `is_terminal` 已经放行过，所以这里 `job` 为空 ⟺ `submitting`。
        //    原先写的是 `existing.status == Submitting` —— 同一个问题的第五份答案。
        if status_row(existing.status).job.is_none() {
            if let Err(error) = state.update(
                &task.id,
                RecordPatch {
                    status: Some(TaskStatus::Pending),
                    error: Some(Some("上次提交未完成，重新提交".to_string())),
                    ..Default::default()
                },
            ) {
                log(&format!("重置「上次提交未完成」失败（内部 bug，请报告）：{error}"));
            }
            queue.push(task.id.clone());
            continue;
        }

        queue.push(task.id.clone());
    }

    // 初始化落盘：写不进去就**别开跑** —— 跑了也续不上，等于白跑几小时。
    if let PersistOutcome::Failed {
        message,
        failed_writes,
    } = state.flush()
    {
        return Err(LovartError::transport(format!(
            "状态文件写不进去（{message}，失败 {failed_writes} 次）：断点续跑会失效，已中止启动"
        )));
    }

    let cancelled = Arc::clone(&deps.cancelled);
    let ctx = Ctx {
        deps,
        ports: ports.unwrap_or_else(|| Arc::new(RealPorts::new())),
        state,
        output_dir,
        concurrency,
        task_timeout: task_timeout.unwrap_or(Duration::from_millis(
            crate::lovart::config::TASK_TIMEOUT_MS,
        )),
        recheck_timeout: recheck_timeout.unwrap_or(Duration::from_secs(30 * 60)),
        max_timeouts: max_timeouts.unwrap_or(3).max(1),
        download_retries: download_retries.unwrap_or(3).max(1),
        max_attempts: max_attempts.unwrap_or(3),
        max_busy_retries: max_busy_retries.unwrap_or(300).max(1),
        busy_backoff_base: busy_backoff_base
            .unwrap_or(Duration::from_millis(BUSY_BACKOFF_BASE_MS)),
        task_by_id,
        queue,
        gate: SubmitGate::new(
            task_interval.unwrap_or(Duration::from_millis(TASK_INTERVAL_DEFAULT_MS)),
            rate_limit_backoff_base.unwrap_or(Duration::from_millis(RATE_LIMIT_BACKOFF_BASE_MS)),
        ),
        mode_ready: false,
    };
    let queue_len = ctx.queue.len();

    log(&format!(
        "任务 {} 条：待处理 {queue_len}，并发 {concurrency}{}",
        tasks.len(),
        if param_changed > 0 {
            format!("（参数变更重跑 {param_changed} 条）")
        } else {
            String::new()
        }
    ));

    let shared = Arc::new(Shared {
        ctx: Mutex::new(ctx),
        log: Arc::clone(&log),
        cancelled,
        auto_confirm,
        mode_init: Mutex::new(()),
    });

    let mut handles = Vec::new();
    for worker_id in 1..=concurrency {
        let shared = Arc::clone(&shared);
        handles.push(std::thread::spawn(move || {
            worker_loop(worker_id, &shared);
        }));
    }
    for handle in handles {
        let _ = handle.join();
    }

    // 收回 state 做摘要
    let shared = Arc::try_unwrap(shared)
        .map_err(|_| LovartError::transport("调度器内部错误：worker 未全部退出"))?;
    let ctx = shared
        .ctx
        .into_inner()
        .unwrap_or_else(|error| error.into_inner());
    let final_flush = ctx.state.flush();

    // 只统计「当前提示词文件」对应的记录，避免和 state 里的历史记录串味
    let current_ids: std::collections::BTreeSet<&str> =
        tasks.iter().map(|task| task.id.as_str()).collect();
    let records: Vec<TaskRecord> = ctx
        .state
        .all()
        .into_iter()
        .filter(|record| current_ids.contains(record.id.as_str()))
        .cloned()
        .collect();
    // 逐状态计数**由表生成** —— 不再手抄 7 个具名字段（那是第二份状态清单）。
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

    Ok(SchedulerSummary {
        total: tasks.len(),
        counts,
        unfinished: records
            .iter()
            .filter(|record| is_unfinished(record.status))
            .count(),
        persist_failed: !final_flush.is_ok(),
        records,
    })
}

fn worker_loop(worker_id: usize, shared: &Shared) {
    while !shared.is_cancelled() {
        match shared.take_job() {
            TakeOutcome::Job(job) => match job.kind {
                JobKind::Submit => run_submit(worker_id, &job.id, shared),
                JobKind::Recheck => run_recheck(worker_id, &job.id, shared),
                // 收尾两件活（只补下载 / 兜底复查）走同一入口，进去按 `kind` 分派。
                JobKind::Redownload | JobKind::RecheckFinal => {
                    run_final(worker_id, &job.id, job.kind, shared)
                }
            },
            TakeOutcome::Wait(ms) => {
                if sleep_or_abort(shared, Duration::from_millis(ms)).is_err() {
                    break;
                }
            }
            TakeOutcome::Empty => break,
        }
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
