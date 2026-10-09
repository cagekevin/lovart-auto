//! Lovart 客户端 —— **本仓的唯一实现**（GUI 与无头 CLI 是同一个二进制）。
//!
//! ── 迁移期已结束 ────────────────────────────────────────────────────
//! 早先有一份 TS 参照实现，本 crate 是**照它移植**的 —— 规则一律「照抄已验证的行为」，
//! 不重新探索。现在 Rust 回放 `fixtures/` 与它录下的黄金**逐条一致**
//! （`tests/golden_replay.rs`），参照物已完成使命、**已退场**。
//!
//! ⚠️ **后果**：`fixtures/*/expected-state.jsonl`（黄金）**不能再自动生成**
//! （生成器是参照实现那一侧的）。所以 `golden_replay.rs` 里除比对外还有
//! **显式规则断言** —— 黄金本身可能是错的，光比黄金发现不了。
//!
//! ── 分层 ────────────────────────────────────────────────────────────
//! ```text
//! lovart/   协议层（HMAC 签名 / 轮询状态机 / project / 附件 / 错误码表）
//! engine/   调度与状态（`state.jsonl` 的唯一写者）
//! cli / run 入口（GUI 与 CLI 共用）
//! gui       Tauri 壳（只读状态 + 起停批次，不实现业务规则）
//! ```
//!
//! ── 增量状态（全部完成）─────────────────────────────────────────────
//! | 增量 | 内容 | 状态 |
//! |---|---|---|
//! | 1 | `engine::state`（`state.jsonl` 唯一写者） | ✅ |
//! | 2 | `lovart::models` + `lovart::prompt`（JSONL 契约 + id 生成） | ✅ |
//! | 3 | `lovart::client` / `task` / `project` / `errors` / `contract` / `config` | ✅ |
//! | 4 | `engine::scheduler` + `download` + **回放 7 组黄金对账** | ✅ |
//! | 5 | `cli` + `run` + `connection` + `refs` + `attachments` | ✅ |
//! | 6 | `gui`（Tauri）+ 前端 `ui/` | ✅ |
//! | 7 | **对账已通过 ⇒ TS 参照实现已退场** | ✅ |

pub mod cli;
pub mod engine;
pub mod gui;
pub mod lovart;
pub mod run;
