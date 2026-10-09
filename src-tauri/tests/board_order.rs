//! 板面顺序契约 —— 「已结束按完成时间倒序 · 未结束按先后顺序」。
//!
//! ── 为什么必须有它 ────────────────────────────────────────────────
//! `StateStore::all()` 返回的是 `BTreeMap` 的**键序**，而键是**内容哈希 id**
//! ⇒ 不排的话顺序看起来是随机的，与提交/完成时间都无关。
//! 2026-10-01 实测：14:56 完成的那条（id `1b9204894b5a`）不排就排在第 3 位，
//! 用户看到的是「刚完成的跑到第二个去了」。
//!
//! 这条契约把它钉死，防止哪天有人"顺手"把 [`board_order`] 去掉、退回随机顺序。
//!
//! ⚠️ 反向探针：把 `build_status_json` 里的 `board_order(&mut records)` 删掉，
//!    这条立刻红。**刻意走真实出口而不是直接调 `board_order`** ——
//!    直接调只能证明"这个函数是对的"，证明不了"有人真的在用它"。

#![allow(non_snake_case)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use lovart_client::run::build_status_json;

static SEQ: AtomicUsize = AtomicUsize::new(0);

fn write_state(lines: &[&str]) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("lv-board-{}-{}", std::process::id(), n));
    fs::create_dir_all(&dir).expect("建临时目录");
    let path = dir.join("state.jsonl");
    fs::write(&path, lines.join("\n")).expect("写 state");
    path
}

/// 造一条记录行。`state_since` 传 `None` = **旧记录**（没有完成时间）。
fn record(id: &str, status: &str, line: u32, state_since: Option<&str>) -> String {
    let since = match state_since {
        Some(value) => format!(r#""stateSince":"{value}","#),
        None => String::new(),
    };
    format!(
        r#"{{"id":"{id}","prompt":"P-{id}","line":{line},"model":"nano-bn-2","aspectRatio":"3:4","imageSize":"2K","status":"{status}","attempts":1,"timeoutCount":0,"busyRetries":0,"readyAt":0,{since}"updatedAt":"2026-10-01T00:00:00.000Z"}}"#
    )
}

fn ordered_ids(path: &Path) -> Vec<String> {
    // 走**真实出口**（GUI 与 `--status --json` 共用的那一个），不直接调 `board_order`。
    let payload = build_status_json(path, Path::new("/tmp")).expect("构造看板 JSON");
    payload["tasks"]
        .as_array()
        .expect("tasks 必须是数组")
        .iter()
        .map(|task| task["id"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn 已结束按完成时间倒序_未结束按先后顺序() {
    let path = write_state(&[
        // 已结束三条：完成时间 10:00 / 14:56 / **未知**（旧记录）
        &record("done-old", "done", 1, Some("2026-10-01T10:00:00.000Z")),
        &record("done-new", "done", 2, Some("2026-10-01T14:56:36.000Z")),
        &record("done-unknown", "done", 3, None),
        // 未结束两条：提示词行号 2 / 1（先后顺序 = 行号升序）
        &record("live-line2", "running", 2, None),
        &record("live-line1", "pending", 1, None),
    ]);

    assert_eq!(
        ordered_ids(&path),
        [
            // 还在跑的排前面，内部按先后顺序（先发的在上）
            "live-line1",
            "live-line2",
            // 已结束按完成时间倒序；完成时间**未知**的沉底
            "done-new",
            "done-old",
            "done-unknown",
        ],
        "板面顺序不对 —— 见本文件头（不排 = id 字典序 = 看起来随机）"
    );
}
