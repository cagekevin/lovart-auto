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

/// 解析本机参考图输入 → 绝对路径；不存在 / 不是文件 / 过大 ⇒ `Err(原因)`。
///
/// 只做「定位 + 体检」，不读内容：草稿通道收到路径时用它确认"前端显示得出来"，
/// 发布链（[`local_file_entry`]）再接着读盘。两处共用这一条判据。
pub fn resolve_local_ref(input: &str, cwd: &Path) -> Result<PathBuf, String> {
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
    Ok(abs_path)
}

/// 本机文件 → `RefEntry`（读盘 + 内容哈希 + `data:` 编码 + 绝对路径）。
///
/// 与 [`read_ref_inputs`] 的区别：**只认本机文件**，不解析 `data:` / http(s)。
/// 给"手里已经是本机路径"的调用方用（scheduler 的上传链），
/// 不必为了一个路径去调通用入口、再塞两个假目录糊过去（ADR-0008）。
pub fn local_file_entry(input: &str, cwd: &Path) -> Result<RefEntry, String> {
    let abs_path = resolve_local_ref(input, cwd)?;
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

/// 图片扩展名白名单（小写）。**落盘与列目录共用这一条**：落得进去的图，
/// 素材库/预设也列得出来（两处各写一份清单，迟早一边多了 `.jpeg` 一边没有）。
pub const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif"];

/// 内容寻址目录的**目录名**（`data/var/refs`）。
///
/// ⚠️ 它同时是**命名规则**的一部分（见 [`naming_for`]）⇒ 拼这个目录的地方一律用本常量，
/// 不许另写 `"refs"` 字面量：路径与命名一错开，同一张 `data:` 图就会落成两种文件名
/// （一处纯哈希、一处可读名），而界面上看不出来（ADR-0002 判据 2）。
pub const CONTENT_DIR_NAME: &str = "refs";

/// 一张图的**来源**。两个消费方（`import_preset_images` 的落盘、`send_to_library` 的命令入参）
/// 共用这一个类型 ⇒ 落盘只有一个实现（`docs/plan/20` §2.5 S1）。
///
/// ⚠️ 来源**不决定文件名** —— 命名由目标目录定，见 [`materialize_image`]。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ImageSource {
    /// 本机路径（`C:\…\a.png` / `file:` URL）。
    Path { value: String },
    /// `data:` base64（剪贴板 / 文件选择器读出来的）。
    Data { value: String },
}

/// 落盘命名策略 —— **只由目标目录决定**，不作为参数暴露（暴露了调用方就能传错）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Naming {
    /// 纯内容哈希（`<sha1 前 16 位>.<ext>`）。
    Content,
    /// 可读名 + 短哈希（`<原名>-<sha1 前 8 位>.<ext>`）。
    Source,
}

/// 目录 → 命名策略。
///
/// 本仓只有 [`CONTENT_DIR_NAME`]（`data/var/refs`）是**内容寻址缓存**：
/// 那里已经落了一堆纯哈希文件，改成可读名等于让旧文件全部重落一份。
/// 其余（`data/presets` · `data/library`）是**用户看得见的目录**——文件名会进编辑区芯片，
/// 16 位哈希认不出来。
fn naming_for(dir: &Path) -> Naming {
    match dir.file_name() {
        Some(name) if name == CONTENT_DIR_NAME => Naming::Content,
        _ => Naming::Source,
    }
}

/// 一张图落进 `dir`，返回绝对路径（同内容同目录 ⇒ 同一个文件，不重写）。
///
/// **唯一落盘点**：`var/refs` 的 `data:` 垫图 · 预设图 · 素材库图都走它。
/// 命名由**目录**定（`refs` ⇒ 纯哈希，其余 ⇒ 原名+哈希）——同一个目录不会因为
/// "来源是剪贴板还是文件选择器"出现两种文件名。
pub fn materialize_image(dir: &Path, source: &ImageSource) -> Result<String, String> {
    let (bytes, ext, stem) = match source {
        ImageSource::Data { value } => {
            let (bytes, ext) = decode_data_url(value)?;
            (bytes, ext, None)
        }
        ImageSource::Path { value } => {
            let (bytes, ext, stem) = read_image_file(dir, value)?;
            (bytes, ext, Some(stem))
        }
    };
    let hash = hex::encode(Sha1::digest(&bytes));
    let file_name = match naming_for(dir) {
        Naming::Content => format!("{}.{ext}", &hash[..16]),
        Naming::Source => format!("{}-{}.{ext}", clean_stem(stem.as_deref()), &hash[..8]),
    };
    write_named_bytes(dir, &file_name, &bytes)
}

/// `data:` URL → (字节, 扩展名)。格式不对 / 不是图片 / 过大 ⇒ `Err(原因)`。
fn decode_data_url(data_url: &str) -> Result<(Vec<u8>, String), String> {
    // 前缀大小写不敏感（`DATA:` / `Data:` 都认）；base64 内容本身仍大小写敏感。
    let has_prefix = data_url
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"));
    if !has_prefix {
        return Err("图片数据格式不对（缺少 data: 前缀或逗号）".to_string());
    }
    let (meta, payload) = data_url[5..]
        .split_once(',')
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
    check_size(&bytes)?;
    Ok((bytes, ext.to_string()))
}

/// 本机图片文件 → (字节, 扩展名, 原名主干)。读不到 / 不是图片 / 过大 ⇒ `Err(原因)`。
///
/// 相对路径按 `dir` 解析（真实调用方都传绝对路径，这条只为保持旧行为）。
fn read_image_file(dir: &Path, value: &str) -> Result<(Vec<u8>, String, String), String> {
    let abs = resolve_local_ref(value, dir)?;
    let bytes = std::fs::read(&abs).map_err(|error| format!("读图片失败：{error}"))?;
    check_size(&bytes)?;
    let ext = abs
        .extension()
        .map(|value| value.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if !IMAGE_EXTS.contains(&ext.as_str()) {
        return Err(format!("不支持的图片类型：{ext}"));
    }
    // `jpeg` 归一成 `jpg`：同一张 JPEG 从剪贴板来是 `image/jpeg`→`jpg`、从文件管理器来是
    // `.jpeg` ⇒ 不归一就会在同一个目录落出两份（扩展名不同 = 文件名不同 = 去重失效）。
    let ext = if ext == "jpeg" { "jpg".to_string() } else { ext };
    let stem = abs
        .file_stem()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok((bytes, ext, stem))
}

/// 体积上限（`MAX_REF_BYTES`）：两种来源共用一条线，不各写一份。
fn check_size(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() as u64 > MAX_REF_BYTES {
        return Err(format!(
            "图片过大（{:.1}MB > {}MB）",
            bytes.len() as f64 / 1024.0 / 1024.0,
            MAX_REF_BYTES / 1024 / 1024
        ));
    }
    Ok(())
}

/// 原名主干 → 能落盘的可读名（Windows 非法字符 / 空白压成 `-`；空 ⇒ 「图片」）。
///
/// `data:` 来源没有文件名 ⇒ 落回「图片」（规则仍是 `原名-哈希.ext`，只是原名是这个词）。
fn clean_stem(stem: Option<&str>) -> String {
    let clean: String = stem
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let clean = clean.trim_matches('-');
    if clean.is_empty() { "图片".to_string() } else { clean.to_string() }
}

/// 落盘文件名 → **显示名**（`portrait-1a2b3c4d.png` ⇒ `portrait`）。
///
/// 文件名是 [`materialize_image`] 造的（原名 + 内容哈希尾巴），尾巴只服务于"同内容不重落"，
/// 不是名字，不该出现在界面上。
///
/// **这是这条规则的唯一实现**：前端 `refLabelOf` 是它在 JS 侧的逐字镜像
/// （两个运行时共用不了一份），改这里必须同步改那里 —— 两处各有一条用例锁同一组样例。
pub fn label_of(file: &str) -> String {
    let stem = Path::new(file)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    match stem.rsplit_once('-') {
        // `!head.is_empty()`：`-1234abcd` 这种没有主干的不能剥成空名字
        // （前端 `refLabelOf` 用 `named[1]` 做同一条判断，两处必须一致）。
        Some((head, tail))
            if !head.is_empty() && tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            head.to_string()
        }
        _ => stem,
    }
}

/// 内部唯一写盘点：同内容不重写（[`materialize_image`] 唯一调用方）。
fn write_named_bytes(dir: &Path, file_name: &str, bytes: &[u8]) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("建目录失败：{error}"))?;
    let path = dir.join(file_name);
    if !path.exists() {
        std::fs::write(&path, bytes).map_err(|error| format!("保存图片失败：{error}"))?;
    }
    Ok(path.display().to_string())
}

/// 把 `--ref` / 行内 `refs` 的输入归一成 `attachments` 可用的条目（含稳定 key）。
///
/// `refs_dir` = `data:` 的落盘目录，由**调用方**给（service 层按 `data/var/refs` 推导）
/// —— `engine` 不知道"数据根"在哪（ADR-0002：调用方不出现生产方的结构知识）。
pub fn read_ref_inputs(inputs: &[String], cwd: &Path, refs_dir: &Path) -> Result<Vec<RefEntry>, String> {
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
        if lower.starts_with("data:") {
            // `data:` 一律落成本机文件：记录里只存路径，发送时才上传。
            // 不落盘的话这条记录就没有"真相"，CDN 一过期就再也找不回来。
            let path = materialize_image(refs_dir, &ImageSource::Data { value: input.to_string() })?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 固定内容（`AAAA` = 3 个零字节）：两条用例要比"同一内容 ⇒ 同一哈希尾巴"。
    const DATA_PNG: &str = "data:image/png;base64,AAAA";

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lovart-refs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn data_source() -> ImageSource {
        ImageSource::Data { value: DATA_PNG.to_string() }
    }

    fn name_of(path: &str) -> String {
        Path::new(path)
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    /// `原名-哈希8.png` → (原名, 哈希8)。不是这个形状 ⇒ 直接失败（判据就在这）。
    fn split_readable(name: &str) -> (String, String) {
        let stem = name.rsplit_once('.').map(|(head, _)| head).unwrap_or(name);
        let (head, tail) = stem
            .rsplit_once('-')
            .unwrap_or_else(|| panic!("可读名必须是「原名-哈希」：{name}"));
        (head.to_string(), tail.to_string())
    }

    #[test]
    fn data_前缀大小写不敏感() {
        let dir = temp_dir("upper");
        let path = materialize_image(&dir, &ImageSource::Data { value: "DATA:image/png;base64,AAAA".to_string() })
            .expect("大写前缀也认");
        assert!(Path::new(&path).exists(), "落盘文件应存在：{path}");
        assert!(name_of(&path).ends_with(".png"), "扩展名应来自 mime：{path}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 缺_data_前缀时报错() {
        let dir = temp_dir("bad");
        let source = ImageSource::Data { value: "image/png;base64,AAAA".to_string() };
        assert!(materialize_image(&dir, &source).is_err());
    }

    /// `var/refs` 的文件名是纯 16 位哈希 —— 旧文件靠它去重，改名等于全部重落一份。
    #[test]
    fn refs_目录仍是纯十六位哈希() {
        let dir = temp_dir("hash").join("refs");
        for source in [
            data_source(),
            // 路径来源落进 refs 目录：命名**仍按目录**（不是按来源），所以也是纯哈希。
            ImageSource::Path { value: image_file("hash-src", "photo.png", b"\0\0\0") },
        ] {
            let path = materialize_image(&dir, &source).expect("应落盘");
            let name = name_of(&path);
            let stem = name.strip_suffix(".png").unwrap_or_else(|| panic!("扩展名不对：{name}"));
            assert_eq!(stem.len(), 16, "refs 里的名字必须是 16 位哈希：{name}");
            assert!(stem.bytes().all(|byte| byte.is_ascii_hexdigit()), "必须是纯哈希：{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S1 的核心判据：**同一个目录里，两种来源的文件名规则相同**（原名+哈希）。
    #[test]
    fn 同一目录两种来源文件名规则相同() {
        let dir = temp_dir("rules").join("library");
        let from_data = materialize_image(&dir, &data_source()).expect("data: 应落盘");
        // 同一个内容（3 个零字节）走路径来源 ⇒ 哈希尾巴必须一样，只有原名不同。
        let file = image_file("rules-src", "photo 1.png", b"\0\0\0");
        let from_path = materialize_image(&dir, &ImageSource::Path { value: file }).expect("路径应落盘");

        let (data_stem, data_hash) = split_readable(&name_of(&from_data));
        let (path_stem, path_hash) = split_readable(&name_of(&from_path));
        assert_eq!(data_stem, "图片", "data: 没有文件名 ⇒ 原名回落到「图片」");
        assert_eq!(path_stem, "photo-1", "原名里的非法字符被压成 `-`");
        assert_eq!(data_hash.len(), 8, "可读名的哈希尾巴是 8 位");
        assert_eq!(data_hash, path_hash, "同一内容在同目录必须得到同一条命名规则");
    }

    /// `.jpeg` 与 `image/jpeg` 必须落到同一个扩展名：否则同一张图在这个目录里有两份。
    #[test]
    fn jpeg_归一成_jpg_同一张图只有一份() {
        let dir = temp_dir("jpeg").join("library");
        let file = image_file("jpeg-src", "shot.jpeg", b"\0\0\0");
        let from_path =
            materialize_image(&dir, &ImageSource::Path { value: file }).expect("路径应落盘");
        let from_data = materialize_image(
            &dir,
            &ImageSource::Data { value: "data:image/jpeg;base64,AAAA".to_string() },
        )
        .expect("data: 应落盘");

        assert!(name_of(&from_path).ends_with(".jpg"), "jpeg 要归一成 jpg：{from_path}");
        assert!(name_of(&from_data).ends_with(".jpg"), "mime 支也应是 jpg：{from_data}");
        assert_eq!(
            split_readable(&name_of(&from_path)).1,
            split_readable(&name_of(&from_data)).1,
            "同一内容两条来源必须同一条命名"
        );
        // 两条来源只是原名不同（`shot` vs `图片`），扩展名与哈希尾巴一致 ⇒ 不会被当成两张图。
        assert_eq!(std::fs::read_dir(&dir).expect("列目录").count(), 2);
    }

    #[test]
    fn 同内容两次同一个文件() {
        let dir = temp_dir("twice").join("library");
        let first = materialize_image(&dir, &data_source()).expect("第一次应落盘");
        let second = materialize_image(&dir, &data_source()).expect("第二次应落盘");
        assert_eq!(first, second, "同内容不应重复写盘");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 路径来源不存在时报错() {
        let dir = temp_dir("missing").join("library");
        let source = ImageSource::Path { value: dir.join("没有这张.png").display().to_string() };
        assert!(materialize_image(&dir, &source).is_err());
    }

    /// 命令入参形状（`docs/plan/20` §3.4）：`{kind:'path'|'data', value}` ——
    /// 前端只按这个形状传，来源再多也不加第二个命令。
    #[test]
    fn 入参形状是_kind_加_value() {
        let data: ImageSource =
            serde_json::from_str(r#"{"kind":"data","value":"data:image/png;base64,AAAA"}"#)
                .expect("data 形状");
        assert!(matches!(data, ImageSource::Data { .. }));
        let path: ImageSource = serde_json::from_str(r#"{"kind":"path","value":"C:/a.png"}"#)
            .expect("path 形状");
        assert!(matches!(path, ImageSource::Path { .. }));
        // 没定义过的来源必须拒绝（不是"猜一个"）。
        assert!(serde_json::from_str::<ImageSource>(r#"{"kind":"url","value":"x"}"#).is_err());
    }

    /// 显示名剥掉 8 位哈希尾巴。前端 `refLabelOf` 是这条规则的 JS 镜像 ⇒ **样例必须同一组**
    /// （那边没有自动检查，改这里就得同步改 `ui/app.js`，否则芯片名又会出现哈希）。
    #[test]
    fn 显示名剥掉八位哈希尾巴() {
        assert_eq!(label_of("portrait-1a2b3c4d.png"), "portrait");
        assert_eq!(label_of("图片-1a2b3c4d.jpg"), "图片");
        // 不是 8 位十六进制 ⇒ 原样（用户自己的文件名不能被改）
        assert_eq!(label_of("a-b-1.png"), "a-b-1");
        assert_eq!(label_of("IMG_0001.png"), "IMG_0001");
        // `var/refs` 的纯哈希名没有可读主干 ⇒ 原样（前端另有「图片N」兜底）
        assert_eq!(label_of("03c438bdd6ac1234.png"), "03c438bdd6ac1234");
        // 没有主干 ⇒ 不许剥成空名字（与前端 `named[1]` 的判断一致）
        assert_eq!(label_of("-1a2b3c4d.png"), "-1a2b3c4d");
    }

    /// 造一张本机图，返回绝对路径。
    fn image_file(tag: &str, name: &str, bytes: &[u8]) -> String {
        let dir = temp_dir(tag);
        std::fs::create_dir_all(&dir).expect("建源目录");
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("写源图");
        path.display().to_string()
    }
}
