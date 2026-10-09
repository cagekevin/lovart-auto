/**
 * `@` 提及候选弹层的**纯函数逻辑**（无 DOM，可单测）—— 移植自 maomao 的
 * `src/components/canvas/shell/promptMention.ts`。
 *
 * 只回答两个问题：**该不该弹**（`detectMentionQuery`）+ **弹在哪**（`computeMentionPlacement`）。
 * 组件（`app.js`）只做 DOM 与事件 —— 不允许在组件里另写 `lastIndexOf('@')` 或自己算坐标，
 * 否则"该不该弹"和"弹在哪"就会有第二份实现，两边必然漂移。
 */
(function () {
  'use strict';

  /** query 超过这个长度即判为"不是在 @ 素材"，自动关闭。 */
  const MENTION_MAX_QUERY = 20;
  /** 弹层固定宽度。 */
  const MENTION_PANEL_W = 280;
  /** 弹层最大高度。 */
  const MENTION_PANEL_MAX_H = 300;
  /** 上方空间不足这个值 ⇒ 翻转到下方。 */
  const MENTION_FLIP_MIN_H = 160;

  /**
   * query 内出现即终止（含全角空格 / 常见中英文标点）。
   * 邮箱与网址（`hello@world.com`）天然被 `.` 拦下 —— 这就是"@ 不该在邮箱里弹"的实现方式。
   */
  const BREAK = new Set([
    ' ', '\n', '\t', '\u3000',
    ',', '.', ';', '!', '?',
    '，', '。', '；', '、', '！', '？', '：',
    '{', '}', '(', ')', '（', '）',
  ]);

  const clamp = (value, min, max) => Math.min(Math.max(value, min), max);

  /**
   * 触发判定（唯一入口）。
   *
   * 语义跟随主流编辑器（Notion / 飞书）：**光标前最近的 `@`，其后到光标之间没有断字符、
   * 且长度不超限**就弹。刻意**不看 `@` 前面是什么** —— 中文里"小猫吃鱼@"这种紧跟写法
   * 应当直接弹候选，不该强迫用户先打一个空格。
   *
   * @param {string} before 光标之前的纯文本片段
   * @returns {{active: boolean, query: string, atIndex: number}}
   */
  function detectMentionQuery(before) {
    const at = String(before || '').lastIndexOf('@');
    if (at < 0) return { active: false, query: '', atIndex: -1 };
    const query = before.slice(at + 1);
    if (query.length > MENTION_MAX_QUERY) return { active: false, query: '', atIndex: -1 };
    for (const ch of query) {
      if (BREAK.has(ch)) return { active: false, query: '', atIndex: -1 };
    }
    return { active: true, query, atIndex: at };
  }

  /**
   * 底对齐定位：**默认向上展开，弹层底边 = `@` 所在行的顶边 − gap**（不遮挡已输入的文字）。
   * 上方空间不够才翻到下方。输入输出都是**视口坐标**（配合 `position: fixed` 使用）。
   *
   * ⚠️ 锚点是「**`@` 字符的矩形**」而不是光标矩形：芯片行 / 不同字宽下光标矩形会偏 3~4px，
   *    弹层就会看着"没贴着 @ 的下笔"。
   *
   * @param {{top:number, bottom:number, left:number}} anchor `@` 字符矩形
   */
  function computeMentionPlacement(anchor, opts) {
    const options = opts || {};
    const panelW = options.panelW ?? MENTION_PANEL_W;
    const panelMaxH = options.panelMaxH ?? MENTION_PANEL_MAX_H;
    const flipMinH = options.flipMinH ?? MENTION_FLIP_MIN_H;
    const gap = options.gap ?? 4;
    const margin = options.margin ?? 8;
    const vw = options.viewportW ?? window.innerWidth;
    const vh = options.viewportH ?? window.innerHeight;

    const spaceAbove = anchor.top - margin;
    const spaceBelow = vh - anchor.bottom - margin;
    const placement = spaceAbove >= flipMinH || spaceAbove >= spaceBelow ? 'up' : 'down';
    const room = placement === 'up' ? spaceAbove : spaceBelow;
    const height = Math.max(96, Math.min(panelMaxH, room));
    const left = clamp(anchor.left, margin, Math.max(margin, vw - panelW - margin));

    return placement === 'up'
      ? { placement, left, top: null, bottom: vh - anchor.top + gap, height }
      : { placement, left, top: anchor.bottom + gap, bottom: null, height };
  }

  window.PromptMention = {
    MENTION_MAX_QUERY,
    MENTION_PANEL_W,
    MENTION_PANEL_MAX_H,
    MENTION_FLIP_MIN_H,
    BREAK,
    detectMentionQuery,
    computeMentionPlacement,
  };
})();
