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
    is_terminal, new_record, reset_for_retry, row as status_row, RecordPatch, StateStore,
    TaskStatus,
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
        // ⚠️ 空项**报错**，不静默丢掉：调用方给了空串说明它自己没校验，
        //    丢掉就是"少一张垫图也照发"，用户看不出来。
        let mut refs: Vec<String> = Vec::with_capacity(input.refs.len());
        for value in &input.refs {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err(format!("第 {line} 条：refs 里有空项"));
            }
            refs.push(trimmed.to_string());
        }
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
            ref_files: None,
        });
    }
    Ok(tasks)
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

/// 声明：为本次投喂的**每一条**任务新建记录并全部入队。
///
/// ⚠️ **永不覆盖已有记录**：同一个提示词再投一次就是新的一张
/// （id 顺延 `-2` / `-3`…）；参数变了也新建 —— 旧图还在，新图是另一张。
///
/// 返回值第一项**恒为 `0`**：它原名是「参数变更重置条数」，而现在已经没有
/// 「重置已有记录」这条路径了。保留它是为了少改一处调用方，
/// 语义说明见 `EnqueueOutcome::param_changed`。
///
/// ⚠️ 只对声明生效；派工走 [`enqueue_records`]（那边按记录无损投影，不碰声明内容）。
pub fn sync_declared(
    state: &mut StateStore,
    queue: &mut Vec<String>,
    tasks: &[PromptTask],
    retry_failed: bool,
    log: &LogFn,
) -> (usize, Vec<Rejection>, Vec<PromptTask>) {
    let mut rejected: Vec<Rejection> = Vec::new();
    // 新建的记录：调用方要补进 `task_by_id`（报告的行号/模式靠它）。
    let mut spawned: Vec<PromptTask> = Vec::with_capacity(tasks.len());
    for task in tasks {
        // CLI 的 `--retry-failed` 是**唯一**特例：把失败那条重置再跑。
        // （新建一条会拿同样参数再失败一次，等于什么也没修。）
        if retry_failed
            && state.get(&task.id).is_some_and(|record| {
                record.status == TaskStatus::Failed && record.thread_id.is_none()
            })
        {
            match reset_for_retry(state, &task.id) {
                Ok(_) => {
                    queue.push(task.id.clone());
                    log(&format!("[retry-failed] 🔁 {} 已重置为待提交", task.id));
                }
                Err(error) => log(&format!("[retry-failed] 重置失败（内部 bug）：{error}")),
            }
            continue;
        }

        // 其余一律**新建一条**：同一个提示词要几张就新建几条，
        // 参数变了也新建（旧图还在，新图是另一张）。id 冲突就顺延 `-2` / `-3`…
        match spawn_extra(state, task) {
            Some(extra) => {
                queue.push(extra.id.clone());
                spawned.push(extra);
            }
            None => rejected.push(Rejection::new(
                &task.prompt,
                "这一条已经排到第 999 张，没有新建",
            )),
        }
    }

    (0, rejected, spawned)
}

/// 为「同一个提示词再要一张」分配新 id 并建记录。
///
/// id 契约不变：`sha1(prompt)` 前缀 + `-N` 后缀（`prompt_id` 的既有约定）。
/// 这里找下一个**未被占用**的序号，所以历史记录一张都不会被覆盖。
pub(crate) fn spawn_extra(state: &mut StateStore, task: &PromptTask) -> Option<PromptTask> {
    // 原 id 空着就用它（首次投喂走这条），否则顺延。
    if state.get(&task.id).is_none() {
        state.create(new_record(task));
        return Some(task.clone());
    }
    let base = task.id.as_str();
    for seq in 2..=999u32 {
        let candidate = format!("{base}-{seq}");
        if state.get(&candidate).is_none() {
            let mut spawned = task.clone();
            spawned.id = candidate;
            state.create(new_record(&spawned));
            return Some(spawned);
        }
    }
    None
}
