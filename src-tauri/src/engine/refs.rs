//! `refs` — 参考图（图生图）输入收口 + CDN 上传缓存。
//!
//! 对应 TS `src/refs.ts`。
//!
//! ── 支持的形态 ──────────────────────────────────────────────────────
//! - http(s) 公网 URL / 本机回环 → 原样下传，交给 `attachments` 处理；
//! - `data:` base64             → 原样下传；
//! - `file:` / 本机路径         → **本模块读盘**并转成 `data:` base64。
//!
//! 为什么本地路径要在这里读：`attachments` 明确不支持本机路径（拿不到文件内容，
//! 会直接判失败并阻断请求）。本模块只做「读盘 + 归一成 URL」，上传 CDN 仍由
//! `attachments` 负责，职责不重叠。
//!
//! ── 内容键与缓存 ────────────────────────────────────────────────────
//! 每个参考素材产出一个**跨运行稳定**的 `key`：
//!   - 本机文件：内容 sha1（**同一张图换路径也命中同一条**，这正是垫图去重的场景）；
//!   - `data:` / 裸 base64：内容 sha1；
//!   - 远程 URL：URL 本身。
//!
//! `key → CDN URL` 存在 `var/refs-cache.json` 里。垫图经常重复，命中缓存就不再上传。
//!
//! ⚠️ 刻意**不用**上传后的 CDN URL 做指纹 —— 那个每次上传都带随机串，
//! 会导致每轮都被判为「参考图变了」而无限重跑。签名只用 key，所以缓存丢了也只是重传。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use sha1::{Digest, Sha1};

use crate::lovart::media_type::mime_from_ext;

/// 单个参考素材体积上限（防手滑传巨大文件把内存吃满）。
///
/// `pub(crate)` 是因为 GUI 的「data URL 落盘」也要守同一条线 ——
/// 两处各写一个数就是两份会漂移的上限。
pub(crate) const MAX_REF_BYTES: u64 = 64 * 1024 * 1024;

/// 一条参考素材（对应 TS `RefEntry`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    /// 原始输入（本机路径 / URL / `data:`），用于回映射到具体任务。
    pub input: String,
    /// 跨运行稳定的指纹，同时用作缓存键。
    pub key: String,
    /// 交给 `attachments` 的形态（本机文件已转成 `data:` base64）。
    pub url: String,
    /// 这张素材对应的**本机文件路径**（读盘得到的才有；URL / `data:` 为 `None`）。
    ///
    /// 记录里存它作为「垫图真相」：CDN 地址会失效，本地文件不会。
    pub local_path: Option<String>,
}

/// `key → Lovart CDN URL`。
pub type RefsCache = BTreeMap<String, String>;

fn short_hash(bytes: &[u8]) -> String {
    let digest = Sha1::digest(bytes);
    hex::encode(digest)[..16].to_string()
}

/// `file:` URL 或相对路径 → 绝对路径。
fn resolve_local(input: &str, cwd: &Path) -> PathBuf {
    if let Some(rest) = input.strip_prefix("file://") {
        return PathBuf::from(rest);
    }
    if input.starts_with("file:") {
        return PathBuf::from(&input["file:".len()..]);
    }
    let path = Path::new(input);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn local_file_entry(input: &str, cwd: &Path) -> Result<RefEntry, String> {
    let abs_path = resolve_local(input, cwd);
    let metadata = std::fs::metadata(&abs_path)
        .map_err(|_| format!("参考图不存在或不是文件：{}", abs_path.display()))?;
    if !metadata.is_file() {
        return Err(format!("参考图不存在或不是文件：{}", abs_path.display()));
    }
    if metadata.len() > MAX_REF_BYTES {
        return Err(format!(
            "参考图过大（{:.1}MB > {}MB）：{}",
            metadata.len() as f64 / 1024.0 / 1024.0,
            MAX_REF_BYTES / 1024 / 1024,
            abs_path.display()
        ));
    }
    let bytes =
        std::fs::read(&abs_path).map_err(|error| format!("读参考图失败：{error}"))?;
    let ext = abs_path
        .extension()
        .map(|value| value.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mime = mime_from_ext(&ext);
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(RefEntry {
        input: input.trim().to_string(),
        // 内容哈希：同一张图换路径/换文件名也命中同一条缓存。
        key: format!("local:{}", short_hash(&bytes)),
        url: format!("data:{mime};base64,{encoded}"),
        local_path: Some(abs_path.display().to_string()),
    })
}

/// 把 `data:` 图片落成本机文件，返回绝对路径（同名同内容直接复用）。
///
/// **唯一实现**：GUI 的「粘贴/选择图片」与这里的 `data:` 输入都走它。
/// 文件名 = 内容 sha1：同一张图重复调用不重复写盘。
pub fn materialize_data_url(dir: &Path, data_url: &str) -> Result<String, String> {
    let (meta, payload) = data_url
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(','))
        .ok_or_else(|| "图片数据格式不对（缺少 data: 前缀或逗号）".to_string())?;
    if !meta.contains("base64") {
        return Err("图片数据不是 base64 编码".to_string());
    }
    let mime = meta.split(';').next().unwrap_or("image/png").to_lowercase();
    let ext = match mime.as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        other => return Err(format!("不支持的图片类型：{other}")),
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|error| format!("图片解码失败：{error}"))?;
    if bytes.len() as u64 > MAX_REF_BYTES {
        return Err(format!(
            "参考图过大（{:.1}MB > {}MB）",
            bytes.len() as f64 / 1024.0 / 1024.0,
            MAX_REF_BYTES / 1024 / 1024
        ));
    }
    std::fs::create_dir_all(dir).map_err(|error| format!("建垫图目录失败：{error}"))?;
    let hash = hex::encode(Sha1::digest(&bytes));
    let path = dir.join(format!("{}.{ext}", &hash[..16]));
    if !path.exists() {
        std::fs::write(&path, &bytes).map_err(|error| format!("保存垫图失败：{error}"))?;
    }
    Ok(path.display().to_string())
}

/// 把 `--ref` / 行内 `refs` 的输入归一成 `attachments` 可用的条目（含稳定 key）。
pub fn read_ref_inputs(inputs: &[String], cwd: &Path) -> Result<Vec<RefEntry>, String> {
    let mut entries: Vec<RefEntry> = Vec::new();
    for raw in inputs {
        // 空项由 `intake::tasks_from_inputs` 挡在前面；这里不再跳过
        // （跳过 = 少一张垫图照发，用户看不出来）。
        let input = raw.trim();
        let lower = input.to_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            entries.push(RefEntry {
                input: input.to_string(),
                key: input.to_string(),
                url: input.to_string(),
                local_path: None,
            });
            continue;
        }
        if input.starts_with("data:") {
            // `data:` 一律落成本机文件：记录里只存路径，发送时才上传。
            // 不落盘的话这条记录就没有"真相"，CDN 一过期就再也找不回来。
            let path = materialize_data_url(&cwd.join("var").join("refs"), input)?;
            entries.push(RefEntry {
                input: input.to_string(),
                key: format!("data:{}", short_hash(input.as_bytes())),
                url: input.to_string(),
                local_path: Some(path),
            });
            continue;
        }
        entries.push(local_file_entry(input, cwd)?);
    }
    Ok(entries)
}

/// 读缓存文件；不存在 / 损坏一律当空缓存（缓存不是真相来源，丢了只是重传）。
pub fn load_refs_cache(file: Option<&Path>) -> RefsCache {
    let Some(file) = file else {
        return RefsCache::new();
    };
    let Ok(text) = std::fs::read_to_string(file) else {
        return RefsCache::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// 写回缓存（失败不致命：只是下次要重传）。
pub fn save_refs_cache(file: Option<&Path>, cache: &RefsCache) {
    let Some(file) = file else {
        return;
    };
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(text) = serde_json::to_string_pretty(cache) else {
        return;
    };
    let _ = std::fs::write(file, format!("{text}\n"));
}
