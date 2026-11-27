//! 把连接与设置装配成客户端依赖。
//!
//! 原先在 `lovart::connection::to_client_deps`，但那里要读 `engine::settings`
//! ⇒ `lovart → engine` 的反向依赖（`ADR-0007`）。
//! 之后暂放在 `engine` 下，但它的消费者全在上层（`Workbench`、`gui`、`run`）
//! ⇒ 属**应用装配**，不是引擎内部知识；现在收进 `service`。
//!
//! 轮询参数在这里从设置填入 —— `ClientDeps::new` 保持**纯构造**（不读环境），
//! 因为三处测试直接调它，读了 env 就依赖开发机的 `.env`。

use std::sync::Arc;

use crate::engine::settings::Settings;
use crate::lovart::client::{ClientDeps, ReqwestTransport};
use crate::lovart::connection::Connection;

/// 由连接与**生效设置**构造客户端依赖。
///
/// ⚠️ `settings` 是参数而不是在这里读环境（2026-11-27，`docs/plan/29`）：轮询参数是
/// **批次参数**，生效点只有进程启动。这里读环境就等于给批次参数开第二个入口。
pub fn to_client_deps(connection: &Connection, settings: &Settings) -> Result<ClientDeps, String> {
    let transport = ReqwestTransport::new()?;
    let mut deps = ClientDeps::new(
        connection.base_url.clone(),
        connection.access_key.clone(),
        connection.secret_key.clone(),
        Arc::new(transport),
    );
    deps.poll_interval = settings.poll_interval();
    deps.poll_max_interval = settings.poll_max_interval();
    Ok(deps)
}
