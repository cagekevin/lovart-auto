//! 批次的**唯一持有者**：调度器、告警、看板缓存、单实例锁、投喂串行锁。
//!
//! 原名是 `gui.rs` 里的那个全局状态。搬到这里的目的不是"整理文件"，而是让
//! **提交 / 停止 / 看板**只有一个实现，GUI 与 HTTP（下一步）都只能通过它。
//!
//! 字段全私有：外部只能走访问器；直接摸字段在 `src-tauri` 里是 **E0616**。
//! 这是 `ADR-0004` / `ADR-0009` 第一级（能让写错编译不过），不是约定。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::engine::intake::TaskInput;
use crate::engine::lockfile::FileLock;
use crate::engine::scheduler::{
    AutoConfirm, Scheduler, SchedulerConfig, SchedulerSummary, SchedulerTuning, SessionEndFn,
};
use crate::engine::settings::Settings;
use crate::engine::state::{StateStore, StateStoreOptions};
use crate::lovart::connection::{
    load_dot_env, read_auto_confirm, reload_dot_env, resolve_connection,
};
use crate::service::wiring::to_client_deps;

/// 外部递送进来的一条草稿（**不是任务**：不写 `state.jsonl`、不占槽、不开跑）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Draft {
    /// `d-<序号>`，收到时生成，本次进程内永不复用。
    pub id: String,
    /// 与前端队列项同形：`{...task, draftId}` 直接可用。
    #[serde(flatten)]
    pub task: TaskInput,
    pub refs_state: RefsState,
}

/// 垫图在"人看得见"这件事上的状态（失败不许静默）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefsState {
    Ready,
    Unavailable(String),
}

/// 草稿集合（**只在内存**：关 GUI 即消失）。
#[derive(Debug, Default)]
pub struct DraftsStore {
    items: Vec<Draft>,
    seq: u32,
}

/// `<工作区>/data/`：唯一的运行数据根（`docs/plan/19`）。
///
/// ⚠️ `data/` 这个字面量只在这里出现一次 —— GUI 与 CLI 都必须通过本函数定位数据根，
/// 不许各自 `cwd.join("data")`（ADR-0002：调用方不出现生产方的结构知识）。
pub fn data_dir(cwd: &Path) -> PathBuf {
    cwd.join("data")
}

/// `data/var/`（**不持 `Workbench` 的调用方**用它，例如 `service::refs` 的整批解析）。
///
/// 与 [`Workbench::var_dir`] 同一个实现：两处各拼一次 `join("var")`，改目录时就会漏一处。
pub fn var_dir_of(cwd: &Path) -> PathBuf {
    data_dir(cwd).join("var")
}

/// `data/var/refs/`（参考图落盘目录）。
///
/// ⚠️ 目录名 `refs` 同时是**命名判据**（`engine::refs::naming_for` 按它决定纯哈希还是
/// 可读名）⇒ 这个名字只允许出现在 `engine::refs::CONTENT_DIR_NAME`，不许在这里再写一个字面量。
pub fn refs_dir_of(cwd: &Path) -> PathBuf {
    var_dir_of(cwd).join(crate::engine::refs::CONTENT_DIR_NAME)
}

/// 把旧布局**一次性移动**到 `data/` 下（幂等，`docs/plan/19` §3.2）。
///
/// - 只移动「源存在、目标不存在」的项；
/// - 两处都在 ⇒ 不动 + 一条告警（不猜哪份新，不自动合并用户数据）；
/// - 每项独立判断 ⇒ 中途失败后下次启动继续，不会重复搬、也不会丢。
pub fn migrate_layout(cwd: &Path) -> Vec<String> {
    let data = data_dir(cwd);
    let mut notes = Vec::new();
    if let Err(error) = std::fs::create_dir_all(&data) {
        return vec![format!("建不了数据目录 {}：{error}", data.display())];
    }
    for name in [".env", "prompts.jsonl", "inbox", "output", "var"] {
        let src = cwd.join(name);
        let dst = data.join(name);
        if !src.exists() {
            continue;
        }
        if dst.exists() {
            notes.push(format!("{name} 在新旧两处都存在，没有自动合并，请自己处理"));
            continue;
        }
        match std::fs::rename(&src, &dst) {
            Ok(()) => notes.push(format!("已迁移 {name} → data/{name}")),
            Err(error) => notes.push(format!("迁移 {name} 失败：{error}（下次启动会重试）")),
        }
    }
    notes
}

/// 把一条草稿的 refs 归一成"前端能显示的形式"，返回整体状态。
///
/// - 本机路径 → 绝对路径 + stat（`resolve_local_ref`）；
/// - `data:` → 落盘成 `data/var/refs/<哈希>` 路径（`materialize_image`）；
/// - `http(s):` → 原样（前端与发布链都认）。
///
/// **不拒收任何合法输入**：只有读不到 / 解不开才标 `Unavailable`。
fn normalize_draft_refs(task: &mut TaskInput, refs_dir: &Path, cwd: &Path) -> RefsState {
    let mut problem: Option<String> = None;
    for raw in task.refs.iter_mut() {
        let input = raw.trim().to_string();
        if input.is_empty() {
            // 空项交给发布链拦（`tasks_from_inputs`），这里不替它做。
            continue;
        }
        // 与 `read_ref_inputs` 同一口径：协议前缀大小写不敏感。
        let lower = input.to_lowercase();
        if lower.starts_with("data:") {
            let source = crate::engine::refs::ImageSource::Data { value: input.clone() };
            match crate::engine::refs::materialize_image(refs_dir, &source) {
                Ok(path) => *raw = path,
                Err(reason) => {
                    problem.get_or_insert(reason);
                }
            }
            continue;
        }
        if lower.starts_with("http://") || lower.starts_with("https://") {
            continue;
        }
        match crate::engine::refs::resolve_local_ref(&input, cwd) {
            Ok(path) => *raw = path.display().to_string(),
            Err(reason) => {
                problem.get_or_insert(reason);
            }
        }
    }
    match problem {
        Some(reason) => RefsState::Unavailable(reason),
        None => RefsState::Ready,
    }
}

/// GUI / HTTP 共用的批次状态。
pub struct Workbench {
    cwd: PathBuf,
    /// 全局自动确认开关（运行态真源；与每个批次的 `AutoConfirm` 共享）。
    auto_confirm: Arc<AtomicBool>,
    /// 花积分开关（运行态真源；与调度器的 `Shared.spend_mode` 共享）。
    ///
    /// ⚠️ **不落盘**：每次启动恒 `false`。忘了关就是整批按 fast 计费，
    ///    所以这个开关不能跨会话记着（与自动确认不同，它的副作用更大）。
    spend_mode: Arc<AtomicBool>,
    /// `.env` 读-改-写互斥：保存设置与切开关会同时改同一个文件。
    env_lock: Mutex<()>,
    /// **批次级**告警 —— 用户**必须看到**、且看板**表达不了**的那些事。
    ///
    /// ── 边界 ──────────────────────────────────────────────────────────
    /// **任务级 = `state.jsonl`（看板）**；**批次级 = 这里（告警）**。
    /// 两者不重叠，谁也不替谁兜底。
    ///
    /// 与旧 `log` 的区别：**告警不裁剪**。日志可以丢，告警丢了就是骗人。
    alerts: Mutex<Vec<String>>,
    /// 看板载荷的「没变就别重算」缓存（`2B` 会搬去 GUI 层）。
    /// 单实例锁（`engine::lockfile`）。由 `setup` 注入，**必须活到窗口关闭** ——
    /// 句柄一关，另一个进程就能进来写 `state.jsonl`。
    single_instance: Mutex<Option<FileLock>>,
    /// 常驻调度器（**惰性启动**：第一次投喂时才建，没配凭据的用户启动不报错）。
    scheduler: Mutex<Option<Scheduler>>,
    /// 投喂串行锁：`refs-cache.json` 是「读-合并-写」，两次并发投喂会互相覆盖
    /// （`docs/plan/09` §1.5.4）。代价可接受 —— 投喂是人工动作。
    intake_lock: Mutex<()>,
    /// 外部递送进来的草稿（**只在内存**：关 GUI 即消失，不写 `state.jsonl`）。
    drafts: Mutex<DraftsStore>,
}

impl Workbench {
    /// ⚠️ 返回 `Arc`：`ensure_scheduler` 要给回调 `Weak<Self>`（否则 Arc 环）。
    pub fn new(cwd: PathBuf) -> Arc<Self> {
        // ⚠️ 布局迁移**必须在读 .env 之前**（plan 19 §3.2）：旧布局的 `.env` 还在根目录，
        // 先搬再读，才不会读到"半个新布局"。
        let migration = migrate_layout(&cwd);
        // 自动确认开关只认 `.env`（不读进程环境变量）。非法值 / 读失败 ⇒ 关 + 一条告警。
        let env_path = data_dir(&cwd).join(".env");
        let (auto_confirm, auto_confirm_warning) = read_auto_confirm(&env_path);
        let state = Arc::new(Self {
            cwd,
            alerts: Mutex::new(Vec::new()),
            auto_confirm: Arc::new(AtomicBool::new(auto_confirm)),
            // 恒关：不读 `.env`，也没有默认值可配（见字段注释）。
            spend_mode: Arc::new(AtomicBool::new(false)),
            env_lock: Mutex::new(()),
            single_instance: Mutex::new(None),
            scheduler: Mutex::new(None),
            intake_lock: Mutex::new(()),
            drafts: Mutex::new(DraftsStore::default()),
        });

        // ⚠️ 启动就把 `.env` 载进来：设置里的「并发数 / 输出文件夹」也住在那里，
        //    不先载入，`get_settings` 读到的永远是默认值（= 设置界面是假的）。
        //
        // 失败**必须上报**：载入失败 ⇒ 后面所有读环境变量的地方都拿到默认值 ⇒
        // 设置界面显示的是**假配置**。原先是 `let _ =`，等于把这件事藏起来（DEBT TD-05-24）。
        if let Err(error) = load_dot_env(&state.env_path()) {
            state.push_alert(&format!("配置载入失败，界面上的设置可能不是真实配置：{error}"));
        }
        for note in migration {
            state.push_alert(&note);
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
    pub fn hold_lock(&self, lock: FileLock) {
        let mut slot = self
            .single_instance
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *slot = Some(lock);
    }

    /// 取（必要时创建）常驻调度器。
    ///
    /// ⚠️ receiver 必须是 `&Arc<Self>`：回调要 `Arc::downgrade(self)`。
    /// 回调一律持 `Weak`，否则 `Workbench → Scheduler → 回调 → Workbench` 是 Arc 环。
    ///
    /// ⚠️ 返回**克隆**而不是 guard：调用方随后会锁 `Mutex<Ctx>`，若拿着槽锁去锁它，
    /// 任何反向路径都会死锁（`docs/plan/09` §1.5.3）。
    pub fn ensure_scheduler(self: &Arc<Self>) -> Result<Scheduler, String> {
        let mut slot = self
            .scheduler
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(scheduler) = slot.as_ref() {
            return Ok(scheduler.clone());
        }

        // 每次创建前重新载入 `.env`（覆盖语义）：手改后点提交仍要用新值。
        reload_dot_env(&self.env_path())?;
        let (settings, warnings) = Settings::from_file(&self.env_path())
            .map_err(|error| format!("读不了 .env：{error}"))?;
        for warning in &warnings {
            self.push_alert(&format!("⚠️ {warning}"));
        }
        let connection = resolve_connection()?;
        let deps = to_client_deps(&connection)?;
        crate::engine::runlog::init(self.run_log_file());

        let weak = Arc::downgrade(self);

        // 落盘失败 ⇒ 跑的中途就上报一次（`on_write_error` 只调一次，不刷屏）。
        let alert_weak = weak.clone();
        let store = StateStore::open_with(
            &self.state_file(),
            StateStoreOptions {
                on_write_error: Some(Box::new(move |message: &str| {
                    if let Some(workbench) = alert_weak.upgrade() {
                        workbench.push_alert(&format!(
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
        let end_weak = weak;
        let on_session_end: SessionEndFn = Arc::new(move |summary: &SchedulerSummary| {
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
            let Some(workbench) = end_weak.upgrade() else {
                return;
            };
            if summary.persist_failed {
                workbench.push_alert(
                    "收尾时状态文件写不进去：本次进度可能续不上，重启后需要重新提交",
                );
            }
            let confirmed = auto_for_cb.count();
            if confirmed > 0 {
                workbench.push_alert(&format!(
                    "本会话自动确认 {confirmed} 条（已扣积分，明细以上游账单为准）"
                ));
            }
        });

        let scheduler = Scheduler::start(SchedulerConfig {
            deps,
            state: store,
            output_dir: self.output_dir(),
            report_file: Some(self.report_file()),
            tuning: SchedulerTuning::from_settings(&settings),
            retry_failed: false,
            auto_confirm,
            spend_mode: Arc::clone(&self.spend_mode),
            log: Arc::new(|message: &str| crate::engine::runlog::line(message)),
            ports: None,
            on_session_end: Some(on_session_end),
        })
        .map_err(|error| format!("调度器启动失败：{}", error.message()))?;

        *slot = Some(scheduler.clone());
        Ok(scheduler)
    }

    /// 取调度器克隆（`None` = 还没创建）。不把 guard 交出去，避免与 ctx 锁嵌套。
    pub fn scheduler_snapshot(&self) -> Option<Scheduler> {
        self.scheduler
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// `data/`（`docs/plan/19`：唯一的运行数据根）。
    pub fn data_dir(&self) -> PathBuf {
        data_dir(&self.cwd)
    }

    pub fn state_file(&self) -> PathBuf {
        self.var_dir().join("state.jsonl")
    }

    pub fn report_file(&self) -> PathBuf {
        self.var_dir().join("report.csv")
    }

    pub fn prompts_file(&self) -> PathBuf {
        self.data_dir().join("prompts.jsonl")
    }

    pub fn presets_dir(&self) -> PathBuf {
        self.data_dir().join("presets")
    }

    /// `data/library/`（素材库；`docs/plan/20`）。目录**不预建**：第一次「发送到素材库」
    /// 时由落盘那一步建，空目录不是它存在的证据。
    pub fn library_dir(&self) -> PathBuf {
        self.data_dir().join("library")
    }

    pub fn output_dir(&self) -> PathBuf {
        // 1Hz 的 `get_state` 会调它：读环境（微秒级），不读盘。
        // 手改 `.env` 的目录要等点「开始」才统一（N19）—— 这里刻意不跟文件，
        // 否则每秒做一次磁盘 I/O。
        // ⚠️ `OUTPUT_DIR` 的相对路径按 `data/` 解析（plan 19 §3.3）；绝对路径原样用。
        self.data_dir().join(Settings::from_env().0.output_dir)
    }

    /// 记一条**批次级**告警（用户必须看到的事）。
    ///
    /// 只做「追加」——**不裁剪**。日志可以丢，告警丢了就是骗人。
    /// 消费方是 [`Workbench::take_alerts`]：前端每轮拉走，拉走即清（不重复弹）。
    pub fn push_alert(&self, message: &str) {
        // 告警也落日志：弹窗**关掉就没了**，而挂机出问题时人不在电脑前。
        crate::engine::runlog::line(&format!("⚠️ 告警：{message}"));
        let mut alerts = self.alerts.lock().unwrap_or_else(|error| error.into_inner());
        alerts.push(format!(
            "[{}] {message}",
            chrono::Local::now().format("%H:%M:%S")
        ));
    }

    /// 取走**尚未读过的**批次级告警（取走即清，不重复弹）。
    pub fn take_alerts(&self) -> Vec<String> {
        let mut alerts = self.alerts.lock().unwrap_or_else(|error| error.into_inner());
        std::mem::take(&mut *alerts)
    }

    /// ⚠️ `pub(crate)`：只有 service 内部（参考图解析）需要"工作区根"。
    /// 外部调用方一律用下面的具名目录 —— 拼路径的写法在 `src-tauri` 里是 **E0624**。
    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// `data/.env` 的绝对路径（读-改-写都走这里，调用方不拼）。
    pub fn env_path(&self) -> PathBuf {
        self.data_dir().join(".env")
    }

    /// `data/inbox/`（导入与列表的根）。
    pub fn inbox_dir(&self) -> PathBuf {
        self.data_dir().join("inbox")
    }

    /// `data/var/`（机器状态根）。
    pub fn var_dir(&self) -> PathBuf {
        var_dir_of(&self.cwd)
    }

    /// `data/var/refs/`（参考图落盘目录）。
    pub fn refs_dir(&self) -> PathBuf {
        refs_dir_of(&self.cwd)
    }

    /// `data/var/refs-cache.json`。
    pub fn refs_cache_file(&self) -> PathBuf {
        self.var_dir().join("refs-cache.json")
    }

    /// `data/var/.lock`（单实例锁）。
    pub fn lock_path(&self) -> PathBuf {
        self.var_dir().join(".lock")
    }

    /// `data/var/run.log`（运行日志；GUI 与 CLI 都只走这里）。
    pub fn run_log_file(&self) -> PathBuf {
        self.var_dir().join("run.log")
    }

    pub fn auto_confirm_on(&self) -> bool {
        self.auto_confirm.load(Ordering::Relaxed)
    }

    pub fn spend_mode_on(&self) -> bool {
        self.spend_mode.load(Ordering::Relaxed)
    }

    pub fn set_auto_confirm(&self, on: bool) {
        self.auto_confirm.store(on, Ordering::Relaxed);
    }

    pub fn set_spend_mode(&self, on: bool) {
        self.spend_mode.store(on, Ordering::Relaxed);
    }

    /// 投喂串行锁（`refs-cache.json` 读-合并-写）。
    pub fn lock_intake(&self) -> std::sync::MutexGuard<'_, ()> {
        self.intake_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// `.env` 读-改-写互斥（保存设置 / 切开关）。
    pub fn lock_env(&self) -> std::sync::MutexGuard<'_, ()> {
        self.env_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// 收一批草稿（**唯一写入口**）。HTTP 的 `/drafts` 与将来的 CLI 都只能调它。
    ///
    /// 归一（stat / `data:` 落盘）在**锁外**做：磁盘 I/O 不占草稿锁，
    /// 否则 HTTP 线程会堵住 GUI 的 `get_drafts`。
    pub fn draft(&self, tasks: Vec<TaskInput>) {
        let normalized: Vec<(TaskInput, RefsState)> = tasks
            .into_iter()
            .map(|mut task| {
                let state = normalize_draft_refs(&mut task, &self.refs_dir(), &self.cwd);
                (task, state)
            })
            .collect();
        let mut store = self.drafts.lock().unwrap_or_else(|error| error.into_inner());
        for (task, refs_state) in normalized {
            store.seq += 1;
            let id = format!("d-{}", store.seq);
            store.items.push(Draft { id, task, refs_state });
        }
    }

    /// 全部草稿（按收到顺序；前端按 `id` 去重）。
    pub fn drafts(&self) -> Vec<Draft> {
        self.drafts
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .items
            .clone()
    }

    /// 按 id 删草稿（删单条传 `[id]`；**不是全清**）。返回删掉的条数。
    pub fn clear_drafts(&self, ids: &[String]) -> usize {
        let mut store = self.drafts.lock().unwrap_or_else(|error| error.into_inner());
        let before = store.items.len();
        store.items.retain(|draft| !ids.contains(&draft.id));
        before - store.items.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lovart-drafts-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn task(prompt: &str, refs: &[&str]) -> TaskInput {
        TaskInput {
            prompt: prompt.to_string(),
            model: String::new(),
            ratio: String::new(),
            size: String::new(),
            refs: refs.iter().map(|value| value.to_string()).collect(),
        }
    }

    #[test]
    fn 迁移幂等且同存不合并() {
        let dir = temp_dir("migrate");
        std::fs::write(dir.join(".env"), b"K=V\n").expect("写旧 .env");
        std::fs::create_dir_all(dir.join("inbox")).expect("建旧 inbox");
        std::fs::write(dir.join("inbox/a.jsonl"), b"a").expect("写旧提示词");

        let first = migrate_layout(&dir);
        assert!(first.iter().any(|n| n.contains(".env")));
        assert!(!dir.join(".env").exists(), "旧 .env 应被移走");
        assert!(data_dir(&dir).join(".env").exists(), "新 .env 应在 data/");
        assert!(data_dir(&dir).join("inbox/a.jsonl").exists());

        let second = migrate_layout(&dir);
        assert!(second.is_empty(), "第二次应什么都不搬：{second:?}");

        // 两处都在 ⇒ 不动 + 告警（不猜哪份新）
        std::fs::write(dir.join("prompts.jsonl"), b"old").expect("写旧 prompts");
        std::fs::write(data_dir(&dir).join("prompts.jsonl"), b"new").expect("写新 prompts");
        let third = migrate_layout(&dir);
        assert!(third.iter().any(|n| n.contains("prompts.jsonl") && n.contains("都存在")));
        assert_eq!(std::fs::read_to_string(dir.join("prompts.jsonl")).unwrap(), "old");
        assert_eq!(
            std::fs::read_to_string(data_dir(&dir).join("prompts.jsonl")).unwrap(),
            "new"
        );
    }

    #[test]
    fn 两条草稿_id_唯一且按序() {
        let dir = temp_dir("ids");
        let wb = Workbench::new(dir);
        wb.draft(vec![task("一", &[]), task("二", &[])]);
        let drafts = wb.drafts();
        assert_eq!(drafts.len(), 2);
        assert_ne!(drafts[0].id, drafts[1].id);
        assert_eq!(drafts[0].refs_state, RefsState::Ready);
    }

    #[test]
    fn 相对路径变绝对路径() {
        let dir = temp_dir("relative");
        std::fs::write(dir.join("a.png"), b"x").expect("写垫图");
        let wb = Workbench::new(dir.clone());
        wb.draft(vec![task("p", &["a.png"])]);
        let drafts = wb.drafts();
        let path = Path::new(&drafts[0].task.refs[0]);
        assert!(path.is_absolute(), "应归一成绝对路径：{}", path.display());
        assert!(path.ends_with("a.png"));
        assert_eq!(drafts[0].refs_state, RefsState::Ready);
    }

    #[test]
    fn 路径不存在标_unavailable() {
        let dir = temp_dir("missing");
        let wb = Workbench::new(dir);
        wb.draft(vec![task("p", &["没有这张.png"])]);
        let drafts = wb.drafts();
        assert!(matches!(drafts[0].refs_state, RefsState::Unavailable(_)));
    }

    #[test]
    fn data_落盘成路径() {
        let dir = temp_dir("data");
        let wb = Workbench::new(dir.clone());
        // 1x1 PNG（只验落盘与路径归一，不验像素）。
        let png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        wb.draft(vec![task("p", &[png])]);
        let drafts = wb.drafts();
        let path = &drafts[0].task.refs[0];
        assert!(!path.starts_with("data:"), "应落盘成路径：{path}");
        assert!(Path::new(path).exists(), "落盘文件应存在：{path}");
        assert_eq!(drafts[0].refs_state, RefsState::Ready);
    }

    #[test]
    fn clear_drafts_只清给定_id() {
        let dir = temp_dir("clear");
        let wb = Workbench::new(dir);
        wb.draft(vec![task("一", &[]), task("二", &[]), task("三", &[])]);
        let first = wb.drafts()[0].id.clone();
        assert_eq!(wb.clear_drafts(&[first.clone()]), 1);
        let left = wb.drafts();
        assert_eq!(left.len(), 2);
        assert!(left.iter().all(|draft| draft.id != first));
        // 删不存在的 id ⇒ 不动
        assert_eq!(wb.clear_drafts(&["d-999".to_string()]), 0);
    }
}
