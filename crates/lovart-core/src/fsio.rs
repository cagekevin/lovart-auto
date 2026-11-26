//! 文件原子写 —— **全 crate 唯一实现**。
//!
//! ── 为什么在 crate 根，不放进 `engine/` 或 `lovart/` ──────────────────
//! 调用方横跨两层：`lovart/preset.rs`（预设清单）与 `assistant/`（会话真相文件）。
//! `engine` 依赖 `lovart`（多个 `use crate::lovart`），反过来没有 ⇒ 放进
//! `engine/` 会让 `lovart/preset.rs` **反向依赖**（`ADR-0007`）；
//! 放进 `lovart/` 又给一个通用文件工具挂上"出图网关"的领域含义。
//! 所以：`lovart ← engine ← service` 之下的 crate 根，谁都能用。
//!
//! ── 它保证什么、不保证什么 ────────────────────────────────────────────
//! 保证：**不会读到写了一半的文件**（写 `.tmp` → 同卷 `rename`，原子替换）。
//! 不保证：**不丢更新**。两次并发"读 → 改 → 写"仍会互相覆盖，
//! 调用方必须自己串行化（单写者 / 一把锁）。

use std::io;
use std::path::{Path, PathBuf};

/// 用 `bytes` 整体替换 `path` 的内容（原子）。
///
/// ⚠️ **不建父目录**：目标目录由调用方负责（各模块的"建目录"各有自己的错误文案）。
/// 写临时文件失败时会清掉 `.tmp`，不在磁盘上留残骸。
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp_os = path.as_os_str().to_os_string();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);

    if let Err(error) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lovart-fsio-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    #[test]
    fn 原子写_落盘内容正确且不留_tmp() {
        let dir = temp_dir("write");
        let path = dir.join("a.json");

        write_atomic(&path, b"first").expect("写第一版");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        assert!(!dir.join("a.json.tmp").exists(), "tmp 应被 rename 走");

        write_atomic(&path, b"second").expect("覆盖第二版");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        assert!(!dir.join("a.json.tmp").exists(), "tmp 应被 rename 走");
    }

    #[test]
    fn 父目录不存在_报错而不是悄悄建() {
        let dir = temp_dir("missing");
        let path = dir.join("no-such-dir").join("a.json");

        let error = write_atomic(&path, b"x").expect_err("父目录不存在应报错");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(!path.exists(), "不许留下目标文件");
        assert!(!dir.join("no-such-dir").exists(), "不许替调用方建目录");
    }
}
