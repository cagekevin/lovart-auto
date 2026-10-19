//! `run` — 命令行入口（对应 TS `src/run.ts`）。
//!
//! ```text
//! lovart-client [选项]
//!
//!   --prompts <file>         提示词文件，JSONL（默认 prompts.jsonl）
//!   --output <dir>           图片输出目录（默认 output）
//!   --state <file>           断点续跑状态（默认 var/state.jsonl）
//!   --report <file>          结果报告 CSV（默认 var/report.csv）
//!   --no-report              不写报告文件
//!   --ref <路径|URL|data:>   参考图（图生图），可重复或用逗号分隔
//!   --refs-cache <file>      参考图 CDN 缓存（默认 var/refs-cache.json）
//!   --no-refs-cache          不读写参考图缓存
//!   --concurrency <n>        并发任务数，硬上限 8（默认 7）
//!   --model <alias>          默认模型（默认 nano-bn-2）
//!   --ratio <w:h>            默认比例（默认 3:4）
//!   --size <1K|2K|4K>        默认尺寸（默认 2K）
//!   --max-attempts <n>       可重试失败上限（默认 3）
//!   --max-busy-retries <n>   402/409 重投上限（默认 300）
//!   --task-interval-ms <n>   全局任务间隔（ms，默认 1000；0 关闭）
//!   --retry-failed           重置无 threadId 的 Failed 并重跑
//!   --max-timeouts <n>       累计超时多少次后释放槽位（默认 3）
//!   --download-retries <n>   单张产物下载重试次数（默认 3）
//!   --task-timeout <分钟>    单任务轮询总超时（默认 90）
//!   --recheck-timeout <分钟> 复查已超时任务的轮询超时（默认 30）
//!   --allow-unknown-model    未知模型名只警告，不终止（默认终止）
//!   --auto-confirm           自动确认高成本操作（会扣积分；每任务每批最多一次）
//!   --no-auto-confirm        关闭自动确认（覆盖 .env）
//!   --check                  只校验提示词文件（**给 AI 自检用**）：不联网、不提交
//!   --dry-run                只解析并打印计划，不提交任何任务
//!   --list-models            列出可用的出图模型别名（从代码生成，权威来源）
//!   --status                 任务看板：读 state 文件，列出在跑/已结束的任务与对应提示词
//!   --status-table           打印状态表（唯一真源：从代码里那张表渲染，文档不再抄一份）
//!   --full                   配合 --status：提示词不截断
//!   --json                   配合 --status：输出机器可读 JSON（供客户端读取）
//!   -h / --help              打印本说明
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::cli::{arg_list, arg_number, arg_string, has_arg, parse_args, ParsedArgs};
use crate::engine::refs::{load_refs_cache, read_ref_inputs, refs_signature, save_refs_cache};
use crate::engine::scheduler::{run_scheduler, AutoConfirm, SchedulerOptions, SchedulerSummary};
use crate::engine::state::{
    is_not_done, is_unfinished, row as status_row, Action, StateStore, TaskRecord, TaskStatus,
    STATUS_TABLE,
};
use crate::lovart::attachments::resolve_attachments;
use crate::lovart::client::{query_mode, set_mode, ClientDeps};
use crate::lovart::connection::{load_dot_env, read_auto_confirm, resolve_connection, to_client_deps};
use crate::lovart::models::MODEL_SPECS;
use crate::lovart::prompt::{parse_prompts, ParseOptions, PromptDefaults, PromptTask};

/// 服务端最多同时 8 个任务；硬编码上限，避免配错把自己打死。
const HARD_MAX_CONCURRENCY: usize = 8;

/// 允许的参数名。**必须与文件头的用法注释一致** ——
/// 多写会放过拼错的参数，少写会把合法参数当未知而报错。
fn known_args() -> BTreeSet<String> {
    [
        "prompts",
        "output",
        "state",
        "report",
        "no-report",
        "ref",
        "refs-cache",
        "no-refs-cache",
        "concurrency",
        "model",
        "ratio",
        "size",
        "max-attempts",
        "max-busy-retries",
        "task-interval-ms",
        "retry-failed",
        "max-timeouts",
        "download-retries",
        "task-timeout",
        "recheck-timeout",
        "allow-unknown-model",
        "auto-confirm",
        "no-auto-confirm",
        "dry-run",
        "check",
        "list-models",
        "status",
        "status-table",
        "full",
        "json",
        "help",
    ]
    .iter()
    .map(|name| name.to_string())
    .collect()
}

/// 解析 `--auto-confirm` / `--no-auto-confirm`（互斥）。`None` = 没表态，交给 `.env`。
///
/// 纯函数（不读文件、不联网）⇒ 可直接测。两个同时给是**用户意图冲突**，不猜。
fn auto_confirm_flag(args: &ParsedArgs) -> Result<Option<bool>, String> {
    let on = has_arg(args, "auto-confirm");
    let off = has_arg(args, "no-auto-confirm");
    if on && off {
        return Err("--auto-confirm 与 --no-auto-confirm 不能同时使用".to_string());
    }
    Ok(if on {
        Some(true)
    } else if off {
        Some(false)
    } else {
        None
    })
}

/// 结束时的对账行（N=0 不打印）。
fn auto_confirm_line(count: usize) -> Option<String> {
    (count > 0).then(|| format!("本批自动确认 {count} 条"))
}

/// 全局停止标志 —— 由 SIGINT 处理器置位，watcher 线程再把它同步进调度器的取消标志。
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

fn log(message: &str) {
    let now = chrono::Local::now();
    println!("[{}] {message}", now.format("%H:%M:%S"));
    // 同一条文案同时进 `var/run.log`（未绑定时是空操作）。
    // **终端会消失，文件不会** —— 挂机排查靠的就是后者。
    crate::engine::runlog::line(message);
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// 启动 SIGINT 处理器：置全局标志，由 watcher 同步给调度器。
///
/// ⚠️ **为什么只做"优雅停止"、不做"落盘保命"**：
/// Rust 侧的写入是**同步**的（见 `engine::state` 文件头），每次状态变更都已落盘，
/// 所以 Ctrl+C 不会丢进度 —— 与 TS 那边"必须显式 flush 否则丢"完全不同。
/// 这里要的只是「别在轮询中途硬杀」，让收尾干净一点。
fn install_sigint_handler() {
    // 不引 ctrlc 依赖：`libc::signal` 就够（只需置一个标志）。
    unsafe {
        // 先转 `*const ()` 再转 `sighandler_t` —— 直接把函数项 cast 成整数会被警告。
        libc::signal(libc::SIGINT, handle_sigint as *const () as libc::sighandler_t);
    }
}

extern "C" fn handle_sigint(_signal: libc::c_int) {
    if STOP_REQUESTED.swap(true, Ordering::SeqCst) {
        // 第二次 Ctrl+C：用户不想等了，直接退出。
        std::process::exit(130);
    }
    // 写日志不是 async-signal-safe，所以这里只置标志；提示由 watcher 线程打。
}

// ── 报告 ──────────────────────────────────────────────────────────────

fn csv_cell(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// 记录里的产物路径 → **当前输出目录下能直接用的绝对路径**。
///
/// 记录里存的是**相对输出目录**的路径（例 `abc123_一只猫.png`）—— 理由见
/// `engine::scheduler` 里写 `files` 的那段注释：绝对路径把"文件夹叫什么、放在哪"
/// 钉进了数据。旧记录里是绝对路径 ⇒ **原样返回**（向后兼容；不替它猜搬到哪去了）。
pub fn resolve_file(output_dir: &Path, file: &str) -> PathBuf {
    let path = Path::new(file);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        output_dir.join(path)
    }
}

/// 写 CSV 报告（带 BOM，Excel 打开中文不乱码）。
pub fn write_report(
    file: &Path,
    records: &[TaskRecord],
    tasks_by_id: &BTreeMap<String, PromptTask>,
    output_dir: &Path,
) -> std::io::Result<()> {
    const HEADER: [&str; 11] = [
        "line",
        "id",
        "status",
        "mode",
        "model",
        "aspectRatio",
        "imageSize",
        "prompt",
        "files",
        "error",
        "updatedAt",
    ];
    let mut lines = vec![HEADER.join(",")];
    for record in records {
        let line = record
            .line
            .or_else(|| tasks_by_id.get(&record.id).map(|task| task.line))
            .map(|value| value.to_string())
            .unwrap_or_default();
        let mode = if tasks_by_id
            .get(&record.id)
            .and_then(|task| task.refs.as_ref())
            .is_some_and(|refs| !refs.is_empty())
        {
            "图生图"
        } else {
            "文生图"
        };
        let cells = [
            line,
            record.id.clone(),
            record.status.as_str().to_string(),
            mode.to_string(),
            record.model.clone(),
            record.aspect_ratio.clone(),
            record.image_size.clone(),
            record.prompt.clone(),
            // 报告给人看的 ⇒ 拼成绝对路径（记录里存的是相对路径，见 `resolve_file`）
            record
                .files
                .clone()
                .unwrap_or_default()
                .iter()
                .map(|item| resolve_file(output_dir, item).display().to_string())
                .collect::<Vec<_>>()
                .join(" | "),
            record.error.clone().unwrap_or_default(),
            record.updated_at.clone(),
        ];
        lines.push(cells.iter().map(|cell| csv_cell(cell)).collect::<Vec<_>>().join(","));
    }
    std::fs::write(file, format!("\u{feff}{}\n", lines.join("\n")))
}

/// 打印「非 done」清单 —— 直接回答「哪些没跑完」。
fn print_pending_list(records: &[TaskRecord]) {
    let not_done: Vec<&TaskRecord> = records
        .iter()
        .filter(|record| is_not_done(record.status))
        .collect();
    if not_done.is_empty() {
        return;
    }
    println!("\n--- 尚未出图（{} 条，同样写进了报告文件）---", not_done.len());
    for record in not_done {
        let line = record
            .line
            .map(|value| value.to_string())
            .unwrap_or_else(|| "?".to_string());
        let prompt: String = record.prompt.chars().take(36).collect();
        println!(
            "  {:<15} 第{}行  {}  {}  {}",
            record.status.as_str(),
            line,
            record.id,
            prompt,
            record.error.clone().unwrap_or_default()
        );
    }
}

/// 提示词截断（终端宽度友好）。
fn clip(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        format!("{}…", flat.chars().take(max).collect::<String>())
    } else {
        flat
    }
}

/// 给人看的状态短名。**读表**（`label` 列）—— 原先这里是一份穷尽 `match`。
fn status_label(status: TaskStatus) -> &'static str {
    status_row(status).label
}

/// 把「距今多久」格式化成 `45秒` / `12分30秒` / `1时05分`。
///
/// 拿不到时间（旧记录 / 非法值）⇒ `—`，**不猜**。
fn format_elapsed(from: Option<&str>) -> String {
    let Some(from) = from else {
        return "—".to_string();
    };
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(from) else {
        return "—".to_string();
    };
    let millis = chrono::Utc::now().signed_duration_since(parsed).num_milliseconds();
    if millis < 0 {
        return "—".to_string();
    }
    let total = millis / 1000;
    if total < 60 {
        return format!("{total}秒");
    }
    let minutes = total / 60;
    if minutes < 60 {
        return format!("{minutes}分{:02}秒", total % 60);
    }
    format!("{}时{:02}分", minutes / 60, minutes % 60)
}

/// 任务在界面上的**分组**。
///
/// ⚠️ 判据**只此一份**：由 Rust 已有的谓词派生，不另立清单。
/// 前端若自己再写一遍 `['pending','submitting',…]`，就是第二份真源 ——
/// 本仓栽过的那个坑（两处同名不同口径）。所以分组随状态一起下发。
///
/// - `active`   拿到槽位、正在生成（`holds_slot`）；
/// - `queued`   已发布但还没轮到（`is_unfinished` 里除去占槽的那部分）；
/// - `finished` 这一轮结束了，不管结果（`!is_unfinished`，含终态与「待下次兜底」）。
fn task_group(status: TaskStatus) -> &'static str {
    // 「要你处理」—— **你不点它就不动**。今天只有 `needs_confirm`（表里带 `Confirm` 动作）。
    //
    // ⚠️ 它**绝不能归进 `finished`**：`needs_confirm` 是终态（`terminal: true`），
    //    按"终态 ⇒ finished"的老口径它会躺在「已结束」里 ——
    //    用户看到的是「**它自己完成了，没问我**」（2026-10-01 实测反馈）。
    //    判据读**状态表**的 `actions` 列（有没有只有人能按的 `Confirm`），不另写清单。
    if status_row(status).actions.contains(&Action::Confirm) {
        return "waiting";
    }
    if crate::engine::state::holds_slot(status) {
        "active"
    } else if crate::engine::state::is_unfinished(status) {
        "queued"
    } else {
        "finished"
    }
}

/// 看板上「未结束」= `active` + `queued` + `waiting`（即**不在「已结束」组**）。
///
/// 这是「取消所有任务」的**作用域判据**，也是它唯一的一份实现（`gui` 直接调）。
///
/// ⚠️ **不要**用 `engine::state::is_unfinished` 顶替：后者是 `!terminal`，
/// 而 `needs_confirm`（待处理）是**终态** —— 于是"待处理"会被漏删。
/// 也**不要**复用 `resume_unfinished` 的过滤（它按 `is_unfinished` 选，问题不同）。
/// 判据就是分组本身（[`task_group`]），不另抄一份状态清单。
pub(crate) fn is_board_open(status: TaskStatus) -> bool {
    task_group(status) != "finished"
}

/// 异常状态 → 界面要用的两件套：`(色调, 一句人话)`。**读表**（`abnormal` 列）。
///
/// ⚠️ 文案里**不许出现实现黑话**（槽位 / 目录 / 500…），也不许口语化。
/// 用户看的是「我的图怎么了」，不是「我们的管道怎么了」。
/// 正常状态返回 `None` —— **正常不说话**，异常才多说一句。
fn abnormal_info(status: TaskStatus) -> Option<(&'static str, &'static str)> {
    status_row(status).abnormal
}

/// 这条异常**重跑有没有意义**。**读表**（`actions` 列里有没有 `Retry`）。
///
/// 「未产出」（内容没过审）与「已拒绝」（政策拒绝）重跑结果一样，
/// 所以表里那两行的 `actions` 里**没有** `Retry` —— 给了就是骗人。
/// 原先这里是 `matches!(status, DownloadFailed | Unresolved | Failed)`：
/// 一个集合字面量，加状态**不报错**、口径会静默漏（普查第 3 类）。
pub fn is_retryable(status: TaskStatus) -> bool {
    status_row(status).actions.contains(&Action::Retry)
}

/// 板面顺序 —— **终端看板与窗口共用这一份**。
///
/// ── 规则 ──────────────────────────────────────────────────────────
/// · **未结束**（处理中 / 排队）：按**先后顺序**（提示词行号 `line`）升序 —— 先发的在上；
/// · **已结束**：按**完成时间**（`stateSince`，进终态那一刻）**倒序** —— 最晚完成在最上。
///
/// ── 为什么必须有它 ────────────────────────────────────────────────
/// `StateStore::all()` 返回的是 `BTreeMap` 的**键序**，而键是**内容哈希 id**
/// ⇒ 不排的话顺序看起来是随机的，与提交/完成时间都无关。
/// （2026-10-01 实测：14:56 完成的那条 id 是 `1b9204894b5a`，不排就排在第 3 位。）
/// 原先 `print_status` 与 `build_status_json` **各写了一遍** `sort_by_key(line)`，
/// 且两边口径不同（终端按行号、窗口干脆不排）—— 这正是本仓禁的"两份真相"。
///
/// ⚠️ 完成时间**用 `stateSince` 而不是 `updatedAt`**：后者会被收尾时的顺带写污染，
///    拿它当完成时间就是撒谎（本仓已有这条警告）。
/// ⚠️ 旧记录没有 `stateSince`（TS 时代写的）⇒ 完成时间未知 ⇒ 降序时自然沉底，
///    **不拿 `updatedAt` 冒充**。
pub fn board_order(records: &mut [TaskRecord]) {
    records.sort_by(|a, b| match (is_unfinished(a.status), is_unfinished(b.status)) {
        // 都在跑/排队：按先后顺序（行号）。没有行号的用 id 兜底，保证顺序确定。
        (true, true) => a.line.cmp(&b.line).then_with(|| a.id.cmp(&b.id)),
        // 都已结束：完成时间倒序（`Option` 里 `None` 小于 `Some` ⇒ 未知的自然沉底）。
        (false, false) => b
            .state_since
            .cmp(&a.state_since)
            .then_with(|| a.id.cmp(&b.id)),
        // 跨组：还在跑的排前面 —— 打开就能先看到"正在动的"。
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
    });
}

/// 看板的**机器可读**形态（`--status --json`）。
///
/// 抽成公开函数而不是留在 `print_status` 里，是为了让 GUI **复用同一份构造**
/// —— 否则「终端看到的」与「窗口里看到的」会成为两份真相（DEBT TD-11-01 的母体）。
pub fn build_status_json(state_file: &Path, output_dir: &Path) -> Result<Value, String> {
    let state = StateStore::open_read_only(state_file).map_err(|error| error.to_string())?;
    let mut records: Vec<TaskRecord> = state.all().into_iter().cloned().collect();
    board_order(&mut records);

    let mut present: BTreeMap<String, bool> = BTreeMap::new();
    for record in &records {
        for file in record.files.clone().unwrap_or_default() {
            // 记录里存的是**相对输出目录**的路径（旧记录是绝对路径）⇒ 先归一成绝对再看盘。
            let resolved = resolve_file(output_dir, &file);
            present
                .entry(file.clone())
                .or_insert_with(|| resolved.is_file());
        }
    }
    let tasks: Vec<Value> = records
        .iter()
        .map(|record| {
            let mut value = serde_json::to_value(record).unwrap_or(Value::Null);
            let files_present: Vec<Value> = record
                .files
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|file| {
                    let ok = present.get(&file).copied().unwrap_or(false);
                    // 给前端的必须是**绝对路径**：`convertFileSrc` 与「打开所在位置」
                    // 都只认绝对路径（相对路径在 webview 里会被解析成 5180 的端口路径）。
                    let absolute = resolve_file(output_dir, &file).display().to_string();
                    json!({ "file": absolute, "present": ok })
                })
                .collect();
            // ⚠️ 必须在 `files_present` 被 move 进对象**之前**算。
            let missing_artifact = !is_not_done(record.status)
                && files_present
                    .iter()
                    .all(|entry| !entry.get("present").and_then(Value::as_bool).unwrap_or(false));

            if let Some(object) = value.as_object_mut() {
                object.insert("filesPresent".to_string(), Value::Array(files_present));
                object.insert(
                    "statusLabel".to_string(),
                    Value::String(status_label(record.status).to_string()),
                );
                object.insert(
                    "stateSinceText".to_string(),
                    Value::String(format_elapsed(record.state_since.as_deref())),
                );
                // ⚠️ 分组 / 异常 / **可做的动作**（`actions`）**一并下发**：前端不再自己写
                //    一遍状态清单，否则「窗口里看到的」与「终端看到的」会分叉
                //    （本仓 TD-11-01 的母体）。
                object.insert(
                    "group".to_string(),
                    Value::String(task_group(record.status).to_string()),
                );
                // ⚠️ 原先这里还有一个 `retryable` 布尔 —— 它与下面的 `actions` 说的是
                //    同一件事（"这条能不能重跑"），是**同一事实的两份投影** ⇒ 已删。
                //    Rust 侧的闸仍走 `is_retryable`（`gui.rs` 的重跑命令用它）。
                // 界面上能做的动作，**由状态表下发**（`actions` 列）——
                // 前端不再自己判"这条能不能重跑 / 能不能接着查"。
                object.insert(
                    "actions".to_string(),
                    Value::Array(
                        status_row(record.status)
                            .actions
                            .iter()
                            .map(|action| Value::String(action.as_str().to_string()))
                            .collect(),
                    ),
                );
                // 「说完成却没图」—— 判据是**表列 `not_done`**，不是 `status == "done"`。
                // 原先这个判断写在前端（`ui/app.js`：`task.status === 'done'`），
                // 是普查里"消费者自判"的两处之一 —— 前端不该知道 `done` 是什么。
                object.insert(
                    "missingArtifact".to_string(),
                    Value::Bool(missing_artifact),
                );
                object.insert(
                    "abnormal".to_string(),
                    match abnormal_info(record.status) {
                        // ⚠️ 只有 `(kind, reason)`：**状态名不在这里重复**
                        //    （卡片头部已经写了；重复一遍就是把版面撑长）。
                        Some((kind, reason)) => json!({ "kind": kind, "reason": reason }),
                        None => Value::Null,
                    },
                );
            }
            value
        })
        .collect();

    // 逐状态计数**由表生成**（遍历 `all()`，键名 = 线格式名的 camelCase）。
    // 原先这里是手抄的 9 行，**漏了 `submitting`** —— 而且加状态时不报错。
    let mut summary = serde_json::Map::new();
    summary.insert("total".to_string(), json!(records.len()));
    for status in crate::engine::state::all() {
        let count = records
            .iter()
            .filter(|record| record.status == status)
            .count();
        summary.insert(crate::engine::state::json_key(status), json!(count));
    }

    Ok(json!({
        "generatedAt": now_iso(),
        "stateFile": state_file.display().to_string(),
        "outputDir": output_dir.display().to_string(),
        "summary": summary,
        "tasks": tasks,
    }))
}

/// 任务看板：不联网、不需要凭据，只读 state 文件渲染。
///
/// 回答三件事：① 现在哪些任务在跑；② 每条在当前状态待了多久；③ 每张图对应哪条提示词。
fn print_status(state_file: &Path, output_dir: &Path, full: bool, as_json: bool) -> Result<(), String> {
    // 只读打开：不压实、不建目录 —— 生成进程可能正在追加写同一个文件。
    let state = StateStore::open_read_only(state_file).map_err(|error| error.to_string())?;
    let mut records: Vec<TaskRecord> = state.all().into_iter().cloned().collect();
    // ⚠️ 顺序**只此一份**（与窗口共用），别再在这里写一遍 `sort_by_key`。
    board_order(&mut records);

    // 核对产物是否真的在盘上（提示词与图对不上时，这里会立刻暴露）
    let mut present: BTreeMap<String, bool> = BTreeMap::new();
    for record in &records {
        for file in record.files.clone().unwrap_or_default() {
            let resolved = resolve_file(output_dir, &file);
            present
                .entry(file.clone())
                .or_insert_with(|| resolved.is_file());
        }
    }

    let count = |status: TaskStatus| records.iter().filter(|r| r.status == status).count();

    if as_json {
        let payload = build_status_json(state_file, output_dir)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
        return Ok(());
    }

    let width = if full { 500 } else { 74 };
    println!("================ 任务看板 ================");
    println!("状态文件 : {}", state_file.display());
    println!("输出目录 : {}", output_dir.display());
    println!("时间     : {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    println!();
    println!("任务总数 : {}", records.len());
    // 行集、短名、括号里那句话**全部读表** —— 与「汇总」共用同一条渲染路径。
    // 原先这 6 行是**第二份短名清单**（且括号里那句是 `rerun` 的另一种写法），
    // 加状态时它不报错、只是新状态不出现。
    let mut rows: Vec<_> = STATUS_TABLE
        .iter()
        .filter(|row| !row.not_done || row.abnormal.is_some())
        .collect();
    // 「已完成」打头（它是唯一"有产出"的那行），其余按表顺序 = 生命周期顺序。
    rows.sort_by_key(|row| row.not_done);
    for row in rows {
        println!(
            "{}: {}（{}）",
            pad_label(row.label),
            count(row.status),
            row.rerun
        );
    }

    let render = |record: &TaskRecord| {
        let line = record
            .line
            .map(|value| value.to_string())
            .unwrap_or_else(|| "?".to_string());
        // 「当前状态已持续」——旧记录没有 `state_since` 就显示 `—`。
        // **刻意不拿 `updated_at` 兜底**：它会被 `finish()` 的顺带写污染，
        // 用它冒充"状态起点"就是撒谎。宁可显示"未知"。
        let since = format_elapsed(record.state_since.as_deref());
        let submitted = record
            .submitted_at
            .as_deref()
            .map(|value| format!(" · 已提交 {}", format_elapsed(Some(value))))
            .unwrap_or_default();
        println!(
            "  第{}行  {}  {}  {} {}  {}  [已持续 {}]{}",
            line,
            record.id,
            record.model,
            record.aspect_ratio,
            record.image_size,
            status_label(record.status),
            since,
            submitted
        );
        println!("          {}", clip(&record.prompt, width));
        for file in record.files.clone().unwrap_or_default() {
            let ok = if present.get(&file).copied().unwrap_or(false) {
                "✅"
            } else {
                "❌ 文件缺失"
            };
            let name = Path::new(&file)
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or(file.clone());
            println!("          产物: {name}  {ok}");
        }
        if let Some(thread_id) = &record.thread_id {
            println!("          thread: {thread_id}");
        }
        if let Some(error) = &record.error {
            println!("          错误: {}", clip(error, width));
        }
    };

    let active: Vec<&TaskRecord> = records
        .iter()
        .filter(|record| is_unfinished(record.status))
        .collect();
    let finished: Vec<&TaskRecord> = records
        .iter()
        .filter(|record| !is_unfinished(record.status))
        .collect();

    if !active.is_empty() {
        println!("\n── 处理中（{}）────────────────────────────", active.len());
        for record in active {
            render(record);
        }
    }
    if !finished.is_empty() {
        println!("\n── 已结束（{}）────────────────────────────", finished.len());
        for record in finished {
            render(record);
        }
    }
    if records.is_empty() {
        println!("\n（状态文件里还没有任务。先跑一次生成，或用 --state 指定别的状态文件）");
    }
    println!("\n提示：--full 显示完整提示词 · --json 输出机器可读格式（供客户端读取）");
    Ok(())
}

// ── 出图模型清单（`--list-models` 与 GUI 下拉共用的**唯一**来源）────────────
//
// ⚠️ 这一整块（筛选 / 文本 / JSON）**必须待在一处**。它曾经散成三份：
//    筛选在 `models`、JSON 投影在 `run`、列宽硬编码在 `print_models` ——
//    于是 GUI 的键名写成 `id`、CLI 写成 `alias`（同一份数据两个键名），
//    而最长别名超了写死的列宽也没有任何东西变红。**一处数据、三处定义、三次漂移机会。**
//
// 为什么不放进 `lovart/models.rs`：那个文件是 `lovart_config.ts` 的**逐行镜像**
//    （见它的文件头），往里塞 Rust 独有的展示函数会毁掉可比对性。
//    这一块是**输出形态**，与 `build_status_json` 同类 —— 所以归这里，GUI 复用。

/// 出图模型清单 —— `(别名, 本名)`。**筛选只此一处**（`Category::Image`）。
fn image_models() -> Vec<(&'static str, &'static str)> {
    MODEL_SPECS
        .iter()
        .filter(|spec| spec.category == crate::lovart::models::Category::Image)
        .map(|spec| (spec.id, spec.readable_name))
        .collect()
}

/// JSON 投影 —— `--list-models --json` 与 GUI 的下拉**共用这一份**（**键名只此一处**）。
pub fn image_models_json() -> Vec<Value> {
    image_models()
        .into_iter()
        .map(|(alias, name)| json!({ "alias": alias, "name": name }))
        .collect()
}

/// 文本表格投影 —— `--list-models` 用。
///
/// 列宽**从数据算**，不写死：曾经写死 `{alias:<28}`，而最长的别名
/// `gpt-image-2.5-sunburst-medium` 是 **30** 字符 ⇒ 那一行只剩一个空格、整列歪掉。
/// 写死宽度与"文档里写死测试条数"是同一类病：写的时候对，加一个更长的名字就歪，
/// **而且不会有任何东西变红**。
fn image_models_text() -> String {
    let models = image_models();
    let width = models
        .iter()
        .map(|(alias, _)| alias.chars().count())
        .max()
        .unwrap_or(0);
    models
        .into_iter()
        .map(|(alias, name)| format!("  {alias:<width$} {name}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 列出可用的**出图**模型别名。
///
/// 为什么要有这个命令：AI 写提示词时需要知道有哪些别名，而唯一权威来源是代码里的
/// `MODEL_SPECS`。让 AI 去读源码既不稳、又容易读漏。
/// ⇒ 给一个命令，**从代码生成**，永远不会像文档那样过期。
///
/// 只列 IMAGE —— 本工具只出图，把 VIDEO / CHAT 也列出来只会误导。
fn print_models(as_json: bool) {
    // 这里显示的是**内置**默认（本命令刻意不读 `.env` / 不碰磁盘）：
    // 生效值 = 命令行 > `.env` 的 LOVART_DEFAULT_MODEL > 这个内置值，
    // 想确认生效值用 `--check`（它会打印真正会用的模型/比例/尺寸）。
    let default_model = PromptDefaults::default().model;

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({ "default": default_model, "models": image_models_json() })
            )
            .unwrap_or_default()
        );
        return;
    }

    println!("================ 可用的出图模型 ================");
    println!("内置默认 : {default_model}（实际默认见 .env 的 LOVART_DEFAULT_MODEL，`--check` 可确认）");
    println!();
    println!("{}", image_models_text());
    println!();
    println!("写进提示词：{{\"prompt\":\"…\",\"model\":\"nano-bn-pro\"}}");
    println!("别名变体（空格 / 无连字符写法，如 \"nano bn pro\"）同样识别。");
    println!("本工具只出图；视频 / 对话模型不在列表内。");
}

/// 打印**状态登记表**（`--status-table`）。
///
/// ── 为什么要有它 ──────────────────────────────────────────────────
/// README 里原先抄了一份状态表 —— 抄的那份**会漂移**（历史上第 4 列就与代码
/// 对不上过）。**文档只放指针**：这张表由代码里那张表直接渲染，不可能不一致。
fn print_status_table() {
    println!("================ 状态表 ================");
    println!("唯一真源：src-tauri/src/engine/state.rs 的 STATUS_TABLE");
    for row in STATUS_TABLE.iter() {
        println!();
        println!("{}（{}）", row.label, row.wire);
        println!("  占槽位   : {}", if row.holds_slot { "✅" } else { "—" });
        println!("  终态     : {}", if row.terminal { "是" } else { "否" });
        println!("  尚未出图 : {}", if row.not_done { "是" } else { "否" });
        println!("  再跑一次 : {}", row.rerun);
        let actions: Vec<&str> = row.actions.iter().map(|action| action.as_str()).collect();
        println!(
            "  界面动作 : {}",
            if actions.is_empty() {
                "—".to_string()
            } else {
                actions.join(" / ")
            }
        );
        if let Some((_, reason)) = row.abnormal {
            println!("  异常说明 : {reason}");
        }
    }
    println!();
    println!("（这张表由代码渲染；加状态 = 在 STATUS_TABLE 加一行）");
}

// ── 主流程 ────────────────────────────────────────────────────────────

/// 解析参数并跑一条命令。
pub fn main_with_args(argv: &[String]) -> Result<(), String> {
    let args: ParsedArgs = parse_args(argv, &known_args());

    // 未知参数**不静默忽略** —— 否则拼错 `--concurreny` 毫无提示，还以为生效了。
    if !args.errors.is_empty() {
        for message in &args.errors {
            log(&format!("❌ {message}"));
        }
        return Err("参数有误，已终止（不猜、不兜底）。可用参数见 --help。".to_string());
    }

    if has_arg(&args, "help") {
        print_help();
        return Ok(());
    }

    // 只列模型：不读文件、不读凭据，纯本地查表（AI 自检时可能还没配 .env）
    if has_arg(&args, "list-models") {
        print_models(has_arg(&args, "json"));
        return Ok(());
    }

    // 状态表：同样不读文件、不读凭据 —— 它**就是代码里那张表**。
    if has_arg(&args, "status-table") {
        print_status_table();
        return Ok(());
    }

    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let prompts_file = cwd.join(arg_string(&args, "prompts", "prompts.jsonl"));
    // ⚠️ 运行产物一律落 `var/`，**不落仓库根** —— 根目录只放源码 / 文档 / 配置。
    let output_dir = cwd.join(arg_string(&args, "output", "output"));
    let state_file = cwd.join(arg_string(&args, "state", "var/state.jsonl"));
    let report_file = if has_arg(&args, "no-report") {
        None
    } else {
        Some(cwd.join(arg_string(&args, "report", "var/report.csv")))
    };

    // 任务看板：不联网、不需要凭据，直接读状态文件渲染后退出
    if has_arg(&args, "status") {
        return print_status(
            &state_file,
            &output_dir,
            has_arg(&args, "full"),
            has_arg(&args, "json"),
        );
    }

    // 配置载入：必须**早于**构造 `defaults` —— 兜底参数（LOVART_DEFAULT_*）住在 `.env` 里。
    // 只读命令（`--help` / `--list-models` / `--status-table` / `--status`）在上面已返回，
    // 因此它们**仍然不需要** `.env`；`--check` / `--dry-run` 会读，因为它们要报告
    // "真正会用的默认值" —— 报告内置值而实际跑 `.env` 值，等于让自检结果说谎。
    load_dot_env(&cwd.join(".env"))?;
    let (env_defaults, default_warnings) = PromptDefaults::from_env();
    for warning in &default_warnings {
        eprintln!("⚠️ {warning}");
    }

    let concurrency = arg_number(&args, "concurrency", 7.0).floor();
    if !(1.0..=HARD_MAX_CONCURRENCY as f64).contains(&concurrency) {
        return Err(format!(
            "--concurrency 必须在 1..{HARD_MAX_CONCURRENCY} 之间（服务端最多同时 {HARD_MAX_CONCURRENCY} 个任务）"
        ));
    }
    let concurrency = concurrency as usize;
    let max_attempts = arg_number(&args, "max-attempts", 3.0).floor().max(1.0) as u32;
    let max_busy_retries = arg_number(&args, "max-busy-retries", 300.0).floor().max(1.0) as u32;
    let max_timeouts = arg_number(&args, "max-timeouts", 3.0).floor().max(1.0) as u32;
    let download_retries = arg_number(&args, "download-retries", 3.0).floor().max(1.0) as u32;
    let task_timeout = Duration::from_secs_f64(
        arg_number(&args, "task-timeout", 90.0).max(1.0) * 60.0,
    );
    let recheck_timeout = Duration::from_secs_f64(
        arg_number(&args, "recheck-timeout", 30.0).max(1.0) * 60.0,
    );
    let task_interval =
        Duration::from_millis(arg_number(&args, "task-interval-ms", 1000.0).max(0.0) as u64);
    let retry_failed = has_arg(&args, "retry-failed");
    // 取值顺序：命令行 > `.env`（LOVART_DEFAULT_*）> 内置默认。与 GUI 设置页同一套兜底。
    let defaults = PromptDefaults {
        model: arg_string(&args, "model", &env_defaults.model),
        aspect_ratio: arg_string(&args, "ratio", &env_defaults.aspect_ratio),
        image_size: arg_string(&args, "size", &env_defaults.image_size),
    };

    // 1. 提示词（dry-run 不需要凭据，所以放在最前面）
    let text = std::fs::read_to_string(&prompts_file)
        .map_err(|_| format!("读不到提示词文件：{}", prompts_file.display()))?;
    let parsed = parse_prompts(
        &text,
        &defaults,
        &ParseOptions {
            allow_unknown_model: has_arg(&args, "allow-unknown-model"),
        },
    );
    for warning in &parsed.warnings {
        log(&format!("⚠️  {warning}"));
    }
    if !parsed.errors.is_empty() {
        for message in &parsed.errors {
            log(&format!("❌ {message}"));
        }
        return Err(format!(
            "提示词文件有 {} 处问题，已终止（不猜、不兜底）。\
             未知模型名可用 --allow-unknown-model 放行；其余请按上面的行号修正。",
            parsed.errors.len()
        ));
    }
    if parsed.tasks.is_empty() {
        return Err(format!("提示词文件里没有有效内容：{}", prompts_file.display()));
    }
    let mut tasks = parsed.tasks;
    log(&format!(
        "解析到 {} 条提示词；默认模型 {}，比例 {}，尺寸 {}",
        tasks.len(),
        defaults.model,
        defaults.aspect_ratio,
        defaults.image_size
    ));

    // AI 自检入口（`--check`）：只要「通过 / 不通过 + 条数」，不要整篇计划。
    // 与 `--dry-run` 共用「解析 → 校验」这一段，**不另写一份解析**。
    if has_arg(&args, "check") {
        println!(
            "✅ {} 条提示词全部合法（默认模型 {} · 比例 {} · 尺寸 {}）",
            tasks.len(),
            defaults.model,
            defaults.aspect_ratio,
            defaults.image_size
        );
        return Ok(());
    }

    if has_arg(&args, "dry-run") {
        let preview_refs = arg_list(&args, "ref");
        println!("\n--- dry-run 计划 ---");
        println!(
            "{}",
            if preview_refs.is_empty() {
                "命令行 --ref: 无".to_string()
            } else {
                format!(
                    "命令行 --ref: {}（整批默认；dry-run 不读盘、不上传）",
                    preview_refs.join(" , ")
                )
            }
        );
        for task in &tasks {
            let own = task.ref_inputs.clone().unwrap_or_default();
            let ref_tag = if !own.is_empty() {
                format!("[ref={}]", own.join(","))
            } else if !preview_refs.is_empty() {
                "[ref←命令行]".to_string()
            } else {
                String::new()
            };
            println!(
                "  第{}行  {}  [model={}][ratio={}][size={}]{}  {}",
                task.line,
                task.id,
                task.model,
                task.aspect_ratio,
                task.image_size,
                ref_tag,
                task.prompt
            );
        }
        println!("--- 未提交任何任务 ---");
        return Ok(());
    }

    // 2. 日志：**从这里开始落盘**。
    //    只读命令（`--check` / `--dry-run` / `--status` / `--list-models`）在上面已返回，
    //    不会往挂机日志里插噪音。
    //    路径固定在 `var/run.log`：与 `--state` 无关 —— 换状态文件不该让日志换个地方找不到。
    let log_file = cwd.join("var/run.log");
    crate::engine::runlog::init(&log_file);
    // 与 GUI **同一句**批次标记：`grep "批次开始" / "批次结束"` 两边通用。
    log(&format!(
        "===== 批次开始：{}（{} 条，并发 {}）=====",
        prompts_file.display(),
        tasks.len(),
        concurrency
    ));
    log(&format!(
        "单任务超时 {} 分，复查超时 {} 分，重试上限 {} 次；日志 {}",
        task_timeout.as_secs() / 60,
        recheck_timeout.as_secs() / 60,
        max_attempts,
        log_file.display()
    ));

    // 3. 凭据（`.env` 已在本函数前半段载入：兜底参数要在解析提示词之前就位）
    // 自动确认：命令行 > `.env` > 默认关。**不读进程环境变量**（见 docs/plan/01 §6）。
    let auto_confirm_override = auto_confirm_flag(&args)?;
    let (file_auto_confirm, auto_confirm_warning) = read_auto_confirm(&cwd.join(".env"));
    if let Some(warning) = auto_confirm_warning {
        eprintln!("⚠️ {warning}");
    }
    let auto_confirm = AutoConfirm::new(Arc::new(AtomicBool::new(
        auto_confirm_override.unwrap_or(file_auto_confirm),
    )));
    let connection = resolve_connection()?;
    log(&format!("接口地址 {}", connection.base_url));
    let deps = to_client_deps(&connection)?;

    // 4. 排队模式：服务端是持久设置，开工前先设一次
    set_mode(&deps, true).map_err(|error| error.to_string())?;
    let mode = query_mode(&deps).ok();
    log(&format!(
        "已切换为排队模式（unlimited）；当前模式回读：{}",
        mode.map(|value| value.to_string()).unwrap_or_else(|| "null".to_string())
    ));

    // 4.5 参考图（图生图）：本地路径读盘 → 转 base64 → 上传 Lovart CDN。
    //     来源两处：命令行 `--ref`（整批默认）与行内 `refs`（**覆盖**命令行，不合并）。
    resolve_refs(&args, &deps, &mut tasks, &cwd)?;

    // 5. 中断处理
    install_sigint_handler();
    let deps_for_watch = deps.clone();
    std::thread::spawn(move || loop {
        if STOP_REQUESTED.load(Ordering::SeqCst) {
            deps_for_watch.cancel();
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    });

    // 6. 开跑
    // 写盘失败**立刻**告警：state 是断点续跑的唯一真相，等到跑完才发现就白跑了几小时。
    let log_for_state = Arc::new(|message: &str| log(message));
    let state = StateStore::open_with(
        &state_file,
        crate::engine::state::StateStoreOptions {
            on_write_error: Some(Box::new(move |message: &str| {
                log_for_state(&format!(
                    "❌ 状态落盘失败：{message}（断点续跑会失效，请检查磁盘空间与写权限）"
                ))
            })),
        },
    )
    .map_err(|error| error.to_string())?;

    let started = std::time::Instant::now();
    let result = run_scheduler(SchedulerOptions {
        deps,
        batch: crate::engine::scheduler::Batch::Declared(tasks.clone()),
        state,
        output_dir: output_dir.clone(),
        concurrency,
        task_timeout: Some(task_timeout),
        recheck_timeout: Some(recheck_timeout),
        max_timeouts: Some(max_timeouts),
        download_retries: Some(download_retries),
        max_attempts: Some(max_attempts),
        max_busy_retries: Some(max_busy_retries),
        task_interval: Some(task_interval),
        rate_limit_backoff_base: None,
        retry_failed,
        busy_backoff_base: None,
        auto_confirm: Arc::clone(&auto_confirm),
        log: Arc::new(|message: &str| log(message)),
        ports: None,
    });
    // 花了钱就要报账：**错误路径也要打印**，所以先读计数再 `?`。
    if let Some(line) = auto_confirm_line(auto_confirm.count()) {
        println!("{line}");
        log(&line);
    }
    let summary = match result {
        Ok(summary) => summary,
        Err(error) => {
            // 批次级失败**必须落盘**：挂机时没人看终端，这句是第二天唯一的线索。
            log(&format!("❌ 批次失败：{}", error.message()));
            return Err(error.to_string());
        }
    };

    print_summary(&summary, started, &output_dir, &state_file);
    // 收尾对账行进日志：挂机第二天先看这一行，就知道「昨晚到底停在哪一类」。
    let counts: Vec<String> = summary
        .counts
        .iter()
        .filter(|(_, count)| **count > 0)
        .map(|(key, count)| format!("{key}={count}"))
        .collect();
    log(&format!(
        "批次结束：共 {} 条（{}），未完成 {} 条",
        summary.total,
        counts.join(" "),
        summary.unfinished
    ));
    if summary.persist_failed {
        log("❌ 收尾时状态文件写不进去：本次进度可能续不上");
    }

    if let Some(report_file) = report_file {
        let tasks_by_id: BTreeMap<String, PromptTask> = tasks
            .iter()
            .map(|task| (task.id.clone(), task.clone()))
            .collect();
        write_report(&report_file, &summary.records, &tasks_by_id, &output_dir)
            .map_err(|error| format!("写报告失败：{error}"))?;
        println!("报告文件 : {}", report_file.display());
    }
    println!("=====================================");
    Ok(())
}

/// 解析参考图并写回每条任务（行内 `refs` **覆盖**命令行 `--ref`，不合并）。
fn resolve_refs(
    args: &ParsedArgs,
    deps: &ClientDeps,
    tasks: &mut [PromptTask],
    cwd: &Path,
) -> Result<(), String> {
    let cli_refs = arg_list(args, "ref");
    let refs_cache_file = if has_arg(args, "no-refs-cache") {
        None
    } else {
        Some(cwd.join(arg_string(args, "refs-cache", "var/refs-cache.json")))
    };
    resolve_refs_core(deps, tasks, cwd, &cli_refs, refs_cache_file.as_deref())
}

/// 参考图解析的**核心**（CLI 与 GUI 共用同一份）。
///
/// ⚠️ 公开是为了让 GUI 复用 —— 抄第二份必然与 CLI 分叉（DEBT TD-11-01 的母体）。
pub fn resolve_refs_core(
    deps: &ClientDeps,
    tasks: &mut [PromptTask],
    cwd: &Path,
    cli_refs: &[String],
    refs_cache_file: Option<&Path>,
) -> Result<(), String> {
    let cli_refs = cli_refs.to_vec();
    let mut needed: Vec<String> = cli_refs.clone();
    for task in tasks.iter() {
        for input in task.ref_inputs.clone().unwrap_or_default() {
            if !needed.iter().any(|existing| existing == &input) {
                needed.push(input);
            }
        }
    }
    if needed.is_empty() {
        return Ok(());
    }

    let entries = read_ref_inputs(&needed, cwd)?;
    let mut cache = load_refs_cache(refs_cache_file);
    let mut url_by_input: BTreeMap<String, String> = BTreeMap::new();
    let mut key_by_input: BTreeMap<String, String> = BTreeMap::new();
    let mut reused = 0usize;

    for entry in &entries {
        key_by_input.insert(entry.input.clone(), entry.key.clone());
        if let Some(cached) = cache.get(&entry.key) {
            url_by_input.insert(entry.input.clone(), cached.clone());
            reused += 1;
            continue;
        }
        let uploaded = resolve_attachments(deps, std::slice::from_ref(&entry.url))
            .map_err(|error| error.message().to_string())?;
        let Some(url) = uploaded.and_then(|urls| urls.into_iter().next()) else {
            return Err(format!("参考图上传失败（未返回 CDN URL）：{}", entry.key));
        };
        cache.insert(entry.key.clone(), url.clone());
        url_by_input.insert(entry.input.clone(), url);
    }
    save_refs_cache(refs_cache_file, &cache);

    let mut image_to_image = 0usize;
    for task in tasks.iter_mut() {
        let own = task.ref_inputs.clone().unwrap_or_default();
        let inputs = if own.is_empty() { cli_refs.clone() } else { own };
        if inputs.is_empty() {
            continue;
        }
        let urls: Vec<String> = inputs
            .iter()
            .filter_map(|input| url_by_input.get(input).cloned())
            .collect();
        if urls.is_empty() {
            continue;
        }
        task.refs = Some(urls);
        task.ref_signature = Some(refs_signature(
            &inputs
                .iter()
                .map(|input| key_by_input.get(input).cloned().unwrap_or_else(|| input.clone()))
                .collect::<Vec<_>>(),
        ));
        image_to_image += 1;
    }

    log(&format!(
        "参考图：{} 个素材（缓存命中 {reused}，新上传 {}）；图生图任务 {image_to_image}/{} 条",
        entries.len(),
        entries.len() - reused,
        tasks.len()
    ));
    Ok(())
}

/// 终端对齐：把短名补到 9 个**显示列**（中日韩字符占 2 列）。
///
/// 为什么不用 `{:<9}`：那个按**字符数**补 —— `已完成` 是 3 个字符却占 6 列，
/// 用字符数补出来的表会歪。
fn pad_label(label: &str) -> String {
    let width: usize = label
        .chars()
        .map(|ch| if ch.is_ascii() { 1 } else { 2 })
        .sum();
    format!("{label}{}", " ".repeat(9usize.saturating_sub(width)))
}

/// 从汇总里取某个状态的计数。键名走 [`crate::engine::state::json_key`] ——
/// 与 `--status --json` 的 summary **同一套键**（一份口径，两处投影）。
///
/// ⚠️ 取不到就**响**，**不兜底成 0**：`counts` 是按同一张表生成的，键必然在。
/// 取不到只可能是"表加了状态、汇总没跟上"这种内部 bug —— 兜成 0 会把 bug
/// 画成「这个状态没有任务」，正是本仓禁的那种"失败伪装成正常"。
fn count_of(summary: &SchedulerSummary, status: TaskStatus) -> usize {
    let key = crate::engine::state::json_key(status);
    *summary
        .counts
        .get(&key)
        .unwrap_or_else(|| panic!("汇总里没有状态 {key} 的计数（加状态时没同步 counts）"))
}

fn print_summary(
    summary: &SchedulerSummary,
    started: std::time::Instant,
    output_dir: &Path,
    state_file: &Path,
) {
    let minutes = started.elapsed().as_secs_f64() / 60.0;
    println!("\n================ 汇总 ================");
    println!("任务总数 : {}", summary.total);
    // 逐状态行**由表生成**（短名 = `label`、括号里那句 = `rerun`）。
    //
    // ⚠️ 原先这里手抄 7 行，而且 `download_failed` 那行写的短名是「下载失败」，
    //    与表里的 `label`（待补下载）**不是同一个名字** —— 同一状态两个名字，
    //    正是这次要收掉的那种重复。括号里那句同理（原先各写各的）。
    let mut rows: Vec<_> = STATUS_TABLE
        .iter()
        .filter(|row| !row.not_done || row.abnormal.is_some())
        .collect();
    rows.sort_by_key(|row| row.not_done);
    for row in rows {
        println!(
            "{}: {}（{}）",
            pad_label(row.label),
            count_of(summary, row.status),
            row.rerun
        );
    }
    println!(
        "{}: {}（再跑一次会自动续跑；与上面那些「尚未出图」是两个口径）",
        pad_label("处理中"),
        summary.unfinished
    );
    if summary.persist_failed {
        println!("⚠️  状态文件落盘失败：本次进度可能续不上，请检查磁盘空间与写权限");
    }
    println!("耗时     : {minutes:.1} 分钟");
    println!("图片目录 : {}", output_dir.display());
    println!("状态文件 : {}", state_file.display());
    print_pending_list(&summary.records);
}

fn print_help() {
    let readme = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("README.md");
    match std::fs::read_to_string(&readme) {
        Ok(text) => println!("{text}"),
        Err(_) => println!("见仓库根 README.md"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::parse_args;

    fn parsed(argv: &[&str]) -> ParsedArgs {
        let known: BTreeSet<String> = known_args();
        let argv: Vec<String> = argv.iter().map(|value| value.to_string()).collect();
        parse_args(&argv, &known)
    }

    #[test]
    fn c1_两个参数同时出现_报错() {
        let error = auto_confirm_flag(&parsed(&["--auto-confirm", "--no-auto-confirm"]))
            .expect_err("必须报错，不猜");
        assert!(error.contains("不能同时"), "{error}");
    }

    #[test]
    fn c2_参数取值() {
        assert_eq!(
            auto_confirm_flag(&parsed(&["--auto-confirm"])).unwrap(),
            Some(true)
        );
        assert_eq!(
            auto_confirm_flag(&parsed(&["--no-auto-confirm"])).unwrap(),
            Some(false)
        );
        assert_eq!(auto_confirm_flag(&parsed(&[])).unwrap(), None);
    }

    #[test]
    fn c6_对账行() {
        assert_eq!(auto_confirm_line(0), None, "0 条不打印，不制造噪音");
        assert_eq!(auto_confirm_line(3).unwrap(), "本批自动确认 3 条");
    }
}
