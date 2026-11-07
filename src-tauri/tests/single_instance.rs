//! 单实例锁：同进程第二次抢锁必须失败。
//!
//! 跨进程的验收在手工清单里（开 GUI 再跑 CLI）；这里钉住「独占」语义本身，
//! 因为它是唯一能挡住「两个写者互相覆盖 state.jsonl」的机制。

use lovart_client::engine::lockfile::FileLock;

#[test]
fn 第二次抢锁被拒_释放后可重抢() {
    let dir = std::env::temp_dir().join(format!("lovart-lock-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let path = dir.join(".lock");

    let first = FileLock::acquire(&path).expect("第一次应成功");
    assert_eq!(first.path(), path.as_path(), "锁路径应原样返回");

    let message = match FileLock::acquire(&path) {
        Ok(_) => panic!("第二个写者必须被拒绝"),
        Err(message) => message,
    };
    // 文案必须指向「占用」，而不是泛化的 IO 错误 ——
    // 否则用户看到「建不了锁」会去查权限，实际是自己开了两个窗口。
    assert!(
        message.contains("已有另一个"),
        "报错文案应说明是被占用：{message}"
    );

    drop(first);
    // 释放后同一路径可以重新抢到（进程内句柄关闭即释放）。
    let third = FileLock::acquire(&path).expect("释放后应能重新抢到");
    drop(third);

    let _ = std::fs::remove_dir_all(&dir);
}
