//! 「工作区 = 程序所在目录」的契约测试（用户裁定 2026-11-26 · plan 19 §3.4）。
//!
//! 唯一判据：`install_dir_from_exe(exe)`。
//! - Windows：exe 所在目录；
//! - macOS：`.app` 同级（exe 在包内 `Contents/MacOS/`，那不是用户放数据的地方）。
//!
//! ⚠️ 没有 `LOVART_HOME`、没有 cwd 候选、没有"看目录内容猜是不是工作区"。
//! 那些叠起来就是「一个能力三个入口」，出事时分不清是哪条判据生效 —— 全部删掉。

use std::path::{Path, PathBuf};

use lovart_client::gui::install_dir_from_exe;

#[test]
fn windows_exe_取所在目录() {
    let exe = Path::new("G:/tools/lovart/lovart-client.exe");
    assert_eq!(
        install_dir_from_exe(exe),
        Some(PathBuf::from("G:/tools/lovart"))
    );
}

#[test]
fn macos_app包内的exe_取app同级() {
    // exe 在 <...>/lovart.app/Contents/MacOS/，取 lovart.app 的父目录。
    let exe = Path::new("/Applications/lovart.app/Contents/MacOS/lovart-client");
    assert_eq!(
        install_dir_from_exe(exe),
        Some(PathBuf::from("/Applications"))
    );
}

#[test]
fn 根目录里的exe_取根() {
    let exe = Path::new("C:/lovart-client.exe");
    assert_eq!(install_dir_from_exe(exe), Some(PathBuf::from("C:/")));
}

#[test]
fn 没有父目录时明确返回_none() {
    // 只有文件名、路径不可解析 ⇒ 不猜（报错由 workspace_dir 给）。
    assert_eq!(install_dir_from_exe(Path::new("lovart-client")), None);
}
