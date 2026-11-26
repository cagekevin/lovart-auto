/**
 * AI 对话插件 —— **唯一入口**：`AssistantPlugin.create({ mount, adapters })`。
 *
 * ── 自包含 ────────────────────────────────────────────────────────────
 * 本目录（`index / store / render / ui / style`）整目录拷走即可接给别的宿主：
 * 不 import 本仓任何模块、不读 `.env`、不持密钥、不直连模型。
 * 宿主只需要实现 4 个 adapter + 6 条同名命令（形状见 `docs/plan/21` §3.4）。
 *
 * ── 它不认识"钱" ──────────────────────────────────────────────────────
 * 发送结果三态由宿主后端给（`status: ok | not_sent | delivered`）：
 * 只有 `not_sent` 允许原样重试；`delivered` 只显示、不重试（可能已计费）。
 * 这里**不猜**状态、不拼错误文案 —— `message` 一律原样显示。
 */
(function (root) {
  'use strict';

  function create(options) {
    const mount = options.mount;
    const adapters = options.adapters || {};
    /** 可选的宿主动作（拉出去 / 停靠回来）—— 不传就没有那两个图标。 */
    const actions = options.actions || {};
    const render = root.AssistantRender;
    const store = root.AssistantStore;

    if (!mount) throw new Error('createAssistantPlugin 需要 mount 元素');
    if (!render || !store) throw new Error('先引 render.js 与 store.js，再引 index.js');

    let state = null;
    let opened = false;
    let sending = false;

    const ui = root.AssistantUi.create({
      mount: mount,
      render: render,
      store: store,
      handlers: {
        onSend: send,
        onNew: async () => {
          ui.clearInput();
          await mutate('assistant_new');
        },
        onSwitch: async (id) => {
          // 未发送的输入不落盘 ⇒ 切会话即丢弃（已接受的损失，见 plan §6）
          ui.clearInput();
          await mutate('assistant_switch', { id: id });
        },
        onRename: async (id, title) => {
          await mutate('assistant_rename', { id: id, title: title });
        },
        onDelete: async (id) => {
          ui.clearInput();
          await mutate('assistant_delete', { id: id });
        },
        onClose: () => {
          opened = false;
          if (options.onClose) options.onClose();
        },
        onPickFiles: async (files) => {
          const paths = [];
          for (const file of files) {
            const path = await saveFile(file);
            if (path) paths.push(path);
          }
          ui.addAttachment(paths);
        },
        onPasteImages: async (event) =>
          adapters.pathsFromClipboard ? adapters.pathsFromClipboard(event) : [],
        openImage: (src) => {
          if (adapters.openImage) adapters.openImage(src);
        },
        imageSrc: (path) => (adapters.imageSrc ? adapters.imageSrc(path) : path),
        // 宿主给了才画那两个图标（插件不认窗口，见 plan 22 §3.4）
        onDetach: actions.onDetach,
        onDock: actions.onDock,
        onToggleTop: actions.onToggleTop,
        onDragStart: actions.onDragStart,
        onOpenMain: actions.onOpenMain,
      },
    });

    // ── 命令 ──────────────────────────────────────────────────────────

    async function call(name, payload) {
      try {
        return await adapters.invoke(name, payload);
      } catch (error) {
        // 命令抛错 = 宿主侧真崩了（业务失败一律在返回值里），照样原样显示
        return { status: 'error', reason: 'storage', message: String(error), detail: '' };
      }
    }

    async function saveFile(file) {
      if (!adapters.saveImageFile) return '';
      try {
        return await adapters.saveImageFile(file);
      } catch (error) {
        ui.setError(String(error));
        return '';
      }
    }

    function apply(result) {
      if (!result || result.status !== 'ok' || !result.state) {
        ui.setError((result && result.message) || '读不到会话');
        return false;
      }
      state = result.state;
      ui.setState(state);
      return true;
    }

    async function refresh() {
      const result = await call('assistant_get_state');
      apply(result);
    }

    async function mutate(name, payload) {
      const result = await call(name, payload);
      const ok = apply(result);
      // 删空 / 删当前后 activeId 可能变了 —— 由后端的 state 决定，前端不自己挑
      if (ok) ui.setError('');
      return ok;
    }

    async function send() {
      if (sending || !state || !state.activeId) return;
      const text = ui.getText().trim();
      const imagePaths = ui.attachments();
      if (!text && !imagePaths.length) return;

      // ① **提交即清空**：文字和图片立刻离开输入框，换成一条"待确认"气泡。
      //    否则等 10–60 秒回复期间，用户看着满格的输入框会以为没发出去，再点一次 = 重复扣费。
      const snapshot = { text: text, imagePaths: imagePaths };
      ui.clearInput();
      ui.appendPending(text, imagePaths);

      sending = true;
      ui.setSending(true);
      ui.setError('');

      const result = await call('assistant_send', {
        conversationId: state.activeId,
        text: text,
        imagePaths: imagePaths,
      });

      sending = false;
      ui.setSending(false);

      if (result && result.status === 'ok') {
        // 服务端 state 里已经有这两条消息 ⇒ 重建会覆盖掉"待确认"那条
        ui.dropPending();
        apply(result);
        return;
      }

      if (result && result.status === 'not_sent') {
        // 没发出去 ⇒ 把内容**放回输入框**（一个字都不许丢），可原样重试
        ui.dropPending();
        ui.restoreInput(snapshot);
        ui.setError(result.message || '没有发出去');
        return;
      }

      // 已发出（可能已计费）：**待确认气泡留着**、内容不放回输入框（否则等于鼓励重发）
      ui.setError(result && result.message ? result.message : '上游已收到请求，但没有保存回复');
      if (result && result.reply) ui.appendEphemeral(result.reply);
    }

    // ── 宿主用的实例方法 ──────────────────────────────────────────────

    return {
      open() {
        opened = true;
        void refresh();
        ui.focus();
      },
      close() {
        opened = false;
      },
      isOpen() {
        return opened;
      },
      /** 点图 / 拖拽进对话的唯一入口（宿主调）。 */
      addAttachment(paths) {
        if (!opened) this.open();
        ui.addAttachment(paths);
      },
      refresh: refresh,
      destroy() {
        opened = false;
        ui.destroy();
      },
    };
  }

  root.AssistantPlugin = { create: create };
})(typeof globalThis !== 'undefined' ? globalThis : this);
