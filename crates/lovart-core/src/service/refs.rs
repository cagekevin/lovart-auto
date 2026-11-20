//! 参考图解析 —— CLI 与 GUI 共用的**唯一**实现。
//!
//! 原先在 `run.rs`（入口层），但 `gui` 的 `start_run` 也要用它 ⇒ 入口层成了业务库。
//! 搬进 `service` 后方向变成 `gui → service → engine::refs`（单向）。
//!
//! 只做「输入 → 来源（本机路径 / 公网 URL）」，**不在这里上传**：
//! 上传推迟到真正发送那一刻，命中 `var/refs-cache.json` 就不重复传。

use std::collections::BTreeMap;
use std::path::Path;

use crate::engine::refs::read_ref_inputs;
use crate::lovart::prompt::PromptTask;

/// 参考图解析的核心。`log` 由调用方给（CLI = 终端 + run.log；GUI = run.log）。
pub fn resolve_refs_core(
    tasks: &mut [PromptTask],
    cwd: &Path,
    cli_refs: &[String],
    log: &dyn Fn(&str),
) -> Result<(), String> {
    let cli_refs = cli_refs.to_vec();
    let mut needed: Vec<String> = cli_refs.clone();
    for task in tasks.iter() {
        for input in task.ref_inputs.clone().unwrap_or_default() {
            if !needed.iter().any(|existing| existing == &input) {
                needed.push(input);
            }
        }
    }
    if needed.is_empty() {
        return Ok(());
    }

    let entries = read_ref_inputs(&needed, cwd)?;
    let mut source_by_input: BTreeMap<String, String> = BTreeMap::new();
    for entry in &entries {
        // 公网 URL 上游能直接访问，原样留着；其余都在 `read_ref_inputs` 里落成了本机文件。
        let source = entry
            .local_path
            .clone()
            .unwrap_or_else(|| entry.input.clone());
        source_by_input.insert(entry.input.clone(), source);
    }

    let mut image_to_image = 0usize;
    for task in tasks.iter_mut() {
        let own = task.ref_inputs.clone().unwrap_or_default();
        let inputs = if own.is_empty() { cli_refs.clone() } else { own };
        if inputs.is_empty() {
            continue;
        }
        let mut files: Vec<String> = Vec::with_capacity(inputs.len());
        for input in &inputs {
            let Some(source) = source_by_input.get(input) else {
                return Err(format!("参考图没解析出地址（内部 bug）：{input}"));
            };
            files.push(source.clone());
        }
        task.ref_files = Some(files);
        image_to_image += 1;
    }

    log(&format!(
        "参考图：{} 个素材；图生图任务 {image_to_image}/{} 条",
        entries.len(),
        tasks.len()
    ));
    Ok(())
}
