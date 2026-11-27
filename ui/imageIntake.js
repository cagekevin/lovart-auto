/**
 * 粘贴摄入 —— 把「剪贴板里的图片」变成本工具能用的东西（`docs/plan/27`）。
 *
 * ── 为什么单独成一个模块 ──────────────────────────────────────────────
 * 「clipboard → File[] → data:」这条链原本散在三处（`app.js` 的 `clipboardImagePaths`、
 * `readFilesAsDataUrls`、编辑区 paste 自己整条重写）。它们只依赖 `FileReader`、
 * **不认识任何业务状态**（不碰 `draft` / `presetEditor` / 弹窗），所以能在这里收成一份、
 * 并在 Node 里纯单测。
 *
 * ── ⚠️ 这里**没有**拖放，别在这加 ────────────────────────────────────
 * 窗口没关 `dragDropEnabled`（Tauri 2 默认开着），WebView2 **不向网页派发** drag/drop 事件
 * ⇒ 任何挂在 DOM 上的 `dragover` / `drop` 监听都是**永不触发的死代码**（2026-11-27 实测）。
 * 拖进来的图走 Tauri 的通道：`app.js::wireDropZone` → `onDragDropEvent` → `routeDroppedPaths`。
 * 要改拖放落点，去改那个函数，不要在本文件加监听。
 *
 * ── 依赖方向 ──────────────────────────────────────────────────────────
 * 本模块**不引用 `app.js` 的任何东西**：不 `notice`、不碰 DOM 状态、不 import 宿主。
 * 只返回数据；播报与落点都在 `app.js`（`admitFiles` / `routePastedFiles`）。
 *
 * ── 接口 ──────────────────────────────────────────────────────────────
 *   readPastedFiles(event)              -> File[]  （clipboardData.items，过滤 image/*）
 *   limitFiles(files, max, maxBytes)    -> { files, rejected:[{name, reason}] }（粘贴：数量+体积）
 *   limitPaths(paths, max)              -> { files, rejected }（拖放：只卡数量，路径无 size）
 *   readAsDataUrls(files)               -> Promise<string[]>（File → data:，全工具唯一转换点）
 *   installPaste({ zone, accept })       -> () => void（只绑 paste；返回解绑）
 *
 * ⚠️ 上限常量**不在这里**：`MAX_PASTE_IMAGES` / `MAX_PASTE_BYTES` 是业务数字，
 *    归 `app.js`，由调用方传进 `limitFiles`（ADR-0006：通用模块不认识业务）。
 * ⚠️ 失败不折成空数组：`readAsDataUrls` 读不出就抛（ADR-0003），由调用方决定怎么说。
 */
(function (root, factory) {
  if (typeof module === 'object' && module.exports) module.exports = factory();
  else root.ImageIntake = factory();
})(typeof globalThis !== 'undefined' ? globalThis : this, function () {
  'use strict';

  /** `image/*` 文件才收；其余（文本、PDF、无类型）一律不算图片。 */
  function isImageFile(file) {
    return Boolean(file) && String(file.type || '').startsWith('image/');
  }

  /** 粘贴板里的图 → File[]。没图返回空数组（调用方据此决定是否 preventDefault）。 */
  function readPastedFiles(event) {
    const items = Array.from((event && event.clipboardData && event.clipboardData.items) || []);
    return items
      .filter((item) => item && item.kind === 'file' && String(item.type || '').startsWith('image/'))
      .map((item) => (item.getAsFile ? item.getAsFile() : null))
      .filter(isImageFile);
  }

  /**
   * 数量 / 体积裁剪（**纯函数**，不改入参）。
   *
   * 返回被拒原因而不是直接播报：播报是调用方的事（ADR-0003 一次法则），
   * 这里只给数据，让「谁播报」只有一处。
   */
  function limitFiles(files, max, maxBytes) {
    const all = Array.from(files || []).filter(isImageFile);
    const kept = [];
    const rejected = [];
    for (const file of all) {
      if (Number.isFinite(max) && kept.length >= max) {
        // 超过数量上限的**全部**列为 rejected（不是静默丢）
        rejected.push({ name: file.name || '图片', reason: 'count', limit: max });
        continue;
      }
      if (Number.isFinite(maxBytes) && file.size > maxBytes) {
        rejected.push({ name: file.name || '图片', reason: 'bytes', limit: maxBytes });
        continue;
      }
      kept.push(file);
    }
    return { files: kept, rejected };
  }

  /**
   * File → `data:`。**全工具唯一转换点**（原 `app.js:readFilesAsDataUrls` 搬来）。
   *
   * ⚠️ 读不出就 reject，不折成空串 / 跳过：静默丢图等于「少一张也照发」。
   */
  function readAsDataUrls(files) {
    const list = Array.from(files || []);
    const out = [];
    let chain = Promise.resolve();
    for (const file of list) {
      chain = chain.then(
        () =>
          new Promise((resolve, reject) => {
            const reader = new FileReader();
            reader.onload = () => resolve(reader.result);
            reader.onerror = () => reject(reader.error || new Error('读不了这张图片'));
            reader.readAsDataURL(file);
          }).then((result) => {
            out.push(result);
          }),
      );
    }
    return chain.then(() => out);
  }

  /**
   * 拖放路径的数量裁剪（**纯函数**）。
   *
   * ⚠️ 拖放给的是**本机绝对路径**，没有 `File.size` ⇒ 体积上限在这里判不了，
   *    交给后端 `materialize_image` 已有的体积闸（唯一一道）。所以本函数**只卡数量**。
   *    粘贴那条用 `limitFiles`（有 `size`，两条都卡）—— 两条通道的上限口径不同，
   *    原因只有一个：拿到的东西不同（`File` vs 路径）。这不是两份实现。
   *
   * 返回被拒原因（数据，不是提示）；播报由调用方做一次（ADR-0003）。
   */
  function limitPaths(paths, max) {
    const all = Array.from(paths || []).filter((path) => typeof path === 'string' && path);
    if (!Number.isFinite(max)) return { files: all, rejected: [] };
    const kept = all.slice(0, max);
    const rejected = all.slice(max).map((path) => ({ name: path, reason: 'count', limit: max }));
    return { files: kept, rejected };
  }

  /**
   * 给一个区域装**粘贴**通道：区上有图片就交给 `accept` 并拦住默认行为；
   * **纯文字一律放行**（不拦的话输入框收不到文字，等于把这个区域变成"粘不了字"）。
   *
   * ⚠️ `paste` 只发给**可聚焦**的元素或 document：`zone` 需要 `tabindex`，
   *    或事件从内部输入框冒泡上来。两者都不成立时它收不到 —— 调用方负责。
   * ⚠️ 本函数**只管粘贴**：拖放不在网页层（见文件头）。
   *
   * 返回解绑函数，调用方（或测试）可以撤掉监听。
   */
  function installPaste(options) {
    const zone = options && options.zone;
    const accept = options && options.accept;
    if (!zone || typeof zone.addEventListener !== 'function') {
      throw new Error('installPaste 需要 zone（DOM 元素）');
    }
    if (typeof accept !== 'function') {
      throw new Error('installPaste 需要 accept(files)');
    }

    const onPaste = (event) => {
      const files = readPastedFiles(event);
      // 没有图片 ⇒ **一点都别碰**：纯文字粘贴必须原样交给输入框。
      if (!files.length) return;
      event.preventDefault();
      accept(files);
    };

    zone.addEventListener('paste', onPaste);

    return function uninstall() {
      zone.removeEventListener('paste', onPaste);
    };
  }

  return {
    isImageFile,
    readPastedFiles,
    limitFiles,
    limitPaths,
    readAsDataUrls,
    installPaste,
  };
});
