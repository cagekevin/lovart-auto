/**
 * 会话投影 —— 把后端的 `ChatState` 变成界面要用的几个值（**纯函数，可单测**）。
 *
 * ── 这里不存真相 ──────────────────────────────────────────────────────
 * 真相在 `data/chat-history.json`（后端唯一写者）。本文件只做：
 * 取标题（空串 → 「新会话」占位）、找当前会话、判断能不能发。
 * **不排序**：列表顺序由后端定（`ts` 降序），前端重排会让"删除后切到第一条"两边不一致。
 *
 * ── 为什么输入框内容不在这里 ──────────────────────────────────────────
 * 未发送的输入归插件内存（`ui.js`），不进 `ChatState`：那是 UI 状态，
 * 写进真相文件等于把两个生命周期塞进一个 JSON。
 */
(function (root, factory) {
  if (typeof module === 'object' && module.exports) module.exports = factory();
  else root.AssistantStore = factory();
})(typeof globalThis !== 'undefined' ? globalThis : this, function () {
  'use strict';

  const NEW_TITLE = '新会话';

  /** 标题：空串 = 还没起标题 ⇒ 显示占位（不把占位写回字段）。 */
  function titleOf(conversation) {
    if (!conversation) return NEW_TITLE;
    const title = String(conversation.title || '').trim();
    return title || NEW_TITLE;
  }

  /** 当前会话（没有则 `null`）。 */
  function activeOf(state) {
    if (!state || !state.activeId) return null;
    const list = state.conversations || [];
    return list.find((conversation) => conversation.id === state.activeId) || null;
  }

  /** 能不能发：**只**看有没有当前会话。缺配置不在这里拦（发送时由后端给文案）。 */
  function canSend(state) {
    return Boolean(activeOf(state));
  }

  /** 会话列表（原样返回，**不重排、不过滤**）。 */
  function listOf(state) {
    return (state && state.conversations) || [];
  }

  /** 一条消息的展示体裁：user 用纯文本气泡，assistant 走 Markdown。 */
  function bodyKind(message) {
    return message && message.role === 'assistant' ? 'markdown' : 'text';
  }

  return {
    NEW_TITLE: NEW_TITLE,
    titleOf: titleOf,
    activeOf: activeOf,
    canSend: canSend,
    listOf: listOf,
    bodyKind: bodyKind,
  };
});
