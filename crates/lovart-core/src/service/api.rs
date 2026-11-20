//! 提交 / 停止 —— **唯一**的业务入口。
//!
//! GUI 的 `publish_queue` / `stop_run` 与下一步的 HTTP `/tasks` / `/stop` 都只能调这里；
//! 它们不许各自拼一遍 `intake` + `start_run`（那会立刻长出两套口径）。

use std::sync::Arc;

use crate::engine::intake::{tasks_from_inputs, TaskInput};
use crate::engine::scheduler::Batch;
use crate::lovart::prompt::{PromptDefaults, PromptTask};
use crate::service::refs::resolve_refs_core;
use crate::service::workbench::Workbench;

/// 起一批要跑的活 —— **两种来源**（语义分界见 `engine::scheduler::Batch`）。
///
/// ⚠️ **派工不走文件**：原先「重跑 / 接着查 / 确认」都是先伪造一份单行提示词文件
/// 再交给调度器。伪造用的 `record_to_task` 只挑 4 个字段 ⇒ 垫图在那份"声明"里为空
/// ⇒ 调度器按「以文件为准」把记录里的垫图覆盖掉（2026-10-01 实测：确认后垫图消失）。
pub enum RunSource {
    /// 用户从编辑框投喂的任务（**声明**）：校验与 id 已由 `intake` 完成。
    Declared(Vec<PromptTask>),
    /// 对已有记录的派工：只处理这些 id（**记录才是真相**）。
    Records(Vec<String>),
}

/// 停止请求的回执：只代表「已发信号」，不代表「已停」。
pub struct StopAck {
    pub note: String,
}

impl Workbench {
    /// 声明式提交：校验 / 建 id / 起批次。返回**受理条数**（不是跑完条数）。
    pub fn publish(self: &Arc<Self>, tasks: Vec<TaskInput>) -> Result<usize, String> {
        let (defaults, _) = PromptDefaults::from_env();
        let parsed = tasks_from_inputs(&tasks, &defaults)?;
        if parsed.is_empty() {
            return Err("待发是空的".to_string());
        }
        let accepted = parsed.len();
        self.start_run(RunSource::Declared(parsed))?;
        Ok(accepted)
    }

    /// 起一个批次（声明或派工）。
    ///
    /// ⚠️ receiver 是 `&Arc<Self>`：后台线程要持一份 `Arc`（不再 `AppHandle::try_state`）。
    pub fn start_run(self: &Arc<Self>, source: RunSource) -> Result<(), String> {
        let batch = match source {
            RunSource::Declared(tasks) => Batch::Declared(tasks),
            RunSource::Records(ids) => Batch::Records(ids),
        };

        // **惰性启动**：第一次投喂才读凭据 / 开状态文件。
        let scheduler = self.ensure_scheduler()?;

        // 参考图解析要联网 + 读写缓存 ⇒ 后台线程；投喂也在同一线程并持有 `intake_lock`
        // （`refs-cache.json` 是「读-合并-写」，并发投喂会互相覆盖）。
        let core = Arc::clone(self);
        std::thread::spawn(move || {
            let _guard = core.lock_intake();

            let mut batch = batch;
            // ⚠️ **只有"声明"要解析参考图**（派工的垫图已经是 CDN URL，重传一遍纯属浪费）。
            if let Batch::Declared(tasks) = &mut batch {
                if let Err(message) = resolve_refs_core(
                    tasks,
                    core.cwd(),
                    &[],
                    &|message| crate::engine::runlog::line(message),
                ) {
                    core.push_alert(&format!("参考图处理失败，这一批没有开跑：{message}"));
                    return;
                }
            }

            match scheduler.enqueue(batch) {
                Ok(outcome) => {
                    if !outcome.opened {
                        crate::engine::runlog::line(&format!(
                            "===== 会话内追加：{} 条 =====",
                            outcome.tasks.len()
                        ));
                    }
                    if !outcome.rejected.is_empty() {
                        let mut lines: Vec<String> = outcome
                            .rejected
                            .iter()
                            .take(3)
                            .map(|rejection| rejection.describe())
                            .collect();
                        if outcome.rejected.len() > 3 {
                            lines.push(format!("……另有 {} 条", outcome.rejected.len() - 3));
                        }
                        core.push_alert(&format!(
                            "有 {} 条没有提交：{}",
                            outcome.rejected.len(),
                            lines.join("；")
                        ));
                    }
                }
                Err(error) => {
                    core.push_alert(&format!("投喂失败：{}", error.message()));
                }
            }
        });

        Ok(())
    }

    /// 协作式停止：只发信号。无批次 ⇒ `Err`（HTTP 映射 409）。
    pub fn stop(&self) -> Result<StopAck, String> {
        let Some(scheduler) = self.scheduler_snapshot() else {
            return Err("当前没有在跑的批次".to_string());
        };
        scheduler.stop_session();
        Ok(StopAck {
            note: "已请求停止：最坏要等当前这次请求结束（约 60 秒）才真正退出".to_string(),
        })
    }
}
