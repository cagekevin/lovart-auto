/**
 * 提示词富文本的**芯片格式契约 + DOM 工具**（无框架、无构建，挂 `window.PromptChips`）。
 *
 * ── 从哪来 ──────────────────────────────────────────────────────────
 * 形态照搬 maomao 生图节点那一套（`src/components/canvas/shell/promptChipFormat.ts`
 * + `promptChips.ts`）：输入框是 `contentEditable`，`@` 调出素材，选中的素材变成
 * 行内**芯片**（图片芯片带缩略图），序列化成 `@{id:label|thumb}` 字符串存下来。
 *
 * ── 为什么用「字符串里的 token」而不是 DOM 快照 ─────────────────────
 * token 是**纯文本**：能进 JSONL、能进 state.jsonl、能在刷新后重建，
 * 而 DOM 快照不行。且它和"人写的话"共处一个字符串 ⇒ 粘贴、复制、发出去都是一条线。
 *
 * ── 只搬哪几件 ──────────────────────────────────────────────────────
 * 搬：格式契约（读/写同一个正则）· 序列化 / 反序列化 · 生成前解析。
 * 不搬：创作库预设芯片、上游文本芯片 —— 本工具的素材池**只有垫图**（参考图）。
 *
 * ⚠️ 正则必须是**工厂**（`promptChipRe()`）而不是模块级常量：`/g` 正则带 `lastIndex`
 *    可变状态，共享一个实例时任何一次 `exec` 中途返回都会让下次**从错位置开始**（静默丢匹配）。
 */
(function () {
  'use strict';

  /**
   * 芯片序列化正则（读侧唯一入口，禁止在别处手写）。
   * 格式：`@{id:label}` 或 `@{id:label|thumbUrl}`（thumb 可选，旧数据向后兼容）。
   * group1 = id · group2 = label · group3 = 可选缩略图 URL（已 encodeURIComponent）。
   */
  function promptChipRe() {
    return /@\{([^:]+):([^|}]*)(?:\|([^}]+))?\}/g;
  }

  /** 芯片串的**唯一构造函数**（写侧唯一入口，与 `promptChipRe` 成对）。 */
  function buildChipText(id, label, thumbUrl) {
    const thumb = thumbUrl ? encodeURIComponent(thumbUrl) : '';
    return thumb ? `@{${id}:${label}|${thumb}}` : `@{${id}:${label}}`;
  }

  /** 零宽空格：芯片前后的光标落点占位（否则行首芯片点不进去）。 */
  const ZWSP = '\u200B';

  function isChipEl(node) {
    return !!node && node.nodeType === Node.ELEMENT_NODE && node.hasAttribute('data-ref-id');
  }

  function isBrEl(node) {
    return !!node && node.nodeType === Node.ELEMENT_NODE && node.tagName === 'BR';
  }

  /** 确保芯片前有光标落点（零宽空格）。 */
  function ensureCaretSlotBeforeChip(chip) {
    const previous = chip.previousSibling;
    if (!previous || isBrEl(previous) || isChipEl(previous)) {
      chip.parentNode.insertBefore(document.createTextNode(ZWSP), chip);
    } else if (previous.nodeType === Node.TEXT_NODE && !previous.textContent) {
      previous.textContent = ZWSP;
    }
  }

  /** 遍历根下所有芯片，补齐落点（删字/换行后调用）。 */
  function normalizeChipSlots(root) {
    for (const chip of Array.from(root.querySelectorAll('[data-ref-id]'))) {
      ensureCaretSlotBeforeChip(chip);
    }
  }

  /**
   * 序列化：`contentEditable` 根 DOM → `@{id:label}` 字符串。
   * 文本节点原样 · `<br>` → `\n` · 芯片 → token · 剔除零宽空格 · 去掉尾部换行。
   */
  function serializeDOM(root) {
    let result = '';
    const walk = (node) => {
      if (node.nodeType === Node.TEXT_NODE) {
        result += node.textContent || '';
        return;
      }
      if (node.nodeType !== Node.ELEMENT_NODE) return;
      if (node.hasAttribute('data-ref-id')) {
        result += buildChipText(
          node.getAttribute('data-ref-id') || '',
          node.getAttribute('data-ref-label') || '',
          node.getAttribute('data-ref-thumb') || undefined,
        );
        return;
      }
      if (node.tagName === 'BR') {
        result += '\n';
        return;
      }
      for (const child of Array.from(node.childNodes)) walk(child);
    };
    for (const child of Array.from(root.childNodes)) walk(child);
    return result.split(ZWSP).join('').replace(/\n+$/, '');
  }

  /**
   * 建芯片元素（`contentEditable=false` 的行内 span —— 浏览器会把它当**一个整体**删）。
   * 图片芯片带缩略图；缩略图地址写进 `data-ref-thumb` ⇒ 字符串自带，重建后不依赖外部列表。
   */
  function buildChipEl(id, label, thumbnailUrl) {
    const span = document.createElement('span');
    span.className = 'prompt-chip';
    span.contentEditable = 'false';
    span.setAttribute('data-ref-id', id);
    span.setAttribute('data-ref-label', label);
    if (thumbnailUrl) span.setAttribute('data-ref-thumb', thumbnailUrl);
    span.title = label;

    if (thumbnailUrl) {
      const img = document.createElement('img');
      img.className = 'prompt-chip-thumb';
      img.src = thumbnailUrl;
      img.alt = '';
      span.appendChild(img);
    } else {
      const icon = document.createElement('span');
      icon.className = 'prompt-chip-icon';
      icon.setAttribute('aria-hidden', 'true');
      icon.textContent = '@';
      span.appendChild(icon);
    }

    const labelEl = document.createElement('span');
    labelEl.className = 'prompt-chip-label';
    labelEl.textContent = label.length > 16 ? `${label.slice(0, 14)}…` : label;
    span.appendChild(labelEl);
    return span;
  }

  /** 反序列化：`@{id:label}` 文本 → Node[]（文本 + `<br>` + 芯片）。 */
  function renderPromptToNodes(text, metaMap) {
    const nodes = [];
    const pushChip = (chip) => {
      const previous = nodes[nodes.length - 1];
      if (!previous || isBrEl(previous) || isChipEl(previous)) {
        nodes.push(document.createTextNode(ZWSP));
      }
      nodes.push(chip);
    };
    const pushTextWithBreaks = (chunk) => {
      if (!chunk) return;
      chunk.split('\n').forEach((line, index) => {
        if (index > 0) nodes.push(document.createElement('br'));
        if (line) nodes.push(document.createTextNode(line));
      });
    };

    let lastIndex = 0;
    let match;
    const re = promptChipRe();
    while ((match = re.exec(text)) !== null) {
      pushTextWithBreaks(text.slice(lastIndex, match.index));
      const id = match[1];
      const meta = metaMap ? metaMap.get(id) : undefined;
      // label 以 metaMap（当前最新）为准，字符串里那个可能是旧的
      const label = (meta && meta.label) || match[2];
      let thumb = '';
      if (match[3]) {
        try {
          thumb = decodeURIComponent(match[3]);
        } catch (error) {
          thumb = match[3];
        }
      }
      if (!thumb && meta && meta.url) thumb = meta.url;
      pushChip(buildChipEl(id, label, thumb));
      lastIndex = re.lastIndex;
    }
    pushTextWithBreaks(text.slice(lastIndex));
    return nodes;
  }

  /**
   * 生成前解析：把 `@{id:label}` 芯片解析成「纯文本 + 参考图」。
   *
   * · 图片芯片 → 其 url 进参考图列表（同 id 去重），位置换成显式垫图引用
   *   `[img=图片N]` —— 编号就是它在文本里**首次出现**的顺序；
   * · 找不到对应素材的芯片 → 替换为空（不产生垃圾字符）。
   *
   * ⚠️ 为什么不是裸词「图片N」：那样模型会把「图片1」当成画面描述词汇而忽略垫图。
   */
  function resolvePromptChips(rawPrompt, refImages) {
    const urlById = new Map();
    for (const ref of refImages || []) {
      if (ref && ref.id && ref.url) urlById.set(ref.id, ref.url);
    }
    const indexById = new Map();
    const resolved = [];
    const text = String(rawPrompt || '').replace(
      promptChipRe(),
      (_match, id) => {
        const url = urlById.get(id);
        if (!url) return '';
        let index = indexById.get(id);
        if (index === undefined) {
          index = indexById.size + 1;
          indexById.set(id, index);
          resolved.push({ id, url });
        }
        return `[img=图片${index}]`;
      },
    );
    return { text: text.trim(), refImages: resolved };
  }

  /**
   * 参考图合并去重：**芯片图在前、其余在后**。
   * ⚠️ 顺序是语义的一部分：芯片顺序对应文本里的「图片N」编号，重排就会错位垫图引用。
   */
  function mergeReferenceImageUrls(chipImages, otherImages) {
    const out = [];
    const seen = new Set();
    for (const item of [...(chipImages || []), ...(otherImages || [])]) {
      const url = item && item.url;
      if (typeof url !== 'string' || url === '') continue;
      if (seen.has(url)) continue;
      seen.add(url);
      out.push(url);
    }
    return out;
  }

  window.PromptChips = {
    promptChipRe,
    buildChipText,
    ZWSP,
    isChipEl,
    isBrEl,
    ensureCaretSlotBeforeChip,
    normalizeChipSlots,
    serializeDOM,
    buildChipEl,
    renderPromptToNodes,
    resolvePromptChips,
    mergeReferenceImageUrls,
  };
})();
