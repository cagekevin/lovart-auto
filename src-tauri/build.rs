//! Tauri 构建脚本。
//!
//! 它读 `tauri.conf.json`、把 `frontendDist`（= 仓库根的 `ui/`）编进二进制、
//! 并生成前端能调用的命令权限清单。
//!
//! ⚠️ 它**不需要 `tauri-cli`** —— `tauri-cli` 只负责打包成 `.app` / `.dmg`。
//! 日常开发 `cargo build` 就够了，产出的二进制直接能开窗口。

fn main() {
    tauri_build::build()
}
