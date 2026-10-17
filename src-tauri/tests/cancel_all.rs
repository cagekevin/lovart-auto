//! 「取消所有任务」的删除核心：**只删看板未结束**（处理中 / 排队 / 待处理），已结束原样保留。
//!
//! 落点说明：判据与删除都只有一份实现（`run::is_board_open` + `StateStore::remove`），
//! 这里测的是"选谁"和"该不该写盘"，不重复测状态表本身。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use lovart_client::engine::state::StateStore;
use lovart_client::gui::remove_open_tasks_core;

/// 每次一个独立目录 —— 与 `state_invariants.rs` 同款（不引 `tempfile` 依赖）。
static SEQ: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lv-cancel-{}-{}-{name}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn line(id: &str, status: &str) -> String {
    format!(
        r#"{{"id":"{id}","prompt":"p","model":"m","aspectRatio":"3:4","imageSize":"2K","status":"{status}","attempts":0,"timeoutCount":0,"busyRetries":0,"readyAt":0,"stateSince":"T1","updatedAt":"T1"}}"#
    )
}

fn write_state(dir: &Path, lines: &[String]) -> PathBuf {
    let path = dir.join("var/state.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

fn remaining_ids(path: &Path) -> Vec<String> {
    StateStore::open_read_only(path)
        .unwrap()
        .all()
        .into_iter()
        .map(|record| record.id.clone())
        .collect()
}

#[test]
fn 只删未结束的记录_已结束原样保留() {
    let dir = temp_dir("mixed");
    let path = write_state(
        &dir,
        &[
            line("1-running", "running"),
            line("2-pending", "pending"),
            line("3-needs", "needs_confirm"),
            line("4-done", "done"),
        ],
    );

    let removed = remove_open_tasks_core(&path).unwrap();

    assert_eq!(removed, 3, "处理中 + 排队 + 待处理都该删");
    assert_eq!(
        remaining_ids(&path),
        vec!["4-done".to_string()],
        "已结束的记录一条都不许动；needs_confirm 属于待处理，必须删（它不是 is_unfinished）"
    );
}

#[test]
fn 没有未结束的记录_不写盘() {
    let dir = temp_dir("only-finished");
    let path = write_state(&dir, &[line("1-done", "done"), line("2-no-artifact", "no_artifact")]);
    let before = fs::read(&path).unwrap();

    let removed = remove_open_tasks_core(&path).unwrap();

    assert_eq!(removed, 0);
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "没有可删的就不许重写整个文件（白压一次 = 无谓的写风险）"
    );
}

#[test]
fn 状态文件不存在_返回零而不是报错() {
    let dir = temp_dir("missing");
    let path = dir.join("var/state.jsonl");

    assert_eq!(remove_open_tasks_core(&path).unwrap(), 0, "没有 ≠ 读失败");
    assert!(!path.exists(), "不许顺手建出一个空文件");
}

#[test]
fn 坏行不影响清空() {
    let dir = temp_dir("corrupt");
    let path = write_state(
        &dir,
        &[line("1-running", "running"), r#"{"id":"bad""#.to_string()],
    );

    assert_eq!(remove_open_tasks_core(&path).unwrap(), 1);
    assert!(remaining_ids(&path).is_empty(), "坏行按既有语义跳过，未结束照删");
}
