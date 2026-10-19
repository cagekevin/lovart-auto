//! 全局限速闸门 / 429 熔断 / retry-failed 的契约测试。
//!
//! 全部走 `support::run_fixture`（真调度器 + 真轮询机 + 脚本化 Transport），零网络。

mod support;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lovart_client::lovart::prompt::{parse_prompts, ParseOptions};
use serde_json::{json, Value};

static SEQ: AtomicUsize = AtomicUsize::new(0);

struct TempFixture {
    dir: PathBuf,
}

impl TempFixture {
    fn new(prompts: &str, upstream: serde_json::Value) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "lovart-gate-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).expect("建临时 fixture 目录");
        fs::write(dir.join("prompts.jsonl"), prompts).expect("写 prompts.jsonl");
        fs::write(dir.join("upstream.json"), upstream.to_string()).expect("写 upstream.json");
        Self { dir }
    }

    fn seed(self, state: &str) -> Self {
        fs::write(self.dir.join("initial-state.jsonl"), state).expect("写 initial-state.jsonl");
        self
    }

    fn run(&self) -> support::FixtureRun {
        support::run_fixture(&self.dir)
    }
}

impl Drop for TempFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn prompt_ids(prompts: &str) -> Vec<String> {
    parse_prompts(prompts, &support::defaults(), &ParseOptions::default())
        .tasks
        .iter()
        .map(|task| task.id.clone())
        .collect()
}

fn last_record(state_text: &str, id: &str) -> Value {
    state_text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value.get("id").and_then(Value::as_str) == Some(id))
        .last()
        .unwrap_or_else(|| panic!("state 里找不到 {id}"))
}

fn done_script(extra: serde_json::Value) -> serde_json::Value {
    let mut base = json!({
        "threadIdPrefix": "t",
        "polls": [{ "status": "done" }],
        "result": { "items": [{ "artifacts": [{ "content": "https://cdn.invalid/a.png" }] }] },
        "taskIntervalMs": 1,
        "rateLimitBackoffMs": 1
    });
    if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    base
}

#[test]
fn 提交_429_开熔断且不写_busy_retries() {
    let script = done_script(json!({
        "send": [
            { "error": "rate limited", "httpStatus": 429 },
            {}
        ],
        "maxAttempts": 1,
        "maxBusyRetries": 5
    }));
    let f = TempFixture::new("{\"prompt\":\"a\"}\n", script);
    let run = f.run();

    let record = &run.normalized.final_records[0];
    assert_eq!(record.status, "done", "429 后应重投成功");
    assert_eq!(record.busy_retries, 0, "429 不计入 busy_retries");
    assert!(
        run.logs.iter().any(|line| line.contains("🚧")),
        "应打全局熔断日志，实际日志：{:?}",
        run.logs
    );
}

#[test]
fn 并发满_402_只退避单任务不熔断() {
    let script = done_script(json!({
        "send": [
            { "error": "concurrent", "httpStatus": 402 },
            {}
        ],
        "maxAttempts": 1,
        "maxBusyRetries": 5
    }));
    let f = TempFixture::new("{\"prompt\":\"a\"}\n", script);
    let run = f.run();

    let record = &run.normalized.final_records[0];
    assert_eq!(record.status, "done");
    assert_eq!(record.busy_retries, 1, "402 仍按任务计次");
    assert!(
        !run.logs.iter().any(|line| line.contains("🚧")),
        "402 不应开全局熔断"
    );
}

#[test]
fn 提交之间遵守全局间隔() {
    let script = done_script(json!({ "concurrency": 3, "taskIntervalMs": 100 }));
    let f = TempFixture::new(
        "{\"prompt\":\"a\"}\n{\"prompt\":\"b\"}\n{\"prompt\":\"c\"}\n",
        script,
    );
    let run = f.run();

    assert_eq!(run.normalized.final_records.len(), 3);
    assert!(run.send_times.len() >= 3, "三次提交都要被记录");
    for pair in run.send_times.windows(2) {
        // 首次提交要额外走一次 set_mode（惰性初始化），send 时间比 claim 晚几毫秒；
        // 留 20ms 容差，断言的是「全局间隔确实生效」，不是精确到毫秒。
        assert!(
            pair[1] - pair[0] >= Duration::from_millis(80),
            "提交间隔不足 80ms（100ms 间隔，留 20ms 给首次 set_mode）：{:?}",
            pair
        );
    }
}

#[test]
fn 轮询_429_保持_running_且不重复提交() {
    let script = done_script(json!({
        "polls": [
            { "httpStatus": 429, "errorBody": { "code": 1429, "error": "rate limited" } },
            { "status": "done" }
        ]
    }));
    let f = TempFixture::new("{\"prompt\":\"a\"}\n", script);
    let run = f.run();

    assert_eq!(run.normalized.final_records[0].status, "done");
    assert!(
        run.logs.iter().any(|line| line.contains("轮询撞限流")),
        "应打轮询限流日志，实际日志：{:?}",
        run.logs
    );
    assert_eq!(
        run.calls.iter().filter(|path| *path == "/chat").count(),
        1,
        "轮询 429 不应重新提交"
    );
}

#[test]
fn 轮询_429_会等熔断到期再重试() {
    let script = done_script(json!({
        "polls": [
            { "httpStatus": 429, "errorBody": { "code": 1429, "error": "rate limited" } },
            { "status": "done" }
        ],
        "rateLimitBackoffMs": 80
    }));
    let f = TempFixture::new("{\"prompt\":\"a\"}\n", script);
    let run = f.run();

    assert_eq!(run.normalized.final_records[0].status, "done");
    assert!(run.poll_times.len() >= 2, "应有两次轮询：429 + done");
    assert!(
        run.poll_times[1] - run.poll_times[0] >= Duration::from_millis(70),
        "轮询 429 后应等熔断（80ms）再重试，而不是紧循环：{:?}",
        run.poll_times
    );
}

#[test]
fn 排队模式只设置一次() {
    let script = done_script(json!({ "concurrency": 3, "taskIntervalMs": 0 }));
    let f = TempFixture::new(
        "{\"prompt\":\"a\"}\n{\"prompt\":\"b\"}\n{\"prompt\":\"c\"}\n",
        script,
    );
    let run = f.run();

    assert_eq!(
        run.calls.iter().filter(|path| *path == "/mode/set").count(),
        1,
        "set_mode 只应调用一次"
    );
}

#[test]
fn retry_failed_重置无_threadid的失败任务() {
    let prompt = "甲：前两次限流，第三次成功";
    let id = "45c6d95b39d5";
    let seed = format!(
        "{{\"id\":\"{id}\",\"prompt\":\"{prompt}\",\"line\":1,\"model\":\"nano-bn-2\",\"aspectRatio\":\"3:4\",\"imageSize\":\"2K\",\"status\":\"failed\",\"attempts\":0,\"timeoutCount\":0,\"busyRetries\":61,\"readyAt\":0,\"stateSince\":\"2026-10-01T06:55:44.506Z\",\"updatedAt\":\"2026-10-01T06:55:44.506Z\",\"error\":\"服务端持续并发满 61 次，放弃\"}}\n"
    );
    let script = done_script(json!({ "retryFailed": true }));
    let f = TempFixture::new(&format!("{{\"prompt\":\"{prompt}\"}}\n"), script).seed(&seed);
    let run = f.run();

    let record = &run.normalized.final_records[0];
    assert_eq!(record.status, "done", "failed 应被重置并重跑");
    assert_eq!(record.busy_retries, 0, "重置后计数归零");
}

#[test]
fn 兜底_429_只落_unresolved_不_requeue() {
    let script = json!({
        "threadIdPrefix": "t",
        "polls": [
            { "status": "running" },
            { "httpStatus": 429, "errorBody": { "code": 1429, "error": "rate limited" } }
        ],
        "result": { "items": [{ "artifacts": [{ "content": "https://cdn.invalid/a.png" }] }] },
        "taskTimeoutMs": 1,
        "maxTimeouts": 1,
        "taskIntervalMs": 1,
        "rateLimitBackoffMs": 1
    });
    let f = TempFixture::new("{\"prompt\":\"a\"}\n", script);
    let run = f.run();

    assert_eq!(run.normalized.final_records[0].status, "unresolved");
    assert!(
        run.logs.iter().any(|line| line.contains("轮询撞限流")),
        "应打轮询限流日志：{:?}",
        run.logs
    );
    assert_eq!(
        run.calls.iter().filter(|path| *path == "/chat").count(),
        1,
        "兜底 429 不应重新提交"
    );
}

#[test]
fn finish_不改别人_ready_at() {
    let prompts = "{\"prompt\":\"a\"}\n{\"prompt\":\"b\"}\n";
    let ids = prompt_ids(prompts);
    let defaults = support::defaults();
    // 200ms 足够 b 先完成；a 的 readyAt 若被 finish 清零，会在 submitting 行里留下 0。
    let future = now_ms() + 200;
    let seed = format!(
        "{{\"id\":\"{}\",\"prompt\":\"a\",\"line\":1,\"model\":\"{}\",\"aspectRatio\":\"{}\",\"imageSize\":\"{}\",\"status\":\"pending\",\"attempts\":0,\"timeoutCount\":0,\"busyRetries\":0,\"readyAt\":{},\"stateSince\":\"2026-10-01T06:55:44.506Z\",\"updatedAt\":\"2026-10-01T06:55:44.506Z\"}}\n",
        ids[0], defaults.model, defaults.aspect_ratio, defaults.image_size, future
    );
    let script = done_script(json!({ "concurrency": 2 }));
    let f = TempFixture::new(prompts, script).seed(&seed);
    let run = f.run();

    let submitting = run
        .state_text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value.get("id").and_then(Value::as_str) == Some(&ids[0]))
        .find(|value| value.get("status").and_then(Value::as_str) == Some("submitting"))
        .expect("第一个任务应被提交");
    assert_eq!(
        submitting.get("readyAt").and_then(Value::as_u64),
        Some(future),
        "finish 不应把别人的 readyAt 清零"
    );
}

#[test]
fn retry_failed_保留_refs_且跳过有_threadid的() {
    let prompts = "{\"prompt\":\"a\"}\n{\"prompt\":\"b\"}\n";
    let ids = prompt_ids(prompts);
    let defaults = support::defaults();
    let first = format!(
        "{{\"id\":\"{}\",\"prompt\":\"a\",\"line\":1,\"model\":\"{}\",\"aspectRatio\":\"{}\",\"imageSize\":\"{}\",\"status\":\"failed\",\"attempts\":0,\"timeoutCount\":0,\"busyRetries\":61,\"readyAt\":0,\"refUrls\":[\"https://cdn.invalid/ref.png\"],\"stateSince\":\"2026-10-01T06:55:44.506Z\",\"updatedAt\":\"2026-10-01T06:55:44.506Z\",\"error\":\"旧错误\"}}\n",
        ids[0], defaults.model, defaults.aspect_ratio, defaults.image_size
    );
    let second = format!(
        "{{\"id\":\"{}\",\"prompt\":\"b\",\"line\":2,\"model\":\"{}\",\"aspectRatio\":\"{}\",\"imageSize\":\"{}\",\"status\":\"failed\",\"attempts\":0,\"timeoutCount\":0,\"busyRetries\":61,\"readyAt\":0,\"threadId\":\"t-old\",\"stateSince\":\"2026-10-01T06:55:44.506Z\",\"updatedAt\":\"2026-10-01T06:55:44.506Z\",\"error\":\"旧错误\"}}\n",
        ids[1], defaults.model, defaults.aspect_ratio, defaults.image_size
    );
    let script = done_script(json!({ "retryFailed": true }));
    let f = TempFixture::new(prompts, script).seed(&format!("{first}{second}"));
    let run = f.run();

    let first = last_record(&run.state_text, &ids[0]);
    assert_eq!(first.get("status").and_then(Value::as_str), Some("done"));
    assert_eq!(
        first
            .get("refUrls")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(1),
        "重置必须保留垫图地址"
    );
    let second = last_record(&run.state_text, &ids[1]);
    assert_eq!(
        second.get("status").and_then(Value::as_str),
        Some("failed"),
        "有 threadId 的 Failed 不应被重置"
    );
}

#[test]
fn 端到端_160_任务_前三次_429_全部完成() {
    let prompts: String = (0..160)
        .map(|index| format!("{{\"prompt\":\"p{index}\"}}\n"))
        .collect();
    let script = json!({
        "threadIdPrefix": "t",
        "send": [
            { "error": "rate limited", "httpStatus": 429 },
            { "error": "rate limited", "httpStatus": 429 },
            { "error": "rate limited", "httpStatus": 429 },
            {}
        ],
        "polls": [{ "status": "done" }],
        "result": { "items": [{ "artifacts": [{ "content": "https://cdn.invalid/a.png" }] }] },
        "concurrency": 7,
        "taskIntervalMs": 1,
        "rateLimitBackoffMs": 1
    });
    let f = TempFixture::new(&prompts, script);
    let run = f.run();

    assert_eq!(run.normalized.final_records.len(), 160);
    assert!(
        run.normalized.final_records.iter().all(|r| r.status == "done"),
        "应全部完成"
    );
    let sends = run.calls.iter().filter(|path| *path == "/chat").count();
    assert!(sends <= 160 + 3 + 5, "提交次数应有界，实际 {sends}");
}
