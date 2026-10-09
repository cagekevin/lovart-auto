//! 模型别名表 —— 对应 TS 的 `lovart/lovart_config.ts`（模型相关部分）。
//!
//! 这是**转发映射，不是业务逻辑**：仅把前端 model 名翻译为 Lovart 工具名。
//! 新增模型只需在表里追加一条，**不新增分支**。
//!
//! ⚠️ 本表是【人工精选子集】，刻意不全（不用的模型不进表）。
//!
//! ⚠️ 时机常量（轮询间隔 / 任务总超时 / 请求超时）**不在本文件** ——
//! 它们归 `lovart::client`（增量 3）。现在建一个没人用的常量就是「幽灵预留」。
//!
//! ⚠️ `IMAGE_RULES` / `VIDEO_RULES` **必须整表移植**，不能只搬 IMAGE 那半张：
//! 只搬一半会让 `resolve_prefer_models(model, Video)` 恒返回 `None` ——
//! 那不是"还没做"，是**一个会静默出错的假能力**（视频模型会被当未知模型处理）。

/// 模型类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    Image,
    Video,
    Chat,
}

impl Category {
    /// 与 TS `LovartCategory` 的字面量一致。
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Image => "IMAGE",
            Category::Video => "VIDEO",
            Category::Chat => "CHAT",
        }
    }
}

/// 模型规格（对应 TS `LovartModelSpec`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelSpec {
    /// 模型 id（表键）。
    pub id: &'static str,
    pub category: Category,
    /// 结构化路选模型用的工具名；`None` ⇒ 仅自然语言路（prompt_only）。
    pub tool: Option<&'static str>,
    pub readable_name: &'static str,
}

/// 模型 id → 规格。**顺序与 TS 的 `LOVART_MODEL_SPECS` 一致**（便于逐行比对）。
pub const MODEL_SPECS: &[ModelSpec] = &[
    ModelSpec {
        id: "gpt-image-2-low",
        category: Category::Image,
        tool: None,
        readable_name: "GPT Image 2 Low",
    },
    ModelSpec {
        id: "gpt-image-2-medium",
        category: Category::Image,
        tool: None,
        readable_name: "GPT Image 2 Medium",
    },
    ModelSpec {
        id: "gpt-image-2-high",
        category: Category::Image,
        tool: None,
        readable_name: "GPT Image 2 High",
    },
    ModelSpec {
        id: "nano-bn-pro",
        category: Category::Image,
        tool: None,
        readable_name: "Nano Banana Pro",
    },
    ModelSpec {
        id: "nano-bn-2",
        category: Category::Image,
        tool: None,
        readable_name: "Nano Banana 2",
    },
    ModelSpec {
        id: "nano-bn-2-lite",
        category: Category::Image,
        tool: None,
        readable_name: "Nano Banana 2 Lite",
    },
    ModelSpec {
        id: "gpt-image-2.5-sunburst-low",
        category: Category::Image,
        tool: None,
        readable_name: "GPT Image 2.5 Sunburst Low",
    },
    ModelSpec {
        id: "gpt-image-2.5-sunburst-medium",
        category: Category::Image,
        tool: None,
        readable_name: "GPT Image 2.5 Sunburst Medium",
    },
    ModelSpec {
        id: "gpt-image-2.5-flare-low",
        category: Category::Image,
        tool: None,
        readable_name: "GPT Image 2.5 Flare Low",
    },
    ModelSpec {
        id: "gpt-image-2.5-flare-medium",
        category: Category::Image,
        tool: None,
        readable_name: "GPT Image 2.5 Flare Medium",
    },
    ModelSpec {
        id: "lovart-chat",
        category: Category::Chat,
        tool: Some("lovart-chat"),
        readable_name: "Lovart Chat",
    },
    ModelSpec {
        id: "seedance-2.0-fast",
        category: Category::Video,
        tool: None,
        readable_name: "Seedance 2.0 Fast",
    },
    ModelSpec {
        id: "seedance-2",
        category: Category::Video,
        tool: None,
        readable_name: "Seedance 2",
    },
    ModelSpec {
        id: "seedance-2.0-mini",
        category: Category::Video,
        tool: None,
        readable_name: "Seedance 2.0 Mini",
    },
    ModelSpec {
        id: "minimax-h3",
        category: Category::Video,
        tool: None,
        readable_name: "MiniMax H3",
    },
    ModelSpec {
        id: "kling-v3-omni",
        category: Category::Video,
        tool: None,
        readable_name: "Kling V3 Omni",
    },
];

/// 按 id 查规格（表只有 16 条，线性扫描足够，不引 map 依赖）。
pub fn model_spec(id: &str) -> Option<&'static ModelSpec> {
    MODEL_SPECS.iter().find(|spec| spec.id == id)
}

/// 别名规则：`(别名数组, 工具名)`。工具名为空串 = 官方暂无对应生成工具，
/// 仅作提示词驱动（不下发无效工具名）。
pub type AliasRules = &'static [(&'static [&'static str], &'static str)];

/// IMAGE 别名表。
///
/// ⚠️ **顺序敏感**：匹配用「子串包含」（`any(k in m)`），故更特异的别名必须排在更短的之前。
/// 例如 `gpt-image-2-low` 必须排在 `gpt-image-2` 之前，否则 low 会被 `gpt-image-2` 误吞。
pub const IMAGE_RULES: AliasRules = &[
    (
        &["gpt-image-2-low", "gpt-image2-low", "gptimage2low"],
        "generate_image_gpt_image_2",
    ),
    (
        &["gpt-image-2-medium", "gpt-image2-medium", "gptimage2medium"],
        "generate_image_gpt_image_2",
    ),
    // 必须在 `gpt-image-2` 之前：`gpt-image-2.5-*` 含 `gpt-image-2` 子串。
    // 工具名已去质量后缀 —— 质量经 prompt『质量 X』子句独立下发（对齐 Lovart：
    // tool_hint = 基础工具名，quality = 独立档位）。
    (
        &[
            "gpt-image-2.5-sunburst-low",
            "gpt-image2-5-sunburst-low",
            "gptimage2-5sunburstlow",
            "gpt image 2.5 sunburst low",
        ],
        "generate_image_gpt_image_2_5_sunburst",
    ),
    (
        &[
            "gpt-image-2.5-sunburst-medium",
            "gpt-image2-5-sunburst-medium",
            "gptimage2-5sunburstmedium",
            "gpt image 2.5 sunburst medium",
        ],
        "generate_image_gpt_image_2_5_sunburst",
    ),
    (
        &[
            "gpt-image-2.5-flare-low",
            "gpt-image2-5-flare-low",
            "gptimage2-5flarelow",
            "gpt image 2.5 flare low",
        ],
        "generate_image_gpt_image_2_5_flare",
    ),
    (
        &[
            "gpt-image-2.5-flare-medium",
            "gpt-image2-5-flare-medium",
            "gptimage2-5flaremedium",
            "gpt image 2.5 flare medium",
        ],
        "generate_image_gpt_image_2_5_flare",
    ),
    (
        &["gpt-image-2", "gpt-image2", "gptimage2"],
        "generate_image_gpt_image_2",
    ),
    (
        &["nano-bn-pro", "nano bn pro", "nanobnpro"],
        "generate_image_nano_banana_pro",
    ),
    // 顺序修正（相对上游 main.py 的缺陷修复）：`nano-bn-2` 是 `nano-bn-2-lite` 的前缀子串，
    // 若把 `nano-bn-2` 排在前面，lite 会被误吞成 generate_image_nano_banana_2，
    // 导致本行空串规则永远不可达。故 lite 必须排在 `nano-bn-2` 之前。
    (
        &["nano-bn-2-lite", "nano banana 2 lite", "nanobn2lite"],
        "",
    ),
    (
        &["nano-bn-2", "nano bn 2"],
        "generate_image_nano_banana_2",
    ),
];

/// VIDEO 别名表（顺序敏感，理由同上）。
pub const VIDEO_RULES: AliasRules = &[
    (
        &[
            "seedance-2.0-fast",
            "seedance-v2-0-fast",
            "seedance 2.0 fast",
            "seedance_2_fast",
            "seedance-2-fast",
            "seedance 2 fast",
        ],
        "generate_video_seedance_v2_0_fast",
    ),
    (
        &["kling-v3-omni", "kling-3-omni", "kling 3 omni"],
        "generate_video_kling_v3_omni",
    ),
    // 顺序修正：`seedance-2` 是 `seedance-2.0-mini` 的前缀子串，mini 必须排在前面。
    (
        &[
            "seedance-2.0-mini",
            "seedance-v2-0-mini",
            "seedance 2.0 mini",
            "seedance_2_mini",
            "seedance-2-mini",
            "seedance 2 mini",
        ],
        "",
    ),
    (
        &["seedance-2", "seedance2", "seedance-v2", "seedance 2"],
        "generate_video_seedance_v2_0",
    ),
    (&["minimax-h3", "minimax h3", "hailuo h3"], ""),
];

/// 把 model 名翻译为 Lovart 工具名（对齐 TS `resolvePreferModels`）。
///
/// - 归一化：`lower()` + `_` → `-`（如 `GPT_Image_2_Low` 也能命中）
/// - 别名模糊匹配：子串包含，按顺序命中即返回（规则表顺序敏感）
/// - 工具名为空串 ⇒ 官方暂无对应生成工具，返回 `None`（**不下发无效工具名**）
/// - `Chat` 类别**恒返回 `None`** —— 上游 `prefer_tool_categories` 的值域只有 IMAGE / VIDEO
pub fn resolve_prefer_models(model: &str, category: Category) -> Option<&'static str> {
    if model.is_empty() {
        return None;
    }
    let rules = match category {
        Category::Image => IMAGE_RULES,
        Category::Video => VIDEO_RULES,
        Category::Chat => return None,
    };
    let normalized = model.to_lowercase().replace('_', "-");
    for (aliases, tool) in rules {
        if aliases.iter().any(|alias| normalized.contains(alias)) {
            return if tool.is_empty() { None } else { Some(tool) };
        }
    }
    None
}

/// 质量模型派生表：模型 id → (基础可读名 base, 质量档位 quality)。
///
/// 设计：模型 id 是【单一数据源】，prompt 与 tool_config 都从这里派生，
/// **不新增 quality 轴、不拆模型**。
///   - `base`：去掉 id 里的 `-low`/`-medium`/`-high` 后缀（Lovart 真实工具名不带质量后缀）；
///   - `quality`：独立档位，prompt 里单独成句，`tool_config` 发基础工具名。
pub const QUALITY_MODELS: &[(&str, &str, &str)] = &[
    ("gpt-image-2-low", "GPT Image 2", "low"),
    ("gpt-image-2-medium", "GPT Image 2", "medium"),
    ("gpt-image-2-high", "GPT Image 2", "high"),
    ("gpt-image-2.5-sunburst-low", "GPT Image 2.5 Sunburst", "low"),
    (
        "gpt-image-2.5-sunburst-medium",
        "GPT Image 2.5 Sunburst",
        "medium",
    ),
    ("gpt-image-2.5-flare-low", "GPT Image 2.5 Flare", "low"),
    ("gpt-image-2.5-flare-medium", "GPT Image 2.5 Flare", "medium"),
];

/// 查质量档位。返回 `(base, quality)`。
pub fn quality_model(id: &str) -> Option<(&'static str, &'static str)> {
    let key = id.trim().to_lowercase();
    QUALITY_MODELS
        .iter()
        .find(|(candidate, _, _)| *candidate == key)
        .map(|(_, base, quality)| (*base, *quality))
}

/// 提示词可读模型名：前端可能传内部代号（如 `nano-bn-2-lite`），
/// 但拼进 prompt 时必须用上游 AI 能识别的**官方可读名**，否则 Agent 看不懂。
///
/// ⚠️ GPT Image 2.5 的质量模型**不在这张表里** —— 它们的可读名（去后缀）+ 质量档位
/// 由 [`QUALITY_MODELS`] 统一派生，避免重复真相源。
pub const PROMPT_MODEL_NAMES: &[(&str, &str)] = &[
    ("nano-bn-pro", "Nano Banana Pro"),
    ("nano-bn-2", "Nano Banana 2"),
    ("nano-bn-2-lite", "Nano Banana 2 Lite"),
    ("seedance-2.0-mini", "Seedance 2.0 mini"),
    ("minimax-h3", "MiniMax H3"),
];

/// 查可读模型名（大小写不敏感）。
pub fn prompt_model_name(id: &str) -> Option<&'static str> {
    let key = id.trim().to_lowercase();
    PROMPT_MODEL_NAMES
        .iter()
        .find(|(candidate, _)| *candidate == key)
        .map(|(_, name)| *name)
}

/// 该模型名能否被解析为 IMAGE 工具（对应 TS `isKnownImageModel`）。
///
/// ⚠️ 存在的理由：未知别名在出图时会**静默退回默认模型**，几百条一起跑就是整批跑错。
/// 所以 `parse_prompts` 默认把未知模型当**错误**终止启动，而不是警告。
pub fn is_known_image_model(model: &str) -> bool {
    if model_spec(model).is_some_and(|spec| spec.category == Category::Image) {
        return true;
    }
    resolve_prefer_models(model, Category::Image).is_some()
}
