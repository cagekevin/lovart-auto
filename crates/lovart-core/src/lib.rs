//! Lovart 核心 —— **协议层与调度层**，不依赖任何 GUI 框架。
//!
//! ── 为什么单独一个 crate ──────────────────────────────────────────────
//! `ADR-0004` / `ADR-0009` 第一级：能让写错**编译不过**，就不靠文档或扫描。
//! 本 crate 的 `Cargo.toml` 没有 tauri 依赖 ⇒ 在这里写 `use tauri::…`
//! 是 **E0432**，任意人、任意位置都绕不过去。
//!
//! ── 分层（依赖方向单向，不许反向）─────────────────────────────────────
//! ```text
//! lovart/   协议层（HMAC 签名 / 轮询状态机 / project / 附件 / 错误码表）
//! engine/   调度与状态（`state.jsonl` 的唯一写者）
//! service/  （下一步）提交 / 看板 / 停止的唯一编排入口
//! fsio.rs   crate 根工具（原子写）：横跨两层都要用 ⇒ 不许放进 engine/ 或 lovart/
//! assistant/ LLM 对话（会话真相文件的唯一写者）：只依赖 fsio 与 serde，不碰 lovart 网关
//! ```
//! `engine` 可以依赖 `lovart`；`lovart` 不许依赖 `engine`。
//! ⚠️ 同 crate 内 Rust 不禁止模块互相 `use`，这条方向**靠评审**（`ADR-0007`），
//! 不是编译期强制 —— 要强制得再拆 crate，本轮不做（`ADR-0002`：不必要地拆也是加壳）。

pub mod assistant;
pub mod engine;
pub mod fsio;
pub mod lovart;
pub mod service;
