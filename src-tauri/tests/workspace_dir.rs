//! 「GUI 工作区怎么挑」的契约测试（`gui::pick_workspace`）。
//!
//! 守一条规则：**双击打开时，运行时文件的根必须落在一个仓库目录里**，
//! 不能是 Finder 交给进程的当前目录 `/`（理由见 `gui.rs::workspace_dir` 的文档）。
//!
//! 为什么这件小事要专门钉死：挑错了**不报错** —— `.env` 读不到（设置界面全空）、
//! `var/state.jsonl` 建不出来、`output/` 写不进去。界面照常开着，
//! 失败只表现为"每条动作都没反应"，正是「失败伪装成空」那一族。
//! 顺序一旦被后来者"顺手简化"，就会悄悄退回原样，所以这里把它写成断言。

use std::path::PathBuf;

use lovart_client::gui::pick_workspace;

/// 一个**像仓库**的临时目录（有 `inbox/` 即算，见 `is_workspace` 判据）。
fn repo_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lv-repo-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("inbox")).expect("建临时 inbox");
    dir
}

/// 一个**不像仓库**的临时目录（空目录）。
fn plain_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lv-plain-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

#[test]
fn 显式指定最高优先_哪怕它不像仓库() {
    let explicit = plain_dir("explicit");
    let cwd = repo_dir("explicit-cwd");
    // 用户明说用哪里 ⇒ 说错也照着去（不替他改判），否则"显式指定"就是假的。
    assert_eq!(
        pick_workspace(Some(explicit.clone()), Some(cwd), None),
        Some(explicit)
    );
}

#[test]
fn 显式指定为空串_等于没指定() {
    let cwd = repo_dir("empty-explicit");
    assert_eq!(
        pick_workspace(Some(PathBuf::new()), Some(cwd.clone()), None),
        Some(cwd),
        "空串不是地址，必须让位给下一个候选"
    );
}

#[test]
fn 当前目录是仓库_就用当前目录() {
    let cwd = repo_dir("cwd");
    let sibling = repo_dir("cwd-sibling");
    // 终端里 `cd 仓库根 && ./lovart-client` 的行为必须保持不变。
    assert_eq!(
        pick_workspace(None, Some(cwd.clone()), Some(sibling)),
        Some(cwd)
    );
}

#[test]
fn 当前目录不是仓库_退到_app同级目录() {
    let cwd = plain_dir("fallback-cwd");
    let sibling = repo_dir("fallback-sibling");
    // 双击 `.app`：当前目录是 `/`（不像仓库），此时用 `.app` 所在目录。
    assert_eq!(
        pick_workspace(None, Some(cwd), Some(sibling.clone())),
        Some(sibling)
    );
}

#[test]
fn 两个候选都不是仓库_返回_none_交给兜底() {
    // `None` 是**有意义**的返回值：调用方据此落到 ~/Documents/lovart。
    // 若这里改成"随便挑一个"，双击启动就会把 `var/` 写进 `/`。
    assert_eq!(
        pick_workspace(None, Some(plain_dir("none-cwd")), Some(plain_dir("none-sibling"))),
        None
    );
    assert_eq!(pick_workspace(None, None, None), None);
}

#[test]
fn 判据认目录结构_不必已经有_env() {
    // 刚解压、还没填密钥的仓库：只有 `.env`（哪怕还没配）或源码目录也算工作区，
    // 否则用户配好的 `inbox/` 会被判到兜底目录去、白放。
    for marker in [".env", "src-tauri"] {
        let dir = std::env::temp_dir().join(format!(
            "lv-marker-{}-{}",
            std::process::id(),
            marker.replace('.', "_")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        if marker == ".env" {
            std::fs::write(dir.join(marker), "").expect("写 .env");
        } else {
            std::fs::create_dir_all(dir.join(marker)).expect("建源码目录");
        }
        assert_eq!(
            pick_workspace(None, Some(dir.clone()), None),
            Some(dir),
            "只有 {marker} 也应当被认成仓库"
        );
    }
}
