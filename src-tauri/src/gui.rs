//! GUI —— Tauri 壳：前端的唯一入口。
//!
//! ── 职责边界 ────────────────────────────────────────────────────────
//! 这一层**只做三件事**：读状态给前端看 · 列 inbox 并校验 · 起/停一个批次。
//! 它**不重新实现任何业务规则** —— 校验走 `parse_prompts`、开跑走 `run_scheduler`、
//! 看板数据走 `run::build_status_json`，与 CLI 是**同一份代码**。
//! （若在这里抄第二份解析/渲染，"窗口里看到的"与"终端看到的"必然分叉。）
//!
//! ── 为什么前端用轮询而不是事件推送 ──────────────────────────────────
//! 状态文件是**唯一真相**，而写它的是调度器线程。与其维护一套事件协议、
//! 不如让前端每秒重读一次状态文件 —— 少一套会漂移的并行真相。
//! （而且这也让「另一个进程在跑批」时窗口照样能看，见方案的「观察模式」。）

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

use crate::engine::lockfile::FileLock;
use crate::engine::scheduler::{
    AutoConfirm, Batch, Scheduler, SchedulerConfig, SchedulerSummary, SchedulerTuning,
};
use crate::engine::settings::{self, Settings};
use crate::engine::state::{
    is_unfinished, row as status_row, Action, RecordPatch, StateStore, StateStoreOptions,
    TaskRecord, TaskStatus,
};
use crate::engine::text::{read_text, read_text_required};
use crate::lovart::connection::{
    load_dot_env, read_auto_confirm, reload_dot_env, resolve_connection, to_client_deps,
    update_env_keys, AUTO_CONFIRM_KEY,
};
use crate::lovart::prompt::{parse_prompts, ParseOptions, PromptDefaults};
use crate::run::build_status_json;

/// `get_state` 的「没变就别重算」缓存。
///
/// ── 为什么需要它 ────────────────────────────────────────────────────
/// `get_state` 每秒跑一次，而它做的事是**全量**的：读整个 `state.jsonl` →
/// 逐行反序列化 → 每条记录再 `serde_json::to_value` → 整体序列化给 IPC。
/// 实测 2263 条 = 一次约 200ms、载荷约 4MB；而其中 `finished` 的记录
/// **永远不再变**。界面静止时，这 200ms 与 4MB 全是白干。
///
/// ⇒ 指纹（[`view_key`]）没变就只回 active 那几条「已持续」文案（几十字节）。
/// 指纹变了才走全量。
#[derive(Default)]
struct ViewCache {
    /// 上次全量重算时的指纹。空串 = 没有缓存 / 被 [`GuiState::invalidate_view`] 作废。
    key: String,
    /// `(id, state_since)` —— 只有 `stateSinceText` 非空的那几条（= `active` 组）。
    ///
    /// 命中缓存时**只重算这几条文案**：它们是全量载荷里唯一每秒都在变的东西，
    /// 其余一个字节都没变。判据用的是 Rust 已经下发过的字段，不另写状态清单。
    elapsed: Vec<(String, Option<String>)>,
}

/// `get_state` 的指纹：**文件侧**（长度 + 修改时间）+ **内存侧**（看板 rev / 清空 / 自动确认）
/// + **输出目录**。
///
/// 文件侧用 `len + mtime` 而不是内容哈希：追加和压实都会改这两个值，
/// 而一次 `metadata()` 是微秒级（对照：2263 条全量重算是 200ms 级）。
///
/// ⚠️ 输出目录**必须进指纹**：设置里改了输出目录时 `state.jsonl` 一个字都不会变，
/// 但 `filesPresent`（绝对路径 + 在不在盘上）与 `missingArtifact` 全都变了。
/// 漏了它 ⇒ 改完设置，界面继续指着旧目录说"图在 / 图缺"。
fn view_key(
    state_file: &Path,
    output_dir: &Path,
    board_rev: u64,
    clearing: bool,
    auto_confirm: bool,
    auto_confirmed: usize,
) -> String {
    let (len, modified) = match std::fs::metadata(state_file) {
        Ok(meta) => (
            meta.len(),
            meta.modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0),
        ),
        // 文件还没建：与「存在但为空」不是一回事，用哨兵长度区分开。
        Err(_) => (u64::MAX, 0),
    };
    format!(
        "{len}:{modified}:{board_rev}:{auto_confirm}:{auto_confirmed}:{}:{}",
        if clearing { 'c' } else { '-' },
        output_dir.display()
    )
}

/// GUI 全局状态。
pub struct GuiState {
    cwd: PathBuf,
    /// 全局自动确认开关（运行态真源；与每个批次的 `AutoConfirm` 共享）。
    auto_confirm: Arc<AtomicBool>,
    /// `.env` 读-改-写互斥：保存设置与切开关会同时改同一个文件。
    env_lock: Mutex<()>,
    /// **批次级**告警 —— 用户**必须看到**、且看板**表达不了**的那些事。
    ///
    /// ── 为什么它取代了原来的 `log` ────────────────────────────────────
    ///
    /// 原来这里存「最近 300 条日志」，出口是 `get_log` 命令 —— **而前端一次都没调过它**
    /// （DEBT TD-05-15）。于是 `start_run` 里所有失败（打不开状态文件 / 参考图解析失败 /
    /// 批次返回 Err / 写报告失败）**只写进一个没人读的 Vec**，
    /// 界面照常显示「发布成功」—— **失败伪装成成功**（DEBT TD-05-21）。
    ///
    /// ── 为什么不是「把日志接上」而是换成一个告警列表 ──────────────────
    ///
    /// 调度器那 30 多条输出**绝大多数是过程**（「已提交」「复查」「补下载」），
    /// 而**任务级的结果（含失败原因）已经落在 `state.jsonl` 里** ——
    /// 看板就是它的显示器。往这里再灌一份，是**同一个事实的第二份真相**。
    ///
    /// ⇒ 边界划清：**任务级 = `state.jsonl`（看板）**；**批次级 = 这里（告警）**。
    /// 两者不重叠，谁也不替谁兜底。
    ///
    /// 与 `log` 的另一个区别：**告警不裁剪**。日志可以丢，告警丢了就是骗人。
    alerts: Mutex<Vec<String>>,
    /// 看板载荷的「没变就别重算」缓存（见 [`ViewCache`]）。
    view: Mutex<ViewCache>,
    /// 单实例锁（`engine::lockfile`）。由 `setup` 注入，**必须活到窗口关闭** ——
    /// 句柄一关，另一个进程就能进来写 `state.jsonl`。
    ///
    /// 用 `Mutex<Option<_>>` 而不是直接持有：`GuiState::new` 时 Tauri 还没起来，
    /// 拿不到对话框，抢锁失败要根据平台给用户一句人话（见 `run` 的 `setup`）。
    single_instance: Mutex<Option<FileLock>>,
    /// 常驻调度器（**惰性启动**：第一次投喂时才建，没配凭据的用户启动不报错）。
    scheduler: Mutex<Option<Scheduler>>,
    /// 投喂串行锁：`refs-cache.json` 是「读-合并-写」，两次并发投喂会互相覆盖
    /// （`docs/plan/09` §1.5.4）。代价可接受 —— 投喂是人工动作。
    intake_lock: Mutex<()>,
}

impl GuiState {
    pub fn new(cwd: PathBuf) -> Self {
        // 自动确认开关只认 `.env`（不读进程环境变量）。非法值 / 读失败 ⇒ 关 + 一条告警。
        let (auto_confirm, auto_confirm_warning) = read_auto_confirm(&cwd.join(".env"));
        let state = Self {
            cwd,
            alerts: Mutex::new(Vec::new()),
            view: Mutex::new(ViewCache::default()),
            auto_confirm: Arc::new(AtomicBool::new(auto_confirm)),
            env_lock: Mutex::new(()),
            single_instance: Mutex::new(None),
            scheduler: Mutex::new(None),
            intake_lock: Mutex::new(()),
        };

        // ⚠️ 启动就把 `.env` 载进来：设置里的「并发数 / 输出文件夹」也住在那里，
        //    不先载入，`get_settings` 读到的永远是默认值（= 设置界面是假的）。
        //
        // 失败**必须上报**：载入失败 ⇒ 后面所有读环境变量的地方都拿到默认值 ⇒
        // 设置界面显示的是**假配置**。原先是 `let _ =`，等于把这件事藏起来（DEBT TD-05-24）。
        if let Err(error) = load_dot_env(&state.cwd.join(".env")) {
            state.push_alert(&format!("配置载入失败，界面上的设置可能不是真实配置：{error}"));
        }
        if let Some(warning) = auto_confirm_warning {
            state.push_alert(&warning);
        }
        // 设置里的非法值（兜底参数 + 新增的 8 个键）：**启动就说**，
        // 否则用户会以为设置生效了。此刻 `from_env` ≈ 文件（`load_dot_env` 刚跑完）。
        let (_, settings_warnings) = Settings::from_env();
        for warning in settings_warnings {
            state.push_alert(&warning);
        }
        state
    }

    /// 收下单实例锁（`run` 的 `setup` 调用，抢到之后）。
    ///
    /// ⚠️ 不提供「没锁也能跑」的分支：抢锁失败时调用方必须让进程退出，
    /// 而不是带着半个锁继续（两个写者 = 记录被覆盖）。
    fn hold_lock(&self, lock: FileLock) {
        let mut slot = self
            .single_instance
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *slot = Some(lock);
    }

    /// 取（必要时创建）常驻调度器。
    ///
    /// ⚠️ 返回**克隆**而不是 guard：调用方随后会锁 `Mutex<Ctx>`，若拿着槽锁去锁它，
    /// 任何反向路径都会死锁（`docs/plan/09` §1.5.3）。
    fn ensure_scheduler(&self, app: &tauri::AppHandle) -> Result<Scheduler, String> {
        let mut slot = self
            .scheduler
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(scheduler) = slot.as_ref() {
            return Ok(scheduler.clone());
        }

        // 每次创建前重新载入 `.env`（覆盖语义）：手改后点提交仍要用新值。
        reload_dot_env(&self.cwd.join(".env"))?;
        let (settings, warnings) = Settings::from_file(&self.cwd.join(".env"))
            .map_err(|error| format!("读不了 .env：{error}"))?;
        for warning in &warnings {
            self.push_alert(&format!("⚠️ {warning}"));
        }
        let connection = resolve_connection()?;
        let deps = to_client_deps(&connection)?;
        crate::engine::runlog::init(self.cwd.join("var/run.log"));

        // 落盘失败 ⇒ 跑的中途就上报一次（`on_write_error` 只调一次，不刷屏）。
        let alert_handle = app.clone();
        let store = StateStore::open_with(
            &self.state_file(),
            StateStoreOptions {
                on_write_error: Some(Box::new(move |message: &str| {
                    if let Some(state) = alert_handle.try_state::<GuiState>() {
                        state.push_alert(&format!(
                            "状态写不进磁盘（断点续跑会失效，重启后进度可能全丢）：{message}"
                        ));
                    }
                })),
            },
        )
        .map_err(|error| format!("打不开状态文件：{error}"))?;

        let auto_confirm = AutoConfirm::new(Arc::clone(&self.auto_confirm));
        // 会话封口回调：把「本会话跑完」变成告警与日志行（原先这段在 `run_scheduler` 返回后）。
        let auto_for_cb = Arc::clone(&auto_confirm);
        let end_handle = app.clone();
        let on_session_end: crate::engine::scheduler::SessionEndFn = Arc::new(
            move |summary: &SchedulerSummary| {
                let counts: Vec<String> = summary
                    .counts
                    .iter()
                    .filter(|(_, count)| **count > 0)
                    .map(|(key, count)| format!("{key}={count}"))
                    .collect();
                crate::engine::runlog::line(&format!(
                    "会话结束：共 {} 条（{}），未完成 {} 条",
                    summary.total,
                    counts.join(" "),
                    summary.unfinished
                ));
                let Some(state) = end_handle.try_state::<GuiState>() else {
                    return;
                };
                if summary.persist_failed {
                    state.push_alert(
                        "收尾时状态文件写不进去：本次进度可能续不上，重启后需要重新提交",
                    );
                }
                let confirmed = auto_for_cb.count();
                if confirmed > 0 {
                    state.push_alert(&format!(
                        "本会话自动确认 {confirmed} 条（已扣积分，明细以上游账单为准）"
                    ));
                }
            },
        );

        let scheduler = Scheduler::start(SchedulerConfig {
            deps,
            state: store,
            output_dir: self.cwd.join(&settings.output_dir),
            report_file: Some(self.cwd.join("var/report.csv")),
            tuning: SchedulerTuning::from_settings(&settings),
            retry_failed: false,
            auto_confirm,
            log: Arc::new(|message: &str| crate::engine::runlog::line(message)),
            ports: None,
            on_session_end: Some(on_session_end),
        })
        .map_err(|error| format!("调度器启动失败：{}", error.message()))?;

        *slot = Some(scheduler.clone());
        Ok(scheduler)
    }

    /// 取调度器克隆（`None` = 还没创建）。不把 guard 交出去，避免与 ctx 锁嵌套。
    fn scheduler_snapshot(&self) -> Option<Scheduler> {
        self.scheduler
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn state_file(&self) -> PathBuf {
        self.cwd.join("var/state.jsonl")
    }

    /// 作废看板缓存 ⇒ 下一轮 `get_state` 强制全量重算。
    ///
    /// ⚠️ **上游探针必须调它**：探针改的是界面标题行那句结论，而它是 JS 里拼的、
    /// 不落 `state.jsonl` ⇒ 文件指纹不变。不作废，标题行会永远停在「探测中…」。
    fn invalidate_view(&self) {
        let mut view = self.view.lock().unwrap_or_else(|error| error.into_inner());
        view.key.clear();
    }

    fn output_dir(&self) -> PathBuf {
        // 1Hz 的 `get_state` 会调它：读环境（微秒级），不读盘。
        // 手改 `.env` 的目录要等点「开始」才统一（N19）—— 这里刻意不跟文件，
        // 否则每秒做一次磁盘 I/O。
        self.cwd.join(Settings::from_env().0.output_dir)
    }

    /// 记一条**批次级**告警（用户必须看到的事）。
    ///
    /// 只做「追加」——**不裁剪**。日志可以丢，告警丢了就是骗人。
    /// 消费方是 [`take_alerts`]：前端每轮拉走，拉走即清（不重复弹）。
    fn push_alert(&self, message: &str) {
        // 告警也落日志：弹窗**关掉就没了**，而挂机出问题时人不在电脑前。
        crate::engine::runlog::line(&format!("⚠️ 告警：{message}"));
        let mut alerts = self.alerts.lock().unwrap_or_else(|error| error.into_inner());
        alerts.push(format!(
            "[{}] {message}",
            chrono::Local::now().format("%H:%M:%S")
        ));
    }
}

// ── 命令 ──────────────────────────────────────────────────────────────

/// 一次 `get_state` 需要的**内存侧面**（与磁盘无关）。
///
/// 抽成结构体的理由：`get_state` 要 `await` 读盘，而锁不能跨 `await` 持有；
/// 同时它让"读哪些内存、拼什么指纹"变成一个可以直接单测的东西（见文件末尾 v1–v4）。
struct ViewInput {
    state_file: PathBuf,
    output_dir: PathBuf,
    batch_label: Option<String>,
    batch_ids: BTreeSet<String>,
    auto_confirmed: usize,
    clearing: bool,
    auto_confirm: bool,
    board_rev: u64,
}

impl ViewInput {
    fn key(&self) -> String {
        view_key(
            &self.state_file,
            &self.output_dir,
            self.board_rev,
            self.clearing,
            self.auto_confirm,
            self.auto_confirmed,
        )
    }
}

/// 取内存侧快照：只碰锁与原子量，**不读盘**。
fn view_input(state: &GuiState) -> ViewInput {
    let state_file = state.state_file();
    let output_dir = state.output_dir();
    let auto_confirm = state.auto_confirm.load(Ordering::Relaxed);

    // 判据只有一处：**调度器**说的是不是真话。先克隆再放槽锁，避免与 ctx 锁长嵌套。
    let scheduler = {
        let slot = state
            .scheduler
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        slot.clone()
    };
    // 计数只在本会话存在时有意义；没有就是 0（不伪造"上次跑到多少"）。
    let (batch_label, batch_ids, auto_confirmed, running) = match scheduler {
        Some(scheduler) if scheduler.is_live() => (
            Some("处理中".to_string()),
            scheduler.held_ids(),
            scheduler.auto_confirmed(),
            true,
        ),
        _ => (None, BTreeSet::new(), 0, false),
    };

    ViewInput {
        state_file,
        output_dir,
        batch_label,
        batch_ids,
        auto_confirmed,
        // 清空已改成锁内同步完成，不再有"清空中"这个中间态。
        clearing: false,
        auto_confirm,
        // 指纹只要「起停」这一位：`held_ids` 每秒都在变，进指纹会让缓存永远失效。
        board_rev: u64::from(running),
    }
}

/// 缓存命中 ⇒ `{ unchanged, elapsed }`；未命中 ⇒ `None`（调用方去读盘）。
///
/// ⚠️ 命中时**仍然重算**那几条「已持续」：它们是全量载荷里唯一每秒都在变的东西。
/// 文案实现只有 `run::active_elapsed_text` 一份，这里不自己拼字。
fn cached_payload(view: &Mutex<ViewCache>, key: &str) -> Option<Value> {
    let view = view.lock().unwrap_or_else(|error| error.into_inner());
    if view.key != key {
        return None;
    }
    let elapsed: Vec<Value> = view
        .elapsed
        .iter()
        .filter_map(|(id, since)| {
            crate::run::active_elapsed_text(since.as_deref())
                .map(|text| json!({ "id": id, "text": text }))
        })
        .collect();
    Some(json!({ "unchanged": true, "elapsed": elapsed }))
}

/// 全量载荷的收尾：并入内存侧字段、逐条 `following`，并把指纹与 elapsed 写回缓存。
///
/// ⚠️ **只在读盘成功之后调** —— 失败时不落指纹，下一轮自然会重算。
fn finish_payload(view: &Mutex<ViewCache>, input: &ViewInput, key: String, mut payload: Value) -> Value {
    if let Some(object) = payload.as_object_mut() {
        object.insert("running".to_string(), Value::Bool(input.batch_label.is_some()));
        object.insert("clearing".to_string(), Value::Bool(input.clearing));
        object.insert(
            "runningBatch".to_string(),
            input
                .batch_label
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        object.insert("autoConfirm".to_string(), Value::Bool(input.auto_confirm));
        object.insert("autoConfirmed".to_string(), json!(input.auto_confirmed));

        let mut elapsed: Vec<(String, Option<String>)> = Vec::new();
        if let Some(tasks) = object.get_mut("tasks").and_then(Value::as_array_mut) {
            for task in tasks {
                let Some(id) = task.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let following = input.batch_ids.contains(id);
                // 缓存只收**已经有文案**的那几条（= `active` 组）：判据仍是 Rust 自己
                // 下发的 `stateSinceText`，不在这里另写一遍"哪些状态要计时"。
                if task
                    .get("stateSinceText")
                    .is_some_and(|value| !value.is_null())
                {
                    elapsed.push((
                        id.to_string(),
                        task.get("stateSince")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    ));
                }
                if let Some(task) = task.as_object_mut() {
                    task.insert("following".to_string(), Value::Bool(following));
                }
            }
        }

        let mut cached = view.lock().unwrap_or_else(|error| error.into_inner());
        cached.key = key;
        cached.elapsed = elapsed;
    }
    payload
}

/// 看板数据（与 `--status --json` **同一份构造**）。
///
/// ── 「在跑」的判据**收口在这里**（DEBT TD-05-22）────────────────────
///
/// 前端原先拿两个字段自己推：任务列表来自 `state.jsonl`（**磁盘**），
/// `runningBatch` 来自 `state.running`（**内存**）。程序重启过之后，
/// 磁盘上仍留着 `running`（那是断点续跑的锚，**故意**的），而内存里没有批次
/// ⇒ 界面左边写「5 在跑」、右边「停止」却是灰的 —— **两句话必有一句是假**。
///
/// ⇒ 现在由这里判定一次，前端只负责显示：
///   · `running`       —— **本地是否真的有批次在跟**（唯一的「在跑」判据）
///   · `runningBatch`  —— 批次的标签（没有则 `null`）
///   · `following`     —— **逐条**：这条是否被本批次接管（见下面「进度条」那段）
///
/// 两者都在这一份 payload 里 ⇒ 前端**不可能**再拼出自相矛盾的两句话。
///
/// ── `following` 为什么必须**逐条**下发 ─────────────────────────────
/// `running`（有批次在跟）是**批次级**的，而界面上每一张卡各画一条进度条。
/// 只给批次级布尔，就会出现「窗口里写着处理中、其中几条其实**谁也没在查**」——
/// 它们不是"正在跑"，是**上次被中断留下的残记录**（调度器的队列只由当前提示词
/// 文件派生，不含它们的 id 就永远不会被复查）。
/// ⇒ 判据下沉到每一条：`following = 有批次 && 这条 id 在本批次的 ids 里`。
///
/// ── 每秒一次，但**大多数秒什么都没变** ─────────────────────────────
/// 指纹（[`view_key`]）没变就只回 `{ unchanged, elapsed }`（几十字节），
/// 不再读盘、不再序列化（见 [`ViewCache`]）。指纹一变仍走全量 —— 也就是
/// 这个函数在任何时刻返回的都是"真话"，只是**没变的时候不必把同一句话重说一遍**。
#[tauri::command]
async fn get_state(state: tauri::State<'_, GuiState>) -> Result<Value, String> {
    // ── 为什么是 `async`（与 `probe_unfinished` 同一条理由）──────────
    // Tauri 的**同步**命令在主线程上 inline 执行，而这里要读整个 `state.jsonl`
    // 再整体序列化给 IPC（实测 2263 条 ≈ 200ms）。每秒堵主线程 200ms = 每秒掉一批帧。
    // ⇒ 读盘那半段挪进 `spawn_blocking`，界面线程始终能响应。
    //
    // 三段：① 取内存快照（便宜）→ ② 指纹命中就直接回 → ③ 否则读盘 + 收尾。
    let input = view_input(&state);
    let key = input.key();
    if let Some(hit) = cached_payload(&state.view, &key) {
        return Ok(hit);
    }
    let read_file = input.state_file.clone();
    let read_dir = input.output_dir.clone();
    let built =
        tauri::async_runtime::spawn_blocking(move || build_status_json(&read_file, &read_dir))
            .await
            .map_err(|error| format!("状态线程异常退出：{error}"))?;
    Ok(finish_payload(&state.view, &input, key, built?))
}

/// 取走**尚未读过的**批次级告警（取走即清，不重复弹）。
///
/// 这是 [`GuiState::push_alert`] 的**唯一消费口** —— 与「有产无消」的旧 `get_log`
/// 相对。前端每轮轮询调一次；返回非空就显示给用户。
#[tauri::command]
fn take_alerts(state: tauri::State<'_, GuiState>) -> Vec<String> {
    let mut alerts = state.alerts.lock().unwrap_or_else(|error| error.into_inner());
    std::mem::take(&mut *alerts)
}

/// 列 `inbox/*.jsonl`，**导入前就校验**（复用 `parse_prompts`，与 CLI 同一份契约）。
///
/// 为什么要在导入前校验：未知模型名在**出图时**才会静默退回默认模型 ——
/// 几百条一起跑就是整批跑错。宁可在这里就报出来。
#[tauri::command]
fn list_inbox(state: tauri::State<'_, GuiState>) -> Result<Value, String> {
    let dir = state.cwd.join("inbox");
    // 与真正开跑**同一份**兜底参数（`.env` 的 LOVART_DEFAULT_*）：导入前的校验/预览
    // 若用内置默认值，用户会看到"检查通过"而跑出来的是另一组参数。
    let (defaults, _) = PromptDefaults::from_env();
    let mut files: Vec<Value> = Vec::new();

    // 目录不存在 = **正常**（还没让 AI 写过东西）⇒ 空列表。
    // 其余读目录失败**照抛** —— 否则界面显示「inbox 里没有文件」，
    // 用户会以为目录是空的，而实际是读不了（DEBT TD-05-19 同母体）。
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!(
                "读不了 inbox 目录（{}）：{error}",
                dir.display()
            ))
        }
    };

    if let Some(entries) = entries {
        let mut paths: Vec<PathBuf> = entries
            // 单个目录项取不出来（多因权限）不影响其余项 —— 这里跳过是**有意的**，
            // 而目录本身打不开已经在上面抛掉了（那才是「整个列表不可信」的那一档）。
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        paths.sort();

        for path in paths {
            files.push(inspect_jsonl(&path, &defaults, true)?);
        }
    }

    Ok(json!({ "dir": dir.display().to_string(), "files": files }))
}

/// 校验一个 `.jsonl` → 前端列表行（**不移动、不导入**）。
///
/// 抽出来的理由：「inbox 里的文件」与「用户自己挑的文件」必须看到**同一份**检查结果 ——
/// 两处各写一遍，迟早一处漏字段、两处说法不一致。
///
/// 读失败**照抛**，绝不 `unwrap_or_default()` 伪装成空文件 ——
/// 空文本会被解析成「0 条 0 错」，界面于是显示「这个文件是空的」。
fn inspect_jsonl(path: &Path, defaults: &PromptDefaults, in_inbox: bool) -> Result<Value, String> {
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    let text = read_text(path)
        .map_err(|error| format!("读不了 {}：{error}", path.display()))?
        .ok_or_else(|| format!("文件在列出之后被移走了：{}（刷新重试）", path.display()))?;
    let parsed = parse_prompts(&text, defaults, &ParseOptions::default());
    let preview: Vec<String> = parsed
        .tasks
        .iter()
        .take(3)
        .map(|task| task.prompt.clone())
        .collect();
    Ok(json!({
        "name": name,
        "path": path.display().to_string(),
        "count": parsed.tasks.len(),
        "preview": preview,
        "errors": parsed.errors,
        "warnings": parsed.warnings,
        "ok": parsed.errors.is_empty() && !parsed.tasks.is_empty(),
        // 只有 inbox 里的文件导入后才会被归位（移进 `inbox/imported/`）——
        // 前端要据此说清楚「这个文件会不会被移动」。
        "inbox": in_inbox,
    }))
}

/// 弹原生「选择文件」对话框，返回用户选中的路径（取消 ⇒ `None`）。
///
/// 起始目录给 `inbox/`（多数时候文件在那儿），但**不限制**在那里 ——
/// 提示词文件可以放在任何目录，用户想导哪份就导哪份。
#[tauri::command]
fn pick_prompt_file(
    state: tauri::State<'_, GuiState>,
    app: tauri::AppHandle,
) -> Result<Option<String>, String> {
    let mut dialog = app
        .dialog()
        .file()
        .set_title("选择提示词文件（.jsonl）")
        .add_filter("提示词文件", &["jsonl"]);
    let inbox = state.cwd.join("inbox");
    if inbox.is_dir() {
        dialog = dialog.set_directory(inbox);
    }
    // `blocking_*` 只能在**非主线程**调用；同步 command 跑在 Tauri 的阻塞线程池里，安全。
    Ok(dialog
        .blocking_pick_file()
        .and_then(|file| file.into_path().ok())
        .map(|path| path.display().to_string()))
}

/// 校验用户自选的一个文件（**只读，不移动**）。
#[tauri::command]
fn inspect_prompt_file(state: tauri::State<'_, GuiState>, path: String) -> Result<Value, String> {
    let path = PathBuf::from(&path);
    if !path.extension().is_some_and(|ext| ext == "jsonl") {
        return Err(format!("不是 .jsonl 文件：{}", path.display()));
    }
    let (defaults, _) = PromptDefaults::from_env();
    let in_inbox = is_inbox_child(&state.cwd, &path);
    inspect_jsonl(&path, &defaults, in_inbox)
}

/// 起一个批次：把 inbox 里的某个文件交给调度器跑。
///
/// ⚠️ **绝不自动开跑** —— 只有用户点了「开始」才走到这里。这是方案的硬约束：
/// AI 写文件，人决定发不发。
///
/// ⚠️ **不是 `#[tauri::command]`**（2026-10-01，DEBT TD-05-15）：原先它注册进了
/// `generate_handler!`，但前端一次都没调过它 —— 而它只被 `publish_queue` /
/// `retry_one` / `retry_abnormal` **在 Rust 内部**调用。注册它等于**多开一个没守的入口**：
/// 前端可以传任意 `file` 路径直接开跑，绕过 `publish_queue` 的落盘与校验。
fn start_run(
    app: tauri::AppHandle,
    state: tauri::State<'_, GuiState>,
    source: RunSource,
) -> Result<(), String> {
    // ── 两种来源：声明 / 派工（理由见 [`RunSource`]）─────────────────
    let batch = match source {
        // 声明：编辑框投喂的任务，校验与 id 已在 `intake::tasks_from_inputs` 完成。
        RunSource::Declared(tasks) => Batch::Declared(tasks),
        // 派工：**不解析参考图** —— 记录里的垫图已经是 CDN URL，重传一遍纯属浪费。
        RunSource::Records(ids) => Batch::Records(ids),
    };

    // **惰性启动**：第一次投喂才读凭据 / 开状态文件。没配凭据的用户此刻才看到错，
    // 与改造前「点发布才报错」一致。
    let scheduler = state.ensure_scheduler(&app)?;

    // 参考图解析要联网 + 读写缓存 ⇒ 后台线程；投喂也在同一线程并持有 `intake_lock`
    // （`refs-cache.json` 是「读-合并-写」，并发投喂会互相覆盖）。
    let app_for_thread = app.clone();
    std::thread::spawn(move || {
        let Some(state) = app_for_thread.try_state::<GuiState>() else {
            return;
        };
        let _guard = state
            .intake_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());

        let mut batch = batch;
        // ⚠️ **只有"声明"要解析参考图**（派工的垫图已经是 CDN URL，重传一遍纯属浪费）。
        if let Batch::Declared(tasks) = &mut batch {
            let refs_cache = state.cwd.join("var/refs-cache.json");
            let deps = scheduler.deps();
            if let Err(message) = crate::run::resolve_refs_core(
                &deps,
                tasks,
                &state.cwd,
                &[],
                Some(refs_cache.as_path()),
            ) {
                state.push_alert(&format!("参考图处理失败，这一批没有开跑：{message}"));
                return;
            }
        }

        match scheduler.enqueue(batch) {
            Ok(outcome) => {
                if !outcome.opened {
                    // 会话内追加：与「批次开始」分开记，第二天能看出是中途加过任务。
                    crate::engine::runlog::line(&format!(
                        "===== 会话内追加：{} 条 =====",
                        outcome.tasks.len()
                    ));
                }
                // 被拒绝的条目必须**明说**：编辑框保留文字后用户会重复点提交，
                // 静默跳过就是「假成功」（界面说已提交，实际什么都没发生）。
                if !outcome.rejected.is_empty() {
                    let mut lines: Vec<String> = outcome
                        .rejected
                        .iter()
                        .take(3)
                        .map(|rejection| rejection.describe())
                        .collect();
                    if outcome.rejected.len() > 3 {
                        lines.push(format!("……另有 {} 条", outcome.rejected.len() - 3));
                    }
                    state.push_alert(&format!(
                        "有 {} 条没有提交：{}",
                        outcome.rejected.len(),
                        lines.join("；")
                    ));
                }
            }
            Err(error) => {
                state.push_alert(&format!("投喂失败：{}", error.message()));
            }
        }
    });

    Ok(())
}

/// 出图模型清单（前端下拉用）。
///
/// ⚠️ **与 `--list-models --json` 共用同一份投影**（`run::image_models_json`）——
/// 这里曾自己 `filter + map` 一遍，键名写成 `id`（CLI 那边是 `alias`）：
/// 同一份数据、同一个二进制、两个键名，正是「同一个事实两个投影」。
/// 别名才是本工具的对外契约（`prompts.jsonl` 的 `model` 字段写的就是它）。
#[tauri::command]
fn list_models() -> Vec<Value> {
    crate::run::image_models_json()
}

/// 读一个提示词文件 → 前端用的任务 JSON（**唯一投影**）。
///
/// ⚠️ 走的是与 CLI `--check` **同一条解析路径** —— 导入前就报错，
/// 而不是等跑完几百条才发现模型名写错了。
/// 读失败收口在 `engine::text`（TD-05-19）：**「读不出来」不许伪装成「文件是空的」**。
fn prompts_json(path: &Path) -> Result<Value, String> {
    let text = read_text_required(path).map_err(|error| error.to_string())?;
    let parsed = parse_prompts(&text, &PromptDefaults::from_env().0, &ParseOptions::default());
    if !parsed.errors.is_empty() {
        return Err(parsed.errors.join("\n"));
    }
    let tasks: Vec<Value> = parsed
        .tasks
        .iter()
        .map(|task| {
            json!({
                "prompt": task.prompt,
                "model": task.model,
                "aspectRatio": task.aspect_ratio,
                "imageSize": task.image_size,
                "refs": task.ref_inputs.clone().unwrap_or_default(),
            })
        })
        .collect();
    Ok(Value::Array(tasks))
}

/// 导入一个提示词文件：**读 + 校验**（`inbox/` 里的还要归位），返回任务给前端。
///
/// ⚠️ **归位（移进 `inbox/imported/`）只针对 `inbox/` 的直属文件** ——
/// 那批文件是「AI 写的、等人发」的，归位就是「已处理」的记录，
/// 记录**就是文件系统本身**（文件在 `imported/` 里），不另立一份会漂移的
/// 「已导入清单」（DEBT **TD-05-12**）。
///
/// 用户自己从别的目录挑来的文件**原地不动**：那是他的文件夹，程序不该替他搬。
/// 代价是同一份外部文件可以再导一遍 —— 这是**用户看得见的手动动作**
/// （队列里会多出那几条，发布前能数、能删），不是静默重复。
///
/// 顺序是**先读+校验、再归位**：
/// · 校验不过 ⇒ 文件留在原地（用户改完还能再来），也不会把错文件搬进 `imported/`；
/// · 归位失败 ⇒ `Err`（不静默）—— 否则用户以为导入成功，下次又导一遍，正好退回原 bug。
///
/// 追加 / 替换是**前端编辑区**的事（待发队列是前端的真相）⇒ 这里**不带 mode 参数**。
/// 校验待发 JSONL 文本 —— **复用 `parse_prompts`**（CLI `--check`、投喂前校验同一份）。
///
/// 返回 `{ tasks, errors }`：`errors` 非空时前端**不动待发**（用户可能改到一半）。
/// 不落盘：存不存、存哪儿，由用户点「存为 JSONL」时决定。
#[tauri::command]
fn validate_draft(text: String) -> Result<Value, String> {
    let (defaults, _) = PromptDefaults::from_env();
    let parsed = parse_prompts(&text, &defaults, &ParseOptions::default());
    let tasks: Vec<Value> = parsed
        .tasks
        .iter()
        .map(|task| {
            json!({
                "prompt": task.prompt,
                "model": task.model,
                "ratio": task.aspect_ratio,
                "size": task.image_size,
                // 前端队列的 `refs` 是 url 字符串数组（见 app.js 文件头的约定）。
                "refs": task.ref_inputs.clone().unwrap_or_default(),
            })
        })
        .collect();
    Ok(json!({ "tasks": tasks, "errors": parsed.errors }))
}

/// 把待发文本存成 `.jsonl`（路径由前端保存对话框给）。
///
/// 与 `prompts.jsonl` **完全同构** ⇒ 存下来的文件可以直接
/// `lovart-client --prompts 它 --check`，也能被「导入」读回来。
#[tauri::command]
fn export_draft(
    state: tauri::State<'_, GuiState>,
    app: tauri::AppHandle,
    text: String,
) -> Result<Option<String>, String> {
    let mut dialog = app
        .dialog()
        .file()
        .set_title("存为 JSONL")
        .add_filter("提示词文件", &["jsonl"])
        .set_file_name(format!(
            "prompts-{}.jsonl",
            chrono::Local::now().format("%Y%m%d-%H%M")
        ));
    // 起始目录给 `inbox/`：下次「导入」直接能在列表里看到它。
    let inbox = state.cwd.join("inbox");
    if inbox.is_dir() {
        dialog = dialog.set_directory(inbox);
    }
    let Some(file) = dialog.blocking_save_file() else {
        return Ok(None); // 用户取消
    };
    let path = file.into_path().map_err(|error| error.to_string())?;
    std::fs::write(&path, text).map_err(|error| format!("写不了 {}：{error}", path.display()))?;
    Ok(Some(path.display().to_string()))
}

#[tauri::command]
fn import_inbox(state: tauri::State<'_, GuiState>, file: String) -> Result<Value, String> {
    import_inbox_core(&state.cwd, Path::new(&file))
}

/// 导入的**核心** —— 不依赖 Tauri，集成测试直接打这里。
///
/// （`import_inbox` 只是把 `cwd` 从 `State` 里取出来。分开的理由与 `run::build_status_json`
/// 相同：**能力要有一个不绑框架的入口**，否则只能靠真跑 GUI 才能验证。）
pub fn import_inbox_core(cwd: &Path, file: &Path) -> Result<Value, String> {
    let value = prompts_json(file)?;
    // ⚠️ 只归位 `inbox/` 的**直属**文件。用户从别的目录挑来的文件属于他自己的文件夹：
    // 搬走 = 替用户动了别人的目录（且不可逆）。防重复导入靠前端队列里看得见的重复条目。
    if is_inbox_child(cwd, file) {
        move_to_imported(cwd, file)?;
    }
    Ok(value)
}

/// 这个文件是不是 `inbox/` 的直属子项（`inbox/imported/` 里的不算）。
///
/// 比较**父目录**而不是拼字符串：`..`、相对路径、符号链接都能对上真实位置。
fn is_inbox_child(cwd: &Path, file: &Path) -> bool {
    let Some(parent) = file.parent() else {
        return false;
    };
    let Ok(inbox) = std::fs::canonicalize(cwd.join("inbox")) else {
        return false;
    };
    std::fs::canonicalize(parent).is_ok_and(|dir| dir == inbox)
}

/// 把导入过的 inbox 文件移进 `inbox/imported/`。
///
/// 重名**加时间戳**，不覆盖 —— 同名文件很可能是**另一批**提示词，覆盖 = 丢数据。
fn move_to_imported(cwd: &Path, file: &Path) -> Result<(), String> {
    let dir = cwd.join("inbox").join("imported");
    std::fs::create_dir_all(&dir).map_err(|error| format!("建 inbox/imported 失败：{error}"))?;
    let name = file
        .file_name()
        .ok_or_else(|| format!("这个路径没有文件名：{}", file.display()))?;
    let mut target = dir.join(name);
    if target.exists() {
        let stem = file
            .file_stem()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_default();
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        target = dir.join(format!("{stem}-{stamp}.jsonl"));
    }
    std::fs::rename(file, &target).map_err(|error| format!("搬去 inbox/imported 失败：{error}"))
}

/// 起一批要跑的活 —— **两种来源**（语义分界见 `engine::scheduler::Batch`）。
///
/// ⚠️ **派工不走文件**：原先「重跑 / 接着查 / 确认」都是先**伪造**一份单行提示词文件
///    再交给调度器。伪造用的 `record_to_task` 只挑 4 个字段 ⇒ 垫图在那份"声明"里为空
///    ⇒ 调度器按「以文件为准」把记录里的垫图覆盖掉（2026-10-01 实测：确认后垫图消失）。
///    这里把两种来源分开：**声明以本次投喂为准，派工以记录为准**。
enum RunSource {
    /// 用户从编辑框投喂的任务（**声明**）：校验与 id 已由 `intake` 完成。
    Declared(Vec<crate::lovart::prompt::PromptTask>),
    /// 对已有记录的派工：只处理这些 id（**记录才是真相**）。
    Records(Vec<String>),
}

/// 发布队列 —— 落文件后走**与 CLI 同一条开跑路径**。
#[tauri::command]
fn publish_queue(
    app: tauri::AppHandle,
    state: tauri::State<'_, GuiState>,
    tasks: Vec<crate::engine::intake::TaskInput>,
) -> Result<(), String> {
    // 结构化入参：**不再**落 `queue.jsonl` 再读回来（那是同一份数据的第二次序列化）。
    // 校验 / id / 默认值全部由 `intake` 一处完成，与 CLI 解析文件共用同一批函数。
    let (defaults, _) = PromptDefaults::from_env();
    let parsed = crate::engine::intake::tasks_from_inputs(&tasks, &defaults)?;
    if parsed.is_empty() {
        return Err("待发是空的".to_string());
    }
    start_run(app, state, RunSource::Declared(parsed))
}

/// 重跑 = **显式**把这条重置为待提交。
///
/// ⚠️ 原先这件事是**副作用**：适配层伪造一个"没有垫图"的声明 ⇒ 调度器判定"参数变了"
///    ⇒ 顺手把记录重置了（**顺手也把垫图清掉了** ⇒ 于是只能加一条闸
///    「用了垫图就不许重跑」）。现在重置就写在下面：**垫图与指纹原样保留** ——
///    它们还是有效的 CDN 地址，重跑当然要接着用。
///
/// ⚠️ 必须做在起批**之前**：派工只处理**非终态**记录，终态的没重置就不会被处理。
fn reset_for_retry(state: &tauri::State<'_, GuiState>, id: &str) -> Result<(), String> {
    match state.scheduler_snapshot() {
        // 走调度器：锁内重置，与 worker 串行。跑批中也允许（旧的「先停止」闸撤掉）。
        Some(scheduler) => scheduler
            .reset_for_retry(id)
            .map_err(|error| error.message().to_string()),
        // 还没建调度器 ⇒ 没有写者，按旧路径。
        None => {
            let mut store =
                StateStore::open(&state.state_file()).map_err(|error| error.to_string())?;
            crate::engine::state::reset_for_retry(&mut store, id)
                .map_err(|error| format!("重置失败：{error}"))?;
            Ok(())
        }
    }
}

/// 重跑某一条（异常行菜单里的「重跑这一条」）。
#[tauri::command]
fn retry_one(
    app: tauri::AppHandle,
    state: tauri::State<'_, GuiState>,
    id: String,
) -> Result<(), String> {
    let store = StateStore::open_read_only(&state.state_file()).map_err(|error| error.to_string())?;
    let record = store
        .all()
        .into_iter()
        .find(|item| item.id == id)
        .ok_or_else(|| format!("找不到这条任务：{id}"))?
        .clone();
    if !crate::run::is_retryable(record.status) {
        return Err("这一条重跑没有意义（内容没过审 / 已按政策拒绝）".to_string());
    }
    // ⚠️ 原先这里有第二道闸：「用了垫图就不许重跑」。它不是设计，是在硬扛
    //    `record_to_task` 丢字段 —— 重跑必须重置记录，而旧路子的重置会把垫图一起清掉。
    //    现在重置是**显式**的、**垫图原样保留** ⇒ 闸撤掉，能力恢复。
    reset_for_retry(&state, &id)?;
    start_run(app, state, RunSource::Records(vec![id]))
}

/// 「接着查这一条」—— 把**这一条**未完成记录交回调度器
/// （有 `threadId` 就复查、有产物就只补下载，**不重复提交**）。
///
/// ── 与 [`retry_one`] 的分工（**别合并**）──────────────────────────
/// · `retry_one`：**重新生成**，会再花一次额度；
/// · 本命令：**接着查**，不重复提交。
/// 闸也不同：这里读**状态表**的 `actions` 列（有没有 `Resume`），
/// 不在这里写第二份"哪些状态能接着查"。
#[tauri::command]
fn resume_one(
    app: tauri::AppHandle,
    state: tauri::State<'_, GuiState>,
    id: String,
) -> Result<(), String> {
    let store = StateStore::open_read_only(&state.state_file()).map_err(|error| error.to_string())?;
    let record = store
        .all()
        .into_iter()
        .find(|item| item.id == id)
        .ok_or_else(|| format!("找不到这条任务：{id}"))?
        .clone();
    let row = crate::engine::state::row(record.status);
    if !row.actions.contains(&crate::engine::state::Action::Resume) {
        return Err(format!("「{}」没有「接着查」这个动作", row.label));
    }
    start_run(app, state, RunSource::Records(vec![id]))
}

/// 「确认并继续」**之前**先看一眼要花什么 —— **只读，不花钱**。
///
/// ── 为什么必须有这一步 ────────────────────────────────────────────
/// 官方要求：先把「这一步大约花多少积分」告诉用户，**等他明确回答**，才准确认。
/// 而那个数字**不在本地记录里**（`lovart/task.rs` 只把上游文案存进了 `error`，
/// 载荷本身没留）⇒ 只能在点的这一刻**现问一次上游**。
///
/// 这里走 `GET /chat/result`，把 `pending_confirmation` 那个载荷**原样**交给界面 ——
/// **不猜字段名**（官方文档只给了模板 `{estimated_cost}`，真实形状没实测过；
/// 猜一个字段名就是把不知道的数字写成肯定句）。
#[tauri::command]
fn preview_confirm(state: tauri::State<'_, GuiState>, id: String) -> Result<Value, String> {
    let (_record, thread_id) = confirmable(&state, &id)?;
    let deps = to_client_deps(&resolve_connection()?)?;
    let result =
        crate::lovart::client::get_result(&deps, &thread_id).map_err(|error| error.to_string())?;
    // ⚠️ 刻意分成 `found` + `detail` 两件，**不归一成一个 `null`**：
    //    「上游没给这个字段」与「上游给了个 null」是两回事，合成一个界面就只能猜。
    let detail = result.get("pending_confirmation").cloned();
    Ok(serde_json::json!({
        "found": detail.is_some(),
        "detail": detail.unwrap_or(Value::Null),
    }))
}

/// 「这条能不能确认」+ 取回它的 `threadId` —— **唯一一份判据**。
///
/// [`preview_confirm`] 与 [`confirm_task`] 共用它。两处各写一遍
/// 「读表有没有 `Confirm` / 有没有 `threadId`」就是同一能力的两份判据（一归闸）。
fn confirmable(
    state: &tauri::State<'_, GuiState>,
    id: &str,
) -> Result<(TaskRecord, String), String> {
    let store = StateStore::open_read_only(&state.state_file()).map_err(|error| error.to_string())?;
    let record = store
        .all()
        .into_iter()
        .find(|item| item.id == id)
        .ok_or_else(|| format!("找不到这条任务：{id}"))?
        .clone();
    drop(store);

    let row = status_row(record.status);
    if !row.actions.contains(&Action::Confirm) {
        return Err(format!("「{}」不需要确认", row.label));
    }
    let thread_id = record
        .thread_id
        .clone()
        .ok_or_else(|| "这条没有 threadId，无法确认".to_string())?;
    Ok((record, thread_id))
}

/// 「确认并继续」—— **会花积分的那一下**。确认后**立刻单独把这一条跑掉**。
///
/// ── 三个闸 ────────────────────────────────────────────────────────
/// ① 状态表说这条能确认吗（`actions` 列有没有 `Confirm`）—— 读表，不写第二份；
/// ② 有没有批次在跑（确认要改状态文件，和跑批写的是同一个文件 —— 与 `remove_tasks` 同一条闸）；
/// ③ 有没有 `threadId`（没有就无从确认）。
///
/// ── 为什么确认后必须推回 `running` ──────────────────────────────────
/// 上游收到确认会**继续生成**，本地必须接着按 threadId 轮询它。
/// 状态不推回去，这条就永远停在 `needs_confirm` —— **确认了却没人去取图**。
///
/// ⚠️ 它**不是队列任务**（表里 `needs_confirm` 的 `job` 是 `None`）：
/// 确认只能由人按，进了队列就是"下次跑批自动确认"，即静默花钱。
#[tauri::command]
fn confirm_task(
    app: tauri::AppHandle,
    state: tauri::State<'_, GuiState>,
    id: String,
) -> Result<String, String> {
    let (_record, thread_id) = confirmable(&state, &id)?;
    // 跑批中也允许确认：写回走调度器锁内，与 worker 串行（旧的「先停止」闸撤掉）。

    // ⚠️ **花钱的那一下，全仓只此一处**。
    let deps = to_client_deps(&resolve_connection()?)?;
    crate::lovart::client::confirm_thread(&deps, &thread_id)
        .map_err(|error| format!("确认失败：{error}"))?;

    // 上游会继续生成 ⇒ 推回 `running`，让轮询接管（表里 `running` 的 `job` = 复查）。
    // 走调度器（锁内）而不是外部 `StateStore::open`：保持「只有一个写者」（不变量 I1）。
    let patch = RecordPatch {
        status: Some(TaskStatus::Running),
        error: Some(None),
        ..Default::default()
    };
    match state.scheduler_snapshot() {
        Some(scheduler) => scheduler
            .update_record(&id, patch)
            .map_err(|error| error.message().to_string())?,
        None => {
            let mut store =
                StateStore::open(&state.state_file()).map_err(|error| error.to_string())?;
            store
                .update(&id, patch)
                .map_err(|error| format!("确认成功但状态没落盘（请报告）：{error}"))?;
        }
    }

    // A 方案：**立刻单独把这一条跑掉** —— **派工，不经过提示词文件**
    // （经过文件就会按"以文件为准"把记录里的垫图覆盖掉，那是本次修的 bug）。
    start_run(app, state, RunSource::Records(vec![id]))?;
    Ok("已确认，正在接着跑".to_string())
}

/// 重跑全部**可重跑**的异常。返回一句人话说明跳过了什么。
#[tauri::command]
fn retry_abnormal(app: tauri::AppHandle, state: tauri::State<'_, GuiState>) -> Result<String, String> {
    let store = StateStore::open_read_only(&state.state_file()).map_err(|error| error.to_string())?;
    let all: Vec<_> = store.all().into_iter().cloned().collect();
    let retryable: Vec<_> = all
        .iter()
        .filter(|record| crate::run::is_retryable(record.status))
        .collect();
    if retryable.is_empty() {
        return Err("没有可重跑的异常".to_string());
    }
    // ⚠️ 原先这里按"有没有垫图"筛一遍（有垫图的不许重跑）。那道闸的来源见 `retry_one`。
    //    现在重置是显式的、垫图原样保留 ⇒ **全都可重跑**。
    let ids: Vec<String> = retryable.iter().map(|record| record.id.clone()).collect();
    for id in &ids {
        reset_for_retry(&state, id)?;
    }
    let count = ids.len();
    start_run(app, state, RunSource::Records(ids))?;
    Ok(format!("已重跑 {count} 条"))
}

/// **自动探测**每条未完成记录在**上游**的真实状态。
///
/// ── 为什么必须有它（别再靠人去查）────────────────────────────────
/// `state.jsonl` 里的 `running` 只说明「**本地**上次提交过」，**不说明上游现在是什么样**：
/// 那条 thread 可能早就生成完了（本地没去取）、可能还在排队、也可能已被中止。
/// 没有它，想知道「这几条到底卡在哪」只能**手工拿密钥去问一遍上游** ——
/// 而那件事本来就是这个程序该做的：**要人去查，就是设计缺陷。**
///
/// ── 边界（刻意的）──────────────────────────────────────────────
/// · **只读**：只 `GET /chat/status`，不提交、不确认、不花钱、不动任何本地状态；
/// · **一条失败不影响其余**：每条各带 `error` 回来（探测失败 ≠ 这条没救，
///   更不许把它伪装成"还在跑"）；
/// · 没有 `threadId` 的（上次连提交都没跑完）返回 `upstream: null` —— 上游没有这个东西，
///   无从探测，这不是错误。
/// 探测的**实现**（不绑 Tauri，阻塞式网络请求全在这里）。
fn probe_unfinished_core(cwd: &Path) -> Result<Value, String> {
    load_dot_env(&cwd.join(".env"))?;
    let connection = resolve_connection()?;
    let deps = to_client_deps(&connection)?;
    let store = StateStore::open_read_only(&cwd.join("var/state.jsonl"))
        .map_err(|error| error.to_string())?;

    let mut probes: Vec<Value> = Vec::new();
    for record in store
        .all()
        .into_iter()
        .filter(|record| is_unfinished(record.status))
    {
        let thread = record.thread_id.clone().filter(|thread| !thread.is_empty());
        let Some(thread) = thread else {
            probes.push(json!({
                "id": record.id,
                "upstream": Value::Null,
                "reason": "上次没提交成功，上游没有这条",
            }));
            continue;
        };
        match crate::lovart::client::get_status(&deps, &thread) {
            Ok(value) => {
                let upstream = value
                    .as_object()
                    .and_then(|object| object.get("status"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                probes.push(json!({ "id": record.id, "upstream": upstream }));
            }
            // 探测失败**如实带回**：绝不在这里悄悄吞掉，也绝不猜成 "running"。
            Err(error) => {
                probes.push(json!({ "id": record.id, "error": error.message() }));
            }
        }
    }
    Ok(Value::Array(probes))
}

/// ⚠️ **必须是 `async`**：Tauri 的同步命令跑在**主线程**上，而这里每条都要发一次
/// HTTP（单次超时 60s）—— 写成同步就是"点开界面卡住几十秒"。
/// `async` + `spawn_blocking` 把阻塞的活挪到工作线程，界面始终能响应。
#[tauri::command]
async fn probe_unfinished(state: tauri::State<'_, GuiState>) -> Result<Value, String> {
    let cwd = state.cwd.clone();
    let probes = tauri::async_runtime::spawn_blocking(move || probe_unfinished_core(&cwd))
        .await
        .map_err(|error| format!("探测线程异常退出：{error}"))?;
    // ⚠️ 探测结论**不落** `state.jsonl`（它是界面标题行那句话的原料）⇒ 文件指纹不变。
    //    不作废缓存，标题行会永远停在「探测中…」（`upstreamSummary()` 进了前端签名，
    //    但签名只在收到全量载荷时才算）。
    state.invalidate_view();
    probes
}

/// **接着查**那些「没人跟」的未完成任务 —— 把它们重新交给调度器。
///
/// ── 与 [`retry_abnormal`] 的分工（**别合并**）────────────────────────
/// · `retry_abnormal`：**已到终态的异常** ⇒ 要**重新生成**，会再花一次额度；
/// · 本命令：**未到终态** ⇒ 只是**接着查**（有 threadId 就按 threadId 复查，
///   **不重复提交**）。两者的代价与风险完全不同，合并就是把它们混成一个。
///
/// ── 为什么必须有这个出口 ──────────────────────────────────────────
/// 调度器的队列**只由当前提示词文件派生**（见 `engine::scheduler` 的建队逻辑），
/// 所以上次被中断留下的 `running`，只要不在下一批的提示词里，就**永远没人再查它**：
/// 既不会完成，也不会变红。README 承诺的「下次自动复查」，GUI 这边**只有
/// 重新发布同一批提示词**才兑现 —— 这条命令是那个兑现口的显式按钮。
///
/// 界面上它只在**真有没人跟的未完成记录**时出现（见 `ui/app.js` 的 `renderBoard`），
/// 所以正常跑批时它是一个不存在的按钮，不会诱导用户乱点。
#[tauri::command]
fn resume_unfinished(app: tauri::AppHandle, state: tauri::State<'_, GuiState>) -> Result<String, String> {
    let store = StateStore::open_read_only(&state.state_file()).map_err(|error| error.to_string())?;
    let all: Vec<_> = store.all().into_iter().cloned().collect();
    let unfinished: Vec<_> = all
        .iter()
        .filter(|record| is_unfinished(record.status))
        .collect();
    if unfinished.is_empty() {
        return Err("没有未完成的任务".to_string());
    }
    let ids: Vec<String> = unfinished.iter().map(|record| record.id.clone()).collect();
    let count = ids.len();
    start_run(app, state, RunSource::Records(ids))?;
    Ok(format!("已接着查 {count} 条"))
}

/// 删除任务记录（**不动磁盘上的图**）。
///
/// 只有**一道**闸：**没有批次在跑** —— 删除是重写整个 `state.jsonl`，与调度器的追加写
/// 并发会静默冲掉数据（见 `StateStore::remove` 的注释）。
///
/// ⚠️ **2026-10-01：拆掉「只删已结束的」那道闸。**
///
/// 原注释的理由是「未完成的任务被删掉后，调度器仍会把它推进到终态并写回，于是删掉的行
/// 会复活」—— 但**复活只发生在跑批时**（调度器只在 `run()` 开头把未完成的收进队列），
/// 而上面那道闸已经保证没在跑。**没在跑，就没有任何东西会写回。**
///
/// 那道闸的真实效果是：程序重启后，`state.jsonl` 里留下的 `running` 残记录
/// **既显示成「处理中」、又删不掉** —— 用户面前摆着一个收拾不掉的烂摊子。
/// 这比「复活」更糟，因为复活至少还能再删一次。
///
/// 保留的代价（**由界面负责说清**）：未完成的任务，上游可能**还在生成**；
/// 删掉本地记录后没人去取那张图，它会变成上游的孤儿。所以前端删除前弹确认。
#[tauri::command]
fn remove_tasks(state: tauri::State<'_, GuiState>, ids: Vec<String>) -> Result<usize, String> {
    if ids.is_empty() {
        return Ok(0);
    }
    // 跑批中也能删：删除走调度器锁内执行，与 worker 的写入天然串行；
    // 在飞的那条会被记进墓碑，收尾时静默丢弃（不会再「复活」）。
    match state.scheduler_snapshot() {
        Some(scheduler) => scheduler
            .remove(&ids)
            .map_err(|error| error.message().to_string()),
        None => {
            let mut store = StateStore::open(&state.state_file()).map_err(|error| error.to_string())?;
            store.remove(&ids).map_err(|error| error.to_string())
        }
    }
}

/// 删除**看板未结束**的记录（`active` / `queued` / `waiting`），返回删除条数。
///
/// ── 为什么它不是一个 Tauri 命令 ──────────────────────────────────────
/// 它要跑在"批次已经退出、又没人能开新批"的那一段里（见 `GuiState::finish_batch`），
/// 由 `cancel_all_tasks` 与批次线程**共用同一个实现** —— 不另开第二个删除入口。
///
/// 判据用 [`crate::run::is_board_open`]（看板分组本体），**不是** `is_unfinished`：
/// 后者是 `!terminal`，会把终态的 `needs_confirm`（待处理）漏掉。
///
/// 一次 `remove` = 一次 `compact`（临时文件 + rename）⇒ 要么全删要么全不删。
pub fn remove_open_tasks_core(state_file: &Path) -> Result<usize, String> {
    let mut store = StateStore::open(state_file).map_err(|error| error.to_string())?;
    let ids: Vec<String> = store
        .all()
        .into_iter()
        .filter(|record| crate::run::is_board_open(record.status))
        .map(|record| record.id.clone())
        .collect();
    if ids.is_empty() {
        // 一条都没有时**不写盘**：没有要删的就别重写整个文件。
        return Ok(0);
    }
    store.remove(&ids).map_err(|error| error.to_string())
}

/// 删除**看板已完成**的记录（`finished` 组），返回删除条数。
///
/// 判据用 [`crate::run::is_board_finished`] —— 与 `remove_open_tasks_core` 同一份
/// 分组本体、互为补集，所以两个入口加起来正好覆盖全表、不会重叠也不会漏。
/// 一条都没有时**不写盘**（没有要删的就别重写整个文件）。
pub fn remove_finished_tasks_core(state_file: &Path) -> Result<usize, String> {
    let mut store = StateStore::open(state_file).map_err(|error| error.to_string())?;
    let ids: Vec<String> = store
        .all()
        .into_iter()
        .filter(|record| crate::run::is_board_finished(record.status))
        .map(|record| record.id.clone())
        .collect();
    if ids.is_empty() {
        return Ok(0);
    }
    store.remove(&ids).map_err(|error| error.to_string())
}

/// 清空所有已完成任务：删除 `finished` 组的全部记录，返回删除条数。
///
/// ── 与「取消所有未完成任务」的区别 ────────────────────────────────
/// 它**不碰批次**（已结束的记录本来没人在跟），所以不置 `wipe`、也不需要停批；
/// 但它和单删一样是**重写整个 `state.jsonl`**，因此共用那两道闸：
/// 清空未结束任务期间拒绝（`ensure_not_clearing`）、跑批中拒绝（追加写会打架）。
/// 只删记录，图片文件与报告都不动。
#[tauri::command]
fn clear_finished_tasks(state: tauri::State<'_, GuiState>) -> Result<usize, String> {
    match state.scheduler_snapshot() {
        // 调度器在 ⇒ 删除必须走它（唯一写者），否则重写文件会冲掉在飞任务刚写的行。
        Some(scheduler) => scheduler
            .remove_finished()
            .map_err(|error| error.message().to_string()),
        // 还没建调度器 ⇒ 没有写者在跑，按旧路径删。
        None => remove_finished_tasks_core(&state.state_file()),
    }
}

/// 取消所有未完成任务：停掉当前批次（若有）→ 等它退出 → 删除全部未结束记录。
///
/// 返回值：
/// - 空闲：`{"cleared": N, "stopping": false}`（N 就是实际删除条数）
/// - 跑批中：`{"cleared": 0, "stopping": true}`，最终条数在批次收尾时由告警给出
///
/// 为什么跑批中**立刻返回**而不是同步等 60 秒：取消是协作式的，
/// 等下去会把 UI 线程挂住（用户看到的是"点了没反应"）。
#[tauri::command]
fn cancel_all_tasks(state: tauri::State<'_, GuiState>) -> Result<Value, String> {
    let cleared = match state.scheduler_snapshot() {
        // 调度器在 ⇒ 锁内删：与 worker 的写入天然串行，不再需要「先停批、等退出」。
        Some(scheduler) => scheduler
            .remove_open()
            .map_err(|error| error.message().to_string())?,
        // 还没建调度器 ⇒ 没有写者在跑。
        None => remove_open_tasks_core(&state.state_file())?,
    };
    state.invalidate_view();
    Ok(json!({ "cleared": cleared }))
}

/// 在系统文件管理器里定位到某个文件（图片菜单的「打开所在位置」）。
#[tauri::command]
fn reveal_path(path: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg("-R").arg(&path);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("explorer");
        command.arg(format!("/select,{path}"));
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let parent = std::path::Path::new(&path)
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let mut command = std::process::Command::new("xdg-open");
        command.arg(parent);
        command
    };
    command.spawn().map_err(|error| error.to_string())?;
    Ok(())
}

/// 打开输出文件夹。
#[tauri::command]
fn open_output_dir(state: tauri::State<'_, GuiState>) -> Result<(), String> {
    let dir = state.output_dir();
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    reveal_path(dir.display().to_string())
}

/// 读设置（两个密钥来自 `.env`）。
///
/// ⚠️ `.env` **不存在**是正常状况（还没配过）⇒ 空；**读不出来**是异常 ⇒ 照抛。
/// 原先两者都是 `unwrap_or_default()` ⇒ 读失败时界面显示空白，
/// 用户以为密钥丢了（DEBT TD-05-19）。
#[tauri::command]
fn get_settings(state: tauri::State<'_, GuiState>) -> Result<Value, String> {
    // **读文件**，不读进程环境（`docs/plan/07` N7）：
    // 手改 `.env` 后打开设置页，若读环境会显示旧值，用户一点保存就把手改覆盖掉。
    // 设置页显示 = 文件内容 = 点「开始」后的生效值（`start_run` 会 reload）。
    let (current, warnings) = Settings::from_file(&state.cwd.join(".env"))?;
    Ok(json!({
        "accessKey": current.access_key,
        "secretKey": current.secret_key,
        "concurrency": current.concurrency,
        // 秒/分钟是**后端换算好**的：前端不做任何 `*1000` / `*60`。
        "taskIntervalSec": settings::ms_to_seconds(current.task_interval_ms),
        "outputDir": current.output_dir,
        "taskTimeoutMin": settings::ms_to_minutes(current.task_timeout_ms),
        "recheckTimeoutMin": settings::ms_to_minutes(current.recheck_timeout_ms),
        "maxAttempts": current.max_attempts,
        "maxTimeouts": current.max_timeouts,
        "maxBusyRetries": current.max_busy_retries,
        "downloadRetries": current.download_retries,
        "pollIntervalSec": current.poll_interval_ms / 1000,
        "pollMaxIntervalSec": current.poll_max_interval_ms / 1000,
        "defaultModel": current.default_model,
        "defaultRatio": current.default_ratio,
        "defaultSize": current.default_size,
        // 「单轮最长」也由后端算：前端插值显示即可。
        "maxWallClockMin": current.max_wall_clock_min(),
        "warnings": warnings,
    }))
}

/// 存设置：**按行**更新 `.env` 里那几个键，注释 / 空行 / 其它键原样保留。
///
/// ⚠️ 读失败**绝不能兜底成空**：那会把用户的配置整个冲掉（DEBT TD-05-19）。
#[tauri::command]
fn save_settings(state: tauri::State<'_, GuiState>, payload: Value) -> Result<Value, String> {
    let path = state.cwd.join(".env");
    let _guard = state
        .env_lock
        .lock()
        .unwrap_or_else(|error| error.into_inner());

    // 先读**文件当前值**：差异写靠它（N8），非法键修复也靠它（N16）。
    let (current, file_warnings) = Settings::from_file(&path)?;
    // 校验先行：任何一项非法 ⇒ `Err`，下面一个字都不写（不做部分保存）。
    let updates = settings::save_updates(&payload, &current)?;
    if !updates.is_empty() {
        let borrowed: Vec<(&str, String)> = updates
            .iter()
            .map(|(key, value)| (*key, value.clone()))
            .collect();
        update_env_keys(&path, &borrowed)?;
    }

    // ⚠️ 必须是 `reload_dot_env` 而不是 `load_dot_env`：后者是「不覆盖」语义，
    // 而 `GuiState::new` 启动时已经载入过一次 ⇒ 用它会**整段跳过**
    // —— 文件改了、运行时还用旧值（DEBT TD-05-11）。
    //
    // reload 失败**不回滚、不返回 Err**：盘已经改了，报错会让用户以为没保存而反复点。
    let mut warnings = file_warnings;
    if let Err(error) = reload_dot_env(&path) {
        warnings.push(format!(
            "已写入 .env，但重新载入失败：{error}；请重启后再跑"
        ));
    }
    // 保存后重算「单轮上限」：前端据此刷新只读提示，**不让前端自己算**（N17）。
    let max_wall_clock_min = Settings::from_file(&path)
        .map(|(saved, _)| saved.max_wall_clock_min())
        .unwrap_or_else(|_| current.max_wall_clock_min());
    Ok(json!({
        "warnings": warnings,
        "updated": updates.len(),
        "maxWallClockMin": max_wall_clock_min,
    }))
}

/// 切换全局自动确认开关 —— **唯一的写入口**（顶栏 chip 用它）。
///
/// ⚠️ 顺序必须是「先写盘成功、后改内存」：反过来会出现"界面显示开、磁盘是 false"，
/// 重启后悄悄变回去 —— 又一个"看着生效其实没生效"。
#[tauri::command]
fn set_auto_confirm(state: tauri::State<'_, GuiState>, enabled: bool) -> Result<bool, String> {
    let _guard = state
        .env_lock
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let path = state.cwd.join(".env");
    let value = if enabled { "true" } else { "false" };
    update_env_keys(&path, &[(AUTO_CONFIRM_KEY, value.to_string())])?;
    state.auto_confirm.store(enabled, Ordering::Relaxed);
    Ok(enabled)
}

/// 停止当前批次（协作式取消：调度器会在下一个检查点退出）。
#[tauri::command]
fn stop_run(state: tauri::State<'_, GuiState>) -> Result<(), String> {
    let scheduler = {
        let slot = state
            .scheduler
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        slot.clone()
    };
    match scheduler {
        Some(scheduler) => {
            scheduler.stop_session();
            state.invalidate_view();
            // 取消是**协作式**的：最坏要等当前那次 HTTP 请求超时（60 秒）才真的退出。
            // 这句必须让用户看到 —— 否则点完界面没动静，用户以为按钮坏了。
            state.push_alert("已请求停止：最坏要等当前这次请求结束（约 60 秒）才真正退出");
            Ok(())
        }
        // 调度器还没建 ⇒ 从来没投喂过，没有在跑的会话。
        None => Err("当前没有在跑的批次".to_string()),
    }
}

// ⚠️ `get_log` / `get_cwd` 已删（2026-10-01，DEBT TD-05-15）：
// 注册了但**前端一次都没调过** —— 幽灵预留。`get_log` 还拖着整套日志机制
// （写了没人读，就是 TD-05-21「批次失败不可见」的成因）。
// 要日志面板 ⇒ **先在前端做出来、再留命令**，顺序不能反。

// ── 启动 ──────────────────────────────────────────────────────────────

/// 「这个目录是工作区」判据。
///
/// 不只看 `.env`：刚解压、还没填密钥的仓库**目录结构已经在了**，
/// 它仍然该被认成工作区（否则会被判到兜底目录去，用户配的 `inbox/` 白放）。
fn is_workspace(dir: &Path) -> bool {
    dir.join(".env").is_file() || dir.join("inbox").is_dir() || dir.join("src-tauri").is_dir()
}

/// `.app` 包**所在的那个目录**（macOS 双击形态）。
///
/// `<…>/lovart.app/Contents/MacOS/lovart-client` → `<…>`。
/// 判据是路径里真有 `.app` 这一层，否则（命令行/其它平台）返回 `None`，
/// 不去猜「二进制所在目录的上两级是不是仓库根」。
fn bundle_sibling_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let app = exe.parent()?.parent()?.parent()?;
    if app.extension().is_some_and(|ext| ext == "app") {
        app.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

/// 用户主目录（`HOME` / Windows 的 `USERPROFILE`）。
fn user_home() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .iter()
        .find_map(|key| std::env::var_os(key))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// 从三个候选里挑工作区 —— **纯函数**（不读环境、不碰磁盘，所以能直接测）。
///
/// 顺序即优先级，理由见 [`workspace_dir`]。返回 `None` ⇒ 三个都不是 ⇒
/// 由 [`workspace_dir`] 落到用户目录兜底。
///
/// ⚠️ `explicit` **不查** [`is_workspace`]：它是用户明说的，说错也该照着去
/// （在那里建 `.env` 是他自己的选择），不该被代码悄悄改判到别处。
pub fn pick_workspace(
    explicit: Option<PathBuf>,
    cwd: Option<PathBuf>,
    bundle_sibling: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(dir) = explicit.filter(|dir| !dir.as_os_str().is_empty()) {
        return Some(dir);
    }
    cwd.filter(|dir| is_workspace(dir))
        .or_else(|| bundle_sibling.filter(|dir| is_workspace(dir)))
}

/// GUI 的**工作区**（`.env` / `inbox/` / `var/` / `output/` 的根）。
///
/// ── 为什么不能直接用当前目录 ────────────────────────────────────────
/// macOS 上从 Finder 双击 `.app`，进程的当前目录是 **`/`** —— 既不是 `.app`
/// 所在目录，也不是用户目录。而本工具的运行时文件**全部按当前目录解析**
/// ⇒ `.env` 读不到（设置界面全空）· `/var/state.jsonl` 建不出来（看板直接报错）·
/// `/output` 写不进去（出图必失败）。**窗口开得起来，但每个动作都失败。**
/// ⇒ 双击即用是本工具的唯一交付形态，所以工作区必须自己定，不能指望当前目录。
///
/// ── 判据（顺序固定，命中即停）─────────────────────────────────────
/// 1. `LOVART_HOME` —— 用户显式指定，最高优先；
/// 2. 当前目录**本身是工作区** ⇒ 终端里 `cd 仓库根 && ./lovart-client` 行为不变；
/// 3. `.app` **同级目录**是工作区 ⇒ 双击装在仓库根里的 .app（本项目的交付形态）；
/// 4. 都没有 ⇒ `~/Documents/lovart`（没有就建）—— 仍是**可写**的根，
///    用户在设置里填一次密钥即可；比"启动就报错"或"悄悄写进 `/`"都好。
pub fn workspace_dir() -> Result<PathBuf, String> {
    let explicit = std::env::var_os("LOVART_HOME").map(PathBuf::from);
    let cwd = std::env::current_dir().ok();
    if let Some(dir) = pick_workspace(explicit, cwd, bundle_sibling_dir()) {
        return Ok(dir);
    }

    let fallback = user_home()
        .ok_or_else(|| "取不到用户主目录，无法确定工作区".to_string())?
        .join("Documents")
        .join("lovart");
    std::fs::create_dir_all(&fallback)
        .map_err(|error| format!("建不了工作区 {}：{error}", fallback.display()))?;
    Ok(fallback)
}

/// 开窗口。**只有 `main` 在无子命令时才会调到这里**。
pub fn run() {
    // ⚠️ 双击启动时当前目录是 `/`，不能直接用 —— 理由与判据见 [`workspace_dir`]。
    let cwd = match workspace_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("启动失败：{error}");
            std::process::exit(1);
        }
    };
    // 单实例锁的路径先算好：`cwd` 随后被 move 进 `GuiState`。
    let lock_path = cwd.join("var/.lock");
    tauri::Builder::default()
        // 原生文件对话框（`pick_prompt_file` 用它）：提示词文件不绑定 `inbox/`。
        .plugin(tauri_plugin_dialog::init())
        .manage(GuiState::new(cwd))
        .setup(move |app| {
            // `state.jsonl` 只有一个写者（`engine::lockfile` 文件头）。
            // 放在 setup 而不是 `run()` 开头：抢不到时要能弹对话框 ——
            // GUI 没有可见终端（`windows_subsystem = "windows"`），
            // 只 `eprintln!` 就是「双击没反应」。
            match FileLock::acquire(&lock_path) {
                Ok(lock) => app.state::<GuiState>().hold_lock(lock),
                Err(message) => {
                    app.dialog()
                        .message(message)
                        .title("lovart · 已有实例在运行")
                        .blocking_show();
                    std::process::exit(1);
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            take_alerts,
            list_inbox,
            list_models,
            pick_prompt_file,
            inspect_prompt_file,
            import_inbox,
            validate_draft,
            export_draft,
            publish_queue,
            retry_one,
            retry_abnormal,
            preview_confirm,
            confirm_task,
            probe_unfinished,
            resume_one,
            resume_unfinished,
            remove_tasks,
            clear_finished_tasks,
            cancel_all_tasks,
            reveal_path,
            open_output_dir,
            get_settings,
            save_settings,
            set_auto_confirm,
            stop_run
        ])
        .run(tauri::generate_context!())
        .expect("启动窗口失败");
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── `get_state` 的「没变就别重算」缓存（v1–v4）────────────────────
    //
    // 立这些用例的理由：**命不中只是变慢，命错才是说假话**。
    // 指纹漏掉某一维（文件 / 批次 / 开关 / 清空），界面就会永远停在上一轮的
    // `following` 或计时上 —— 而那正是本仓最不能接受的一类错。

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lovart-view-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    /// 一条只有必填字段的记录（= `state.jsonl` 的一行）。
    fn record_line(id: &str, status: &str, since: Option<&str>) -> String {
        let mut value = json!({
            "id": id,
            "prompt": "测试提示词",
            "model": "m",
            "aspectRatio": "1:1",
            "imageSize": "1K",
            "status": status,
            "attempts": 0,
            "updatedAt": "2026-10-24T00:00:00.000Z",
        });
        if let Some(since) = since {
            value["stateSince"] = json!(since);
        }
        value.to_string()
    }

    fn input_of(state_file: PathBuf, dir: PathBuf) -> ViewInput {
        ViewInput {
            state_file,
            output_dir: dir,
            batch_label: None,
            batch_ids: BTreeSet::new(),
            auto_confirmed: 0,
            clearing: false,
            auto_confirm: false,
            board_rev: 0,
        }
    }

    #[test]
    fn v1_指纹没变_第二次必须命中且只回计时() {
        let dir = temp_dir("v1");
        let since = "2026-10-24T00:00:00.000Z";
        let file = dir.join("state.jsonl");
        std::fs::write(&file, record_line("t1", "running", Some(since))).expect("写状态文件");
        let input = input_of(file, dir);
        let key = input.key();
        let view = Mutex::new(ViewCache::default());

        assert!(cached_payload(&view, &key).is_none(), "首次必须未命中");
        let full = finish_payload(
            &view,
            &input,
            key.clone(),
            build_status_json(&input.state_file, &input.output_dir).expect("构造看板"),
        );
        assert_eq!(full["tasks"].as_array().map(Vec::len), Some(1));
        assert_eq!(full["tasks"][0]["following"], json!(false));

        let hit = cached_payload(&view, &key).expect("第二次必须命中");
        assert_eq!(hit["unchanged"], json!(true));
        assert_eq!(hit["elapsed"][0]["id"], json!("t1"));
        assert!(
            hit["elapsed"][0]["text"].as_str().is_some_and(|text| !text.is_empty()),
            "命中时仍然要重算「已持续」——它是唯一每秒都在变的东西"
        );
        assert!(
            hit["tasks"].is_null(),
            "命中时不许再带全量任务（那 4MB 就是白搭的）"
        );
    }

    #[test]
    fn v2_状态文件变了_指纹必须变() {
        let dir = temp_dir("v2");
        let file = dir.join("state.jsonl");
        std::fs::write(
            &file,
            record_line("t1", "running", Some("2026-10-24T00:00:00.000Z")),
        )
        .expect("写状态文件");
        let before = input_of(file.clone(), dir.clone()).key();

        // 追加一行 —— 与调度器的真实写法一致（只追加，不改已有行）
        let mut text = std::fs::read_to_string(&file).expect("读状态文件");
        text.push('\n');
        text.push_str(&record_line("t1", "done", None));
        std::fs::write(&file, text).expect("追加状态");

        let after = input_of(file, dir).key();
        assert_ne!(before, after, "文件变了而指纹没变 ⇒ 界面永远不刷新");
    }

    #[test]
    fn v3_批次变了_指纹必须变() {
        let dir = temp_dir("v3");
        let file = dir.join("state.jsonl");
        std::fs::write(
            &file,
            record_line("t1", "running", Some("2026-10-24T00:00:00.000Z")),
        )
        .expect("写状态文件");
        let mut input = input_of(file, dir);
        let before = input.key();
        // 换了一批（文件一个字没变）—— 变的只有「谁在被跟」
        input.board_rev += 1;
        input.batch_ids.insert("t1".to_string());
        assert_ne!(
            before,
            input.key(),
            "换批但文件没变 ⇒ 必须重算，否则 following 还是上一批的"
        );
    }

    #[test]
    fn v5_输出目录变了_指纹必须变() {
        let dir = temp_dir("v5");
        let file = dir.join("state.jsonl");
        std::fs::write(
            &file,
            record_line("t1", "running", Some("2026-10-24T00:00:00.000Z")),
        )
        .expect("写状态文件");
        let mut input = input_of(file, dir.clone());
        let before = input.key();
        // 设置里换了输出目录：状态文件一个字没变，但 `filesPresent` 全变了
        input.output_dir = dir.join("另一个输出目录");
        assert_ne!(
            before,
            input.key(),
            "指纹漏了输出目录 ⇒ 改完设置界面还指着旧目录说图在不在"
        );
    }

    #[test]
    fn v4_探针回来必须作废缓存() {
        let dir = temp_dir("v4");
        std::fs::create_dir_all(dir.join("var")).expect("建 var");
        let file = dir.join("var/state.jsonl");
        std::fs::write(
            &file,
            record_line("t1", "running", Some("2026-10-24T00:00:00.000Z")),
        )
        .expect("写状态文件");

        // 走真实构造：`state_file` / `output_dir` 与生产路径同一套推导
        let state = GuiState::new(dir);
        let input = view_input(&state);
        let key = input.key();
        finish_payload(
            &state.view,
            &input,
            key.clone(),
            build_status_json(&input.state_file, &input.output_dir).expect("构造看板"),
        );
        assert!(cached_payload(&state.view, &key).is_some(), "先造出命中");

        state.invalidate_view();
        assert!(
            cached_payload(&state.view, &key).is_none(),
            "探针结论不落盘，不作废缓存 ⇒ 标题行永远停在「探测中…」"
        );
    }
}
