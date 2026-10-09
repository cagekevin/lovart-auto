//! `prompts.jsonl` 解析 —— 对应 TS 的 `src/prompts.ts`。
//!
//! **提示词格式契约的唯一实现**（Rust 版必须逐条对齐）。
//!
//! ── 为什么是 JSONL，而不是自研 txt DSL ──────────────────────────────
//! 文件格式的真实职责是「**AI ↔ 工具**」的契约 —— 用户走 GUI（表格编辑），不读这个文件。
//! 既然是契约，就有**两个实现**（TS + Rust）：
//!   - 自研 DSL：要人肉对齐两套语义 ⇒ **必然分叉**；
//!   - JSONL：两边都用标准库解析（`JSON.parse` / `serde_json`）⇒ **结构上不可能分叉**。
//! 且 AI 输出 JSON 的可靠性远高于写自定义语法。
//!
//! ── 契约 ────────────────────────────────────────────────────────────
//! 一行一条 JSON 对象：
//! ```text
//! {"prompt": "一只戴帽子的猫，水彩风格"}
//! {"prompt": "赛博朋克城市夜景", "model": "nano-bn-pro", "ratio": "16:9"}
//! {"prompt": "同一个舞者，改成日出", "refs": ["refs/舞者.png"]}
//! ```
//!
//! | 字段 | 必填 | 类型 | 说明 |
//! |---|---|---|---|
//! | `prompt` | ✅ | string | 非空 |
//! | `model`  | — | string | 模型别名；未知别名 ⇒ **报错** |
//! | `ratio`  | — | string | `w:h` |
//! | `size`   | — | string | `1K` / `2K` / `4K` |
//! | `refs`   | — | string[] | 参考图（路径 / URL / `data:`），**相对 cwd**；非空数组 |
//!
//! 失败一律 **fail-loud**（不静默忽略、不取最后一个、不兜底默认值）：
//!   非法 JSON · 非对象 · 未知字段 · `prompt` 缺失/空 · 类型不符 · 未知模型 · 非法 ratio/size。
//! 空行跳过。行号从 1 起（对应文件真实行号，便于排错）。

use serde_json::{Map, Value};
use sha1::{Digest, Sha1};

use super::models::{
    is_known_image_model, model_spec, prompt_model_name, quality_model, resolve_prefer_models,
    Category,
};

/// 提示词的默认参数（对应 TS `PromptDefaults`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptDefaults {
    pub model: String,
    pub aspect_ratio: String,
    pub image_size: String,
}

impl Default for PromptDefaults {
    /// 与 `run.ts` 的 `DEFAULTS` 一致 —— 黄金用例不该依赖命令行默认值漂移。
    fn default() -> Self {
        Self {
            model: "nano-bn-2".to_string(),
            aspect_ratio: "3:4".to_string(),
            image_size: "2K".to_string(),
        }
    }
}

/// 一条待出图任务（对应 TS `PromptTask`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTask {
    /// 由**提示词文本**的 sha1 前 12 位决定（同文本重号加 `-2` / `-3` 后缀）。
    ///
    /// ⚠️ **只哈希文本**，不含 model / ratio / size / refs —— 所以改了这些参数
    /// **不会**换 id，必须靠调度器的「参数变更检测」重跑，否则会静默沿用旧参数的产物。
    /// 这条前提由 TS 的 `test/prompts.test.ts` 钉死，Rust 侧逐位对齐。
    pub id: String,
    pub prompt: String,
    pub model: String,
    pub aspect_ratio: String,
    pub image_size: String,
    /// 源文件里的行号（1 起），便于排错。
    pub line: u32,
    /// 本行 `refs` 声明的原始参考图输入（本机路径 / URL / `data:`），**相对 cwd**。
    /// 空 ⇒ 该行回退到命令行 `--ref`；两者都没有 ⇒ 文生图。
    pub ref_inputs: Option<Vec<String>>,
    /// 解析后的 Lovart CDN URL（由启动阶段填充）。
    pub refs: Option<Vec<String>>,
    /// 参考图指纹（由启动阶段填充），用于「换了参考图就重跑」的判定。
    pub ref_signature: Option<String>,
}

/// 解析结果（对应 TS `ParsedPrompts`）。
#[derive(Debug, Clone, Default)]
pub struct ParsedPrompts {
    pub tasks: Vec<PromptTask>,
    pub warnings: Vec<String>,
    /// 致命问题。调用方见到非空必须**终止启动**（不猜、不兜底）。
    pub errors: Vec<String>,
}

/// 解析选项（对应 TS `ParseOptions`）。
#[derive(Debug, Clone, Default)]
pub struct ParseOptions {
    /// 允许未知模型名。默认 `false`：未知模型会被当成**错误**并终止启动。
    ///
    /// 理由：未知别名在出图时会**静默退回默认模型**，几百条一起跑就是整批跑错。
    pub allow_unknown_model: bool,
}

/// 允许的字段。**新增字段必须同时更新 `README.md` 的「prompts.jsonl 格式」一节** ——
/// 那是契约的唯一真源（契约是「AI ↔ 工具」的接口，改了不写下来，AI 就会写错）。
const ALLOWED_KEYS: [&str; 5] = ["prompt", "model", "ratio", "size", "refs"];

/// `w:h`，w / h 各 1–3 位数字（等价 TS 的 `/^\d{1,3}:\d{1,3}$/`）。
fn is_valid_ratio(value: &str) -> bool {
    let Some((width, height)) = value.split_once(':') else {
        return false;
    };
    let digits = |part: &str| {
        !part.is_empty() && part.len() <= 3 && part.bytes().all(|byte| byte.is_ascii_digit())
    };
    digits(width) && digits(height)
}

/// 归一化尺寸档位（等价 TS 的 `/^(1K|2K|4K)$/i` + 大写归一）。
///
/// ⚠️ **2026-10-01：删掉 `720p`（与 TS 同步）。** 它是**高度**单位，而 `1K/2K/4K`
/// 是**宽度** —— 两种度量混在一起；上游对图片默认就是 `2K`。
/// 删前已核实 `fixtures/` 与 `state.jsonl` **无一处**用到它 ⇒ 不会让既有文件失效。
///
/// 三个档位全部大写，故归一化就是 `to_ascii_uppercase`（不再有小写特例）。
fn normalize_size(value: &str) -> Option<&'static str> {
    match value.to_ascii_uppercase().as_str() {
        "1K" => Some("1K"),
        "2K" => Some("2K"),
        "4K" => Some("4K"),
        _ => None,
    }
}

/// 提示词 id = `sha1(文本)` 十六进制前 12 位。
///
/// ⚠️ **必须是 sha1**：换成 sha256 会让全部 id 改变 ⇒ 已有 `state.jsonl` 全部失效
/// （断点续跑归零、整批重跑）。这是跨实现契约，不是安全选择。
fn prompt_hash(text: &str) -> String {
    let digest = Sha1::digest(text.as_bytes());
    hex::encode(digest)[..12].to_string()
}

/// 未知模型告警（**失败可见**）。
///
/// 模型既不在 specs、别名表也未命中时，模型选择会**静默失效** ——
/// 无 `tool_config`、prompt 也无模型硬约束，上游按默认模型出图且不报错。
/// 打点让「选了模型但后端不认识」立刻暴露，而不是默默用错模型。
fn warn_unknown_model(model_id: &str, phase: &str) {
    eprintln!(
        "[lovart] 未知模型「{model_id}」（{phase}）：specs 未登记且 IMAGE/VIDEO 别名表均未命中，\
         本次不下发 tool_config 且 prompt 无模型硬约束，上游将按默认模型生成。\
         请核对 lovart::models 的别名表。"
    );
}

/// 构造最终 prompt（对应 TS `buildLovartPrompt`）。
///
/// - `CHAT` / 未知模型：不拼前缀、不包裹，直接透传原文；
/// - `IMAGE`/`VIDEO`：前缀（图片只给 `resolution` 档位，比例经 `extra_params` 传）
///   + 生成指令句（内嵌 `using the {model} model` 硬约束）+ `<user_prompt>` 包裹的原文。
///
/// ⚠️ **尺寸只传档位标签，不传具体像素**：像素会被上游当硬约束，
/// 长边算大后超出免费额度会触发 `pending_confirmation`（需人工批准才扣积分）。
///
/// ⚠️ **同一件事只说一遍**（2026-10-01）：质量模型原先会同时发中文指令句与英文指令句，
/// 内容完全重复。现在只有英文那一句 —— 理由见 `tests/lovart_prompt.test.ts`。
pub fn build_lovart_prompt(
    model_id: &str,
    user_prompt: &str,
    size: Option<&str>,
    has_refs: bool,
    extra_params: &[String],
) -> String {
    let spec = model_spec(model_id);
    let Some(category) = spec.map(|spec| spec.category) else {
        warn_unknown_model(model_id, "prompt 路");
        return user_prompt.to_string();
    };
    // CHAT：透传原文，不拼前缀不包裹（对齐上游 chat 路径）。
    if category == Category::Chat {
        return user_prompt.to_string();
    }

    // 可读模型名：质量模型走派生表（base 去后缀）；否则查 `PROMPT_MODEL_NAMES`；
    // 都没有则回退 model 原串（对齐上游兜底行为，**不是** specs.readable_name）。
    let quality = quality_model(model_id);
    let model_name = quality
        .map(|(base, _)| base)
        .or_else(|| prompt_model_name(model_id))
        .unwrap_or(model_id);
    let model_clause = if model_name.is_empty() {
        String::new()
    } else {
        format!(" using the {model_name} model")
    };
    let quality_suffix = quality
        .map(|(_, quality)| format!(", quality {quality}"))
        .unwrap_or_default();

    // 尺寸/参数前缀：**只传档位标签 + 比例，不传具体像素**。
    let mut parts: Vec<String> = Vec::new();
    if category == Category::Image {
        if let Some(size) = size {
            if !size.is_empty() {
                parts.push(format!("resolution: {size}"));
            }
        }
    }
    for param in extra_params {
        // ⚠️ 必须先 trim 再判空：`"  "` 是有效字符串，直接 push 会拼出双逗号。
        let trimmed = param.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    }
    let prefix = parts.join(", ");

    let instr = match category {
        Category::Image => {
            // 注意：不能写 "Use reference and edit"，那会引导上游走 edit_media（改图）。
            let head = if has_refs {
                "Reference image attached. "
            } else {
                ""
            };
            format!("{head}Generate exactly ONE image{model_clause}{quality_suffix}.")
        }
        Category::Video => format!("Generate exactly ONE video{model_clause}."),
        Category::Chat => String::new(),
    };
    let head = match (prefix.is_empty(), instr.is_empty()) {
        (false, false) => format!("{prefix}\n{instr}"),
        (false, true) => prefix,
        (true, false) => instr,
        (true, true) => String::new(),
    };

    // 原文用 `<user_prompt>` 包裹并追加「原样使用」指令：
    // 让上游 Agent 明确知晓这是原文，须严格透传，不做改写或润色。
    let wrapped = format!(
        "<user_prompt>\n{user_prompt}\n</user_prompt>\n以上为用户提示词原文，直接使用，请勿修改"
    );
    if head.is_empty() {
        wrapped
    } else {
        format!("{head}\n{wrapped}")
    }
}

/// 按模型规格构造结构化路 `tool_config`（prompt_only 模型返回 `None`）。
pub fn build_lovart_tool_config(model_id: &str) -> Option<Value> {
    let spec = model_spec(model_id);
    // CHAT 不下发 —— 上游 `prefer_tool_categories` 的**值域只有 IMAGE / VIDEO**。
    if spec.is_some_and(|spec| spec.category == Category::Chat) {
        return None;
    }
    if let Some(spec) = spec {
        return resolve_prefer_models(model_id, spec.category)
            .map(|tool| tool_config_value(spec.category, tool));
    }
    // 未登记（别名变体、大小写/下划线差异）：两个 category 各试一遍，命中即下发。
    if let Some(tool) = resolve_prefer_models(model_id, Category::Image) {
        return Some(tool_config_value(Category::Image, tool));
    }
    if let Some(tool) = resolve_prefer_models(model_id, Category::Video) {
        return Some(tool_config_value(Category::Video, tool));
    }
    warn_unknown_model(model_id, "结构化路 tool_config");
    None
}

fn tool_config_value(category: Category, tool: &str) -> Value {
    let mut inner = Map::new();
    inner.insert(
        category.as_str().to_string(),
        Value::Array(vec![Value::String(tool.to_string())]),
    );
    let mut outer = Map::new();
    outer.insert(
        "prefer_tool_categories".to_string(),
        Value::Object(inner),
    );
    Value::Object(outer)
}

/// 解析 `prompts.jsonl`（对应 TS `parsePrompts`）。
pub fn parse_prompts(
    text: &str,
    defaults: &PromptDefaults,
    options: &ParseOptions,
) -> ParsedPrompts {
    let mut tasks: Vec<PromptTask> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    // 同哈希出现次数 → id 后缀（`-2` / `-3` …）
    let mut seen: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();

    for (index, line) in text.split('\n').enumerate() {
        let line_no = index + 1;
        let raw = line.trim();
        if raw.is_empty() {
            continue;
        }

        let parsed: Value = match serde_json::from_str(raw) {
            Ok(value) => value,
            Err(error) => {
                errors.push(format!("第 {line_no} 行：不是合法 JSON —— {error}"));
                continue;
            }
        };
        let Value::Object(object) = &parsed else {
            errors.push(format!("第 {line_no} 行：必须是 JSON 对象（一行一条）"));
            continue;
        };

        // ⚠️ 未知字段只**报错**，不 `continue` —— 这一行仍会产出任务。
        //    与 TS 同行为；不影响安全：调用方见到任何 errors 都会终止启动。
        //    （字段遍历顺序：TS 是插入序，Rust 的 `serde_json::Map` 是字典序 ——
        //     只影响 errors 的排列，不影响集合。errors 不参与状态对账。）
        for key in object.keys() {
            if !ALLOWED_KEYS.contains(&key.as_str()) {
                errors.push(format!(
                    "第 {line_no} 行：未知字段「{key}」；可用：prompt / model / ratio / size / refs"
                ));
            }
        }

        let Some(raw_prompt) = object.get("prompt").and_then(Value::as_str) else {
            errors.push(format!("第 {line_no} 行：prompt 必填，且必须是非空字符串"));
            continue;
        };
        if raw_prompt.trim().is_empty() {
            errors.push(format!("第 {line_no} 行：prompt 必填，且必须是非空字符串"));
            continue;
        }
        let prompt_text = raw_prompt.trim().to_string();

        let mut model = defaults.model.clone();
        if let Some(value) = object.get("model") {
            match value.as_str() {
                None => errors.push(format!("第 {line_no} 行：model 必须是非空字符串")),
                Some(text) if text.trim().is_empty() => {
                    errors.push(format!("第 {line_no} 行：model 必须是非空字符串"))
                }
                Some(text) => {
                    let value = text.trim().to_string();
                    if !is_known_image_model(&value) {
                        let message = format!(
                            "第 {line_no} 行：模型「{value}」不在别名表里，出图时会静默退回默认模型（{}）",
                            defaults.model
                        );
                        if options.allow_unknown_model {
                            warnings.push(message);
                        } else {
                            errors.push(message);
                        }
                    }
                    model = value;
                }
            }
        }

        let mut aspect_ratio = defaults.aspect_ratio.clone();
        if let Some(value) = object.get("ratio") {
            match value.as_str() {
                Some(text) if is_valid_ratio(text.trim()) => {
                    aspect_ratio = text.trim().to_string();
                }
                _ => errors.push(format!(
                    "第 {line_no} 行：ratio 必须是 \"w:h\" 形式，收到 {value}"
                )),
            }
        }

        let mut image_size = defaults.image_size.clone();
        if let Some(value) = object.get("size") {
            let normalized = value.as_str().and_then(|text| normalize_size(text.trim()));
            match normalized {
                Some(size) => image_size = size.to_string(),
                None => errors.push(format!(
                    "第 {line_no} 行：size 必须是 \"1K\" / \"2K\" / \"4K\" 之一，收到 {value}"
                )),
            }
        }

        let mut ref_inputs: Option<Vec<String>> = None;
        if let Some(value) = object.get("refs") {
            match value.as_array() {
                None => errors.push(format!("第 {line_no} 行：refs 必须是非空字符串数组")),
                Some(items) if items.is_empty() => {
                    errors.push(format!("第 {line_no} 行：refs 必须是非空字符串数组"))
                }
                Some(items) => {
                    let mut collected: Vec<String> = Vec::with_capacity(items.len());
                    let mut invalid = false;
                    for item in items {
                        match item.as_str() {
                            Some(text) if !text.trim().is_empty() => {
                                collected.push(text.trim().to_string())
                            }
                            _ => invalid = true,
                        }
                    }
                    if invalid {
                        errors.push(format!("第 {line_no} 行：refs 的元素必须是非空字符串"));
                    } else {
                        ref_inputs = Some(collected);
                    }
                }
            }
        }

        // id 用提示词内容哈希，改文件顺序或增删行不会打乱已有进度。
        // ⚠️ 只哈希文本，不含 model/ratio/size/refs —— 所以改了这些必须靠调度器的
        //    「参数变更检测」重跑，否则会静默沿用旧参数的产物。
        let hash = prompt_hash(&prompt_text);
        let occurrence = seen.entry(hash.clone()).or_insert(0);
        *occurrence += 1;
        let id = if *occurrence > 1 {
            format!("{hash}-{occurrence}")
        } else {
            hash
        };

        tasks.push(PromptTask {
            id,
            prompt: prompt_text,
            model,
            aspect_ratio,
            image_size,
            line: line_no as u32,
            ref_inputs,
            // 参考图解析（上传到 CDN + 算指纹）由启动阶段做，解析器不碰网络。
            refs: None,
            ref_signature: None,
        });
    }

    ParsedPrompts {
        tasks,
        warnings,
        errors,
    }
}
