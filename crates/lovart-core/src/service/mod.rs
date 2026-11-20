//! 应用服务层 —— 提交 / 停止 / 看板的**唯一编排入口**。
//!
//! 依赖方向：`service → engine → lovart`（单向；`engine` 不许反过来认 `service`）。
//! 这里不出现 tauri：GUI 只是它的一个消费者（另一个是 `src-tauri/src/http`）。

pub mod api;
pub mod meta;
pub mod refs;
pub mod snapshot;
pub mod wiring;
pub mod workbench;

pub use snapshot::Snapshot;
pub use workbench::Workbench;
