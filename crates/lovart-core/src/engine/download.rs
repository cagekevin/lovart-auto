//! 产物下载（对应 TS `src/download.ts`）。
//!
//! URL 来自上游 CDN，不保证有扩展名，按 `Content-Type` 兜底。
//!
//! 三个要点：
//! 1. **重试**：下载失败多半是网络抖动，不该判任务失败，内部退避重试；
//! 2. **幂等**：同名同大小的文件直接复用，不重复下载；
//! 3. **不覆盖**：同名但大小不同（例如改了参数重跑）时加 `_rN` 后缀，旧图不丢。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::lovart::errors::LovartError;
use crate::lovart::media_type::{ext_from_mime, ext_from_url};

/// 单次下载超时（ms）。
const DOWNLOAD_TIMEOUT_MS: u64 = 60_000;

/// 同名但大小不同时的后缀尝试次数上限。
const MAX_REUSE_SUFFIX: usize = 50;

/// 取消检查粒度（ms）—— 与 `task.rs` 同口径。
const CANCEL_POLL_MS: u64 = 200;

/// 下载结果（对应 TS `DownloadResult`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadedFile {
    pub path: PathBuf,
    pub bytes: usize,
    /// `true` = 命中同名同大小文件，未重新下载。
    pub reused: bool,
}

/// 把提示词压成安全的文件名片段（对应 TS `slugify`）。
///
/// ⚠️ 必须与 TS 逐字符一致：文件名不同 ⇒ 同一张图会被下载两次、或旧图找不回来。
pub fn slugify(text: &str, max_len: usize) -> String {
    let mut cleaned = String::with_capacity(text.len());
    // ① 换行 / 制表 → 空格
    for ch in text.chars() {
        cleaned.push(if ch == '\r' || ch == '\n' || ch == '\t' {
            ' '
        } else {
            ch
        });
    }
    // ② 去掉文件系统非法字符
    let cleaned: String = cleaned
        .chars()
        .filter(|ch| !"\\/:*?\"<>|".contains(*ch))
        .collect();
    // ③ 连续空白 → 单下划线
    let mut squashed = String::with_capacity(cleaned.len());
    let mut in_space = false;
    for ch in cleaned.chars() {
        if ch.is_whitespace() {
            if !in_space {
                squashed.push('_');
                in_space = true;
            }
        } else {
            squashed.push(ch);
            in_space = false;
        }
    }
    // ④ trim（下划线不算 trim 目标 —— TS 的 trim 在③之后做，去掉的是空白）
    let trimmed = squashed.trim_matches(|ch: char| ch.is_whitespace());
    let result = if trimmed.is_empty() { "prompt" } else { trimmed };
    result.chars().take(max_len).collect()
}

/// 可被取消打断的睡眠。
fn sleep_or_abort(cancelled: &AtomicBool, total: Duration) -> Result<(), LovartError> {
    let mut remaining = total;
    let step = Duration::from_millis(CANCEL_POLL_MS);
    while remaining > Duration::ZERO {
        if cancelled.load(Ordering::SeqCst) {
            return Err(LovartError::transport("已取消"));
        }
        let chunk = remaining.min(step);
        std::thread::sleep(chunk);
        remaining -= chunk;
    }
    Ok(())
}

/// 下载字节流，带指数退避重试。
fn fetch_bytes(
    url: &str,
    cancelled: &AtomicBool,
    retries: u32,
    timeout: Duration,
) -> Result<(Vec<u8>, String), LovartError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|error| LovartError::transport(error.to_string()))?;

    let mut last_error = String::from("未知错误");
    for attempt in 1..=retries {
        match client.get(url).send() {
            Ok(response) => {
                if !response.status().is_success() {
                    last_error = format!("HTTP {}", response.status().as_u16());
                } else {
                    let content_type = response
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("")
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_lowercase();
                    match response.bytes() {
                        Ok(bytes) if !bytes.is_empty() => {
                            return Ok((bytes.to_vec(), content_type));
                        }
                        Ok(_) => last_error = "响应内容为空".to_string(),
                        Err(error) => last_error = error.to_string(),
                    }
                }
            }
            Err(error) => last_error = error.to_string(),
        }

        // 用户主动中断：不重试，立刻抛出。
        if cancelled.load(Ordering::SeqCst) {
            return Err(LovartError::transport(last_error));
        }
        if attempt < retries {
            sleep_or_abort(cancelled, Duration::from_millis(1000 * u64::from(attempt)))?;
        }
    }
    Err(LovartError::transport(last_error))
}

/// 下载单个产物到 `output_dir`，返回落盘路径。
///
/// 已存在同名同大小文件时直接复用；同名不同大小则加 `_rN` 后缀，**绝不覆盖已有图**。
pub fn download_artifact(
    url: &str,
    output_dir: &Path,
    file_base: &str,
    cancelled: &AtomicBool,
    retries: u32,
) -> Result<DownloadedFile, LovartError> {
    let retries = retries.max(1);
    let (bytes, content_type) = fetch_bytes(
        url,
        cancelled,
        retries,
        Duration::from_millis(DOWNLOAD_TIMEOUT_MS),
    )?;

    // URL 后缀优先（更贴近上游原意）；取不到再用 Content-Type；都没有才回退 png
    let ext = ext_from_url(url).unwrap_or_else(|| ext_from_mime(&content_type).to_string());

    std::fs::create_dir_all(output_dir)
        .map_err(|error| LovartError::transport(format!("建输出目录失败：{error}")))?;

    let mut target = output_dir.join(format!("{file_base}.{ext}"));
    for index in 1..=MAX_REUSE_SUFFIX {
        match std::fs::metadata(&target) {
            Ok(existing) if existing.len() as usize == bytes.len() => {
                return Ok(DownloadedFile {
                    path: target,
                    bytes: bytes.len(),
                    reused: true,
                });
            }
            Ok(_) => {
                target = output_dir.join(format!("{file_base}_r{index}.{ext}"));
            }
            Err(_) => break,
        }
    }

    std::fs::write(&target, &bytes)
        .map_err(|error| LovartError::transport(format!("写产物失败：{error}")))?;
    Ok(DownloadedFile {
        path: target,
        bytes: bytes.len(),
        reused: false,
    })
}
