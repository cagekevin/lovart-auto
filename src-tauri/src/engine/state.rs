//! `state.jsonl` —— 断点续跑状态，**本文件的唯一写者**。
//!
//! 与 TS 版 `src/state.ts` **逐条对齐** —— 它是 Step 4.1「TS 与 Rust 产出的状态必须一致」
//! 这条对账的 Rust 半边。
//!
//! 每行一条 JSON 记录；同一 id 以**最后一条**为准（追加写，天然支持崩溃恢复）。
//!
//! ── 三条不变量（TS 侧由 `test/state.test.ts` 守住，这里一一对应）──────
//!
//! ① **状态清单 = 唯一真源**，派生归类全部由它算出。
//! ② **两个时间戳收口在唯一写入口 [`StateStore::update`]** —— 调度器有 10 个状态迁移点，
//!    让每个调用点自己记得写 ⇒ 必漏。
//! ③ **落盘失败不静默** —— 断点续跑的**全部价值**建立在这个文件上，
//!    写不进去却毫无声响 = 失败伪装成成功（DEBT TD-06-01）。
//!
//! ── Rust 比 TS 强的三处（不是"顺手改"，是类型层能守就该守）────────────
//!
//! | 处 | TS | Rust |
//! |---|---|---|
//! | 状态归类 | 集合字面量，漏归类只能靠测试发现 | **穷尽 `match`** ⇒ 新增变体**编译不过** |
//! | `patch` 能否覆盖时间戳 | 靠 `update` 里显式赋值挡住 | `RecordPatch` **没有**这两个字段 ⇒ 结构上不可能 |
//! | 读取失败 | `catch {}` 吞掉一切 ⇒ 权限错静默变"空状态"⇒ 重复提交 | **只吞 `NotFound`**，其余照抛 |
//!
//! ── 与 TS 版的一处**有意差异** ──────────────────────────────────────
//! TS 的写入走 promise 链（`appendFile` 是异步的），Rust 的 `write` 是**同步**的
//! ⇒ 天然串行，不需要写队列。但 [`StateStore::flush`] 这个**接口形状**保留：
//! 调用方必须显式接住「状态没落盘」这件事，而不是靠"反正同步所以没事"。

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::lovart::prompt::PromptTask;

impl TaskRecord {
    /// 记录 → 任务（**无损**：含已上传的垫图 URL 与指纹）。
    ///
    /// ── 为什么这件事必须在这里做 ──────────────────────────────────────
    /// 它是**派工的输入** —— 调度器按 id 处理已有记录时用它。
    /// 原先这件事由**适配层**（`gui.rs` 的 `record_to_task`）做，而且**只挑 4 个字段**
    /// （`prompt/model/ratio/size`）：垫图一丢，调度器那条"声明为准"的同步逻辑
    /// 就按「没有就清空」把记录里的垫图抹掉（2026-10-01 实测：点确认后垫图消失）。
    /// 字段搬进这个 `impl` 之后**漏不掉** —— 漏了编译器直接报错。
    pub fn to_prompt_task(&self) -> PromptTask {
        PromptTask {
            id: self.id.clone(),
            prompt: self.prompt.clone(),
            model: self.model.clone(),
            aspect_ratio: self.aspect_ratio.clone(),
            image_size: self.image_size.clone(),
            line: self.line.unwrap_or(0),
            // 垫图**已经是 CDN URL**（提交时上传过）⇒ 不需要 `ref_inputs`：
            // 参考图解析那一步会因输入端为空而跳过，这些地址原样留着。
            ref_inputs: None,
            refs: self.ref_urls.clone(),
            ref_signature: self.ref_signature.clone(),
        }
    }
}

// ── ① 状态清单 = 唯一真源 ──────────────────────────────────────────────

/// 任务状态。
///
/// 序列化名与 TS 的字符串字面量**逐字对齐**（`snake_case`）—— 两边写的是同一个文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// 还没提交。
    Pending,
    /// 已认领、正在提交（占槽位；此刻进程死掉，重启后回退 pending）。
    Submitting,
    /// 已提交，轮询中（占槽位）。
    Running,
    /// 轮询超时，仍保留槽位与 threadId，待复查。
    Timeout,
    /// 超时复查达上限：已释放槽位，保留 threadId，收尾/下次兜底复查。
    Unresolved,
    /// 生成成功但下载失败：保留 artifacts，收尾/下次补下载（**不重跑生成**）。
    DownloadFailed,
    /// 出图完成并已下载。
    Done,
    /// 上游 done 但无产物（多为内容审核）。
    NoArtifact,
    /// 需要人工确认（**本程序不自动确认**）。
    NeedsConfirm,
    /// 确定性失败。
    Failed,
}

// ── ② 状态登记表 = 唯一真源 ────────────────────────────────────────────

/// 这一条在调度器队列里要做的**动作** —— 也就是「再跑一次会怎样」的代码形态。
///
/// ⚠️ 它取代了原先**四处各答一半**的同一问题（2026-10-01 普查）：
/// `scheduler::kind_of` · `run_final` 里两处 `if status == …` · README 第 4 列。
/// 现在这四个地方答的都是这一个枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// 还没提交过 ⇒ 重新提交。
    Submit,
    /// 已提交、有 `threadId` ⇒ 按 threadId 复查，**不重复提交**。
    Recheck,
    /// 生成成功、下载失败 ⇒ **只补下载，绝不重新生成**。
    Redownload,
    /// 超时达上限 ⇒ 兜底复查一次。
    RecheckFinal,
}

/// 界面上对**这一条**能做的动作。
///
/// 只装"由状态决定"的那些。「打开所在位置」看的是**产物文件在不在**、
/// 「删除」看的是**有没有批次在跑** —— 都不是状态的属性，所以刻意不在这里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 重新生成（**会再花一次额度**）。
    Retry,
    /// 接着查 / 收尾（有 threadId 就复查、有产物就补下载，**不重复提交**）。
    Resume,
    /// **确认上游那一步的高成本操作**（`pending_confirmation`）—— **会花积分**。
    ///
    /// ⚠️ 它**只能由人按**：绝不能进调度队列（进了 = 下次跑批自动确认 = 静默花钱，
    /// 那就是把 `AUTO_CONFIRM` 翻成 `true` 的另一种写法，官方明令 Do NOT auto-confirm）。
    Confirm,
}

impl Action {
    /// 下发给前端的名字（`ui/app.js` 用它挑菜单项）。**穷尽 `match`** ——
    /// 加一个动作时这里编译不过，强制作者同时决定前端怎么显示它。
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Retry => "retry",
            Action::Resume => "resume",
            Action::Confirm => "confirm",
        }
    }
}

/// 状态登记表的**一行** = 一个状态的全部后果。
///
/// ── 为什么要有这张表 ──────────────────────────────────────────────
/// 2026-10-01 普查：同一份「状态意味着什么」被写在 **19 处** ——
/// 5 处手抄清单 · 7 处穷尽 `match` · 2 处集合字面量 ·
/// 3 处读状态走特殊分支 · 2 处前端自判。
/// 加一个状态要**手改 12 处**，另有 2 处**不报错、静默漏口径**。
/// ⇒ 全部收进这张表：**加一个状态 = 加一行**，其余地方一律读表。
pub struct StatusRow {
    pub status: TaskStatus,
    /// 线格式名。与 `serde(rename_all = "snake_case")` 同值 ——
    /// **由测试钉死**（`table_wire_matches_serde`），两处漂移就红。
    pub wire: &'static str,
    /// 给人看的短名（终端看板与窗口共用同一份）。
    pub label: &'static str,
    /// 已到终态、不再需要处理。[`is_terminal`]
    pub terminal: bool,
    /// 仍占着服务端槽位。[`holds_slot`]
    pub holds_slot: bool,
    /// 尚未产出成品（除 `done` 之外）。[`is_not_done`]
    pub not_done: bool,
    /// 队列动作。`None` = **不进队列**。
    pub job: Option<Job>,
    /// 取件优先级（小的先做）。复查占槽的任务最优先 —— 它能**释放槽位**。
    pub priority: u32,
    /// 界面上可做的动作。
    pub actions: &'static [Action],
    /// 异常两件套 `(色调, 一句人话)`；正常状态 `None`（**正常不说话**）。
    ///
    /// ⚠️ **没有"短标签"这一项**：原先有，但它与 [`StatusRow::label`] 逐字相同
    ///    （待补下载/待补下载、失败/失败…）—— 卡片头部已经写了状态名，再来一遍
    ///    只是把版面撑长（2026-10-01 实测反馈：页面文字太多）。
    /// ⚠️ 文案里**不许出现实现黑话**（槽位 / 目录 / 500…），也不许口语化
    ///    （「你不点它就不动」这种）—— 这是工具，不是聊天。
    pub abnormal: Option<(&'static str, &'static str)>,
    /// 「再跑一次会怎样」的人话 —— README 那张表的第 4 列，**搬进代码**。
    /// 它和 [`Job`] 是同一件事的两种写法（人话 / 机器），刻意同排一行。
    pub rerun: &'static str,
}

/// **状态登记表**。顺序 = 状态清单顺序（[`all`] 与测试遍历都用它）。
pub const STATUS_TABLE: [StatusRow; 10] = [
    StatusRow {
        status: TaskStatus::Pending,
        wire: "pending",
        label: "待提交",
        terminal: false,
        holds_slot: false,
        not_done: true,
        job: Some(Job::Submit),
        priority: 2,
        actions: &[],
        abnormal: None,
        rerun: "重新提交",
    },
    StatusRow {
        status: TaskStatus::Submitting,
        wire: "submitting",
        label: "提交中",
        terminal: false,
        holds_slot: true,
        not_done: true,
        // 不在队列里：它正被某个 worker 认领着。认领期间进程被杀 ⇒ 没有 threadId
        // 可续 ⇒ 由建队逻辑回退 `pending`（那里读的是 `job.is_none()`，见 scheduler）。
        job: None,
        priority: 99,
        actions: &[],
        abnormal: None,
        rerun: "已认领、没有 threadId；进程重启后回退 pending",
    },
    StatusRow {
        status: TaskStatus::Running,
        wire: "running",
        label: "生成中",
        terminal: false,
        holds_slot: true,
        not_done: true,
        job: Some(Job::Recheck),
        priority: 1,
        actions: &[Action::Resume],
        abnormal: None,
        rerun: "按 threadId 继续复查，不重复提交",
    },
    StatusRow {
        status: TaskStatus::Timeout,
        wire: "timeout",
        label: "超时待查",
        terminal: false,
        holds_slot: true,
        not_done: true,
        job: Some(Job::Recheck),
        // 0 = 最优先：它占着槽位，只有复查掉它才能释放槽位、推进整批。
        priority: 0,
        actions: &[Action::Resume],
        abnormal: None,
        rerun: "优先复查，累计达 --max-timeouts 后转 unresolved",
    },
    StatusRow {
        status: TaskStatus::Unresolved,
        wire: "unresolved",
        label: "超时未决",
        terminal: false,
        holds_slot: false,
        not_done: true,
        job: Some(Job::RecheckFinal),
        priority: 3,
        actions: &[Action::Resume, Action::Retry],
        abnormal: Some(("warn", "等待超时，再发一次会重新复查")),
        rerun: "收尾/下次兜底复查，仍不重复提交",
    },
    StatusRow {
        status: TaskStatus::DownloadFailed,
        wire: "download_failed",
        label: "待补下载",
        terminal: false,
        holds_slot: false,
        not_done: true,
        job: Some(Job::Redownload),
        priority: 4,
        actions: &[Action::Resume, Action::Retry],
        abnormal: Some(("warn", "图已生成，只差取回；再发一次即可补齐")),
        rerun: "收尾/下次只补下载，绝不重新生成",
    },
    StatusRow {
        status: TaskStatus::Done,
        wire: "done",
        label: "已完成",
        terminal: true,
        holds_slot: false,
        // 唯一"已产出"的状态 —— 这就是 `is_not_done` 的全部内容。
        not_done: false,
        job: None,
        priority: 99,
        actions: &[],
        abnormal: None,
        rerun: "跳过",
    },
    StatusRow {
        status: TaskStatus::NoArtifact,
        wire: "no_artifact",
        label: "未产出",
        terminal: true,
        holds_slot: false,
        not_done: true,
        job: None,
        priority: 99,
        // 刻意**不给** Retry：内容没过审，重跑结果一样，给了就是骗人。
        actions: &[],
        abnormal: Some(("warn", "上游未返回图片（多为内容审核）；重跑结果相同")),
        rerun: "跳过，不重试",
    },
    StatusRow {
        status: TaskStatus::NeedsConfirm,
        wire: "needs_confirm",
        label: "待确认",
        terminal: true,
        holds_slot: false,
        not_done: true,
        job: None,
        priority: 99,
        // 「确认并继续」——**会花积分**，所以只有人能按（见 `Action::Confirm`）。
        // 它**不进队列**（`job: None` 保持不动）：进队列就是自动确认、静默花钱。
        actions: &[Action::Confirm],
        // ⚠️ 文案随能力一起改：原先写「按政策拒绝了」（死路一条），
        //    现在有了「确认并继续」，再说拒绝就是**自相矛盾**。
        abnormal: Some(("warn", "需你确认后才会继续；这一步会消耗积分")),
        rerun: "跳过，不自动扣积分",
    },
    StatusRow {
        status: TaskStatus::Failed,
        wire: "failed",
        label: "失败",
        terminal: true,
        holds_slot: false,
        not_done: true,
        job: None,
        priority: 99,
        actions: &[Action::Retry],
        abnormal: Some(("bad", "上游返回错误，重试已达上限")),
        rerun: "跳过",
    },
];

/// 查表 —— **唯一入口**。19 处判据全部经它，所以不可能有第二份口径。
///
/// 不返回 `Option`：表覆盖全部状态由测试钉死（`table_covers_every_status`），
/// 查不到就是"加了状态没加行"的内部 bug，**必须响**，不许静默兜底。
pub fn row(status: TaskStatus) -> &'static StatusRow {
    STATUS_TABLE
        .iter()
        .find(|row| row.status == status)
        .expect("状态登记表漏了这个状态：加状态时必须在 STATUS_TABLE 加一行")
}

/// 全部状态（顺序 = 表顺序）。
///
/// ⚠️ **不另立清单** —— 原先 `TaskStatus::ALL` 是手抄的第二份，加状态时会漏。
pub fn all() -> impl Iterator<Item = TaskStatus> {
    STATUS_TABLE.iter().map(|row| row.status)
}

/// `--status --json` 的 summary 里这个状态叫什么：线格式名的 camelCase
/// （`download_failed` → `downloadFailed`）。
///
/// ⚠️ 由 [`StatusRow::wire`] **算出来**、不另立一份键名清单：
/// 原先 `build_status_json` 里手抄了 9 个键，**漏了 `submitting`**
/// —— 手抄清单的典型漏法（加状态时没人记得回来补，而且不报错）。
/// 键名与原先那 9 个**逐字一致**，由测试钉死（`json_summary_keys`）。
pub fn json_key(status: TaskStatus) -> String {
    let mut out = String::with_capacity(status.as_str().len());
    let mut upper = false;
    for ch in status.as_str().chars() {
        if ch == '_' {
            upper = true;
        } else if upper {
            out.extend(ch.to_uppercase());
            upper = false;
        } else {
            out.push(ch);
        }
    }
    out
}

impl TaskStatus {
    /// 与 `serde` 的线格式名一致（写日志 / 对账用）。**读表**，不另写字面量。
    pub fn as_str(self) -> &'static str {
        row(self).wire
    }
}

/// 已到终态、不再需要处理（**读表**）。
///
/// ⚠️ 原先这里是穷尽 `match`，好处是「加状态编译不过」。收进表之后那个保护
/// **由测试接手**（`table_covers_every_status` + `state_invariants`）：漏加一行就红。
/// 这是刻意的取舍 —— 19 处判据共用一份口径，比"每处各自编译不过"值钱。
pub fn is_terminal(status: TaskStatus) -> bool {
    row(status).terminal
}

/// 仍占着服务端槽位的状态（**读表**）。
///
/// **槽位计数由它派生，不另立计数器**（DEBT TD-01-01）。
///
/// `Submitting` 是关键：把「已认领、正在提交」变成**显式状态**，
/// 就不再需要那种"预占 +1 / 归还 -1"的计数器 —— 计数器与真相会漂移。
pub fn holds_slot(status: TaskStatus) -> bool {
    row(status).holds_slot
}

/// **仍在处理中**（未到终态）。汇总的「处理中」用它（对应 TS `UNFINISHED_STATUSES`）。
///
/// 纯派生（`!terminal`），**不重抄清单** —— 所以它不会漂移。
pub fn is_unfinished(status: TaskStatus) -> bool {
    !row(status).terminal
}

/// **尚未产出成品**（除 `done` 之外的全部）。看板的「尚未出图」用它
/// （对应 TS `NOT_DONE_STATUSES`）。
///
/// ⚠️ 它与 [`is_unfinished`] 回答的是**两个不同的问题**：
///   - 「处理中」= 还在跑；
///   - 「尚未出图」= 需要你关注（含 `no_artifact` / `needs_confirm` / `failed`
///     这些**已结束但没图**的）。
/// 原先 TS 两处都叫「未完成」且口径不同 ⇒ 终端打出「未完成 5」紧接「未完成清单 12 条」
/// （TD-01-03）。治法不是硬合成一个，而是**各叫各的名字**。
///
/// 读表：它是表里的 `not_done` 列（**唯一一行 `false` 的是 `done`**）。
pub fn is_not_done(status: TaskStatus) -> bool {
    row(status).not_done
}

/// 槽位 = 占槽状态的记录数。**派生，不维护计数器**（DEBT TD-01-01）。
///
/// 这是方案 Step 1.5 点名要求建的原语，调用点有两处：
/// **① 取件判定**（`scheduler` 决定还能不能提交）· **② 看板展示**。
pub fn slots_in_use(state: &StateStore) -> usize {
    state
        .all()
        .iter()
        .filter(|record| holds_slot(record.status))
        .count()
}

/// 把一条记录重置为待提交（重跑）。**保留 `ref_urls` / `ref_signature`** —— 重跑仍要带垫图。
///
/// 唯一实现：GUI 的「重跑这一条」与 CLI 的 `--retry-failed` 共用。
pub fn reset_for_retry(store: &mut StateStore, id: &str) -> Result<TaskRecord, StateError> {
    store.update(
        id,
        RecordPatch {
            status: Some(TaskStatus::Pending),
            attempts: Some(0),
            timeout_count: Some(0),
            busy_retries: Some(0),
            ready_at: Some(0),
            thread_id: clear(),
            submitted_at: clear(),
            project_id: clear(),
            artifacts: clear(),
            files: clear(),
            error: clear(),
            // 刻意不动 ref_urls / ref_signature —— 重跑仍要带垫图。
            ..Default::default()
        },
    )
}

// ── 记录 ──────────────────────────────────────────────────────────────

/// 一条任务记录（对应 TS `TaskRecord`）。
///
/// 字段名用 `camelCase` 序列化 —— 与 TS 的 `JSON.stringify` 一致，两边写同一个文件。
///
/// ⚠️ `Option` 字段一律 `skip_serializing_if`：TS 的 `JSON.stringify` **会省略
/// `undefined` 值的键**，不跳过就会写出 `"threadId": null` 这种 TS 读得进、
/// 但语义不同的东西（TS 里 `null` 与 `undefined` 不是一回事）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    pub id: String,
    pub prompt: String,
    /// 源文件行号（1 起），便于排错。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub model: String,
    pub aspect_ratio: String,
    pub image_size: String,
    pub status: TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub attempts: u32,
    /// 累计轮询超时次数（含提交阶段那一次）。
    ///
    /// 用于给「槽位保留」设上限：达到 `maxTimeouts` 就释放槽位转 `unresolved`，
    /// 否则服务端卡住的任务会永久占住本地槽位，把并发拖垮。
    #[serde(default)]
    pub timeout_count: u32,
    /// 参考图指纹（图生图用）。跨运行稳定，用于「换了参考图就重跑」的判定。
    /// 空 = 文生图。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_signature: Option<String>,
    /// 这次用的**垫图**（参考图）在 Lovart CDN 上的地址。空 = 文生图。
    ///
    /// ── 为什么必须存**地址**，而不是只留指纹 ─────────────────────────
    /// 任务发出去以后，「我当时垫的是哪张图」只有一个地方能回答，就是这里。
    /// 原先只存 [`ref_signature`](Self::ref_signature) —— 那是单向哈希，
    /// **反推不出图**：界面上连看一眼垫图都做不到（用户直接抱怨过这件事）。
    ///
    /// ⚠️ 存 **CDN URL** 而不是原图：原图在 `data:` 里动辄 1MB，塞进 `state.jsonl`
    /// 会让它每行都变巨大（追加写的文件，膨胀是乘法）。而 CDN URL 本来就有 ——
    /// 它是 `resolve_attachments` 那次上传的**真实产物地址**，正好就是
    /// 「给我一个地址我就能查」要的那个地址。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_urls: Option<Vec<String>>,
    /// 累计「服务端并发满」重投次数。
    ///
    /// 刻意与 `attempts` 分开：并发满是**排队状态**，不是失败。
    /// 混进 `attempts` 会让 `maxAttempts`（默认 3）把「另一批占着几小时」直接判死。
    #[serde(default)]
    pub busy_retries: u32,
    /// 最早可提交时间（毫秒时间戳）。撞「服务端并发满」后按任务各自退避。
    ///
    /// 刻意**落盘**而不是放内存：重启后排队状态不丢，且与任务状态同居一处（单一真相）。
    #[serde(default)]
    pub ready_at: u64,
    /// **首次拿到 threadId** 的时刻（ISO 字符串）。写一次不再改（除非重跑把 threadId 清空）。
    ///
    /// ⚠️ 当前**没有展示消费者**：它原先给进度条当「30% 锚点」，而进度条已经删了。
    ///    字段保留是为了将来算「总耗时」（首次提交 → 完成）；删它要动 schema 与重跑逻辑。
    /// ⚠️ 不能用 `updated_at` 顶替 —— 任务会 `running → timeout → running` 来回切，
    /// 那个时间戳跟着变。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submitted_at: Option<String>,
    /// **进入当前 `status`** 的时刻（ISO 字符串）。**仅在 status 变化时**写。
    ///
    /// 用途：看板给**生产中的任务**算「已持续 12:30」（`run.rs` 的 `elapsed_text`）；
    /// 排队 / 待确认 / 终态**不用它计时**，终态只取它的绝对时刻当「结束」。
    /// ⚠️ 不能用 `updated_at` 顶替 —— `finish()` 里 `update(id, { readyAt: 0 })`
    /// 会**顺带刷新** `updated_at`，于是「别人一完成，你的时间戳就被改了」。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_since: Option<String>,
    /// 记录最后落盘时间。**不是**状态起点，见 `state_since`。
    pub updated_at: String,
}

/// 新建一条记录（对应 TS `newRecord`）。
pub fn new_record(task: &PromptTask) -> TaskRecord {
    let now = now_iso();
    TaskRecord {
        id: task.id.clone(),
        prompt: task.prompt.clone(),
        line: Some(task.line),
        model: task.model.clone(),
        aspect_ratio: task.aspect_ratio.clone(),
        image_size: task.image_size.clone(),
        status: TaskStatus::Pending,
        thread_id: None,
        project_id: None,
        artifacts: None,
        files: None,
        error: None,
        attempts: 0,
        timeout_count: 0,
        // ⚠️ **必须从 task 带过来**（原来这里写死 `None`）：
        //    参考图是在 `resolve_refs_core` 里解析好、随 `PromptTask` 传进调度器的，
        //    新建记录时丢掉 ⇒ **第一次跑的任务永远没有垫图信息**
        //    （后面只在"参数变了"的补丁分支里才会写上，而新任务根本走不到那里）。
        //    症状就是：图生图发出去，界面上、记录里都看不到垫图（用户报过）。
        ref_signature: task.ref_signature.clone(),
        ref_urls: task.refs.clone(),
        busy_retries: 0,
        ready_at: 0,
        // 新记录已处于 pending，所以状态起点就是现在。submittedAt 等首次拿到 threadId 再写。
        submitted_at: None,
        state_since: Some(now.clone()),
        updated_at: now,
    }
}

/// ISO 8601（毫秒精度，UTC，`Z` 结尾）—— 与 JS `new Date().toISOString()` 逐字符一致。
fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

// ── 增量 patch ────────────────────────────────────────────────────────

/// 「可显式清空」的字段：`None` = 不改；`Some(None)` = 清空；`Some(Some(v))` = 设值。
///
/// TS 靠「键存在即覆盖」（`{...base, ...patch}`）实现同一语义 —— 传 `undefined`
/// 就是清空。Rust 没有这个歧义，用双层 `Option` 显式表达。
pub type Clearable<T> = Option<Option<T>>;

/// 构造「设值」（`Some(Some(v))`）。
pub fn set<T>(value: T) -> Clearable<T> {
    Some(Some(value))
}

/// 构造「清空」（`Some(None)`）。
pub fn clear<T>() -> Clearable<T> {
    Some(None)
}

/// 一次状态变更（对应 TS `update` 的 patch 参数）。
///
/// ⚠️ **刻意没有 `state_since` / `submitted_at` 之外的 id / updated_at** ——
/// 那两个时间戳由状态机自己决定，不在 patch 里 ⇒ 调用方**结构上不可能**覆盖它们。
/// TS 那边是"注释承诺 + 显式赋值"，这里由类型兑现。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordPatch {
    pub status: Option<TaskStatus>,
    /// 源文件行号（提示词文件重排后行号会变）。
    pub line: Option<u32>,
    pub model: Option<String>,
    pub aspect_ratio: Option<String>,
    pub image_size: Option<String>,
    pub attempts: Option<u32>,
    pub timeout_count: Option<u32>,
    pub busy_retries: Option<u32>,
    pub ready_at: Option<u64>,
    pub thread_id: Clearable<String>,
    pub project_id: Clearable<String>,
    pub artifacts: Clearable<Vec<String>>,
    pub files: Clearable<Vec<String>>,
    pub error: Clearable<String>,
    pub ref_signature: Clearable<String>,
    /// 垫图地址（与 [`ref_signature`](RecordPatch::ref_signature) 同步改）：
    /// 换了参考图但提示词没变时，记录里的地址必须跟着换，否则界面会显示**旧垫图**。
    pub ref_urls: Clearable<Vec<String>>,
    /// `Some(None)` = 显式清空（重跑重置）；`None` = 沿用旧值。
    pub submitted_at: Clearable<String>,
}

// ── 落盘结果 ──────────────────────────────────────────────────────────

/// 落盘结果 —— **判别联合**，不是 `()`。
///
/// 写盘失败（盘满 / 权限 / 文件被占）会让断点续跑失效，调用方**必须知道**。
/// TS 原先这里是 `.catch(() => undefined)`：失败完全无声，程序照常打印「已提交」，
/// 重启后进度全丢（DEBT TD-06-01）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistOutcome {
    Ok,
    Failed {
        /// 首次失败的原因（原文）。
        message: String,
        /// 累计失败次数。
        failed_writes: usize,
    },
}

impl PersistOutcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, PersistOutcome::Ok)
    }
}

/// 状态层错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateError {
    /// `update` 一个不存在的 id。**这是 bug，不是运行时状况** —— 绝不静默造记录。
    UnknownTask(String),
}

impl std::fmt::Display for StateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateError::UnknownTask(id) => write!(formatter, "state: 未知任务 {id}"),
        }
    }
}

impl std::error::Error for StateError {}

// ── StateStore ────────────────────────────────────────────────────────

/// 写盘失败时的回调。**只调一次**（首次失败），用于在跑的中途就把失败暴露出来，
/// 而不是等到收尾 `flush()` 才发现。
pub type WriteErrorHandler = Box<dyn Fn(&str) + Send + Sync>;

/// [`StateStore::open_with`] 的可选项。
#[derive(Default)]
pub struct StateStoreOptions {
    /// 首次写失败时**立刻**回调一次（不刷屏）。
    pub on_write_error: Option<WriteErrorHandler>,
}

/// `state.jsonl` 的**唯一写者**。
///
/// ⚠️ 记录用 `BTreeMap`（按 id 有序）而不是 `HashMap`：后者的迭代顺序**每次进程运行都不同**，
/// 会让 `compact()` 产出与调度遍历顺序不可复现。TS 侧是插入序；两者语义上都不是契约
/// （对账刻意不比迁移顺序），但**确定性**是白拿的，没有理由不要。
pub struct StateStore {
    file_path: PathBuf,
    records: BTreeMap<String, TaskRecord>,
    loaded_lines: usize,
    /// 首次写失败的原文（用于立刻告警一次，不刷屏）。
    first_write_error: Option<String>,
    failed_writes: usize,
    on_write_error: Option<WriteErrorHandler>,
}

impl StateStore {
    /// 打开（不存在则视为空，并建好父目录）。
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_with(path, StateStoreOptions::default())
    }

    /// 打开并带选项（如写失败回调）。
    pub fn open_with(path: impl AsRef<Path>, options: StateStoreOptions) -> io::Result<Self> {
        let file_path = path.as_ref().to_path_buf();
        if let Some(dir) = file_path.parent() {
            if !dir.as_os_str().is_empty() {
                fs::create_dir_all(dir)?;
            }
        }

        let mut store = StateStore {
            file_path,
            records: BTreeMap::new(),
            loaded_lines: 0,
            first_write_error: None,
            failed_writes: 0,
            on_write_error: options.on_write_error,
        };
        store.load()?;

        // 反复重跑会让追加历史膨胀：行数远大于记录数时折叠一次。
        if store.loaded_lines > std::cmp::max(200, store.records.len() * 4) {
            store.compact()?;
        }
        Ok(store)
    }

    /// 只读打开：**不建目录、不压实**。
    ///
    /// 供「任务看板」这类旁路消费者使用 —— 另一个进程可能正在追加写同一个文件，
    /// 而压实会整体重写它，有覆盖并发写入的风险。
    ///
    /// ⚠️ 仍然返回 `&mut self` 可用的 `StateStore`；**调用方有责任不调用写方法**。
    /// （用类型区分读写需要一个额外的类型参数，本增量不做 —— 但看板那条路只在
    /// `main`/`commands` 里用，加一次 `#[must_use]` 与注释比引入类型体操更划算。）
    pub fn open_read_only(path: impl AsRef<Path>) -> io::Result<Self> {
        let mut store = StateStore {
            file_path: path.as_ref().to_path_buf(),
            records: BTreeMap::new(),
            loaded_lines: 0,
            first_write_error: None,
            failed_writes: 0,
            on_write_error: None,
        };
        store.load()?;
        Ok(store)
    }

    fn load(&mut self) -> io::Result<()> {
        // 「只吞 `NotFound`」这条判断的**唯一实现**在 `engine::text` ——
        // 这里不再自己写一遍（写第二遍就会有两份口径，本仓栽过的正是这个）。
        let Some(text) = crate::engine::text::read_text(&self.file_path)? else {
            // 首次运行没有 state 文件 —— 这是**正常状况**，不是错误。
            return Ok(());
        };

        for line in text.split('\n') {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            self.loaded_lines += 1;
            // 半行（崩溃时可能产生）直接跳过 —— 与 TS 同口径。
            if let Ok(record) = serde_json::from_str::<TaskRecord>(trimmed) {
                if !record.id.is_empty() {
                    self.records.insert(record.id.clone(), record);
                }
            }
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&TaskRecord> {
        self.records.get(id)
    }

    pub fn all(&self) -> Vec<&TaskRecord> {
        self.records.values().collect()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// 已加载的历史行数（压实判据用）。
    pub fn loaded_lines(&self) -> usize {
        self.loaded_lines
    }

    /// 首次写入某任务（记录尚不存在时用）。
    pub fn create(&mut self, record: TaskRecord) -> &TaskRecord {
        let id = record.id.clone();
        self.records.insert(id.clone(), record);
        // 先入内存再落盘：与 TS 同序（`records.set` → `append`）。
        let snapshot = self.records[&id].clone();
        self.write(&snapshot);
        &self.records[&id]
    }

    /// 追加一次状态变更。
    ///
    /// ── 两个时间戳在这里**收口**（不靠调用方逐点写）────────────────────
    /// 调度器有 **10 个状态迁移点**。让每个调用点自己记得写时间戳 ⇒ **必漏**。
    /// 收口在这个**唯一写入口** ⇒ 传什么 patch 都影响不到这两个字段的正确性。
    ///
    ///   - `state_since`：**仅当 `status` 真的变了**才写。
    ///     绝不能用 `updated_at` 顶替 —— `finish()` 里 `update(id, { readyAt: 0 })`
    ///     会顺带刷新 `updated_at`。
    ///   - `submitted_at`：**首次拿到 threadId** 时写。
    ///     参数变更重跑会先把 `thread_id` 清空，所以「再次从无到有」会重新计时 ——
    ///     这正是重跑该有的语义（新 threadId = 新的 30% 锚点）。
    pub fn update(&mut self, id: &str, patch: RecordPatch) -> Result<TaskRecord, StateError> {
        let base = self
            .records
            .get(id)
            .cloned()
            .ok_or_else(|| StateError::UnknownTask(id.to_string()))?;

        let now = now_iso();
        let status_changed = patch.status.is_some_and(|status| status != base.status);
        // 「首次从无到有」：base 没有，而 patch **显式设了**（`clear()` 不算）。
        let first_thread = base.thread_id.is_none() && matches!(patch.thread_id, Some(Some(_)));

        let mut next = base.clone();
        if let Some(value) = patch.status {
            next.status = value;
        }
        if let Some(value) = patch.line {
            next.line = Some(value);
        }
        if let Some(value) = patch.model {
            next.model = value;
        }
        if let Some(value) = patch.aspect_ratio {
            next.aspect_ratio = value;
        }
        if let Some(value) = patch.image_size {
            next.image_size = value;
        }
        if let Some(value) = patch.attempts {
            next.attempts = value;
        }
        if let Some(value) = patch.timeout_count {
            next.timeout_count = value;
        }
        if let Some(value) = patch.busy_retries {
            next.busy_retries = value;
        }
        if let Some(value) = patch.ready_at {
            next.ready_at = value;
        }
        if let Some(value) = patch.thread_id {
            next.thread_id = value;
        }
        if let Some(value) = patch.project_id {
            next.project_id = value;
        }
        if let Some(value) = patch.artifacts {
            next.artifacts = value;
        }
        if let Some(value) = patch.files {
            next.files = value;
        }
        if let Some(value) = patch.error {
            next.error = value;
        }
        if let Some(value) = patch.ref_signature {
            next.ref_signature = value;
        }
        if let Some(value) = patch.ref_urls {
            next.ref_urls = value;
        }

        // ⚠️ 以下三个字段**由状态机自己决定**，显式赋值 —— 它们根本不在 `RecordPatch` 里，
        //    所以「调用方的 patch 覆盖掉它们」这件事在类型层就不成立。
        next.id = id.to_string();
        next.updated_at = now.clone();
        next.state_since = Some(if status_changed {
            now.clone()
        } else {
            base.state_since.clone().unwrap_or_else(|| now.clone())
        });
        next.submitted_at = match patch.submitted_at {
            // 显式设值 / 显式清空
            Some(explicit) => explicit,
            // 首次拿到 threadId
            None if first_thread => Some(now.clone()),
            // 沿用旧值 —— 30% 锚点不能被无关更新冲掉
            None => base.submitted_at.clone(),
        };

        self.records.insert(id.to_string(), next.clone());
        self.write(&next);
        Ok(next)
    }

    /// 如实报告落盘结果。
    ///
    /// ⚠️ **接口形状刻意与 TS 一致**（返回判别联合而不是 `()`）：Rust 的写是同步的，
    /// 但"调用方必须显式接住『状态没落盘』"这件事不能因为同步就省掉。
    pub fn flush(&self) -> PersistOutcome {
        match &self.first_write_error {
            Some(message) => PersistOutcome::Failed {
                message: message.clone(),
                failed_writes: self.failed_writes,
            },
            None => PersistOutcome::Ok,
        }
    }

    /// 首次写失败的原因（未失败则 `None`）。用于跑的中途**立刻**发现。
    pub fn first_write_error(&self) -> Option<&str> {
        self.first_write_error.as_deref()
    }

    /// 累计写失败次数。
    pub fn failed_writes(&self) -> usize {
        self.failed_writes
    }

    /// 把追加历史折叠成「每个 id 一行」，防止文件无限膨胀。
    pub fn compact(&mut self) -> io::Result<()> {
        let mut body = String::new();
        for record in self.records.values() {
            let line = serde_json::to_string(record)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            body.push_str(&line);
            body.push('\n');
        }

        // 写临时文件再 `rename`：rename 在同一文件系统内是原子的，
        // 所以中途崩溃不会留下"写了一半的 state"。
        let mut tmp_os = self.file_path.clone().into_os_string();
        tmp_os.push(".tmp");
        let tmp = PathBuf::from(tmp_os);
        fs::write(&tmp, body.as_bytes())?;
        fs::rename(&tmp, &self.file_path)?;

        self.loaded_lines = self.records.len();
        Ok(())
    }

    /// 删掉若干条记录，返回真正删掉的数量。
    ///
    /// ⚠️ **这不是「往 state.jsonl 加一条删除标记」—— 它是重写整个文件。**
    /// 所以调用方**必须先确认没有批次在跑**：调度器是往同一个文件**追加写**的，
    /// 而这里是「读 → 改 → 写」；两者并发 ⇒ 这次重写会把窗口期内追加的新行
    /// **静默冲掉**（没有报错、没有告警，就是少了）。这条前置条件由调用方保证。
    ///
    /// 复用 [`compact`](Self::compact) 的写法：**临时文件 + rename**（同文件系统内原子），
    /// 中途崩溃不会留下「写了一半的状态」。
    ///
    /// 只删记录，**不动磁盘上的图** —— 删用户的作品是另一件事，不该藏在「删任务」里。
    pub fn remove(&mut self, ids: &[String]) -> io::Result<usize> {
        let before = self.records.len();
        for id in ids {
            self.records.remove(id);
        }
        let removed = before - self.records.len();
        if removed > 0 {
            self.compact()?;
        }
        Ok(removed)
    }

    /// 追加一行。
    ///
    /// ⚠️ **每次重新打开文件**，不缓存句柄：`compact()` 用 `rename` 换掉了 inode，
    /// 缓存的句柄会继续往**已被解除链接的旧文件**里写 —— 那些数据会静默消失。
    fn write(&mut self, record: &TaskRecord) {
        let mut line = match serde_json::to_string(record) {
            Ok(text) => text,
            Err(error) => {
                self.record_write_error(&error.to_string());
                return;
            }
        };
        line.push('\n');

        let result = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file_path)
            .and_then(|mut file| file.write_all(line.as_bytes()));

        if let Err(error) = result {
            self.record_write_error(&error.to_string());
        }
    }

    /// 记一次写失败：计数 + **首次立刻回调一次**（跑的中途就暴露，不必等到收尾）。
    fn record_write_error(&mut self, message: &str) {
        self.failed_writes += 1;
        if self.first_write_error.is_none() {
            self.first_write_error = Some(message.to_string());
            if let Some(handler) = &self.on_write_error {
                handler(message);
            }
        }
    }
}

// ── 对账归一化（与 TS `test/support/replay.ts` 的 `normalizeStateFile` 同口径）──

/// 语义投影：**剥掉一切时间字段**。
///
/// `updated_at` / `state_since` / `submitted_at` / `ready_at` 都是真实时钟，
/// 回放**不可能**复现 ⇒ 拿它们对账就是造一个必然 flaky 的测试。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedRecord {
    pub id: String,
    pub status: String,
    pub model: String,
    pub aspect_ratio: String,
    pub image_size: String,
    pub attempts: u32,
    pub timeout_count: u32,
    pub busy_retries: u32,
    pub error: Option<String>,
    pub artifact_count: usize,
    pub file_count: usize,
    pub thread_id: Option<String>,
}

/// 把 `state.jsonl` 的**全文**（含历史行）归一化。
///
/// ⚠️ 迁移**顺序**刻意不比（L3）：退避带随机 jitter，并发下顺序天然不可复现，
/// 比了就是一个 flaky 测试。**只比 L1（最终状态 + 语义字段）与 L2（出现过哪些状态）。**
pub fn normalize_state_text(text: &str) -> NormalizedState {
    let mut latest: BTreeMap<String, TaskRecord> = BTreeMap::new();
    let mut seen: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();

    for line in text.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<TaskRecord>(trimmed) else {
            continue; // 半行（崩溃残留）跳过 —— 与 StateStore 同口径
        };
        if record.id.is_empty() {
            continue;
        }
        seen.entry(record.id.clone())
            .or_default()
            .insert(record.status.as_str().to_string());
        latest.insert(record.id.clone(), record);
    }

    let final_records: Vec<NormalizedRecord> = latest
        .values()
        .map(|record| NormalizedRecord {
            id: record.id.clone(),
            status: record.status.as_str().to_string(),
            model: record.model.clone(),
            aspect_ratio: record.aspect_ratio.clone(),
            image_size: record.image_size.clone(),
            attempts: record.attempts,
            timeout_count: record.timeout_count,
            busy_retries: record.busy_retries,
            error: record.error.clone(),
            artifact_count: record.artifacts.as_ref().map_or(0, Vec::len),
            file_count: record.files.as_ref().map_or(0, Vec::len),
            thread_id: record.thread_id.clone(),
        })
        .collect();

    let seen_sorted: BTreeMap<String, Vec<String>> = seen
        .into_iter()
        .map(|(id, set)| (id, set.into_iter().collect()))
        .collect();

    NormalizedState {
        final_records,
        seen: seen_sorted,
    }
}

/// 归一化结果（对应 TS `NormalizedState`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedState {
    /// 每个 id 的**最终状态**（按 id 排序）—— L1 对账。
    #[serde(rename = "final")]
    pub final_records: Vec<NormalizedRecord>,
    /// 每个 id **出现过哪些状态**（按 id / 状态名排序）—— L2 对账，能抓到「多绕了一圈」。
    pub seen: BTreeMap<String, Vec<String>>,
}
