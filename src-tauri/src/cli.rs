//! 命令行解析 —— 对应 TS `src/cli.ts`。
//!
//! 单独成文件而不是塞在 `run.rs` 里：解析器必须是**纯函数**才可测（DEBT TD-11-01）。
//!
//! ── 支持 ────────────────────────────────────────────────────────────
//!   `--key value` · `--key=value` · 重复 key（收成数组）· `--` 终止符 · `-h`
//!
//! ── TS 旧版的真实缺陷（这里一并避免）────────────────────────────────
//!   1. **值不能以 `-` 开头** ⇒ `--ref --weird.png` 被当 flag 丢掉。
//!      用 `--ref=--weird.png`（`=` 形式无歧义）；
//!   2. 不支持 `--key=value`；
//!   3. **重复 key 互相覆盖** ⇒ 多个 `--ref` 只能再写一段单独扫 argv 的代码
//!      （同一件事两处实现）；
//!   4. **未知 key 静默忽略** ⇒ 拼错 `--concurreny` 毫无提示，还以为生效了；
//!   5. 不支持 `--` 终止符。

use std::collections::{BTreeMap, BTreeSet};

/// 解析结果（对应 TS `ParsedArgs`）。
#[derive(Debug, Clone, Default)]
pub struct ParsedArgs {
    /// key → 所有取值（按出现顺序）。未出现过的 key 不在表里。
    pub values: BTreeMap<String, Vec<String>>,
    /// 出现过、但没带值的 key（纯开关）。
    pub flags: BTreeSet<String>,
    /// `--` 之后的内容，以及任何不以 `-` 开头的 token。
    pub positional: Vec<String>,
    /// 解析问题（目前只有未知参数）。**不静默忽略** ——
    /// 调用方应报错退出，否则用户拼错参数还以为生效了。
    pub errors: Vec<String>,
}

/// 解析 argv（**不含**程序名本身）。
///
/// `known_keys` = 允许的参数名（不含 `--` 前缀）；未知的一律进 `errors`。
pub fn parse_args(argv: &[String], known_keys: &BTreeSet<String>) -> ParsedArgs {
    let mut args = ParsedArgs::default();
    let mut terminated = false;

    let mut index = 0usize;
    while index < argv.len() {
        let token = argv[index].as_str();
        index += 1;

        if terminated || !token.starts_with('-') || token == "-" {
            args.positional.push(token.to_string());
            continue;
        }
        if token == "--" {
            terminated = true;
            continue;
        }
        if token == "-h" {
            args.flags.insert("help".to_string());
            continue;
        }
        if !token.starts_with("--") {
            args.errors
                .push(format!("未知的短选项「{token}」（本工具只认 -h）"));
            continue;
        }

        let eq = token.find('=');
        let key = match eq {
            Some(position) => &token[2..position],
            None => &token[2..],
        };
        if !known_keys.contains(key) {
            args.errors.push(format!("未知参数「--{key}」"));
            continue;
        }

        // `--key=value`：值可以以 `-` 开头（传「像参数的值」的唯一无歧义写法）
        if let Some(position) = eq {
            args.values
                .entry(key.to_string())
                .or_default()
                .push(token[position + 1..].to_string());
            continue;
        }
        // `--key value`：值以 `-` 开头就视为下一个选项，本参数按纯开关处理
        match argv.get(index) {
            Some(next) if !next.starts_with('-') => {
                args.values
                    .entry(key.to_string())
                    .or_default()
                    .push(next.clone());
                index += 1;
            }
            _ => {
                args.flags.insert(key.to_string());
            }
        }
    }

    args
}

/// 该参数是否出现过（无论带不带值）。
pub fn has_arg(args: &ParsedArgs, key: &str) -> bool {
    args.flags.contains(key) || args.values.contains_key(key)
}

/// 取该参数的**最后一个**值；未给出、只给了开关、或值为空串时返回 `None`。
///
/// 「空串视为未给出」：`--key=$VAR` 在变量为空时会得到 `--key=`。
/// 这时回退默认值比拿空串去读文件更安全，也避免把「没填」悄悄变成 0 这个经典陷阱。
pub fn arg_value<'a>(args: &'a ParsedArgs, key: &str) -> Option<&'a str> {
    let value = args.values.get(key)?.last()?;
    if value.trim().is_empty() {
        return None;
    }
    Some(value)
}

pub fn arg_string(args: &ParsedArgs, key: &str, fallback: &str) -> String {
    arg_value(args, key).unwrap_or(fallback).to_string()
}

pub fn arg_number(args: &ParsedArgs, key: &str, fallback: f64) -> f64 {
    match arg_value(args, key).and_then(|value| value.parse::<f64>().ok()) {
        Some(parsed) if parsed.is_finite() => parsed,
        _ => fallback,
    }
}

/// 只在**显式给出且合法**时返回 `Some`；没写 / 写错都是 `None`。
///
/// 为什么需要它：`arg_number(args, key, 默认值)` 一定返回一个数，
/// 调用方为了拿默认值就得在 CLI 层再写一份默认值 —— 那正是
/// 「GUI 一套、CLI 一套」的重复来源（`docs/plan/07` R1-6）。
/// 用本函数 + `SchedulerTuning::from_settings()`，默认值只在设置层存在。
///
/// 写错（`--task-timeout abc`）会往 stderr 打一条告警：**不阻断**（与既有
/// `arg_number` 的宽容一致），但不再无声。
pub fn arg_number_opt(args: &ParsedArgs, key: &str) -> Option<f64> {
    let raw = args.values.get(key)?.last()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.parse::<f64>() {
        Ok(parsed) if parsed.is_finite() => Some(parsed),
        _ => {
            eprintln!("⚠️ --{key} 的值「{trimmed}」不是数字，已忽略（改用 .env / 内置默认）");
            None
        }
    }
}

/// 展平一个**可重复**的参数，并支持逗号分隔：`--ref a,b --ref c` → `['a','b','c']`。
///
/// 去空、去重（保序）—— 参考图重复给出没有意义，还会白白多算一次上传。
pub fn arg_list(args: &ParsedArgs, key: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in args.values.get(key).map(Vec::as_slice).unwrap_or(&[]) {
        for part in raw.split(',') {
            let trimmed = part.trim();
            if !trimmed.is_empty() && !out.iter().any(|existing| existing == trimmed) {
                out.push(trimmed.to_string());
            }
        }
    }
    out
}
