/**
 * 垫图条 —— **渲染 + 手势的唯一实现**（`docs/plan/18` §4）。
 *
 * 两处调用（编辑区 `#refs` / 预设编辑器 `#presetRefs`）共用这一份：
 *   RefStrip.render(box, refs, { src, onRemove, onReorder })
 *
 * ── 不做什么 ──────────────────────────────────────────────────────────
 * · **不持状态**：删 / 排完把结果回交调用方（编辑区写 draft.refs，预设写本地态）。
 * · **不管「+」按钮**：它接的是各自的 file input，组件只画缩略图。
 * · **不做业务校验**：找不到 id 这类事回交调用方（不在 DOM 事件里 throw）。
 *
 * ── 为什么不用 HTML5 draggable ────────────────────────────────────────
 * 窗口 dragDropEnabled 默认开着（拖文件进窗口靠它），webview 内部 dragstart/drop 会被吃掉。
 * 监听挂 window 而非 setPointerCapture：源节点可能被看板轮询整块换掉，捕获跟着丢。
 */
(function () {
  'use strict';

  const ZWSP = '\u200B';

  function startPointerDrag(event, handlers) {
    if (event.button !== 0) return;
    const fromX = event.clientX;
    const fromY = event.clientY;
    let moved = false;
    const onMove = (moveEvent) => {
      if (!moved) {
        if (Math.hypot(moveEvent.clientX - fromX, moveEvent.clientY - fromY) < 4) return;
        moved = true;
        handlers.onStart();
      }
      handlers.onMove(moveEvent);
    };
    const onEnd = (endEvent) => {
      window.removeEventListener('pointermove', onMove);
      window.removeEventListener('pointerup', onEnd);
      window.removeEventListener('pointercancel', onEnd);
      window.removeEventListener('blur', onEnd);
      handlers.onEnd(endEvent, moved);
    };
    window.addEventListener('pointermove', onMove);
    window.addEventListener('pointerup', onEnd);
    window.addEventListener('pointercancel', onEnd);
    window.addEventListener('blur', onEnd);
  }

  /** 把拖着的那一格挪到离指针最近的兄弟前/后（顺序唯一来源 = DOM）。 */
  function placeRefNode(box, node, event) {
    const siblings = Array.from(box.querySelectorAll('.ref')).filter((item) => item !== node);
    if (!siblings.length) return;
    let nearest = siblings[0];
    let best = Infinity;
    for (const item of siblings) {
      const rect = item.getBoundingClientRect();
      const dx = event.clientX - (rect.left + rect.width / 2);
      const dy = event.clientY - (rect.top + rect.height / 2);
      const distance = dx * dx + dy * dy;
      if (distance < best) { best = distance; nearest = item; }
    }
    const rect = nearest.getBoundingClientRect();
    if (event.clientX > rect.left + rect.width / 2) nearest.after(node);
    else nearest.before(node);
  }

  /**
   * 渲染垫图条（**不含「+」按钮**）。
   * @param {HTMLElement} box 容器
   * @param {Array<{id:string,url:string,label:string}>} refs 当前列表（不修改）
   * @param {{src:(url:string)=>string, onRemove:(id:string)=>void, onReorder:(ids:string[])=>void}} handlers
   */
  function render(box, refs, handlers) {
    box.replaceChildren();
    refs.forEach((ref, index) => {
      const node = document.createElement('div');
      node.className = 'ref';
      node.dataset.refId = ref.id;
      node.title = `第 ${index + 1} 张 · 拖动可调整顺序`;

      const img = document.createElement('img');
      img.src = handlers.src(ref.url);
      img.alt = ref.label || '';
      img.loading = 'lazy';
      img.draggable = false;
      node.appendChild(img);

      // 序号 = 提交位次（文本里 `[img=图片N]` 的 N 就是它）。
      const num = document.createElement('span');
      num.className = 'ref-num';
      num.textContent = String(index + 1);
      node.appendChild(num);

      const remove = document.createElement('button');
      remove.className = 'ref-remove';
      remove.type = 'button';
      remove.textContent = '×';
      remove.title = '移除参考图';
      remove.addEventListener('click', () => handlers.onRemove(ref.id));
      node.appendChild(remove);

      node.addEventListener('pointerdown', (event) => {
        if (event.target.closest('.ref-remove')) return;
        node.classList.add('is-armed');
        startPointerDrag(event, {
          onStart: () => node.classList.add('is-dragging'),
          onMove: (moveEvent) => placeRefNode(box, node, moveEvent),
          onEnd: (_endEvent, moved) => {
            node.classList.remove('is-armed', 'is-dragging');
            if (!moved) return;
            // 顺序只从 DOM 读一次，回交调用方；缺 id 不抛，回交空数组让调用方报。
            const ids = Array.from(box.querySelectorAll('.ref')).map((item) => item.dataset.refId);
            handlers.onReorder(ids);
          },
        });
      });
      box.appendChild(node);
    });
  }

  window.RefStrip = { render, startPointerDrag };
})();
