//! `lovartConnection` — 连接解析（从环境变量 / `.env` 读凭据）。
//!
//! 对应 TS `lovart/lovart_connection.ts`。

use std::path::Path;

use super::client::{ClientDeps, ReqwestTransport};
use super::config::DEFAULT_BASE_URL;

/// 连接信息（对应 TS `LovartConnection`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    pub base_url: String,
    pub access_key: String,
    pub secret_key: String,
}

/// `.env` 的一条（对应 TS `DotEnvEntry`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotEnvEntry {
    pub key: String,
    pub value: String,
    /// 源文件行号（1 起）。
    pub line: usize,
}

/// 解析结果（对应 TS `ParsedDotEnv`）。
#[derive(Debug, Clone, Default)]
pub struct ParsedDotEnv {
    pub entries: Vec<DotEnvEntry>,
    /// 格式不正确的行（**不静默跳过** —— 配错了必须让人看见）。
    pub errors: Vec<String>,
}

/// 变量名是否合法：字母/下划线开头，后接字母数字下划线。
fn is_valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// 解析 `=` 右侧的值。
///
/// - 引号包裹（`"…"` / `'…'`）：整体取值，**内部保留 `#`**（`"a # b"` 就是 `a # b`）；
/// - 裸值：` #` 起算行尾注释；
/// - **引号没配平 ⇒ 报错**。旧版会把它当成值的一部分（`KEY="a" # 注释` 得到 `"a" # 注释`），
///   静默给出一个错值 —— 比报错危险得多。
fn parse_value(raw: &str, line_no: usize, errors: &mut Vec<String>) -> Option<String> {
    if raw.is_empty() {
        return Some(String::new());
    }
    let quote = raw.chars().next().unwrap_or(' ');
    if quote == '"' || quote == '\'' {
        let rest = &raw[quote.len_utf8()..];
        let Some(end) = rest.find(quote) else {
            errors.push(format!("第 {line_no} 行：{quote} 引号没有闭合"));
            return None;
        };
        let tail = rest[end + quote.len_utf8()..].trim();
        if !tail.is_empty() && !tail.starts_with('#') {
            errors.push(format!("第 {line_no} 行：引号闭合后还有多余内容「{tail}」"));
            return None;
        }
        return Some(rest[..end].to_string());
    }
    if raw.starts_with('#') {
        return Some(String::new());
    }
    match raw.find(" #") {
        Some(position) => Some(raw[..position].trim().to_string()),
        None => Some(raw.trim().to_string()),
    }
}

/// 解析 `.env` 文本 —— **纯函数**（I/O 留在 [`load_dot_env`]，这里才好测）。
///
/// ── 支持 ────────────────────────────────────────────────────────────
///   `KEY=VALUE` · `export KEY=VALUE` · 整行/行尾 `#` 注释 · 单/双引号包裹
pub fn parse_dot_env(text: &str) -> ParsedDotEnv {
    let mut parsed = ParsedDotEnv::default();
    for (index, raw_line) in text.split('\n').enumerate() {
        let line_no = index + 1;
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `export KEY=VALUE` 是常见写法（.env 常被 shell source）
        if let Some(rest) = line.strip_prefix("export ") {
            line = rest.trim();
        }

        let Some(eq) = line.find('=') else {
            parsed
                .errors
                .push(format!("第 {line_no} 行：不是 KEY=VALUE 形式 —— {}", raw_line.trim()));
            continue;
        };
        let key = line[..eq].trim();
        if !is_valid_key(key) {
            parsed.errors.push(format!(
                "第 {line_no} 行：变量名「{key}」不合法（字母/数字/下划线，不以数字开头）"
            ));
            continue;
        }
        let Some(value) = parse_value(line[eq + 1..].trim(), line_no, &mut parsed.errors) else {
            continue;
        };
        parsed.entries.push(DotEnvEntry {
            key: key.to_string(),
            value,
            line: line_no,
        });
    }
    parsed
}

/// 解析 `.env` 并注入进程环境 —— **唯一的实现**。
///
/// 「覆盖 / 不覆盖」由两个具名入口选择，见 [`load_dot_env`] / [`reload_dot_env`]。
///
/// 文件不存在 ⇒ 静默返回（用已有环境变量即可）。
/// 文件存在但**有格式错的行 ⇒ 报错**（不静默跳过）：配错了不报，
/// 用户只会看到「缺少 LOVART_ACCESS_KEY」这种离现场很远的提示。
fn inject_dot_env(path: &Path, override_existing: bool) -> Result<(), String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(()); // 没有 .env 就用已有环境变量
    };
    let parsed = parse_dot_env(&text);
    if !parsed.errors.is_empty() {
        return Err(format!(
            "{} 有 {} 处格式问题：\n  {}",
            path.display(),
            parsed.errors.len(),
            parsed.errors.join("\n  ")
        ));
    }
    for entry in parsed.entries {
        if override_existing || std::env::var_os(&entry.key).is_none() {
            std::env::set_var(&entry.key, &entry.value);
        }
    }
    Ok(())
}

/// **首次载入**：不覆盖已存在的变量。
///
/// 「不覆盖」是**刻意**的 —— dotenv 的标准语义是**真实环境变量优先于 `.env`**，
/// 所以 shell `export` / 容器注入的变量不会被文件盖掉。
///
/// ⚠️ 正因如此，**它不能拿来"重载"**：已经载入过一次的键会被**整段跳过**。
/// 需要「`.env` 刚被改写 ⇒ 立刻生效」的场合，用 [`reload_dot_env`]。
pub fn load_dot_env(path: &Path) -> Result<(), String> {
    inject_dot_env(path, false)
}

/// **重载**：覆盖已存在的变量。
///
/// 与 [`load_dot_env`] 只差这一处语义。**存在的唯一理由**是 GUI 的 `save_settings`：
/// 用户刚点过「保存」，`.env` 就是此刻的权威 —— 不覆盖 = 白重载
/// （文件改了、运行时还用旧密钥，DEBT **TD-05-11**）。
///
/// ⚠️ 别在启动路径上顺手用它：那会把外部环境变量盖掉，破坏 [`load_dot_env`] 的优先级语义。
pub fn reload_dot_env(path: &Path) -> Result<(), String> {
    inject_dot_env(path, true)
}

/// 解析连接：accessKey + secretKey + baseUrl。
pub fn resolve_connection() -> Result<Connection, String> {
    let access_key = std::env::var("LOVART_ACCESS_KEY").unwrap_or_default();
    let secret_key = std::env::var("LOVART_SECRET_KEY").unwrap_or_default();
    let access_key = access_key.trim().to_string();
    let secret_key = secret_key.trim().to_string();
    if access_key.is_empty() || secret_key.is_empty() {
        return Err(
            "缺少 LOVART_ACCESS_KEY / LOVART_SECRET_KEY\n请在项目根目录的 .env 中配置（可复制 .env.example）"
                .to_string(),
        );
    }
    let base_url = std::env::var("LOVART_BASE_URL")
        .unwrap_or_default()
        .trim()
        .to_string();
    let base_url = if base_url.is_empty() {
        DEFAULT_BASE_URL.to_string()
    } else {
        base_url
    };
    Ok(Connection {
        base_url: base_url.trim_end_matches('/').to_string(),
        access_key,
        secret_key,
    })
}

/// 由连接构造客户端依赖。
pub fn to_client_deps(connection: &Connection) -> Result<ClientDeps, String> {
    let transport = ReqwestTransport::new()?;
    Ok(ClientDeps::new(
        connection.base_url.clone(),
        connection.access_key.clone(),
        connection.secret_key.clone(),
        std::sync::Arc::new(transport),
    ))
}
