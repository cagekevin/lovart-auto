//! 运行时设置 —— **唯一读取点**。
//!
//! 为什么单独立文件：`gui.rs` 与 `run.rs` 是两个入口，env 读取若各写一份，
//! 就会出现「GUI 卡 10 分钟、CLI 没上限」这种已经发生过的漂移
//! （见 `docs/plan/07` 的 R1-1）。两边都只准调这里。
//!
//! ── 三个入口，一份解析 ──────────────────────────────────────────────
//! | 入口 | 用途 | 读什么 |
//! |---|---|---|
//! | [`Settings::from_file`] | 设置页 / `save_settings` 的「当前值」 | `.env` 文本 |
//! | [`Settings::from_env`]  | 跑批 / CLI / `to_client_deps` | 进程环境（`load_dot_env` 之后） |
//! | [`Settings::from_pairs`]| 上面两个的唯一实现 | 键值对 |
//!
//! 设置页读文件而不是读环境，是因为「手改 `.env` → 打开设置 → 点保存」
//! 会把环境里的旧值写回去，**覆盖手改**（`docs/plan/07` N7）。
//!
//! ── 容错规则 ────────────────────────────────────────────────────────
//! - 缺键 ⇒ 内置默认，**不告警**（没配是正常状态）；
//! - 值非法 / 越界 ⇒ 内置默认 + **一条告警**（进设置页 / 顶栏，不静默）；
//! - 同名多行 ⇒ **后者覆盖前者**，与 `inject_dot_env` 的 `set_var` 语义一致。
//!
//! ⚠️ 不缓存：每次调用都重新读。缓存一旦存在，`save_settings` 后就必须记得失效，
//! 而「保存了但忘了失效」正是 TD-05-11 的成因。

use std::path::Path;
use std::time::Duration;

use crate::lovart::connection::parse_dot_env;
use crate::lovart::prompt::{
    PromptDefaults, DEFAULT_MODEL_KEY, DEFAULT_RATIO_KEY, DEFAULT_SIZE_KEY,
};

// ── 键名（唯一常量；字符串字面量不许散落在调用方）──────────────────────

pub const ACCESS_KEY: &str = "LOVART_ACCESS_KEY";
pub const SECRET_KEY: &str = "LOVART_SECRET_KEY";
pub const CONCURRENCY_KEY: &str = "LOVART_CONCURRENCY";
pub const OUTPUT_DIR_KEY: &str = "LOVART_OUTPUT_DIR";
pub const TASK_INTERVAL_KEY: &str = "LOVART_TASK_INTERVAL_MS";
pub const TASK_TIMEOUT_KEY: &str = "LOVART_TASK_TIMEOUT_MS";
pub const RECHECK_TIMEOUT_KEY: &str = "LOVART_RECHECK_TIMEOUT_MS";
pub const MAX_ATTEMPTS_KEY: &str = "LOVART_MAX_ATTEMPTS";
pub const MAX_TIMEOUTS_KEY: &str = "LOVART_MAX_TIMEOUTS";
pub const MAX_BUSY_RETRIES_KEY: &str = "LOVART_MAX_BUSY_RETRIES";
pub const DOWNLOAD_RETRIES_KEY: &str = "LOVART_DOWNLOAD_RETRIES";
pub const POLL_INTERVAL_KEY: &str = "LOVART_POLL_INTERVAL_MS";
pub const POLL_MAX_INTERVAL_KEY: &str = "LOVART_POLL_MAX_INTERVAL_MS";

/// [`Settings::from_env`] 要遍历的全部键。
pub const SETTING_KEYS: &[&str] = &[
    ACCESS_KEY,
    SECRET_KEY,
    CONCURRENCY_KEY,
    OUTPUT_DIR_KEY,
    TASK_INTERVAL_KEY,
    TASK_TIMEOUT_KEY,
    RECHECK_TIMEOUT_KEY,
    MAX_ATTEMPTS_KEY,
    MAX_TIMEOUTS_KEY,
    MAX_BUSY_RETRIES_KEY,
    DOWNLOAD_RETRIES_KEY,
    POLL_INTERVAL_KEY,
    POLL_MAX_INTERVAL_KEY,
    DEFAULT_MODEL_KEY,
    DEFAULT_RATIO_KEY,
    DEFAULT_SIZE_KEY,
];

// ── 默认值（唯一；生产代码与测试都从这里取）───────────────────────────

/// 并发默认 7，硬上限 8（服务端最多同时 8 个任务，配大了只会换来 402）。
pub const DEFAULT_CONCURRENCY: usize = 7;
pub const MAX_CONCURRENCY: usize = 8;

pub const DEFAULT_OUTPUT_DIR: &str = "output";

/// 提交间隔默认 5s（1s 对上游偏密，429 明显变多）；上限 10 分钟。
pub const DEFAULT_TASK_INTERVAL_MS: u64 = 5_000;
pub const MAX_TASK_INTERVAL_MS: u64 = 600_000;

/// 任务总超时 90 分；复查超时 30 分。
pub const DEFAULT_TASK_TIMEOUT_MS: u64 = 90 * 60 * 1000;
pub const DEFAULT_RECHECK_TIMEOUT_MS: u64 = 30 * 60 * 1000;

/// 时间类设置的公共区间：1 分钟 ~ 24 小时。
///
/// 下限 1 分钟：轮询起始间隔就是 10s，再小没有意义；
/// 上限 24 小时：再大只会把「任务卡死」掩盖成「还在跑」。
pub const MIN_TIMEOUT_MS: u64 = 60_000;
pub const MAX_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;

pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;
pub const MAX_MAX_ATTEMPTS: u32 = 20;

pub const DEFAULT_MAX_TIMEOUTS: u32 = 3;
pub const MAX_MAX_TIMEOUTS: u32 = 10;

pub const DEFAULT_MAX_BUSY_RETRIES: u32 = 300;
pub const MAX_MAX_BUSY_RETRIES: u32 = 5_000;

pub const DEFAULT_DOWNLOAD_RETRIES: u32 = 3;
pub const MAX_DOWNLOAD_RETRIES: u32 = 10;

/// 轮询起始 10s；退避封顶 60s。
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 10_000;
pub const MIN_POLL_INTERVAL_MS: u64 = 1_000;
pub const MAX_POLL_INTERVAL_MS: u64 = 600_000;

pub const DEFAULT_POLL_MAX_INTERVAL_MS: u64 = 60_000;
pub const MIN_POLL_MAX_INTERVAL_MS: u64 = 1_000;
pub const MAX_POLL_MAX_INTERVAL_MS: u64 = 3_600_000;

// ── 结构 ─────────────────────────────────────────────────────────────

/// 一份「已解析、已校验」的设置快照。
///
/// 所有字段都是**生效值**：非法输入已经被兜底，`warnings` 里说明了为什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub access_key: String,
    pub secret_key: String,
    pub concurrency: usize,
    pub output_dir: String,
    pub task_interval_ms: u64,
    pub task_timeout_ms: u64,
    pub recheck_timeout_ms: u64,
    pub max_attempts: u32,
    pub max_timeouts: u32,
    pub max_busy_retries: u32,
    pub download_retries: u32,
    pub poll_interval_ms: u64,
    pub poll_max_interval_ms: u64,
    pub default_model: String,
    pub default_ratio: String,
    pub default_size: String,
    /// 解析时发现非法的键（值非法或越界）。
    ///
    /// 只给 `save_settings` 用：这些键**无条件重写为默认值**，
    /// 不受「值没变就不写」的约束（否则 `.env` 里的手误永远修不掉，每次打开都告警）。
    pub invalid_keys: Vec<&'static str>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            access_key: String::new(),
            secret_key: String::new(),
            concurrency: DEFAULT_CONCURRENCY,
            output_dir: DEFAULT_OUTPUT_DIR.to_string(),
            task_interval_ms: DEFAULT_TASK_INTERVAL_MS,
            task_timeout_ms: DEFAULT_TASK_TIMEOUT_MS,
            recheck_timeout_ms: DEFAULT_RECHECK_TIMEOUT_MS,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            max_timeouts: DEFAULT_MAX_TIMEOUTS,
            max_busy_retries: DEFAULT_MAX_BUSY_RETRIES,
            download_retries: DEFAULT_DOWNLOAD_RETRIES,
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
            poll_max_interval_ms: DEFAULT_POLL_MAX_INTERVAL_MS,
            default_model: PromptDefaults::default().model,
            default_ratio: PromptDefaults::default().aspect_ratio,
            default_size: PromptDefaults::default().image_size,
            invalid_keys: Vec::new(),
        }
    }
}

impl Settings {
    /// **纯函数**：键值对 → 生效值 + 告警。不碰 I/O、不碰环境变量。
    ///
    /// 同名键**后者覆盖前者**：与 `inject_dot_env` 逐个 `set_var` 的语义一致。
    /// （旧 `get_settings` 用 `find()` 取第一条，文件里出现两行同名键时
    /// 设置页显示的值和实际运行的值不同 —— 这是个真 bug。）
    pub fn from_pairs<'a, I>(pairs: I) -> (Self, Vec<String>)
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let map: std::collections::BTreeMap<&str, &str> = pairs.into_iter().collect();
        let get = |key: &str| map.get(key).copied();
        let mut warnings: Vec<String> = Vec::new();
        let mut invalid: Vec<&'static str> = Vec::new();

        let concurrency = number(
            get(CONCURRENCY_KEY),
            CONCURRENCY_KEY,
            1,
            MAX_CONCURRENCY as u64,
            DEFAULT_CONCURRENCY as u64,
            &mut warnings,
            &mut invalid,
        ) as usize;

        let task_interval_ms = number(
            get(TASK_INTERVAL_KEY),
            TASK_INTERVAL_KEY,
            0,
            MAX_TASK_INTERVAL_MS,
            DEFAULT_TASK_INTERVAL_MS,
            &mut warnings,
            &mut invalid,
        );

        let task_timeout_ms = number(
            get(TASK_TIMEOUT_KEY),
            TASK_TIMEOUT_KEY,
            MIN_TIMEOUT_MS,
            MAX_TIMEOUT_MS,
            DEFAULT_TASK_TIMEOUT_MS,
            &mut warnings,
            &mut invalid,
        );

        let recheck_timeout_ms = number(
            get(RECHECK_TIMEOUT_KEY),
            RECHECK_TIMEOUT_KEY,
            MIN_TIMEOUT_MS,
            MAX_TIMEOUT_MS,
            DEFAULT_RECHECK_TIMEOUT_MS,
            &mut warnings,
            &mut invalid,
        );

        let max_attempts = number(
            get(MAX_ATTEMPTS_KEY),
            MAX_ATTEMPTS_KEY,
            1,
            MAX_MAX_ATTEMPTS as u64,
            DEFAULT_MAX_ATTEMPTS as u64,
            &mut warnings,
            &mut invalid,
        ) as u32;

        let max_timeouts = number(
            get(MAX_TIMEOUTS_KEY),
            MAX_TIMEOUTS_KEY,
            1,
            MAX_MAX_TIMEOUTS as u64,
            DEFAULT_MAX_TIMEOUTS as u64,
            &mut warnings,
            &mut invalid,
        ) as u32;

        let max_busy_retries = number(
            get(MAX_BUSY_RETRIES_KEY),
            MAX_BUSY_RETRIES_KEY,
            1,
            MAX_MAX_BUSY_RETRIES as u64,
            DEFAULT_MAX_BUSY_RETRIES as u64,
            &mut warnings,
            &mut invalid,
        ) as u32;

        let download_retries = number(
            get(DOWNLOAD_RETRIES_KEY),
            DOWNLOAD_RETRIES_KEY,
            1,
            MAX_DOWNLOAD_RETRIES as u64,
            DEFAULT_DOWNLOAD_RETRIES as u64,
            &mut warnings,
            &mut invalid,
        ) as u32;

        let poll_interval_ms = number(
            get(POLL_INTERVAL_KEY),
            POLL_INTERVAL_KEY,
            MIN_POLL_INTERVAL_MS,
            MAX_POLL_INTERVAL_MS,
            DEFAULT_POLL_INTERVAL_MS,
            &mut warnings,
            &mut invalid,
        );

        let poll_max_interval_ms = {
            let parsed = number(
                get(POLL_MAX_INTERVAL_KEY),
                POLL_MAX_INTERVAL_KEY,
                MIN_POLL_MAX_INTERVAL_MS,
                MAX_POLL_MAX_INTERVAL_MS,
                DEFAULT_POLL_MAX_INTERVAL_MS,
                &mut warnings,
                &mut invalid,
            );
            // 倒挂 ⇒ 抬到起始间隔。运行时还有一层防御（`poll_thread`），
            // 这里负责「给用户一个能跑的值 + 说清楚为什么改了」。
            if parsed < poll_interval_ms {
                warnings.push(format!(
                    "{POLL_MAX_INTERVAL_KEY}（{parsed}ms）小于 {POLL_INTERVAL_KEY}（{poll_interval_ms}ms），\
                     已抬到 {poll_interval_ms}ms"
                ));
                // 也算「这一项是坏的」：保存时要无条件重写，否则文件里的倒挂值
                // 会原样留着，每次打开设置都告警一遍（N16）。
                invalid.push(POLL_MAX_INTERVAL_KEY);
                poll_interval_ms
            } else {
                parsed
            }
        };

        let output_dir = get(OUTPUT_DIR_KEY)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_OUTPUT_DIR)
            .to_string();

        // 兜底参数走 `PromptDefaults::resolve`（唯一判定点），告警合并进同一个数组。
        let (prompt, prompt_warnings) = PromptDefaults::resolve(
            get(DEFAULT_MODEL_KEY),
            get(DEFAULT_RATIO_KEY),
            get(DEFAULT_SIZE_KEY),
        );
        warnings.extend(prompt_warnings);

        (
            Self {
                access_key: get(ACCESS_KEY).map(str::trim).unwrap_or_default().to_string(),
                secret_key: get(SECRET_KEY).map(str::trim).unwrap_or_default().to_string(),
                concurrency,
                output_dir,
                task_interval_ms,
                task_timeout_ms,
                recheck_timeout_ms,
                max_attempts,
                max_timeouts,
                max_busy_retries,
                download_retries,
                poll_interval_ms,
                poll_max_interval_ms,
                default_model: prompt.model,
                default_ratio: prompt.aspect_ratio,
                default_size: prompt.image_size,
                invalid_keys: invalid,
            },
            warnings,
        )
    }

    /// 读 `.env` 文本。**文件不存在**是正常状况（还没配过）⇒ 全默认；
    /// **读不出来**是异常 ⇒ `Err`（设置页据此不打开面板，避免显示假配置）。
    pub fn from_file(path: &Path) -> Result<(Self, Vec<String>), String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(format!("读不了 {}：{error}", path.display())),
        };
        let parsed = parse_dot_env(&text);
        let (settings, mut warnings) = Self::from_pairs(
            parsed
                .entries
                .iter()
                .map(|entry| (entry.key.as_str(), entry.value.as_str())),
        );
        // 格式错的行无法归到某个键（连 `=` 都没有），只能整行报告；
        // 保存时**不做**自动修复 —— 不知道它是哪个键，猜就是改错东西。
        let mut all = parsed.errors;
        all.append(&mut warnings);
        Ok((settings, all))
    }

    /// 读进程环境（`.env` 由 `load_dot_env` / `reload_dot_env` 载入）。
    ///
    /// 与 [`Self::from_file`] 共用 `from_pairs` —— 不允许出现第二份解析。
    pub fn from_env() -> (Self, Vec<String>) {
        let owned: Vec<(String, String)> = SETTING_KEYS
            .iter()
            .filter_map(|key| {
                std::env::var(key)
                    .ok()
                    .map(|value| ((*key).to_string(), value))
            })
            .collect();
        Self::from_pairs(
            owned
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
    }

    // ── 派生 ──────────────────────────────────────────────────────────

    pub fn task_timeout(&self) -> Duration {
        Duration::from_millis(self.task_timeout_ms)
    }

    pub fn recheck_timeout(&self) -> Duration {
        Duration::from_millis(self.recheck_timeout_ms)
    }

    pub fn task_interval(&self) -> Duration {
        Duration::from_millis(self.task_interval_ms)
    }

    pub fn poll_interval(&self) -> Duration {
        Duration::from_millis(self.poll_interval_ms)
    }

    pub fn poll_max_interval(&self) -> Duration {
        Duration::from_millis(self.poll_max_interval_ms)
    }

    /// **单轮**墙钟上限（分钟）：`task + recheck × max_timeouts`。
    ///
    /// 只对「新提交的任务」成立 —— 「接着查」的任务不经过 `task_timeout`，
    /// 单轮最多 `recheck_timeout`（`docs/plan/07` N14）。文案必须写「单轮」。
    ///
    /// 公式对照 `scheduler.rs` 的超时计数：初次 1 次 + 复查 `max_timeouts-1` 次
    /// + 收尾 1 次 = 复查共 `max_timeouts` 次。
    pub fn max_wall_clock_min(&self) -> u64 {
        ms_to_minutes(self.task_timeout_ms)
            + ms_to_minutes(self.recheck_timeout_ms) * u64::from(self.max_timeouts)
    }
}

/// 毫秒 → 分钟（四舍五入）。
///
/// 四舍五入而不是向下取整：`.env` 里手写 `90000`（1.5 分）时，
/// 向下取整会让设置页显示 1，用户一动别的字段就可能把它写成 60000。
pub fn ms_to_minutes(ms: u64) -> u64 {
    (ms + 30_000) / 60_000
}

/// 分钟 → 毫秒。
pub fn minutes_to_ms(minutes: u64) -> u64 {
    minutes.saturating_mul(60_000)
}

/// 秒（可带一位小数）→ 毫秒。负数按 0 处理。
pub fn seconds_to_ms(seconds: f64) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }
    (seconds * 1000.0).round() as u64
}

/// 毫秒 → 秒（保留一位小数）。
pub fn ms_to_seconds(ms: u64) -> f64 {
    (ms as f64 / 100.0).round() / 10.0
}

/// 设置页保存：把 payload 翻译成 `.env` 的写入项（**纯函数**，便于测）。
///
/// 四条规则（`docs/plan/07`）：
/// 1. **缺字段 ⇒ 不改**（旧版前端没渲染这一项）；
/// 2. **`null` / 空串 ⇒ `Err`**（用户清空了，不替他补默认 —— N11）；
/// 3. **与当前生效值相同 ⇒ 不写**（N8：整包提交不许改写没碰过的字段）；
/// 4. **当前值非法的键无条件重写**（N16：否则 `.env` 里的手误永远修不掉，
///    每次打开设置都告警一遍）。
///
/// 校验顺序是「全部校验完再返回」，任何一个失败 ⇒ 整个 `Err`，
/// 调用方据此**一个字节都不写**（不做部分保存）。
pub fn save_updates(
    payload: &serde_json::Value,
    current: &Settings,
) -> Result<Vec<(&'static str, String)>, String> {
    let mut updates: Vec<(&'static str, String)> = Vec::new();

    // ── 并发 ──
    if let Some(value) = take_u64(payload, "concurrency", "并发数", 1, MAX_CONCURRENCY as u64)? {
        let value = value as usize;
        if needs_write(current, CONCURRENCY_KEY, value, current.concurrency) {
            updates.push((CONCURRENCY_KEY, value.to_string()));
        }
    }

    // ── 提交间隔（秒，可带一位小数）──
    match payload.get("taskIntervalSec") {
        None => {}
        Some(serde_json::Value::Null) => return Err("提交间隔不能为空".to_string()),
        Some(value) => {
            let seconds = value
                .as_f64()
                .ok_or_else(|| "提交间隔必须是数字（秒，可带一位小数）".to_string())?;
            if !(0.0..=600.0).contains(&seconds) {
                return Err("提交间隔必须在 0~600 秒之间（0 = 不间隔）".to_string());
            }
            let ms = seconds_to_ms(seconds);
            if needs_write(current, TASK_INTERVAL_KEY, ms, current.task_interval_ms) {
                updates.push((TASK_INTERVAL_KEY, ms.to_string()));
            }
        }
    }

    // ── 任务超时（分钟）──
    if let Some(minutes) = take_u64(payload, "taskTimeoutMin", "单任务超时", 1, 1440)? {
        let ms = minutes_to_ms(minutes);
        // 比较**显示值**而不是 ms：`.env` 里的 90000（1.5 分）显示成 2，
        // 用户没动它时不能被写成 120000。
        let unchanged = !current.invalid_keys.contains(&TASK_TIMEOUT_KEY)
            && ms_to_minutes(current.task_timeout_ms) == minutes;
        if !unchanged {
            updates.push((TASK_TIMEOUT_KEY, ms.to_string()));
        }
    }

    // ── 复查超时（分钟）──
    if let Some(minutes) = take_u64(payload, "recheckTimeoutMin", "复查超时", 1, 1440)? {
        let ms = minutes_to_ms(minutes);
        let unchanged = !current.invalid_keys.contains(&RECHECK_TIMEOUT_KEY)
            && ms_to_minutes(current.recheck_timeout_ms) == minutes;
        if !unchanged {
            updates.push((RECHECK_TIMEOUT_KEY, ms.to_string()));
        }
    }

    // ── 四个次数 ──
    if let Some(value) = take_u64(payload, "maxAttempts", "失败重试上限", 1, MAX_MAX_ATTEMPTS as u64)?
    {
        if needs_write(current, MAX_ATTEMPTS_KEY, value as u32, current.max_attempts) {
            updates.push((MAX_ATTEMPTS_KEY, value.to_string()));
        }
    }
    if let Some(value) = take_u64(payload, "maxTimeouts", "超时次数上限", 1, MAX_MAX_TIMEOUTS as u64)?
    {
        if needs_write(current, MAX_TIMEOUTS_KEY, value as u32, current.max_timeouts) {
            updates.push((MAX_TIMEOUTS_KEY, value.to_string()));
        }
    }
    if let Some(value) = take_u64(
        payload,
        "maxBusyRetries",
        "402/409 重投上限",
        1,
        MAX_MAX_BUSY_RETRIES as u64,
    )? {
        if needs_write(
            current,
            MAX_BUSY_RETRIES_KEY,
            value as u32,
            current.max_busy_retries,
        ) {
            updates.push((MAX_BUSY_RETRIES_KEY, value.to_string()));
        }
    }
    if let Some(value) = take_u64(
        payload,
        "downloadRetries",
        "下载重试次数",
        1,
        MAX_DOWNLOAD_RETRIES as u64,
    )? {
        if needs_write(
            current,
            DOWNLOAD_RETRIES_KEY,
            value as u32,
            current.download_retries,
        ) {
            updates.push((DOWNLOAD_RETRIES_KEY, value.to_string()));
        }
    }

    // ── 轮询间隔（秒）与封顶 ──
    let mut effective_poll_interval = current.poll_interval_ms;
    if let Some(seconds) = take_u64(payload, "pollIntervalSec", "轮询间隔", 1, 600)? {
        effective_poll_interval = seconds * 1000;
        if needs_write(
            current,
            POLL_INTERVAL_KEY,
            effective_poll_interval,
            current.poll_interval_ms,
        ) {
            updates.push((POLL_INTERVAL_KEY, effective_poll_interval.to_string()));
        }
    }
    if let Some(seconds) = take_u64(payload, "pollMaxIntervalSec", "轮询封顶", 1, 3_600)? {
        let ms = seconds * 1000;
        // UI 入口的倒挂**直接拒绝**；只有手改 `.env` 才「抬平 + 告警」。
        // 两条入口容错级别不同是刻意的：UI 背后有人可以立刻改。
        if ms < effective_poll_interval {
            return Err("轮询封顶不能小于轮询间隔".to_string());
        }
        if needs_write(current, POLL_MAX_INTERVAL_KEY, ms, current.poll_max_interval_ms) {
            updates.push((POLL_MAX_INTERVAL_KEY, ms.to_string()));
        }
    }

    // ── 输出文件夹（不允许空）──
    if let Some(value) = take_string(payload, "outputDir", "输出文件夹", false)? {
        if value != current.output_dir {
            updates.push((OUTPUT_DIR_KEY, value));
        }
    }

    // ── 凭据（允许清空）──
    if let Some(value) = take_string(payload, "accessKey", "访问密钥", true)? {
        if value != current.access_key {
            updates.push((ACCESS_KEY, value));
        }
    }
    if let Some(value) = take_string(payload, "secretKey", "签名密钥", true)? {
        if value != current.secret_key {
            updates.push((SECRET_KEY, value));
        }
    }

    // ── 兜底参数（三个一起校验；缺的那个用当前值）──
    if ["defaultModel", "defaultRatio", "defaultSize"]
        .iter()
        .any(|key| payload.get(*key).is_some())
    {
        let text_of = |key: &str, fallback: &str| {
            match payload.get(key) {
                None => fallback.to_string(),
                Some(serde_json::Value::Null) => String::new(),
                Some(value) => value.as_str().unwrap_or_default().trim().to_string(),
            }
        };
        let validated = PromptDefaults::validated(
            &text_of("defaultModel", &current.default_model),
            &text_of("defaultRatio", &current.default_ratio),
            &text_of("defaultSize", &current.default_size),
        )?;
        // 这三个**整组写回**，不做差异写：前端每次都提交完整的三元组，
        // 写回是幂等的；而 `.env` 里手写的非法值（如 `3x4`）在 `from_pairs`
        // 里已被兜成默认、又无法结构化地标进 `invalid_keys`（判定在
        // `PromptDefaults` 内部）—— 整组写回是唯一能顺手修掉它的地方。
        updates.push((DEFAULT_MODEL_KEY, validated.model));
        updates.push((DEFAULT_RATIO_KEY, validated.aspect_ratio));
        updates.push((DEFAULT_SIZE_KEY, validated.image_size));
    }

    Ok(updates)
}

/// 该项要不要写：当前值非法 ⇒ 必须写；否则值变了才写。
fn needs_write<T: PartialEq>(
    current: &Settings,
    key: &'static str,
    payload_value: T,
    current_value: T,
) -> bool {
    current.invalid_keys.contains(&key) || payload_value != current_value
}

fn take_u64(
    payload: &serde_json::Value,
    key: &str,
    label: &str,
    min: u64,
    max: u64,
) -> Result<Option<u64>, String> {
    match payload.get(key) {
        None => Ok(None),
        Some(serde_json::Value::Null) => Err(format!("{label}不能为空")),
        Some(value) => {
            let parsed = value
                .as_u64()
                .ok_or_else(|| format!("{label}必须是整数"))?;
            if !(min..=max).contains(&parsed) {
                return Err(format!("{label}必须在 {min}~{max} 之间"));
            }
            Ok(Some(parsed))
        }
    }
}

fn take_string(
    payload: &serde_json::Value,
    key: &str,
    label: &str,
    allow_empty: bool,
) -> Result<Option<String>, String> {
    match payload.get(key) {
        None => Ok(None),
        Some(serde_json::Value::Null) => {
            if allow_empty {
                Ok(Some(String::new()))
            } else {
                Err(format!("{label}不能为空"))
            }
        }
        Some(value) => {
            let text = value
                .as_str()
                .ok_or_else(|| format!("{label}必须是文本"))?;
            let trimmed = text.trim();
            if trimmed.is_empty() && !allow_empty {
                return Err(format!("{label}不能为空"));
            }
            Ok(Some(trimmed.to_string()))
        }
    }
}

/// 解析一个整数并夹在 `min..=max` 内；缺键用默认、非法/越界用默认 + 告警。
fn number(
    raw: Option<&str>,
    key: &'static str,
    min: u64,
    max: u64,
    default: u64,
    warnings: &mut Vec<String>,
    invalid: &mut Vec<&'static str>,
) -> u64 {
    let Some(value) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return default;
    };
    match value.parse::<u64>() {
        Ok(parsed) if (min..=max).contains(&parsed) => parsed,
        Ok(parsed) => {
            warnings.push(format!(
                "{key} 的值 {parsed} 超出 {min}..={max}，已按默认值 {default} 处理"
            ));
            invalid.push(key);
            default
        }
        Err(_) => {
            // 手改 `.env` 最常见的错法：把分钟当毫秒写（`=90`），
            // 或者写成 `90m`。两种都落到这里，不会静默变成 90ms。
            warnings.push(format!(
                "{key} 的值「{value}」不是整数，已按默认值 {default} 处理"
            ));
            invalid.push(key);
            default
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pairs(items: &[(&str, &str)]) -> (Settings, Vec<String>) {
        Settings::from_pairs(items.iter().copied())
    }

    #[test]
    fn s1_空输入全部默认() {
        let (settings, warnings) = pairs(&[]);
        assert_eq!(settings, Settings::default());
        assert!(warnings.is_empty(), "缺键不该告警：{warnings:?}");
    }

    #[test]
    fn s2_合法值生效() {
        let (settings, warnings) = pairs(&[
            (CONCURRENCY_KEY, "3"),
            (TASK_TIMEOUT_KEY, "7200000"),
            (RECHECK_TIMEOUT_KEY, "600000"),
            (MAX_ATTEMPTS_KEY, "5"),
            (MAX_TIMEOUTS_KEY, "2"),
            (MAX_BUSY_RETRIES_KEY, "50"),
            (DOWNLOAD_RETRIES_KEY, "4"),
            (POLL_INTERVAL_KEY, "3000"),
            (POLL_MAX_INTERVAL_KEY, "30000"),
        ]);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(settings.concurrency, 3);
        assert_eq!(settings.task_timeout_ms, 7_200_000);
        assert_eq!(settings.recheck_timeout_ms, 600_000);
        assert_eq!(settings.max_attempts, 5);
        assert_eq!(settings.max_timeouts, 2);
        assert_eq!(settings.max_busy_retries, 50);
        assert_eq!(settings.download_retries, 4);
        assert_eq!(settings.poll_interval_ms, 3_000);
        assert_eq!(settings.poll_max_interval_ms, 30_000);
    }

    #[test]
    fn s3_非法与越界都回退并告警() {
        let (settings, warnings) = pairs(&[
            (TASK_TIMEOUT_KEY, "abc"),
            (RECHECK_TIMEOUT_KEY, "0"),
            (CONCURRENCY_KEY, "99"),
            (MAX_ATTEMPTS_KEY, "999"),
        ]);
        assert_eq!(settings.task_timeout_ms, DEFAULT_TASK_TIMEOUT_MS);
        assert_eq!(settings.recheck_timeout_ms, DEFAULT_RECHECK_TIMEOUT_MS);
        assert_eq!(settings.concurrency, DEFAULT_CONCURRENCY);
        assert_eq!(settings.max_attempts, DEFAULT_MAX_ATTEMPTS);
        assert_eq!(warnings.len(), 4, "每个非法键一条，不合并：{warnings:?}");
    }

    #[test]
    fn s4_手写分钟不会被当成毫秒() {
        // 把 90（分钟）写进 _MS 键 ⇒ 60_000 以下 ⇒ 回退 + 告警，绝不变成 90ms。
        let (settings, warnings) = pairs(&[(TASK_TIMEOUT_KEY, "90")]);
        assert_eq!(settings.task_timeout_ms, DEFAULT_TASK_TIMEOUT_MS);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("60000"), "{warnings:?}");
    }

    #[test]
    fn s5_同名多行后者覆盖() {
        let (settings, _) = pairs(&[(TASK_TIMEOUT_KEY, "600000"), (TASK_TIMEOUT_KEY, "7200000")]);
        assert_eq!(
            settings.task_timeout_ms, 7_200_000,
            "必须与 inject_dot_env 的 set_var 语义一致（最后一个赢）"
        );
    }

    #[test]
    fn s6_提交间隔_0_合法() {
        let (settings, warnings) = pairs(&[(TASK_INTERVAL_KEY, "0")]);
        assert_eq!(settings.task_interval_ms, 0, "0 = 关闭间隔，不能当非法");
        assert!(warnings.is_empty());
    }

    #[test]
    fn s7_轮询倒挂被抬到起始间隔() {
        let (settings, warnings) =
            pairs(&[(POLL_INTERVAL_KEY, "30000"), (POLL_MAX_INTERVAL_KEY, "5000")]);
        assert_eq!(settings.poll_max_interval_ms, 30_000);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("已抬到"), "{warnings:?}");
    }

    #[test]
    fn s8_单位换算互为逆且不出现浮点毛刺() {
        assert_eq!(ms_to_minutes(5_400_000), 90);
        assert_eq!(ms_to_minutes(90_000), 2); // 1.5 分四舍五入
        assert_eq!(minutes_to_ms(90), 5_400_000);
        assert_eq!(seconds_to_ms(1.5), 1_500);
        assert_eq!(seconds_to_ms(0.0), 0);
        assert_eq!(seconds_to_ms(-3.0), 0);
        assert_eq!(ms_to_seconds(1_500), 1.5);
    }

    #[test]
    fn s9_单轮上限按文档公式() {
        let settings = Settings::default();
        assert_eq!(
            settings.max_wall_clock_min(),
            90 + 30 * 3,
            "90 + 30×3 = 180（README 旧文案的 90+30+30 少算一次）"
        );
    }

    #[test]
    fn s10_输出目录空串回默认() {
        let (settings, warnings) = pairs(&[(OUTPUT_DIR_KEY, "   ")]);
        assert_eq!(settings.output_dir, DEFAULT_OUTPUT_DIR);
        assert!(warnings.is_empty(), "空 ≠ 非法，不该告警");
    }

    #[test]
    fn s11_兜底参数告警合并进同一数组() {
        let (settings, warnings) = pairs(&[(DEFAULT_RATIO_KEY, "3x4")]);
        assert_eq!(
            settings.default_ratio,
            PromptDefaults::default().aspect_ratio
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains(DEFAULT_RATIO_KEY));
    }

    #[test]
    fn s12_无关键不影响结果() {
        let (before, _) = pairs(&[]);
        let (after, _) = pairs(&[("LOVART_SOMETHING_ELSE", "x")]);
        assert_eq!(before, after);
    }

    #[test]
    fn s13_只写变化的字段() {
        let updates = save_updates(&json!({"concurrency": 3}), &Settings::default()).unwrap();
        assert_eq!(updates, vec![(CONCURRENCY_KEY, "3".to_string())]);
    }

    #[test]
    fn s14_未触碰的字段不进_updates() {
        // `.env` 里 90000ms = 1.5 分，UI 显示 2；用户只改了并发 ⇒ 不许把超时改成 2 分。
        let (current, _) = pairs(&[(TASK_TIMEOUT_KEY, "90000")]);
        let updates = save_updates(
            &json!({"taskTimeoutMin": 2, "concurrency": 7}),
            &current,
        )
        .unwrap();
        assert!(updates.is_empty(), "整包提交不许改写没碰过的字段：{updates:?}");
    }

    #[test]
    fn s15_空值拒绝而不是补默认() {
        assert!(save_updates(&json!({"concurrency": null}), &Settings::default()).is_err());
        assert!(save_updates(&json!({"outputDir": ""}), &Settings::default()).is_err());
    }

    #[test]
    fn s16_小数分钟被拒绝() {
        let error = save_updates(&json!({"taskTimeoutMin": 90.5}), &Settings::default())
            .expect_err("UI 只收整数分钟");
        assert!(error.contains("整数"), "{error}");
    }

    #[test]
    fn s17_间隔一秒半按毫秒写入() {
        let updates =
            save_updates(&json!({"taskIntervalSec": 1.5}), &Settings::default()).unwrap();
        assert_eq!(updates, vec![(TASK_INTERVAL_KEY, "1500".to_string())]);
    }

    #[test]
    fn s18_非法键被无条件修复() {
        let (current, warnings) = pairs(&[(TASK_TIMEOUT_KEY, "abc")]);
        assert!(!warnings.is_empty());
        assert!(current.invalid_keys.contains(&TASK_TIMEOUT_KEY));

        let updates = save_updates(&json!({"taskTimeoutMin": 90}), &current).unwrap();
        assert_eq!(
            updates,
            vec![(TASK_TIMEOUT_KEY, DEFAULT_TASK_TIMEOUT_MS.to_string())],
            "非法值必须在保存时被修掉，否则每次打开都告警"
        );
    }

    #[test]
    fn s19_缺字段不改() {
        let updates = save_updates(&json!({"maxAttempts": 5}), &Settings::default()).unwrap();
        assert_eq!(updates, vec![(MAX_ATTEMPTS_KEY, "5".to_string())]);
    }

    #[test]
    fn s20_轮询倒挂在_ui_层直接拒绝() {
        let error = save_updates(
            &json!({"pollIntervalSec": 60, "pollMaxIntervalSec": 30}),
            &Settings::default(),
        )
        .expect_err("UI 入口不许静默抬平");
        assert!(error.contains("不能小于"), "{error}");
    }

    #[test]
    fn s21_并发越界报错而不是夹紧() {
        assert!(save_updates(&json!({"concurrency": 99}), &Settings::default()).is_err());
    }

    #[test]
    fn s22_凭据允许清空() {
        let (current, _) = pairs(&[(ACCESS_KEY, "ak")]);
        let updates = save_updates(&json!({"accessKey": ""}), &current).unwrap();
        assert_eq!(updates, vec![(ACCESS_KEY, String::new())]);
    }

    #[test]
    fn s23_合法值未变时一个都不写() {
        let (current, _) = pairs(&[]);
        let updates = save_updates(
            &json!({
                "concurrency": DEFAULT_CONCURRENCY,
                "taskIntervalSec": 5,
                "taskTimeoutMin": 90,
                "recheckTimeoutMin": 30,
                "maxAttempts": 3,
                "maxTimeouts": 3,
                "maxBusyRetries": 300,
                "downloadRetries": 3,
                "pollIntervalSec": 10,
                "pollMaxIntervalSec": 60,
                "outputDir": DEFAULT_OUTPUT_DIR,
            }),
            &current,
        )
        .unwrap();
        assert!(updates.is_empty(), "{updates:?}");
    }
}
