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

use crate::engine::scheduler::{run_scheduler, AutoConfirm, Batch, SchedulerOptions};
use crate::engine::state::{
    clear, is_unfinished, row as status_row, Action, RecordPatch, StateStore, StateStoreOptions,
    TaskRecord, TaskStatus,
};
use crate::engine::text::{read_text, read_text_required};
use crate::lovart::connection::{
    load_dot_env, read_auto_confirm, reload_dot_env, resolve_connection, to_client_deps,
    update_env_keys, AUTO_CONFIRM_KEY,
};
use crate::lovart::prompt::{parse_prompts, ParseOptions, PromptDefaults};
use crate::run::build_status_json;

/// 运行中的一个批次。
struct RunningBatch {
    /// 调度器的取消标志。
    cancelled: Arc<AtomicBool>,
    /// 本批的自动确认机制（与调度器共享同一个 Arc；`get_state` 读计数）。
    auto_confirm: Arc<AutoConfirm>,
    label: String,
    /// **本批次交给调度器的任务 id**（= 这批真会去提交/复查的那些）。
    ///
    /// 它是「进度条转不转」的**唯一判据**（`get_state` 把它逐条下发成 `following`）。
    ///
    /// ── 为什么必须有它 ────────────────────────────────────────────────
    /// `state.jsonl` 里会有**没人跟**的未完成记录：调度器的队列**只由当前提示词文件
    /// 派生**（见 `engine::scheduler` 的建队逻辑），上次被中断留下的 `running`
    /// 只要不在这份文件里，就永远没人再查它 —— 既不会完成，也不会变红。
    /// 原先界面按 `status` 判断，于是给这些记录也画一条 **CSS 无限循环**的进度条：
    /// 「一直在转」= **宣告一件没发生的事**（本仓「成功必须为真」那条红线的反面）。
    /// ⇒ 判据从「状态是什么」改成「**本批次有没有接管它**」。
    ids: BTreeSet<String>,
}

/// GUI 全局状态。
pub struct GuiState {
    cwd: PathBuf,
    running: Mutex<Option<RunningBatch>>,
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
}

/// 输出文件夹名（设置里可改，住在 `.env`）。
fn output_dir_name() -> String {
    std::env::var("LOVART_OUTPUT_DIR")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "output".to_string())
}

/// 并发数（设置里可改）。
///
/// 硬上限 **8** —— 服务端最多同时 8 个任务，配大了只会换来 402。
fn concurrency() -> usize {
    std::env::var("LOVART_CONCURRENCY")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| (1..=8).contains(value))
        .unwrap_or(7)
}

impl GuiState {
    pub fn new(cwd: PathBuf) -> Self {
        // 自动确认开关只认 `.env`（不读进程环境变量）。非法值 / 读失败 ⇒ 关 + 一条告警。
        let (auto_confirm, auto_confirm_warning) = read_auto_confirm(&cwd.join(".env"));
        let state = Self {
            cwd,
            running: Mutex::new(None),
            alerts: Mutex::new(Vec::new()),
            auto_confirm: Arc::new(AtomicBool::new(auto_confirm)),
            env_lock: Mutex::new(()),
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
        state
    }

    fn state_file(&self) -> PathBuf {
        self.cwd.join("var/state.jsonl")
    }

    fn output_dir(&self) -> PathBuf {
        self.cwd.join(output_dir_name())
    }

    /// 记一条**批次级**告警（用户必须看到的事）。
    ///
    /// 只做「追加」——**不裁剪**。日志可以丢，告警丢了就是骗人。
    /// 消费方是 [`take_alerts`]：前端每轮拉走，拉走即清（不重复弹）。
    fn push_alert(&self, message: &str) {
        let mut alerts = self.alerts.lock().unwrap_or_else(|error| error.into_inner());
        alerts.push(format!(
            "[{}] {message}",
            chrono::Local::now().format("%H:%M:%S")
        ));
    }
}

// ── 命令 ──────────────────────────────────────────────────────────────

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
#[tauri::command]
fn get_state(state: tauri::State<'_, GuiState>) -> Result<Value, String> {
    let mut payload = build_status_json(&state.state_file(), &state.output_dir())?;
    // 计数只在本批存在时有意义；没有批次就是 0（不伪造"上次跑到多少"）。
    let (batch, auto_confirmed) = {
        let running = state
            .running
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match running.as_ref() {
            Some(batch) => (
                Some((batch.label.clone(), batch.ids.clone())),
                batch.auto_confirm.count(),
            ),
            None => (None, 0),
        }
    };
    let auto_confirm = state.auto_confirm.load(Ordering::Relaxed);
    if let Some(object) = payload.as_object_mut() {
        object.insert("running".to_string(), Value::Bool(batch.is_some()));
        object.insert(
            "runningBatch".to_string(),
            batch
                .as_ref()
                .map(|(label, _)| Value::String(label.clone()))
                .unwrap_or(Value::Null),
        );
        object.insert("autoConfirm".to_string(), Value::Bool(auto_confirm));
        object.insert("autoConfirmed".to_string(), json!(auto_confirmed));
        if let Some(tasks) = object.get_mut("tasks").and_then(Value::as_array_mut) {
            for task in tasks {
                let following = task
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| batch.as_ref().is_some_and(|(_, ids)| ids.contains(id)));
                if let Some(task) = task.as_object_mut() {
                    task.insert("following".to_string(), Value::Bool(following));
                }
            }
        }
    }
    Ok(payload)
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
    let defaults = PromptDefaults::default();
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
            let name = path
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_default();
            // 读失败**照抛**，绝不 `unwrap_or_default()` 伪装成空文件 ——
            // 空文本会被解析成「0 条 0 错」，界面于是显示「这个文件是空的」。
            let text = read_text(&path)
                .map_err(|error| format!("读不了 {}：{error}", path.display()))?
                .ok_or_else(|| {
                    format!("文件在列出之后被移走了：{}（刷新重试）", path.display())
                })?;
            let parsed = parse_prompts(&text, &defaults, &ParseOptions::default());
            let preview: Vec<String> = parsed
                .tasks
                .iter()
                .take(3)
                .map(|task| task.prompt.clone())
                .collect();
            files.push(json!({
                "name": name,
                "path": path.display().to_string(),
                "count": parsed.tasks.len(),
                "preview": preview,
                "errors": parsed.errors,
                "warnings": parsed.warnings,
                "ok": parsed.errors.is_empty() && !parsed.tasks.is_empty(),
            }));
        }
    }

    Ok(json!({ "dir": dir.display().to_string(), "files": files }))
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
    {
        let running = state.running.lock().unwrap_or_else(|error| error.into_inner());
        if running.is_some() {
            return Err("已有批次在跑，先停止它".to_string());
        }
    }

    // ── 两种来源：声明 / 派工（理由见 [`RunSource`]）─────────────────
    let (mut batch, label) = match source {
        RunSource::File(file) => {
            let prompts_file = PathBuf::from(&file);
            if !prompts_file.is_file() {
                return Err(format!("找不到提示词文件：{file}"));
            }
            // 校验先做：有错就别开跑（与 CLI 的 `--check` 同一条路径）。
            let text = std::fs::read_to_string(&prompts_file).map_err(|error| error.to_string())?;
            let parsed = parse_prompts(&text, &PromptDefaults::default(), &ParseOptions::default());
            if !parsed.errors.is_empty() {
                return Err(format!(
                    "提示词文件有 {} 处问题，已拒绝开跑：\n{}",
                    parsed.errors.len(),
                    parsed.errors.join("\n")
                ));
            }
            if parsed.tasks.is_empty() {
                return Err("提示词文件里没有有效内容".to_string());
            }
            let label = prompts_file
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_else(|| file.clone());
            (Batch::Declared(parsed.tasks), label)
        }
        // 派工：**不解析参考图** —— 记录里的垫图已经是 CDN URL，重传一遍纯属浪费。
        RunSource::Records(ids) => {
            let label = format!("派工 {} 条", ids.len());
            (Batch::Records(ids), label)
        }
    };

    load_dot_env(&state.cwd.join(".env"))?;
    let connection = resolve_connection()?;
    let deps = to_client_deps(&connection)?;
    let cancelled = Arc::clone(&deps.cancelled);
    // 每批一份机制：开关与 `GuiState` 共享（热更新），计数与许可按批隔离。
    let auto_confirm = AutoConfirm::new(Arc::clone(&state.auto_confirm));

    let cwd = state.cwd.clone();

    // 本批次**接管**的任务 id（逐条下发给前端的 `following` 取它）。
    // 用**全量**而不是"调度器真正入队的那部分"：已终态的那几条会被调度器跳过，
    // 而它们本来就在 `finished` 组里、不画进度条 —— 多带几个 id 不会让任何一条
    // 进度条多转一下，却省掉一次与调度器建队逻辑的重复实现。
    let ids: BTreeSet<String> = match &batch {
        Batch::Declared(tasks) => tasks.iter().map(|task| task.id.clone()).collect(),
        Batch::Records(ids) => ids.iter().cloned().collect(),
    };

    {
        let mut running = state.running.lock().unwrap_or_else(|error| error.into_inner());
        *running = Some(RunningBatch {
            cancelled: Arc::clone(&cancelled),
            auto_confirm: Arc::clone(&auto_confirm),
            label: label.clone(),
            ids,
        });
    }

    // 参考图解析 + 调度都在后台线程：不能阻塞 UI 线程。
    let app_for_thread = app.clone();
    std::thread::spawn(move || {
        let state_file = cwd.join("var/state.jsonl");
        let output_dir = cwd.join(output_dir_name());
        let report_file = cwd.join("var/report.csv");

        // 参考图：与 CLI **同一条路径**（本机路径读盘 → CDN → 写回 task.refs）。
        // 命令行没有 `--ref`，所以整批默认参考图为空，只用行内 `refs`。
        let refs_cache = cwd.join("var/refs-cache.json");
        // ⚠️ **只有"声明"要解析参考图**（派工的垫图已经是 CDN URL，重传一遍纯属浪费）
        //    —— `if let` 这一句本身就是那条分界。
        if let Batch::Declared(tasks) = &mut batch {
            if let Err(message) =
                crate::run::resolve_refs_core(&deps, tasks, &cwd, &[], Some(refs_cache.as_path()))
            {
                // 批次**根本没跑起来** ⇒ 批次级失败 ⇒ 必须让用户看到（原先只写进没人读的日志）。
                if let Some(state) = app_for_thread.try_state::<GuiState>() {
                    state.push_alert(&format!("参考图处理失败，这一批没有开跑：{message}"));
                    *state.running.lock().unwrap_or_else(|error| error.into_inner()) = None;
                }
                return;
            }
        }

        // ⚠️ 调度器的过程输出**在这里刻意丢弃** —— 不是「忘了接」。
        //
        // 边界划分：**任务级**结果（含每条失败的原因）已经落在 `state.jsonl`，
        // 看板就是它的显示器；**批次级**结果走 `push_alert`。
        // 再往界面灌一份过程日志，等于给同一个事实造第二份真相
        // （原先那条 `get_log` 通道就是这么变成幽灵的 —— DEBT TD-05-15）。
        //
        // 若哪天要接上它，**先想清楚它与看板的分工**，别让两处都成了显示器。
        let log: crate::engine::scheduler::LogFn = Arc::new(|_message: &str| {});

        // 落盘失败 ⇒ **跑的中途**就上报一次（`StateStore` 的 `on_write_error` 只调一次，
        // 不刷屏）。这是 `PersistOutcome` 那套机制的**唯一接线口** ——
        // 原先它一个消费者都没有（DEBT TD-06-04）。
        let alert_handle = app_for_thread.clone();
        let state_store = match StateStore::open_with(
            &state_file,
            StateStoreOptions {
                on_write_error: Some(Box::new(move |message: &str| {
                    if let Some(state) = alert_handle.try_state::<GuiState>() {
                        state.push_alert(&format!(
                            "状态写不进磁盘（断点续跑会失效，重启后进度可能全丢）：{message}"
                        ));
                    }
                })),
            },
        ) {
            Ok(store) => store,
            Err(error) => {
                if let Some(state) = app_for_thread.try_state::<GuiState>() {
                    state.push_alert(&format!("打不开状态文件，这一批没有开跑：{error}"));
                    *state.running.lock().unwrap_or_else(|error| error.into_inner()) = None;
                }
                return;
            }
        };

        // 报告（CSV）用：id → 任务。**两种来源各用各的权威** ——
        // 声明批用文件里的（文件是权威）；派工批用**记录的无损投影**（记录是权威）。
        // 要在 `batch` 被移进 `run_scheduler` **之前**算好。
        let tasks_by_id: std::collections::BTreeMap<_, _> = match &batch {
            Batch::Declared(tasks) => tasks
                .iter()
                .map(|task| (task.id.clone(), task.clone()))
                .collect(),
            Batch::Records(_) => {
                let mut map = std::collections::BTreeMap::new();
                match StateStore::open_read_only(&state_file) {
                    Ok(store) => {
                        for record in store.all() {
                            map.insert(record.id.clone(), record.to_prompt_task());
                        }
                    }
                    // 报告里的 `line` / `mode` 两列会因此为空 —— **说出来**，不静默。
                    Err(error) => {
                        if let Some(state) = app_for_thread.try_state::<GuiState>() {
                            state.push_alert(&format!(
                                "读不到状态文件，这次报告里的 line/mode 会缺：{error}"
                            ));
                        }
                    }
                }
                map
            }
        };

        let auto_for_count = Arc::clone(&auto_confirm);
        let result = run_scheduler(SchedulerOptions {
            deps,
            batch,
            state: state_store,
            // 留一份给收尾的 `write_report`（它要把记录里的相对路径拼成绝对路径）
            output_dir: output_dir.clone(),
            concurrency: concurrency(),
            task_timeout: None,
            recheck_timeout: None,
            max_timeouts: None,
            download_retries: None,
            max_attempts: None,
            max_busy_retries: None,
            busy_backoff_base: None,
            auto_confirm,
            log: Arc::clone(&log),
            ports: None,
        });

        // 花了钱就要报账：**Ok / Err 两个分支都读**，且必须在清 `running` 之前。
        let auto_count = auto_for_count.count();
        if auto_count > 0 {
            if let Some(state) = app_for_thread.try_state::<GuiState>() {
                state.push_alert(&format!(
                    "本批自动确认 {auto_count} 条（已扣积分，明细以上游账单为准）"
                ));
            }
        }

        match result {
            Ok(summary) => {
                // 报告与 CLI 同一份格式（`tasks_by_id` 在上面按来源备好了）
                if let Err(error) =
                    crate::run::write_report(&report_file, &summary.records, &tasks_by_id, &output_dir)
                {
                    if let Some(state) = app_for_thread.try_state::<GuiState>() {
                        state.push_alert(&format!(
                            "写报告失败（图已经出了，只是这份 CSV 没生成）：{error}"
                        ));
                    }
                }
                // 收尾落盘失败 ⇒ 本次进度可能续不上。**必须说**：
                // 不说的话用户以为跑完了就没事，重启才发现进度全丢（DEBT TD-06-04）。
                if summary.persist_failed {
                    if let Some(state) = app_for_thread.try_state::<GuiState>() {
                        state.push_alert(
                            "收尾时状态文件写不进去：本次进度可能续不上，重启后需要重新提交",
                        );
                    }
                }
            }
            Err(error) => {
                // 批次级失败 —— 原先是 `log(...)`，而那条通道没人读 ⇒ 界面显示「发布成功」。
                if let Some(state) = app_for_thread.try_state::<GuiState>() {
                    state.push_alert(&format!("批次失败：{}", error.message()));
                }
            }
        }

        if let Some(state) = app_for_thread.try_state::<GuiState>() {
            *state.running.lock().unwrap_or_else(|error| error.into_inner()) = None;
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
    let parsed = parse_prompts(&text, &PromptDefaults::default(), &ParseOptions::default());
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

/// 从 `inbox/` 导入一个文件：**读 + 校验 + 归位**，返回任务给前端。
///
/// ⚠️ **归位（移进 `inbox/imported/`）是这个动作不可分的一半** ——
/// 只取不归位 ⇒ 同一个文件可以**反复导入** ⇒ 重复出图、重复扣积分，
/// 而且全程不报错（DEBT **TD-05-12**）。
/// 「已处理」的记录**就是文件系统本身**（文件在 `imported/` 里）——
/// 不另立一份「已导入清单」：那是第二份真相，必然与磁盘漂移。
///
/// 顺序是**先读+校验、再归位**：
/// · 校验不过 ⇒ 文件留在 `inbox/`（用户改完还能再来），也不会把错文件搬进 `imported/`；
/// · 归位失败 ⇒ `Err`（不静默）—— 否则用户以为导入成功，下次又导一遍，正好退回原 bug。
///
/// 追加 / 替换是**前端编辑区**的事（待发队列是前端的真相）⇒ 这里**不带 mode 参数**。
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
    move_to_imported(cwd, file)?;
    Ok(value)
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

/// 把前端队列落成一个提示词文件，返回路径。
///
/// 只写**非默认值**：默认模型/比例/尺寸不落进文件，这样用户拿去 CLI 跑也一样。
fn write_queue_file(cwd: &std::path::Path, tasks: &[Value]) -> Result<PathBuf, String> {
    if tasks.is_empty() {
        return Err("待发是空的".to_string());
    }
    let defaults = PromptDefaults::default();
    let mut text = String::new();
    for task in tasks {
        let object = task.as_object().ok_or("任务格式不对")?;
        let prompt = object
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if prompt.is_empty() {
            return Err("有一条任务没有提示词".to_string());
        }
        let mut line = serde_json::Map::new();
        line.insert("prompt".to_string(), Value::String(prompt.to_string()));
        let model = object.get("model").and_then(Value::as_str).unwrap_or("");
        if !model.is_empty() && model != defaults.model {
            line.insert("model".to_string(), Value::String(model.to_string()));
        }
        let ratio = object.get("ratio").and_then(Value::as_str).unwrap_or("");
        if !ratio.is_empty() && ratio != defaults.aspect_ratio {
            line.insert("ratio".to_string(), Value::String(ratio.to_string()));
        }
        let size = object.get("size").and_then(Value::as_str).unwrap_or("");
        if !size.is_empty() && size != defaults.image_size {
            line.insert("size".to_string(), Value::String(size.to_string()));
        }
        let refs: Vec<Value> = object
            .get("refs")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .filter(|item| !item.trim().is_empty())
                    .map(|item| Value::String(item.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        if !refs.is_empty() {
            line.insert("refs".to_string(), Value::Array(refs));
        }
        text.push_str(&serde_json::to_string(&Value::Object(line)).map_err(|error| error.to_string())?);
        text.push('\n');
    }

    let dir = cwd.join("var");
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let path = dir.join("queue.jsonl");
    std::fs::write(&path, text).map_err(|error| error.to_string())?;
    Ok(path)
}

/// 起一批要跑的活 —— **两种来源**（语义分界见 `engine::scheduler::Batch`）。
///
/// ⚠️ **派工不走文件**：原先「重跑 / 接着查 / 确认」都是先**伪造**一份单行提示词文件
///    再交给调度器。伪造用的 `record_to_task` 只挑 4 个字段 ⇒ 垫图在那份"声明"里为空
///    ⇒ 调度器按「以文件为准」把记录里的垫图覆盖掉（2026-10-01 实测：确认后垫图消失）。
///    这里把两种来源分开：**声明以文件为准，派工以记录为准**。
enum RunSource {
    /// 用户在待发里发布的那份提示词（**声明**）。
    File(String),
    /// 对已有记录的派工：只处理这些 id（**记录才是真相**）。
    Records(Vec<String>),
}

/// 发布队列 —— 落文件后走**与 CLI 同一条开跑路径**。
#[tauri::command]
fn publish_queue(
    app: tauri::AppHandle,
    state: tauri::State<'_, GuiState>,
    tasks: Vec<Value>,
) -> Result<(), String> {
    let path = write_queue_file(&state.cwd, &tasks)?;
    start_run(app, state, RunSource::File(path.display().to_string()))
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
    if state
        .running
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .is_some()
    {
        return Err("正在跑批，先停止再重跑（重跑要改状态文件，和跑批写的是同一个文件）".to_string());
    }
    let mut store = StateStore::open(&state.state_file()).map_err(|error| error.to_string())?;
    store
        .update(
            id,
            RecordPatch {
                status: Some(TaskStatus::Pending),
                attempts: Some(0),
                timeout_count: Some(0),
                busy_retries: Some(0),
                ready_at: Some(0),
                thread_id: clear(),
                submitted_at: clear(),
                project_id: clear(),
                artifacts: clear(),
                files: clear(),
                error: clear(),
                // ⚠️ `ref_urls` / `ref_signature` **刻意不动** —— 重跑仍要带垫图。
                ..Default::default()
            },
        )
        .map_err(|error| format!("重置失败：{error}"))?;
    Ok(())
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
    if state
        .running
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .is_some()
    {
        return Err("正在跑批，先停止再确认（确认要改状态文件，和跑批写的是同一个文件）".to_string());
    }

    // ⚠️ **花钱的那一下，全仓只此一处**。
    let deps = to_client_deps(&resolve_connection()?)?;
    crate::lovart::client::confirm_thread(&deps, &thread_id)
        .map_err(|error| format!("确认失败：{error}"))?;

    // 上游会继续生成 ⇒ 推回 `running`，让轮询接管（表里 `running` 的 `job` = 复查）。
    let mut store = StateStore::open(&state.state_file()).map_err(|error| error.to_string())?;
    store
        .update(
            &id,
            RecordPatch {
                status: Some(TaskStatus::Running),
                error: Some(None),
                ..Default::default()
            },
        )
        .map_err(|error| format!("确认成功但状态没落盘（请报告）：{error}"))?;
    drop(store);

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
    tauri::async_runtime::spawn_blocking(move || probe_unfinished_core(&cwd))
        .await
        .map_err(|error| format!("探测线程异常退出：{error}"))?
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
    if state
        .running
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .is_some()
    {
        return Err("正在跑批，先停止再删除（删除要重写状态文件，和跑批写的是同一个文件）".to_string());
    }

    let mut store = StateStore::open(&state.state_file()).map_err(|error| error.to_string())?;
    store.remove(&ids).map_err(|error| error.to_string())
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
    let text = read_text(&state.cwd.join(".env"))
        .map_err(|error| format!("读不了 .env：{error}"))?
        .unwrap_or_default();
    let parsed = crate::lovart::connection::parse_dot_env(&text);
    let find = |key: &str| {
        parsed
            .entries
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.value.clone())
            .unwrap_or_default()
    };
    Ok(json!({
        "accessKey": find("LOVART_ACCESS_KEY"),
        "secretKey": find("LOVART_SECRET_KEY"),
        "concurrency": concurrency(),
        "outputDir": output_dir_name(),
    }))
}

/// 存设置：**按行**更新 `.env` 里那几个键，注释 / 空行 / 其它键原样保留。
///
/// ⚠️ 读失败**绝不能兜底成空**：那会把用户的配置整个冲掉（DEBT TD-05-19）。
#[tauri::command]
fn save_settings(state: tauri::State<'_, GuiState>, payload: Value) -> Result<(), String> {
    let path = state.cwd.join(".env");
    let _guard = state
        .env_lock
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut updates: Vec<(&str, String)> = Vec::new();
    if let Some(value) = payload.get("accessKey").and_then(Value::as_str) {
        updates.push(("LOVART_ACCESS_KEY", value.trim().to_string()));
    }
    if let Some(value) = payload.get("secretKey").and_then(Value::as_str) {
        updates.push(("LOVART_SECRET_KEY", value.trim().to_string()));
    }
    // 并发数 / 输出文件夹也落进 `.env` —— 它们是**真在用的**（见 `concurrency()` / `output_dir_name()`），
    // 不是只显示不生效的装饰。
    if let Some(value) = payload.get("concurrency").and_then(Value::as_u64) {
        updates.push(("LOVART_CONCURRENCY", value.clamp(1, 8).to_string()));
    }
    if let Some(value) = payload.get("outputDir").and_then(Value::as_str) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            updates.push(("LOVART_OUTPUT_DIR", trimmed.to_string()));
        }
    }
    update_env_keys(&path, &updates)?;
    // 写回后立刻**重载**（覆盖），免得这一轮还用着旧凭据。
    // ⚠️ 必须是 `reload_dot_env` 而不是 `load_dot_env`：后者是「不覆盖」语义
    // （刻意让真实环境变量优先），而 `GuiState::new` 启动时已经载入过一次
    // ⇒ 用它会**整段跳过** —— 文件改了、运行时还用旧密钥（DEBT TD-05-11）。
    reload_dot_env(&path)?;
    Ok(())
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
    let running = state.running.lock().unwrap_or_else(|error| error.into_inner());
    match running.as_ref() {
        Some(batch) => {
            batch.cancelled.store(true, Ordering::SeqCst);
            // 取消是**协作式**的：最坏要等当前那次 HTTP 请求超时（60 秒）才真的退出。
            // 这句必须让用户看到 —— 否则点完界面没动静，用户以为按钮坏了。
            state.push_alert("已请求停止：最坏要等当前这次请求结束（约 60 秒）才真正退出");
            Ok(())
        }
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
    tauri::Builder::default()
        .manage(GuiState::new(cwd))
        .invoke_handler(tauri::generate_handler![
            get_state,
            take_alerts,
            list_inbox,
            list_models,
            import_inbox,
            publish_queue,
            retry_one,
            retry_abnormal,
            preview_confirm,
            confirm_task,
            probe_unfinished,
            resume_one,
            resume_unfinished,
            remove_tasks,
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
