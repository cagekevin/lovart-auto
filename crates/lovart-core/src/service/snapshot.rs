//! 看板投影 —— **唯一**把 `state.jsonl` 变成界面的地方。
//!
//! GUI 的 `get_state`、CLI 的 `--status --json`（以及下一步的 HTTP `/state`）都只能走这里；
//! 原先这套代码分在 `run.rs`（`build_status_json`）与 `gui.rs`（`view_input` / `finish_payload`），
//! 搬进 `service` 是为了让"另一个前端"也能拿到**同一份**看板。
//!
//! 依赖方向：`service → engine`（这里只读 `state` 与分组判据，不写状态）。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::engine::group::task_group;
use crate::engine::report::resolve_file;
use crate::engine::state::{
    is_not_done, row as status_row, Action, StateStore, TaskRecord, TaskStatus,
};
use crate::service::workbench::Workbench;

/// `get_state` 的指纹：**文件侧**（长度 + 修改时间）+ **内存侧**（看板 rev / 清空 /
/// 自动确认 / 花积分）+ **输出目录**。
///
/// 文件侧用 `len + mtime` 而不是内容哈希：追加和压实都会改这两个值，
/// 而一次 `metadata()` 是微秒级（对照：2263 条全量重算是 200ms 级）。
///
/// ⚠️ 输出目录**必须进指纹**：设置里改了输出目录时 `state.jsonl` 一个字都不会变，
/// 但 `filesPresent`（绝对路径 + 在不在盘上）与 `missingArtifact` 全都变了。
/// 漏了它 ⇒ 改完设置，界面继续指着旧目录说"图在 / 图缺"。
pub fn view_key(
    state_file: &Path,
    output_dir: &Path,
    board_rev: u64,
    clearing: bool,
    auto_confirm: bool,
    auto_confirmed: usize,
    spend_mode: bool,
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
        "{len}:{modified}:{board_rev}:{auto_confirm}:{auto_confirmed}:{spend_mode}:{}:{}",
        if clearing { 'c' } else { '-' },
        output_dir.display()
    )
}

/// 给人看的状态短名。**读表**（`label` 列）—— 原先这里是一份穷尽 `match`。
pub fn status_label(status: TaskStatus) -> &'static str {
    status_row(status).label
}

/// 把「距今多久」格式化成 `45秒` / `12分30秒` / `1时05分`。
///
/// 拿不到时间（旧记录 / 非法值）⇒ `—`，**不猜**。
pub fn format_elapsed(from: Option<&str>) -> String {
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

/// 「**正在生产**已持续多久」—— 只有 `active` 组才有值。
///
/// 旧记录没有 `stateSince` ⇒ `format_elapsed` 返回 `"—"` ⇒ 这里转成 `None`（不显示），
/// 而不是让界面显示「已持续 —」。
pub fn elapsed_text(status: TaskStatus, state_since: Option<&str>) -> Option<String> {
    if task_group(status) != "active" {
        return None;
    }
    active_elapsed_text(state_since)
}

/// 同一条文案的**后半段**：已知属于 `active` 时，只按 `state_since` 出字。
///
/// 存在的唯一理由是缓存命中路径 —— 那时手里只有上一轮的 `state_since`
/// （`state.jsonl` 没变 ⇒ 状态也不可能变），不必为了重算这几个字再读一遍全部记录。
pub fn active_elapsed_text(state_since: Option<&str>) -> Option<String> {
    let text = format_elapsed(state_since);
    (text != "—").then_some(text)
}

/// 绝对时刻 `MM-DD HH:MM`（与前端 `formatTime` 同格式）；解析不了 ⇒ `未知`。
pub fn format_clock(value: Option<&str>) -> String {
    let Some(value) = value else {
        return "未知".to_string();
    };
    match chrono::DateTime::parse_from_rfc3339(value) {
        Ok(parsed) => parsed
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string(),
        Err(_) => "未知".to_string(),
    }
}

/// 异常状态 → 界面要用的两件套：`(色调, 一句人话)`。**读表**（`abnormal` 列）。
pub fn abnormal_info(status: TaskStatus) -> Option<(&'static str, &'static str)> {
    status_row(status).abnormal
}

/// 这条异常**重跑有没有意义**。**读表**（`actions` 列里有没有 `Retry`）。
pub fn is_retryable(status: TaskStatus) -> bool {
    status_row(status).actions.contains(&Action::Retry)
}

/// 板面顺序 —— **终端看板与窗口共用这一份**。
///
/// 先按 [`task_group`] 分档：`active` → `queued` → `waiting` → `finished`；
/// · `active` / `queued`：按提示词行号 `line` 升序；
/// · `waiting` / `finished`：按进入该状态的时刻（`stateSince`）倒序。
pub fn board_order(records: &mut [TaskRecord]) {
    records.sort_by(|a, b| {
        let (rank_a, rank_b) = (group_rank(a.status), group_rank(b.status));
        if rank_a != rank_b {
            return rank_a.cmp(&rank_b);
        }
        match rank_a {
            0 | 1 => a.line.cmp(&b.line).then_with(|| a.id.cmp(&b.id)),
            _ => b
                .state_since
                .cmp(&a.state_since)
                .then_with(|| a.id.cmp(&b.id)),
        }
    });
}

/// 板面分档：`active` → `queued` → `waiting` → `finished`。
fn group_rank(status: TaskStatus) -> u8 {
    match task_group(status) {
        "active" => 0,
        "queued" => 1,
        "waiting" => 2,
        _ => 3,
    }
}

/// 产物存在性：`文件名（记录里的原文）→ 是否真在盘上`。
pub fn files_present(records: &[TaskRecord], output_dir: &Path) -> BTreeMap<String, bool> {
    let mut present: BTreeMap<String, bool> = BTreeMap::new();
    for record in records {
        for file in record.files.iter().flatten() {
            let resolved = resolve_file(output_dir, file);
            present
                .entry(file.clone())
                .or_insert_with(|| resolved.is_file());
        }
    }
    present
}

/// 逐状态计数（顺序 = `STATUS_TABLE`）。
pub fn status_counts(records: &[TaskRecord]) -> Vec<(TaskStatus, usize)> {
    crate::engine::state::all()
        .map(|status| {
            let count = records
                .iter()
                .filter(|record| record.status == status)
                .count();
            (status, count)
        })
        .collect()
}

/// 看板的**机器可读**形态（`--status --json` / `get_state` / HTTP `/state` 共用）。
pub fn build_status_json(state_file: &Path, output_dir: &Path) -> Result<Value, String> {
    let state = StateStore::open_read_only(state_file).map_err(|error| error.to_string())?;
    let mut records: Vec<TaskRecord> = state.all().into_iter().cloned().collect();
    board_order(&mut records);

    let present = files_present(&records, output_dir);
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
                    match elapsed_text(record.status, record.state_since.as_deref()) {
                        Some(text) => Value::String(text),
                        None => Value::Null,
                    },
                );
                object.insert(
                    "group".to_string(),
                    Value::String(task_group(record.status).to_string()),
                );
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
                object.insert(
                    "missingArtifact".to_string(),
                    Value::Bool(missing_artifact),
                );
                object.insert(
                    "abnormal".to_string(),
                    match abnormal_info(record.status) {
                        Some((kind, reason)) => json!({ "kind": kind, "reason": reason }),
                        None => Value::Null,
                    },
                );
            }
            value
        })
        .collect();

    let mut summary = serde_json::Map::new();
    summary.insert("total".to_string(), json!(records.len()));
    for (status, count) in status_counts(&records) {
        summary.insert(crate::engine::state::json_key(status), json!(count));
    }

    Ok(json!({
        "stateFile": state_file.display().to_string(),
        "outputDir": output_dir.display().to_string(),
        "summary": summary,
        "tasks": tasks,
    }))
}

/// 一次 `get_state` 需要的**内存侧面**（与磁盘无关）。
pub struct ViewInput {
    pub state_file: PathBuf,
    pub output_dir: PathBuf,
    pub batch_label: Option<String>,
    pub batch_ids: BTreeSet<String>,
    pub auto_confirmed: usize,
    pub clearing: bool,
    pub auto_confirm: bool,
    pub spend_mode: bool,
    pub board_rev: u64,
}

impl ViewInput {
    pub fn key(&self) -> String {
        view_key(
            &self.state_file,
            &self.output_dir,
            self.board_rev,
            self.clearing,
            self.auto_confirm,
            self.auto_confirmed,
            self.spend_mode,
        )
    }
}

/// 取内存侧快照：只碰锁与原子量，**不读盘**。
pub fn view_input(state: &Workbench) -> ViewInput {
    let state_file = state.state_file();
    let output_dir = state.output_dir();
    let auto_confirm = state.auto_confirm_on();
    let spend_mode = state.spend_mode_on();

    // 判据只有一处：**调度器**说的是不是真话。先克隆再放槽锁，避免与 ctx 锁长嵌套。
    let scheduler = state.scheduler_snapshot();
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
        spend_mode,
        // 指纹只要「起停」这一位：`held_ids` 每秒都在变，进指纹会让缓存永远失效。
        board_rev: u64::from(running),
    }
}

/// 命中时 GUI 用的载荷：`{ unchanged: true, elapsed: [{id, text}] }`。
///
/// 只重算「已持续」那几条（全量载荷里唯一每秒都变的东西）；判据仍然是
/// Rust 下发的 `stateSinceText`，不另写状态清单。
pub fn unchanged_payload(active: &[(String, Option<String>)]) -> Value {
    let elapsed: Vec<Value> = active
        .iter()
        .filter_map(|(id, since)| {
            active_elapsed_text(since.as_deref()).map(|text| json!({ "id": id, "text": text }))
        })
        .collect();
    json!({ "unchanged": true, "elapsed": elapsed })
}

/// 全量载荷的收尾：并入内存侧字段、逐条 `following`，并返回 active 列表
/// （GUI 拿它做下一次命中的「已持续」重算）。
pub fn finish_payload(
    input: &ViewInput,
    mut payload: Value,
) -> (Value, Vec<(String, Option<String>)>) {
    let mut elapsed: Vec<(String, Option<String>)> = Vec::new();
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
        object.insert("spendMode".to_string(), Value::Bool(input.spend_mode));

        if let Some(tasks) = object.get_mut("tasks").and_then(Value::as_array_mut) {
            for task in tasks {
                let Some(id) = task.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let following = input.batch_ids.contains(id);
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

    }
    (payload, elapsed)
}

/// 看板快照：唯一出口的返回形状。
pub struct Snapshot {
    pub key: String,
    /// `None` = `since` 命中（未变化）。
    pub payload: Option<Value>,
    /// 命中时 GUI 用它重算「已持续」；未命中时是本次全量的 active 列表。
    pub active: Vec<(String, Option<String>)>,
}

impl Workbench {
    /// 看板唯一出口。
    ///
    /// - `since = Some(上次的 key)` ⇒ 命中时 `payload = None`，不读盘；
    /// - `since = None` ⇒ 永远全量（HTTP 用这个）。
    pub fn snapshot(&self, since: Option<&str>) -> Result<Snapshot, String> {
        let input = view_input(self);
        let key = input.key();
        if since == Some(key.as_str()) {
            return Ok(Snapshot {
                key,
                payload: None,
                active: Vec::new(),
            });
        }
        let built = build_status_json(&input.state_file, &input.output_dir)?;
        let (payload, active) = finish_payload(&input, built);
        Ok(Snapshot {
            key,
            payload: Some(payload),
            active,
        })
    }
}
