//! 两个板级清理动作的删除核心：
//!
//! - 「取消所有未完成任务」→ [`remove_open_tasks_core`]：只删看板**未结束**
//!   （处理中 / 排队 / 待处理），已结束原样保留；
//! - 「清空所有已完成任务」→ [`remove_finished_tasks_core`]：只删 `finished` 组，
//!   未结束（含待处理）原样保留。
//!
//! 两个作用域互为补集，所以下面**两边都立了"对方那条一条都不许动"的用例** ——
//! 只测自己那半，"顺手把另一半也删了"这种错就测不出来。
//!
//! 落点说明：判据与删除都只有一份实现（`run::is_board_open` / `run::is_board_finished`
//! + `StateStore::remove`），这里测的是"选谁"和"该不该写盘"，不重复测状态表本身。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use lovart_client::engine::state::StateStore;
use lovart_client::gui::{remove_finished_tasks_core, remove_open_tasks_core};

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

// ── 「清空所有已完成任务」：作用域是 `finished` 组 ──────────────────────

#[test]
fn 清空已完成_只删已结束的记录() {
    let dir = temp_dir("clear-finished");
    let path = write_state(
        &dir,
        &[
            line("1-running", "running"),
            line("2-needs", "needs_confirm"),
            line("3-done", "done"),
            line("4-no-artifact", "no_artifact"),
        ],
    );

    let removed = remove_finished_tasks_core(&path).unwrap();

    assert_eq!(removed, 2, "只删已结束的两条");
    let mut left = remaining_ids(&path);
    left.sort();
    assert_eq!(
        left,
        vec!["1-running".to_string(), "2-needs".to_string()],
        "未结束（含待处理的 needs_confirm）一条都不许动 —— 那是「取消所有未完成任务」的作用域"
    );
}

#[test]
fn 没有已结束的记录_不写盘() {
    let dir = temp_dir("clear-finished-none");
    let path = write_state(
        &dir,
        &[line("1-running", "running"), line("2-pending", "pending")],
    );
    let before = fs::read(&path).unwrap();

    assert_eq!(remove_finished_tasks_core(&path).unwrap(), 0);
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "没有可删的就不许重写整个文件（白压一次 = 无谓的写风险）"
    );
}

#[test]
fn 清空已完成_状态文件不存在_返回零而不是报错() {
    let dir = temp_dir("clear-finished-missing");
    let path = dir.join("var/state.jsonl");

    assert_eq!(remove_finished_tasks_core(&path).unwrap(), 0, "没有 ≠ 读失败");
    assert!(!path.exists(), "不许顺手建出一个空文件");
}
