//! `lovart_client` — Lovart 原生协议原语（对应 TS `lovart/lovart_client.ts`）。
//!
//! 信封 `{code, message, data}` 在此剥离；`code != 0` 抛带上游原始文案的错（不翻译 / 不静默）。
//! 非 2xx 抛 `LovartError::Upstream`，`code` 即 **HTTP 状态码**，供上层判定是否可重试。
//!
//! ── 为什么是**阻塞** HTTP，而不是 async / tokio ──────────────────────
//! 1. 并发只有 ≤8（服务端槽位上限），全程在等 API —— tokio 的价值（海量连接）用不上；
//! 2. 阻塞代码**没有 `Send`/`Sync` 约束、没有 lifetime、没有 `select!`** ——
//!    对一个 AI 主写、人审的库，这是实打实的正确性收益（异步 Rust 是最容易
//!    "编译过了但语义微妙错"的地方）；
//! 3. 取消语义对得上：`Arc<AtomicBool>` ↔ TS 的 `AbortSignal`，一一对应好对账；
//! 4. Tauri 的 sync command 本来就跑在线程池上，不冲突。
//!
//! ── 超时分层 ────────────────────────────────────────────────────────
//! 单次请求用 `request_timeout`（默认 60s）；任务总超时由轮询状态机用
//! `task_timeout`（默认 90min）控制 —— 两者不再混用同一个值。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use serde_json::{Map, Value};
use sha2::Sha256;

use super::config::{
    DEFAULT_BASE_URL, PATH_PREFIX, PROJECT_TYPE, REQUEST_TIMEOUT_MS,
};
use super::contract::{as_string, normalize_send_body, SendInput};
use super::errors::{ErrCode, ErrType, LovartError};

// ── 传输层（依赖注入点）────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }
}

/// 请求体。**刻意做窄**（不是通用 HTTP body）：本域只会发 JSON 与单文件 multipart。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestBody {
    None,
    Json(String),
    Multipart { filename: String, bytes: Vec<u8> },
}

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: RequestBody,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// 出站传输的窄接口（依赖注入点）。
///
/// 生产实参 = [`ReqwestTransport`]；测试注入假实现即可**不打真上游**地断言请求。
/// 对应 TS 的 `LovartTransport`。
pub trait Transport: Send + Sync {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, String>;
}

/// 基于 `reqwest`（blocking）的生产实现。
pub struct ReqwestTransport {
    client: reqwest::blocking::Client,
}

impl ReqwestTransport {
    pub fn new() -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self { client })
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new().expect("构建 HTTP 客户端")
    }
}

impl Transport for ReqwestTransport {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, String> {
        let method = match request.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
        };
        let mut builder = self.client.request(method, &request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(timeout) = request.timeout {
            builder = builder.timeout(timeout);
        }
        builder = match &request.body {
            RequestBody::None => builder,
            RequestBody::Json(text) => builder.body(text.clone()),
            RequestBody::Multipart { filename, bytes } => {
                let part = reqwest::blocking::multipart::Part::bytes(bytes.clone())
                    .file_name(filename.clone());
                builder.multipart(reqwest::blocking::multipart::Form::new().part("file", part))
            }
        };
        let response = builder.send().map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let body = response.text().map_err(|error| error.to_string())?;
        Ok(HttpResponse { status, body })
    }
}

// ── 依赖 ──────────────────────────────────────────────────────────────

/// 客户端依赖（对应 TS `LovartClientDeps`）。
#[derive(Clone)]
pub struct ClientDeps {
    pub base_url: String,
    pub access_key: String,
    pub secret_key: String,
    pub transport: Arc<dyn Transport>,
    /// 外部取消信号（Ctrl+C 等）。等价 TS 的 `AbortSignal`。
    pub cancelled: Arc<AtomicBool>,
    /// 单次 HTTP 请求超时。
    pub request_timeout: Duration,
    /// 任务总超时（供轮询状态机用）。
    pub task_timeout: Duration,
    /// 轮询起始间隔。
    pub poll_interval: Duration,
    /// 轮询退避封顶。**必须 ≥ `poll_interval`**（`poll_thread` 里还有一层防御）。
    pub poll_max_interval: Duration,
    /// done 后复核等待。
    pub done_recheck: Duration,
}

impl ClientDeps {
    pub fn new(base_url: impl Into<String>, access_key: impl Into<String>, secret_key: impl Into<String>, transport: Arc<dyn Transport>) -> Self {
        Self {
            base_url: base_url.into(),
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            transport,
            cancelled: Arc::new(AtomicBool::new(false)),
            request_timeout: Duration::from_millis(REQUEST_TIMEOUT_MS),
            // ⚠️ 这两个是**内置默认**，真实值由 `to_client_deps` 从设置填入。
            // 别在这里读环境：「纯构造」是测试确定性的前提。
            task_timeout: Duration::from_millis(super::config::TASK_TIMEOUT_MS),
            poll_interval: Duration::from_millis(super::config::POLL_INTERVAL_MS),
            poll_max_interval: Duration::from_millis(super::config::POLL_MAX_INTERVAL_MS),
            done_recheck: Duration::from_millis(super::config::DONE_RECHECK_MS),
        }
    }

    pub fn default_base_url() -> &'static str {
        DEFAULT_BASE_URL
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

// ── HMAC 鉴权 ─────────────────────────────────────────────────────────

/// HMAC-SHA256 十六进制签名。
///
/// 公开是为了能被**外部真值**校验（RFC 4231 测试向量）—— 只测"自洽"是不够的，
/// 签名算法算错会静默地让**所有**请求 401。
pub fn hmac_sha256_hex(secret: &str, payload: &str) -> String {
    // `new_from_slice` 对 HMAC 接受任意长度密钥，不会失败。
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC 接受任意长度密钥");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// HMAC-SHA256 鉴权头（Lovart 原生 Agent 协议）。
///
/// 签名串 = `"{METHOD}\n{path}\n{timestamp}"`，**每次请求重算时间戳与签名**。
fn build_hmac_auth_headers(deps: &ClientDeps, method: &str, path: &str) -> Vec<(String, String)> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
        .to_string();
    let signature = hmac_sha256_hex(&deps.secret_key, &format!("{method}\n{path}\n{timestamp}"));
    vec![
        ("X-Access-Key".to_string(), deps.access_key.clone()),
        ("X-Timestamp".to_string(), timestamp),
        ("X-Signature".to_string(), signature),
        ("X-Signed-Method".to_string(), method.to_string()),
        ("X-Signed-Path".to_string(), path.to_string()),
        ("User-Agent".to_string(), "LovartAgentSkill/1.0".to_string()),
    ]
}

// ── 信封与错误 ────────────────────────────────────────────────────────

/// 剥 `{code,data}` 信封并做 `code != 0` 检（上游文案原样透传）。
fn parse_envelope(json: Value) -> Result<Value, LovartError> {
    let Some(record) = json.as_object() else {
        return Ok(json);
    };
    if let Some(code) = record.get("code") {
        // ⚠️ 与 TS 一致：**严格**比较 0。字符串 `"0"` 在 TS 里 `"0" !== 0` 成立 ⇒ 也是错误。
        //    这里同理，只有数字 0 才算成功。
        if code.as_i64() != Some(0) {
            let message = record
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Lovart request failed")
                .to_string();
            let code = match code.as_i64() {
                Some(number) => ErrCode::Number(number),
                None => ErrCode::Text(code.to_string()),
            };
            return Err(LovartError::domain(message, code, ErrType::Upstream));
        }
    }
    match record.get("data") {
        Some(data) => Ok(data.clone()),
        None => Ok(json),
    }
}

/// 非 2xx → 构造**带业务码**的错误。
///
/// 上游在非 2xx 时仍会返回 `{code, error}` 信封（例：
/// `{"code":2012,"error":"Concurrent task limit reached..."}`）。
/// 原实现把整个响应体塞进 message 就丢了语义 ⇒ 上层只能按 HTTP 码判重试性
/// ⇒ **「并发满」被判死**（TD-01-06），且用户看到的是一坨 JSON（TD-01-07）。
///
/// 这里：业务码解到 `biz_code`；文案取 `error` / `message`（可读）；原始体留作兜底。
fn build_upstream_error(text: &str, http_status: u16, fallback: &str) -> LovartError {
    let mut biz_code: Option<i64> = None;
    let mut readable = String::new();

    if let Ok(Value::Object(body)) = serde_json::from_str::<Value>(text) {
        match body.get("code") {
            Some(code) => {
                if let Some(number) = code.as_i64() {
                    biz_code = Some(number);
                } else if let Some(text) = code.as_str() {
                    // 上游偶尔把业务码写成字符串（`"2012"`）。
                    if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) {
                        biz_code = text.parse().ok();
                    }
                }
            }
            None => {}
        }
        readable = as_string(body.get("error").unwrap_or(&Value::Null)).to_string();
        if readable.is_empty() {
            readable = as_string(body.get("message").unwrap_or(&Value::Null)).to_string();
        }
    }

    let message = if !readable.is_empty() {
        readable
    } else {
        let truncated: String = text.chars().take(300).collect();
        if truncated.is_empty() {
            format!("{fallback} ({http_status})")
        } else {
            truncated
        }
    };
    LovartError::upstream(message, http_status).with_biz_code(biz_code)
}

// ── 请求 ──────────────────────────────────────────────────────────────

fn build_url(deps: &ClientDeps, path: &str, query: &[(&str, String)]) -> Result<String, LovartError> {
    let base = deps.base_url.trim_end_matches('/');
    let mut url = reqwest::Url::parse(&format!("{base}{path}"))
        .map_err(|error| LovartError::transport(format!("URL 非法：{error}")))?;
    if !query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in query {
            pairs.append_pair(key, value);
        }
        drop(pairs);
    }
    Ok(url.to_string())
}

/// 经注入 transport 发请求并剥信封。
///
/// 非 2xx 抛 `LovartError::Upstream`（`code` = HTTP 状态码）；
/// 传输失败抛 `LovartError::Transport`（**一律瞬态**）。
pub fn lovart_request(
    deps: &ClientDeps,
    method: Method,
    subpath: &str,
    body: Option<&Value>,
    query: &[(&str, String)],
) -> Result<Value, LovartError> {
    let path = format!("{PATH_PREFIX}{subpath}");
    let url = build_url(deps, &path, query)?;

    let mut headers = build_hmac_auth_headers(deps, method.as_str(), &path);
    let request_body = match body {
        None => RequestBody::None,
        Some(value) => {
            headers.push(("Content-Type".to_string(), "application/json".to_string()));
            RequestBody::Json(value.to_string())
        }
    };

    let request = HttpRequest {
        method,
        url: url.clone(),
        headers,
        body: request_body,
        timeout: Some(deps.request_timeout),
    };

    let response = deps
        .transport
        .send(&request)
        .map_err(LovartError::transport)?;

    if !(200..=299).contains(&response.status) {
        let label = format!("{} {}", method.as_str(), path);
        return Err(build_upstream_error(&response.body, response.status, &label));
    }

    // ⚠️ 2xx 但响应体不是合法 JSON ⇒ 这是**确定性失败**（不是瞬态），
    //    重试没有意义：上游给了 200 却给不出信封，多半是打到了错误的端点。
    let json: Value = serde_json::from_str(&response.body).map_err(|error| {
        LovartError::internal(
            format!("上游返回 2xx 但响应体不是合法 JSON：{error}"),
            ErrType::Upstream,
        )
    })?;
    parse_envelope(json)
}

// ── 端点 ──────────────────────────────────────────────────────────────

pub fn create_project(deps: &ClientDeps) -> Result<String, LovartError> {
    let mut body = Map::new();
    body.insert("project_id".to_string(), Value::String(String::new()));
    body.insert("canvas".to_string(), Value::String(String::new()));
    body.insert("project_cover_list".to_string(), Value::Array(Vec::new()));
    body.insert("pic_count".to_string(), Value::Number(0.into()));
    body.insert(
        "project_type".to_string(),
        Value::Number(PROJECT_TYPE.into()),
    );
    let data = lovart_request(
        deps,
        Method::Post,
        "/project/save",
        Some(&Value::Object(body)),
        &[],
    )?;
    Ok(data
        .as_object()
        .and_then(|object| object.get("project_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string())
}

/// 切换生成模式。**每次提交前调用**（服务端是持久设置，别指望它自己保持）。
///
/// `unlimited = true` ⇒ 排队模式（免费，可能要等很久）；`false` ⇒ fast（扣积分）。
pub fn set_mode(deps: &ClientDeps, unlimited: bool) -> Result<(), LovartError> {
    let mut body = Map::new();
    body.insert("unlimited".to_string(), Value::Bool(unlimited));
    lovart_request(deps, Method::Post, "/mode/set", Some(&Value::Object(body)), &[])?;
    Ok(())
}

/// 查询当前模式（用于校验是否被外部改回 fast）。
pub fn query_mode(deps: &ClientDeps) -> Result<Value, LovartError> {
    lovart_request(
        deps,
        Method::Post,
        "/mode/query",
        Some(&Value::Object(Map::new())),
        &[],
    )
}

pub fn send_chat(deps: &ClientDeps, input: &SendInput) -> Result<String, LovartError> {
    let body = normalize_send_body(input); // 字段别名 + 非空才附
    let data = lovart_request(deps, Method::Post, "/chat", Some(&body), &[])?;
    Ok(data
        .as_object()
        .and_then(|object| object.get("thread_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string())
}

pub fn get_status(deps: &ClientDeps, thread_id: &str) -> Result<Value, LovartError> {
    lovart_request(
        deps,
        Method::Get,
        "/chat/status",
        None,
        &[("thread_id", thread_id.to_string())],
    )
}

pub fn get_result(deps: &ClientDeps, thread_id: &str) -> Result<Value, LovartError> {
    lovart_request(
        deps,
        Method::Get,
        "/chat/result",
        None,
        &[("thread_id", thread_id.to_string())],
    )
}

pub fn confirm_thread(deps: &ClientDeps, thread_id: &str) -> Result<(), LovartError> {
    let mut body = Map::new();
    body.insert(
        "thread_id".to_string(),
        Value::String(thread_id.to_string()),
    );
    lovart_request(deps, Method::Post, "/chat/confirm", Some(&Value::Object(body)), &[])?;
    Ok(())
}

/// 把字节上传到 Lovart CDN，返回 CDN URL。失败抛 `UploadFailed`。
pub fn upload_file(
    deps: &ClientDeps,
    bytes: &[u8],
    filename: &str,
) -> Result<String, LovartError> {
    let path = format!("{PATH_PREFIX}/file/upload");
    let url = build_url(deps, &path, &[])?;
    let headers = build_hmac_auth_headers(deps, Method::Post.as_str(), &path);
    let request = HttpRequest {
        method: Method::Post,
        url,
        headers,
        body: RequestBody::Multipart {
            filename: filename.to_string(),
            bytes: bytes.to_vec(),
        },
        timeout: Some(deps.request_timeout),
    };

    let response = deps
        .transport
        .send(&request)
        .map_err(LovartError::transport)?;
    if !(200..=299).contains(&response.status) {
        return Err(build_upstream_error(
            &response.body,
            response.status,
            "POST /file/upload",
        ));
    }
    let json: Value = serde_json::from_str(&response.body).map_err(|error| {
        LovartError::internal(
            format!("上传返回 2xx 但响应体不是合法 JSON：{error}"),
            ErrType::UploadFailed,
        )
    })?;
    let data = parse_envelope(json)?;
    let url = data
        .as_object()
        .and_then(|object| object.get("url"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if url.is_empty() {
        return Err(LovartError::internal("上传未返回 CDN URL", ErrType::UploadFailed));
    }
    Ok(url.to_string())
}
