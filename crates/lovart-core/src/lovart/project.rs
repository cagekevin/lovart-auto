//! `lovart_project` — 按 accessKey 缓存默认 project + 失效自愈
//! （对应 TS `lovart/lovart_project.ts`）。
//!
//! project 只是上游的会话容器，**应用重开时重建一次无副作用** ⇒ 不做磁盘持久化。
//!
//! 对齐上游行为：
//!   - `ensure`：缓存命中直接返回，不做额外 validate 网络往返；
//!   - **失效自愈点在「发送失败时」**：send 抛 project 失效错误 → clear 缓存 → 重建 → 重试一次；
//!   - **并发**：ensure / clear 串行化，避免并发首次建 project 时各建一个。
//!
//! ── 与 TS 的一处**有意差异** ────────────────────────────────────────
//! TS 用**模块级全局** `Map` + promise 链。Rust 侧做成 [`ProjectCache`] **结构体**，
//! 由调用方持有并传入 —— 全局可变状态在 Rust 里要 `Mutex` + `OnceLock`，
//! 而"测试之间共享缓存"会让测试互相污染。结构体既等价又可控。

use std::collections::BTreeMap;
use std::sync::Mutex;

use super::client::{create_project, send_chat, ClientDeps};
use super::contract::SendInput;
use super::errors::{ErrCode, ErrType, LovartError};

/// project 缓存（等价 TS 的模块级 `projectCache` + `mutationChain`）。
#[derive(Default)]
pub struct ProjectCache {
    /// accessKey → project_id（进程内权威缓存）。
    inner: Mutex<BTreeMap<String, String>>,
    /// ensure / clear 的串行化闸（等价上游的 `asyncio.Lock`）。
    ///
    /// ⚠️ 这个锁会**跨网络调用**持有 —— 那是刻意的：并发首次建 project 时，
    /// 不串行化就会各建一个（上游会看到一堆孤儿 project）。
    gate: Mutex<()>,
}

impl ProjectCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// ensure 默认 project：缓存命中直接返回；未命中则建一次并写回缓存。
    pub fn ensure(&self, deps: &ClientDeps) -> Result<String, LovartError> {
        let key = deps.access_key.clone();
        if let Some(cached) = self.get(&key) {
            return Ok(cached);
        }

        let _guard = self.gate.lock().unwrap_or_else(|error| error.into_inner());
        // 链内二次检查：等待期间别的 ensure 可能已建好。
        if let Some(cached) = self.get(&key) {
            return Ok(cached);
        }

        let project_id = create_project(deps)?;
        if project_id.is_empty() {
            return Err(LovartError::internal(
                "Lovart 创建 project 失败：未返回 project_id",
                ErrType::ProjectInvalid,
            ));
        }
        self.put(&key, &project_id);
        Ok(project_id)
    }

    /// 清除某 accessKey 的 project 缓存（供失效自愈时重建用）。
    pub fn clear(&self, access_key: &str) {
        let _guard = self.gate.lock().unwrap_or_else(|error| error.into_inner());
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(access_key);
    }

    fn get(&self, key: &str) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(key)
            .cloned()
    }

    fn put(&self, key: &str, value: &str) {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(key.to_string(), value.to_string());
    }
}

/// 判断某发送错误是否表示「上游 project 已失效」（需重建）。
///
/// ⚠️ **这是已知的债**：靠**文案关键词**判定，而不是上游错误码 ——
/// 上游改一句提示语就失效，而且**不报错**（静默退化成「每次都重建 project」）。
/// 移植时保持原行为（不擅自改判定口径 —— 改了就可能把「真失效」漏判成「别的问题」，
/// 那是更坏的失败方向）。等拿到上游的失效错误码再换成码判定。
pub fn is_project_invalid(error: &LovartError) -> bool {
    let message = error.message().to_lowercase();
    const HINTS: [&str; 12] = [
        "not found",
        "not exist",
        "does not exist",
        "invalid",
        "expired",
        "deleted",
        "missing",
        "unknown project",
        "项目不存在",
        "已删除",
        "失效",
        "不存在",
    ];
    if HINTS.iter().any(|hint| message.contains(hint)) {
        return true;
    }
    // 或（状态为 400/404/409 且文案含 "project"）
    match error.code().and_then(ErrCode::as_number) {
        Some(status) if (status == 400 || status == 404 || status == 409) => {
            message.contains("project")
        }
        _ => false,
    }
}

/// send + project 失效自愈：ensure project → send；
/// 抛 project 失效错误则 clear + 重建 → 重试一次；仍失败原样抛出。
///
/// 返回 `(thread_id, project_id)`。
pub fn send_chat_with_project(
    cache: &ProjectCache,
    deps: &ClientDeps,
    input: &SendInput,
) -> Result<(String, String), LovartError> {
    let project_id = cache.ensure(deps)?;

    let mut attempt = input.clone();
    attempt.project_id = project_id.clone();
    match send_chat(deps, &attempt) {
        Ok(thread_id) => Ok((thread_id, project_id)),
        Err(error) if is_project_invalid(&error) => {
            // 自愈：清掉坏缓存 → 重建 → **只重试一次**（不循环，避免无限重建）。
            cache.clear(&deps.access_key);
            let rebuilt = cache.ensure(deps)?;
            attempt.project_id = rebuilt.clone();
            let thread_id = send_chat(deps, &attempt)?;
            Ok((thread_id, rebuilt))
        }
        Err(error) => Err(error),
    }
}
