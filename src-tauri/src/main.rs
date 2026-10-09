//! 客户端入口 —— **同一个二进制**：双击 = GUI，带子命令 = 无头 CLI。
//!
//! ⚠️ 命令行跑批是**用户能力**（不想开 GUI / 脚本化 / 服务器上用），
//! 只是 **AI 的工作流不调它** —— AI 只写文件 + `--check` 自检，发不发由用户定。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // 无参数 = 双击 = 开窗口。
    if args.is_empty() {
        lovart_client::gui::run();
        return;
    }

    // 有参数 = 无头 CLI。流程在库里，GUI 侧复用同一份（不抄第二份）。
    if let Err(message) = lovart_client::run::main_with_args(&args) {
        eprintln!("\n启动失败：{message}");
        std::process::exit(1);
    }
}
