/**
 * 素材库 —— **命令代理**（`docs/plan/20`；无框架、无 DOM）。
 *
 * ── 职责 ──────────────────────────────────────────────────────────────
 * 形状真源在 Rust（`service::library`）⇒ 这里**不定义形状、不解析 JSON、不碰 DOM**。
 * 本模块是前端**唯一**的素材库命令调用点（ADR-0005）：`app.js` 不直接 invoke。
 * 网格渲染、点图进编辑区（`pushRef`）、viewer 按钮取 `source` 都在 `app.js`。
 *
 *   list()       -> async { ok:true, images:[{path,name}] } | { ok:false, reason, detail }
 *   send(source) -> async { ok:true, name } | { ok:false, reason, detail }
 *                   source = {kind:'path'|'data', value}（一个入参形状，两种来源）
 *   remove(file) -> async { ok:true } | { ok:false, reason, detail }
 *
 * ⚠️ 失败只给 reason（判别位），**不折成空数组**（ADR-0003）；措辞由 app.js 的 libraryMessage 翻译。
 * ⚠️ 库 = 后端 `data/library/` 目录本身（不建索引）⇒ 每次 `list()` 都是重扫目录。
 */
(function () {
  'use strict';

  // 包装只有一份（`ui/tauri.js`）：失败形状是它的契约，不在每个代理里各写一遍。
  function invoke(name, args) { return window.Tauri.invoke(name, args); }

  function list() { return invoke('list_library_images'); }
  function send(source) { return invoke('send_to_library', { source }); }
  function remove(file) { return invoke('remove_library_image', { file }); }

  window.Library = { list, send, remove };
})();
