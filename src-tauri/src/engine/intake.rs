//! 投喂与建队 —— 记录**创建 / 参数同步 / 入队**的唯一起点。
//!
//! ── 为什么单独一个模块 ──────────────────────────────────────────────
//! 这件事原先内联在 `run_scheduler` 开头，只有「开跑一批」能做。常驻调度器之后，
//! 运行中追加、重跑、接着查、确认都要做**同一件事**（查重 → 参数变更检测 → 写记录
//! → 入队）。写在两个地方，参数变更检测迟早在其中一处漏掉。
//!
//! ── 两条路，一句话的区别 ────────────────────────────────────────────
//! - **声明**（[`sync_declared`]）：以本次投喂的参数为准。改了模型/比例/尺寸/参考图
//!   ⇒ 终态记录重置重跑；非终态记录同步参数。
//! - **派工**（[`enqueue_records`]）：**一个字段都不写回记录**，只把 id 送进队列。
//!   派工若走声明的路，就会按「声明里没有垫图」把记录里的垫图清掉
//!   （2026-10-01 实测：点确认后垫图消失）。
//!
//! ── 禁止 ────────────────────────────────────────────────────────────
//! 不许在别处再写一份「参数变了就重置」的判断，也不许绕过本模块直接
//! `state.create` / `queue.push` —— 那会让「同一条被投喂两次」的判据在两处漂移。

use crate::engine::scheduler::LogFn;
use crate::engine::state::{
    clear, is_terminal, new_record, reset_for_retry, row as status_row, set, Clearable, RecordPatch,
    StateStore, TaskStatus,
};
use crate::lovart::models::is_known_image_model;
use crate::lovart::prompt::{
    is_valid_ratio, normalize_size, prompt_hash, prompt_id, PromptDefaults, PromptTask,
};

/// 被拒绝的一条投喂（给用户看的一句话）。
///
/// 为什么要有它：编辑框保留文字后，用户重复点提交很常见。**静默跳过 = 假成功**
/// （界面显示"已提交"，实际什么都没发生）。拒绝必须显式说出口。
#[derive(Debug, Clone)]
pub struct Rejection {
    /// 提示词摘要（前 16 字），用户才认得出是哪一条。
    pub label: String,
    pub reason: &'static str,
}

impl Rejection {
    pub fn new(text: &str, reason: &'static str) -> Self {
        let text = text.trim();
        let label = if text.chars().count() > 16 {
            format!("{}…", text.chars().take(16).collect::<String>())
        } else if text.is_empty() {
            "(空)".to_string()
        } else {
            text.to_string()
        };
        Self { label, reason }
    }

    /// 拼成一句给告警通道用的话。
    pub fn describe(&self) -> String {
        format!("「{}」{}", self.label, self.reason)
    }
}

/// GUI 的一条待投喂任务（`publish_queue` 的入参元素，字段名与前端一致）。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct TaskInput {
    pub prompt: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub ratio: String,
    #[serde(default)]
    pub size: String,
    #[serde(default)]
    pub refs: Vec<String>,
}

/// 把前端任务批量转成 `PromptTask`。
///
/// - 校验复用 `lovart::prompt` 的同一批函数（不另写一份别名表）；
/// - id 用 [`prompt_id`] 与文件解析保持一致：同批同文本 `-2` / `-3`；
/// - 空串 = 用默认值（与旧链路「省略字段」等价，前端 `draft.model` 初始就是空）。
pub fn tasks_from_inputs(
    inputs: &[TaskInput],
    defaults: &PromptDefaults,
) -> Result<Vec<PromptTask>, String> {
    let mut seen: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    let mut tasks = Vec::with_capacity(inputs.len());
    for (index, input) in inputs.iter().enumerate() {
        let line = index + 1;
        let prompt = input.prompt.trim();
        if prompt.is_empty() {
            return Err(format!("第 {line} 条没有提示词"));
        }
        let model = match input.model.trim() {
            "" => defaults.model.clone(),
            value if is_known_image_model(value) => value.to_string(),
            value => return Err(format!("第 {line} 条：模型「{value}」不在别名表里")),
        };
        let aspect_ratio = match input.ratio.trim() {
            "" => defaults.aspect_ratio.clone(),
            value if is_valid_ratio(value) => value.to_string(),
            value => return Err(format!("第 {line} 条：比例「{value}」格式不对（应为 w:h）")),
        };
        let image_size = match input.size.trim() {
            "" => defaults.image_size.clone(),
            value => normalize_size(value)
                .ok_or_else(|| format!("第 {line} 条：尺寸「{value}」必须是 1K / 2K / 4K"))?
                .to_string(),
        };
        let refs: Vec<String> = input
            .refs
            .iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect();
        let hash = prompt_hash(prompt);
        let occurrence = seen.entry(hash.clone()).or_insert(0);
        *occurrence += 1;
        tasks.push(PromptTask {
            id: prompt_id(&hash, *occurrence),
            prompt: prompt.to_string(),
            model,
            aspect_ratio,
            image_size,
            line: line as u32,
            ref_inputs: if refs.is_empty() { None } else { Some(refs) },
            refs: None,
            ref_signature: None,
        });
    }
    Ok(tasks)
}

/// 垫图地址的补丁值：**有就给、没有就显式清空**。
///
/// 「没有就清空」是刻意的：换了提示词/参考图之后，旧地址必须消失 ——
/// 留着它，界面会拿**上一张垫图**冒充这一张，而用户正是靠这个地址去核对垫图的。
fn ref_urls_of(task: &PromptTask) -> Clearable<Vec<String>> {
    match task.refs.clone() {
        Some(urls) if !urls.is_empty() => set(urls),
        _ => clear(),
    }
}

/// 派工：按 id 取记录 → 投影成任务 → 入队。返回报告用的任务表。
///
/// 两条判据与声明路径**同源**（都读状态表）：终态不处理；`job` 为空的
/// `submitting` 回退待提交（它没拿到 threadId，无从续查）。
pub fn enqueue_records(
    state: &mut StateStore,
    ids: &[String],
    queue: &mut Vec<String>,
    log: &LogFn,
) -> (Vec<PromptTask>, Vec<Rejection>) {
    let mut tasks = Vec::with_capacity(ids.len());
    let mut rejected = Vec::new();
    for id in ids {
        let Some(record) = state.get(id).cloned() else {
            rejected.push(Rejection::new(id, "没有这条记录"));
            continue;
        };
        if is_terminal(record.status) {
            rejected.push(Rejection::new(
                &record.prompt,
                "已结束，未重新派工（要再来一次请点「重跑」）",
            ));
            continue;
        }
        if status_row(record.status).job.is_none() {
            if let Err(error) = state.update(
                id,
                RecordPatch {
                    status: Some(TaskStatus::Pending),
                    error: Some(Some("上次提交未完成，重新提交".to_string())),
                    ..Default::default()
                },
            ) {
                log(&format!("重置「上次提交未完成」失败（内部 bug，请报告）：{error}"));
            }
        }
        queue.push(id.clone());
        tasks.push(record.to_prompt_task());
    }
    (tasks, rejected)
}

/// 声明：按本次投喂的参数，创建 / 重置 / 同步记录并全部入队。
///
/// 返回「因参数变更而重置重跑」的条数（日志与摘要都要用）。
///
/// ⚠️ 只对声明生效。派工不要走这里 —— 它会按「声明里没有垫图」去覆盖记录，
/// 那正是 2026-10-01 那个 bug（点确认后垫图消失）。
pub fn sync_declared(
    state: &mut StateStore,
    queue: &mut Vec<String>,
    tasks: &[PromptTask],
    retry_failed: bool,
    log: &LogFn,
) -> (usize, Vec<Rejection>) {
    let mut param_changed = 0usize;
    let mut rejected: Vec<Rejection> = Vec::new();
    for task in tasks {
        let existing = state.get(&task.id).cloned();
        let Some(existing) = existing else {
            state.create(new_record(task));
            queue.push(task.id.clone());
            continue;
        };

        // 参数（模型/比例/尺寸/参考图）被改过：以当前文件为准。
        // id 只哈希提示词文本，不检测的话终态记录会静默沿用旧参数/旧参考图的产物。
        let ref_signature = task.ref_signature.clone().unwrap_or_default();
        let same_params = existing.model == task.model
            && existing.aspect_ratio == task.aspect_ratio
            && existing.image_size == task.image_size
            && existing.ref_signature.clone().unwrap_or_default() == ref_signature;

        // ⚠️ 待确认（`needs_confirm`）：上游可能**已经出图**，只等你点头。
        // 它是终态、`job` 为空，若按普通终态走「参数变了就重置重跑」，
        // 会清掉 threadId（那张图再也找不回）并**重新扣一次费**。
        // 所以投喂一律拒绝，让它走「确认并继续」或显式「重跑」。
        if existing.status == TaskStatus::NeedsConfirm {
            rejected.push(Rejection::new(
                &task.prompt,
                "在等你确认；请点「确认并继续」或「重跑」，重新发布不会生效",
            ));
            continue;
        }

        if !same_params && is_terminal(existing.status) {
            if let Err(error) = state.update(
                &task.id,
                RecordPatch {
                    status: Some(TaskStatus::Pending),
                    model: Some(task.model.clone()),
                    aspect_ratio: Some(task.aspect_ratio.clone()),
                    image_size: Some(task.image_size.clone()),
                    attempts: Some(0),
                    timeout_count: Some(0),
                    busy_retries: Some(0),
                    ready_at: Some(0),
                    thread_id: clear(),
                    // 一并清掉 30% 锚点：重跑会拿到**新** threadId，进度应重新计时。
                    submitted_at: clear(),
                    project_id: clear(),
                    artifacts: clear(),
                    files: clear(),
                    error: clear(),
                    ref_signature: if ref_signature.is_empty() {
                        clear()
                    } else {
                        set(ref_signature.clone())
                    },
                    ref_urls: ref_urls_of(task),
                    ..Default::default()
                },
            ) {
                log(&format!("参数变更重置失败（内部 bug，请报告）：{error}"));
            }
            queue.push(task.id.clone());
            param_changed += 1;
            continue;
        }

        let mut patch = RecordPatch::default();
        if !same_params {
            patch.model = Some(task.model.clone());
            patch.aspect_ratio = Some(task.aspect_ratio.clone());
            patch.image_size = Some(task.image_size.clone());
            patch.ref_signature = if ref_signature.is_empty() {
                clear()
            } else {
                set(ref_signature.clone())
            };
            // 垫图地址与指纹**必须同进同退**：只改指纹不改地址，界面就会拿旧垫图当新的
            // （图换了、显示的地址没换 ⇒ 用户对着错的图查）。见 `TaskRecord::ref_urls`。
            patch.ref_urls = ref_urls_of(task);
        }
        if existing.line != Some(task.line) {
            patch.line = Some(task.line);
        }
        if patch != RecordPatch::default() {
            if let Err(error) = state.update(&task.id, patch) {
                log(&format!("参数同步失败（内部 bug，请报告）：{error}"));
            }
        }

        if is_terminal(existing.status) {
            if retry_failed && existing.status == TaskStatus::Failed && existing.thread_id.is_none()
            {
                match reset_for_retry(state, &task.id) {
                    Ok(_) => {
                        queue.push(task.id.clone());
                        log(&format!("[retry-failed] 🔁 {} 已重置为待提交", task.id));
                    }
                    Err(error) => {
                        log(&format!("[retry-failed] 重置失败（内部 bug）：{error}"));
                    }
                }
            } else if retry_failed
                && existing.status == TaskStatus::Failed
                && existing.thread_id.is_some()
            {
                log(&format!(
                    "⚠️ {} 失败但已有 threadId，跳过 --retry-failed",
                    task.id
                ));
            } else {
                // 终态 + 参数没变：**明确拒绝**而不是静默跳过。
                // 编辑框保留文字后用户会重复点提交，静默跳过就是「假成功」。
                rejected.push(Rejection::new(
                    &task.prompt,
                    "已完成且参数没变，未重复提交（要再出一张请点「重跑」）",
                ));
            }
            continue;
        }

        // ⚠️ 判据**读表**：非终态里**只有 `submitting` 不进队列**（`job` 为空）。
        //    它是「已认领、还没拿到 threadId」的那个 —— 进程在上次提交中途被杀，
        //    没有 threadId 无从续查 ⇒ 只能重新提交。
        //    （提交可能已成功但没落盘 —— 这个窗口无法消除，只能缩到最小。）
        //    上面 `is_terminal` 已经放行过，所以这里 `job` 为空 ⟺ `submitting`。
        if status_row(existing.status).job.is_none() {
            if let Err(error) = state.update(
                &task.id,
                RecordPatch {
                    status: Some(TaskStatus::Pending),
                    error: Some(Some("上次提交未完成，重新提交".to_string())),
                    ..Default::default()
                },
            ) {
                log(&format!("重置「上次提交未完成」失败（内部 bug，请报告）：{error}"));
            }
            queue.push(task.id.clone());
            continue;
        }

        queue.push(task.id.clone());
    }
    (param_changed, rejected)
}
