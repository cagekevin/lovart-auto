//! `replay` — 黄金用例的**脚本化上游**（对应 TS `test/support/replay.ts`）。
//!
//! ── 为什么在**传输层**回放，而不是 `Ports` 层 ────────────────────────
//! `Ports::poll` = `poll_thread` —— 那**就是轮询状态机本身**。
//! 在端口层替换它 ⇒ 黄金用例覆盖不到「done 复核 / `pending_confirmation` / 退避 / 总超时」，
//! 而这些正是要验证的 `task.rs`。
//! ⇒ 回放放在 `Transport`（最底层），让**真**调度器 + **真**轮询机 + **真**协议层全部跑起来。
//!
//! 唯一例外是 `download`：它不走 `Transport`（直接 reqwest）⇒ 必须在**端口层**替换，
//! 否则回放会真的联网下载。
//!
//! ── 三条纪律（与 TS 同）──────────────────────────────────────────────
//! ① **不联网、不写产物盘**：`Transport` 返回构造出来的响应；`download` 只返回假路径。
//! ② **带总超时**：轮询有次数预算 + 墙钟看门狗 —— 否则一条写坏的规则会让测试**挂住**
//!    而不是失败（本仓 TD-04-01 的教训）。
//! ③ **对账剥时间**：见 `state::normalize_state_text`。

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use lovart_client::engine::download::DownloadedFile;
use lovart_client::engine::scheduler::{
    run_scheduler, Batch, Ports, RealPorts, SchedulerOptions, SchedulerSummary,
};
use lovart_client::engine::state::{normalize_state_text, NormalizedState, StateStore};
use lovart_client::lovart::client::{ClientDeps, HttpRequest, HttpResponse, Transport};
use lovart_client::lovart::errors::LovartError;
use lovart_client::lovart::prompt::{parse_prompts, ParseOptions, PromptDefaults, PromptTask};

/// 与 `run.ts` 的 `DEFAULTS` 一致 —— 黄金用例不该依赖命令行默认值漂移。
pub fn defaults() -> PromptDefaults {
    PromptDefaults::default()
}

/// 轮询次数预算：超过就判「未收敛」。把死循环变成**失败**，而不是让测试挂住。
const POLL_BUDGET: usize = 500;

/// 墙钟总超时（ms）：兜底。任何 fixture 都该在它之前跑完。
const WALL_CLOCK_MS: u64 = 15_000;

// ── 脚本契约 ──────────────────────────────────────────────────────────

/// 一次 `/chat/status` 的响应。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PollStep {
    /// 正常响应：原样作为 `data`（必须含 `status`）。
    pub status: Option<String>,
    /// 模拟 HTTP 失败（非 2xx）；给了它就忽略 `status`。
    pub http_status: Option<u16>,
    /// 失败时的响应体（`{code, error}` 信封）。
    pub error_body: Option<Value>,
    /// 附加字段（`items` / `failures` 等）。
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// 一次提交尝试的结果。`error` 缺省 = 成功。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SendStep {
    pub error: Option<String>,
    /// 非 2xx（走 `build_upstream_error` 那条路，会解出 `biz_code`）。
    pub http_status: Option<u16>,
    /// 2xx 但信封 `code != 0`（走 `parse_envelope` 那条路）。
    pub biz_code: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamScript {
    pub project_id: Option<String>,
    /// threadId 前缀；实际 id 是 `${prefix}-1` / `-2` / …（**每个任务不同**）。
    pub thread_id_prefix: Option<String>,
    /// 提交步骤，按**尝试次数**消费；用完后**重复最后一项**。缺省一直成功。
    pub send: Option<Vec<SendStep>>,
    /// 轮询响应序列。**每个 thread 各自从头走一遍**，用完后重复最后一项。
    pub polls: Vec<PollStep>,
    pub result: Option<Value>,
    /// URL → `'fail'` 表示下载失败；缺省成功。
    pub downloads: Option<BTreeMap<String, String>>,
    /// 对同一份 state **连跑几轮**（缺省 1）。
    pub runs: Option<usize>,
    pub concurrency: Option<usize>,
    pub max_attempts: Option<u32>,
    pub max_busy_retries: Option<u32>,
    pub max_timeouts: Option<u32>,
    /// 单任务轮询总超时（ms）；缺省 60_000。**超时用例要调小**。
    pub task_timeout_ms: Option<u64>,
}

// ── 传输层回放 ────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct ReplayStats {
    pub poll: usize,
    pub send: usize,
    pub download: usize,
}

pub struct ReplayTransport {
    script: UpstreamScript,
    calls: Mutex<Vec<String>>,
    stats: Mutex<ReplayStats>,
    thread_seq: Mutex<u32>,
    send_attempt: Mutex<usize>,
    /// **按 thread 独立**的轮询索引 —— 全局索引会让多任务互相交错，黄金就不可复现了。
    poll_index: Mutex<BTreeMap<String, usize>>,
}

fn json_response(body: &Value, status: u16) -> HttpResponse {
    HttpResponse {
        status,
        body: body.to_string(),
    }
}

fn ok_data(data: Value) -> HttpResponse {
    json_response(&json!({ "code": 0, "data": data }), 200)
}

impl ReplayTransport {
    pub fn new(script: UpstreamScript) -> Self {
        Self {
            script,
            calls: Mutex::new(Vec::new()),
            stats: Mutex::new(ReplayStats::default()),
            thread_seq: Mutex::new(0),
            send_attempt: Mutex::new(0),
            poll_index: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn poll_count(&self) -> usize {
        self.stats.lock().unwrap().poll
    }

    pub fn download_count(&self) -> usize {
        self.stats.lock().unwrap().download
    }
}

impl Transport for ReplayTransport {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, String> {
        let url = reqwest::Url::parse(&request.url).map_err(|error| error.to_string())?;
        let path = url
            .path()
            .strip_prefix("/v1/openapi")
            .unwrap_or(url.path())
            .to_string();
        self.calls.lock().unwrap().push(path.clone());

        match path.as_str() {
            "/project/save" => {
                let project_id = self
                    .script
                    .project_id
                    .clone()
                    .unwrap_or_else(|| "replay-project".to_string());
                Ok(ok_data(json!({ "project_id": project_id })))
            }
            "/mode/set" | "/mode/query" => Ok(ok_data(json!({}))),
            "/chat" => {
                self.stats.lock().unwrap().send += 1;
                let steps = self
                    .script
                    .send
                    .clone()
                    .unwrap_or_else(|| vec![SendStep::default()]);
                let step = {
                    let mut attempt = self.send_attempt.lock().unwrap();
                    let index = (*attempt).min(steps.len() - 1);
                    *attempt += 1;
                    steps[index].clone()
                };

                if let Some(message) = step.error {
                    let code = step.biz_code.unwrap_or(1);
                    let body = json!({ "code": code, "error": message });
                    // httpStatus 给了 ⇒ 走非 2xx；否则走 2xx + 信封 code
                    return Ok(match step.http_status {
                        Some(status) => json_response(&body, status),
                        None => json_response(&body, 200),
                    });
                }

                let mut seq = self.thread_seq.lock().unwrap();
                *seq += 1;
                let prefix = self
                    .script
                    .thread_id_prefix
                    .clone()
                    .unwrap_or_else(|| "t".to_string());
                Ok(ok_data(json!({ "thread_id": format!("{prefix}-{seq}") })))
            }
            "/chat/status" => {
                let mut stats = self.stats.lock().unwrap();
                stats.poll += 1;
                // 纪律②：预算超了就抛（非瞬态）⇒ 任务判失败、循环结束，测试**失败**而不是挂住
                if stats.poll > POLL_BUDGET {
                    return Err(format!(
                        "回放：轮询超过 {POLL_BUDGET} 次 —— 调度器未收敛（疑似无限循环）"
                    ));
                }
                drop(stats);

                let thread_id = url
                    .query_pairs()
                    .find(|(key, _)| key == "thread_id")
                    .map(|(_, value)| value.to_string())
                    .unwrap_or_default();
                let index = {
                    let mut map = self.poll_index.lock().unwrap();
                    let current = *map.get(&thread_id).unwrap_or(&0);
                    map.insert(thread_id.clone(), current + 1);
                    current
                };
                let step = self.script.polls[index.min(self.script.polls.len() - 1)].clone();

                if let Some(status) = step.http_status {
                    let body = step
                        .error_body
                        .clone()
                        .unwrap_or_else(|| json!({ "code": 1, "error": "replay http error" }));
                    return Ok(json_response(&body, status));
                }
                let mut data = step.extra.clone();
                if let Some(status) = step.status {
                    data.insert("status".to_string(), Value::String(status));
                }
                Ok(ok_data(Value::Object(data)))
            }
            "/chat/result" => {
                let fallback = self.script.polls.last().cloned().unwrap_or_default();
                let mut data = fallback.extra;
                if let Some(status) = fallback.status {
                    data.insert("status".to_string(), Value::String(status));
                }
                let result = self.script.result.clone().unwrap_or(Value::Object(data));
                Ok(ok_data(result))
            }
            // `/chat/confirm` 等：成功但不做事
            _ => Ok(ok_data(json!({}))),
        }
    }
}

// ── 端口层只替换下载（见文件头「唯一例外」）────────────────────────────

pub struct ReplayPorts {
    inner: RealPorts,
    downloads: BTreeMap<String, String>,
    counter: AtomicUsize,
}

impl ReplayPorts {
    pub fn new(script: &UpstreamScript) -> Self {
        Self {
            inner: RealPorts::new(),
            downloads: script.downloads.clone().unwrap_or_default(),
            counter: AtomicUsize::new(0),
        }
    }
}

impl Ports for ReplayPorts {
    fn set_mode(&self, deps: &ClientDeps, unlimited: bool) -> Result<(), LovartError> {
        self.inner.set_mode(deps, unlimited)
    }

    fn send(&self, deps: &ClientDeps, input: &lovart_client::lovart::contract::SendInput) -> Result<(String, String), LovartError> {
        self.inner.send(deps, input)
    }

    fn poll(
        &self,
        deps: &ClientDeps,
        thread_id: &str,
        task_timeout: Duration,
    ) -> Result<Value, LovartError> {
        self.inner.poll(deps, thread_id, task_timeout)
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
        self.counter.fetch_add(1, Ordering::SeqCst);
        if self.downloads.get(url).map(String::as_str) == Some("fail") {
            return Err(LovartError::transport("回放：模拟下载失败"));
        }
        Ok(DownloadedFile {
            path: PathBuf::from(format!("{}/{file_base}.png", output_dir.display())),
            bytes: 1,
            reused: false,
        })
    }
}

// ── 跑一个 fixture ────────────────────────────────────────────────────

pub struct FixtureRun {
    pub normalized: NormalizedState,
    pub state_text: String,
    pub calls: Vec<String>,
    pub downloads: usize,
    pub summary: SchedulerSummary,
}

static SEQ: AtomicUsize = AtomicUsize::new(0);

/// 回放一个 fixture：读 `prompts.jsonl` + `upstream.json`，跑**真**调度器，返回产出的状态。
pub fn run_fixture(dir: &Path) -> FixtureRun {
    let prompts_text = std::fs::read_to_string(dir.join("prompts.jsonl")).expect("读 prompts.jsonl");
    let script: UpstreamScript = serde_json::from_str(
        &std::fs::read_to_string(dir.join("upstream.json")).expect("读 upstream.json"),
    )
    .expect("解析 upstream.json");

    let parsed = parse_prompts(&prompts_text, &defaults(), &ParseOptions::default());
    assert!(
        parsed.errors.is_empty(),
        "fixture 的 prompts.jsonl 不合法：{:?}",
        parsed.errors
    );
    let tasks: Vec<PromptTask> = parsed.tasks;

    let transport = Arc::new(ReplayTransport::new(script.clone()));
    let transport_dyn: Arc<dyn Transport> = transport.clone();

    let mut deps = ClientDeps::new(
        "http://replay.invalid",
        "replay-ak",
        "replay-sk",
        transport_dyn,
    );
    // 回放不真等：轮询间隔与 done 复核都压到 1ms
    deps.poll_interval = Duration::from_millis(1);
    deps.done_recheck = Duration::from_millis(1);
    deps.request_timeout = Duration::from_secs(5);
    deps.task_timeout = Duration::from_millis(script.task_timeout_ms.unwrap_or(60_000));

    let cancelled = Arc::clone(&deps.cancelled);

    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let temp = std::env::temp_dir().join(format!("lv-fixture-{}-{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).expect("建临时目录");
    let state_file = temp.join("state.jsonl");

    // 可选预置状态：用于「参数变更重跑」这类需要**先有一条已存在的记录**的场景。
    let seed = dir.join("initial-state.jsonl");
    if seed.is_file() {
        std::fs::copy(&seed, &state_file).expect("复制 initial-state.jsonl");
    }

    let ports: Arc<dyn Ports> = Arc::new(ReplayPorts::new(&script));
    let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let log_sink = Arc::clone(&logs);
    let log: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |message: &str| {
        log_sink.lock().unwrap().push(message.to_string());
    });

    // 纪律②：墙钟看门狗 —— 超时就置取消标志，让调度器收敛（测试**失败**而不是挂住）
    let watchdog_flag = Arc::clone(&cancelled);
    let watchdog = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(WALL_CLOCK_MS));
        watchdog_flag.store(true, Ordering::SeqCst);
    });

    let output_dir = temp.join("out");
    let mut summary: Option<SchedulerSummary> = None;

    for _round in 1..=(script.runs.unwrap_or(1)) {
        let state = StateStore::open(&state_file).expect("打开 state");
        summary = Some(
            run_scheduler(SchedulerOptions {
                deps: deps.clone(),
                batch: Batch::Declared(tasks.clone()),
                state,
                output_dir: output_dir.clone(),
                // 缺省 1：顺序执行 ⇒ 完全可复现。要测并发才调大。
                concurrency: script.concurrency.unwrap_or(1),
                task_timeout: script.task_timeout_ms.map(Duration::from_millis),
                recheck_timeout: Some(Duration::from_millis(1)),
                max_timeouts: script.max_timeouts,
                download_retries: Some(1),
                max_attempts: script.max_attempts,
                max_busy_retries: script.max_busy_retries,
                busy_backoff_base: Some(Duration::from_millis(1)),
                log: Arc::clone(&log),
                ports: Some(Arc::clone(&ports)),
            })
            .expect("调度器不应报错"),
        );
    }

    // 看门狗还没醒就不用等它
    drop(watchdog);

    let state_text = std::fs::read_to_string(&state_file).expect("读 state");
    let normalized = normalize_state_text(&state_text);
    let calls = transport.calls();
    let downloads = transport.download_count();
    let _ = std::fs::remove_dir_all(&temp);

    FixtureRun {
        normalized,
        state_text,
        calls,
        downloads,
        summary: summary.expect("至少跑一轮"),
    }
}

/// 统计 `/chat` 调用次数（= 提交过几次）。
pub fn send_count(run: &FixtureRun) -> usize {
    run.calls.iter().filter(|path| *path == "/chat").count()
}

/// 统计 `/chat/confirm` 调用次数（**必须恒为 0** —— 那是要扣积分的操作）。
pub fn confirm_count(run: &FixtureRun) -> usize {
    run.calls.iter().filter(|path| *path == "/chat/confirm").count()
}
