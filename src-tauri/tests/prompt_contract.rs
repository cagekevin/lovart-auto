//! `prompts.jsonl` 契约回归 —— **逐条对齐 TS 的 `test/prompts.test.ts`**。
//!
//! 重点不是「函数返回什么」，而是两条契约：
//!   ① **fail-loud**：任何非法输入都必须进 `errors`，绝不静默忽略 / 兜底默认值；
//!   ② **id 稳定性**：id 只由提示词文本决定 ⇒ 增删行 / 改顺序不打乱已有进度。
//!      而正因如此，改了参数 id 不变 —— 这是调度器必须做「参数变更检测」的**前提**，
//!      所以这里要把它**钉死**，将来谁想改哈希口径会立刻看到这组断言。
//!
//! ⚠️ 另有**只有 Rust 侧才有**的一条（TS 做不到）：
//!    「黄金 id 对账」—— 拿 `fixtures/*/expected-state.jsonl` 里 TS **真实算出的** id
//!    反向校验 Rust 的 sha1。上面那些断言只能证明「Rust 内部自洽」，
//!    证明不了「Rust 算出的 id 与 TS 一致」；而对齐恰恰是唯一重要的事。

#![allow(non_snake_case)]

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use lovart_client::lovart::prompt::{parse_prompts, ParseOptions, ParsedPrompts, PromptDefaults};

fn defaults() -> PromptDefaults {
    PromptDefaults::default()
}

fn parse(text: &str) -> ParsedPrompts {
    parse_prompts(text, &defaults(), &ParseOptions::default())
}

fn parse_with(text: &str, options: ParseOptions) -> ParsedPrompts {
    parse_prompts(text, &defaults(), &options)
}

// ── 正常路径 ──────────────────────────────────────────────────────────

#[test]
fn 最简一行_只给_prompt_其余取默认值() {
    let result = parse(r#"{"prompt":"一只戴帽子的猫"}"#);
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(result.warnings.is_empty());
    assert_eq!(result.tasks.len(), 1);
    let task = &result.tasks[0];
    assert_eq!(task.prompt, "一只戴帽子的猫");
    assert_eq!(task.model, "nano-bn-2");
    assert_eq!(task.aspect_ratio, "3:4");
    assert_eq!(task.image_size, "2K");
    assert_eq!(task.line, 1);
}

#[test]
fn 行内字段覆盖默认值() {
    let result = parse(r#"{"prompt":"x","model":"nano-bn-pro","ratio":"16:9","size":"1k"}"#);
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    let task = &result.tasks[0];
    assert_eq!(task.model, "nano-bn-pro");
    assert_eq!(task.aspect_ratio, "16:9");
    assert_eq!(task.image_size, "1K", "size 必须归一化成档位写法");
}

#[test]
fn refs_解析成_ref_inputs() {
    let result = parse(r#"{"prompt":"x","refs":["refs/舞者.png","https://a/b.png"]}"#);
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(
        result.tasks[0].ref_inputs.as_deref(),
        Some(
            &[
                "refs/舞者.png".to_string(),
                "https://a/b.png".to_string()
            ][..]
        )
    );
}

#[test]
fn 空行跳过_但不占行号() {
    let result = parse("\n\n{\"prompt\":\"x\"}\n\n\n{\"prompt\":\"y\"}\n");
    assert_eq!(result.tasks.len(), 2);
    assert_eq!(result.tasks[0].line, 3, "行号必须是文件真实行号，否则排错找不到地方");
    assert_eq!(result.tasks[1].line, 6);
}

// ── id 稳定性（调度器的参数变更检测建立在它之上）──────────────────────

#[test]
fn id_只由提示词文本决定_增删行改顺序都不打乱进度() {
    let a = parse("{\"prompt\":\"甲\"}\n{\"prompt\":\"乙\"}").tasks;
    let b = parse("\n{\"prompt\":\"乙\"}\n\n{\"prompt\":\"甲\"}\n").tasks;

    let ids_a: BTreeSet<&String> = a.iter().map(|task| &task.id).collect();
    let ids_b: BTreeSet<&String> = b.iter().map(|task| &task.id).collect();
    assert_eq!(
        ids_a, ids_b,
        "同一批提示词换个顺序，id 必须完全一致（否则断点续跑全失效）"
    );
}

#[test]
fn 改了参数_id_不变_这正是参数变更检测存在的原因() {
    let base = parse(r#"{"prompt":"同一个提示词"}"#).tasks[0].clone();
    let changed = parse(
        r#"{"prompt":"同一个提示词","model":"nano-bn-pro","ratio":"16:9","size":"4K","refs":["a.png"]}"#,
    )
    .tasks[0]
        .clone();

    assert_eq!(
        changed.id, base.id,
        "id 不含 model/ratio/size/refs；若这里变了，说明哈希口径改了，\
         调度器的参数变更检测会变成死代码（旧 state 也会全部失效）"
    );
    // 反过来：参数确实被记进了任务对象，供调度器比对
    assert_ne!(changed.model, base.model);
    assert_ne!(changed.aspect_ratio, base.aspect_ratio);
    assert_ne!(changed.image_size, base.image_size);
}

#[test]
fn 完全相同的提示词_第二条起加序号后缀_互不覆盖() {
    let result = parse("{\"prompt\":\"同\"}\n{\"prompt\":\"同\"}\n{\"prompt\":\"同\"}");
    assert_eq!(result.tasks.len(), 3);
    let ids: Vec<&str> = result.tasks.iter().map(|task| task.id.as_str()).collect();
    let unique: BTreeSet<&str> = ids.iter().copied().collect();
    assert_eq!(unique.len(), 3, "id 必须唯一，否则后一条会覆盖前一条的进度");
    assert!(!ids[0].ends_with("-2"));
    assert!(ids[1].ends_with("-2"), "{}", ids[1]);
    assert!(ids[2].ends_with("-3"), "{}", ids[2]);
}

// ── fail-loud ─────────────────────────────────────────────────────────

#[test]
fn 非法_JSON_非对象_未知字段_prompt_缺失_全部进_errors() {
    let text = [
        "这不是 JSON",
        "[1,2,3]",
        r#"{"prompt":"x","nope":1}"#,
        r#"{"prompt":"   "}"#,
        "{}",
    ]
    .join("\n");
    let result = parse(&text);

    assert_eq!(result.errors.len(), 5, "五行非法，必须五条 errors：{:?}", result.errors);
    // 只有「未知字段」那行仍产出任务（它的 prompt 本身合法）。这不影响安全：
    // 调用方见到任何 errors 都会**终止启动**，这些任务永远不会被提交。
    assert_eq!(result.tasks.len(), 1);
    assert_eq!(result.tasks[0].prompt, "x");

    assert!(result.errors[0].contains("不是合法 JSON"), "{}", result.errors[0]);
    assert!(result.errors[1].contains("必须是 JSON 对象"), "{}", result.errors[1]);
    assert!(result.errors[2].contains("未知字段「nope」"), "{}", result.errors[2]);
    assert!(result.errors[3].contains("prompt 必填"), "{}", result.errors[3]);
    assert!(result.errors[4].contains("prompt 必填"), "{}", result.errors[4]);
}

#[test]
fn 未知模型_默认报错终止_不是_warning() {
    let result = parse(r#"{"prompt":"x","model":"zzz-不存在的模型"}"#);
    assert_eq!(
        result.errors.len(),
        1,
        "未知模型必须终止 —— 静默退回默认模型会让整批跑错"
    );
    assert!(result.errors[0].contains("不在别名表里"), "{}", result.errors[0]);
}

#[test]
fn 未知模型_显式放行后才降级为_warning() {
    let result = parse_with(
        r#"{"prompt":"x","model":"zzz-不存在的模型"}"#,
        ParseOptions {
            allow_unknown_model: true,
        },
    );
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(result.warnings.len(), 1);
    assert_eq!(
        result.tasks[0].model, "zzz-不存在的模型",
        "放行后要按用户写的名字记下来"
    );
}

#[test]
fn ratio_必须形如_w_h() {
    for good in ["16:9", "21:9"] {
        let result = parse(&format!(r#"{{"prompt":"x","ratio":"{good}"}}"#));
        assert!(result.errors.is_empty(), "{good}: {:?}", result.errors);
    }
    for bad in ["16-9", "16:9:1", "", "x:y", "16:"] {
        let result = parse(&format!(
            r#"{{"prompt":"x","ratio":{}}}"#,
            serde_json::to_string(bad).unwrap()
        ));
        assert_eq!(result.errors.len(), 1, "{bad}: {:?}", result.errors);
    }
}

#[test]
fn size_只接受三个档位() {
    for good in ["1K", "2K", "4K", "2k"] {
        let result = parse(&format!(r#"{{"prompt":"x","size":"{good}"}}"#));
        assert!(result.errors.is_empty(), "{good}: {:?}", result.errors);
    }
    // ⚠️ `720p` 从「合法」挪到了「必须拒」—— 2026-10-01 的有意收紧，不是漏写。
    for bad in ["720p", "720P", "8K", "1080p", "2048x2731", ""] {
        let result = parse(&format!(
            r#"{{"prompt":"x","size":{}}}"#,
            serde_json::to_string(bad).unwrap()
        ));
        assert_eq!(result.errors.len(), 1, "{bad}: {:?}", result.errors);
    }
}

#[test]
fn size_归一化成大写档位() {
    // ⚠️ 上面那条只断言「不报错」—— 归一化**错了也不会红**（变异测试暴露过）。
    //    这里把归一后的**值**钉死。（三个档位全大写，`720p` 那个小写特例已随删除消失。）
    for (input, expected) in [("1k", "1K"), ("2k", "2K"), ("4k", "4K"), ("4K", "4K")] {
        let result = parse(&format!(r#"{{"prompt":"x","size":"{input}"}}"#));
        assert_eq!(
            result.tasks[0].image_size, expected,
            "size={input} 应归一成 {expected}"
        );
    }
}

#[test]
fn ratio_每边最多三位数字() {
    // ⚠️ TS 的正则是 `\d{1,3}` —— 四位必须拒。
    //    上面那条用的坏值里没有四位数字的，所以放宽长度**也不会红**（变异测试暴露过）。
    for bad in ["1234:9", "16:9999", "0000:1"] {
        let result = parse(&format!(r#"{{"prompt":"x","ratio":"{bad}"}}"#));
        assert_eq!(result.errors.len(), 1, "{bad} 应被拒：{:?}", result.errors);
    }
    assert!(parse(r#"{"prompt":"x","ratio":"999:999"}"#).errors.is_empty());
}

#[test]
fn refs_必须是非空字符串数组() {
    assert!(parse(r#"{"prompt":"x","refs":["a.png"]}"#).errors.is_empty());
    for bad in ["[]", "\"a.png\"", "[\"a.png\",\"\"]", "[1,2]", "{}"] {
        let result = parse(&format!(r#"{{"prompt":"x","refs":{bad}}}"#));
        assert_eq!(result.errors.len(), 1, "{bad}: {:?}", result.errors);
    }
}

#[test]
fn 类型不符_model_ratio_非字符串也要报错() {
    assert_eq!(parse(r#"{"prompt":"x","model":123}"#).errors.len(), 1);
    assert_eq!(parse(r#"{"prompt":"x","model":"  "}"#).errors.len(), 1);
    assert_eq!(parse(r#"{"prompt":"x","ratio":169}"#).errors.len(), 1);
}

#[test]
fn errors_指向文件真实行号() {
    let result = parse("\n\n\n{\"prompt\":\"好\"}\n{\"prompt\":\"\"}");
    assert_eq!(result.errors.len(), 1);
    assert!(result.errors[0].contains("第 5 行"), "{}", result.errors[0]);
}

// ── Rust 侧独有：与 TS 的真实产出对账 ─────────────────────────────────

/// 拿黄金文件里 TS **真实算出的** id，反向校验 Rust 的 sha1。
///
/// 上面那些断言只能证明「Rust 内部自洽」—— 它证明不了 Rust 算出的 id 与 TS 一致。
/// 而对齐恰恰是唯一重要的事：id 一错，`state.jsonl` 里所有进度都对不上，
/// 断点续跑归零、整批重跑（而且**不会报错**，只会"莫名其妙全都重跑一遍"）。
#[test]
fn 黄金_id_对账_Rust_算出的_id_与_TS_一致() {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("fixtures");
    let mut entries: Vec<PathBuf> = fs::read_dir(&fixtures)
        .expect("读 fixtures/")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    entries.sort();
    assert!(!entries.is_empty(), "fixtures/ 里没有用例");

    let mut checked = 0usize;
    for dir in entries {
        let prompts_path = dir.join("prompts.jsonl");
        let golden_path = dir.join("expected-state.jsonl");
        if !prompts_path.is_file() || !golden_path.is_file() {
            continue;
        }
        let name = dir.file_name().unwrap().to_string_lossy().to_string();

        let parsed = parse(&fs::read_to_string(&prompts_path).unwrap());
        assert!(
            parsed.errors.is_empty(),
            "{name} 的 prompts.jsonl 在 Rust 侧不合法：{:?}",
            parsed.errors
        );

        let rust_ids: BTreeSet<String> = parsed.tasks.iter().map(|task| task.id.clone()).collect();
        assert_eq!(
            rust_ids.len(),
            parsed.tasks.len(),
            "{name}：同一文件里 id 撞了"
        );

        // 黄金文件里出现过的全部 id（含 07 的 initial-state 预置记录）
        let golden_ids: BTreeSet<String> = fs::read_to_string(&golden_path)
            .unwrap()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter_map(|value| value.get("id").and_then(|id| id.as_str()).map(str::to_string))
            .collect();

        for id in &rust_ids {
            assert!(
                golden_ids.contains(id),
                "{name}：Rust 算出 id `{id}`，但 TS 的黄金文件里没有它 —— \
                 **两边哈希口径已经分叉**（这一条不对齐，断点续跑会静默全失效）\n  \
                 Rust ids: {rust_ids:?}\n  黄金 ids: {golden_ids:?}"
            );
        }
        checked += 1;
    }

    assert!(checked >= 7, "只对账了 {checked} 组，应为 7 组");
}
