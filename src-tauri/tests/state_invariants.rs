//! `engine::state` 的不变量回归 —— **逐条对齐 TS 的 `test/state.test.ts`**。
//!
//! 测的不是「函数返回什么」，而是**不变量** —— 即「什么情况绝不能发生」：
//!   ① 终态与占槽状态必须互斥（否则槽位永不释放，并发被拖垮）；
//!   ② 派生归类必须与状态清单一致（新增状态不可能漏归类）；
//!   ③ 落盘失败必须**诚实可见**，不能静默丢断点续跑数据（DEBT TD-06-01）；
//!   ④ 两个时间戳由状态机决定，调用方改不动。
//!
//! ⚠️ 另有两条**只有 Rust 侧才有**的断言（TS 做不到）：
//!   - 「黄金文件能完整反序列化」：直接守住 schema 契约（两边写同一个文件）；
//!   - 「读取失败只吞 NotFound」：TS 是 `catch {}` 吞掉一切 ⇒ 权限错静默变"空状态"⇒ 重复提交。

// 测试名用中文（可读性优先，与 TS 侧的描述性测试名同风格）⇒ 会撞 `non_snake_case`，整文件放行。
#![allow(non_snake_case)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lovart_client::engine::state::{
    all, clear, holds_slot, is_not_done, is_terminal, is_unfinished, json_key, new_record,
    normalize_state_text, row as status_row, set, slots_in_use, PersistOutcome, RecordPatch,
    StateError, StateStore, StateStoreOptions, TaskRecord, TaskStatus, STATUS_TABLE,
};
use lovart_client::lovart::prompt::PromptTask;

// ── 测试脚手架 ────────────────────────────────────────────────────────

static SEQ: AtomicUsize = AtomicUsize::new(0);

/// 每次调用一个独立目录 —— 与 TS 的 `mkdtemp` 同作用（不引 `tempfile` 依赖）。
fn case_dir() -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("lv-state-{}-{}", std::process::id(), n));
    fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

fn make_task(id: &str) -> PromptTask {
    PromptTask {
        id: id.to_string(),
        prompt: format!("提示词-{id}"),
        model: "nano-bn-2".to_string(),
        aspect_ratio: "3:4".to_string(),
        image_size: "2K".to_string(),
        line: 1,
        ref_inputs: None,
        refs: None,
        ref_signature: None,
    }
}

fn open_state() -> (StateStore, PathBuf) {
    let path = case_dir().join("state.jsonl");
    let store = StateStore::open(&path).expect("打开 state");
    (store, path)
}

fn read_lines(path: &PathBuf) -> Vec<String> {
    fs::read_to_string(path)
        .expect("读 state")
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

fn patch_status(status: TaskStatus) -> RecordPatch {
    RecordPatch {
        status: Some(status),
        ..Default::default()
    }
}

// ── ① 集合不变量 ──────────────────────────────────────────────────────

#[test]
fn 终态与占槽状态必须互斥() {
    for status in all() {
        assert!(
            !(is_terminal(status) && holds_slot(status)),
            "「{}」同时是终态与占槽状态 ⇒ 槽位永不释放",
            status.as_str()
        );
    }
}

#[test]
fn 终态归类被钉死() {
    // ⚠️ 这条**不能**由下面那条「派生归类与状态清单一致」代替 ——
    //    那条是**构造性重言式**（`is_unfinished` 的定义就是 `!is_terminal`），
    //    把 `Failed` 挪出终态它照样绿（变异测试实测到过）。
    //
    // 归类是**业务判断**，不是定义关系 ⇒ 必须逐项钉死。
    // 漏一项的后果很具体：`Failed` 不算终态 ⇒ `is_unfinished(Failed) == true`
    // ⇒ 调度器会把已确定性失败的任务反复取出来重试。
    let terminal: Vec<&str> = all()
        .filter(|status| is_terminal(*status))
        .map(|status| status.as_str())
        .collect();
    assert_eq!(terminal, ["done", "no_artifact", "needs_confirm", "failed"]);
}

#[test]
fn 占槽归类被钉死() {
    // 同上：`holds_slot` 也是业务判断，必须逐项钉死。
    // 漏一项的后果：`submitting` 不算占槽 ⇒ 提交窗口内槽位被低估 ⇒ 并发上限形同虚设
    // （TD-01-01 的原始病根）。
    let slot: Vec<&str> = all()
        .filter(|status| holds_slot(*status))
        .map(|status| status.as_str())
        .collect();
    assert_eq!(slot, ["submitting", "running", "timeout"]);
}

#[test]
fn 派生归类与状态清单一致() {
    // ⚠️ 诚实说明：**这条是构造性重言式**（`is_unfinished` / `is_not_done` 的定义
    //    就是那两句话），它**守不住归类本身**（归类由上面两条钉死）。
    //
    // 它的价值只有一个，但那一个很关键：一旦有人把 `is_unfinished` 从"派生"
    // 改成**手写的独立清单**，两份清单就会漂移 —— 那正是 TD-01-03 的病根
    // （两处都叫「未完成」但口径不同）。这条断言就是"必须派生、不许另立清单"的看门人。
    for status in all() {
        assert_eq!(
            is_unfinished(status),
            !is_terminal(status),
            "UNFINISHED 与「非终态」不一致：{}",
            status.as_str()
        );
        assert_eq!(
            is_not_done(status),
            status != TaskStatus::Done,
            "NOT_DONE 与「除 done 外全部」不一致：{}",
            status.as_str()
        );
    }
}

#[test]
fn 处理中与尚未出图是两个口径不能合并() {
    // 这正是 TD-01-03 的根因：两处都叫「未完成」但口径不同。
    // no_artifact = 已结束但没图 ⇒ 属于「尚未出图」，不属于「处理中」。
    assert!(
        !is_unfinished(TaskStatus::NoArtifact),
        "no_artifact 已结束，不该算「处理中」"
    );
    assert!(
        is_not_done(TaskStatus::NoArtifact),
        "no_artifact 没图，必须出现在「尚未出图」"
    );
    assert!(!is_not_done(TaskStatus::Done));
}

#[test]
fn submitting_必须占槽位() {
    // 「已认领、正在提交」如果不算占槽，槽位计数会在提交窗口内**低估**，
    // 并发上限形同虚设 —— 这正是当初不得不加手动计数器的原因。
    assert!(holds_slot(TaskStatus::Submitting));
}

#[test]
fn download_failed_与_unresolved_都不占槽位() {
    // 这两条是「槽位一定有界递减」的落点：生成已结束，槽位必须已释放。
    for status in [TaskStatus::DownloadFailed, TaskStatus::Unresolved] {
        assert!(!holds_slot(status), "{} 不该占槽位", status.as_str());
        assert!(
            !is_terminal(status),
            "{} 不是终态（还要补下载/兜底复查）",
            status.as_str()
        );
    }
}

#[test]
fn 状态清单十个且线格式名互不相同() {
    assert_eq!(all().count(), 10, "状态清单长度变了 ⇒ 必须同步 TS 侧");
    let mut names: Vec<&str> = all().map(|s| s.as_str()).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(before, names.len(), "线格式名有重复 ⇒ 两边写同一个文件会串");
}

#[test]
fn 记录到任务的无损投影_垫图与指纹不能丢() {
    // ⚠️ 这条钉的是 **2026-10-01 那个"点确认后垫图消失"** 的根因：
    //    适配层曾经自己造投影（`record_to_task`），**只挑 4 个字段** —— 垫图一丢，
    //    调度器那条"以声明为准"的同步逻辑就按「没有就清空」把记录里的垫图抹掉。
    //    投影搬进 `TaskRecord::to_prompt_task` 之后，**漏字段会编译不过**；
    //    这条测试再从行为上钉死一次（防止有人"顺手"把它精简回去）。
    let mut record = new_record(&make_task("img2img"));
    record.ref_signature = Some("sig-abc".to_string());
    record.ref_urls = Some(vec!["https://cdn.example/a.png".to_string()]);

    let task = record.to_prompt_task();

    assert_eq!(task.refs, record.ref_urls, "垫图地址丢了 ⇒ 重跑/接着查会变成文生图");
    assert_eq!(task.ref_signature, record.ref_signature, "垫图指纹丢了");
    assert_eq!(
        (task.prompt.as_str(), task.model.as_str()),
        (record.prompt.as_str(), record.model.as_str())
    );
}

// ── 状态登记表（唯一真源）的守卫 ─────────────────────────────────────
//
// ⚠️ 这组测试是**收表之后接手的那个保护**：原先"加状态时穷尽 `match` 编译不过"
//    由编译器盯着；现在判据一律读表，就得由测试盯着"表有没有漏行 / 有没有漂移"。
//    漏一行的后果是运行期 `row()` 直接 panic（那里刻意用 `expect` 不兜底），
//    所以这三条必须钉死。

#[test]
fn 登记表覆盖每个状态且不重复() {
    assert_eq!(STATUS_TABLE.len(), 10, "表行数 ≠ 状态数");
    for status in all() {
        // 漏行 ⇒ 这里 panic（`row()` 的 expect 就是为它写的）
        assert_eq!(status_row(status).status, status);
    }
    let mut seen: Vec<&str> = all().map(|s| s.as_str()).collect();
    seen.sort_unstable();
    let before = seen.len();
    seen.dedup();
    assert_eq!(before, seen.len(), "表里有两行是同一个状态");
}

#[test]
fn 表里的线格式名与_serde_逐字一致() {
    // 两处漂移的后果很具体：`as_str()` 写日志、`serde` 写状态文件 ——
    // 对不上就是"日志里一个名字、文件里另一个名字"，对账时才发现。
    // （收表前 `as_str` 是手写字面量的第二份，收表后由这条测试接管。）
    for status in all() {
        let serde_name = serde_json::to_string(&status).expect("TaskStatus 必须能序列化");
        assert_eq!(
            serde_name,
            format!("\"{}\"", status_row(status).wire),
            "{} 的线格式名与 serde 不一致",
            status.as_str()
        );
    }
}

#[test]
fn json_summary_键名是原契约() {
    // `--status --json` 的 summary 键名**有外部消费者**（脚本可能读 summary.done）。
    // 收表后键名由 `json_key()` 从线格式名算出来，这条测试钉住"算出来的还是原来那些"
    // —— 原先手抄的那 9 个键里**漏了 `submitting`**，现在它补上了。
    let keys: Vec<String> = all().map(json_key).collect();
    for expected in [
        "done",
        "running",
        "timeout",
        "pending",
        "unresolved",
        "downloadFailed",
        "noArtifact",
        "needsConfirm",
        "failed",
        "submitting",
    ] {
        assert!(
            keys.iter().any(|key| key == expected),
            "summary 少了键 {expected}"
        );
    }
}

#[test]
fn 槽位从状态派生_不维护计数器() {
    let (mut store, _) = open_state();
    store.create(new_record(&make_task("a")));
    store.create(new_record(&make_task("b")));
    assert_eq!(slots_in_use(&store), 0, "pending 不占槽位");

    store.update("a", patch_status(TaskStatus::Submitting)).unwrap();
    assert_eq!(slots_in_use(&store), 1, "submitting 必须占槽");

    store.update("a", patch_status(TaskStatus::Running)).unwrap();
    store.update("b", patch_status(TaskStatus::Running)).unwrap();
    assert_eq!(slots_in_use(&store), 2);

    // 生成结束、下载失败 ⇒ 槽位必须已释放
    store.update("a", patch_status(TaskStatus::DownloadFailed)).unwrap();
    assert_eq!(slots_in_use(&store), 1, "download_failed 必须释放槽位");
}

// ── ② 追加写与压实 ────────────────────────────────────────────────────

#[test]
fn 追加写_同一_id_以最后一条为准() {
    let (mut store, path) = open_state();
    store.create(new_record(&make_task("a")));
    for i in 1..=5u32 {
        store
            .update(
                "a",
                RecordPatch {
                    attempts: Some(i),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    assert!(store.flush().is_ok());

    let lines = read_lines(&path);
    assert_eq!(lines.len(), 6, "1 次 create + 5 次 update = 6 行追加");

    // 重开：必须取最后一条，而不是第一条
    let reopened = StateStore::open(&path).unwrap();
    assert_eq!(reopened.get("a").unwrap().attempts, 5);
}

#[test]
fn compact_折叠成每个_id_一行() {
    let (mut store, path) = open_state();
    store.create(new_record(&make_task("a")));
    store.create(new_record(&make_task("b")));
    for i in 0..10u32 {
        store
            .update(
                "a",
                RecordPatch {
                    attempts: Some(i),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    store.compact().unwrap();

    let lines = read_lines(&path);
    assert_eq!(lines.len(), 2, "两个 id ⇒ 两行");
    let reopened = StateStore::open(&path).unwrap();
    assert_eq!(reopened.get("a").unwrap().attempts, 9);
    assert_eq!(reopened.get("b").unwrap().status, TaskStatus::Pending);
}

#[test]
fn 半行_崩溃残留_跳过不影响其它记录() {
    let path = case_dir().join("state.jsonl");
    fs::write(
        &path,
        "{\"id\":\"a\",\"prompt\":\"p\",\"model\":\"m\",\"aspectRatio\":\"3:4\",\"imageSize\":\"2K\",\"status\":\"done\",\"attempts\":0,\"updatedAt\":\"x\"}\n{\"id\":\"b\",\"sta\n",
    )
    .unwrap();

    let store = StateStore::open(&path).unwrap();
    assert_eq!(store.all().len(), 1, "半行必须被跳过，不能污染其它记录");
    assert_eq!(store.get("a").unwrap().status, TaskStatus::Done);
}

#[test]
fn update_未知_id_报错不静默造记录() {
    let (mut store, _) = open_state();
    let error = store
        .update("不存在", patch_status(TaskStatus::Done))
        .unwrap_err();
    assert_eq!(error, StateError::UnknownTask("不存在".to_string()));
    assert_eq!(store.all().len(), 0, "绝不能静默造出一条记录");
}

// ── ③ 落盘失败必须诚实可见（TD-06-01）─────────────────────────────────

#[test]
fn 落盘失败_立刻告警一次且_flush_如实报告() {
    let dir = case_dir();
    let path = dir.join("state.jsonl");
    // 先建一个**可读**文件（否则 `load` 会先失败），再把它设成只读 ⇒ 追加必失败。
    fs::write(&path, "").unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&path, permissions).unwrap();

    let alerts = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = std::sync::Arc::clone(&alerts);
    let mut store = StateStore::open_with(
        &path,
        StateStoreOptions {
            on_write_error: Some(Box::new(move |message: &str| {
                sink.lock().unwrap().push(message.to_string());
            })),
        },
    )
    .unwrap();

    store.create(new_record(&make_task("a")));
    store.update("a", patch_status(TaskStatus::Running)).unwrap();

    let outcome = store.flush();
    match outcome {
        PersistOutcome::Failed {
            message,
            failed_writes,
        } => {
            assert_eq!(failed_writes, 2, "两次写入都失败，必须都计数");
            assert!(!message.is_empty(), "失败原因不能是空串");
        }
        PersistOutcome::Ok => panic!("写不进去却报 ok ⇒ 静默丢断点续跑数据"),
    }
    assert_eq!(
        alerts.lock().unwrap().len(),
        1,
        "必须**立刻**告警一次（跑的中途就暴露，不必等收尾）"
    );

    // 收尾：把权限改回来才能删目录
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(false);
    fs::set_permissions(&path, permissions).unwrap();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn 落盘成功_无告警且_flush_ok() {
    let (mut store, _) = open_state();
    store.create(new_record(&make_task("a")));
    assert!(store.flush().is_ok());
    assert_eq!(store.failed_writes(), 0);
    assert!(store.first_write_error().is_none());
}

#[test]
fn 读取失败只吞_NotFound_其余照抛() {
    // ⚠️ 这条是 Rust 侧**独有**的断言。TS 是 `catch {}` 吞掉一切 ⇒
    //    权限错 / 路径是目录都会静默变成「空状态」⇒ 已有进度被当成不存在 ⇒ **重复提交**。
    //    与 TD-06-01 同一母体，这里就地堵住。
    let dir = case_dir();

    // 不存在 ⇒ 视作空（首次运行）
    let missing = dir.join("nope").join("state.jsonl");
    let store = StateStore::open(&missing).unwrap();
    assert!(store.is_empty(), "首次运行应是空状态，而不是报错");

    // 路径是**目录** ⇒ 读失败，必须抛（不能静默当空状态）
    let as_dir = dir.join("state.jsonl");
    fs::create_dir_all(&as_dir).unwrap();
    assert!(
        StateStore::open(&as_dir).is_err(),
        "读一个目录失败时必须抛错 —— 静默当空状态会导致重复提交"
    );
}

// ── ④ 两个时间戳（进度条与看板的数据基础）──────────────────────────────

fn tick() {
    // 必须等过 1ms：否则「无条件写 now」的实现会因**同毫秒**而蒙混过关，
    // 断言就失去鉴别力（TS 侧的变异测试暴露过这个问题）。
    std::thread::sleep(Duration::from_millis(2));
}

#[test]
fn state_since_新建时就有值() {
    let (mut store, _) = open_state();
    let record = store.create(new_record(&make_task("a")));
    assert!(
        record.state_since.is_some(),
        "create 时必须写 stateSince，否则看板一上来就显示 —"
    );
}

#[test]
fn state_since_状态变化时前进() {
    let (mut store, _) = open_state();
    let initial = store
        .create(new_record(&make_task("a")))
        .state_since
        .clone()
        .unwrap();
    tick();
    let updated = store
        .update("a", patch_status(TaskStatus::Running))
        .unwrap();
    assert_ne!(
        updated.state_since.clone().unwrap(),
        initial,
        "状态变了，状态起点必须跟着变"
    );
}

#[test]
fn state_since_非状态变更时不动() {
    let (mut store, _) = open_state();
    store.create(new_record(&make_task("a")));
    store.update("a", patch_status(TaskStatus::Running)).unwrap();
    let after_status_change = store.get("a").unwrap().state_since.clone();

    tick();

    // finish() 里会这样顺带清掉排队等待 —— 它**不是**状态变更
    let after_side_write = store
        .update(
            "a",
            RecordPatch {
                ready_at: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        after_side_write.state_since, after_status_change,
        "readyAt 顺带写不该动 stateSince —— 这正是 updatedAt 顶替不了它的原因"
    );

    tick();
    let after_counters = store
        .update(
            "a",
            RecordPatch {
                attempts: Some(5),
                busy_retries: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(after_counters.state_since, after_status_change);
}

#[test]
fn state_since_同一状态重复写也不重置计时() {
    let (mut store, _) = open_state();
    store.create(new_record(&make_task("a")));
    store.update("a", patch_status(TaskStatus::Running)).unwrap();
    let first = store.get("a").unwrap().state_since.clone();
    tick();
    let again = store
        .update("a", patch_status(TaskStatus::Running))
        .unwrap();
    assert_eq!(again.state_since, first, "status 没变 ⇒ 不该重置计时");
}

#[test]
fn submitted_at_首次拿到_thread_id_时写入之后不变() {
    let (mut store, _) = open_state();
    store.create(new_record(&make_task("a")));
    assert!(
        store.get("a").unwrap().submitted_at.is_none(),
        "还没提交 ⇒ 不该有 submittedAt"
    );

    let submitted = store
        .update(
            "a",
            RecordPatch {
                status: Some(TaskStatus::Running),
                thread_id: set("t1".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
    let anchor = submitted.submitted_at.clone();
    assert!(anchor.is_some(), "首次拿到 threadId ⇒ 写 submittedAt（30% 锚点）");

    tick();
    let later = store
        .update(
            "a",
            RecordPatch {
                status: Some(TaskStatus::Timeout),
                timeout_count: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(later.submitted_at, anchor, "30% 锚点必须稳定");

    let back = store
        .update("a", patch_status(TaskStatus::Running))
        .unwrap();
    assert_eq!(
        back.submitted_at, anchor,
        "running↔timeout 来回切也不该变"
    );
}

#[test]
fn submitted_at_重跑会重新计时() {
    let (mut store, _) = open_state();
    store.create(new_record(&make_task("a")));
    let first = store
        .update(
            "a",
            RecordPatch {
                status: Some(TaskStatus::Running),
                thread_id: set("t1".to_string()),
                ..Default::default()
            },
        )
        .unwrap()
        .submitted_at
        .clone();

    // queue 的参数变更重置分支：清空 threadId + submittedAt
    store
        .update(
            "a",
            RecordPatch {
                status: Some(TaskStatus::Pending),
                thread_id: clear(),
                submitted_at: clear(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        store.get("a").unwrap().submitted_at.is_none(),
        "重置后不该残留旧锚点"
    );
    assert!(store.get("a").unwrap().thread_id.is_none());

    tick();
    let second = store
        .update(
            "a",
            RecordPatch {
                status: Some(TaskStatus::Running),
                thread_id: set("t2".to_string()),
                ..Default::default()
            },
        )
        .unwrap()
        .submitted_at
        .clone();
    assert!(second.is_some(), "重新提交 ⇒ 新的 30% 锚点");
    assert_ne!(second, first, "必须是**新**时刻，否则进度条会显示上一轮的耗时");
}

// ── ⑤ Rust 侧独有：schema 契约 ────────────────────────────────────────

/// 黄金文件必须能被 Rust **完整**反序列化 —— 两边写同一个文件，字段名/可选性
/// 一旦漂移，这里立刻红。比"看一遍代码"可靠。
#[test]
fn 黄金文件全部能反序列化() {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("fixtures");
    assert!(fixtures.is_dir(), "找不到 fixtures/：{}", fixtures.display());

    let mut checked = 0usize;
    let mut entries: Vec<_> = fs::read_dir(&fixtures)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    entries.sort();

    for dir in entries {
        let golden = dir.join("expected-state.jsonl");
        if !golden.is_file() {
            continue;
        }
        let text = fs::read_to_string(&golden).unwrap();
        let lines: Vec<&str> = text.lines().filter(|line| !line.trim().is_empty()).collect();
        assert!(!lines.is_empty(), "{} 是空的", golden.display());

        for line in &lines {
            let record: Result<TaskRecord, _> = serde_json::from_str(line);
            let record = record.unwrap_or_else(|error| {
                panic!(
                    "Rust 读不了 TS 写出的记录（schema 已漂移）：\n  {error}\n  {line}"
                )
            });
            // 反序列化成功还不够：再序列化回去，关键字段必须原样
            let round_trip = serde_json::to_value(&record).unwrap();
            let original: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(
                round_trip.get("status"),
                original.get("status"),
                "status 往返不一致：{line}"
            );
            assert_eq!(
                round_trip.get("id"),
                original.get("id"),
                "id 往返不一致：{line}"
            );
            // 可选字段必须**省略**而不是写成 null（TS 里 null ≠ undefined）
            for key in ["threadId", "submittedAt", "stateSince", "artifacts", "error"] {
                if original.get(key).is_none() {
                    assert!(
                        round_trip.get(key).is_none(),
                        "{key} 在原文里缺省，Rust 却写成了 null —— TS 侧会读成「存在但为空」"
                    );
                }
            }
        }
        checked += 1;
    }

    assert!(checked >= 7, "只检查到 {checked} 组黄金文件，应为 7 组");
}

#[test]
fn 归一化与_TS_同口径_剥时间_留状态集合() {
    let text = concat!(
        r#"{"id":"a","prompt":"p","model":"m","aspectRatio":"3:4","imageSize":"2K","status":"pending","attempts":0,"timeoutCount":0,"busyRetries":0,"readyAt":0,"stateSince":"T1","updatedAt":"T1"}"#,
        "\n",
        r#"{"id":"a","prompt":"p","model":"m","aspectRatio":"3:4","imageSize":"2K","status":"running","threadId":"t-1","attempts":0,"timeoutCount":0,"busyRetries":0,"readyAt":0,"submittedAt":"T2","stateSince":"T2","updatedAt":"T2"}"#,
        "\n",
    );
    let normalized = normalize_state_text(text);
    assert_eq!(normalized.final_records.len(), 1);
    assert_eq!(normalized.final_records[0].status, "running");
    assert_eq!(normalized.final_records[0].thread_id.as_deref(), Some("t-1"));
    assert_eq!(
        normalized.seen.get("a").unwrap(),
        &vec!["pending".to_string(), "running".to_string()],
        "L2 必须留下出现过的状态集合（能抓到「多绕了一圈」）"
    );
}

// ── ⑥ 垫图（参考图）必须留在记录里 ────────────────────────────────────

/// 「我当初垫的是哪张图」只有一个地方能回答 —— 记录本身。
///
/// 守的不变量：**新建记录时必须把 task 上的垫图信息带过来**。
/// 原先 `new_record` 把 `ref_signature` 写死成 `None`（`ref_urls` 根本不存在），
/// 结果是**第一次跑的任务永远没有垫图信息** —— 界面上看不到、地址也查不到；
/// 后面只有「参数变了」的补丁分支才会写上，而新任务根本走不到那里。
#[test]
fn 新建记录必须带上垫图_否则第一次跑就丢了() {
    let mut task = make_task("ref-task");
    task.refs = Some(vec!["https://cdn.invalid/ref-a.png".to_string()]);
    task.ref_signature = Some("sig-a".to_string());

    let record = new_record(&task);

    assert_eq!(
        record.ref_signature.as_deref(),
        Some("sig-a"),
        "指纹丢了 ⇒ 「换了垫图要重跑」的判定失效"
    );
    assert_eq!(
        record.ref_urls,
        Some(vec!["https://cdn.invalid/ref-a.png".to_string()]),
        "地址丢了 ⇒ 用户再也看不到自己垫的是哪张图"
    );
}

/// 文生图（没有垫图）必须**省略**这两个字段，而不是写成 `null`
/// —— TS 侧把 `null` 读成「存在但为空」，与「没有」不是一回事。
#[test]
fn 文生图不写垫图字段而不是写_null() {
    let record = new_record(&make_task("plain"));
    let value = serde_json::to_value(&record).expect("序列化");
    assert!(value.get("refUrls").is_none(), "没有垫图就不该出现 refUrls");
    assert!(value.get("refSignature").is_none());
}

/// 补丁语义：**换了垫图必须能显式清掉旧地址**，不能只改指纹。
/// 只改指纹会让界面拿**上一张垫图**冒充这一张（用户正是靠这个地址核对垫图的）。
#[test]
fn 垫图地址可以显式清空() {
    let (mut store, _path) = open_state();
    let mut task = make_task("swap");
    task.refs = Some(vec!["https://cdn.invalid/first.png".to_string()]);
    store.create(new_record(&task));

    store
        .update(
            "swap",
            RecordPatch {
                ref_urls: clear(),
                ..Default::default()
            },
        )
        .expect("清空垫图");

    assert_eq!(store.get("swap").expect("记录还在").ref_urls, None);
}
