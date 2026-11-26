//! `run` — 命令行入口（对应 TS `src/run.ts`）。
//!
//! ```text
//! lovart-client [选项]
//!
//!   --prompts <file>         提示词文件，JSONL（默认 data/prompts.jsonl）
//!   --output <dir>           图片输出目录（默认 data/output）
//!   --state <file>           断点续跑状态（默认 data/var/state.jsonl）
//!   --report <file>          结果报告 CSV（默认 data/var/report.csv）
//!   --no-report              不写报告文件
//!   --ref <路径|URL|data:>   参考图（图生图），可重复或用逗号分隔
//!   --refs-cache <file>      参考图 CDN 缓存（默认 data/var/refs-cache.json）
//!   --no-refs-cache          不读写参考图缓存
//!   --model <alias>          默认模型（默认 nano-bn-2）
//!   --ratio <w:h>            默认比例（默认 3:4）
//!   --size <1K|2K|4K>        默认尺寸（默认 2K）
//!   --retry-failed           重置无 threadId 的 Failed 并重跑
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
//!
//! ⚠️ **运行参数（并发 / 提交间隔 / 重试 / 超时）没有命令行开关**（2026-11-18 定）：
//! 它们是「机器怎么跑」，只能由用户在界面「设置」里改（落到 `.env`）。
//! 命令行只决定「画什么」。传这些参数会**报错退出**并指出该去哪里改 —— 见 `cli.rs`
//! 的 `RETIRED_TUNING_KEYS`。理由：CLI 是 AI 的入口，AI 不该能调这些旋钮。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::cli::{arg_list, arg_string, has_arg, parse_args, ParsedArgs};
use crate::engine::group::task_group;
use crate::engine::scheduler::{
    run_scheduler, AutoConfirm, SchedulerOptions, SchedulerSummary, SchedulerTuning,
};
use crate::engine::settings::Settings;
use crate::engine::state::{
    is_not_done, StateStore, TaskRecord, TaskStatus, STATUS_TABLE,
};
use crate::service::meta::{image_models_json, image_models_text};
use crate::service::refs::resolve_refs_core;
use crate::service::workbench::{data_dir, migrate_layout};
use crate::service::snapshot::{
    board_order, build_status_json, elapsed_text, files_present, format_clock, status_counts,
    status_label,
};
use crate::service::wiring::to_client_deps;
use crate::lovart::client::{query_mode, set_mode, Mode};
use crate::lovart::connection::{load_dot_env, read_auto_confirm, resolve_connection};
use crate::lovart::prompt::{parse_prompts, ParseOptions, PromptDefaults, PromptTask};

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
        "model",
        "ratio",
        "size",
        "retry-failed",
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

// ── 报告 / 分组已下沉到 `engine` ──────────────────────────────────────
// 原先定义在本文件，而 `engine::scheduler` 反向调用它们（`crate::run::*`）；
// 拆 `lovart-core` 后那个方向编译不过（E0433），实现已搬进 `engine`。
// ⚠️ 不做 `pub use` 转发：同名别名会让同一函数出现两个路径（ADR-0005 / 0006）。

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


/// 任务看板：不联网、不需要凭据，只读 state 文件渲染。
///
/// 回答三件事：① 现在哪些任务在跑；② 每条在当前状态待了多久；③ 每张图对应哪条提示词。
fn print_status(state_file: &Path, output_dir: &Path, full: bool, as_json: bool) -> Result<(), String> {
    // ⚠️ JSON 分支**必须在任何读盘动作之前**返回。
    //    原先这里先读 store / `all()` / `board_order` / 算产物存在性，
    //    然后 `build_status_json` 又从头读一遍 —— 同一份数据读两次、排两次、stat 两次。
    if as_json {
        let payload = build_status_json(state_file, output_dir)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
        return Ok(());
    }

    // 只读打开：不压实、不建目录 —— 生成进程可能正在追加写同一个文件。
    let state = StateStore::open_read_only(state_file).map_err(|error| error.to_string())?;
    let mut records: Vec<TaskRecord> = state.all().into_iter().cloned().collect();
    // ⚠️ 顺序**只此一份**（与窗口共用），别再在这里写一遍 `sort_by_key`。
    board_order(&mut records);

    // 核对产物是否真的在盘上（提示词与图对不上时，这里会立刻暴露）
    let present = files_present(&records, output_dir);
    let counts = status_counts(&records);

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
        let count = counts
            .iter()
            .find(|(status, _)| *status == row.status)
            .map(|(_, count)| *count)
            .unwrap_or(0);
        println!("{}: {}（{}）", pad_label(row.label), count, row.rerun);
    }

    let render = |record: &TaskRecord| {
        let line = record
            .line
            .map(|value| value.to_string())
            .unwrap_or_else(|| "?".to_string());
        // 行尾时间只有两种口径：
        // · 生产中的任务 ⇒ `[已持续 X]`（当前这一段状态跑了多久）；
        // · 已结束 ⇒ `结束 MM-DD HH:MM`（绝对时刻，静态）。
        // 排队 / 待确认 ⇒ **什么都不打**（没开始生产，没有"持续"可言）。
        // ⚠️ 不再打 `· 已提交 X`：它与"已持续"是同一件事（过了多久）的两份投影。
        let tail = match task_group(record.status) {
            "active" => elapsed_text(record.status, record.state_since.as_deref())
                .map(|text| format!("  [已持续 {text}]"))
                .unwrap_or_default(),
            "finished" => format!("  结束 {}", format_clock(record.state_since.as_deref())),
            _ => String::new(),
        };
        println!(
            "  第{}行  {}  {}  {} {}  {}{}",
            line,
            record.id,
            record.model,
            record.aspect_ratio,
            record.image_size,
            status_label(record.status),
            tail
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

    // 四段分组与界面**同一判据**（`task_group`）。原先用 `is_unfinished` 二分，
    // `needs_confirm`（终态）会被归进「已结束」—— 与窗口把它放在「待你处理」矛盾。
    let mut groups: [(&str, Vec<&TaskRecord>); 4] = [
        ("处理中", Vec::new()),
        ("排队", Vec::new()),
        ("待你处理", Vec::new()),
        ("已结束", Vec::new()),
    ];
    for record in &records {
        let index = match task_group(record.status) {
            "active" => 0,
            "queued" => 1,
            "waiting" => 2,
            _ => 3,
        };
        groups[index].1.push(record);
    }
    for (title, group) in groups {
        if group.is_empty() {
            continue;
        }
        println!("\n── {title}（{}）────────────────────────────", group.len());
        for record in group {
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
    // ⚠️ 这里**不能**写「空格写法同样识别」。别名是逐条枚举的：`nano bn pro` 在表里、
    //    `GPT Image 2 Low` 不在 ⇒ 通则不成立（TD-01-3，2026-11-20 用 `--check` 实测）。
    //    要支持更多写法，正确改法是让 `resolve_prefer_models` 归一化空白（1 处），
    //    不是往别名表里逐条补 —— 补不满，还会给子串匹配添新的误吞面。
    println!("别名匹配是子串包含：nano-bn-pro / nano bn pro / nanobnpro 都命中。");
    println!("以上面的别名为准；写不准会报错并终止（不会静默退回默认模型）。");
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
    // ⚠️ CLI 不构造 `Workbench`，但**必须和 GUI 一样先做布局迁移**（plan 19 §3.2）：
    // 否则 GUI 已把数据搬进 `data/` 后，CLI 还去根目录找 `var/state.jsonl`。
    // 迁移是幂等的、一次性改名，不联网、不读凭据；只读命令也会跑这一步（只改名，无业务副作用）。
    for note in migrate_layout(&cwd) {
        eprintln!("ℹ️ {note}");
    }
    // 相对路径仍按**工作区**解析（显式传旧路径 `var/state.jsonl` 照样有效）；
    // 只是默认值换到 `data/` 下（plan 19 §3.3）。
    let prompts_file = cwd.join(arg_string(&args, "prompts", "data/prompts.jsonl"));
    let output_dir = cwd.join(arg_string(&args, "output", "data/output"));
    let state_file = cwd.join(arg_string(&args, "state", "data/var/state.jsonl"));
    let report_file = if has_arg(&args, "no-report") {
        None
    } else {
        Some(cwd.join(arg_string(&args, "report", "data/var/report.csv")))
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
    load_dot_env(&data_dir(&cwd).join(".env"))?;
    // **唯一读取点**：数值设置与兜底参数一次读完（`.env` 已在本函数前半段载入）。
    let (current, settings_warnings) = Settings::from_env();
    for warning in &settings_warnings {
        eprintln!("⚠️ {warning}");
    }

    // 运行参数**只来自设置**（`.env`，由界面「设置」写入）：CLI 不提供覆盖入口。
    // 这不是删能力 —— 用户在设置页照旧能改；关掉的是 AI 唯一够得着的那个面。
    let tuning = SchedulerTuning::from_settings(&current);
    let retry_failed = has_arg(&args, "retry-failed");
    // 取值顺序：命令行 > `.env`（LOVART_DEFAULT_*）> 内置默认。与 GUI 设置页同一套兜底。
    let defaults = PromptDefaults {
        model: arg_string(&args, "model", &current.default_model),
        aspect_ratio: arg_string(&args, "ratio", &current.default_ratio),
        image_size: arg_string(&args, "size", &current.default_size),
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

    // 2. 单实例写锁：`state.jsonl` 只有一个写者（理由见 `engine::lockfile` 文件头）。
    //    ⚠️ 必须在所有只读命令之后 —— `--status` / `--check` / `--dry-run` 上面已返回，
    //    它们不写状态文件；给它们加锁会让「GUI 跑批时查状态」变成报错。
    //    锁固定 `data/var/.lock`，不跟随 `--state`：不同状态文件仍共享 `data/var/report.csv`
    //    与 `data/var/refs-cache.json`，拆开锁等于允许它们互相覆盖。
    let _lock = crate::engine::lockfile::FileLock::acquire(&data_dir(&cwd).join("var/.lock"))?;

    // 3. 日志：**从这里开始落盘**。
    //    只读命令（`--check` / `--dry-run` / `--status` / `--list-models`）在上面已返回，
    //    不会往挂机日志里插噪音。
    //    路径固定在 `data/var/run.log`：与 `--state` 无关 —— 换状态文件不该让日志换个地方找不到。
    let log_file = data_dir(&cwd).join("var/run.log");
    crate::engine::runlog::init(&log_file);
    // 与 GUI **同一句**批次标记：`grep "批次开始" / "批次结束"` 两边通用。
    log(&format!(
        "===== 批次开始：{}（{} 条，并发 {}）=====",
        prompts_file.display(),
        tasks.len(),
        tuning.concurrency
    ));
    log(&format!(
        "单任务超时 {} 分，复查超时 {} 分，重试上限 {} 次；日志 {}",
        // 保留一位小数：`--task-timeout 90.5` 不能被显示成 90。
        tuning.task_timeout.as_secs_f64() / 60.0,
        tuning.recheck_timeout.as_secs_f64() / 60.0,
        tuning.max_attempts,
        log_file.display()
    ));

    // 4. 凭据（`.env` 已在本函数前半段载入：兜底参数要在解析提示词之前就位）
    // 自动确认：命令行 > `.env` > 默认关。**不读进程环境变量**（见 docs/plan/01 §6）。
    let auto_confirm_override = auto_confirm_flag(&args)?;
    let (file_auto_confirm, auto_confirm_warning) = read_auto_confirm(&data_dir(&cwd).join(".env"));
    if let Some(warning) = auto_confirm_warning {
        eprintln!("⚠️ {warning}");
    }
    let auto_confirm = AutoConfirm::new(Arc::new(AtomicBool::new(
        auto_confirm_override.unwrap_or(file_auto_confirm),
    )));
    let connection = resolve_connection()?;
    log(&format!("接口地址 {}", connection.base_url));
    let deps = to_client_deps(&connection)?;

    // 4. 排队模式：服务端是**账号级**持久设置，开工前先设一次并回读。
    //    这里只是**早失败**（凭据 / 网络不通就别开跑，省得跑一半才发现）；
    //    调度器在**每条提交前**还会再设一次（见 `scheduler::Shared::ensure_mode`），
    //    因为跑批可能持续十几个小时，中途随时会被别的设备改回 fast（扣积分）。
    set_mode(&deps, Mode::Queue).map_err(|error| error.to_string())?;
    let mode = query_mode(&deps).ok();
    log(&format!(
        "已切换为排队模式（unlimited）；当前模式回读：{}",
        mode.map(|value| value.to_string()).unwrap_or_else(|| "null".to_string())
    ));

    // 4.5 参考图（图生图）：本地路径读盘 → 转 base64 → 上传 Lovart CDN。
    //     来源两处：命令行 `--ref`（整批默认）与行内 `refs`（**覆盖**命令行，不合并）。
    resolve_refs(&args, &mut tasks, &cwd)?;

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
        tuning,
        retry_failed,
        auto_confirm: Arc::clone(&auto_confirm),
        log: Arc::new(|message: &str| log(message)),
        ports: None,
        // 报告由调度器在**会话封口**时写（与 GUI 同一条路径）；这里只保留打印。
        report_file: report_file.clone(),
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

    if let Some(report_file) = &report_file {
        // 文件已由调度器在会话封口时写好（与 GUI 同一条路径），这里只报路径。
        println!("报告文件 : {}", report_file.display());
    }
    println!("=====================================");
    Ok(())
}

/// 解析参考图并写回每条任务（行内 `refs` **覆盖**命令行 `--ref`，不合并）。
fn resolve_refs(args: &ParsedArgs, tasks: &mut [PromptTask], cwd: &Path) -> Result<(), String> {
    let cli_refs = arg_list(args, "ref");
    resolve_refs_core(tasks, cwd, &cli_refs, &|message| log(message))
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

    /// 运行参数没有命令行入口：**能力还在**（设置页 / `.env`），只是 AI 够不着。
    /// 报错必须指向「设置」，否则用户会以为是自己拼错了。
    #[test]
    fn c7_运行参数不接受命令行覆盖() {
        for key in crate::cli::RETIRED_TUNING_KEYS {
            let flag = format!("--{key}");
            let args = parsed(&[flag.as_str(), "3"]);
            assert_eq!(args.errors.len(), 1, "--{key} 必须被拒绝：{:?}", args.errors);
            assert!(
                args.errors[0].contains("设置"),
                "--{key} 的报错要指出该去哪里改：{:?}",
                args.errors
            );
        }
    }

    /// 反向约束：参数名表里**不能**再出现这些键，否则上面的拒绝会被 `known_keys` 短路。
    #[test]
    fn c8_运行参数不在已知参数表里() {
        let known = known_args();
        for key in crate::cli::RETIRED_TUNING_KEYS {
            assert!(!known.contains(*key), "{key} 不该出现在 known_args()");
        }
    }
}
