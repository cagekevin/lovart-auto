//! AI 对话 —— **纯 LLM 对话核心**（`docs/plan/21` + `docs/plan/23`）。
//!
//! ── 它是什么、不是什么 ────────────────────────────────────────────────
//! 是：一次问答 + 会话历史回传 + 图片作输入上下文。
//! 不是：不出图、不发任务、不入待发队列、不带 tools、不做流式。
//!
//! ── 形状（只有四个）──────────────────────────────────────────────────
//! 1. 状态：`ChatState` / `Conversation` / `ChatMessage`（`store` 是唯一写者）
//! 2. 失败：`ChatError` 一个枚举，`reason/message/detail` 由它**一处**生成
//! 3. 结果：`SendResult` 三态，判据只有一个轴 —— **上游是否给了 `thread_id`**
//! 4. 契约：`SendArg` 进；`SendResult.to_json()` / `ChatError.to_json()` 出
//!
//! ── 为什么错误走 `Ok(status)` 而不是 `Err` ────────────────────────────
//! 业务函数返回 `Result<T, ChatError>`，而 `ChatError` **不是 `String`** ⇒
//! `gui.rs` 的 command 只能 `Ok(to_json(...))`；想让业务失败走 `Err`
//! 必须手写一层转换，不会顺手写出来（`ADR-0004`）。
//!
//! ── 上游：Lovart 自有通道（plan 23）──────────────────────────────────
//! `assistant` **不 import `crate::lovart`**：上游往返是注入的 [`ChatCompletion`]，
//! 生产实现是 [`lovart_chat::LovartCompletion`]，测试给桩。
//! 因此"组装 / 判失败 / 落盘"三条业务分支全部可以离线测。

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub mod lovart_chat;
pub mod store;

pub use store::ChatStore;

/// 会话文件格式版本。只认 `1`：不认识就报 `bad_history`，不猜、不就地改写。
pub const HISTORY_VERSION: u32 = 1;

/// 标题取首条 user 消息的前 N 个字符（`char` 计，不按字节）。
pub const TITLE_MAX_CHARS: usize = 16;

/// 图片读不到时写进请求正文的标记。气泡里的占位文案**必须**是同一句。
pub const MISSING_IMAGE_MARK: &str = "[图片缺失]";

/// 网关凭据的键名 —— 与出图**同一份**（`lovart/connection.rs` 读的就是这两个）。
pub mod keys {
    pub const ACCESS_KEY: &str = "LOVART_ACCESS_KEY";
    pub const SECRET_KEY: &str = "LOVART_SECRET_KEY";
}

// ────────────────────────────── 状态 ──────────────────────────────

/// 唯一真相 + 唯一投影。**任何改变状态的命令都返回它**，没有第二形状。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChatState {
    pub config: ConfigStatus,
    /// 顺序由后端定：`ts` 降序（新建在前）。插件不得重排。
    pub conversations: Vec<Conversation>,
    /// `None` = 还没有会话（不是空串）。
    pub active_id: Option<String>,
}

impl ChatState {
    pub fn conversation(&self, id: &str) -> Option<&Conversation> {
        self.conversations.iter().find(|conv| conv.id == id)
    }
}

/// "缺哪些"的集合，不是单值枚举：同时缺两个必须一次说完。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigStatus {
    pub missing: Vec<MissingPart>,
}

impl ConfigStatus {
    pub fn is_ready(&self) -> bool {
        self.missing.is_empty()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MissingPart {
    /// `data/chat-system-prompt.txt` 不存在 / 读不了
    SystemPrompt,
    /// `LOVART_ACCESS_KEY` / `LOVART_SECRET_KEY` 任一缺失
    Credentials,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Conversation {
    pub id: String,
    /// 空串 = 未起标题（前端显示「新会话」）。
    /// 首条消息写切片；**非空则永不覆盖**（重命名即写非空）。
    pub title: String,
    /// Unix 毫秒。
    pub ts: i64,
    pub messages: Vec<ChatMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub id: String,
    pub role: Role,
    pub text: String,
    /// 本机路径（不复制）。
    pub image_paths: Vec<String>,
    /// Unix 毫秒。
    pub created_at: i64,
}

// ────────────────────────────── 配置（宿主细节）──────────────────────────────

/// 宿主侧配置的**值**（不是全局）。生产端每条命令用 [`ChatConfig::from_env`] 重建，
/// 所以手改 `.env` 后下一条命令就生效，不需要缓存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatConfig {
    pub prompt_path: PathBuf,
    /// 网关凭据是否齐（`LOVART_ACCESS_KEY` + `LOVART_SECRET_KEY`，与出图同一对）。
    pub has_credentials: bool,
}

impl ChatConfig {
    /// **唯一**读进程环境的地方（`.env` 由 `load_dot_env` 在启动时注入）。
    pub fn from_env(prompt_path: PathBuf) -> Self {
        Self {
            prompt_path,
            has_credentials: env_non_empty(keys::ACCESS_KEY).is_some()
                && env_non_empty(keys::SECRET_KEY).is_some(),
        }
    }

    /// 缺哪些 —— `get_state`（只读打标）与 `send`（发送前拦截）**共用这一个**。
    ///
    /// 判据是"**读得出非空正文**"，不是 `is_file()`：0 字节或全空白的文件
    /// 传上去等于没有系统提示词，那是"缺"，不是"有"。
    pub fn status(&self) -> ConfigStatus {
        let mut missing = Vec::new();
        if !self.prompt_text().is_some_and(|text| !text.trim().is_empty()) {
            missing.push(MissingPart::SystemPrompt);
        }
        if !self.has_credentials {
            missing.push(MissingPart::Credentials);
        }
        ConfigStatus { missing }
    }

    fn prompt_text(&self) -> Option<String> {
        std::fs::read_to_string(&self.prompt_path).ok()
    }

    /// 系统提示词正文。读不到 / 读到空白 ⇒ `MissingConfig`（**不空串、不默认句**）。
    pub fn prompt(&self) -> Result<String, ChatError> {
        match self.prompt_text() {
            Some(text) if !text.trim().is_empty() => Ok(text),
            Some(_) => Err(self.missing_error(
                vec![MissingPart::SystemPrompt],
                "文件是空的，请写入一句系统提示词",
            )),
            None => Err(self.missing_error(
                vec![MissingPart::SystemPrompt],
                "读不了这个文件",
            )),
        }
    }

    fn missing_error(&self, missing: Vec<MissingPart>, why: &str) -> ChatError {
        ChatError::MissingConfig {
            missing,
            prompt_path: self.prompt_path.display().to_string(),
            why: why.to_string(),
        }
    }
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

// ────────────────────────────── 失败 ──────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamKind {
    Http,
    Envelope,
    Timeout,
    Io,
}

impl UpstreamKind {
    fn as_str(self) -> &'static str {
        match self {
            UpstreamKind::Http => "http",
            UpstreamKind::Envelope => "envelope",
            UpstreamKind::Timeout => "timeout",
            UpstreamKind::Io => "io",
        }
    }
}

/// 业务失败**只有一个类型**，`reason/message/detail` 只在这里生成一次。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatError {
    BadHistory {
        path: String,
        why: String,
    },
    NotFound {
        id: String,
    },
    MissingConfig {
        missing: Vec<MissingPart>,
        prompt_path: String,
        why: String,
    },
    BadImage {
        path: String,
        why: String,
    },
    Upstream {
        kind: UpstreamKind,
        why: String,
    },
    PersistFailed {
        why: String,
    },
}

impl ChatError {
    /// 线上判别位（`reason`）。
    pub fn reason(&self) -> &'static str {
        match self {
            ChatError::BadHistory { .. } => "bad_history",
            ChatError::NotFound { .. } => "not_found",
            ChatError::MissingConfig { .. } => "missing_config",
            ChatError::BadImage { .. } => "bad_image",
            ChatError::Upstream { kind, .. } => match kind {
                UpstreamKind::Http => "upstream_http",
                UpstreamKind::Envelope => "upstream_envelope",
                UpstreamKind::Timeout => "upstream_timeout",
                UpstreamKind::Io => "upstream_io",
            },
            ChatError::PersistFailed { .. } => "persist_failed",
        }
    }

    /// 给人看的文案：**必须含路径与动作**（本项目没有设置页）。
    pub fn message(&self) -> String {
        match self {
            ChatError::BadHistory { path, why } => {
                format!("会话记录有问题，没有改动任何内容：\n{path}\n{why}")
            }
            ChatError::NotFound { id } => format!("会话不存在（可能已被删除）：{id}"),
            ChatError::MissingConfig {
                missing,
                prompt_path,
                why,
            } => {
                let mut lines = vec![format!("还不能发送：{why}")];
                for part in missing {
                    lines.push(match part {
                        MissingPart::SystemPrompt => {
                            format!("· 系统提示词：请写入 {prompt_path} 后重试")
                        }
                        MissingPart::Credentials => {
                            "· 网关凭据：在 data/.env 里配置 LOVART_ACCESS_KEY 与 LOVART_SECRET_KEY".to_string()
                        }
                    });
                }
                lines.join("\n")
            }
            ChatError::BadImage { path, why } => {
                format!("这张图发不出去，请重新添加一次：\n{path}\n{why}")
            }
            ChatError::Upstream { kind, why } => match kind {
                UpstreamKind::Timeout => {
                    format!("上游超时：请求已经发出，可能已计费，**不要重复发送**。\n{why}")
                }
                _ => format!("上游失败：{why}"),
            },
            ChatError::PersistFailed { why } => format!(
                "回复已拿到，但保存会话失败。内容不要再发一次（可能已计费）。\n{why}"
            ),
        }
    }

    /// 给日志用的一行（不复述界面文案）。
    pub fn detail(&self) -> String {
        match self {
            ChatError::BadHistory { path, why } => format!("path={path} why={why}"),
            ChatError::NotFound { id } => format!("id={id}"),
            ChatError::MissingConfig { missing, why, .. } => {
                let parts: Vec<&str> = missing
                    .iter()
                    .map(|part| match part {
                        MissingPart::SystemPrompt => "system_prompt",
                        MissingPart::Credentials => "credentials",
                    })
                    .collect();
                format!("missing={} why={why}", parts.join(","))
            }
            ChatError::BadImage { path, why } => format!("path={path} why={why}"),
            ChatError::Upstream { kind, why } => format!("kind={} why={why}", kind.as_str()),
            ChatError::PersistFailed { why } => format!("why={why}"),
        }
    }

    fn payload(&self, status: &str) -> Value {
        json!({
            "status": status,
            "reason": self.reason(),
            "message": self.message(),
            "detail": self.detail(),
        })
    }

    /// 读/写类命令的失败形状：`{status:'error', reason, message, detail}`。
    pub fn to_json(&self) -> Value {
        self.payload("error")
    }
}

/// 一次发送的结果。**判据只有一个轴：上游是否给了 `thread_id`。**
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendResult {
    /// 拿到回复、落盘成功。回复就是 `state` 里最后一条 assistant。
    Ok { state: ChatState },
    /// 没提交上（连接 / 非 2xx / 信封坏 / 上传失败 / 组装失败）⇒ 可原样重试。
    NotSent(ChatError),
    /// 提交上了（钱可能花了）⇒ **禁止自动重试**；`reply` 只在本次界面显示。
    Delivered {
        error: ChatError,
        reply: Option<String>,
    },
}

impl SendResult {
    pub fn to_json(&self) -> Value {
        match self {
            SendResult::Ok { state } => json!({ "status": "ok", "state": state }),
            SendResult::NotSent(error) => error.payload("not_sent"),
            SendResult::Delivered { error, reply } => {
                let mut payload = error.payload("delivered");
                if let Some(reply) = reply {
                    payload["reply"] = json!(reply);
                }
                payload
            }
        }
    }
}

// ────────────────────────── 上游往返（可注入）──────────────────────────

/// "把一段 prompt 和几张图变成一段回复"——**唯一**的上游契约（plan 23 §1）。
///
/// 生产实现是 [`lovart_chat::LovartCompletion`]；测试给桩。
/// `assistant` 不 import `lovart` ⇒ 业务分支（组装、判失败、落盘）能离线测。
pub trait ChatCompletion {
    fn complete(&self, prompt: &str, attachments: &[String]) -> Result<String, CompleteError>;
}

/// 传输层的两种失败。**分界只有一条：拿到 `thread_id` 没有。**
///
/// 不用 "2xx" 当分界：这条通道是「提交 → 轮询 → 取结果」三段，2xx 只覆盖第一段，
/// 而计费发生在"上游接受并建了 thread"那一刻。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompleteError {
    /// 没提交上 ⇒ 可重试
    NotSent { kind: UpstreamKind, why: String },
    /// 提交上了（钱可能花了）⇒ 禁止重试
    Accepted { kind: UpstreamKind, why: String },
}

// ────────────────────────────── 发送 ──────────────────────────────

/// 线上入参（`assistant_send` 的载荷）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendArg {
    pub conversation_id: String,
    pub text: String,
    #[serde(default)]
    pub image_paths: Vec<String>,
}

/// 一次发送的完整流程。
///
/// 锁只在两段同步代码里持有：①取历史快照 ②追加本轮到真相文件。
/// **上游往返在锁外**（`I-2`）—— 用户此刻还能切会话、改名、删会话。
pub fn send(
    store: &Mutex<ChatStore>,
    arg: &SendArg,
    transport: &dyn ChatCompletion,
) -> SendResult {
    // ① 锁内：取配置与历史快照
    let (config, conversation) = {
        let guard = lock(store);
        let state = match guard.state() {
            Ok(state) => state,
            Err(error) => return SendResult::NotSent(error),
        };
        let conversation = match state.conversation(&arg.conversation_id) {
            Some(conversation) => conversation.clone(),
            None => {
                return SendResult::NotSent(ChatError::NotFound {
                    id: arg.conversation_id.clone(),
                })
            }
        };
        (guard.config().clone(), conversation)
    }; // ← 锁在这里释放

    // ② 锁外：先查配置（缺就当场停住，别掉到传输层被报成"上游失败"）
    let status = config.status();
    if !status.is_ready() {
        return SendResult::NotSent(ChatError::MissingConfig {
            missing: status.missing,
            prompt_path: config.prompt_path.display().to_string(),
            why: "对话还没有配好".to_string(),
        });
    }

    let prompt = match config.prompt() {
        Ok(prompt) => prompt,
        Err(error) => return SendResult::NotSent(error),
    };

    let (body, attachments) =
        match flatten_prompt(&prompt, &conversation, &arg.text, &arg.image_paths) {
            Ok(pair) => pair,
            Err(error) => return SendResult::NotSent(error),
        };

    let reply = match transport.complete(&body, &attachments) {
        Ok(reply) => reply,
        Err(CompleteError::NotSent { kind, why }) => {
            return SendResult::NotSent(ChatError::Upstream { kind, why })
        }
        Err(CompleteError::Accepted { kind, why }) => {
            return SendResult::Delivered {
                error: ChatError::Upstream { kind, why },
                reply: None,
            }
        }
    };

    // ③ 锁内：重新读文件、按 id 追加（I-6）—— 不用 ① 的快照写回
    let mut guard = lock(store);
    match guard.append_turn(&arg.conversation_id, &arg.text, &arg.image_paths, &reply) {
        Ok(state) => SendResult::Ok { state },
        Err(error) => SendResult::Delivered {
            error,
            reply: Some(reply),
        },
    }
}

fn lock(store: &Mutex<ChatStore>) -> MutexGuard<'_, ChatStore> {
    store.lock().unwrap_or_else(|error| error.into_inner())
}

/// 组装：一段 `prompt` + 附件（本机路径）。
///
/// 上游只吃「一段字符串 + 一组附件 URL」，所以 system / 历史 / 本轮必须拼成一段文字
/// （`docs/plan/23` §4）。图片**不进正文**，走附件；缺图按 I-6 分两种：
/// **历史**读不到 ⇒ 该条正文追加 `[图片缺失]` 并继续；**本轮**读不到 ⇒ `BadImage`。
pub fn flatten_prompt(
    prompt: &str,
    conversation: &Conversation,
    text: &str,
    current_images: &[String],
) -> Result<(String, Vec<String>), ChatError> {
    let mut blocks: Vec<String> = Vec::with_capacity(conversation.messages.len() + 2);
    blocks.push(prompt.trim_end().to_string());

    let mut attachments: Vec<String> = Vec::new();

    for message in &conversation.messages {
        let who = match message.role {
            Role::User => "用户",
            Role::Assistant => "助手",
        };
        let mut line = format!("{who}：{}", message.text);
        for path in &message.image_paths {
            if std::path::Path::new(path).is_file() {
                // 同一张图在多条消息里出现只传一次（否则一次发送会把同一文件上传 N 遍）
                push_unique(&mut attachments, path);
            } else {
                line.push(' ');
                line.push_str(MISSING_IMAGE_MARK);
            }
        }
        blocks.push(line);
    }

    for path in current_images {
        if let Err(error) = std::fs::metadata(path) {
            return Err(ChatError::BadImage {
                path: path.clone(),
                why: format!("读不了：{error}"),
            });
        }
        push_unique(&mut attachments, path);
    }
    blocks.push(format!("用户：{text}"));

    Ok((blocks.join("\n\n"), attachments))
}

fn push_unique(list: &mut Vec<String>, path: &str) {
    if !list.iter().any(|existing| existing == path) {
        list.push(path.to_string());
    }
}

/// 标题切片（`char` 计，不按字节；不 trim 掉中间的空格）。
pub fn title_from(text: &str) -> String {
    text.chars()
        .take(TITLE_MAX_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation_with(messages: Vec<ChatMessage>) -> Conversation {
        Conversation {
            id: "c1".to_string(),
            title: String::new(),
            ts: 1,
            messages,
        }
    }

    fn message(role: Role, text: &str, images: &[&str]) -> ChatMessage {
        ChatMessage {
            id: "m1".to_string(),
            role,
            text: text.to_string(),
            image_paths: images.iter().map(|path| path.to_string()).collect(),
            created_at: 1,
        }
    }

    fn temp_png(tag: &str) -> String {
        let path =
            std::env::temp_dir().join(format!("lovart-chat-{tag}-{}.png", std::process::id()));
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n").expect("写临时 png");
        path.display().to_string()
    }

    fn ready_store(tag: &str) -> std::sync::Arc<Mutex<ChatStore>> {
        let dir = std::env::temp_dir().join(format!("lovart-chat-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let prompt_path = dir.join("prompt.txt");
        std::fs::write(&prompt_path, "你是助手").expect("写提示词");
        std::sync::Arc::new(Mutex::new(ChatStore::new(
            dir.join("chat-history.json"),
            ChatConfig {
                prompt_path,
                has_credentials: true,
            },
        )))
    }

    /// 测试用上游：只回一段固定正文，或按指定错误失败。
    struct Stub {
        result: Result<String, CompleteError>,
    }

    /// 「请求进行中，另一边新建了会话」——用来证明 send 不拿 t0 快照写回。
    struct NewConversationDuringRequest {
        store: std::sync::Arc<Mutex<ChatStore>>,
        reply: String,
    }

    /// 「请求进行中，另一边把这个会话删了」——回复到达时目标已不存在。
    struct DeleteDuringRequest {
        store: std::sync::Arc<Mutex<ChatStore>>,
        id: String,
    }

    impl ChatCompletion for DeleteDuringRequest {
        fn complete(
            &self,
            _prompt: &str,
            _attachments: &[String],
        ) -> Result<String, CompleteError> {
            self.store.lock().unwrap().delete(&self.id).expect("请求期间删除");
            Ok("答一".to_string())
        }
    }

    impl ChatCompletion for NewConversationDuringRequest {
        fn complete(
            &self,
            _prompt: &str,
            _attachments: &[String],
        ) -> Result<String, CompleteError> {
            self.store.lock().unwrap().new_conversation().expect("请求期间新建");
            Ok(self.reply.clone())
        }
    }

    impl ChatCompletion for Stub {
        fn complete(&self, _prompt: &str, _attachments: &[String]) -> Result<String, CompleteError> {
            self.result.clone()
        }
    }

    #[test]
    fn 组装_system_加全部历史加本轮() {
        let history = vec![
            message(Role::User, "第一问", &[]),
            message(Role::Assistant, "第一答", &[]),
        ];
        let conversation = conversation_with(history);

        let (body, attachments) =
            flatten_prompt("你是助手", &conversation, "第二问", &[]).expect("组装");

        assert_eq!(
            body,
            "你是助手\n\n用户：第一问\n\n助手：第一答\n\n用户：第二问"
        );
        assert!(attachments.is_empty());
        assert!(!body.contains("tools"), "正文里不该出现 tools");
    }

    #[test]
    fn 组装_本轮图片进附件而不进正文() {
        let png = temp_png("turn");
        let conversation = conversation_with(vec![]);

        let (body, attachments) =
            flatten_prompt("p", &conversation, "看图", &[png.clone()]).expect("组装");

        assert_eq!(body, "p\n\n用户：看图");
        assert_eq!(attachments, vec![png], "图片走附件（CDN 上传），不进正文");
    }

    #[test]
    fn 组装_本轮坏图报_bad_image() {
        let conversation = conversation_with(vec![]);
        let error = flatten_prompt("p", &conversation, "x", &["C:/no/such.png".into()])
            .expect_err("坏图应报错");
        assert_eq!(error.reason(), "bad_image");
        assert!(error.message().contains("C:/no/such.png"));
    }

    #[test]
    fn 组装_历史坏图降级成标记且不进附件() {
        let png = temp_png("history");
        let history = vec![message(Role::User, "当时有图", &[png.as_str()])];
        let conversation = conversation_with(history);

        std::fs::remove_file(&png).expect("删图");
        let (body, attachments) = flatten_prompt("p", &conversation, "再看", &[]).expect("历史缺图不该报错");

        assert!(body.contains(MISSING_IMAGE_MARK), "正文里要有同一句标记");
        assert!(attachments.is_empty(), "读不到的图不许进附件");
    }

    #[test]
    fn 组装_历史图能读时进附件() {
        let png = temp_png("history-ok");
        let history = vec![message(Role::User, "当时有图", &[png.as_str()])];
        let conversation = conversation_with(history);

        let (_, attachments) = flatten_prompt("p", &conversation, "再看", &[]).expect("组装");
        assert_eq!(attachments, vec![png], "历史图照传（与参照实现一致）");
    }

    #[test]
    fn 结果三态各有独立判别位() {
        let not_sent = SendResult::NotSent(ChatError::NotFound { id: "c1".into() });
        assert_eq!(not_sent.to_json()["status"], "not_sent");
        assert_eq!(not_sent.to_json()["reason"], "not_found");

        let delivered = SendResult::Delivered {
            error: ChatError::PersistFailed {
                why: "磁盘满".into(),
            },
            reply: Some("回复".into()),
        };
        assert_eq!(delivered.to_json()["status"], "delivered");
        assert_eq!(delivered.to_json()["reply"], "回复");
        assert_eq!(delivered.to_json()["reason"], "persist_failed");

        assert_eq!(
            ChatError::BadHistory {
                path: "p".into(),
                why: "w".into()
            }
            .to_json()["status"],
            "error"
        );
    }

    #[test]
    fn 配置_缺什么就列什么且一次说完() {
        let config = ChatConfig {
            prompt_path: PathBuf::from("C:/no/such/prompt.txt"),
            has_credentials: false,
        };
        assert_eq!(
            config.status().missing,
            vec![MissingPart::SystemPrompt, MissingPart::Credentials],
            "两项一起说完，不许修一个撞一个"
        );

        let error = config.prompt().expect_err("缺提示词");
        assert_eq!(error.reason(), "missing_config");
        assert!(error.message().contains("C:/no/such/prompt.txt"), "文案必须含路径");
    }

    #[test]
    fn 缺配置时发送直接_not_sent_且不打上游() {
        let store = ready_store("send-missing");
        let id = store
            .lock()
            .unwrap()
            .new_conversation()
            .expect("新建")
            .active_id
            .unwrap();

        let mut config = store.lock().unwrap().config().clone();
        config.prompt_path = PathBuf::from("C:/no/such/prompt.txt");
        config.has_credentials = false;
        store.lock().unwrap().set_config(config);

        let result = send(
            &store,
            &SendArg {
                conversation_id: id,
                text: "在吗".to_string(),
                image_paths: Vec::new(),
            },
            &Stub {
                result: Err(CompleteError::NotSent {
                    kind: UpstreamKind::Io,
                    why: "不该被调用".to_string(),
                }),
            },
        );

        match result {
            SendResult::NotSent(error) => {
                assert_eq!(error.reason(), "missing_config");
                assert!(error.message().contains("LOVART_ACCESS_KEY"), "要给出键名");
                assert!(
                    error.message().contains("C:/no/such/prompt.txt"),
                    "两项一次说完（提示词也要给路径）"
                );
            }
            other => panic!("应为 not_sent，实际 {other:?}"),
        }
    }

    #[test]
    fn 提交无_thread_id_算未发出() {
        let store = ready_store("not-sent");
        let id = store
            .lock()
            .unwrap()
            .new_conversation()
            .expect("新建")
            .active_id
            .unwrap();
        let before = std::fs::read_to_string(store.lock().unwrap().path()).unwrap();

        let result = send(
            &store,
            &SendArg {
                conversation_id: id,
                text: "问".to_string(),
                image_paths: Vec::new(),
            },
            &Stub {
                result: Err(CompleteError::NotSent {
                    kind: UpstreamKind::Http,
                    why: "连接失败".to_string(),
                }),
            },
        );

        assert!(matches!(result, SendResult::NotSent(_)), "未提交 ⇒ not_sent");
        assert_eq!(
            std::fs::read_to_string(store.lock().unwrap().path()).unwrap(),
            before,
            "未发出时文件一个字节都不许动"
        );
    }

    #[test]
    fn 请求期间被删_算已提交但不写盘() {
        let store = ready_store("deleted-in-flight");
        let id = store
            .lock()
            .unwrap()
            .new_conversation()
            .expect("新建")
            .active_id
            .unwrap();

        let transport = DeleteDuringRequest {
            store: store.clone(),
            id: id.clone(),
        };
        let result = send(
            &store,
            &SendArg {
                conversation_id: id.clone(),
                text: "问".to_string(),
                image_paths: Vec::new(),
            },
            &transport,
        );

        match result {
            SendResult::Delivered { error, reply } => {
                assert_eq!(error.reason(), "not_found", "目标已不存在");
                assert_eq!(reply.as_deref(), Some("答一"), "回复仍要在界面上显示");
            }
            other => panic!("应为 delivered，实际 {other:?}"),
        }
        assert!(
            store.lock().unwrap().state().unwrap().conversation(&id).is_none(),
            "文件里不许出现幽灵会话"
        );
    }

    #[test]
    fn 已提交但落盘失败算_delivered_且带回复() {
        let store = ready_store("persist-fail");
        let id = store
            .lock()
            .unwrap()
            .new_conversation()
            .expect("新建")
            .active_id
            .unwrap();

        let path = store.lock().unwrap().path().to_path_buf();
        let mut perms = std::fs::metadata(&path).expect("读权限").permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).expect("设只读");

        let result = send(
            &store,
            &SendArg {
                conversation_id: id,
                text: "问".to_string(),
                image_paths: Vec::new(),
            },
            &Stub {
                result: Ok("答一".to_string()),
            },
        );

        // 还原权限，免得清理临时目录时被别人挡（测试自身也不该留下只读文件）
        let mut perms = std::fs::metadata(&path).expect("读权限").permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        let _ = std::fs::set_permissions(&path, perms);

        match result {
            SendResult::Delivered { error, reply } => {
                assert_eq!(error.reason(), "persist_failed");
                assert_eq!(reply.as_deref(), Some("答一"), "回复不许丢");
            }
            other => panic!("应为 delivered，实际 {other:?}"),
        }
    }

    #[test]
    fn 请求期间切会话_不会被快照写回覆盖() {
        let store = ready_store("in-flight");
        let id = store
            .lock()
            .unwrap()
            .new_conversation()
            .expect("新建")
            .active_id
            .unwrap();

        // 请求「进行中」时，另一边新建了一个会话（activeId 变成新会话）
        let transport = NewConversationDuringRequest {
            store: store.clone(),
            reply: "答一".to_string(),
        };
        let result = send(
            &store,
            &SendArg {
                conversation_id: id.clone(),
                text: "问一".to_string(),
                image_paths: Vec::new(),
            },
            &transport,
        );

        let state = match result {
            SendResult::Ok { state } => state,
            other => panic!("应成功，实际 {other:?}"),
        };
        assert_ne!(state.active_id.as_deref(), Some(id.as_str()), "activeId 应停在新会话");
        let original = state.conversation(&id).expect("原会话还在");
        assert_eq!(original.messages.len(), 2, "回复写回了原会话");
        assert_eq!(original.messages[1].text, "答一");
    }
}
