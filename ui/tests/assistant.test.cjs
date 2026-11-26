/**
 * AI 对话插件的前端单测（零依赖，Node 内置 `node --test`）。
 *
 * 为什么是 `.cjs`：插件的 `render.js` / `store.js` 是 UMD（浏览器挂全局、Node 走
 * `module.exports`），`.cjs` 才有 `require`；`.mjs` 里没有。
 *
 * 为什么测"数据"而不是 DOM：解析是纯函数（`parseMarkdown` 返回节点描述），
 * 分支对不对在数据层就能断言，不需要 DOM 桩，也不会因为样式改动而假红。
 */
'use strict';

const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');

const render = require('../assistant-plugin/render.js');
const store = require('../assistant-plugin/store.js');

const PLUGIN_DIR = path.join(__dirname, '..', 'assistant-plugin');

// ── 块级 ────────────────────────────────────────────────────────────

test('块级：标题按 # 数量分级', () => {
  const blocks = render.parseMarkdown('# 一\n## 二\n### 三');
  assert.deepStrictEqual(
    blocks.map((block) => [block.type, block.level]),
    [
      ['heading', 1],
      ['heading', 2],
      ['heading', 3],
    ],
  );
});

test('块级：无序 / 有序列表各自成块', () => {
  const blocks = render.parseMarkdown('- 甲\n- 乙\n\n1. 一\n2. 二');
  assert.deepStrictEqual(
    blocks.map((block) => [block.type, block.ordered, block.items.length]),
    [
      ['list', false, 2],
      ['list', true, 2],
    ],
  );
});

test('块级：markdown 表格识别 |---| 并按行切单元格', () => {
  const blocks = render.parseMarkdown('| 名字 | 数量 |\n|---|---|\n| 甲 | 1 |\n| 乙 | 2 |');
  assert.strictEqual(blocks.length, 1);
  const [table] = blocks;
  assert.strictEqual(table.type, 'table');
  assert.strictEqual(table.header.length, 2);
  assert.deepStrictEqual(
    table.rows.map((row) => row.map((cell) => cell.map((token) => token.text).join(''))),
    [
      ['甲', '1'],
      ['乙', '2'],
    ],
  );
});

test('块级：代码块保留语言与原文（含 json）', () => {
  const blocks = render.parseMarkdown('```json\n{"a":1}\n```');
  assert.strictEqual(blocks[0].type, 'code');
  assert.strictEqual(blocks[0].lang, 'json');
  assert.strictEqual(blocks[0].text, '{"a":1}');
});

test('块级：引用 / 分隔线 / 段落多行', () => {
  const blocks = render.parseMarkdown('> 引用一句\n\n---\n\n第一行\n第二行');
  assert.deepStrictEqual(
    blocks.map((block) => block.type),
    ['quote', 'hr', 'paragraph'],
  );
  assert.strictEqual(blocks[2].lines.length, 2, '两行都进同一个段落（渲染时用 <br>）');
});

// ── 行内 ────────────────────────────────────────────────────────────

test('行内：加粗 / 斜体 / 删除线 / 行内代码 / 链接 / @ 胶囊', () => {
  const tokens = render.parseInline(
    '**粗** *斜* ~~删~~ `code` [站点](https://a.b) @{id:label}',
  );
  assert.deepStrictEqual(
    tokens.map((token) => token.type),
    ['strong', 'text', 'em', 'text', 'del', 'text', 'code', 'text', 'link', 'text', 'at'],
  );
});

test('安全：javascript: 链接退化成纯文本，不产出 link token', () => {
  const tokens = render.parseInline('[点我](javascript:alert(1))');
  assert.ok(tokens.every((token) => token.type !== 'link'), '不许产出可点的链接');
  assert.ok(tokens.some((token) => token.type === 'text' && token.text.includes('点我')));
  assert.strictEqual(render.safeHref('javascript:alert(1)'), '');
  assert.strictEqual(render.safeHref('https://a.b'), 'https://a.b');
  assert.strictEqual(render.safeHref('mailto:a@b.c'), 'mailto:a@b.c');
});

// ── 图片三形态 ──────────────────────────────────────────────────────

test('图片：![](url) / <img src> / 裸 URL 都识别成图片', () => {
  const markdown = render.parseInline('![](a.png)');
  assert.strictEqual(markdown[0].type, 'image');

  const html = render.parseInline('<img src="b.jpg" />');
  assert.strictEqual(html[0].type, 'image');
  assert.strictEqual(html[0].url, 'b.jpg');

  const bare = render.parseInline('看这张 https://x.y/c.png 挺好');
  assert.ok(bare.some((token) => token.type === 'image' && token.url === 'https://x.y/c.png'));
});

test('图片：Markdown 链接目标不当图（[t](a.png) 是链接）', () => {
  const tokens = render.parseInline('[点开](https://x.y/a.png)');
  assert.strictEqual(tokens[0].type, 'link');
  assert.ok(tokens.every((token) => token.type !== 'image'));
});

test('图片判据：排除 blob / ipfs / 网页后缀', () => {
  assert.strictEqual(render.looksLikeImageUrl('https://x.y/a.png'), true);
  assert.strictEqual(render.looksLikeImageUrl('https://x.y/page'), false);
  assert.strictEqual(render.looksLikeImageUrl('blob:abc'), false);
  assert.strictEqual(render.looksLikeImageUrl('ipfs://abc.png'), false);
  assert.strictEqual(render.looksLikeImageUrl('data:image/png;base64,AAA'), true);
});

// ── 零 XSS 面 ───────────────────────────────────────────────────────

test('安全：插件源码里没有 innerHTML / outerHTML / insertAdjacentHTML 写入', () => {
  const files = fs.readdirSync(PLUGIN_DIR).filter((name) => name.endsWith('.js'));
  assert.ok(files.length >= 4, '插件脚本应在 4 个以上');
  // 查的是**写入形态**（带点的成员访问）：注释里写「不写 innerHTML」不算违规，
  // 那正是这条规矩留在代码里的唯一痕迹。
  const banned = ['.innerHTML', '.outerHTML', '.insertAdjacentHTML', 'document.write('];
  files.forEach((name) => {
    const source = fs.readFileSync(path.join(PLUGIN_DIR, name), 'utf8');
    banned.forEach((pattern) => {
      assert.ok(!source.includes(pattern), `${name} 里不许出现 ${pattern}`);
    });
  });
});

// ── 投影（store）────────────────────────────────────────────────────

test('投影：空标题显示「新会话」，但不改字段', () => {
  const conversation = { id: 'c1', title: '', messages: [] };
  assert.strictEqual(store.titleOf(conversation), store.NEW_TITLE);
  assert.strictEqual(conversation.title, '', '占位文案不许写回数据');
  assert.strictEqual(store.titleOf({ id: 'c2', title: '改过的', messages: [] }), '改过的');
});

test('投影：当前会话 / 能不能发，只看 activeId', () => {
  const state = {
    activeId: 'c2',
    conversations: [
      { id: 'c1', title: '一', messages: [] },
      { id: 'c2', title: '二', messages: [] },
    ],
  };
  assert.strictEqual(store.activeOf(state).id, 'c2');
  assert.strictEqual(store.canSend(state), true);
  assert.strictEqual(store.canSend({ activeId: null, conversations: [] }), false);
  assert.strictEqual(store.activeOf({ activeId: 'ghost', conversations: [] }), null);
});

test('投影：列表原样返回，不重排（顺序由后端定）', () => {
  const state = {
    activeId: 'c1',
    conversations: [
      { id: 'c3', ts: 3, messages: [] },
      { id: 'c1', ts: 1, messages: [] },
      { id: 'c2', ts: 2, messages: [] },
    ],
  };
  assert.deepStrictEqual(
    store.listOf(state).map((conversation) => conversation.id),
    ['c3', 'c1', 'c2'],
  );
});

test('投影：user 走纯文本，assistant 走 Markdown', () => {
  assert.strictEqual(store.bodyKind({ role: 'user' }), 'text');
  assert.strictEqual(store.bodyKind({ role: 'assistant' }), 'markdown');
});
