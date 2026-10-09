//! `lovart_attachments` — 参考素材形态收口：把各种形态统一转成 Lovart 可用的 CDN URL。
//!
//! 对应 TS `lovart/lovart_attachments.ts`。
//!
//! ── 形态分流（对齐上游）────────────────────────────────────────────
//! - http(s) **公网** URL  → 直接透传（Lovart 服务器可访问）；
//! - http(s) **本机回环**  → 本机下载字节 → 上传 Lovart CDN
//!   （Lovart 访问不到用户本机端口）；
//! - `data:` base64      → 解析 header 得扩展名 → 解码 → 上传 CDN；
//! - 无前缀裸 base64      → 识别魔数 → 解码 → 上传 CDN；
//! - 其余（`blob:` / 本地路径 / 未知）→ 计入失败并留痕。
//!
//! ⚠️ **只要有真实素材失败即抛错阻断整条请求**（不部分成功继续）——
//! 否则会出现「prompt 声称有参考图、Lovart 却收不到」⇒ 图生图**静默退化成文生图**。

use base64::Engine;

use super::client::{upload_file, ClientDeps};
use super::errors::{ErrType, LovartError};
use super::media_type::ext_from_mime;

/// 常见媒体 base64 魔数前缀（无 `data:` 前缀的裸 base64）→ 扩展名。
const B64_MEDIA_MAGIC: &[(&str, &str)] = &[
    ("/9j/", "jpg"),
    ("iVBOR", "png"),
    ("R0lGOD", "gif"),
    ("UklGR", "webp"),
    ("Qk02", "bmp"),
    ("SUQz", "mp3"),
    ("SU5G", "m4a"),
    ("AAAA", "mp4"),
    ("GkXf", "webm"),
    ("Zkxh", "flac"),
    ("/e8/", "mp3"),
    ("TWFn", "m4a"),
];

/// 简短伪随机 hex（文件名后缀）。
///
/// 不引 `rand`：只需要"每次不一样"，不需要统计质量。
fn rand_hex() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    format!("{nanos:08x}")
}

/// 判断字符串是否可能是裸 base64 媒体数据（无 `data:` 前缀）。
fn looks_like_base64_media(text: &str) -> bool {
    if text.len() < 64 {
        return false;
    }
    if text.starts_with("http://")
        || text.starts_with("https://")
        || text.starts_with("data:")
        || text.starts_with("blob:")
    {
        return false;
    }
    B64_MEDIA_MAGIC
        .iter()
        .any(|(prefix, _)| text.starts_with(prefix))
}

/// 从裸 base64 魔数前缀推断扩展名；表外返回 `None`（诚实「不认识」，不静默当 png）。
fn ext_from_b64_magic(text: &str) -> Option<&'static str> {
    B64_MEDIA_MAGIC
        .iter()
        .find(|(prefix, _)| text.starts_with(prefix))
        .map(|(_, ext)| *ext)
}

fn base64_to_bytes(b64: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|error| error.to_string())
}

/// 解析 `data:` URL → 字节 + 扩展名。
fn bytes_from_data_url(data_url: &str) -> Result<(Vec<u8>, &'static str), String> {
    let comma = data_url.find(',').unwrap_or(0);
    let header = if data_url.contains(',') {
        &data_url[..comma]
    } else {
        ""
    };
    let b64 = if data_url.contains(',') {
        &data_url[comma + 1..]
    } else {
        ""
    };
    Ok((base64_to_bytes(b64)?, ext_from_mime(header)))
}

/// 是否本机回环 host（`127.0.0.1` / `localhost` / `0.0.0.0` / `[::1]`）。
fn is_loopback_hostname(host: &str) -> bool {
    let host = host.to_lowercase();
    host == "127.0.0.1" || host == "localhost" || host == "0.0.0.0" || host == "[::1]"
}

fn hostname_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()?
        .host_str()
        .map(str::to_string)
}

/// 是否「像本机路径」（`file:` / 盘符路径 / 以 `/` 开头的绝对路径）。
fn is_local_path_like(url: &str) -> bool {
    if url.starts_with("file:") {
        return true;
    }
    let bytes: Vec<char> = url.chars().take(3).collect();
    if bytes.first() == Some(&'/') {
        return true;
    }
    // `C:\` / `C:/`
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == ':'
        && (bytes[2] == '\\' || bytes[2] == '/')
}

/// 参考素材统一收口入口。
///
/// 返回 Lovart 可用的 URL 列表；无参考素材返回 `None`。
/// 存在真实下载/上传失败或形态不支持时抛 `UploadFailed`，**阻断整条请求**。
pub fn resolve_attachments(
    deps: &ClientDeps,
    urls: &[String],
) -> Result<Option<Vec<String>>, LovartError> {
    if urls.is_empty() {
        return Ok(None);
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(deps.request_timeout)
        .build()
        .map_err(|error| LovartError::transport(error.to_string()))?;

    let mut out: Vec<String> = Vec::new();
    let mut failed = 0usize;
    let mut last_error: Option<String> = None;

    for raw in urls {
        let url = raw.trim();
        if url.is_empty() {
            continue;
        }

        // 1) http(s)
        if url.starts_with("http://") || url.starts_with("https://") {
            let loopback = hostname_of(url).is_some_and(|host| is_loopback_hostname(&host));
            if !loopback {
                // 1b) 外网 URL：直接透传。
                out.push(url.to_string());
                continue;
            }
            // 1a) 本机回环：必须本地下载后转 CDN。
            match client.get(url).send() {
                Ok(response) if response.status().is_success() => {
                    let content_type = response
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    match response.bytes() {
                        Ok(bytes) => {
                            let ext = ext_from_mime(&content_type);
                            match upload_file(
                                deps,
                                &bytes,
                                &format!("_local_{}.{ext}", rand_hex()),
                            ) {
                                Ok(cdn) => out.push(cdn),
                                Err(error) => {
                                    failed += 1;
                                    last_error = Some(error.message().to_string());
                                }
                            }
                        }
                        Err(error) => {
                            failed += 1;
                            last_error = Some(error.to_string());
                        }
                    }
                }
                Ok(response) => {
                    failed += 1;
                    last_error = Some(format!(
                        "下载本机回环参考图失败 ({})",
                        response.status().as_u16()
                    ));
                }
                Err(error) => {
                    failed += 1;
                    last_error = Some(error.to_string());
                }
            }
            continue;
        }

        // 2) data: base64 → 上传 CDN
        if url.starts_with("data:") {
            match bytes_from_data_url(url).and_then(|(bytes, ext)| {
                upload_file(deps, &bytes, &format!("_ref_{}.{ext}", rand_hex()))
                    .map_err(|error| error.message().to_string())
            }) {
                Ok(cdn) => out.push(cdn),
                Err(message) => {
                    failed += 1;
                    last_error = Some(message);
                }
            }
            continue;
        }

        // 3) 无前缀裸 base64 → 识别魔数后上传 CDN
        if looks_like_base64_media(url) {
            let Some(ext) = ext_from_b64_magic(url) else {
                failed += 1;
                let clip: String = url.chars().take(24).collect();
                last_error = Some(format!(
                    "裸 base64 素材的魔数不在已知媒体表（无法判定格式），未上传：{clip}…"
                ));
                continue;
            };
            match base64_to_bytes(url).and_then(|bytes| {
                upload_file(deps, &bytes, &format!("_ref_{}.{ext}", rand_hex()))
                    .map_err(|error| error.message().to_string())
            }) {
                Ok(cdn) => out.push(cdn),
                Err(message) => {
                    failed += 1;
                    last_error = Some(message);
                }
            }
            continue;
        }

        // 4) 其余（blob: / 本地路径 / 未知格式）：拿不到内容 ⇒ 不能上传，但必须计入失败并留痕。
        let clip: String = url.chars().take(40).collect();
        last_error = Some(if url.starts_with("blob:") {
            "前端临时预览地址（blob:），取不到内容 —— 请先保存/上传该图片再引用".to_string()
        } else if is_local_path_like(url) {
            format!("本机文件路径，未转成公网地址：{clip}")
        } else {
            format!("未知素材形态，无法转成 Lovart 可用 URL：{clip}")
        });
        failed += 1;
    }

    if failed > 0 {
        return Err(LovartError::internal(
            format!(
                "有 {failed} 个参考素材无法用于本次生成（上传失败或形态不支持），已中止请求。\
                 请确认网络可用后重试。详情: {}",
                last_error.unwrap_or_else(|| "unknown".to_string())
            ),
            ErrType::UploadFailed,
        ));
    }
    if out.is_empty() {
        return Ok(None);
    }
    Ok(Some(out))
}
