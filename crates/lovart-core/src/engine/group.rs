//! 看板分组 —— 界面分组判据的**唯一实现**。
//!
//! ⚠️ 判据**只此一份**：由 Rust 已有的谓词派生，不另立清单。
//! 前端若自己再写一遍 `['pending','submitting',…]`，就是第二份真源 ——
//! 本仓栽过的那个坑（两处同名不同口径）。所以分组随状态一起下发。
//!
//! 原先这四个函数在 `run.rs`（入口层），而 `engine::scheduler` 反向调用它们
//! （`crate::run::is_board_open`）。拆 `lovart-core` 后入口层不在 core 里，
//! 这个反向依赖直接编译不过（E0433）—— 因此下沉到本模块，方向改成
//! `scheduler → group`（同层，单向）。

use crate::engine::state::{holds_slot, is_unfinished, row as status_row, Action, TaskStatus};

/// 任务在界面上的**分组**。
///
/// - `active`   拿到槽位、正在生成（`holds_slot`）；
/// - `queued`   已发布但还没轮到（`is_unfinished` 里除去占槽的那部分）；
/// - `finished` 这一轮结束了，不管结果（`!is_unfinished`，含终态与「待下次兜底」）。
pub fn task_group(status: TaskStatus) -> &'static str {
    // 「要你处理」—— **你不点它就不动**。今天只有 `needs_confirm`（表里带 `Confirm` 动作）。
    //
    // ⚠️ 它**绝不能归进 `finished`**：`needs_confirm` 是终态（`terminal: true`），
    //    按"终态 ⇒ finished"的老口径它会躺在「已结束」里 ——
    //    用户看到的是「**它自己完成了，没问我**」（2026-10-01 实测反馈）。
    //    判据读**状态表**的 `actions` 列（有没有只有人能按的 `Confirm`），不另写清单。
    if status_row(status).actions.contains(&Action::Confirm) {
        return "waiting";
    }
    if holds_slot(status) {
        "active"
    } else if is_unfinished(status) {
        "queued"
    } else {
        "finished"
    }
}

/// 看板上「未结束」= `active` + `queued` + `waiting`（即**不在「已结束」组**）。
///
/// 这是「取消所有未完成任务」的**作用域判据**，也是它唯一的一份实现（`gui` 直接调）。
///
/// ⚠️ **不要**用 `engine::state::is_unfinished` 顶替：后者是 `!terminal`，
/// 而 `needs_confirm`（待处理）是**终态** —— 于是"待处理"会被漏删。
/// 也**不要**复用 `resume_unfinished` 的过滤（它按 `is_unfinished` 选，问题不同）。
/// 判据就是分组本身（[`task_group`]），不另抄一份状态清单。
pub fn is_board_open(status: TaskStatus) -> bool {
    task_group(status) != "finished"
}

/// 看板上「**已完成**」= `finished` 组（[`is_board_open`] 的补集）。
///
/// 这是「清空所有已完成任务」的**作用域判据**，与上面那条共用同一份分组实现
/// （补集写法是故意的：正着写第二份清单，迟早和 `is_board_open` 走岔）。
pub fn is_board_finished(status: TaskStatus) -> bool {
    !is_board_open(status)
}

/// 看板上「**排队中**」= `queued` 组：**还没提交**（没有 `threadId`，上游不知道它存在）。
///
/// 这是「取消排队」的**作用域判据**，与上面两条共用 [`task_group`] 这一份实现。
///
/// ⚠️ 它**故意**只等于 `queued`：`active` 是已提交（在等结果）、`waiting` 是上游已创建
///    但要你确认（要花积分）—— 那两者删掉只是"本地不再取图"，和"还没发出去"不是一件事。
pub fn is_board_queued(status: TaskStatus) -> bool {
    task_group(status) == "queued"
}
