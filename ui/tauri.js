/**
 * Tauri 命令调用 —— **唯一包装**（`invoke` 的失败形状只有这一份）。
 *
 * ── 为什么单独一个文件 ────────────────────────────────────────────────
 * `presets.js` / `library.js` 各是一条**命令代理**（模块边界不同，必须分开），
 * 但"命令抛错 ⇒ `{ok:false, reason:'storage', detail}`"这条规则只该有一处：
 * 两份逐行同构的包装就是两个可抄样本（ADR-0001 #1），而它们会一起漂移。
 *
 *   Tauri.invoke(name, args) -> 命令返回值；命令抛错 ⇒ { ok:false, reason:'storage', detail }
 *
 * ⚠️ 命令名、入参、返回形状仍由各模块自己声明（`Presets` / `Library`）——
 *    这里**不登记命令表**：多一张表就是第二份真相。
 */
(function () {
  'use strict';

  async function invoke(name, args) {
    const { invoke: tauriInvoke } = window.__TAURI__.core;
    try {
      return await tauriInvoke(name, args || undefined);
    } catch (error) {
      return { ok: false, reason: 'storage', detail: String(error) };
    }
  }

  window.Tauri = { invoke };
})();
