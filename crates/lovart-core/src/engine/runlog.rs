//! 运行日志（`var/run.log`）—— **过程时间线**。
//!
//! ── 为什么需要它（与 `state.jsonl` 的分工）────────────────────────────
//! `state.jsonl` 是**结果快照**：每条任务的最新状态 + 最后一次错误原文。
//! 它回答不了「几点几分断的、断在哪一步」—— 而这正是挂机排查的第一问。
//! 原先 GUI 把调度器的过程输出**直接丢掉**（`gui.rs` 里的空闭包），CLI 只
//! `println` 到终端，终端一关就没了。这里补上唯一的持久化通道。
//!
//! ── 三条硬规则 ────────────────────────────────────────────────────
//! 1. **日志永远不许影响跑批**：任何写失败（盘满 / 权限 / 文件被删）都只
//!    丢句柄、下一行重开，绝不返回错误、绝不 panic —— 日志是旁路，不是依赖。
//! 2. **每行带时间戳**：多行文案（上游错误原文常带换行）逐行加前缀，
//!    保证 `grep` 出来的每一行都能独立看懂。
//! 3. **有上限**：超过 [`MAX_BYTES`] 轮转成 `run.log.1`（只留一份历史），
//!    避免挂机跑几个月把盘写满。
//!
//! 绑定是**进程级一次性的**（[`init`]），因为 GUI 每个批次都走同一条入口，
//! 而日志文件按工作区固定 —— 重复 init 没有意义，也不该把两批的日志拆开。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 单个文件上限；超过就轮转成 `run.log.1`（只保留一份历史）。
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;

struct Sink {
    path: PathBuf,
    /// `None` = 上次打开或写入失败，下一行会重试。持锁期间只做一次 `write_all`。
    file: Mutex<Option<fs::File>>,
}

static SINK: OnceLock<Sink> = OnceLock::new();

/// 绑定日志文件。**进程内只生效一次**：返回 `false` = 已经绑定过（本次忽略）。
///
/// 绑定会顺手建好父目录并写一行开场，用来回答「这份日志是哪一次进程写的」——
/// 挂机第二天打开日志，一眼就能看到中间有没有重启过。
pub fn init(path: impl Into<PathBuf>) -> bool {
    let path = path.into();
    if SINK.get().is_some() {
        return false;
    }
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let sink = Sink {
        path,
        file: Mutex::new(None),
    };
    write_message(
        &sink,
        &format!("===== 日志会话开始 pid={} =====", std::process::id()),
    );
    SINK.set(sink).is_ok()
}

/// 当前日志文件（未绑定时 `None`）。仅供提示用户「日志在哪」。
pub fn path() -> Option<&'static Path> {
    SINK.get().map(|sink| sink.path.as_path())
}

/// 追加一行（多行会自动逐行加时间戳）。**未绑定时静默无操作**。
pub fn line(message: &str) {
    let Some(sink) = SINK.get() else {
        return;
    };
    write_message(sink, message);
}

fn now_stamp() -> String {
    // 本地时间：排查时人对得上「我几点走的、几点断的」。
    chrono::Local::now()
        .format("%Y-%m-%d %H:%M:%S%.3f")
        .to_string()
}

/// 拼装成「每行一个时间戳前缀」。纯函数：单测直接钉住格式，不再抄第二份。
fn format_lines(stamp: &str, message: &str) -> String {
    let mut body = String::new();
    for part in message.split('\n') {
        let part = part.trim_end();
        if part.is_empty() {
            continue;
        }
        body.push_str(&format!("[{stamp}] {part}\n"));
    }
    body
}

fn write_message(sink: &Sink, message: &str) {
    let body = format_lines(&now_stamp(), message);
    if body.is_empty() {
        return;
    }

    let mut guard = sink
        .file
        .lock()
        .unwrap_or_else(|error| error.into_inner());

    // 轮转：先关句柄再改名（Windows 上打开着的文件改不了名）。
    if guard
        .as_ref()
        .and_then(|file| file.metadata().ok())
        .is_some_and(|meta| meta.len() > MAX_BYTES)
    {
        *guard = None;
        let _ = fs::rename(&sink.path, backup_path(&sink.path));
    }
    if guard.is_none() {
        *guard = open_append(&sink.path);
    }
    let Some(file) = guard.as_mut() else {
        return;
    };
    if file
        .write_all(body.as_bytes())
        .and_then(|_| file.flush())
        .is_err()
    {
        // 写失败就丢掉句柄，下一行重开。**不回报、不中断** —— 见文件头规则 1。
        *guard = None;
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".1");
    PathBuf::from(name)
}

fn open_append(path: &Path) -> Option<fs::File> {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 轮转路径是原路径加后缀() {
        let path = Path::new("/tmp/var/run.log");
        assert_eq!(backup_path(path), PathBuf::from("/tmp/var/run.log.1"));
    }

    #[test]
    fn 多行逐行带时间戳() {
        let stamp = "2026-10-17 20:00:00.000";
        // 空行不落盘；上游错误原文常带换行，每行都必须能独立看懂。
        assert_eq!(
            format_lines(stamp, "第一行\n\n第二行\n"),
            "[2026-10-17 20:00:00.000] 第一行\n[2026-10-17 20:00:00.000] 第二行\n"
        );
        assert_eq!(format_lines(stamp, ""), "");
        assert_eq!(format_lines(stamp, "\n\n"), "");
    }

    #[test]
    fn 写进真实文件且每行都有时间戳() {
        // 用独立的 Sink，不碰全局（`OnceLock` 一旦绑定就跨测试生效）。
        let dir = std::env::temp_dir().join(format!(
            "lovart-runlog-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        let path = dir.join("var").join("run.log");
        let sink = Sink {
            path: path.clone(),
            file: Mutex::new(None),
        };

        write_message(&sink, "第一行\n第二行");
        write_message(&sink, "第三行");

        let text = std::fs::read_to_string(&path).expect("日志应已落盘");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|line| line.starts_with('[')));
        assert!(lines[0].ends_with("第一行"));
        assert!(lines[2].ends_with("第三行"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 路径不可写时不崩溃也不报错() {
        // 日志是旁路：绑到一个不可能创建的路径也只能静默失败。
        let sink = Sink {
            path: PathBuf::from("/dev/null/不可能/run.log"),
            file: Mutex::new(None),
        };
        write_message(&sink, "写不进去也不能影响跑批");
    }
}
