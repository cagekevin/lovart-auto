//! 会话真相文件的**唯一写者**（`data/chat-history.json`）。
//!
//! ── 三条不变量（判对错只看它们）──────────────────────────────────────
//! `I-1` 每次写 = **重新读文件 → 改声明的那一项 → 原子写**。
//!       不缓存整表，不用上一命令读到的快照。
//! `I-4` 文件不存在 = 空状态；文件在但 JSON 坏 / 形状非法 / 版本不认识 =
//!       `bad_history`（**绝不回空列表**——"没有"和"读不出来"是两件事）。
//! `I-7` 删除 = 整条移除 + `active_id` 迁移，**同一次写**。
//!
//! ── 为什么本模块不碰锁 ────────────────────────────────────────────────
//! 并发闸在 `gui.rs`：一个 `Mutex<ChatStore>`。`assistant::send` 会在
//! "取快照"与"追加本轮"之间**放开锁**去发请求（`I-2`），锁的持有范围
//! 由调用方决定，所以这里只提供短事务方法、不自己加锁。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::{
    title_from, ChatConfig, ChatError, ChatMessage, ChatState, Conversation, Role, HISTORY_VERSION,
};

/// 同进程内的单调序号：同一毫秒内连续新建也不会撞 id（跨进程靠毫秒）。
static SEQ: AtomicU64 = AtomicU64::new(0);

fn next_id(prefix: &str) -> String {
    format!(
        "{prefix}{}-{}",
        Utc::now().timestamp_millis(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// 文件形状（`docs/plan/21` §3）：`{ version, activeId, conversations }`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct History {
    version: u32,
    active_id: Option<String>,
    conversations: Vec<Conversation>,
}

impl Default for History {
    fn default() -> Self {
        Self {
            version: HISTORY_VERSION,
            active_id: None,
            conversations: Vec::new(),
        }
    }
}

/// 唯一碰 `chat-history.json` 的类型。路径由 `Workbench` 具名访问器给。
pub struct ChatStore {
    path: PathBuf,
    config: ChatConfig,
}

impl ChatStore {
    pub fn new(path: PathBuf, config: ChatConfig) -> Self {
        Self { path, config }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn config(&self) -> &ChatConfig {
        &self.config
    }

    /// 刷新宿主配置。**每条命令开头调一次**：`.env` 是进程环境注入的，
    /// 不刷新就会拿启动那一刻的值（用户手改后必须重开程序，属于第二处真相）。
    pub fn set_config(&mut self, config: ChatConfig) {
        self.config = config;
    }

    /// 只读：不打标任何会话，不暴露"当前选中"以外的状态。
    pub fn state(&self) -> Result<ChatState, ChatError> {
        let history = self.load()?;
        Ok(self.to_state(history))
    }

    /// 新建 + 立即成为 active（同一次写）。
    pub fn new_conversation(&mut self) -> Result<ChatState, ChatError> {
        let mut history = self.load()?;
        let conversation = Conversation {
            id: next_id("c"),
            title: String::new(),
            ts: Utc::now().timestamp_millis(),
            messages: Vec::new(),
        };
        history.active_id = Some(conversation.id.clone());
        history.conversations.insert(0, conversation); // 顺序：ts 降序 ⇒ 新建在前
        self.save(&history)?;
        Ok(self.to_state(history))
    }

    /// 切换 active。id 不存在 ⇒ `not_found`（**不新建代替**）。
    pub fn select(&mut self, id: &str) -> Result<ChatState, ChatError> {
        let mut history = self.load()?;
        if !history.conversations.iter().any(|conv| conv.id == id) {
            return Err(ChatError::NotFound { id: id.to_string() });
        }
        history.active_id = Some(id.to_string());
        self.save(&history)?;
        Ok(self.to_state(history))
    }

    /// 写标题。空串等价于「回到未起标题」（前端对空输入按取消处理）。
    pub fn rename(&mut self, id: &str, title: &str) -> Result<ChatState, ChatError> {
        let mut history = self.load()?;
        let conversation = history
            .conversations
            .iter_mut()
            .find(|conv| conv.id == id)
            .ok_or_else(|| ChatError::NotFound { id: id.to_string() })?;
        conversation.title = title.trim().to_string();
        self.save(&history)?;
        Ok(self.to_state(history))
    }

    /// 整条移除 + `active_id` 迁移（同一次写，`I-7`）。
    pub fn delete(&mut self, id: &str) -> Result<ChatState, ChatError> {
        let mut history = self.load()?;
        let before = history.conversations.len();
        history.conversations.retain(|conv| conv.id != id);
        if history.conversations.len() == before {
            return Err(ChatError::NotFound { id: id.to_string() });
        }
        if history.active_id.as_deref() == Some(id) {
            history.active_id = history.conversations.first().map(|conv| conv.id.clone());
        }
        self.save(&history)?;
        Ok(self.to_state(history))
    }

    /// 一轮提交：追加 user + assistant，首条消息时写标题。**一次写**（`I-6`）。
    pub fn append_turn(
        &mut self,
        id: &str,
        text: &str,
        image_paths: &[String],
        reply: &str,
    ) -> Result<ChatState, ChatError> {
        let mut history = self.load()?;
        let conversation = history
            .conversations
            .iter_mut()
            .find(|conv| conv.id == id)
            .ok_or_else(|| ChatError::NotFound { id: id.to_string() })?;

        let now = Utc::now().timestamp_millis();
        conversation.messages.push(ChatMessage {
            id: next_id("m"),
            role: Role::User,
            text: text.to_string(),
            image_paths: image_paths.to_vec(),
            created_at: now,
        });
        conversation.messages.push(ChatMessage {
            id: next_id("m"),
            role: Role::Assistant,
            text: reply.to_string(),
            image_paths: Vec::new(),
            created_at: now,
        });
        if conversation.title.is_empty() {
            conversation.title = title_from(text);
        }

        self.save(&history)?;
        Ok(self.to_state(history))
    }

    // ── 内部 ──────────────────────────────────────────────────────

    fn to_state(&self, history: History) -> ChatState {
        ChatState {
            config: self.config.status(),
            conversations: history.conversations,
            active_id: history.active_id,
        }
    }

    fn load(&self) -> Result<History, ChatError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(History::default()),
            Err(error) => {
                return Err(ChatError::BadHistory {
                    path: self.path.display().to_string(),
                    why: format!("读不了：{error}"),
                })
            }
        };

        let history: History = serde_json::from_str(&text).map_err(|error| ChatError::BadHistory {
            path: self.path.display().to_string(),
            why: format!("格式不对：{error}"),
        })?;

        if history.version != HISTORY_VERSION {
            return Err(ChatError::BadHistory {
                path: self.path.display().to_string(),
                why: format!(
                    "版本 {} 不认识（只认 {HISTORY_VERSION}）—— 不猜、不就地改写",
                    history.version
                ),
            });
        }

        if let Some(active) = &history.active_id {
            if !history.conversations.iter().any(|conv| &conv.id == active) {
                return Err(ChatError::BadHistory {
                    path: self.path.display().to_string(),
                    why: format!("activeId 指向不存在的会话：{active}"),
                });
            }
        }

        Ok(history)
    }

    fn save(&self, history: &History) -> Result<(), ChatError> {
        let body = serde_json::to_string_pretty(history).map_err(|error| ChatError::PersistFailed {
            why: format!("序列化会话失败：{error}"),
        })?;
        crate::fsio::write_atomic(&self.path, body.as_bytes()).map_err(|error| {
            ChatError::PersistFailed {
                why: format!("写不了 {}：{error}", self.path.display()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::MissingPart;

    fn temp_store(tag: &str) -> ChatStore {
        let dir = std::env::temp_dir().join(format!("lovart-chat-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let prompt_path = dir.join("prompt.txt");
        std::fs::write(&prompt_path, "你是助手").expect("写提示词");
        ChatStore::new(
            dir.join("chat-history.json"),
            ChatConfig {
                prompt_path,
                has_credentials: true,
            },
        )
    }

    #[test]
    fn 文件不存在_是空状态而不是错误() {
        let store = temp_store("empty");
        let state = store.state().expect("空状态");
        assert!(state.conversations.is_empty());
        assert_eq!(state.active_id, None, "没有会话 = None，不是空串");
        assert!(state.config.is_ready(), "配置齐全");
    }

    #[test]
    fn 新建_立即成为_active_且列表新建在前() {
        let mut store = temp_store("new");
        let first = store.new_conversation().expect("新建一");
        let first_id = first.active_id.clone().expect("active");
        let second = store.new_conversation().expect("新建二");
        let second_id = second.active_id.clone().expect("active");

        assert_ne!(first_id, second_id, "id 不撞");
        assert_eq!(second.active_id.as_deref(), Some(second_id.as_str()));
        assert_eq!(second.conversations[0].id, second_id, "新建在前");
        assert_eq!(second.conversations[1].id, first_id);
    }

    #[test]
    fn 切换不存在的会话_报_not_found_且不新建() {
        let mut store = temp_store("select");
        store.new_conversation().expect("新建");
        let error = store.select("nope").expect_err("应报错");
        assert_eq!(error.reason(), "not_found");
        assert_eq!(store.state().unwrap().conversations.len(), 1);
    }

    #[test]
    fn 改名只动_title_别的字段不变() {
        let mut store = temp_store("rename");
        let state = store.new_conversation().expect("新建");
        let id = state.active_id.clone().unwrap();

        let renamed = store.rename(&id, "  我的会话  ").expect("改名");
        let conversation = &renamed.conversations[0];
        assert_eq!(conversation.title, "我的会话", "两端空白被 trim");
        assert_eq!(conversation.ts, state.conversations[0].ts, "ts 不动");
        assert!(conversation.messages.is_empty(), "消息不动");
    }

    #[test]
    fn 删除当前会话_active_迁到第一条_删空回空() {
        let mut store = temp_store("delete");
        store.new_conversation().expect("一");
        let second = store.new_conversation().expect("二");
        let active = second.active_id.clone().unwrap();
        let older = second.conversations[1].id.clone();

        let after = store.delete(&active).expect("删当前");
        assert_eq!(after.active_id.as_deref(), Some(older.as_str()), "迁到第一条");
        assert_eq!(after.conversations.len(), 1);

        let empty = store.delete(&older).expect("删最后一条");
        assert_eq!(empty.active_id, None);
        assert!(empty.conversations.is_empty());
    }

    #[test]
    fn 删除不存在的会话_报_not_found_且文件不变() {
        let mut store = temp_store("delete-missing");
        store.new_conversation().expect("新建");
        let before = std::fs::read_to_string(store.path()).unwrap();
        assert_eq!(store.delete("nope").expect_err("应报错").reason(), "not_found");
        assert_eq!(std::fs::read_to_string(store.path()).unwrap(), before);
    }

    #[test]
    fn 坏_json_报_bad_history_不回空列表() {
        let store = temp_store("bad-json");
        std::fs::write(store.path(), b"{not json").expect("写坏文件");

        let error = store.state().expect_err("应报错");
        assert_eq!(error.reason(), "bad_history");
        assert!(error.message().contains("chat-history.json"), "文案必须含路径");
    }

    #[test]
    fn 版本不认识_或_active_指向不存在_都算_bad_history() {
        let store = temp_store("version");
        std::fs::write(store.path(), br#"{"version":2,"activeId":null,"conversations":[]}"#)
            .expect("写版本 2");
        assert_eq!(store.state().expect_err("版本 2").reason(), "bad_history");

        std::fs::write(
            store.path(),
            br#"{"version":1,"activeId":"ghost","conversations":[]}"#,
        )
        .expect("写幽灵 active");
        assert_eq!(store.state().expect_err("幽灵 active").reason(), "bad_history");
    }

    #[test]
    fn 一轮提交_追加两条_首条写标题_之后不覆盖() {
        let mut store = temp_store("append");
        let id = store.new_conversation().expect("新建").active_id.unwrap();

        let after = store
            .append_turn(&id, "第一个问题的前十六个字会被当成标题吧", &[], "答一")
            .expect("第一轮");
        let conversation = &after.conversations[0];
        assert_eq!(conversation.messages.len(), 2);
        assert_eq!(conversation.messages[0].role, Role::User);
        assert_eq!(conversation.messages[1].role, Role::Assistant);
        assert_eq!(
            conversation.title,
            "第一个问题的前十六个字会被当成标",
            "char 计、16 字切片"
        );

        store.rename(&id, "改过的标题").expect("改名");
        let after = store
            .append_turn(&id, "第二个问题", &[], "答二")
            .expect("第二轮");
        assert_eq!(after.conversations[0].messages.len(), 4);
        assert_eq!(after.conversations[0].title, "改过的标题", "非空标题永不覆盖");
    }

    #[test]
    fn 追加到不存在的会话_报_not_found() {
        let mut store = temp_store("append-missing");
        let error = store.append_turn("nope", "x", &[], "y").expect_err("应报错");
        assert_eq!(error.reason(), "not_found");
    }

    #[test]
    fn 配置缺失不阻断读取_只打标() {
        let mut store = temp_store("config");
        store.new_conversation().expect("新建");
        store.config.has_credentials = false;

        let state = store.state().expect("配置缺失不该让读失败");
        assert_eq!(
            state.config.missing,
            vec![MissingPart::Credentials],
            "提示词文件在 ⇒ 只缺凭据"
        );
        assert_eq!(state.conversations.len(), 1, "历史照常可读");
    }
}
