//! 本机 HTTP 接口 —— 给「另一个前端」用的稳定入口。
//!
//! ── 只有四个端点，且路由是**穷尽 `match`** ──────────────────────────────
//! `POST /tasks` · `POST /drafts` · `GET /state` · `POST /stop`。没有 `settings` / `save_settings`
//! 这类分支 —— 不是"文档里说别调"，是代码里根本没有可执行路径（`ADR-0009` 级 1）。
//! `/drafts` 与 `/tasks` 的差别是语义：前者只递送、等人确认；后者直接开跑。
//!
//! ── 串行处理（不 spawn）──────────────────────────────────────────────
//! 监听循环里一次只处理一个请求：`/state` 是 200ms 级全量，若并发就变成
//! N × 200ms + N × 4MB。串行之后**没有锁、没有锁中毒、没有 DoS 面**。
//!
//! ── 依赖方向 ─────────────────────────────────────────────────────────
//! 本文件在 `src-tauri`（适配层），只调 `service`；反过来不行：
//! `lovart-core` 的 `Cargo.toml` 没有 tauri，也没有 tiny_http。

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response};

use lovart_core::engine::intake::TaskInput;
use lovart_core::service::Workbench;

/// 本机端口。写死：外部前端只需要记住一个数。
const PORT: u16 = 17863;
/// 请求体上限 8MB（超了 413，不截断、不猜测）。
const MAX_BODY: usize = 8 * 1024 * 1024;

/// 起服务（失败只告警，不阻断 GUI）。
pub fn spawn(core: Arc<Workbench>) {
    std::thread::spawn(move || {
        let token = match load_or_create_token(&core.var_dir()) {
            Ok(token) => token,
            // ⚠️ fail closed：读失败**不**重新生成（那会静默让外部前端手里的 token 失效）。
            Err(message) => {
                core.push_alert(&format!("HTTP 未启动：{message}"));
                return;
            }
        };
        let server = match tiny_http::Server::http(("127.0.0.1", PORT)) {
            Ok(server) => server,
            // 端口冲突：告警并继续（GUI 本身还能用）。
            Err(error) => {
                core.push_alert(&format!(
                    "HTTP 端口 {PORT} 绑定失败，外部前端用不了：{error}"
                ));
                return;
            }
        };
        core.push_alert(&format!(
            "HTTP 已启动：http://127.0.0.1:{PORT}（token 见 var/http-token）"
        ));
        // 串行：一次一个请求（见文件头）。
        for request in server.incoming_requests() {
            handle(&core, &token, request);
        }
    });
}

pub(crate) fn handle(core: &Arc<Workbench>, token: &str, request: Request) {
    let method = request.method().clone();
    let path = request
        .url()
        .split('?')
        .next()
        .unwrap_or_default()
        .to_string();

    if !authorized(&request, token) {
        respond(request, 401, json!({ "error": "token 无效（看 var/http-token）" }));
        return;
    }

    match (method, path.as_str()) {
        (Method::Post, "/tasks") => handle_tasks(core, request),
        (Method::Post, "/drafts") => handle_drafts(core, request),
        (Method::Get, "/state") => handle_state(core, request),
        // 穷尽路由：其余全部 404（没有 settings 分支可写）。
        _ => respond(request, 404, json!({ "error": "没有这个端点" })),
    }
}

/// 解析 `{ "tasks": [ … ] }` —— `/tasks` 与 `/drafts` 共用同一份契约。
fn parse_tasks_body(request: &mut Request) -> Result<Vec<TaskInput>, (u16, String)> {
    let body = read_body(request)?;
    let parsed: Result<TasksBody, _> = serde_json::from_slice(&body);
    parsed
        .map(|TasksBody { tasks }| tasks)
        .map_err(|error| (400, format!("body 不是合法 JSON：{error}")))
}

fn handle_tasks(core: &Arc<Workbench>, mut request: Request) {
    let tasks = match parse_tasks_body(&mut request) {
        Ok(tasks) => tasks,
        Err((status, message)) => {
            respond(request, status, json!({ "error": message }));
            return;
        }
    };
    match core.publish(tasks) {
        Ok(accepted) => respond(
            request,
            200,
            json!({
                "accepted": accepted,
                "receivedAt": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            }),
        ),
        // `publish` 的错误都是"输入不合法 / 待发为空"，400。
        Err(message) => respond(request, 400, json!({ "error": message })),
    }
}

/// 只递送、不开跑：草稿落在编辑窗的「待发」里，等人点发布。
///
/// 成功 **204**（空 body）：受理即结束，不给调用方任何要解析的东西。
fn handle_drafts(core: &Arc<Workbench>, mut request: Request) {
    let tasks = match parse_tasks_body(&mut request) {
        Ok(tasks) => tasks,
        Err((status, message)) => {
            respond(request, status, json!({ "error": message }));
            return;
        }
    };
    core.draft(tasks);
    respond_empty(request, 204);
}

fn handle_state(core: &Arc<Workbench>, request: Request) {
    match core.snapshot(None) {
        Ok(snapshot) => match snapshot.payload {
            Some(payload) => respond(request, 200, payload),
            // `snapshot(None)` 必然全量；走到这里说明实现被改坏了。
            None => respond(request, 500, json!({ "error": "看板没返回数据（内部 bug）" })),
        },
        Err(message) => respond(request, 500, json!({ "error": message })),
    }
}

#[derive(serde::Deserialize)]
struct TasksBody {
    tasks: Vec<TaskInput>,
}

fn authorized(request: &Request, token: &str) -> bool {
    let expected = format!("Bearer {token}");
    request.headers().iter().any(|header| {
        header.field.equiv("Authorization") && header.value.as_str() == expected
    })
}

fn read_body(request: &mut Request) -> Result<Vec<u8>, (u16, String)> {
    if request.body_length().is_some_and(|length| length > MAX_BODY) {
        return Err((413, "body 超过 8MB".to_string()));
    }
    let mut body = Vec::new();
    request
        .as_reader()
        .take(MAX_BODY as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|error| (400, format!("读不了 body：{error}")))?;
    if body.len() > MAX_BODY {
        return Err((413, "body 超过 8MB".to_string()));
    }
    Ok(body)
}

fn respond(request: Request, status: u16, body: Value) {
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..])
        .expect("静态 header");
    let _ = request.respond(
        Response::from_string(body.to_string())
            .with_status_code(status)
            .with_header(header),
    );
}

/// 空 body 的成功回执（`/drafts` 的 204）。
fn respond_empty(request: Request, status: u16) {
    let _ = request.respond(Response::empty(status));
}

/// 读 `var/http-token`；不存在才生成。删文件 = 轮换。
fn load_or_create_token(dir: &Path) -> Result<String, String> {
    let path = dir.join("http-token");
    if path.exists() {
        let token = std::fs::read_to_string(&path)
            .map_err(|error| format!("读不了 {}：{error}", path.display()))?;
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err(format!("{} 是空的（删掉它才重新生成）", path.display()));
        }
        return Ok(token);
    }
    std::fs::create_dir_all(dir).map_err(|error| format!("建不了 var/：{error}"))?;
    let token = generate_token();
    std::fs::write(&path, &token).map_err(|error| format!("写不了 {}：{error}", path.display()))?;
    Ok(token)
}

fn generate_token() -> String {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default(),
    );
    let mut second = RandomState::new().build_hasher();
    second.write_u64(std::process::id() as u64);
    format!("{:016x}{:016x}", hasher.finish(), second.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lovart-http-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    /// 起一个随机端口的服务器，用**同一个 `handle`** 跑请求（不复制路由逻辑）。
    fn start(core: Arc<Workbench>, token: &str) -> std::net::SocketAddr {
        let server = tiny_http::Server::http(("127.0.0.1", 0)).expect("起测试服务器");
        let addr = server.server_addr().to_ip().expect("拿到端口");
        let core = Arc::clone(&core);
        let token = token.to_string();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                handle(&core, &token, request);
            }
        });
        addr
    }

    /// 手写一个 HTTP/1.1 请求，只返回状态码。
    fn request(
        addr: std::net::SocketAddr,
        method: &str,
        path: &str,
        auth: Option<&str>,
        body: &str,
    ) -> u16 {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(addr).expect("连服务器");
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        if let Some(token) = auth {
            head.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        head.push_str("\r\n");
        head.push_str(body);
        stream.write_all(head.as_bytes()).expect("发请求");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("读响应");
        response
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("响应行里有状态码")
    }

    #[test]
    fn drafts_端点三态_且受理后进草稿() {
        let dir = temp_dir("drafts-http");
        let core = Workbench::new(dir.clone());
        let token = load_or_create_token(&dir).expect("token");
        let addr = start(Arc::clone(&core), &token);

        // 合法 ⇒ 204，且草稿真的进去了
        assert_eq!(
            request(addr, "POST", "/drafts", Some(&token), r#"{"tasks":[{"prompt":"p"}]}"#),
            204
        );
        assert_eq!(core.drafts().len(), 1, "204 之后草稿必须真的在集合里");

        // 无 token ⇒ 401
        assert_eq!(request(addr, "POST", "/drafts", None, r#"{"tasks":[]}"#), 401);

        // body 不合法 ⇒ 400
        assert_eq!(request(addr, "POST", "/drafts", Some(&token), "not json"), 400);
    }

    #[test]
    fn token_不存在才生成_第二次必须复用() {
        let dir = temp_dir("reuse");
        let first = load_or_create_token(&dir).expect("首次生成");
        assert!(!first.is_empty());
        let second = load_or_create_token(&dir).expect("复用");
        assert_eq!(first, second, "已存在的 token 不许被覆盖（否则外部前端每次都要改配置）");
    }

    #[test]
    fn token_文件为空时必须报错而不是重新生成() {
        let dir = temp_dir("empty");
        std::fs::write(dir.join("http-token"), "   ").expect("写空 token");
        assert!(
            load_or_create_token(&dir).is_err(),
            "空 token 必须 fail closed，不能静默换新（那会让外部前端手里的 token 失效）"
        );
    }
}
