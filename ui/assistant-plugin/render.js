/**
 * 对话渲染 —— 受限 Markdown（形状对齐 maomao 的 `ChatMarkdown` / `markdownImages`）。
 *
 * ── 为什么先解析成数据、再建 DOM ──────────────────────────────────────
 * 解析是**纯函数**（`parseMarkdown` 返回节点描述数组），建 DOM 是 `toDom`。
 * 这样 `node --test` 能直接断言分支，不需要 DOM 桩，也不需要任何依赖。
 *
 * ── 安全 ──────────────────────────────────────────────────────────────
 * **不写 `innerHTML`**：全部 `createElement` + `textContent`。
 * 链接只放行 `http(s):` / `mailto:`；不安全的链接退化成纯文本（`safeHref`）。
 * 图片加载失败显示 `[图片缺失]`（与请求正文里的标记同一句）。
 */
(function (root, factory) {
  if (typeof module === 'object' && module.exports) module.exports = factory();
  else root.AssistantRender = factory();
})(typeof globalThis !== 'undefined' ? globalThis : this, function () {
  'use strict';

  const IMAGE_EXT = /\.(png|jpe?g|webp|gif|bmp|avif)(\?|#|$)/i;
  const SAFE_SCHEME = /^(https?:|mailto:)/i;
  const BARE_URL = /https?:\/\/[^\s<>"'）]+/g;
  const MISSING_IMAGE = '[图片缺失]';

  /** 裸地址像不像一张图（排除 blob / ipfs / ipns / 网页后缀）。 */
  function looksLikeImageUrl(value) {
    const url = String(value == null ? '' : value).trim();
    if (!url) return false;
    if (/^data:image\//i.test(url)) return true;
    if (/^(blob:|ipfs:|ipns:)/i.test(url)) return false;
    if (/^https?:\/\//i.test(url)) return IMAGE_EXT.test(url);
    return IMAGE_EXT.test(url);
  }

  /** 只放行 http(s) / mailto；其余返回空串（调用方退化成纯文本）。 */
  function safeHref(value) {
    const href = String(value == null ? '' : value).trim();
    return SAFE_SCHEME.test(href) ? href : '';
  }

  const INLINE = new RegExp(
    [
      '(`[^`]+`)', // 行内代码
      '(<img[^>]*src=["\\\']([^"\\\']+)["\\\'][^>]*>)', // <img src>
      '(!\\[[^\\]]*\\]\\([^)\\s]+\\))', // ![]()
      '(\\[[^\\]]*\\]\\([^)\\s]+\\))', // [](),普通链接
      '(\\*\\*[^*]+\\*\\*)', // **加粗**
      '(~~[^~]+~~)', // ~~删除线~~
      '(\\*[^*]+\\*)', // *斜体*
      '(@\\{[^}]+\\}|@[A-Za-z0-9_.-]+)', // @ 胶囊
    ].join('|'),
    'g',
  );

  /** 一行文本 → 行内 token 数组。 */
  function parseInline(value) {
    const source = String(value == null ? '' : value);
    const tokens = [];
    let last = 0;
    let match;
    INLINE.lastIndex = 0;
    while ((match = INLINE.exec(source))) {
      if (match.index > last) tokens.push({ type: 'text', text: source.slice(last, match.index) });
      last = INLINE.lastIndex;
      const piece = match[0];

      if (piece[0] === '`') {
        tokens.push({ type: 'code', text: piece.slice(1, -1) });
      } else if (piece.slice(0, 4).toLowerCase() === '<img') {
        const src = /src=["']([^"']+)["']/i.exec(piece);
        if (src) tokens.push({ type: 'image', url: src[1] });
      } else if (piece.slice(0, 2) === '![') {
        const parts = /^!\[([^\]]*)\]\(([^)\s]+)\)$/.exec(piece);
        if (parts) tokens.push({ type: 'image', url: parts[2] });
      } else if (piece[0] === '[') {
        const parts = /^\[([^\]]*)\]\(([^)\s]+)\)$/.exec(piece);
        if (parts) {
          const href = safeHref(parts[2]);
          // ⚠️ 链接目标**不当图**，即使后缀像图片：`[t](a.png)` 是一个链接
          if (href) tokens.push({ type: 'link', label: parts[1], href });
          else tokens.push({ type: 'text', text: parts[1] });
        }
      } else if (piece.slice(0, 2) === '**') {
        tokens.push({ type: 'strong', text: piece.slice(2, -2) });
      } else if (piece.slice(0, 2) === '~~') {
        tokens.push({ type: 'del', text: piece.slice(2, -2) });
      } else if (piece[0] === '*') {
        tokens.push({ type: 'em', text: piece.slice(1, -1) });
      } else {
        tokens.push({ type: 'at', text: piece });
      }
    }
    if (last < source.length) tokens.push({ type: 'text', text: source.slice(last) });
    // 裸地址是第三种图片形态 ⇒ 在同一个函数里切完（调用方不需要记得补这一步）
    return splitBareUrls(tokens);
  }

  /** 把文本 token 里的裸地址切成图片 token（三种图片形态之一）。 */
  function splitBareUrls(tokens) {
    const out = [];
    tokens.forEach((token) => {
      if (token.type !== 'text') {
        out.push(token);
        return;
      }
      const text = token.text;
      let last = 0;
      let match;
      BARE_URL.lastIndex = 0;
      while ((match = BARE_URL.exec(text))) {
        const url = match[0].replace(/[),.;]+$/, '');
        if (!looksLikeImageUrl(url)) continue;
        if (match.index > last) out.push({ type: 'text', text: text.slice(last, match.index) });
        out.push({ type: 'image', url: url });
        last = match.index + url.length;
      }
      if (last < text.length) out.push({ type: 'text', text: text.slice(last) });
    });
    return out;
  }

  function isTableDivider(line) {
    return /^[\s|:-]*-[\s|:-]*$/.test(line) && line.indexOf('-') >= 0 && line.indexOf('|') >= 0;
  }

  function splitRow(line) {
    let text = line.trim();
    if (text.startsWith('|')) text = text.slice(1);
    if (text.endsWith('|')) text = text.slice(0, -1);
    return text.split('|').map((cell) => cell.trim());
  }

  function isBlockStart(line) {
    return (
      /^```/.test(line) ||
      /^#{1,3}\s+/.test(line) ||
      /^\s*>/.test(line) ||
      /^\s*([-*+]|\d+[.)])\s+/.test(line) ||
      /^[\s|:-]*-[\s|:-]*$/.test(line) ||
      /^\s*\|.*\|\s*$/.test(line)
    );
  }

  /** 整段回复 → 块级节点数组（纯数据，可单测）。 */
  function parseMarkdown(value) {
    const lines = String(value == null ? '' : value).replace(/\r\n?/g, '\n').split('\n');
    const blocks = [];
    let index = 0;

    while (index < lines.length) {
      const line = lines[index];
      if (!line.trim()) {
        index += 1;
        continue;
      }

      if (/^```/.test(line)) {
        const lang = line.slice(3).trim();
        const body = [];
        index += 1;
        while (index < lines.length && !/^```/.test(lines[index])) {
          body.push(lines[index]);
          index += 1;
        }
        index += 1; // 吃掉收尾 ```
        blocks.push({ type: 'code', lang: lang, text: body.join('\n') });
        continue;
      }

      if (/^[\s|:-]*-[\s|:-]*$/.test(line) && line.indexOf('|') < 0) {
        blocks.push({ type: 'hr' });
        index += 1;
        continue;
      }

      const heading = /^(#{1,3})\s+(.*)$/.exec(line);
      if (heading) {
        blocks.push({ type: 'heading', level: heading[1].length, inline: parseInline(heading[2]) });
        index += 1;
        continue;
      }

      if (/^\s*\|.*\|\s*$/.test(line) && isTableDivider(lines[index + 1] || '')) {
        const header = splitRow(line).map(parseInline);
        const rows = [];
        index += 2;
        while (index < lines.length && /^\s*\|/.test(lines[index]) && lines[index].trim()) {
          rows.push(splitRow(lines[index]).map(parseInline));
          index += 1;
        }
        blocks.push({ type: 'table', header: header, rows: rows });
        continue;
      }

      if (/^\s*>/.test(line)) {
        const inner = [];
        while (index < lines.length && /^\s*>/.test(lines[index])) {
          inner.push(lines[index].replace(/^\s*>\s?/, ''));
          index += 1;
        }
        blocks.push({ type: 'quote', inline: inner.flatMap((text) => parseInline(text)) });
        continue;
      }

      const bullet = /^\s*[-*+]\s+(.*)$/.exec(line);
      if (bullet) {
        const items = [];
        while (index < lines.length) {
          const hit = /^\s*[-*+]\s+(.*)$/.exec(lines[index]);
          if (!hit) break;
          items.push(parseInline(hit[1]));
          index += 1;
        }
        blocks.push({ type: 'list', ordered: false, items: items });
        continue;
      }

      const ordered = /^\s*\d+[.)]\s+(.*)$/.exec(line);
      if (ordered) {
        const items = [];
        while (index < lines.length) {
          const hit = /^\s*\d+[.)]\s+(.*)$/.exec(lines[index]);
          if (!hit) break;
          items.push(parseInline(hit[1]));
          index += 1;
        }
        blocks.push({ type: 'list', ordered: true, items: items });
        continue;
      }

      const paragraph = [];
      while (index < lines.length && lines[index].trim() && !isBlockStart(lines[index])) {
        paragraph.push(parseInline(lines[index]));
        index += 1;
      }
      blocks.push({ type: 'paragraph', lines: paragraph });
    }

    return blocks;
  }

  // ── 建 DOM（唯一放行 <img> / <a> 的地方，全部 createElement）──────────

  function inlineNodes(tokens, doc, options) {
    const nodes = [];
    tokens.forEach((token) => {
      switch (token.type) {
        case 'code': {
          const code = doc.createElement('code');
          code.className = 'md-inline-code';
          code.textContent = token.text;
          nodes.push(code);
          break;
        }
        case 'strong': {
          const strong = doc.createElement('strong');
          strong.textContent = token.text;
          nodes.push(strong);
          break;
        }
        case 'em': {
          const em = doc.createElement('em');
          em.textContent = token.text;
          nodes.push(em);
          break;
        }
        case 'del': {
          const del = doc.createElement('del');
          del.textContent = token.text;
          nodes.push(del);
          break;
        }
        case 'at': {
          const chip = doc.createElement('span');
          chip.className = 'md-at';
          chip.textContent = token.text;
          nodes.push(chip);
          break;
        }
        case 'link': {
          const anchor = doc.createElement('a');
          anchor.className = 'md-link';
          anchor.href = token.href;
          anchor.target = '_blank';
          anchor.rel = 'noopener noreferrer';
          anchor.textContent = token.label || token.href;
          nodes.push(anchor);
          break;
        }
        case 'image': {
          const image = doc.createElement('img');
          image.className = 'md-image';
          image.alt = '图片';
          image.src = token.url;
          image.addEventListener('error', () => {
            const missing = doc.createElement('span');
            missing.className = 'md-missing';
            missing.textContent = MISSING_IMAGE;
            if (image.parentNode) image.parentNode.replaceChild(missing, image);
          });
          if (options && typeof options.openImage === 'function') {
            image.addEventListener('click', () => options.openImage(token.url));
          }
          nodes.push(image);
          break;
        }
        default: {
          nodes.push(doc.createTextNode(token.text));
        }
      }
    });
    return nodes;
  }

  function blockNode(block, doc, options) {
    switch (block.type) {
      case 'heading': {
        const level = Math.min(3, Math.max(1, block.level));
        const heading = doc.createElement('h' + (level + 2)); // h3 / h4 / h5：不抢对话框标题
        heading.className = 'md-heading md-h' + level;
        inlineNodes(block.inline, doc, options).forEach((node) => heading.appendChild(node));
        return heading;
      }
      case 'hr': {
        const rule = doc.createElement('hr');
        rule.className = 'md-hr';
        return rule;
      }
      case 'quote': {
        const quote = doc.createElement('blockquote');
        quote.className = 'md-quote';
        inlineNodes(block.inline, doc, options).forEach((node) => quote.appendChild(node));
        return quote;
      }
      case 'list': {
        const list = doc.createElement(block.ordered ? 'ol' : 'ul');
        list.className = 'md-list';
        block.items.forEach((item) => {
          const li = doc.createElement('li');
          inlineNodes(item, doc, options).forEach((node) => li.appendChild(node));
          list.appendChild(li);
        });
        return list;
      }
      case 'code': {
        const wrap = doc.createElement('div');
        wrap.className = 'md-code';

        const head = doc.createElement('div');
        head.className = 'md-code-head';
        const lang = doc.createElement('span');
        lang.className = 'md-code-lang';
        lang.textContent = block.lang || 'text';
        const copy = doc.createElement('button');
        copy.type = 'button';
        copy.className = 'md-code-copy';
        copy.textContent = '复制';
        copy.addEventListener('click', () => {
          const done = () => {
            copy.textContent = '已复制';
            setTimeout(() => {
              copy.textContent = '复制';
            }, 1200);
          };
          const fail = () => {
            // 复制失败必须可见（不许点了没反应）
            copy.textContent = '复制失败';
          };
          if (navigator.clipboard && navigator.clipboard.writeText) {
            navigator.clipboard.writeText(block.text).then(done, fail);
          } else {
            fail();
          }
        });
        head.appendChild(lang);
        head.appendChild(copy);

        const pre = doc.createElement('pre');
        pre.className = 'md-code-body';
        const code = doc.createElement('code');
        code.textContent = block.text;
        pre.appendChild(code);

        wrap.appendChild(head);
        wrap.appendChild(pre);
        return wrap;
      }
      case 'table': {
        const table = doc.createElement('table');
        table.className = 'md-table';
        const head = doc.createElement('thead');
        const headRow = doc.createElement('tr');
        block.header.forEach((cell) => {
          const th = doc.createElement('th');
          inlineNodes(cell, doc, options).forEach((node) => th.appendChild(node));
          headRow.appendChild(th);
        });
        head.appendChild(headRow);
        table.appendChild(head);

        const body = doc.createElement('tbody');
        block.rows.forEach((row) => {
          const tr = doc.createElement('tr');
          row.forEach((cell) => {
            const td = doc.createElement('td');
            inlineNodes(cell, doc, options).forEach((node) => td.appendChild(node));
            tr.appendChild(td);
          });
          body.appendChild(tr);
        });
        table.appendChild(body);
        return table;
      }
      default: {
        // 段落：多行之间用 <br>（与 maomao 一致）
        const paragraph = doc.createElement('p');
        paragraph.className = 'md-p';
        block.lines.forEach((line, index) => {
          if (index > 0) paragraph.appendChild(doc.createElement('br'));
          inlineNodes(line, doc, options).forEach((node) => paragraph.appendChild(node));
        });
        return paragraph;
      }
    }
  }

  /** 块级节点数组 → DocumentFragment。 */
  function toDom(blocks, doc, options) {
    const fragment = doc.createDocumentFragment();
    blocks.forEach((block) => fragment.appendChild(blockNode(block, doc, options)));
    return fragment;
  }

  return {
    MISSING_IMAGE: MISSING_IMAGE,
    looksLikeImageUrl: looksLikeImageUrl,
    safeHref: safeHref,
    parseInline: parseInline,
    parseMarkdown: parseMarkdown,
    toDom: toDom,
  };
});
