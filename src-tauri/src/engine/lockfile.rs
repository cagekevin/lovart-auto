//! 单实例文件锁 —— `state.jsonl` 只有一个写者。
//!
//! ── 为什么需要 ──────────────────────────────────────────────────────
//! 常驻调度器与 CLI 跑批都会写 `var/state.jsonl`。两个进程同时写 = 后写覆盖先写：
//! 记录凭空消失（假删）或被写回（复活）。原先靠「GUI 里没法开第二个」这种约定挡着，
//! 常驻化之后必须由机器挡（`docs/plan/09` §1.5.5 / §3.5）。
//!
//! ── 与「只读命令」的边界 ────────────────────────────────────────────
//! `--status` / `--check` / `--dry-run` / `--list-models` / `--status-table`
//! **不抢锁**：它们不写状态文件。抢锁会让「GUI 跑批时查状态」变成报错 ——
//! 那是用户可感知的行为改变。
//!
//! ── 实现 ────────────────────────────────────────────────────────────
//! 不引 `fs2`，也不用 `tauri-plugin-single-instance`（后者只覆盖 GUI 分支，
//! 而 CLI 是同一二进制的无 GUI 路径，管不到）：
//!   - Windows：`share_mode(0)` 独占打开，第二个句柄拿到 ERROR_SHARING_VIOLATION；
//!   - Unix：`libc::flock(LOCK_EX | LOCK_NB)`（`libc` 已是依赖）。
//! 两者的释放都由「进程退出 / 句柄关闭」保证，不需要显式 unlock、也不会留下
//! 需要人工清理的锁文件（文件留在磁盘上是正常的，它就是锁的载体）。
//!
//! ── 禁止 ────────────────────────────────────────────────────────────
//! 不许在同一个进程里重复 `acquire` 同一个路径（Windows 的 `share_mode(0)`
//! 对同进程第二个句柄同样拒绝，会把自己锁在外面）。进程内只允许一处抢锁。

use std::fs::File;
use std::path::{Path, PathBuf};

/// 进程级单实例锁。
///
/// ⚠️ 存活期必须覆盖整个跑批 / 整个窗口：句柄一关，另一个进程就能进来写
/// `state.jsonl`。调用方拿到的值**不要**提前 `drop`。
pub struct FileLock {
    /// 句柄一关锁就没了 —— 靠持有它来持锁，没有别的动作。
    _file: File,
    path: PathBuf,
}

impl FileLock {
    /// 抢锁。抢不到一律 `Err`，**不静默降级**成「没锁也能跑」。
    pub fn acquire(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("建不了锁目录 {}：{error}", parent.display()))?;
        }
        let file = open_exclusive(path).map_err(|error| {
            if is_busy(&error) {
                format!(
                    "已有另一个 lovart-client 在运行（{} 被占用）。\
                     同时开两个会互相覆盖 state.jsonl —— 请先退出那一个。",
                    path.display()
                )
            } else {
                format!("建不了单实例锁 {}：{error}", path.display())
            }
        })?;
        Ok(Self {
            _file: file,
            path: path.to_path_buf(),
        })
    }

    /// 锁文件路径（报错文案与排查用）。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(windows)]
fn open_exclusive(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    // `share_mode(0)` = 不允许任何其他句柄读写打开。第二个实例必然拿到
    // ERROR_SHARING_VIOLATION(32)，这正是我们要的「占用」信号。
    // 不用 `create_new`：锁文件本来就该复用，占用语义由共享模式表达。
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .share_mode(0)
        .open(path)
}

#[cfg(unix)]
fn open_exclusive(path: &Path) -> std::io::Result<File> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)?;
    // LOCK_NB：抢不到立刻返回，不等待 —— CLI 报错退出比静默挂起好。
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

#[cfg(windows)]
fn is_busy(error: &std::io::Error) -> bool {
    // 32 = ERROR_SHARING_VIOLATION，33 = ERROR_LOCK_VIOLATION。
    matches!(error.raw_os_error(), Some(32) | Some(33))
}

#[cfg(unix)]
fn is_busy(error: &std::io::Error) -> bool {
    // EWOULDBLOCK 与 EAGAIN 在多数平台同值；两个都判，避免平台差异漏掉。
    matches!(
        error.raw_os_error(),
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
    )
}
