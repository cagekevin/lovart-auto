//! CSV 报告 —— 跑批结果的人读视图（带 BOM，Excel 打开中文不乱码）。
//!
//! 原先在 `run.rs`（入口层），而 `engine::scheduler` 反向调用 `crate::run::write_report`。
//! 拆 `lovart-core` 后入口层不在 core 里 ⇒ 反向依赖编译不过（E0433），
//! 因此下沉到 engine 内部；`run.rs` 后来已删除，这里是唯一实现。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::engine::state::TaskRecord;
use crate::lovart::prompt::PromptTask;

/// 记录里的产物路径 → **当前输出目录下能直接用的绝对路径**。
///
/// 记录里存的是**相对输出目录**的路径（例 `abc123_一只猫.png`）—— 理由见
/// `engine::scheduler` 里写 `files` 的那段注释：绝对路径把"文件夹叫什么、放在哪"
/// 钉进了数据。旧记录里是绝对路径 ⇒ **原样返回**（向后兼容；不替它猜搬到哪去了）。
pub fn resolve_file(output_dir: &Path, file: &str) -> PathBuf {
    let path = Path::new(file);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        output_dir.join(path)
    }
}

fn csv_cell(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// 写 CSV 报告（带 BOM，Excel 打开中文不乱码）。
pub fn write_report(
    file: &Path,
    records: &[TaskRecord],
    tasks_by_id: &BTreeMap<String, PromptTask>,
    output_dir: &Path,
) -> std::io::Result<()> {
    const HEADER: [&str; 11] = [
        "line",
        "id",
        "status",
        "mode",
        "model",
        "aspectRatio",
        "imageSize",
        "prompt",
        "files",
        "error",
        "updatedAt",
    ];
    let mut lines = vec![HEADER.join(",")];
    for record in records {
        let line = record
            .line
            .or_else(|| tasks_by_id.get(&record.id).map(|task| task.line))
            .map(|value| value.to_string())
            .unwrap_or_default();
        let mode = if tasks_by_id
            .get(&record.id)
            .and_then(|task| task.ref_files.as_ref())
            .is_some_and(|files| !files.is_empty())
        {
            "图生图"
        } else {
            "文生图"
        };
        let cells = [
            line,
            record.id.clone(),
            record.status.as_str().to_string(),
            mode.to_string(),
            record.model.clone(),
            record.aspect_ratio.clone(),
            record.image_size.clone(),
            record.prompt.clone(),
            // 报告给人看的 ⇒ 拼成绝对路径（记录里存的是相对路径，见 `resolve_file`）
            record
                .files
                .clone()
                .unwrap_or_default()
                .iter()
                .map(|item| resolve_file(output_dir, item).display().to_string())
                .collect::<Vec<_>>()
                .join(" | "),
            record.error.clone().unwrap_or_default(),
            record.updated_at.clone(),
        ];
        lines.push(
            cells
                .iter()
                .map(|cell| csv_cell(cell))
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    std::fs::write(file, format!("\u{feff}{}\n", lines.join("\n")))
}
