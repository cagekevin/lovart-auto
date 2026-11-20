//! 读文本文件 —— **只吞 `NotFound`**，其余照抛。
//!
//! ── 为什么单独一个模块（而不是每处各写一次判断）──────────────────────
//!
//! 「读一个文件，读不出来怎么办」这个判断，本仓曾散在 **4 处**，且**每处口径不同**：
//!
//! | 处 | 原口径 | 后果 |
//! |---|---|---|
//! | `StateStore::load` | 只吞 `NotFound`（**对**） | —— |
//! | `gui.rs` 的 `save_settings` | `unwrap_or_default()` | 读不出 `.env` ⇒ 按「空文件」重写 ⇒ **把用户配置整个冲掉** |
//! | `gui.rs` 的 `get_settings` | `unwrap_or_default()` | 设置界面显示空白，用户以为密钥丢了 |
//! | `gui.rs` 的 `list_inbox` | `unwrap_or_default()` | 文件读不出 ⇒ 解析成 0 条 0 错 ⇒ 界面显示「0 条」 |
//!
//! ⇒ 收口到这里：**「读文本」只有一个实现**。
//! 这不是「把判断挪个地方」—— 是把**同一条判断**从 4 份副本收成 1 份，
//! 并把 `StateStore`（原本正确的那一份）也改成调用它，让「正确」不再依赖某一处的自觉。
//!
//! ⚠️ 与 TS 版**有意不同**：TS 的 `catch {}` 吞掉**一切**异常，
//! 于是权限错 / 路径是目录 / IO 错都静默变成「空状态」⇒ 已有进度被当成不存在
//! ⇒ **重复提交、重复出图**（DEBT TD-06-03）。这里只吞 `NotFound`。
//!
//! ── 为什么 `NotFound` 可以吞 ──────────────────────────────────────────
//!
//! 「文件还没建」是**正常状况**（首次运行），不是异常；其余 IO 错都是异常，
//! **必须**让调用方知道 —— 否则就是「失败伪装成成功」（DEBT TD-06-01 的母体）。

use std::io;
use std::path::Path;

/// 读文本。`Ok(None)` **仅当**文件不存在（正常）；其余 IO 错**照抛**。
///
/// 调用方拿到 `Ok(None)` 时的语义是「**这里本来就没有这个文件**」，
/// 而不是「读失败了」—— 后者永远不会以 `None` 的形式出现。
pub fn read_text(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// 同 [`read_text`]，但把「文件不存在」也当成错误。
///
/// 用于**必须存在**的文件（如用户刚点选的那个提示词文件）——
/// 那种场合下「不存在」本身就是异常，不该被 `None` 悄悄放过。
pub fn read_text_required(path: &Path) -> io::Result<String> {
    read_text(path)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("找不到文件：{}", path.display()),
        )
    })
}
