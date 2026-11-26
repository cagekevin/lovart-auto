/**
 * 提示词预设 —— **命令代理 + 名字兜底**（`docs/plan/18`；无框架、无 DOM）。
 *
 * ── 职责 ──────────────────────────────────────────────────────────────
 * 存储形状真源在 Rust（`lovart::preset`）⇒ 这里**不再定义形状、不再解析 JSON**。
 * 本模块是前端**唯一的命令调用点**（ADR-0005）：app.js 不直接 invoke 预设命令。
 * 名字兜底规则（首行前 16 字）也只在这里一份，界面不自己拼。
 *
 *   list()                -> async { ok:true, presets, fresh } | { ok:false, reason, detail }
 *   create(name,text,images) / update(id,name,text,images) / remove(id) -> async Result
 *   importImages(inputs)  -> async { ok:true, images:[{file,path,label}] } | Result
 *   resolveImages(files)  -> async { ok:true, images:[{file,path,label,missing,reason}] }
 *   displayName(preset)   -> string（同步纯函数）
 *
 * ⚠️ 失败只给 reason（判别位），**不折成空数组**（ADR-0003）；措辞由 app.js 的 presetMessage 翻译。
 */
(function () {
  'use strict';

  const NAME_MAX = 16;
  const KEY_LEGACY = 'lovart.promptPresets.v2'; // 只用于一次性迁移；不再是真相。

  function cleanName(name) {
    return String(name == null ? '' : name).replace(/\s+/g, ' ').trim();
  }

  function nameFrom(text) {
    const firstLine = String(text || '').split('\n').find((line) => line.trim()) || '';
    const name = cleanName(firstLine);
    return name.length > NAME_MAX ? `${name.slice(0, NAME_MAX)}…` : name;
  }

  function displayName(preset) {
    if (!preset) return '';
    return preset.name || nameFrom(preset.text);
  }

  async function invoke(name, args) {
    const { invoke: tauriInvoke } = window.__TAURI__.core;
    try {
      return await tauriInvoke(name, args || undefined);
    } catch (error) {
      return { ok: false, reason: 'storage', detail: String(error) };
    }
  }

  function list() { return invoke('get_presets'); }
  function create(name, text, images) {
    return invoke('create_preset', { name, text, images: images.map((i) => ({ file: i.file })) });
  }
  function update(id, name, text, images) {
    return invoke('update_preset', { id, name, text, images: images.map((i) => ({ file: i.file })) });
  }
  function remove(id) { return invoke('remove_preset', { id }); }
  function importImages(inputs) { return invoke('import_preset_images', { inputs }); }
  function resolveImages(files) { return invoke('resolve_preset_images', { files }); }

  /** 一次性迁移：老 localStorage 里的 {id,name,text} → 后端命令逐条写。不删老数据。 */
  async function migrateLegacy() {
    let raw = null;
    try {
      raw = window.localStorage.getItem(KEY_LEGACY);
    } catch (error) {
      return { ok: false, reason: 'storage', detail: String(error) };
    }
    if (!raw) return { ok: true };
    let items;
    try {
      items = JSON.parse(raw);
    } catch (error) {
      return { ok: false, reason: 'storage', detail: String(error) };
    }
    if (!Array.isArray(items)) return { ok: true };
    for (const item of items) {
      if (!item || typeof item.text !== 'string' || !item.text.trim()) continue;
      const result = await create(item.name || '', item.text, []);
      if (!result.ok) return result;
    }
    return { ok: true };
  }

  window.Presets = {
    list, create, update, remove, importImages, resolveImages, migrateLegacy, displayName,
  };
})();
