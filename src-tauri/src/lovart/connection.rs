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

/// 去掉行首空白与可选的 `export ` 前缀（`.env` 常被 shell source）。
///
/// **这是「一行怎么算声明」的唯一规则** —— `parse_dot_env` 与 [`update_env_keys`] 共用。
/// 各写一份的话，`export KEY=…` 在解析器里认、在写入器里不认 ⇒ 切换开关会**追加第二行**，
/// 而读取取第一条 ⇒ 写了等于没写，且每切一次多一行垃圾。
fn strip_export(raw_line: &str) -> &str {
    let line = raw_line.trim();
    match line.strip_prefix("export ") {
        Some(rest) => rest.trim(),
        None => line,
    }
}

/// 从一行里取变量名；不是声明行（空行 / 注释 / 无 `=` / 名字非法）⇒ `None`。
fn env_line_key(raw_line: &str) -> Option<&str> {
    let line = strip_export(raw_line);
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let eq = line.find('=')?;
    let key = line[..eq].trim();
    is_valid_key(key).then_some(key)
}

/// 解析 `.env` 文本 —— **纯函数**（I/O 留在 [`load_dot_env`]，这里才好测）。
///
/// ── 支持 ────────────────────────────────────────────────────────────
///   `KEY=VALUE` · `export KEY=VALUE` · 整行/行尾 `#` 注释 · 单/双引号包裹
pub fn parse_dot_env(text: &str) -> ParsedDotEnv {
    let mut parsed = ParsedDotEnv::default();
    for (index, raw_line) in text.split('\n').enumerate() {
        let line_no = index + 1;
        let line = strip_export(raw_line);
        if line.is_empty() || line.starts_with('#') {
            continue;
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

// ── 自动确认开关（全局）────────────────────────────────────────────────

/// 自动确认开关在 `.env` 里的键名 —— **唯一常量**，GUI / CLI / 测试都引用它。
pub const AUTO_CONFIRM_KEY: &str = "LOVART_AUTO_CONFIRM";

/// 只认 `true` / `false`（大小写不敏感）。其它值 ⇒ `None`（由调用方按关闭处理并告警）。
///
/// 花钱开关不猜：`1` / `yes` / 空 都不算开。
fn parse_env_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// 读 `.env` 里的自动确认开关 —— **GUI 与 CLI 共用的唯一读入口**。
///
/// 返回 `(值, 需要告知用户的问题)`：
/// - 文件不存在 / 没有这个键 ⇒ `(false, None)`（正常状态，不告警）；
/// - 值非法 / 读失败 ⇒ `(false, Some(文案))`（降级为关，但必须说出来）。
///
/// ⚠️ 不读进程环境变量：一个看不见的 `LOVART_AUTO_CONFIRM=1` 不能推翻用户在界面上点的开关。
pub fn read_auto_confirm(path: &Path) -> (bool, Option<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (false, None),
        // `load_dot_env` 对读失败是静默的（它把"读不出来"当成"没有 .env"），
        // 所以这条必须由开关这一层报出来，否则就是静默降级。
        Err(error) => {
            return (
                false,
                Some(format!(
                    "读不了 {}：{error}，自动确认按关闭处理",
                    path.display()
                )),
            )
        }
    };
    let parsed = parse_dot_env(&text);
    let Some(entry) = parsed
        .entries
        .iter()
        .find(|entry| entry.key == AUTO_CONFIRM_KEY)
    else {
        return (false, None);
    };
    match parse_env_bool(&entry.value) {
        Some(value) => (value, None),
        None => (
            false,
            Some(format!(
                "{AUTO_CONFIRM_KEY} 的值「{}」无法识别，已按关闭处理",
                entry.value
            )),
        ),
    }
}

/// 把一个值渲染成 `.env` 里安全的形式。
///
/// ⚠️ 不加引号直接写会**破坏下次解析**：`parse_value` 对未加引号的值执行
/// `find(" #")` 截断，且以 `#` 开头的值会被当成注释。输出目录填 `my #dir`
/// 就会在保存后被读成 `my`（`docs/plan/07` N9）。
///
/// 需要引号的场合：首尾空白、含 `#`、含引号。
/// 两种引号都出现时无法安全表示 ⇒ `Err`（宁可拒绝，不写坏文件）。
fn format_env_value(value: &str) -> Result<String, String> {
    let needs_quotes = value != value.trim()
        || value.contains('#')
        || value.contains('"')
        || value.contains('\'');
    if !needs_quotes {
        return Ok(value.to_string());
    }
    if !value.contains('"') {
        return Ok(format!("\"{value}\""));
    }
    if !value.contains('\'') {
        return Ok(format!("'{value}'"));
    }
    Err(format!(
        "值「{value}」同时包含单引号和双引号，无法安全写进 .env"
    ))
}

/// 按行更新 `.env` 里的若干键 —— **唯一写入口**（`set_auto_confirm` 与 `save_settings` 共用）。
///
/// 与旧版"解析成 entries 再整体重写"的区别：**注释、空行、其它键、`export` 前缀全部原样保留**。
/// 同名多行一律改成同一个值（不留歧义）；缺键则追加到末尾。
pub fn update_env_keys(path: &Path, updates: &[(&str, String)]) -> Result<(), String> {
    // 先把所有值渲染好：**任何一个写不出来，整次拒绝**，不写半截文件。
    let prepared: Vec<(&str, String)> = updates
        .iter()
        .map(|(key, value)| format_env_value(value).map(|rendered| (*key, rendered)))
        .collect::<Result<_, _>>()?;

    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("读不了 {}：{error}", path.display())),
    };
    let mut lines: Vec<String> = if text.is_empty() {
        Vec::new()
    } else {
        text.split('\n')
            .map(|line| line.trim_end_matches('\r').to_string())
            .collect()
    };

    for (key, value) in &prepared {
        let mut found = false;
        for line in &mut lines {
            if env_line_key(line).as_deref() != Some(*key) {
                continue;
            }
            let exported = line.trim_start().starts_with("export ");
            *line = if exported {
                format!("export {key}={value}")
            } else {
                format!("{key}={value}")
            };
            found = true;
        }
        if !found {
            // 与上一行隔一个空行（文件非空且末尾不是空行时）。
            if lines.last().is_some_and(|line| !line.is_empty()) {
                lines.push(String::new());
            }
            lines.push(format!("{key}={value}"));
        }
    }

    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    std::fs::write(path, out).map_err(|error| format!("写不了 {}：{error}", path.display()))
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
///
/// 轮询参数在这里从设置填入 —— `ClientDeps::new` 保持**纯构造**（不读环境），
/// 因为三处测试直接调它，读了 env 就依赖开发机的 `.env`。
pub fn to_client_deps(connection: &Connection) -> Result<ClientDeps, String> {
    let transport = ReqwestTransport::new()?;
    let mut deps = ClientDeps::new(
        connection.base_url.clone(),
        connection.access_key.clone(),
        connection.secret_key.clone(),
        std::sync::Arc::new(transport),
    );
    let (settings, _) = crate::engine::settings::Settings::from_env();
    deps.poll_interval = settings.poll_interval();
    deps.poll_max_interval = settings.poll_max_interval();
    Ok(deps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn temp_env(text: Option<&str>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lovart-env-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        if let Some(text) = text {
            std::fs::write(&path, text).unwrap();
        }
        path
    }

    fn text_of(path: &std::path::Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn e1_文件不存在_关闭且不告警() {
        assert_eq!(read_auto_confirm(&temp_env(None)), (false, None));
    }

    #[test]
    fn e2_缺键_关闭且不告警() {
        let path = temp_env(Some("LOVART_ACCESS_KEY=ak\n"));
        assert_eq!(read_auto_confirm(&path), (false, None));
    }

    #[test]
    fn e3_大小写都认() {
        assert_eq!(
            read_auto_confirm(&temp_env(Some("LOVART_AUTO_CONFIRM=true\n"))),
            (true, None)
        );
        assert_eq!(
            read_auto_confirm(&temp_env(Some("LOVART_AUTO_CONFIRM=FALSE\n"))),
            (false, None)
        );
    }

    #[test]
    fn e4_export_引号_空格都认() {
        let path = temp_env(Some("export LOVART_AUTO_CONFIRM = \"true\"\n"));
        assert_eq!(read_auto_confirm(&path), (true, None));
    }

    #[test]
    fn e5_非法值_关闭并告警() {
        let (value, warning) = read_auto_confirm(&temp_env(Some("LOVART_AUTO_CONFIRM=1\n")));
        assert!(!value);
        assert!(warning.unwrap().contains("无法识别"));
    }

    #[test]
    fn e6_读失败_关闭并告警() {
        let path = temp_env(None);
        std::fs::write(&path, [0xFFu8, 0xFE]).unwrap();
        let (value, warning) = read_auto_confirm(&path);
        assert!(!value);
        assert!(warning.unwrap().contains("读不了"));
    }

    #[test]
    fn e7_注释不算声明() {
        let path = temp_env(Some("# LOVART_AUTO_CONFIRM=true\n"));
        assert_eq!(read_auto_confirm(&path), (false, None));
    }

    #[test]
    fn e8_只改目标行_注释与其它键保留() {
        let path = temp_env(Some(
            "# 注释\nLOVART_ACCESS_KEY=ak\nLOVART_AUTO_CONFIRM=false\n",
        ));
        update_env_keys(&path, &[(AUTO_CONFIRM_KEY, "true".to_string())]).unwrap();
        assert_eq!(
            text_of(&path),
            "# 注释\nLOVART_ACCESS_KEY=ak\nLOVART_AUTO_CONFIRM=true\n"
        );
    }

    #[test]
    fn e9_export_前缀保留且不追加第二行() {
        let path = temp_env(Some("export LOVART_AUTO_CONFIRM=false\n"));
        update_env_keys(&path, &[(AUTO_CONFIRM_KEY, "true".to_string())]).unwrap();
        assert_eq!(text_of(&path), "export LOVART_AUTO_CONFIRM=true\n");
    }

    #[test]
    fn e10_等号带空格也原地替换() {
        let path = temp_env(Some("LOVART_AUTO_CONFIRM = false\n"));
        update_env_keys(&path, &[(AUTO_CONFIRM_KEY, "true".to_string())]).unwrap();
        assert_eq!(text_of(&path), "LOVART_AUTO_CONFIRM=true\n");
    }

    #[test]
    fn e11_同名多行全改() {
        let path = temp_env(Some(
            "LOVART_AUTO_CONFIRM=false\nLOVART_AUTO_CONFIRM=false\n",
        ));
        update_env_keys(&path, &[(AUTO_CONFIRM_KEY, "true".to_string())]).unwrap();
        assert_eq!(
            text_of(&path),
            "LOVART_AUTO_CONFIRM=true\nLOVART_AUTO_CONFIRM=true\n"
        );
    }

    #[test]
    fn e12_缺键只追加一次() {
        let path = temp_env(Some("LOVART_ACCESS_KEY=ak\n"));
        update_env_keys(&path, &[(AUTO_CONFIRM_KEY, "true".to_string())]).unwrap();
        update_env_keys(&path, &[(AUTO_CONFIRM_KEY, "false".to_string())]).unwrap();
        let text = text_of(&path);
        assert_eq!(
            text.matches("LOVART_AUTO_CONFIRM=").count(),
            1,
            "重复切换不许长行：{text}"
        );
        assert!(text.contains("LOVART_AUTO_CONFIRM=false"));
    }

    #[test]
    fn e13_注释里的键不改() {
        let path = temp_env(Some("# LOVART_AUTO_CONFIRM=true\n"));
        update_env_keys(&path, &[(AUTO_CONFIRM_KEY, "false".to_string())]).unwrap();
        let text = text_of(&path);
        assert!(text.starts_with("# LOVART_AUTO_CONFIRM=true\n"));
        assert!(text.contains("\nLOVART_AUTO_CONFIRM=false"));
    }

    #[test]
    fn e15_含井号的值加引号且能回读() {
        let path = temp_env(Some("LOVART_ACCESS_KEY=ak\n"));
        update_env_keys(&path, &[("LOVART_OUTPUT_DIR", "my #dir".to_string())]).unwrap();
        let parsed = parse_dot_env(&text_of(&path));
        let entry = parsed
            .entries
            .iter()
            .find(|entry| entry.key == "LOVART_OUTPUT_DIR")
            .expect("键应存在");
        assert_eq!(entry.value, "my #dir", "加引号后必须原样回读");
    }

    #[test]
    fn e16_无特殊字符不加引号() {
        let path = temp_env(Some("LOVART_ACCESS_KEY=ak\n"));
        update_env_keys(&path, &[("LOVART_OUTPUT_DIR", "output2".to_string())]).unwrap();
        assert!(text_of(&path).contains("LOVART_OUTPUT_DIR=output2"));
    }

    #[test]
    fn e17_两种引号都有则整次拒绝且不动文件() {
        let path = temp_env(Some("LOVART_ACCESS_KEY=ak\n"));
        let before = text_of(&path);
        let error = update_env_keys(&path, &[("LOVART_OUTPUT_DIR", "a'b\"c".to_string())])
            .expect_err("无法安全表示时必须拒绝");
        assert!(error.contains("引号"), "{error}");
        assert_eq!(text_of(&path), before, "拒绝时一个字节都不许改");
    }

    #[test]
    fn e14_前缀相似的键不误伤() {
        let path = temp_env(Some("LOVART_AUTO_CONFIRM_EXTRA=true\n"));
        update_env_keys(&path, &[(AUTO_CONFIRM_KEY, "false".to_string())]).unwrap();
        let text = text_of(&path);
        assert!(text.contains("LOVART_AUTO_CONFIRM_EXTRA=true"));
        assert!(text.contains("LOVART_AUTO_CONFIRM=false"));
    }
}
