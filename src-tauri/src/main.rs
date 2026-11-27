//! 客户端入口 —— 双击 = 图形界面（唯一形态）。
//!
//! ⚠️ 命令行模式已删除：其他程序走本机 HTTP（`src-tauri/src/http.rs`），
//!    读写同一套 `data/`。见 `docs/接入指南-本机HTTP-其他前端-2026-11-20.md`。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    lovart_client::gui::run();
}
