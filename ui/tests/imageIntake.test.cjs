/**
 * 粘贴摄入单测（零依赖，`node --test`）。
 *
 * 为什么是 `.cjs`：`ui/imageIntake.js` 是 UMD（浏览器挂全局、Node 走 `module.exports`），
 * `.cjs` 才有 `require`；`.mjs` 里没有（与 `assistant.test.cjs` 同一理由）。
 *
 * 为什么测数据不测 DOM：`readPastedFiles` / `limitFiles` 都是纯函数，用极小的假对象
 * 就能喂，不需要 DOM 桩，也不会因为样式改动而假红。
 *
 * ⚠️ 这里**没有**拖放用例：拖放不走网页层（`dragDropEnabled` 开着，DOM 收不到 drop），
 *    拖放落点由 `app.js::wireDropZone` + `maskAtPoint` 负责。
 */
'use strict';

const test = require('node:test');
const assert = require('node:assert');

const intake = require('../imageIntake.js');

// ── 假对象：只给被测代码真正读的那几个字段 ────────────────────────────

function fakeFile(name, type, size) {
  return { name, type, size: size == null ? 1 : size };
}

function fakePasteEvent(items) {
  return {
    clipboardData: {
      items: items.map((file) => ({
        kind: 'file',
        type: file.type,
        getAsFile: () => file,
      })),
    },
  };
}

// ── readPastedFiles ──────────────────────────────────────────────────

test('粘贴：只收 image/*，文本项与非文件项被忽略', () => {
  const png = fakeFile('a.png', 'image/png');
  const txt = fakeFile('a.txt', 'text/plain');
  const event = fakePasteEvent([png]);
  event.clipboardData.items.push({ kind: 'string', type: 'text/plain' });
  event.clipboardData.items.push({ kind: 'file', type: 'text/plain', getAsFile: () => txt });

  const files = intake.readPastedFiles(event);
  assert.strictEqual(files.length, 1);
  assert.strictEqual(files[0].name, 'a.png');
});

test('粘贴：没有图片时返回空数组（调用方据此决定是否 preventDefault）', () => {
  const event = fakePasteEvent([]);
  assert.deepStrictEqual(intake.readPastedFiles(event), []);
  assert.deepStrictEqual(intake.readPastedFiles({}), []);
  assert.deepStrictEqual(intake.readPastedFiles(null), []);
});

// ── limitFiles ───────────────────────────────────────────────────────

test('裁剪：超数量上限的进 rejected，原因写明 count', () => {
  const files = [fakeFile('1.png', 'image/png'), fakeFile('2.png', 'image/png'), fakeFile('3.png', 'image/png')];
  const { files: kept, rejected } = intake.limitFiles(files, 2, Infinity);
  assert.deepStrictEqual(kept.map((f) => f.name), ['1.png', '2.png']);
  assert.strictEqual(rejected.length, 1);
  assert.strictEqual(rejected[0].reason, 'count');
  assert.strictEqual(rejected[0].name, '3.png');
});

test('裁剪：超体积的进 rejected，原因写明 bytes', () => {
  const big = fakeFile('big.png', 'image/png', 20 * 1024 * 1024);
  const small = fakeFile('small.png', 'image/png', 1024);
  const { files: kept, rejected } = intake.limitFiles([big, small], 8, 10 * 1024 * 1024);
  assert.deepStrictEqual(kept.map((f) => f.name), ['small.png']);
  assert.strictEqual(rejected.length, 1);
  assert.strictEqual(rejected[0].reason, 'bytes');
});

test('裁剪：不改入参数组（纯函数）', () => {
  const files = [fakeFile('1.png', 'image/png'), fakeFile('2.png', 'image/png')];
  const before = files.slice();
  intake.limitFiles(files, 1, Infinity);
  assert.deepStrictEqual(files, before);
});

test('裁剪：非图片不占名额、也不进 rejected', () => {
  const png = fakeFile('a.png', 'image/png');
  const pdf = fakeFile('b.pdf', 'application/pdf');
  const { files: kept, rejected } = intake.limitFiles([pdf, png], 1, Infinity);
  assert.deepStrictEqual(kept.map((f) => f.name), ['a.png']);
  assert.deepStrictEqual(rejected, []);
});

// ── installPaste ─────────────────────────────────────────────────────

function fakeZone() {
  const listeners = new Map();
  return {
    listeners,
    addEventListener: (name, fn) => listeners.set(name, fn),
    removeEventListener: (name) => listeners.delete(name),
  };
}

test('installPaste：粘贴图片交给 accept 并 preventDefault', () => {
  const zone = fakeZone();
  let got = null;
  intake.installPaste({ zone, accept: (files) => { got = files; } });

  const png = fakeFile('pasted.png', 'image/png');
  const event = fakePasteEvent([png]);
  let prevented = false;
  event.preventDefault = () => { prevented = true; };

  const onPaste = zone.listeners.get('paste');
  assert.ok(onPaste, 'installPaste 必须绑 paste');
  onPaste(event);

  assert.strictEqual(prevented, true, '图片粘贴必须 preventDefault');
  assert.strictEqual(got.length, 1);
  assert.strictEqual(got[0].name, 'pasted.png');
});

test('installPaste：粘贴纯文字不拦（让输入框自己处理）', () => {
  const zone = fakeZone();
  let called = 0;
  intake.installPaste({ zone, accept: () => { called += 1; } });

  const event = fakePasteEvent([]);
  let prevented = false;
  event.preventDefault = () => { prevented = true; };
  event.clipboardData.getData = () => '一段文字';

  zone.listeners.get('paste')(event);

  assert.strictEqual(prevented, false, '纯文字粘贴不许 preventDefault');
  assert.strictEqual(called, 0, '没有图片就不该调 accept');
});

test('installPaste：uninstall 撤掉监听', () => {
  const zone = fakeZone();
  const uninstall = intake.installPaste({ zone, accept: () => {} });
  assert.ok(zone.listeners.has('paste'));
  uninstall();
  assert.strictEqual(zone.listeners.size, 0);
});

test('installPaste：缺 zone / accept 直接报错（不静默装不上）', () => {
  assert.throws(() => intake.installPaste({ accept: () => {} }), /zone/);
  assert.throws(() => intake.installPaste({ zone: fakeZone() }), /accept/);
});

test('模块不导出拖放函数（拖放不走网页层）', () => {
  assert.strictEqual(intake.readDroppedFiles, undefined);
  assert.strictEqual(intake.install, undefined);
});

// ── limitPaths：拖放只有路径，没有 File.size ─────────────────────────

test('拖放裁剪：只按数量卡（路径没有 size，体积交后端）', () => {
  const paths = ['C:/a.png', 'C:/b.png', 'C:/c.png'];
  const { files: kept, rejected } = intake.limitPaths(paths, 2);
  assert.deepStrictEqual(kept, ['C:/a.png', 'C:/b.png']);
  assert.strictEqual(rejected.length, 1);
  assert.strictEqual(rejected[0].reason, 'count');
});

test('拖放裁剪：数量在内时全留、不 reject', () => {
  const paths = ['C:/a.png', 'C:/b.png'];
  const { files: kept, rejected } = intake.limitPaths(paths, 8);
  assert.deepStrictEqual(kept, paths);
  assert.deepStrictEqual(rejected, []);
});

test('拖放裁剪：max 非有限值 ⇒ 不设上限（不静默丢）', () => {
  const paths = ['C:/a.png', 'C:/b.png', 'C:/c.png'];
  const { files: kept } = intake.limitPaths(paths, Infinity);
  assert.strictEqual(kept.length, 3);
});

test('拖放裁剪：不改入参数组（纯函数）', () => {
  const paths = ['C:/a.png', 'C:/b.png'];
  const before = paths.slice();
  intake.limitPaths(paths, 1);
  assert.deepStrictEqual(paths, before);
});

test('拖放裁剪：空 / 非数组 ⇒ 空结果，不抛', () => {
  assert.deepStrictEqual(intake.limitPaths(null, 8).files, []);
  assert.deepStrictEqual(intake.limitPaths([], 8).files, []);
});
