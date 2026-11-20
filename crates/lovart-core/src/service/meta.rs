//! 出图模型清单 —— `--list-models` 与 GUI 下拉共用的**唯一**来源。
//!
//! ⚠️ 筛选 / 文本 / JSON **必须待在一处**。它曾经散成三份：筛选在 `models`、
//! JSON 投影在 `run`、列宽硬编码在 `print_models` —— 于是 GUI 的键名写成 `id`、
//! CLI 写成 `alias`，同一份数据两个键名。
//!
//! 不放进 `lovart/models.rs`：那个文件是 `lovart_config.ts` 的逐行镜像，
//! 往里塞展示函数会毁掉可比对性。这一块是**输出形态**，与 `build_status_json` 同类。

use serde_json::{json, Value};

use crate::lovart::models::{Category, MODEL_SPECS};

/// 出图模型清单 —— `(别名, 本名)`。**筛选只此一处**（`Category::Image`）。
fn image_models() -> Vec<(&'static str, &'static str)> {
    MODEL_SPECS
        .iter()
        .filter(|spec| spec.category == Category::Image)
        .map(|spec| (spec.id, spec.readable_name))
        .collect()
}

/// JSON 投影 —— `--list-models --json` 与 GUI 下拉**共用这一份**（键名只此一处）。
pub fn image_models_json() -> Vec<Value> {
    image_models()
        .into_iter()
        .map(|(alias, name)| json!({ "alias": alias, "name": name }))
        .collect()
}

/// 文本表格投影 —— `--list-models` 用。列宽**从数据算**，不写死。
pub fn image_models_text() -> String {
    let models = image_models();
    let width = models
        .iter()
        .map(|(alias, _)| alias.chars().count())
        .max()
        .unwrap_or(0);
    models
        .into_iter()
        .map(|(alias, name)| format!("  {alias:<width$} {name}"))
        .collect::<Vec<_>>()
        .join("\n")
}
