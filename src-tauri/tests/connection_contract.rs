//! `lovart::connection` 的契约测试。
//!
//! 守两件事，都是踩过的真实教训：
//! ① **「载入」与「重载」是两个语义，不能混** —— 「设置保存后不生效」的根因就是混了它们
//!    （`save_settings` 拿"不覆盖"的载入当"重载"用 ⇒ 文件改了、运行时还用旧密钥）；
//! ② **格式错必须报错**（不静默跳过）—— 静默跳过会让用户看到
//!    「缺少 LOVART_ACCESS_KEY」这种离现场很远的提示。

use std::path::PathBuf;

use lovart_client::lovart::connection::{load_dot_env, reload_dot_env};

/// 每次一个独立目录 —— 与 `state_invariants.rs` 同款（不引 `tempfile` 依赖）。
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lv-conn-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

#[test]
fn 载入不覆盖_重载才覆盖() {
    let dir = temp_dir("reload");
    let path = dir.join(".env");
    // 独有键名：测试是并行跑的，不能碰别的测试的变量。
    let key = "LOVART_PROBE_RELOAD";

    // ① 模拟「启动时已经载入过一次」—— 进程里已经有旧值。
    std::env::set_var(key, "旧");
    // ② 用户改了配置 ⇒ 文件里是新值。
    std::fs::write(&path, format!("{key}=新\n")).expect("写 .env");

    // ③ 「载入」语义 ⇒ **不覆盖** ⇒ 旧值还在。
    load_dot_env(&path).expect("载入");
    assert_eq!(
        std::env::var(key).as_deref(),
        Ok("旧"),
        "load_dot_env 必须**不覆盖**（dotenv 标准语义：真实环境变量优先于 .env）"
    );

    // ④ 「重载」语义 ⇒ **覆盖** ⇒ 新值立刻生效。
    reload_dot_env(&path).expect("重载");
    assert_eq!(
        std::env::var(key).as_deref(),
        Ok("新"),
        "reload_dot_env 必须**覆盖** —— 否则设置面板保存后不生效（TD-05-11）"
    );

    std::env::remove_var(key);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 格式错必须报错且指到行() {
    let dir = temp_dir("badformat");
    let path = dir.join(".env");
    // 没有 `=` 的行 ⇒ 解析必然报错。
    std::fs::write(&path, "这行不是 KEY=VALUE\n").expect("写 .env");

    let message = load_dot_env(&path).expect_err("格式错必须报错，不能静默跳过");
    assert!(
        message.contains("第 1 行"),
        "报错必须指到行号（否则用户只知道\"配错了\"却不知道错在哪），实际：{message}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
