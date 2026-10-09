//! 产物路径的**可搬运性**契约（`run::resolve_file`）。
//!
//! 守一条规则：**记录里不许钉死"文件夹叫什么、放在哪"。**
//!
//! ── 为什么专门守它 ────────────────────────────────────────────────
//! 2026-10-01 实测踩过一次：`state.jsonl` 里存的是绝对路径
//! （`/Users/…/旧名字/output/xxx.png`），用户把仓库文件夹**改了名**，
//! 8 条记录全指向不存在的目录 ⇒ 界面上的图**全成破图**，而图其实好好躺在 `output/` 里。
//!
//! 现在的口径：记录里存**相对输出目录**的路径，读取时才按**当前**输出目录拼回绝对路径。
//! ⇒ 搬运 / 改名不再影响历史记录。旧记录里是绝对路径 ⇒ 原样返回（向后兼容）。
#![allow(non_snake_case)]

use std::path::{Path, PathBuf};

use lovart_client::run::resolve_file;

#[test]
fn 相对路径按当前输出目录拼回来() {
    let output = PathBuf::from("/tmp/某工作区/output");
    assert_eq!(
        resolve_file(&output, "abc123_一只猫.png"),
        PathBuf::from("/tmp/某工作区/output/abc123_一只猫.png")
    );
}

#[test]
fn 绝对路径原样返回_不替它猜搬到哪了() {
    // 旧记录里是绝对路径：它可能已经失效，但那不归这里管 ——
    // 猜一个"可能搬到哪"就是伪造事实。失效由 `filesPresent` 如实报成 `present:false`。
    let absolute = "/Users/kevin/Documents/某个改过名的仓库/output/abc123_一只猫.png";
    assert_eq!(
        resolve_file(Path::new("/tmp/随便/out"), absolute),
        PathBuf::from(absolute)
    );
}

#[test]
fn 工作区改名不影响老记录找图() {
    // 同一个"相对路径"，换个工作区就指到新的地方 —— 这正是要的性质。
    // ⚠️ 这里用的是**中性旧名**，不是真实历史路径 —— 若写成当前目录名，
    //    下面的 `assert_ne!` 会变成"自己不等于自己"，测试**假红**。
    let before = resolve_file(Path::new("/Users/kevin/Documents/旧工作区名/output"), "a.png");
    let after = resolve_file(Path::new("/Users/kevin/Documents/lovart 自动/output"), "a.png");
    assert_eq!(before.file_name(), after.file_name());
    assert_ne!(before, after, "相对路径必须跟着工作区走，而不是钉在旧绝对路径上");
}
