//! 看板展示字段契约 —— 「只有正在生产的任务才下发已持续」。
//!
//! 全部走真实出口 `build_status_json`（GUI 的 `get_state` 与 `--status --json` 共用），
//! 不直接调内部派生函数 —— 直接调只能证明"函数是对的"，证明不了"真的在用"。

#![allow(non_snake_case)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use lovart_client::run::build_status_json;
use serde_json::{json, Value};

static SEQ: AtomicUsize = AtomicUsize::new(0);

struct TempState {
    dir: PathBuf,
    path: PathBuf,
}

impl TempState {
    fn new(lines: &[String]) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "lv-board-contract-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).expect("建临时目录");
        let path = dir.join("state.jsonl");
        fs::write(&path, lines.join("\n")).expect("写 state");
        Self { dir, path }
    }

    fn payload(&self) -> Value {
        build_status_json(&self.path, Path::new("/tmp")).expect("构造看板 JSON")
    }
}

impl Drop for TempState {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn record(id: &str, status: &str, line: u32, state_since: Option<&str>) -> String {
    let since = match state_since {
        Some(value) => format!(r#""stateSince":"{value}","#),
        None => String::new(),
    };
    format!(
        r#"{{"id":"{id}","prompt":"P-{id}","line":{line},"model":"nano-bn-2","aspectRatio":"3:4","imageSize":"2K","status":"{status}","attempts":1,"timeoutCount":0,"busyRetries":0,"readyAt":0,{since}"updatedAt":"2026-10-01T00:00:00.000Z"}}"#
    )
}

fn task<'a>(tasks: &'a [Value], id: &str) -> &'a Value {
    tasks
        .iter()
        .find(|task| task["id"] == id)
        .unwrap_or_else(|| panic!("找不到任务 {id}"))
}

/// 取字段并**要求键真的存在**。
///
/// ⚠️ 不能用 `task["stateSinceText"]` 直接断言 `== Null`：字段整个缺失时
/// `Value` 的索引也返回 `Null`，测试会**假绿**（删字段都测不出来）。
fn field<'a>(task: &'a Value, key: &str) -> &'a Value {
    task.get(key)
        .unwrap_or_else(|| panic!("任务缺字段 {key}（契约要求显式下发 null）"))
}

#[test]
fn 只有_active_才下发已持续() {
    let state = TempState::new(&[
        record("queued", "pending", 1, Some("2026-10-01T00:00:00.000Z")),
        record("active", "running", 2, Some("2026-10-01T00:00:00.000Z")),
        record("waiting", "needs_confirm", 3, Some("2026-10-01T00:00:00.000Z")),
        record("finished", "done", 4, Some("2026-10-01T00:00:00.000Z")),
    ]);
    let payload = state.payload();
    let tasks = payload["tasks"].as_array().expect("tasks 必须是数组");

    assert!(
        field(task(tasks, "queued"), "stateSinceText").is_null(),
        "排队中（还没开始生产）不该有「已持续」"
    );
    assert!(
        field(task(tasks, "waiting"), "stateSinceText").is_null(),
        "待确认（停住等人）不该有「已持续」"
    );
    assert!(
        field(task(tasks, "finished"), "stateSinceText").is_null(),
        "终态不该有「已持续」，改看 stateSince 绝对时刻"
    );

    let text = field(task(tasks, "active"), "stateSinceText")
        .as_str()
        .expect("active 必须下发字符串");
    assert_ne!(text, "—", "时间可解析时不该退化成占位符");
}

#[test]
fn 旧记录缺_state_since_时不显示已持续() {
    let state = TempState::new(&[
        record("old-running", "running", 1, None),
        record("old-pending", "pending", 2, None),
    ]);
    let payload = state.payload();
    let tasks = payload["tasks"].as_array().expect("tasks 必须是数组");

    assert!(
        field(task(tasks, "old-running"), "stateSinceText").is_null(),
        "旧记录没有状态起点 ⇒ 不显示，而不是「已持续 —」"
    );
    assert!(field(task(tasks, "old-pending"), "stateSinceText").is_null());
}

#[test]
fn payload_不含_generatedAt() {
    let state = TempState::new(&[record("a", "pending", 1, Some("2026-10-01T00:00:00.000Z"))]);
    let payload = state.payload();
    assert!(
        payload.get("generatedAt").is_none(),
        "generatedAt 全仓零消费，已删；重新加回来会让 payload 多一份没人读的时间"
    );
}

#[test]
fn summary_覆盖全部状态且计数一致() {
    // 顺序 = STATUS_TABLE 的生命周期顺序；键名 = 线格式名的 camelCase。
    let statuses = [
        "pending",
        "submitting",
        "running",
        "timeout",
        "unresolved",
        "download_failed",
        "done",
        "no_artifact",
        "needs_confirm",
        "failed",
    ];
    let lines: Vec<String> = statuses
        .iter()
        .enumerate()
        .map(|(index, status)| {
            record(
                &format!("t{index}"),
                status,
                index as u32 + 1,
                Some("2026-10-01T00:00:00.000Z"),
            )
        })
        .collect();
    let state = TempState::new(&lines);
    let payload = state.payload();
    let summary = payload["summary"].as_object().expect("summary 必须是对象");

    assert_eq!(summary["total"], json!(10));
    for key in [
        "pending",
        "submitting",
        "running",
        "timeout",
        "unresolved",
        "downloadFailed",
        "done",
        "noArtifact",
        "needsConfirm",
        "failed",
    ] {
        assert_eq!(
            summary[key],
            json!(1),
            "summary.{key} 必须为 1 —— 计数与状态表漂移了"
        );
    }
}
