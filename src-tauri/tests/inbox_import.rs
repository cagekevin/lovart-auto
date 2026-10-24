//! 「从 inbox 导入」的契约测试。
//!
//! 守两条规则：
//! 1. **`inbox/` 直属文件：导入 = 读 + 校验 + 归位，三件事一起发生**（DEBT **TD-05-12**）。
//!    少了「归位」⇒ 同一个文件能**反复导入** ⇒ 重复出图、重复扣积分，而且全程不报错。
//! 2. **`inbox/` 之外的文件（用户自己挑的）：只读，绝不移动** —— 那是用户的文件夹。

use std::path::{Path, PathBuf};

use lovart_client::gui::import_inbox_core;

/// 每次一个独立目录 —— 与 `state_invariants.rs` 同款（不引 `tempfile` 依赖）。
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lv-inbox-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("inbox")).expect("建临时 inbox");
    dir
}

fn write_inbox_file(cwd: &Path, name: &str, body: &str) -> PathBuf {
    let path = cwd.join("inbox").join(name);
    std::fs::write(&path, body).expect("写 inbox 文件");
    path
}

#[test]
fn 导入后文件必须离开_inbox_并归位到_imported() {
    let cwd = temp_dir("move");
    let file = write_inbox_file(&cwd, "batch.jsonl", "{\"prompt\":\"一只猫\"}\n");

    let tasks = import_inbox_core(&cwd, &file).expect("导入应当成功");
    assert_eq!(tasks.as_array().map(Vec::len), Some(1), "应当返回 1 条任务");

    // ① 原文件必须**离开** `inbox/` —— 否则 `list_inbox` 还会列到它，用户能再导一遍。
    assert!(!file.exists(), "导入后原文件必须不在 inbox/ 了");
    // ② 它必须**躺在 `imported/` 里**，而不是被删掉（删掉 = 丢数据）。
    assert!(
        cwd.join("inbox")
            .join("imported")
            .join("batch.jsonl")
            .is_file(),
        "必须归位到 inbox/imported/"
    );
}

#[test]
fn 归位重名不覆盖_换名进_imported() {
    let cwd = temp_dir("collide");
    // 先占位：`imported/` 里已经有一个同名文件（上一批的）。
    let imported = cwd.join("inbox").join("imported");
    std::fs::create_dir_all(&imported).expect("建 imported");
    std::fs::write(imported.join("batch.jsonl"), "{\"prompt\":\"上一批\"}\n").expect("写占位");

    let file = write_inbox_file(&cwd, "batch.jsonl", "{\"prompt\":\"这一批\"}\n");
    import_inbox_core(&cwd, &file).expect("导入应当成功");

    // 占位那份**必须还在且内容没变** —— 同名很可能是**另一批**提示词，覆盖 = 丢数据。
    let kept = std::fs::read_to_string(imported.join("batch.jsonl")).expect("读占位");
    assert!(kept.contains("上一批"), "旧文件被覆盖了：{kept}");
    // 新来的那份换名进去了 ⇒ 两份都在。
    assert_eq!(
        std::fs::read_dir(&imported).expect("列 imported").count(),
        2,
        "imported/ 里应当有两份（旧的 + 换了名的新份）"
    );
}

#[test]
fn inbox_之外的文件导入后原地不动() {
    let cwd = temp_dir("outside");
    let outside = cwd.join("elsewhere");
    std::fs::create_dir_all(&outside).expect("建外部目录");
    let file = outside.join("batch.jsonl");
    std::fs::write(&file, "{\"prompt\":\"一只狗\"}\n").expect("写外部文件");

    let tasks = import_inbox_core(&cwd, &file).expect("导入应当成功");
    assert_eq!(tasks.as_array().map(Vec::len), Some(1), "应当返回 1 条任务");

    // 用户的目录不许被程序改动：文件还在原地，`imported/` 里也不该多出副本。
    assert!(file.is_file(), "inbox 之外的文件必须原地不动");
    assert!(
        !cwd.join("inbox").join("imported").join("batch.jsonl").exists(),
        "不许把外部文件搬进 inbox/imported/"
    );
}

#[test]
fn 校验不过的文件留在_inbox_不被搬走() {
    let cwd = temp_dir("invalid");
    // 非法 JSON ⇒ 解析必然报错。
    let file = write_inbox_file(&cwd, "bad.jsonl", "这不是 JSON\n");

    let message = import_inbox_core(&cwd, &file).expect_err("非法文件必须报错");
    assert!(message.contains("第 1 行"), "报错要指到行，实际：{message}");

    // 顺序判据：**先读+校验、再归位** —— 校验不过 ⇒ 文件原地不动，
    // 用户改完还能再来一次（搬走了就得去 `imported/` 里捞）。
    assert!(file.is_file(), "校验不过时文件必须留在 inbox/");
}
