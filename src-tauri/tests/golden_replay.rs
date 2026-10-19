//! 黄金用例回放 —— **Rust 回放的产出必须与录下的黄金逐条一致**。
//!
//! 这是**唯一的机器防线**：`fixtures/` 里是参照实现当年录下的期望状态。
//! 光验「参照实现 == 黄金」证明不了 Rust 一致 —— 尺子造好了却只量了造尺子的人。
//!
//! ⚠️ 黄金**已不能再自动生成**（生成器是参照实现那一侧的）⇒ 它是**冻结**的，要改只能手写。
//! 所以本文件里除比对外还有**显式规则断言** —— 黄金本身可能是错的，光比黄金发现不了。
//!
//! ── 为什么在**传输层**回放（与参照实现当年的判断一致）──────────────────
//!
//! `Ports::poll` = `poll_thread` —— 那**就是轮询状态机本身**。在端口层替换它，
//! 黄金用例就覆盖不到「done 复核 / `pending_confirmation` / 退避 / 总超时」，
//! 而这些正是 Rust 的 `lovart/task.rs` 要负责的规则。
//! ⇒ 回放放在 [`Transport`]（最底层），让**真**调度器 + **真**轮询机 + **真**协议层全部跑起来。
//!
//! 唯一例外是 `download`：它不走 `Transport`（走 reqwest 直连）⇒ 必须在**端口层**替换，
//! 否则回放会真的联网下载。这与 TS 侧的「唯一例外」是同一条理由。
//!
//! ── 三条纪律（与 TS 侧逐条对齐）────────────────────────────────────────
//!
//! ① **不联网、不写产物盘**：`transport` 返回构造出来的响应；`download` 只返回假路径。
//! ② **带总超时**：轮询有次数预算，超了就**判失败**而不是让测试**挂住**
//!    —— 规则被写坏会导致无限循环，挂住的测试没有任何信息量（TD-04-01 的教训）。
//! ③ **对账剥时间**：走 `normalize_state_text`（与 TS `normalizeStateFile` 同口径）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use lovart_client::engine::scheduler::{
    run_scheduler, AutoConfirm, Batch, Ports, RealPorts, SchedulerOptions,
};
use lovart_client::engine::state::{normalize_state_text, StateStore};
use lovart_client::engine::download::DownloadedFile;
use lovart_client::lovart::client::{
    ClientDeps, HttpRequest, HttpResponse, Transport,
};
use lovart_client::lovart::config::PATH_PREFIX;
use lovart_client::lovart::contract::SendInput;
use lovart_client::lovart::errors::LovartError;
use lovart_client::lovart::prompt::{parse_prompts, ParseOptions, PromptDefaults};
use lovart_client::lovart::task::AutoConfirmDecision;

/// 轮询次数预算：超过就判「未收敛」。
///
/// ⚠️ 这不是防上游慢，是**把死循环变成一次可读的失败** ——
/// 写坏的规则会让测试**挂住**而不是变红，而挂住什么信息都不给（TD-04-01 的教训）。
const POLL_BUDGET: usize = 500;

/// 一组黄金用例的三件套。
struct Fixture {
    name: String,
    dir: PathBuf,
}

// ── 脚本化上游（传输层）──────────────────────────────────────────────────

/// `upstream.json` 里的一次提交尝试。`{}` = 成功。
#[derive(Debug, Clone, Default)]
struct SendStep {
    error: Option<String>,
    http_status: Option<u16>,
    biz_code: Option<i64>,
}

#[derive(Default)]
struct ScriptedState {
    send_attempt: usize,
    thread_seq: usize,
    /// **按 thread 独立**的轮询索引 —— 全局索引会让多任务互相交错，黄金就不可复现了。
    poll_index: BTreeMap<String, usize>,
    poll_calls: usize,
}

/// 脚本化的出站传输：**不联网**，按 `upstream.json` 作答。
struct ScriptedTransport {
    project_id: String,
    thread_id_prefix: String,
    send_steps: Vec<SendStep>,
    polls: Vec<Value>,
    result: Option<Value>,
    /// URL → 是否模拟下载失败（下载本身在端口层，这里只留给断言用）。
    state: Mutex<ScriptedState>,
}

fn json_response(body: Value, status: u16) -> HttpResponse {
    HttpResponse {
        status,
        body: serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string()),
    }
}

fn ok_envelope(data: Value) -> HttpResponse {
    json_response(json!({ "code": 0, "data": data }), 200)
}

impl Transport for ScriptedTransport {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, String> {
        // 只取路径（去掉 base_url 与 PATH_PREFIX），与 TS 侧同一套分发。
        let path = request
            .url
            .split_once(PATH_PREFIX)
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_else(|| request.url.clone());
        let path = path.split('?').next().unwrap_or("").to_string();

        match path.as_str() {
            "/project/save" => Ok(ok_envelope(json!({ "project_id": self.project_id }))),
            "/mode/set" | "/mode/query" => Ok(ok_envelope(json!({}))),
            "/chat" => {
                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                let index = state.send_attempt.min(self.send_steps.len().saturating_sub(1));
                let step = self.send_steps.get(index).cloned().unwrap_or_default();
                state.send_attempt += 1;
                drop(state);

                if let Some(error) = step.error {
                    // 给了 httpStatus ⇒ 走非 2xx（`build_upstream_error` 解 bizCode）；
                    // 否则走 2xx + 信封 code（`parse_envelope` 抛）。
                    let status = step.http_status.unwrap_or(200);
                    return Ok(json_response(
                        json!({ "code": step.biz_code.unwrap_or(1), "error": error }),
                        status,
                    ));
                }
                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                state.thread_seq += 1;
                let thread_id = format!("{}-{}", self.thread_id_prefix, state.thread_seq);
                Ok(ok_envelope(json!({ "thread_id": thread_id })))
            }
            "/chat/status" => {
                let thread_id = request
                    .url
                    .split_once("thread_id=")
                    .map(|(_, rest)| rest.split('&').next().unwrap_or("").to_string())
                    .unwrap_or_default();

                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                state.poll_calls += 1;
                if state.poll_calls > POLL_BUDGET {
                    // 纪律②：超预算 ⇒ 抛（非瞬态）⇒ 任务判失败、循环结束，**测试失败而不是挂住**。
                    return Err(format!(
                        "回放：轮询超过 {POLL_BUDGET} 次 —— 调度器未收敛（疑似无限循环）"
                    ));
                }
                let index = *state.poll_index.get(&thread_id).unwrap_or(&0);
                state.poll_index.insert(thread_id, index + 1);
                drop(state);

                let last = self.polls.len().saturating_sub(1);
                let step = self
                    .polls
                    .get(index.min(last))
                    .cloned()
                    .unwrap_or_else(|| json!({ "status": "running" }));

                let mut data = step.as_object().cloned().unwrap_or_default();
                let http_status = data.remove("httpStatus").and_then(|v| v.as_u64());
                let error_body = data.remove("errorBody");
                if let Some(status) = http_status {
                    let body = error_body
                        .unwrap_or_else(|| json!({ "code": 1, "error": "replay http error" }));
                    return Ok(json_response(body, status as u16));
                }
                Ok(ok_envelope(Value::Object(data)))
            }
            "/chat/result" => {
                let fallback = self.polls.last().cloned().unwrap_or(Value::Null);
                Ok(ok_envelope(self.result.clone().unwrap_or(fallback)))
            }
            // `/chat/confirm` 等：成功但不做事。
            _ => Ok(ok_envelope(json!({}))),
        }
    }
}

// ── 端口层：只替换下载（见文件头「唯一例外」）─────────────────────────────

struct ReplayPorts {
    inner: RealPorts,
    downloads: BTreeMap<String, bool>,
    download_count: AtomicUsize,
}

impl Ports for ReplayPorts {
    fn set_mode(&self, deps: &ClientDeps, unlimited: bool) -> Result<(), LovartError> {
        self.inner.set_mode(deps, unlimited)
    }

    fn send(&self, deps: &ClientDeps, input: &SendInput) -> Result<(String, String), LovartError> {
        self.inner.send(deps, input)
    }

    fn poll(
        &self,
        deps: &ClientDeps,
        thread_id: &str,
        task_timeout: Duration,
        allow: &dyn Fn() -> AutoConfirmDecision,
        confirmed: &AtomicUsize,
    ) -> Result<Value, LovartError> {
        self.inner
            .poll(deps, thread_id, task_timeout, allow, confirmed)
    }

    fn extract_artifacts(&self, result: &Value) -> Result<Vec<String>, LovartError> {
        self.inner.extract_artifacts(result)
    }

    fn download(
        &self,
        url: &str,
        output_dir: &Path,
        file_base: &str,
        _retries: u32,
    ) -> Result<DownloadedFile, LovartError> {
        self.download_count.fetch_add(1, Ordering::SeqCst);
        if self.downloads.get(url).copied().unwrap_or(false) {
            return Err(LovartError::transport("回放：模拟下载失败"));
        }
        Ok(DownloadedFile {
            path: output_dir.join(format!("{file_base}.png")),
            bytes: 1,
            reused: false,
        })
    }
}

// ── 跑一个 fixture ──────────────────────────────────────────────────────

/// 读 `fixtures/*/upstream.json` 的脚本。
fn read_script(text: &str) -> (ScriptedTransport, usize, Option<u64>, Option<u32>, Option<u32>, Option<u32>, usize, BTreeMap<String, bool>) {
    let script: Value = serde_json::from_str(text).expect("upstream.json 必须是合法 JSON");
    let get_u64 = |key: &str| script.get(key).and_then(Value::as_u64);
    let get_u32 = |key: &str| script.get(key).and_then(Value::as_u64).map(|v| v as u32);

    let send_steps: Vec<SendStep> = script
        .get("send")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| SendStep {
                    error: item.get("error").and_then(Value::as_str).map(str::to_string),
                    http_status: item.get("httpStatus").and_then(Value::as_u64).map(|v| v as u16),
                    biz_code: item.get("bizCode").and_then(Value::as_i64),
                })
                .collect()
        })
        .unwrap_or_else(|| vec![SendStep::default()]);

    let downloads = script
        .get("downloads")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(url, value)| (url.clone(), value.as_str() == Some("fail")))
                .collect()
        })
        .unwrap_or_default();

    let transport = ScriptedTransport {
        project_id: script
            .get("projectId")
            .and_then(Value::as_str)
            .unwrap_or("replay-project")
            .to_string(),
        thread_id_prefix: script
            .get("threadIdPrefix")
            .and_then(Value::as_str)
            .unwrap_or("t")
            .to_string(),
        send_steps,
        polls: script
            .get("polls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_else(|| vec![json!({ "status": "done" })]),
        result: script.get("result").cloned(),
        state: Mutex::new(ScriptedState::default()),
    };

    (
        transport,
        get_u64("runs").unwrap_or(1) as usize,
        get_u64("taskTimeoutMs"),
        get_u32("maxAttempts"),
        get_u32("maxBusyRetries"),
        get_u32("maxTimeouts"),
        get_u64("concurrency").unwrap_or(1) as usize,
        downloads,
    )
}

/// 回放一个 fixture，返回产出的 `state.jsonl` 全文。
fn run_fixture(fixture: &Fixture) -> String {
    let prompts_text = std::fs::read_to_string(fixture.dir.join("prompts.jsonl"))
        .unwrap_or_else(|error| panic!("{} 读不了 prompts.jsonl：{error}", fixture.name));
    let script_text = std::fs::read_to_string(fixture.dir.join("upstream.json"))
        .unwrap_or_else(|error| panic!("{} 读不了 upstream.json：{error}", fixture.name));

    let (transport, runs, task_timeout_ms, max_attempts, max_busy_retries, max_timeouts, concurrency, downloads) =
        read_script(&script_text);

    let defaults = PromptDefaults::default();
    let parsed = parse_prompts(&prompts_text, &defaults, &ParseOptions::default());
    assert!(
        parsed.errors.is_empty(),
        "{} 的 prompts.jsonl 不合法：{:?}",
        fixture.name,
        parsed.errors
    );

    // 临时目录（不污染 fixtures/）。用进程 id + 名字保证并发跑测试时不互相踩。
    let temp = std::env::temp_dir().join(format!("lv-golden-{}-{}", std::process::id(), fixture.name));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).expect("建临时目录");

    let state_file = temp.join("state.jsonl");

    // 可选预置状态：用于「参数变更重跑」这类需要**先有一条已存在记录**的场景。
    // ⚠️ 只容忍「文件不存在」，其它错误（权限等）照抛 —— 与 TS 侧同口径。
    match std::fs::read_to_string(fixture.dir.join("initial-state.jsonl")) {
        Ok(seed) => std::fs::write(&state_file, seed).expect("写预置状态"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("{} 的 initial-state.jsonl 读不了：{error}", fixture.name),
    }

    let task_timeout = Duration::from_millis(task_timeout_ms.unwrap_or(60_000));

    // ⚠️ transport **跨轮共用**（与 TS 侧 `makeReplayTransport` 建在循环外一致）：
    //    轮询索引按 thread 累积，重建会让第二轮又从第一个 poll 开始 —— 那就不叫回放了。
    let transport: Arc<dyn Transport> = Arc::new(transport);

    for _round in 0..runs {
        // 每轮**重新打开** state —— 补下载 / 兜底复查 都是「第二次运行」才发生的行为。
        let state = StateStore::open(&state_file).expect("打开状态文件");

        let deps = ClientDeps {
            base_url: "http://replay.invalid".to_string(),
            access_key: "replay-ak".to_string(),
            secret_key: "replay-sk".to_string(),
            transport: Arc::clone(&transport),
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            // 回放不真等：轮询间隔与 done 复核都压到 1ms
            request_timeout: Duration::from_millis(5_000),
            task_timeout,
            poll_interval: Duration::from_millis(1),
            done_recheck: Duration::from_millis(1),
        };

        let _ = run_scheduler(SchedulerOptions {
            deps,
            batch: Batch::Declared(parsed.tasks.clone()),
            state,
            output_dir: temp.join("out"),
            concurrency,
            task_timeout: Some(task_timeout),
            recheck_timeout: Some(Duration::from_millis(1)),
            max_timeouts,
            download_retries: Some(1),
            max_attempts,
            max_busy_retries,
            task_interval: Some(Duration::from_millis(1)),
            rate_limit_backoff_base: Some(Duration::from_millis(1)),
            retry_failed: false,
            busy_backoff_base: Some(Duration::from_millis(1)),
            // 黄金用例录的是"默认关闭"的行为：这里必须恒关，否则回放会去真确认。
            auto_confirm: AutoConfirm::disabled(),
            log: Arc::new(|_message: &str| {}),
            ports: Some(Arc::new(ReplayPorts {
                inner: RealPorts::new(),
                downloads: downloads.clone(),
                download_count: AtomicUsize::new(0),
            })),
        });
    }

    let text = std::fs::read_to_string(&state_file).expect("读回产出的状态文件");
    let _ = std::fs::remove_dir_all(&temp);
    text
}

fn fixtures() -> Vec<Fixture> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures");
    let mut out: Vec<Fixture> = std::fs::read_dir(&root)
        .unwrap_or_else(|error| panic!("读不了 {}：{error}", root.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .map(|dir| Fixture {
            name: dir.file_name().unwrap_or_default().to_string_lossy().to_string(),
            dir,
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

// ── 断言 ────────────────────────────────────────────────────────────────

/// **L1 + L2 对账**：Rust 回放产出 == TS 录下的黄金。
///
/// ⚠️ **L3（迁移顺序）刻意不比** —— 退避带随机 jitter，并发下顺序天然不可复现，
/// 比了就是造一个必然 flaky 的测试。
#[test]
fn golden_回放_rust_与_ts_产出一致() {
    let all = fixtures();
    assert!(
        !all.is_empty(),
        "fixtures/ 里一个用例都没有 —— 黄金用例是这条对账的全部依据"
    );

    let mut failures: Vec<String> = Vec::new();

    for fixture in &all {
        let expected_text = std::fs::read_to_string(fixture.dir.join("expected-state.jsonl"))
            .unwrap_or_else(|error| panic!("{} 读不了 expected-state.jsonl：{error}", fixture.name));
        let actual_text = run_fixture(fixture);

        let expected = normalize_state_text(&expected_text);
        let actual = normalize_state_text(&actual_text);

        // L1：每个 id 的最终状态 + 语义字段
        if expected.final_records != actual.final_records {
            failures.push(format!(
                "[{}] L1 最终状态不一致\n  TS  : {:#?}\n  Rust: {:#?}",
                fixture.name, expected.final_records, actual.final_records
            ));
        }
        // L2：每个 id 出现过哪些状态（能抓到「多绕了一圈」）
        if expected.seen != actual.seen {
            failures.push(format!(
                "[{}] L2 状态集合不一致\n  TS  : {:?}\n  Rust: {:?}",
                fixture.name, expected.seen, actual.seen
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "黄金对账失败（{} / {} 组）：\n\n{}",
        failures.len(),
        all.len(),
        failures.join("\n\n")
    );

    println!("黄金对账全绿：{} 组", all.len());
}
