//! `mediaType` — MIME ↔ 扩展名（**唯一一份**，跨层共用）。
//!
//! ── 为什么放这里 ────────────────────────────────────────────────────
//! 消费方横跨两层：`engine`（download · refs）与 `lovart`（attachments）。
//! 若放 `engine`，协议层就要反向依赖调度层 ⇒ 放**最低层** `lovart/`。
//! 本文件**零依赖**，是纯叶原语。
//!
//! ── 为什么是「一张表 + 两个派生视图」──────────────────────────────
//! TS 原先三份各写一张表（`download` / `refs` / `lovart_attachments`），
//! 子集各不相同（5 / 16 / 18 条）⇒ 必然漂移（DEBT TD-02-06）。
//! 现在只有 **CANONICAL 一张真源**，正反两个视图都由它派生。

/// 规范表：扩展名 ↔ MIME。**唯一真源**，两个方向都从它算。
const CANONICAL: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("webp", "image/webp"),
    ("gif", "image/gif"),
    ("bmp", "image/bmp"),
    ("avif", "image/avif"),
    ("svg", "image/svg+xml"),
    ("mp4", "video/mp4"),
    ("mov", "video/quicktime"),
    ("webm", "video/webm"),
    ("mp3", "audio/mpeg"),
    ("wav", "audio/wav"),
    ("m4a", "audio/mp4"),
    ("aac", "audio/aac"),
    ("flac", "audio/flac"),
    ("ogg", "audio/ogg"),
];

/// MIME 别名：同一个扩展名的其他写法（上游会混用）。
const MIME_ALIASES: &[(&str, &str)] = &[("image/jpg", "jpg"), ("audio/x-wav", "wav")];

/// MIME（可带 `data:` 前缀与 `;charset`）→ 扩展名（无点）。不认识回退 `png`。
pub fn ext_from_mime(mime: &str) -> &'static str {
    let trimmed = mime.trim();
    // 等价 TS 的 `/^data:/i` —— 大小写不敏感
    let without_scheme = match trimmed.get(..5) {
        Some(prefix) if prefix.eq_ignore_ascii_case("data:") => &trimmed[5..],
        _ => trimmed,
    };
    let bare = without_scheme
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();

    // 别名优先（`image/jpg` 与 `image/jpeg` 同义）
    for (alias, ext) in MIME_ALIASES {
        if bare == *alias {
            return ext;
        }
    }
    for (ext, canonical) in CANONICAL {
        if bare == *canonical {
            return ext;
        }
    }
    "png"
}

/// 扩展名（可带点）→ MIME。不认识回退 `image/png`。
pub fn mime_from_ext(ext: &str) -> &'static str {
    let bare = ext.trim_start_matches('.').to_lowercase();
    for (candidate, mime) in CANONICAL {
        if bare == *candidate {
            return mime;
        }
    }
    "image/png"
}

/// URL 路径 → 扩展名。
///
/// 取不到返回 `None` —— **诚实「不知道」**，由调用方决定兜底（不在这里冒充 png）。
pub fn ext_from_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let path = parsed.path();
    let last = path.rsplit('/').next().unwrap_or("");
    let (_, ext) = last.rsplit_once('.')?;
    // 等价 TS 的 `/\.([a-z0-9]{2,5})$/i`
    if (2..=5).contains(&ext.len()) && ext.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        Some(ext.to_lowercase())
    } else {
        None
    }
}
