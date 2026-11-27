//! GUI —— Tauri 壳：前端的唯一入口。
//!
//! ── 职责边界 ────────────────────────────────────────────────────────
//! 这一层**只做三件事**：读状态给前端看 · 列 inbox 并校验 · 起/停一个批次。
//! 它**不重新实现任何业务规则** —— 校验走 `parse_prompts`、开跑走 `service::api`、
//! 看板数据走 `service::snapshot::build_status_json`，与 CLI 是**同一份代码**。
//! （若在这里抄第二份解析/渲染，"窗口里看到的"与"终端看到的"必然分叉。）
//!
//! ── 为什么前端用轮询而不是事件推送 ──────────────────────────────────
//! 状态文件是**唯一真相**，而写它的是调度器线程。与其维护一套事件协议、
//! 不如让前端每秒重读一次状态文件 —— 少一套会漂移的并行真相。
//! （而且这也让「另一个进程在跑批」时窗口照样能看，见方案的「观察模式」。）

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tauri::{Emitter, Manager};
use tauri_plugin_dialog::DialogExt;

use crate::engine::lockfile::FileLock;
use crate::engine::settings::{self, Settings};
use crate::engine::state::{
    is_unfinished, row as status_row, Action, RecordPatch, StateStore, TaskRecord, TaskStatus,
};
use crate::engine::text::{read_text, read_text_required};
use crate::service::wiring::to_client_deps;
use crate::lovart::config::REQUEST_TIMEOUT_MS;
use crate::lovart::connection::{
    load_dot_env, reload_dot_env, resolve_connection, update_env_keys, AUTO_CONFIRM_KEY,
};
use crate::lovart::prompt::{parse_prompts, ParseOptions, PromptDefaults, PromptTask};
use crate::service::api::RunSource;
use crate::service::snapshot::{is_retryable, unchanged_payload};
use lovart_core::service::{Draft, Workbench};

use crate::assistant::{self, ChatError, ChatState, ChatStore};

// ── 命令 ──────────────────────────────────────────────────────────────

/// GUI 自己的看板缓存（`2B-2`）：只存上次的指纹与「已持续」原料。
///
/// ⚠️ 缓存**只在 GUI**：service 里没有缓存字段 ⇒ HTTP `/state` 不可能拿到
/// `unchanged`（`ADR-0005` 的"一个能力一个入口"）。
#[derive(Default)]
struct GuiViewCache {
    key: String,
    elapsed: Vec<(String, Option<String>)>,
}

impl GuiViewCache {
    fn clear(&mut self) {
        self.key.clear();
        self.elapsed.clear();
    }
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
async fn get_state(
    state: tauri::State<'_, Arc<Workbench>>,
    view: tauri::State<'_, Mutex<GuiViewCache>>,
) -> Result<Value, String> {
    // ── 为什么是 `async`（与 `probe_unfinished` 同一条理由）──────────
    // Tauri 的**同步**命令在主线程上 inline 执行，而这里要读整个 `state.jsonl`
    // 再整体序列化给 IPC（实测 2263 条 ≈ 200ms）。每秒堵主线程 200ms = 每秒掉一批帧。
    // ⇒ 读盘那半段挪进 `spawn_blocking`，界面线程始终能响应。
    let since = {
        let cache = view.lock().unwrap_or_else(|error| error.into_inner());
        if cache.key.is_empty() {
            None
        } else {
            Some(cache.key.clone())
        }
    };
    let core = Arc::clone(state.inner());
    let snapshot = tauri::async_runtime::spawn_blocking(move || core.snapshot(since.as_deref()))
        .await
        .map_err(|error| format!("状态线程异常退出：{error}"))??;

    let mut cache = view.lock().unwrap_or_else(|error| error.into_inner());
    match snapshot.payload {
        // 命中：只回「已持续」那几条（用上次全量留下的原料重算）。
        None => Ok(unchanged_payload(&cache.elapsed)),
        Some(payload) => {
            cache.key = snapshot.key;
            cache.elapsed = snapshot.active;
            Ok(payload)
        }
    }
}

/// 取走**尚未读过的**批次级告警（取走即清，不重复弹）。
///
/// 这是 [`Arc<Workbench>::push_alert`] 的**唯一消费口** —— 与「有产无消」的旧 `get_log`
/// 相对。前端每轮轮询调一次；返回非空就显示给用户。
#[tauri::command]
fn take_alerts(state: tauri::State<'_, Arc<Workbench>>) -> Vec<String> {
    state.take_alerts()
}

/// 列 `inbox/*.jsonl`，**导入前就校验**（复用 `parse_prompts`，与 CLI 同一份契约）。
///
/// 为什么要在导入前校验：未知模型名在**出图时**才会静默退回默认模型 ——
/// 几百条一起跑就是整批跑错。宁可在这里就报出来。
#[tauri::command]
fn list_inbox(state: tauri::State<'_, Arc<Workbench>>) -> Result<Value, String> {
    list_inbox_core(&state.inbox_dir())
}

/// 列 inbox 的**核心** —— 不依赖 Tauri（理由与 [`import_inbox_core`] 相同：
/// 能力要有一个不绑框架的入口，否则只能靠真跑 GUI 才能验证）。
pub fn list_inbox_core(dir: &Path) -> Result<Value, String> {
    // 与真正开跑**同一份**兜底参数（`.env` 的 LOVART_DEFAULT_*）：导入前的校验/预览
    // 若用内置默认值，用户会看到"检查通过"而跑出来的是另一组参数。
    let (defaults, _) = PromptDefaults::from_env();
    let mut files: Vec<Value> = Vec::new();

    // 扫目录（不存在⇒空 / 读不了⇒报错 / 过滤 / 排序）只有 `list_files_in` 一处实现：
    // 素材库列图片走同一条，校验与组形状各留各的（plan 20 §2.5 S2）。
    for path in crate::service::files::list_files_in(dir, &["jsonl"])? {
        files.push(inspect_jsonl(&path, &defaults, true)?);
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
    state: tauri::State<'_, Arc<Workbench>>,
    app: tauri::AppHandle,
) -> Result<Option<String>, String> {
    let mut dialog = app
        .dialog()
        .file()
        .set_title("选择提示词文件（.jsonl）")
        .add_filter("提示词文件", &["jsonl"]);
    let inbox = state.inbox_dir();
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
fn inspect_prompt_file(state: tauri::State<'_, Arc<Workbench>>, path: String) -> Result<Value, String> {
    let path = PathBuf::from(&path);
    if !path.extension().is_some_and(|ext| ext == "jsonl") {
        return Err(format!("不是 .jsonl 文件：{}", path.display()));
    }
    let (defaults, _) = PromptDefaults::from_env();
    let in_inbox = is_inbox_child(&state.inbox_dir(), &path);
    inspect_jsonl(&path, &defaults, in_inbox)
}

/// 出图模型清单（前端下拉用）。
///
/// ⚠️ **与 `--list-models --json` 共用同一份投影**（`service::meta::image_models_json`）——
/// 这里曾自己 `filter + map` 一遍，键名写成 `id`（CLI 那边是 `alias`）：
/// 同一份数据、同一个二进制、两个键名，正是「同一个事实两个投影」。
/// 别名才是本工具的对外契约（`prompts.jsonl` 的 `model` 字段写的就是它）。
#[tauri::command]
fn list_models() -> Vec<Value> {
    crate::service::meta::image_models_json()
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
    Ok(Value::Array(parsed.tasks.iter().map(task_input_json).collect()))
}

/// 一条解析出的任务 → 前端契约（键名与 `intake::TaskInput`、`prompts.jsonl` 一致）。
///
/// ⚠️ **唯一投影**：`import_inbox`（`prompts_json`）与 `validate_draft` 都从这里出。
///    写成 `aspectRatio`/`imageSize` 就等于让前端为这两条命令各留一处改名 ——
///    而记录（`TaskRecord`）用的是 `aspect_ratio`/`image_size`，那是**另一个**
///    持久化 schema，不要跟着改。
/// ⚠️ **键集必须恰好是 `TaskInput` 那 5 个**：前端把返回的整个对象入队，多出一个键
///    就会随「待发 → 导出 JSONL」写进文件，而 `parse_prompts` 对未知字段是 fail-loud。
///    加字段前先想清楚它是契约的一部分还是只给界面看。
fn task_input_json(task: &PromptTask) -> Value {
    json!({
        "prompt": task.prompt,
        "model": task.model,
        "ratio": task.aspect_ratio,
        "size": task.image_size,
        "refs": task.ref_inputs.clone().unwrap_or_default(),
    })
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
    let tasks: Vec<Value> = parsed.tasks.iter().map(task_input_json).collect();
    Ok(json!({ "tasks": tasks, "errors": parsed.errors }))
}

/// 把待发文本存成 `.jsonl`（路径由前端保存对话框给）。
///
/// 与 `prompts.jsonl` **完全同构** ⇒ 存下来的文件可以直接
/// `lovart-client --prompts 它 --check`，也能被「导入」读回来。
#[tauri::command]
fn export_draft(
    state: tauri::State<'_, Arc<Workbench>>,
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
    let inbox = state.inbox_dir();
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
fn import_inbox(state: tauri::State<'_, Arc<Workbench>>, file: String) -> Result<Value, String> {
    import_inbox_core(&state.inbox_dir(), Path::new(&file))
}

/// 把前端读出来的 `data:` 垫图落成本机文件，返回绝对路径。
///
/// ── 为什么必须有它（2026-11-09 实测）────────────────────────────────
/// 粘贴 / 文件选择得到的垫图只有 base64。原样塞进发布参数时，同一张图会在
/// 200 条任务里**重复序列化 200 遍**（单张 10MB → base64 约 13MB ⇒ 2.6GB 字符串）
/// ⇒ V8 抛 `RangeError: Invalid string length`，**整批发不出去**。
/// 落成文件后参数里只有短路径；`refs.rs` 读盘并按内容哈希去重，同一张图仍只上传一次。
///
/// 文件名 = 内容 sha1：同一张图重复调用直接复用，不重复写盘。
///
/// **async**：解码 10MB base64 是几百毫秒级的活，不能压在主线程上。
#[tauri::command]
async fn save_ref_image(state: tauri::State<'_, Arc<Workbench>>, data: String) -> Result<String, String> {
    let dir = state.refs_dir();
    // **唯一实现**在 `engine::refs`：命令行、GUI、预设、素材库都走它（命名按目录定）。
    tauri::async_runtime::spawn_blocking(move || {
        let source = crate::engine::refs::ImageSource::Data { value: data };
        crate::engine::refs::materialize_image(&dir, &source)
    })
    .await
    .map_err(|error| format!("保存垫图线程异常退出：{error}"))?
}

// ── 提示词预设（plan 18）──────────────────────────────────────────────
//
// 形状在 `lovart::preset`、目录由 `Workbench::presets_dir()` 决定；这里只做
// 「取 State → 调核心 → 拼前端形状」。失败带 reason，措辞由前端 `presetMessage` 翻译。

// 预设图片的可读名（`portrait-1a2b3c4d.png` ⇒ `portrait`）不再是本文件的规则：
// 文件名是 `engine::refs::materialize_image` 造的，派生名跟着它走（`engine::refs::label_of`）。

/// 失败 → `{ok:false, reason, detail}`（判别位不折成值，ADR-0003）。
fn preset_err(error: &crate::lovart::preset::PresetError) -> Value {
    json!({ "ok": false, "reason": error.reason(), "detail": error.detail() })
}

/// `{ ok:true, presets:[...] }` / `{ ok:false, reason, detail }`。
/// 真相只有 `data/presets/manifest.json` —— 一次性 localStorage 迁移已完成（用户裁定 2026-11-26），
/// 不再回 `fresh` 位、不再读 localStorage。
#[tauri::command]
fn get_presets(state: tauri::State<'_, Arc<Workbench>>) -> Value {
    match crate::lovart::preset::load(&state.presets_dir()) {
        Ok(list) => json!({ "ok": true, "presets": list }),
        Err(error) => preset_err(&error),
    }
}

#[tauri::command]
fn create_preset(
    state: tauri::State<'_, Arc<Workbench>>,
    name: String,
    text: String,
    images: Vec<crate::lovart::preset::PresetImage>,
) -> Value {
    let dir = state.presets_dir();
    let clean = text.trim().to_string();
    if clean.is_empty() {
        return preset_err(&crate::lovart::preset::PresetError::Empty);
    }
    let mut list = match crate::lovart::preset::load(&dir) {
        Ok(list) => list,
        Err(error) => return preset_err(&error),
    };
    let id = crate::lovart::preset::new_id(&list);
    let item = crate::lovart::preset::Preset {
        id,
        name: name.trim().to_string(),
        text: clean,
        images,
    };
    list.insert(0, item);
    match crate::lovart::preset::save(&dir, &list) {
        Ok(()) => json!({ "ok": true }),
        Err(error) => preset_err(&error),
    }
}

#[tauri::command]
fn update_preset(
    state: tauri::State<'_, Arc<Workbench>>,
    id: String,
    name: String,
    text: String,
    images: Vec<crate::lovart::preset::PresetImage>,
) -> Value {
    let dir = state.presets_dir();
    let clean = text.trim().to_string();
    if clean.is_empty() {
        return preset_err(&crate::lovart::preset::PresetError::Empty);
    }
    let mut list = match crate::lovart::preset::load(&dir) {
        Ok(list) => list,
        Err(error) => return preset_err(&error),
    };
    let Some(at) = list.iter().position(|item| item.id == id) else {
        return preset_err(&crate::lovart::preset::PresetError::Missing);
    };
    list[at] = crate::lovart::preset::Preset {
        id,
        name: name.trim().to_string(),
        text: clean,
        images,
    };
    match crate::lovart::preset::save(&dir, &list) {
        Ok(()) => json!({ "ok": true }),
        Err(error) => preset_err(&error),
    }
}

#[tauri::command]
fn remove_preset(state: tauri::State<'_, Arc<Workbench>>, id: String) -> Value {
    let dir = state.presets_dir();
    let mut list = match crate::lovart::preset::load(&dir) {
        Ok(list) => list,
        Err(error) => return preset_err(&error),
    };
    let before = list.len();
    list.retain(|item| item.id != id);
    if list.len() == before {
        return preset_err(&crate::lovart::preset::PresetError::Missing);
    }
    match crate::lovart::preset::save(&dir, &list) {
        Ok(()) => json!({ "ok": true }),
        Err(error) => preset_err(&error),
    }
}

/// 前端拖入 / 选中的图 → 落进 `data/presets/`，返回 `{file,path,label}`（保序同长）。
#[tauri::command]
async fn import_preset_images(
    state: tauri::State<'_, Arc<Workbench>>,
    inputs: Vec<String>,
) -> Result<Value, String> {
    let dir = state.presets_dir();
    tauri::async_runtime::spawn_blocking(move || {
        let mut out = Vec::new();
        for input in inputs {
            // 两个入口（路径 / `data:`）只是"前端从哪拿到这张图"，落盘实现与命名只一条。
            let source = if input.to_lowercase().starts_with("data:") {
                crate::engine::refs::ImageSource::Data { value: input }
            } else {
                crate::engine::refs::ImageSource::Path { value: input }
            };
            let path = crate::engine::refs::materialize_image(&dir, &source)?;
            let file = Path::new(&path).file_name().unwrap_or_default().to_string_lossy().to_string();
            let label = crate::engine::refs::label_of(&file);
            out.push(json!({ "file": file, "path": path, "label": label }));
        }
        Ok(json!({ "ok": true, "images": out }))
    })
    .await
    .map_err(|error| format!("导入预设图片线程异常退出：{error}"))?
}

/// 取用时把 `file` 拼成绝对路径。**不做存在性检查**（用户裁定 2026-11-26）：
/// 文件被删了就把路径照样给前端 ⇒ 缩略图显示破图，用户自己看得出来、自己处理。
/// 这里多判一次 missing，前端就多一条分支要维护，收益为零。
#[tauri::command]
fn resolve_preset_images(state: tauri::State<'_, Arc<Workbench>>, files: Vec<String>) -> Value {
    let dir = state.presets_dir();
    let images: Vec<Value> = files
        .into_iter()
        .map(|file| {
            let path = dir.join(&file);
            json!({
                "file": file,
                "path": path.display().to_string(),
                "label": crate::engine::refs::label_of(&file),
            })
        })
        .collect();
    json!({ "ok": true, "images": images })
}

// ── 素材库（plan 20）────────────────────────────────────────────────────
//
// **库 = `data/library/` 目录本身**：不建索引、不查引用、不弹「检测到新文件」。
// 形状与三个动作在 `service::library`；这里只做「取 State → 调核心 → 拼前端形状」。

/// 失败 → `{ok:false, reason, detail}`（判别位不折成值，ADR-0003）。
fn library_err(error: &crate::service::library::LibraryError) -> Value {
    json!({ "ok": false, "reason": error.reason(), "detail": error.detail() })
}

/// 列库里的图片 —— **每次都重扫目录**（目录是唯一真相，没有索引可对不上）。
#[tauri::command]
fn list_library_images(state: tauri::State<'_, Arc<Workbench>>) -> Value {
    match crate::service::library::list_images(&state.library_dir()) {
        Ok(images) => json!({ "ok": true, "images": images }),
        Err(error) => library_err(&error),
    }
}

/// 把一张图**复制**进素材库（viewer 按钮与弹窗「＋」共用这一条命令）。
///
/// `source` = `{kind:'path'|'data', value}`：一个入参形状、两种来源（ADR-0003）。
/// URL 图在前端就禁用入口，到不了这里。
///
/// ⚠️ 失败一律装在 `{ok:false, reason, detail}` 的**返回值**里（`Ok` 包着），
///    不抛 `Err` —— 前端只有一条失败通道（翻译 `reason`），不学两套（ADR-0003）。
///    async 命令带 `State` 时 Tauri 强制返回 `Result`，所以外面这层 `Ok` 只是壳。
#[tauri::command]
async fn send_to_library(
    state: tauri::State<'_, Arc<Workbench>>,
    source: crate::engine::refs::ImageSource,
) -> Result<Value, String> {
    let dir = state.library_dir();
    // 解码 10MB base64 是几百毫秒级的活，不压在主线程上（与 `save_ref_image` 同由）。
    let result =
        tauri::async_runtime::spawn_blocking(move || crate::service::library::send(&dir, &source))
            .await;
    Ok(match result {
        Ok(Ok(name)) => json!({ "ok": true, "name": name }),
        Ok(Err(error)) => library_err(&error),
        Err(error) => json!({
            "ok": false,
            "reason": "storage",
            "detail": format!("发送线程异常退出：{error}"),
        }),
    })
}

/// 删库里的一个文件（**只删文件**：编辑区里正引用的那张会变破图，那是可见反馈）。
#[tauri::command]
fn remove_library_image(state: tauri::State<'_, Arc<Workbench>>, file: String) -> Value {
    match crate::service::library::remove(&state.library_dir(), &file) {
        Ok(()) => json!({ "ok": true }),
        Err(error) => library_err(&error),
    }
}

/// 导入的**核心** —— 不依赖 Tauri，集成测试直接打这里。
///
/// （`import_inbox` 只是把 `cwd` 从 `State` 里取出来。分开的理由与 `service::snapshot::build_status_json`
/// 相同：**能力要有一个不绑框架的入口**，否则只能靠真跑 GUI 才能验证。）
pub fn import_inbox_core(inbox: &Path, file: &Path) -> Result<Value, String> {
    let value = prompts_json(file)?;
    // ⚠️ 只归位 `inbox/` 的**直属**文件。用户从别的目录挑来的文件属于他自己的文件夹：
    // 搬走 = 替用户动了别人的目录（且不可逆）。防重复导入靠前端队列里看得见的重复条目。
    if is_inbox_child(inbox, file) {
        move_to_imported(inbox, file)?;
    }
    Ok(value)
}

/// 这个文件是不是 `inbox/` 的直属子项（`inbox/imported/` 里的不算）。
///
/// 比较**父目录**而不是拼字符串：`..`、相对路径、符号链接都能对上真实位置。
fn is_inbox_child(inbox: &Path, file: &Path) -> bool {
    let Some(parent) = file.parent() else {
        return false;
    };
    let Ok(inbox) = std::fs::canonicalize(inbox) else {
        return false;
    };
    std::fs::canonicalize(parent).is_ok_and(|dir| dir == inbox)
}

/// 把导入过的 inbox 文件移进 `inbox/imported/`。
///
/// 重名**加时间戳**，不覆盖 —— 同名文件很可能是**另一批**提示词，覆盖 = 丢数据。
fn move_to_imported(inbox: &Path, file: &Path) -> Result<(), String> {
    let dir = inbox.join("imported");
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

/// 发布队列 —— 落文件后走**与 CLI 同一条开跑路径**。
#[tauri::command]
async fn publish_queue(
    state: tauri::State<'_, Arc<Workbench>>,
    tasks: Vec<crate::engine::intake::TaskInput>,
) -> Result<(), String> {
    // 结构化入参：**不再**落 `queue.jsonl` 再读回来（那是同一份数据的第二次序列化）。
    // 校验 / id / 默认值全部由 `intake` 一处完成，与 CLI 解析文件共用同一批函数。
    state.publish(tasks).map(|_| ())
}

/// 全部草稿（外部递送进来、等人确认的）。`refs` 已是前端能显示的路径 / URL。
#[tauri::command]
async fn get_drafts(state: tauri::State<'_, Arc<Workbench>>) -> Result<Vec<Draft>, String> {
    Ok(state.drafts())
}

/// 按 id 删草稿（删单条传 `[id]`；发布成功与「载入编辑区」取走都走它）。
#[tauri::command]
async fn clear_drafts(
    state: tauri::State<'_, Arc<Workbench>>,
    ids: Vec<String>,
) -> Result<usize, String> {
    Ok(state.clear_drafts(&ids))
}

/// 重跑 = **显式**把这条重置为待提交。
///
/// ⚠️ 原先这件事是**副作用**：适配层伪造一个"没有垫图"的声明 ⇒ 调度器判定"参数变了"
///    ⇒ 顺手把记录重置了（**顺手也把垫图清掉了** ⇒ 于是只能加一条闸
///    「用了垫图就不许重跑」）。现在重置就写在下面：**垫图与指纹原样保留** ——
///    它们还是有效的 CDN 地址，重跑当然要接着用。
///
/// ⚠️ 必须做在起批**之前**：派工只处理**非终态**记录，终态的没重置就不会被处理。
fn reset_for_retry(state: &tauri::State<'_, Arc<Workbench>>, id: &str) -> Result<(), String> {
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
async fn retry_one(
    state: tauri::State<'_, Arc<Workbench>>,
    id: String,
) -> Result<(), String> {
    let store = StateStore::open_read_only(&state.state_file()).map_err(|error| error.to_string())?;
    let record = store
        .all()
        .into_iter()
        .find(|item| item.id == id)
        .ok_or_else(|| format!("找不到这条任务：{id}"))?
        .clone();
    if !is_retryable(record.status) {
        return Err("这一条重跑没有意义（内容没过审 / 已按政策拒绝）".to_string());
    }
    // ⚠️ 原先这里有第二道闸：「用了垫图就不许重跑」。它不是设计，是在硬扛
    //    `record_to_task` 丢字段 —— 重跑必须重置记录，而旧路子的重置会把垫图一起清掉。
    //    现在重置是**显式**的、**垫图原样保留** ⇒ 闸撤掉，能力恢复。
    reset_for_retry(&state, &id)?;
    state.start_run(RunSource::Records(vec![id]))
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
async fn resume_one(
    state: tauri::State<'_, Arc<Workbench>>,
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
    state.start_run(RunSource::Records(vec![id]))
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
fn preview_confirm(state: tauri::State<'_, Arc<Workbench>>, id: String) -> Result<Value, String> {
    let (_record, thread_id) = confirmable(&state, &id)?;
    let deps = to_client_deps(&resolve_connection()?, &state.effective())?;
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
    state: &tauri::State<'_, Arc<Workbench>>,
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
async fn confirm_task(
    state: tauri::State<'_, Arc<Workbench>>,
    id: String,
) -> Result<String, String> {
    let (_record, thread_id) = confirmable(&state, &id)?;
    // 跑批中也允许确认：写回走调度器锁内，与 worker 串行（旧的「先停止」闸撤掉）。

    // ⚠️ **花钱的那一下，全仓只此一处**。
    let deps = to_client_deps(&resolve_connection()?, &state.effective())?;
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
    state.start_run(RunSource::Records(vec![id]))?;
    Ok("已确认，正在接着跑".to_string())
}

/// 重跑全部**可重跑**的异常。返回一句人话说明跳过了什么。
#[tauri::command]
fn retry_abnormal(state: tauri::State<'_, Arc<Workbench>>) -> Result<String, String> {
    let store = StateStore::open_read_only(&state.state_file()).map_err(|error| error.to_string())?;
    let all: Vec<_> = store.all().into_iter().cloned().collect();
    let retryable: Vec<_> = all
        .iter()
        .filter(|record| is_retryable(record.status))
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
    state.start_run(RunSource::Records(ids))?;
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
///
/// `settings` 由调用方给（`Workbench::effective()`）：探测是**只读一次性**动作，
/// 但它用的是同一份批次参数，不另开第二个读环境的口子（`docs/plan/29`）。
fn probe_unfinished_core(
    env_path: &Path,
    state_file: &Path,
    settings: &Settings,
) -> Result<Value, String> {
    load_dot_env(env_path)?;
    let connection = resolve_connection()?;
    let deps = to_client_deps(&connection, settings)?;
    let store = StateStore::open_read_only(state_file)
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
                // ⚠️ 缺 `status` 不许当成空串：前端会把空串显示成「未提交过」——
                //    那是把「读不懂响应」说成「上游没这条」，比报错更误导。
                let upstream = value
                    .as_object()
                    .and_then(|object| object.get("status"))
                    .and_then(Value::as_str)
                    .filter(|status| !status.is_empty());
                match upstream {
                    Some(status) => {
                        probes.push(json!({ "id": record.id, "upstream": status }));
                    }
                    None => probes.push(json!({
                        "id": record.id,
                        "error": "上游响应缺少 status 字段，无法判定",
                    })),
                }
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
async fn probe_unfinished(
    state: tauri::State<'_, Arc<Workbench>>,
    view: tauri::State<'_, Mutex<GuiViewCache>>,
) -> Result<Value, String> {
    let env_path = state.env_path();
    let state_file = state.state_file();
    let settings = state.effective();
    let probes = tauri::async_runtime::spawn_blocking(move || {
        probe_unfinished_core(&env_path, &state_file, &settings)
    })
        .await
        .map_err(|error| format!("探测线程异常退出：{error}"))?;
    // ⚠️ 探测结论**不落** `state.jsonl`（它是界面标题行那句话的原料）⇒ 文件指纹不变。
    //    不作废缓存，标题行会永远停在「探测中…」（`upstreamSummary()` 进了前端签名，
    //    但签名只在收到全量载荷时才算）。
    view.lock().unwrap_or_else(|error| error.into_inner()).clear();
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
fn resume_unfinished(state: tauri::State<'_, Arc<Workbench>>) -> Result<String, String> {
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
    state.start_run(RunSource::Records(ids))?;
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
async fn remove_tasks(state: tauri::State<'_, Arc<Workbench>>, ids: Vec<String>) -> Result<usize, String> {
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

/// 删除看板上**某一组**的记录（无调度器分支），返回删除条数。
///
/// 「哪一组」由调用方传判据（`engine::group::is_board_*`）—— 这是**唯一**的"改文件"实现：
/// 三组各抄一份的话，`ids.is_empty()`（一条都没有时**不写盘**）这类细节迟早漏一份。
///
/// 一次 `remove` = 一次 `compact`（临时文件 + rename）⇒ 要么全删要么全不删。
///
/// ⚠️ `pub` 是为了集成测试（`tests/cancel_all.rs` 直接测"选谁 + 该不该写盘"）；
/// 业务调用一律走 `remove_board_tasks`（有调度器时走调度器的 `remove_where`）。
pub fn remove_board_tasks_core(state_file: &Path, keep: fn(TaskStatus) -> bool) -> Result<usize, String> {
    let mut store = StateStore::open(state_file).map_err(|error| error.to_string())?;
    let ids: Vec<String> = store
        .all()
        .into_iter()
        .filter(|record| keep(record.status))
        .map(|record| record.id.clone())
        .collect();
    if ids.is_empty() {
        // 一条都没有时**不写盘**：没有要删的就别重写整个文件。
        return Ok(0);
    }
    store.remove(&ids).map_err(|error| error.to_string())
}

/// 按**判据**批量删记录：调度器在就走它（唯一写者），否则直接改文件。
///
/// 三个板级清理动作（未结束 / 排队 / 已完成）只差"挑哪些"这一个判据，
/// 删除路径只有这一条 ⇒ 不再出现三份 `match scheduler_snapshot()`。
fn remove_board_tasks(
    state: &Arc<Workbench>,
    view: &Mutex<GuiViewCache>,
    keep: fn(TaskStatus) -> bool,
) -> Result<usize, String> {
    let cleared = match state.scheduler_snapshot() {
        Some(scheduler) => scheduler
            .remove_where(keep)
            .map_err(|error| error.message().to_string())?,
        None => remove_board_tasks_core(&state.state_file(), keep)?,
    };
    view.lock().unwrap_or_else(|error| error.into_inner()).clear();
    Ok(cleared)
}

/// 清空所有已完成任务：删除 `finished` 组的全部记录，返回删除条数。
///
/// 不碰批次（已结束的记录本来没人在跟）；只删记录，图片文件与报告都不动。
#[tauri::command]
async fn clear_finished_tasks(
    state: tauri::State<'_, Arc<Workbench>>,
    view: tauri::State<'_, Mutex<GuiViewCache>>,
) -> Result<usize, String> {
    remove_board_tasks(
        state.inner(),
        view.inner(),
        crate::engine::group::is_board_finished,
    )
}

/// 取消**排队中**（还没提交）的任务：删除 `queued` 组的记录，返回删除条数。
///
/// 与「取消所有未完成」的区别：作用域只有 `queued`，**不停批次**、不关会话 ——
/// 已提交的（`active`）和上游等你确认的（`waiting`）都不动。
/// 这些记录还没有 `threadId`（上游不知道它们存在）⇒ 删除没有上游副作用。
#[tauri::command]
async fn cancel_queued_tasks(
    state: tauri::State<'_, Arc<Workbench>>,
    view: tauri::State<'_, Mutex<GuiViewCache>>,
) -> Result<usize, String> {
    remove_board_tasks(
        state.inner(),
        view.inner(),
        crate::engine::group::is_board_queued,
    )
}

/// 取消所有未完成任务：**停会话** + 删除看板未结束的记录，返回 `{ cleared }`。
///
/// ⚠️ 比另外两个多一步"停会话"（`remove_open` 内部置 `stopped` / `cancelled` 并
/// 关掉本次会话），所以它不能走 [`remove_board_tasks`]；没有调度器时只剩"删记录"。
#[tauri::command]
async fn cancel_all_tasks(
    state: tauri::State<'_, Arc<Workbench>>,
    view: tauri::State<'_, Mutex<GuiViewCache>>,
) -> Result<Value, String> {
    let cleared = match state.scheduler_snapshot() {
        Some(scheduler) => scheduler
            .remove_open()
            .map_err(|error| error.message().to_string())?,
        // 还没建调度器 ⇒ 没有写者在跑。
        None => remove_board_tasks_core(&state.state_file(), crate::engine::group::is_board_open)?,
    };
    view.lock().unwrap_or_else(|error| error.into_inner()).clear();
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
fn open_output_dir(state: tauri::State<'_, Arc<Workbench>>) -> Result<(), String> {
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
fn get_settings(state: tauri::State<'_, Arc<Workbench>>) -> Result<Value, String> {
    // **读文件**，不读进程环境（`docs/plan/07` N7）：
    // 手改 `.env` 后打开设置页，若读环境会显示旧值，用户一点保存就把手改覆盖掉。
    let (current, warnings) = Settings::from_file(&state.env_path())?;
    // **文件值 ≠ 生效值**的那几个键：由 `drift` 算，界面只渲染（`docs/plan/29`）。
    // 不在这里手写「重启后生效」——那句话会和代码再漂一次。
    let pending_restart = current.drift(&state.effective());
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
        // 空 = 文件与生效值一致（没有待重启的项）。
        "pendingRestart": pending_restart,
        "warnings": warnings,
    }))
}

/// 存设置：**按行**更新 `.env` 里那几个键，注释 / 空行 / 其它键原样保留。
///
/// ⚠️ 读失败**绝不能兜底成空**：那会把用户的配置整个冲掉（DEBT TD-05-19）。
#[tauri::command]
fn save_settings(state: tauri::State<'_, Arc<Workbench>>, payload: Value) -> Result<Value, String> {
    let path = state.env_path();
    let _guard = state.lock_env();

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
    // 而 `Workbench::new` 启动时已经载入过一次 ⇒ 用它会**整段跳过**
    // —— 文件改了、运行时还用旧值（DEBT TD-05-11）。
    //
    // reload 失败**不回滚、不返回 Err**：盘已经改了，报错会让用户以为没保存而反复点。
    let mut warnings = file_warnings;
    if let Err(error) = reload_dot_env(&path) {
        warnings.push(format!(
            "已写入 .env，但重新载入失败：{error}；请重启后再跑"
        ));
    }
    // 保存后重算「单轮上限」+「哪些项要重启」：**同一次读文件**（N17 不让前端自己算）。
    // `pendingRestart` 用刚写盘的文件比生效值，而不是拿 `current` 拼：差异写的语义是
    // 「只改被改的那几项」，用 `current` 算会把这次没碰的键也当成没变（结论来源不对）。
    let (max_wall_clock_min, pending_restart) = match Settings::from_file(&path) {
        Ok((saved, _)) => (
            saved.max_wall_clock_min(),
            saved.drift(&state.effective()),
        ),
        // 读不回刚写的文件（极少见）：上限沿用保存前的，待重启项**不猜**。
        Err(_) => (current.max_wall_clock_min(), Vec::new()),
    };
    Ok(json!({
        "warnings": warnings,
        "updated": updates.len(),
        "maxWallClockMin": max_wall_clock_min,
        "pendingRestart": pending_restart,
    }))
}

/// 切换全局自动确认开关 —— **唯一的写入口**（顶栏 chip 用它）。
///
/// ⚠️ 顺序必须是「先写盘成功、后改内存」：反过来会出现"界面显示开、磁盘是 false"，
/// 重启后悄悄变回去 —— 又一个"看着生效其实没生效"。
#[tauri::command]
fn set_auto_confirm(state: tauri::State<'_, Arc<Workbench>>, enabled: bool) -> Result<bool, String> {
    let _guard = state.lock_env();
    let path = state.env_path();
    let value = if enabled { "true" } else { "false" };
    update_env_keys(&path, &[(AUTO_CONFIRM_KEY, value.to_string())])?;
    state.set_auto_confirm(enabled);
    Ok(enabled)
}

/// 切换**花积分模式** —— 唯一的写入口（顶栏 chip 用它）。
///
/// ── 它是什么 ────────────────────────────────────────────────────────
/// 只决定「提交前把服务端设成哪条通道」：开 ⇒ fast（不排队、按张扣积分），
/// 关 ⇒ 免费排队。**不落盘**：重启后恒关（忘了关就是整批按 fast 计费）。
/// 与「自动确认」无关 —— fast 只买"不排队"，**不跳过**上游的确认（`lovart/task.rs`）。
///
/// ── 为什么只改内存、不在这里发 `/mode/set` ─────────────────────────
/// 服务端模式的**唯一写点**是调度器的 `ensure_mode`（每条提交前一次，参数按这个
/// 开关现算）。在这里再补一次"关闭时立刻还原 `Queue`"，等于造出第二个写点、
/// 第二种失败语义，而收益只是把窗口从"下一条提交前"缩到"此刻" ——
/// 不提交本来就不花钱。见 `docs/plan/14` §5「不做」。
#[tauri::command]
fn set_spend_mode(state: tauri::State<'_, Arc<Workbench>>, enabled: bool) -> bool {
    state.set_spend_mode(enabled);
    enabled
}

/// 停止当前批次（协作式取消：调度器会在下一个检查点退出）。
#[tauri::command]
async fn stop_run(
    state: tauri::State<'_, Arc<Workbench>>,
    view: tauri::State<'_, Mutex<GuiViewCache>>,
) -> Result<(), String> {
    // 取消是**协作式**的：最坏要等当前那次 HTTP 请求超时（60 秒）才真的退出。
    // 这句必须让用户看到 —— 否则点完界面没动静，用户以为按钮坏了。
    let ack = state.stop()?;
    view.lock().unwrap_or_else(|error| error.into_inner()).clear();
    state.push_alert(&ack.note);
    Ok(())
}

// ⚠️ `get_log` / `get_cwd` 已删（2026-10-01，DEBT TD-05-15）：
// 注册了但**前端一次都没调过** —— 幽灵预留。`get_log` 还拖着整套日志机制
// （写了没人读，就是 TD-05-21「批次失败不可见」的成因）。
// 要日志面板 ⇒ **先在前端做出来、再留命令**，顺序不能反。

// ── 启动 ──────────────────────────────────────────────────────────────

/// 工作区 = **程序所在目录**（`docs/plan/19` §3.4）。唯一判据。
///
/// - Windows：`lovart-client.exe` 所在目录；
/// - macOS：`.app` **同级**目录 —— exe 在包内 `Contents/MacOS/`，那不是用户放文件的地方
///   （改包内容会破坏签名、升级会被覆盖），所以取 `.app` 这一层的父目录。
///
/// ⚠️ 没有 `LOVART_HOME`、没有 cwd 候选、没有"看目录内容猜是不是工作区"。
/// 用户原话：「软件在哪，工作区就在哪，跟着 exe 走的」。
///
/// 纯函数（不读环境、不碰磁盘）⇒ 可直接测。
pub fn install_dir_from_exe(exe: &Path) -> Option<PathBuf> {
    // macOS 的 .app 包：exe 在 <...>/X.app/Contents/MacOS/ 里 ⇒ 取 X.app 的同级。
    for ancestor in exe.ancestors() {
        if ancestor.extension().is_some_and(|ext| ext == "app") {
            return ancestor.parent().map(Path::to_path_buf);
        }
    }
    // 其它平台（Windows / Linux）：exe 所在目录。
    // ⚠️ 相对文件名的 parent() 是空路径 ⇒ 明确 None，不许拿空路径当工作区（那等于偷用 cwd）。
    exe.parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

/// GUI 的**工作区**（`data/` 所在的那一层）= [`install_dir_from_exe`] 的结果。
///
/// ── 为什么不用当前目录 ──────────────────────────────────────────────
/// macOS 从 Finder 双击 `.app`，进程当前目录是 **`/`** —— 既不是程序所在目录，
/// 也不是放数据的地方（写不进去）。Windows 双击时 cwd 虽然通常是 exe 同级，
/// 但那只是巧合 —— 从快捷方式 / 别处启动时就不成立。
/// ⇒ **只认程序位置**，不看 cwd、不看环境变量、不看目录内容。
///
/// ── 初始化 ≠ 兜底 ──────────────────────────────────────────────────
/// 首次启动在程序所在目录建 `data/`：位置就是用户放程序的那一层，一眼能找到。
/// 没密钥是正常状态（`load_dot_env` 对不存在的 `.env` 静默）⇒ 正常进界面让用户填。
/// 建 `data/` 失败（例如装进只读目录）⇒ **报错退出**：那是真的错了，不兜底、不闪退伪装。
pub fn workspace_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("取不到程序路径，无法确定工作区：{error}"))?;
    let install = install_dir_from_exe(&exe)
        .ok_or_else(|| format!("程序路径 {} 没有所在目录", exe.display()))?;
    std::fs::create_dir_all(install.join("data"))
        .map_err(|error| format!("在 {} 建 data/ 失败：{error}", install.display()))?;
    eprintln!("工作区：{}（数据在其 data/ 下）", install.display());
    Ok(install)
}

// ── AI 对话（`docs/plan/21`）─────────────────────────────────────────────
//
// 六条命令形状一致：**业务失败也走 `Ok`**（`{status:'error', …}`），`Err` 只留给崩溃。
// `ChatError` 不是 `String` ⇒ 想让失败走 `Err` 必须手写转换，不会顺手写出来（ADR-0004）。
//
// 每个命令都重建一次 `ChatConfig`（读进程环境）：手改 `.env` 后下一条命令就生效，
// 不在内存里缓存配置（那是第二处真相）。

fn chat_config(state: &tauri::State<'_, Arc<Workbench>>) -> assistant::ChatConfig {
    assistant::ChatConfig::from_env(state.chat_system_prompt_file())
}

/// 独立窗口的 label（`docs/plan/22`）。
const ASSISTANT_WINDOW: &str = "assistant";

/// 告诉**另一个窗口**"状态变了" —— 只广播事实，不推状态：
/// 真相在 `chat-history.json` 里，另一边自己重读一次即可（不搞快照/增量合并）。
fn emit_changed(window: &tauri::Window) {
    let other = if window.label() == ASSISTANT_WINDOW {
        "main"
    } else {
        ASSISTANT_WINDOW
    };
    let _ = window.app_handle().emit_to(other, "assistant://changed", ());
}

/// 短事务：锁 → 刷新配置 → 改一项 → 回全量 `ChatState`。
fn with_chat<F>(
    state: &tauri::State<'_, Arc<Workbench>>,
    chat: &tauri::State<'_, Arc<Mutex<ChatStore>>>,
    window: &tauri::Window,
    action: F,
) -> Value
where
    F: FnOnce(&mut ChatStore) -> Result<ChatState, ChatError>,
{
    let mut guard = chat.lock().unwrap_or_else(|error| error.into_inner());
    guard.set_config(chat_config(state));
    match action(&mut guard) {
        Ok(next) => {
            emit_changed(window);
            json!({ "status": "ok", "state": next })
        }
        Err(error) => error.to_json(),
    }
}

#[tauri::command]
fn assistant_get_state(
    state: tauri::State<'_, Arc<Workbench>>,
    chat: tauri::State<'_, Arc<Mutex<ChatStore>>>,
    window: tauri::Window,
) -> Result<Value, String> {
    Ok(with_chat(&state, &chat, &window, |store| store.state()))
}

#[tauri::command]
fn assistant_new(
    state: tauri::State<'_, Arc<Workbench>>,
    chat: tauri::State<'_, Arc<Mutex<ChatStore>>>,
    window: tauri::Window,
) -> Result<Value, String> {
    Ok(with_chat(&state, &chat, &window, |store| {
        store.new_conversation()
    }))
}

#[tauri::command]
fn assistant_switch(
    state: tauri::State<'_, Arc<Workbench>>,
    chat: tauri::State<'_, Arc<Mutex<ChatStore>>>,
    window: tauri::Window,
    id: String,
) -> Result<Value, String> {
    Ok(with_chat(&state, &chat, &window, |store| store.select(&id)))
}

#[tauri::command]
fn assistant_rename(
    state: tauri::State<'_, Arc<Workbench>>,
    chat: tauri::State<'_, Arc<Mutex<ChatStore>>>,
    window: tauri::Window,
    id: String,
    title: String,
) -> Result<Value, String> {
    Ok(with_chat(&state, &chat, &window, |store| {
        store.rename(&id, &title)
    }))
}

#[tauri::command]
fn assistant_delete(
    state: tauri::State<'_, Arc<Workbench>>,
    chat: tauri::State<'_, Arc<Mutex<ChatStore>>>,
    window: tauri::Window,
    id: String,
) -> Result<Value, String> {
    Ok(with_chat(&state, &chat, &window, |store| store.delete(&id)))
}

/// 发送：**锁只持有两小段**（取快照 / 追加本轮），请求在锁外（`I-2`）。
///
/// ⚠️ **必须 `async` + `spawn_blocking`**：这一步是阻塞 HTTP + 轮询（最长 60 秒）。
/// 写成同步命令会占住 IPC 线程 —— 发送期间整个界面（看板轮询、按钮、拖拽）全部停摆，
/// 用户看到的就是"非常的卡"。业务层逻辑一行没变，只是把阻塞工作挪出 IPC 线程。
#[tauri::command]
async fn assistant_send(
    state: tauri::State<'_, Arc<Workbench>>,
    chat: tauri::State<'_, Arc<Mutex<ChatStore>>>,
    transport: tauri::State<'_, Arc<Mutex<Option<assistant::lovart_chat::LovartCompletion>>>>,
    window: tauri::Window,
    conversation_id: String,
    text: String,
    image_paths: Vec<String>,
) -> Result<Value, String> {
    let workbench = state.inner().clone();
    let store = chat.inner().clone();
    let slot = transport.inner().clone();

    let joined = tauri::async_runtime::spawn_blocking(move || {
        {
            let mut guard = store.lock().unwrap_or_else(|error| error.into_inner());
            guard.set_config(assistant::ChatConfig::from_env(
                workbench.chat_system_prompt_file(),
            ));
        }
        let arg = assistant::SendArg {
            conversation_id,
            text,
            image_paths,
        };

        // 上游通道懒建一次并常驻：`LovartCompletion` 自持 project 缓存，
        // 每条命令重建就会每句话都建一个新 project。
        // ⚠️ `try_lock`：上一条还在等回复时**立刻告诉用户**，而不是让第二条在后台排队
        // （排队等于"点一下没反应"，也就是用户说的卡）。
        let mut guard = match slot.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                return assistant::SendResult::NotSent(assistant::ChatError::Upstream {
                    kind: assistant::UpstreamKind::Io,
                    why: "上一条还在等回复，等它回来再发（这条没有发出去）".to_string(),
                })
            }
        };
        if guard.is_none() {
            // 凭据拿不到 ⇒ **业务失败走 Ok**（不是 Err）：插件要看到 not_sent + 文案。
            let missing_credentials = |why: String| {
                assistant::SendResult::NotSent(assistant::ChatError::MissingConfig {
                    missing: vec![assistant::MissingPart::Credentials],
                    prompt_path: workbench.chat_system_prompt_file().display().to_string(),
                    why: why,
                })
            };
            let connection = match resolve_connection() {
                Ok(connection) => connection,
                Err(error) => return missing_credentials(format!("网关凭据不可用：{error}")),
            };
            let mut deps = match to_client_deps(&connection, &workbench.effective()) {
                Ok(deps) => deps,
                Err(error) => return missing_credentials(format!("网关凭据不可用：{error}")),
            };
            // 对话的总预算沿用出图那个"单请求"口径（plan 23 §5：不自造数值）
            deps.task_timeout = std::time::Duration::from_millis(REQUEST_TIMEOUT_MS);
            *guard = Some(assistant::lovart_chat::LovartCompletion::new(deps));
        }
        let transport = guard.as_ref().expect("刚建好");
        assistant::send(&store, &arg, transport)
    })
    .await
    .map_err(|error| format!("发送线程异常：{error}"))?;

    if matches!(joined, assistant::SendResult::Ok { .. }) {
        emit_changed(&window);
    }
    Ok(joined.to_json())
}

// ── AI 对话：独立窗口（`docs/plan/22`）─────────────────────────────────
//
// 窗口归 Rust 管（开/关/停靠各一条命令）：前端拿不到窗口对象，就不可能出现两处关闭逻辑。

#[tauri::command]
async fn open_assistant_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(ASSISTANT_WINDOW) {
        // 已经开着 ⇒ 只提到前面（重复点「↗」不该开出第二个窗口）
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return Ok(());
    }

    tauri::WebviewWindowBuilder::new(
        &app,
        ASSISTANT_WINDOW,
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("AI 对话")
    .inner_size(420.0, 780.0)
    .min_inner_size(320.0, 420.0)
    // **不要系统标题栏**：否则最上面一排是「— □ ×」、下面一排是我们自己的
    // 「+ 📌 ×」，两排按钮、两个关闭键。窗口由插件顶栏承担标题栏职责
    // （`data-tauri-drag-region` 提供拖动区）。
    .decorations(false)
    .resizable(true)
    // 标记"这是对话窗口"：`app.js` 靠它早退，不跑主界面那套轮询/拖放
    .initialization_script("window.__ASSISTANT_WINDOW__ = true;")
    .build()
    .map_err(|error| format!("打不开对话窗口：{error}"))?;
    Ok(())
}

/// 把本机图片塞进对话窗口（主窗口「发送到 AI 对话」的唯一出口）。
///
/// ⚠️ 不直接 `emit` 图片：窗口可能是**这一刻才新建**的，监听器还没注册 ⇒ 事件会丢。
/// 所以先入队、再通知，窗口起来后自己取（`assistant_take_pending`）。
/// 主窗口 → 对话窗口的一次性交接队列。
///
/// 为什么是模块级 static 而不是 Tauri managed state：它只承载"开窗前塞进来的图片"，
/// 生命周期是**一次命令**；做成全局托管状态反而多一处需要清理的东西。
static PENDING_ATTACH: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[tauri::command]
async fn assistant_attach(app: tauri::AppHandle, paths: Vec<String>) -> Result<(), String> {
    {
        let mut pending = PENDING_ATTACH
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for path in paths {
            if !pending.contains(&path) {
                pending.push(path);
            }
        }
    }
    open_assistant_window(app.clone()).await?;
    if let Some(window) = app.get_webview_window(ASSISTANT_WINDOW) {
        let _ = window.set_focus();
    }
    let _ = app.emit_to(ASSISTANT_WINDOW, "assistant://attach", ());
    Ok(())
}

/// 取走待加图片（窗口启动与收到通知时各调一次；取走即清）。
#[tauri::command]
fn assistant_take_pending() -> Vec<String> {
    let mut pending = PENDING_ATTACH
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    std::mem::take(&mut *pending)
}

/// 把主窗口叫回来（对话窗口顶栏的「主页面」按钮调）。
///
/// 只做三件事：还原、显示、聚焦 —— 不新建窗口（主窗口是应用本体，
/// 它被关掉就等于退出程序，这里不会出现 `None` 之外的情况）。
#[tauri::command]
fn open_main_window(app: tauri::AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "主窗口不存在".to_string())?;
    window
        .unminimize()
        .map_err(|error| format!("还原主窗口失败：{error}"))?;
    window
        .show()
        .map_err(|error| format!("显示主窗口失败：{error}"))?;
    window
        .set_focus()
        .map_err(|error| format!("聚焦主窗口失败：{error}"))?;
    Ok(())
}

/// 对话窗口置顶开关（前端图钉按钮调；默认不置顶）。
#[tauri::command]
fn set_assistant_on_top(app: tauri::AppHandle, on_top: bool) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(ASSISTANT_WINDOW) {
        window
            .set_always_on_top(on_top)
            .map_err(|error| format!("置顶失败：{error}"))?;
    }
    Ok(())
}

#[tauri::command]
fn close_assistant_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(ASSISTANT_WINDOW) {
        window
            .close()
            .map_err(|error| format!("关不掉对话窗口：{error}"))?;
    }
    Ok(())
}

#[tauri::command]
fn dock_assistant_window(app: tauri::AppHandle) -> Result<(), String> {
    // 先让主窗口把面板打开、再关窗：反过来会出现"窗口没了、面板也没开"的空档
    let _ = app.emit_to("main", "assistant://dock", ());
    if let Some(window) = app.get_webview_window(ASSISTANT_WINDOW) {
        window
            .close()
            .map_err(|error| format!("关不掉对话窗口：{error}"))?;
    }
    Ok(())
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
    // 锁路径由 `Workbench` 给：与 `state_file` / `output_dir` 同一处定义（ADR-0001）。
    let core = Workbench::new(cwd);
    let lock_path = core.lock_path();
    // 会话真相文件的唯一写者：路径只从 `Workbench` 的具名访问器取（plan 19），
    // 构造在 `.manage(core)` 之前（`core` 随后被 move 进 Tauri）。
    let chat_store = Mutex::new(ChatStore::new(
        core.chat_history_file(),
        assistant::ChatConfig::from_env(core.chat_system_prompt_file()),
    ));
    tauri::Builder::default()
        // 原生文件对话框（`pick_prompt_file` 用它）：提示词文件不绑定 `inbox/`。
        .plugin(tauri_plugin_dialog::init())
        .manage(core)
        // 看板缓存只在 GUI 层（service 无缓存 ⇒ HTTP 不可能拿到 unchanged）。
        .manage(Mutex::new(GuiViewCache::default()))
        // AI 对话：唯一的并发闸（`I-3`），不另造第二把锁。
        // `Arc` 是给 `assistant_send` 用的：阻塞网络要丢到阻塞线程池（见那条命令的注释）。
        .manage(Arc::new(chat_store))
        // 对话的上游通道懒建一次（它自持 project 缓存，重建会每句话建一个 project）
        .manage(Arc::new(Mutex::new(
            None::<assistant::lovart_chat::LovartCompletion>,
        )))
        .setup(move |app| {
            // `state.jsonl` 只有一个写者（`engine::lockfile` 文件头）。
            // 放在 setup 而不是 `run()` 开头：抢不到时要能弹对话框 ——
            // GUI 没有可见终端（`windows_subsystem = "windows"`），
            // 只 `eprintln!` 就是「双击没反应」。
            match FileLock::acquire(&lock_path) {
                Ok(lock) => app.state::<Arc<Workbench>>().hold_lock(lock),
                Err(message) => {
                    app.dialog()
                        .message(message)
                        .title("lovart · 已有实例在运行")
                        .blocking_show();
                    std::process::exit(1);
                }
            }
            // 本机 HTTP：只服务同一台机器上的另一个前端（token 在 `var/http-token`）。
            // 失败只告警，不阻断 GUI（端口占用 / token 读失败）。
            crate::http::spawn(app.state::<Arc<Workbench>>().inner().clone());
            Ok(())
        })
        // 关窗前把攒批的状态落盘：合并写之后不 drain，最多丢最近 2 秒的进度。
        // ⚠️ 只落盘、**不停 worker**（`shutdown` 要 join 在飞 HTTP，最坏 60 秒）。
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::CloseRequested { .. }) {
                if let Some(state) = window.try_state::<Arc<Workbench>>() {
                    if let Some(scheduler) = state.scheduler_snapshot() {
                        let _ = scheduler.flush_now();
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            take_alerts,
            list_inbox,
            list_models,
            pick_prompt_file,
            inspect_prompt_file,
            import_inbox,
            save_ref_image,
            get_presets,
            create_preset,
            update_preset,
            remove_preset,
            import_preset_images,
            resolve_preset_images,
            list_library_images,
            send_to_library,
            remove_library_image,
            validate_draft,
            export_draft,
            publish_queue,
            get_drafts,
            clear_drafts,
            retry_one,
            retry_abnormal,
            preview_confirm,
            confirm_task,
            probe_unfinished,
            resume_one,
            resume_unfinished,
            remove_tasks,
            clear_finished_tasks,
            cancel_queued_tasks,
            cancel_all_tasks,
            reveal_path,
            open_output_dir,
            get_settings,
            save_settings,
            set_auto_confirm,
            set_spend_mode,
            stop_run,
            assistant_get_state,
            assistant_new,
            assistant_switch,
            assistant_rename,
            assistant_delete,
            assistant_send,
            open_assistant_window,
            assistant_attach,
            assistant_take_pending,
            open_main_window,
            set_assistant_on_top,
            close_assistant_window,
            dock_assistant_window
        ])
        .run(tauri::generate_context!())
        .expect("启动窗口失败");
}

#[cfg(test)]
mod tests {
    use super::*;
    // 看板投影已下沉到 `service::snapshot`，这几项只在用例里直接构造。
    use lovart_core::service::snapshot::ViewInput;
    use std::collections::BTreeSet;

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

    /// 任务契约的键名：`import_inbox` 与 `validate_draft` 必须发 `ratio`/`size`。
    ///
    /// 曾经一处发 `aspectRatio`/`imageSize`、另一处发 `ratio`/`size` —— 同一个
    /// `PromptTask` 两个投影，前端只好为每条命令各留一处改名。
    #[test]
    fn task_input_json_uses_input_keys() {
        let task = PromptTask {
            id: "t".to_string(),
            prompt: "p".to_string(),
            model: "nano-bn-2".to_string(),
            aspect_ratio: "3:4".to_string(),
            image_size: "2K".to_string(),
            line: 1,
            ref_inputs: Some(vec!["C:/refs/a.png".to_string()]),
            ref_files: None,
        };
        let value = task_input_json(&task);
        assert_eq!(value["ratio"], json!("3:4"));
        assert_eq!(value["size"], json!("2K"));
        assert_eq!(value["refs"], json!(["C:/refs/a.png"]));
        assert!(
            value.get("aspectRatio").is_none(),
            "记录 schema 的键名不该出现在任务契约里"
        );
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("任务契约必须是对象")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["model", "prompt", "ratio", "refs", "size"],
            "键集必须恰好是 TaskInput 的 5 个：前端会整对象入队，多一个键会让导出的 JSONL 被判「未知字段」"
        );
    }

    /// `list_inbox` 的扫目录段改调 `list_files_in`（plan 20 §2.5 S2）后，组合行为要有闸：
    /// 只列 `.jsonl`、目录不存在给空列表（不是报错）。
    #[test]
    fn list_inbox_只列_jsonl_目录不存在给空() {
        let dir = temp_dir("inbox-core");
        std::fs::write(dir.join("b.jsonl"), "{\"prompt\":\"p\"}\n").expect("写提示词");
        std::fs::write(dir.join("a.jsonl"), "{\"prompt\":\"q\"}\n").expect("写提示词");
        std::fs::write(dir.join("note.txt"), "x").expect("写杂文件");

        let value = list_inbox_core(&dir).expect("应能列");
        let files = value["files"].as_array().expect("files 是数组");
        let names: Vec<&str> = files.iter().filter_map(|file| file["name"].as_str()).collect();
        assert_eq!(names, ["a.jsonl", "b.jsonl"], "只列 jsonl，且按文件名排序");

        // 目录不存在 = 正常（还没让 AI 写过东西）⇒ 空列表，不是错误。
        let empty = list_inbox_core(&dir.join("never")).expect("不存在不算失败");
        assert!(empty["files"].as_array().expect("files 是数组").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
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
            spend_mode: false,
            board_rev: 0,
        }
    }

    #[test]
    fn v1_指纹没变_第二次必须命中且只回计时() {
        let dir = temp_dir("v1");
        let since = "2026-10-24T00:00:00.000Z";
        std::fs::create_dir_all(dir.join("var")).expect("建 var");
        let file = dir.join("var/state.jsonl");
        std::fs::write(&file, record_line("t1", "running", Some(since))).expect("写状态文件");

        // 走真实构造：`state_file` / `output_dir` 与生产路径同一套推导
        let state = Workbench::new(dir);
        let first = state.snapshot(None).expect("首次全量");
        let payload = first.payload.expect("首次必须是全量");
        assert_eq!(payload["tasks"].as_array().map(Vec::len), Some(1));
        assert_eq!(payload["tasks"][0]["following"], json!(false));

        let hit = state.snapshot(Some(&first.key)).expect("第二次");
        assert!(hit.payload.is_none(), "命中时不许再带全量任务（那 4MB 就是白搭的）");
        let unchanged = unchanged_payload(&first.active);
        assert_eq!(unchanged["unchanged"], json!(true));
        assert_eq!(unchanged["elapsed"][0]["id"], json!("t1"));
        assert!(
            unchanged["elapsed"][0]["text"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "命中时仍然要重算「已持续」——它是唯一每秒都在变的东西"
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

}
