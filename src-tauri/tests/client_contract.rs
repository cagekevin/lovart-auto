//! 协议层契约回归 —— 对齐 TS `lovart/lovart_client.ts` 的行为。
//!
//! **全部在传输层打桩**：注入假 transport，不打真上游。这样能断言到
//! 「发出去的请求长什么样」（路径 / 方法 / 鉴权头 / 请求体）——
//! 而这是最容易悄悄写错、又最难在真跑里发现的部分（错了只会得到 401/404）。
//!
//! ⚠️ 有一条是**外部真值**校验（RFC 4231 的 HMAC-SHA256 向量）：
//! 只测"自洽"不够，签名算错会静默地让**所有**请求 401。

#![allow(non_snake_case)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use lovart_client::lovart::client::{
    confirm_thread, create_project, get_result, get_status, hmac_sha256_hex, lovart_request,
    query_mode, send_chat, set_mode, upload_file, ClientDeps, HttpRequest, HttpResponse, Method,
    RequestBody, Transport,
};
use lovart_client::lovart::contract::{is_truthy, normalize_send_body, SendInput};
use lovart_client::lovart::errors::{
    is_busy_error, is_retryable_error, is_retryable_http_status, ErrType, LovartError,
};
use lovart_client::engine::scheduler::AutoConfirm;
use lovart_client::lovart::task::{extract_artifacts, poll_thread, AutoConfirmDecision};

// ── 假传输 ────────────────────────────────────────────────────────────

#[derive(Default)]
struct Fake {
    seen: Mutex<Vec<HttpRequest>>,
    replies: Mutex<VecDeque<Result<HttpResponse, String>>>,
}

impl Fake {
    fn new(replies: Vec<Result<HttpResponse, String>>) -> Arc<Self> {
        Arc::new(Fake {
            seen: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into()),
        })
    }

    fn seen(&self) -> Vec<HttpRequest> {
        self.seen.lock().unwrap().clone()
    }

    fn last(&self) -> HttpRequest {
        self.seen().pop().expect("应至少发出一次请求")
    }
}

impl Transport for Fake {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, String> {
        self.seen.lock().unwrap().push(request.clone());
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(ok(json!({ "code": 0, "data": {} }))))
    }
}

fn ok(body: Value) -> HttpResponse {
    HttpResponse {
        status: 200,
        body: body.to_string(),
    }
}

fn http(status: u16, body: &str) -> HttpResponse {
    HttpResponse {
        status,
        body: body.to_string(),
    }
}

fn deps_with(replies: Vec<Result<HttpResponse, String>>) -> (ClientDeps, Arc<Fake>) {
    let fake = Fake::new(replies);
    let transport: Arc<dyn Transport> = fake.clone();
    (
        ClientDeps::new("http://replay.invalid", "replay-ak", "replay-sk", transport),
        fake,
    )
}

fn deps_ok() -> (ClientDeps, Arc<Fake>) {
    deps_with(vec![Ok(ok(json!({ "code": 0, "data": {} })))])
}

fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

// ── HMAC（外部真值）────────────────────────────────────────────────────

#[test]
fn hmac_sha256_对齐_rfc4231_测试向量() {
    // RFC 4231 §4.2 Test Case 1：key = 0x0b × 20，data = "Hi There"
    let key = "\u{0b}".repeat(20);
    assert_eq!(
        hmac_sha256_hex(&key, "Hi There"),
        "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
        "HMAC 实现与标准不符 ⇒ 所有请求都会 401（而且看不出为什么）"
    );
}

#[test]
fn 鉴权头齐全且签名串是_方法_换行_路径_换行_时间戳() {
    let (deps, fake) = deps_ok();
    get_status(&deps, "t-1").unwrap();
    let request = fake.last();

    let timestamp = header(&request, "X-Timestamp").expect("X-Timestamp");
    let signature = header(&request, "X-Signature").expect("X-Signature");
    assert_eq!(header(&request, "X-Access-Key"), Some("replay-ak"));
    assert_eq!(header(&request, "X-Signed-Method"), Some("GET"));
    assert_eq!(header(&request, "X-Signed-Path"), Some("/v1/openapi/chat/status"));
    assert_eq!(header(&request, "User-Agent"), Some("LovartAgentSkill/1.0"));

    // 用**同一个**时间戳重算签名 —— 断言签名串的构造方式，而不是"有个签名"
    let expected = hmac_sha256_hex(
        "replay-sk",
        &format!("GET\n/v1/openapi/chat/status\n{timestamp}"),
    );
    assert_eq!(signature, expected, "签名串的构造方式与上游约定不符");
}

// ── 信封 ──────────────────────────────────────────────────────────────

#[test]
fn 信封_code_为_0_时剥出_data() {
    let (deps, _) = deps_with(vec![Ok(ok(json!({ "code": 0, "data": { "thread_id": "t-9" } })))]);
    assert_eq!(send_chat(&deps, &SendInput::default()).unwrap(), "t-9");
}

#[test]
fn 信封_code_非_0_时抛错且保留上游文案() {
    let (deps, _) = deps_with(vec![Ok(ok(
        json!({ "code": 1429, "message": "rate limited" }),
    ))]);
    let error = send_chat(&deps, &SendInput::default()).unwrap_err();
    assert_eq!(error.message(), "rate limited", "上游文案必须原样透传，不翻译");
    assert_eq!(error.code().unwrap().as_number(), Some(1429));
    assert_eq!(error.kind(), ErrType::Upstream);
}

#[test]
fn 信封里没有_code_时原样返回() {
    let (deps, _) = deps_with(vec![Ok(ok(json!({ "status": "done" })))]);
    let value = get_status(&deps, "t-1").unwrap();
    assert_eq!(value.get("status").and_then(Value::as_str), Some("done"));
}

#[test]
fn 信封_code_是字符串_0_也算失败() {
    // ⚠️ 与 TS 一致：`"0" !== 0` 严格比较 ⇒ 字符串 "0" 是**错误**。
    //    若这里放行，上游把码写成字符串时会静默当成成功。
    let (deps, _) = deps_with(vec![Ok(ok(json!({ "code": "0", "message": "x" })))]);
    assert!(get_status(&deps, "t-1").is_err());
}

// ── 非 2xx ────────────────────────────────────────────────────────────

#[test]
fn 非_2xx_的_code_是_http_状态码() {
    let (deps, _) = deps_with(vec![Ok(http(404, "not found"))]);
    let error = get_status(&deps, "t-1").unwrap_err();
    assert_eq!(error.code().unwrap().as_number(), Some(404));
    assert!(!is_retryable_error(&error), "404 是确定性硬失败，重试没意义");
}

#[test]
fn 非_2xx_时解出业务码与可读文案_而不是一坨_json() {
    // 这是 TD-01-06 / TD-01-07 的回归：上游 402 + {code:2012, error:"..."}
    let (deps, _) = deps_with(vec![Ok(http(
        402,
        r#"{"code":2012,"error":"Concurrent task limit reached, please retry later"}"#,
    ))]);
    let error = get_status(&deps, "t-1").unwrap_err();

    assert_eq!(error.code().unwrap().as_number(), Some(402), "HTTP 码");
    assert_eq!(error.biz_code(), Some(2012), "业务码必须解出来");
    assert_eq!(
        error.message(),
        "Concurrent task limit reached, please retry later",
        "文案要取 error 字段，不是把整个 JSON 塞进 message"
    );
    assert!(
        is_busy_error(&error),
        "并发满必须被判成「排队」，否则会被 maxAttempts 判死"
    );
}

#[test]
fn 非_json_响应体原样当文案() {
    let (deps, _) = deps_with(vec![Ok(http(500, "<html>gateway error</html>"))]);
    let error = get_status(&deps, "t-1").unwrap_err();
    assert_eq!(error.message(), "<html>gateway error</html>");
    assert!(is_retryable_error(&error), "5xx 是瞬态");
}

#[test]
fn 响应_2xx_但响应体不是_json_是确定性失败() {
    // 上游给 200 却给不出信封，多半是打到了错误的端点 —— 重试没有意义。
    let (deps, _) = deps_with(vec![Ok(http(200, "<html>login page</html>"))]);
    let error = get_status(&deps, "t-1").unwrap_err();
    assert!(
        !is_retryable_error(&error),
        "2xx 非 JSON 若算瞬态，会一路重试到 maxAttempts 才放弃"
    );
}

#[test]
fn 传输失败是瞬态() {
    let (deps, _) = deps_with(vec![Err("connection reset by peer".to_string())]);
    let error = get_status(&deps, "t-1").unwrap_err();
    assert!(matches!(error, LovartError::Transport { .. }));
    assert!(is_retryable_error(&error), "网络抖动必须可重试");
    assert!(!is_busy_error(&error), "网络错不是「服务端忙」");
}

// ── 重试性 / 忙码判据 ─────────────────────────────────────────────────

#[test]
fn 可重试的_http_码被钉死() {
    for good in [0, 402, 408, 429, 500, 503, 599] {
        assert!(is_retryable_http_status(good), "{good} 应可重试");
    }
    for bad in [400, 401, 403, 404, 409, 422, 600] {
        assert!(!is_retryable_http_status(bad), "{bad} 不该可重试");
    }
}

#[test]
fn 忙码表被钉死_http_码与业务码两边都查() {
    // 409 / 429 是 HTTP 码；2011 / 2012 / 1429 是信封业务码；402 两边都出现。
    let cases: Vec<(Option<i64>, Option<i64>)> = vec![
        (Some(402), None),
        (Some(409), None),
        (Some(429), None),
        (Some(200), Some(2011)),
        (Some(200), Some(2012)),
        (Some(200), Some(1429)),
    ];
    for (http_code, biz_code) in cases {
        let error = LovartError::upstream("busy", http_code.unwrap() as u16)
            .with_biz_code(biz_code);
        assert!(
            is_busy_error(&error),
            "http={http_code:?} biz={biz_code:?} 应判成「排队」"
        );
    }
    // 反例：普通 500 不是忙
    assert!(!is_busy_error(&LovartError::upstream("boom", 500)));
}

// ── 请求形状 ──────────────────────────────────────────────────────────

#[test]
fn send_chat_的请求体做字段别名且空附件不附() {
    let (deps, fake) = deps_with(vec![Ok(ok(json!({ "code": 0, "data": { "thread_id": "t" } })))]);
    send_chat(
        &deps,
        &SendInput {
            prompt: "一只猫".to_string(),
            project_id: "p-1".to_string(),
            attachments: Some(Vec::new()),
            tool_config: None,
        },
    )
    .unwrap();

    let request = fake.last();
    assert_eq!(request.method, Method::Post);
    assert!(request.url.ends_with("/v1/openapi/chat"));
    let RequestBody::Json(text) = &request.body else {
        panic!("应为 JSON 体");
    };
    let body: Value = serde_json::from_str(text).unwrap();
    assert_eq!(body.get("project_id").and_then(Value::as_str), Some("p-1"));
    assert_eq!(body.get("prompt").and_then(Value::as_str), Some("一只猫"));
    assert!(
        body.get("attachments").is_none(),
        "空数组与「不传」是两种语义，不能附"
    );
    assert_eq!(
        header(&request, "Content-Type"),
        Some("application/json")
    );
}

#[test]
fn normalize_send_body_只在非空时附_attachments_与_tool_config() {
    let bare = normalize_send_body(&SendInput {
        prompt: "p".to_string(),
        project_id: "proj".to_string(),
        attachments: None,
        tool_config: None,
    });
    assert_eq!(bare, json!({ "prompt": "p", "project_id": "proj" }));

    let full = normalize_send_body(&SendInput {
        prompt: "p".to_string(),
        project_id: "proj".to_string(),
        attachments: Some(vec!["https://cdn/a.png".to_string()]),
        tool_config: Some(json!({ "prefer_tool_categories": { "IMAGE": ["tool"] } })),
    });
    assert_eq!(full.get("attachments").unwrap(), &json!(["https://cdn/a.png"]));
    assert!(full.get("tool_config").is_some());
}

#[test]
fn set_mode_发_unlimited() {
    let (deps, fake) = deps_ok();
    set_mode(&deps, true).unwrap();
    let RequestBody::Json(text) = &fake.last().body else {
        panic!("应为 JSON 体");
    };
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), json!({ "unlimited": true }));
}

#[test]
fn query_参数进_url_而不是请求体() {
    let (deps, fake) = deps_ok();
    get_status(&deps, "t-1").unwrap();
    let request = fake.last();
    assert_eq!(request.method, Method::Get);
    assert!(request.url.contains("thread_id=t-1"), "{}", request.url);
    assert_eq!(request.body, RequestBody::None, "GET 不该带请求体");

    let (deps, fake) = deps_ok();
    get_result(&deps, "t-2").unwrap();
    assert!(fake.last().url.contains("/chat/result?thread_id=t-2"));
}

#[test]
fn create_project_取_project_id_且带_project_type() {
    let (deps, fake) = deps_with(vec![Ok(ok(
        json!({ "code": 0, "data": { "project_id": "p-42" } }),
    ))]);
    assert_eq!(create_project(&deps).unwrap(), "p-42");

    let RequestBody::Json(text) = &fake.last().body else {
        panic!("应为 JSON 体");
    };
    let body: Value = serde_json::from_str(text).unwrap();
    assert_eq!(body.get("project_type"), Some(&json!(3)));
    assert_eq!(body.get("project_id").and_then(Value::as_str), Some(""));
}

#[test]
fn confirm_thread_打_chat_confirm() {
    let (deps, fake) = deps_ok();
    confirm_thread(&deps, "t-7").unwrap();
    let request = fake.last();
    assert!(request.url.ends_with("/v1/openapi/chat/confirm"));
    let RequestBody::Json(text) = &request.body else {
        panic!("应为 JSON 体");
    };
    assert_eq!(
        serde_json::from_str::<Value>(text).unwrap().get("thread_id"),
        Some(&json!("t-7"))
    );
}

#[test]
fn query_mode_是_post_带空对象() {
    let (deps, fake) = deps_ok();
    query_mode(&deps).unwrap();
    let request = fake.last();
    assert_eq!(request.method, Method::Post);
    assert!(request.url.ends_with("/v1/openapi/mode/query"));
    let RequestBody::Json(text) = &request.body else {
        panic!("应为 JSON 体");
    };
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), json!({}));
}

// ── 上传 ──────────────────────────────────────────────────────────────

#[test]
fn upload_file_走_multipart_并返回_cdn_url() {
    let (deps, fake) = deps_with(vec![Ok(ok(
        json!({ "code": 0, "data": { "url": "https://cdn.invalid/a.png" } }),
    ))]);
    let url = upload_file(&deps, b"PNGDATA", "a.png").unwrap();
    assert_eq!(url, "https://cdn.invalid/a.png");

    let request = fake.last();
    match &request.body {
        RequestBody::Multipart { filename, bytes } => {
            assert_eq!(filename, "a.png");
            assert_eq!(bytes, b"PNGDATA");
        }
        other => panic!("应为 multipart，实际 {other:?}"),
    }
    assert!(
        header(&request, "Content-Type").is_none(),
        "multipart 的 Content-Type 必须由传输层自己带 boundary，不能手工设成 application/json"
    );
}

#[test]
fn upload_file_未返回_url_抛_upload_failed() {
    let (deps, _) = deps_with(vec![Ok(ok(json!({ "code": 0, "data": {} })))]);
    let error = upload_file(&deps, b"x", "a.png").unwrap_err();
    assert_eq!(error.kind(), ErrType::UploadFailed);
}

#[test]
fn lovart_request_的_base_url_末尾斜杠不会双写() {
    let fake = Fake::new(vec![Ok(ok(json!({ "code": 0, "data": {} })))]);
    let transport: Arc<dyn Transport> = fake.clone();
    let deps = ClientDeps::new("http://replay.invalid///", "ak", "sk", transport);
    lovart_request(&deps, Method::Get, "/chat/status", None, &[]).unwrap();
    let url = fake.last().url;
    assert!(
        url.starts_with("http://replay.invalid/v1/openapi/chat/status"),
        "{url}"
    );
}

// ── 「要花积分」那道闸（2026-10-01 查出的漏判）────────────────────────

/// `/chat/result` 的 `pending_confirmation` **是对象**时，也必须判成「待确认」。
///
/// ── 这条为什么必须有 ──────────────────────────────────────────────
/// 查出的漏判：Rust 这边用 `as_bool()` 读它，而官方样例里它是**对象**
/// （`{"pending_confirmation": {...}}`，里面带花费估算）⇒ 读到 `false` ⇒
/// 当成「已完成」返回 ⇒ **确认闸门被绕过**，任务错落成「未产出」。
/// 黄金用例 `06-needs-confirm` 只喂了 **status 路径**，这条 result 路径**零覆盖**，
/// 所以错了两边都没人发现（TS 原版是 `if (result?.pending_confirmation)`，真值判断）。
///
/// ⚠️ 反向探针：把 `task.rs` 里 `is_some_and(is_truthy)` 换回
/// `and_then(Value::as_bool).unwrap_or(false)`，这条立刻红。
#[test]
fn result_路径的_pending_confirmation_是对象也要判待确认() {
    let (mut deps, fake) = deps_with(vec![
        // ① /chat/status → done
        Ok(ok(json!({ "code": 0, "data": { "status": "done" } }))),
        // ② done 复核再查一次 → 还是 done
        Ok(ok(json!({ "code": 0, "data": { "status": "done" } }))),
        // ③ /chat/result → **对象**形状的待确认（官方样例就是这个形状）
        Ok(ok(json!({
            "code": 0,
            "data": { "pending_confirmation": { "estimated_cost": 12 } }
        }))),
    ]);
    // 复核等待在生产是 5 秒，测试里清零（否则每条用例白等 5 秒）。
    deps.done_recheck = Duration::ZERO;

    let error = poll_thread(
        &deps,
        "t-1",
        &|| AutoConfirmDecision::Disabled,
        &AtomicUsize::new(0),
    )
    .unwrap_err();
    assert_eq!(
        error.kind(),
        ErrType::PendingConfirmation,
        "对象形状的 pending_confirmation 被判成了「已完成」—— 确认闸门被绕过"
    );
    assert_eq!(confirm_requests(&fake), 0, "默认关闭时不许发确认");
}

/// JS 真值判定（对齐 TS `if (x)`）—— 上游那个字段可能是 `true` / 对象 / 字符串。
#[test]
fn js_真值判定_与_TS_对齐() {
    for truthy in [
        json!(true),
        json!({ "estimated_cost": 12 }),
        json!("yes"),
        json!(1),
        json!([1]),
    ] {
        assert!(is_truthy(&truthy), "{truthy} 在 JS 里是真值");
    }
    for falsy in [json!(null), json!(false), json!(0), json!("")] {
        assert!(!is_truthy(&falsy), "{falsy} 在 JS 里是假值");
    }
}

// ── 自动确认（全局开关）────────────────────────────────────────────────
//
// 这组用例守四条红线：
// 1. 默认关闭：一条确认都不许发；
// 2. 开启后：先 POST /chat/confirm，再继续轮询（不能跳过 done 复核）；
// 3. 每任务每批最多一次：第二次 pending 必须转人工；
// 4. 计数在确认成功那一刻落，与轮询最终怎么收场无关。

fn confirm_requests(fake: &Fake) -> usize {
    fake.seen()
        .iter()
        .filter(|request| request.url.contains("/chat/confirm"))
        .count()
}

fn ok_result(url: &str) -> Result<HttpResponse, String> {
    Ok(ok(json!({
        "code": 0,
        "data": { "items": [{ "artifacts": [{ "type": "image", "content": url }] }] }
    })))
}

#[test]
fn 默认关闭_状态路径不确认() {
    let (deps, fake) = deps_with(vec![Ok(ok(
        json!({ "code": 0, "data": { "status": "pending_confirmation" } }),
    ))]);
    let confirmed = AtomicUsize::new(0);
    let error = poll_thread(
        &deps,
        "t-1",
        &|| AutoConfirmDecision::Disabled,
        &confirmed,
    )
    .unwrap_err();

    assert_eq!(error.kind(), ErrType::PendingConfirmation);
    assert!(
        error.message().contains("人工确认"),
        "文案应说清是未开启：{}",
        error.message()
    );
    assert_eq!(confirm_requests(&fake), 0, "默认关闭时不许发确认");
    assert_eq!(confirmed.load(Ordering::Relaxed), 0);
}

#[test]
fn 自动确认开启后_状态路径先确认再继续() {
    let (mut deps, fake) = deps_with(vec![
        Ok(ok(json!({ "code": 0, "data": { "status": "pending_confirmation" } }))),
        Ok(ok(json!({ "code": 0, "data": {} }))), // confirm
        Ok(ok(json!({ "code": 0, "data": { "status": "done" } }))),
        Ok(ok(json!({ "code": 0, "data": { "status": "done" } }))), // done 复核
        ok_result("https://x/a.png"),
    ]);
    deps.done_recheck = Duration::ZERO;

    let auto = AutoConfirm::new(Arc::new(AtomicBool::new(true)));
    let allow = || auto.decide("t-1");
    let result = poll_thread(&deps, "t-1", &allow, auto.confirmed()).unwrap();

    assert_eq!(confirm_requests(&fake), 1, "应恰好确认一次");
    assert_eq!(auto.count(), 1, "确认成功要计数");
    assert_eq!(
        extract_artifacts(&result).unwrap(),
        vec!["https://x/a.png".to_string()]
    );
    let status_calls = fake
        .seen()
        .iter()
        .filter(|request| request.url.contains("/chat/status"))
        .count();
    assert_eq!(
        status_calls, 3,
        "pending 1 次 + done 复核 2 次，确认不能跳过状态机"
    );
}

#[test]
fn 自动确认开启后_结果路径先确认再继续() {
    let (mut deps, fake) = deps_with(vec![
        Ok(ok(json!({ "code": 0, "data": { "status": "done" } }))),
        Ok(ok(json!({ "code": 0, "data": { "status": "done" } }))),
        Ok(ok(json!({
            "code": 0,
            "data": { "pending_confirmation": { "estimated_cost": 12 } }
        }))),
        Ok(ok(json!({ "code": 0, "data": {} }))), // confirm
        Ok(ok(json!({ "code": 0, "data": { "status": "done" } }))),
        Ok(ok(json!({ "code": 0, "data": { "status": "done" } }))),
        ok_result("https://x/b.png"),
    ]);
    deps.done_recheck = Duration::ZERO;

    let auto = AutoConfirm::new(Arc::new(AtomicBool::new(true)));
    let allow = || auto.decide("t-1");
    let result = poll_thread(&deps, "t-1", &allow, auto.confirmed()).unwrap();

    assert_eq!(confirm_requests(&fake), 1);
    assert_eq!(auto.count(), 1);
    assert_eq!(
        extract_artifacts(&result).unwrap(),
        vec!["https://x/b.png".to_string()]
    );
}

#[test]
fn 同一任务第二次_pending_不再确认() {
    let (deps, fake) = deps_with(vec![
        Ok(ok(json!({ "code": 0, "data": { "status": "pending_confirmation" } }))),
        Ok(ok(json!({ "code": 0, "data": {} }))), // confirm
        Ok(ok(json!({ "code": 0, "data": { "status": "pending_confirmation" } }))),
    ]);

    let auto = AutoConfirm::new(Arc::new(AtomicBool::new(true)));
    let allow = || auto.decide("t-1");
    let error = poll_thread(&deps, "t-1", &allow, auto.confirmed()).unwrap_err();

    assert_eq!(error.kind(), ErrType::PendingConfirmation);
    assert!(
        error.message().contains("已自动确认过一次"),
        "必须说清「再确认会再扣一次」：{}",
        error.message()
    );
    assert_eq!(confirm_requests(&fake), 1, "同一任务本批只能确认一次");
    assert_eq!(auto.count(), 1);
}

#[test]
fn 自动确认失败_只发一次且转人工() {
    let (deps, fake) = deps_with(vec![
        Ok(ok(json!({ "code": 0, "data": { "status": "pending_confirmation" } }))),
        Ok(http(500, r#"{"code":500,"message":"boom"}"#)),
    ]);

    let auto = AutoConfirm::new(Arc::new(AtomicBool::new(true)));
    let allow = || auto.decide("t-1");
    let error = poll_thread(&deps, "t-1", &allow, auto.confirmed()).unwrap_err();

    assert_eq!(error.kind(), ErrType::PendingConfirmation);
    assert!(
        error.message().contains("自动确认失败"),
        "文案要说明是确认失败：{}",
        error.message()
    );
    assert!(
        error.message().contains("boom"),
        "上游原文必须透传：{}",
        error.message()
    );
    assert_eq!(confirm_requests(&fake), 1, "确认失败绝不重试");
    assert_eq!(auto.count(), 0, "失败不计账");
}

#[test]
fn 确认成功后轮询失败_计数不丢() {
    let (deps, _fake) = deps_with(vec![
        Ok(ok(json!({ "code": 0, "data": { "status": "pending_confirmation" } }))),
        Ok(ok(json!({ "code": 0, "data": {} }))), // confirm
        Ok(http(401, r#"{"code":401,"message":"nope"}"#)),
    ]);

    let auto = AutoConfirm::new(Arc::new(AtomicBool::new(true)));
    let allow = || auto.decide("t-1");
    let error = poll_thread(&deps, "t-1", &allow, auto.confirmed()).unwrap_err();

    assert_eq!(auto.count(), 1, "钱花了，账不能随轮询错误丢掉");
    assert_ne!(
        error.kind(),
        ErrType::PendingConfirmation,
        "这是轮询硬失败，不是待确认"
    );
}

#[test]
fn 关掉开关后_不再确认() {
    let enabled = Arc::new(AtomicBool::new(true));
    let auto = AutoConfirm::new(Arc::clone(&enabled));

    let (deps, _) = deps_with(vec![
        Ok(ok(json!({ "code": 0, "data": { "status": "pending_confirmation" } }))),
        Ok(ok(json!({ "code": 0, "data": {} }))),
        Ok(ok(json!({ "code": 0, "data": { "status": "pending_confirmation" } }))),
    ]);
    let allow = || auto.decide("t-1");
    let _ = poll_thread(&deps, "t-1", &allow, auto.confirmed()).unwrap_err();

    // 关掉后新一次轮询：pending 直接转人工，且不再发确认。
    enabled.store(false, Ordering::Relaxed);
    let (deps2, fake2) = deps_with(vec![Ok(ok(
        json!({ "code": 0, "data": { "status": "pending_confirmation" } }),
    ))]);
    let error = poll_thread(&deps2, "t-1", &allow, auto.confirmed()).unwrap_err();

    assert!(
        error.message().contains("人工确认"),
        "关掉后应报未开启：{}",
        error.message()
    );
    assert_eq!(confirm_requests(&fake2), 0);
}

/// 服务第一条 status 时把开关关掉 —— 用来证明开关是在「确认那一刻」读的，
/// 不是轮询开始时快照（若是快照，这条用例会确认一次而变红）。
struct FlipTransport {
    inner: Arc<Fake>,
    enabled: Arc<AtomicBool>,
}

impl Transport for FlipTransport {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, String> {
        if request.url.contains("/chat/status") {
            self.enabled.store(false, Ordering::Relaxed);
        }
        self.inner.send(request)
    }
}

#[test]
fn 开关在确认那一刻才求值() {
    let fake = Fake::new(vec![Ok(ok(
        json!({ "code": 0, "data": { "status": "pending_confirmation" } }),
    ))]);
    let enabled = Arc::new(AtomicBool::new(true));
    let transport: Arc<dyn Transport> = Arc::new(FlipTransport {
        inner: Arc::clone(&fake),
        enabled: Arc::clone(&enabled),
    });
    let deps = ClientDeps::new("http://replay.invalid", "ak", "sk", transport);
    let auto = AutoConfirm::new(Arc::clone(&enabled));
    let allow = || auto.decide("t-1");
    let error = poll_thread(&deps, "t-1", &allow, auto.confirmed()).unwrap_err();

    assert!(error.message().contains("人工确认"), "{}", error.message());
    assert_eq!(
        confirm_requests(&fake),
        0,
        "开关在轮询开始后关掉，就不该再确认"
    );
}
