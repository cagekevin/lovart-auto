//! 会话统计 —— 一次「从空闲被投喂 → 再次空闲」的窗口。
//!
//! ── 职责边界 ────────────────────────────────────────────────────────
//! 只做**计数与封口**，不参与调度：该干什么仍由 `state.jsonl` 的记录派生
//! （`engine::state` 的状态表）。所以它没有"状态"，只有「这一次投喂了哪些 id」。
//!
//! ── 为什么必须有个会话 ──────────────────────────────────────────────
//! 报告 CSV 与 `run.log` 的批次标记需要一个「本次跑了哪些」的集合。没有它，
//! 报告只能统计 `state.jsonl` 全量历史 —— 图一多，报告就跟本次无关了。
//!
//! ── 与旧「批次」的区别 ──────────────────────────────────────────────
//! 旧批次是**执行单元**（结束 = worker 全退出）。会话只是**统计窗口**：
//! 调度器常驻，`enqueue` 可以发生在会话中途（追加），会话号码不变。

use std::collections::BTreeSet;

/// 当前会话。`id` 为 `None` = 空闲（没有未封口的会话）。
pub struct SessionState {
    id: Option<String>,
    /// 本会话（当前这次）投喂的 id —— **封口时取走**。
    current: BTreeSet<String>,
    /// 进程启动以来投喂过的**全部** id —— 只给 `shutdown` 兜底统计用。
    ///
    /// ⚠️ 必须与 `current` 分开：不分的话，常驻进程里第二个会话的报告
    /// 会把第一个会话的记录也算进去（报告张冠李戴）。
    all: BTreeSet<String>,
    /// 中途追加的轮数（首轮不算）。
    appended_rounds: usize,
}

impl Default for SessionState {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionState {
    pub fn new() -> Self {
        Self {
            id: None,
            current: BTreeSet::new(),
            all: BTreeSet::new(),
            appended_rounds: 0,
        }
    }

    /// 记一轮投喂。返回 `true` 表示**开启了新会话**。
    ///
    /// ⚠️ 会话号只用在日志与告警里，不参与任何文件名 —— 所以系统时间回拨
    /// 导致重号也无害（不像文件名那样会覆盖）。
    pub fn record(&mut self, ids: impl IntoIterator<Item = String>) -> bool {
        let opened = self.id.is_none();
        if opened {
            self.id = Some(chrono::Local::now().format("%Y%m%d-%H%M%S").to_string());
        } else {
            self.appended_rounds += 1;
        }
        for id in ids {
            self.current.insert(id.clone());
            self.all.insert(id);
        }
        opened
    }

    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// 本会话的 id（封口前的窗口）。
    pub fn ids(&self) -> &BTreeSet<String> {
        &self.current
    }

    /// 进程启动以来的全部 id（`shutdown` 兜底统计用）。
    pub fn all_ids(&self) -> &BTreeSet<String> {
        &self.all
    }

    pub fn appended_rounds(&self) -> usize {
        self.appended_rounds
    }

    pub fn is_open(&self) -> bool {
        self.id.is_some()
    }

    /// 封口：取走会话号与本会话 id，回到空闲。已封口返回 `None`（幂等判据）。
    pub fn close(&mut self) -> Option<(String, BTreeSet<String>)> {
        let id = self.id.take()?;
        self.appended_rounds = 0;
        Some((id, std::mem::take(&mut self.current)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 首轮开启会话_次轮只记追加() {
        let mut session = SessionState::new();
        assert!(!session.is_open());
        assert!(session.record(vec!["a".into()]), "首轮应开启会话");
        assert!(session.id().is_some());
        assert!(!session.record(vec!["b".into()]), "次轮不该换会话号");
        assert_eq!(session.appended_rounds(), 1);
        assert_eq!(session.ids().len(), 2);
    }

    #[test]
    fn 封口只取本会话_全部集合保留() {
        let mut session = SessionState::new();
        session.record(vec!["a".into()]);
        let (id, ids) = session.close().expect("应能封口");
        assert!(!id.is_empty());
        assert_eq!(ids.len(), 1);
        assert!(session.close().is_none(), "已封口再封口应返回 None");
        assert!(!session.is_open());
        // 第二个会话的报告不能把第一个会话的记录算进来，但 shutdown 兜底仍看得到全部。
        assert!(session.ids().is_empty());
        assert_eq!(session.all_ids().len(), 1);
    }
}
