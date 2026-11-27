/**
 * 对话面板 DOM —— 顶栏（标题下拉 / 新建 / 关闭）+ 消息区 + 底部 OneBox。
 *
 * ── 谁拥有什么 ────────────────────────────────────────────────────────
 * 本文件**不调 invoke**、不认识命令名：所有动作通过 `handlers` 交给 `index.js`。
 * 它自己只持有两样**纯 UI 状态**：输入框文字、待发附件 chip（切会话即清空）。
 *
 * ── 交互口径（用户裁定过的）──────────────────────────────────────────
 * 删除会话**不弹窗**：垃圾桶首点变红，2 秒内再点才执行，点别处或超时复位。
 * 重命名是**行内输入**：Enter 提交、Esc 取消。
 */
(function (root, factory) {
  if (typeof module === 'object' && module.exports) module.exports = factory();
  else root.AssistantUi = factory();
})(typeof globalThis !== 'undefined' ? globalThis : this, function () {
  'use strict';

  const DELETE_ARM_MS = 2000;

  function el(doc, tag, className) {
    const node = doc.createElement(tag);
    if (className) node.className = className;
    return node;
  }

  function icon(doc, path, size) {
    const svg = doc.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('viewBox', '0 0 24 24');
    svg.setAttribute('width', String(size || 14));
    svg.setAttribute('height', String(size || 14));
    svg.setAttribute('aria-hidden', 'true');
    const shape = doc.createElementNS('http://www.w3.org/2000/svg', 'path');
    shape.setAttribute('d', path);
    shape.setAttribute('fill', 'none');
    shape.setAttribute('stroke', 'currentColor');
    shape.setAttribute('stroke-width', '1.6');
    shape.setAttribute('stroke-linecap', 'round');
    shape.setAttribute('stroke-linejoin', 'round');
    svg.appendChild(shape);
    return svg;
  }

  const PATH_EDIT = 'M4 20h4L20 8l-4-4L4 16v4zM14 6l4 4';
  const PATH_TRASH = 'M4 7h16M9 7V5h6v2M6 7l1 13h10l1-13M10 11v6M14 11v6';
  const PATH_SEND = 'M12 19V5M6 11l6-6 6 6';
  const PATH_PLUS = 'M12 5v14M5 12h14';
  // 拉出去（箭头出框）/ 停靠回来（箭头入框）
  const PATH_DETACH = 'M14 4h6v6M20 4l-8 8M9 4H5v15h15v-4';
  const PATH_DOCK = 'M10 4H4v6M4 4l8 8M15 20h5V5h-5';
  // 关闭：**用 SVG 不用文字 '×'** —— 文字字形与 13px 图标的基线/大小对不齐，
  // 排在一行右边就是"一大一小、一高一低"。
  const PATH_CLOSE = 'M6 6l12 12M18 6L6 18';
  // 置顶：竖立的图钉
  const PATH_PIN = 'M9 3h6l-1 6 3 3v2H7v-2l3-3zM12 14v7';
  // 回到主页面：房子
  const PATH_HOME = 'M3 11l9-8 9 8M6 9.5V20h12V9.5';

  function create(options) {
    const doc = options.doc || document;
    const render = options.render;
    const store = options.store;
    const handlers = options.handlers || {};
    const mount = options.mount;

    let state = null;
    let sending = false;
    let pendingNode = null; // 「已提交、还没落盘」的气泡
    let attachments = [];
    let armedId = '';
    let armTimer = 0;

    mount.textContent = '';
    const root = el(doc, 'div', 'ap-root');
    mount.appendChild(root);

    // ── 顶栏 ──
    // 窗口是**无系统边框**的（`decorations(false)`）⇒ 这一行自己当标题栏：
    // `data-tauri-drag-region` 让空白处可以拖动窗口，按钮本身仍可点。
    const header = el(doc, 'header', 'agent-header');
    header.setAttribute('data-tauri-drag-region', '');
    // `data-tauri-drag-region` 在无边框窗口里**不一定生效**（取决于 webview 与 Tauri 版本）。
    // 所以再显式接一次：在顶栏空白处按下就调宿主的 `startDragging()`。
    // 按钮上的按下不算（否则点按钮会变成拖窗）。
    header.addEventListener('pointerdown', (event) => {
      if (!handlers.onDragStart) return;
      if (event.button !== 0) return;
      const target = event.target;
      if (target && target.closest && target.closest('button')) return;
      handlers.onDragStart();
    });
    const titleBtn = doc.createElement('button');
    titleBtn.type = 'button';
    titleBtn.className = 'agent-title-btn';
    titleBtn.setAttribute('aria-haspopup', 'listbox');
    titleBtn.setAttribute('aria-expanded', 'false');
    const titleText = el(doc, 'span', 'agent-title-text');
    titleText.textContent = store.NEW_TITLE;
    titleBtn.appendChild(titleText);
    titleBtn.appendChild(icon(doc, 'M4 9l4 4 4-4', 10));

    const pop = el(doc, 'div', 'agent-pop is-conv');
    pop.hidden = true;

    const newBtn = doc.createElement('button');
    newBtn.type = 'button';
    newBtn.className = 'agent-icon-btn';
    newBtn.title = '新建会话';
    newBtn.setAttribute('aria-label', '新建会话');
    newBtn.appendChild(icon(doc, PATH_PLUS, 13));

    // 回主页面（只在宿主提供 `onOpenMain` 时出现）：对话是独立窗口，主窗口可能被压在后面
    const homeBtn = doc.createElement('button');
    homeBtn.type = 'button';
    homeBtn.className = 'agent-icon-btn';
    homeBtn.title = '打开主页面';
    homeBtn.setAttribute('aria-label', '打开主页面');
    homeBtn.appendChild(icon(doc, PATH_HOME, 13));
    homeBtn.addEventListener('click', () => {
      if (handlers.onOpenMain) handlers.onOpenMain();
    });

    // 置顶（只在宿主提供 `onToggleTop` 时出现）：点一下钉住、再点恢复默认
    const pinBtn = doc.createElement('button');
    pinBtn.type = 'button';
    pinBtn.className = 'agent-icon-btn';
    pinBtn.title = '置顶窗口';
    pinBtn.setAttribute('aria-label', '置顶窗口');
    pinBtn.setAttribute('aria-pressed', 'false');
    pinBtn.appendChild(icon(doc, PATH_PIN, 13));
    let pinned = false;
    pinBtn.addEventListener('click', () => {
      pinned = !pinned;
      pinBtn.classList.toggle('is-on', pinned);
      pinBtn.setAttribute('aria-pressed', pinned ? 'true' : 'false');
      if (handlers.onToggleTop) handlers.onToggleTop(pinned);
    });

    const closeBtn = doc.createElement('button');
    closeBtn.type = 'button';
    closeBtn.className = 'agent-icon-btn';
    closeBtn.title = '关闭（Esc）';
    closeBtn.setAttribute('aria-label', '关闭');
    closeBtn.appendChild(icon(doc, PATH_CLOSE, 13));

    // 「拉出去 / 停靠回来」**由宿主决定**：给了动作才画按钮，插件本身不认窗口。
    // （别的宿主不传 `onDetach`/`onDock` ⇒ 这里一个图标都不出现。）
    const detachBtn = doc.createElement('button');
    detachBtn.type = 'button';
    detachBtn.className = 'agent-icon-btn';
    const detachAction = handlers.onDetach
      ? { path: PATH_DETACH, title: '拉出去成独立窗口', run: handlers.onDetach }
      : handlers.onDock
        ? { path: PATH_DOCK, title: '停靠回主窗口', run: handlers.onDock }
        : null;
    if (detachAction) {
      detachBtn.title = detachAction.title;
      detachBtn.setAttribute('aria-label', detachAction.title);
      detachBtn.appendChild(icon(doc, detachAction.path, 13));
      detachBtn.addEventListener('click', () => detachAction.run());
    }

    header.appendChild(titleBtn);
    if (handlers.onOpenMain) header.appendChild(homeBtn);
    if (detachAction) header.appendChild(detachBtn);
    header.appendChild(newBtn);
    if (handlers.onToggleTop) header.appendChild(pinBtn);
    header.appendChild(closeBtn);

    // ── 消息区 ──
    const body = el(doc, 'div', 'agent-body');
    const messages = el(doc, 'div', 'agent-messages');
    const thinking = el(doc, 'div', 'agent-thinking');
    thinking.hidden = true;
    const dots = el(doc, 'span', 'agent-dots');
    for (let i = 0; i < 3; i += 1) dots.appendChild(el(doc, 'i', 'agent-dot'));
    const thinkingText = el(doc, 'span', 'agent-thinking-text');
    thinkingText.textContent = '思考中...';
    thinking.appendChild(dots);
    thinking.appendChild(thinkingText);
    const errorBar = el(doc, 'div', 'agent-error');
    errorBar.hidden = true;
    body.appendChild(messages);
    body.appendChild(thinking);
    body.appendChild(errorBar);

    // ── 底部 OneBox ──
    const composer = el(doc, 'div', 'agent-composer');
    const attRow = el(doc, 'div', 'agent-att-row');
    attRow.hidden = true;

    const textarea = doc.createElement('textarea');
    textarea.className = 'agent-textarea';
    textarea.rows = 1;
    textarea.placeholder = '问点什么…回车发送，Shift+Enter 换行';
    textarea.setAttribute('aria-label', '对话输入');

    const tools = el(doc, 'div', 'agent-tools');
    const importBtn = doc.createElement('button');
    importBtn.type = 'button';
    importBtn.className = 'agent-tool-btn';
    importBtn.title = '导入图片';
    importBtn.setAttribute('aria-label', '导入图片');
    importBtn.appendChild(icon(doc, PATH_PLUS, 14));

    const sendBtn = doc.createElement('button');
    sendBtn.type = 'button';
    sendBtn.className = 'agent-send';
    sendBtn.title = '发送（Enter）';
    sendBtn.setAttribute('aria-label', '发送');
    sendBtn.appendChild(icon(doc, PATH_SEND, 15));

    tools.appendChild(importBtn);
    tools.appendChild(sendBtn);

    const picker = doc.createElement('input');
    picker.type = 'file';
    picker.accept = 'image/*';
    picker.multiple = true;
    picker.hidden = true;

    // ── 输入框高度拖拽 ────────────────────────────────────────────────
    // 手柄钉在盒子**顶边**：往上拖变高、往下拖变矮。手工拉过就以手工值为准
    // （CSS 的 `min-height` 与这里 `MIN_INPUT_H` 同一个数，不会出现"拖到底又被弹回"）。
    const MIN_INPUT_H = 76;
    const grip = el(doc, 'div', 'agent-composer-grip');
    grip.setAttribute('role', 'separator');
    grip.setAttribute('aria-orientation', 'horizontal');
    grip.title = '拖动调整输入框高度';
    grip.addEventListener('pointerdown', (event) => {
      if (event.button !== 0) return;
      event.preventDefault();
      grip.setPointerCapture(event.pointerId);
      composer.classList.add('is-resizing');
      const startY = event.clientY;
      const startH = textarea.offsetHeight;
      // 上限按窗口高度算，不写死一个数（窗口被拉窄/拉矮时不会把消息区挤没）
      const maxH = Math.max(MIN_INPUT_H, Math.round(window.innerHeight * 0.5));
      const onMove = (moveEvent) => {
        const next = Math.min(
          Math.max(startH - (moveEvent.clientY - startY), MIN_INPUT_H),
          maxH,
        );
        textarea.style.height = `${next}px`;
      };
      const onUp = () => {
        composer.classList.remove('is-resizing');
        grip.removeEventListener('pointermove', onMove);
        grip.removeEventListener('pointerup', onUp);
        grip.removeEventListener('pointercancel', onUp);
      };
      grip.addEventListener('pointermove', onMove);
      grip.addEventListener('pointerup', onUp);
      grip.addEventListener('pointercancel', onUp);
    });

    composer.appendChild(grip);
    composer.appendChild(attRow);
    composer.appendChild(textarea);
    composer.appendChild(tools);

    root.appendChild(header);
    root.appendChild(pop);
    root.appendChild(body);
    root.appendChild(composer);
    root.appendChild(picker);

    // ── 内部渲染 ──

    function clearArm() {
      armedId = '';
      window.clearTimeout(armTimer);
      armTimer = 0;
      Array.from(pop.querySelectorAll('.agent-row-btn.is-danger')).forEach((node) =>
        node.classList.remove('is-danger'),
      );
    }

    function renderAttachments() {
      attRow.textContent = '';
      attRow.hidden = attachments.length === 0;
      attachments.forEach((path) => {
        const chip = el(doc, 'div', 'agent-att');
        const image = doc.createElement('img');
        image.alt = '待发图片';
        image.src = handlers.imageSrc ? handlers.imageSrc(path) : path;
        image.addEventListener('click', () => {
          if (handlers.openImage) handlers.openImage(path);
        });
        const drop = doc.createElement('button');
        drop.type = 'button';
        drop.className = 'agent-att-remove';
        drop.title = '移除';
        drop.setAttribute('aria-label', '移除这张图');
        drop.textContent = '×';
        drop.addEventListener('click', () => {
          attachments = attachments.filter((item) => item !== path);
          renderAttachments();
        });
        chip.appendChild(image);
        chip.appendChild(drop);
        attRow.appendChild(chip);
      });
    }

    function renderPop() {
      pop.textContent = '';
      const list = store.listOf(state);
      if (!list.length) {
        const empty = el(doc, 'div', 'agent-pop-empty');
        empty.textContent = '暂无对话';
        pop.appendChild(empty);
        return;
      }
      list.forEach((conversation) => {
        const row = el(doc, 'div', 'agent-row');
        if (state && conversation.id === state.activeId) row.classList.add('is-current');

        const pick = doc.createElement('button');
        pick.type = 'button';
        pick.className = 'agent-row-pick';
        pick.textContent = store.titleOf(conversation);
        pick.addEventListener('click', () => {
          pop.hidden = true;
          titleBtn.setAttribute('aria-expanded', 'false');
          if (handlers.onSwitch) handlers.onSwitch(conversation.id);
        });

        const rename = doc.createElement('button');
        rename.type = 'button';
        rename.className = 'agent-row-btn';
        rename.title = '重命名';
        rename.setAttribute('aria-label', '重命名');
        rename.appendChild(icon(doc, PATH_EDIT, 12));
        rename.addEventListener('click', () => startRename(row, conversation));

        const remove = doc.createElement('button');
        remove.type = 'button';
        remove.className = 'agent-row-btn is-danger-arm';
        remove.title = '删除（再点一次确认）';
        remove.setAttribute('aria-label', '删除会话');
        remove.appendChild(icon(doc, PATH_TRASH, 12));
        remove.addEventListener('click', () => {
          if (armedId === conversation.id) {
            clearArm();
            if (handlers.onDelete) handlers.onDelete(conversation.id);
            return;
          }
          clearArm();
          armedId = conversation.id;
          remove.classList.add('is-danger');
          armTimer = window.setTimeout(clearArm, DELETE_ARM_MS);
        });

        row.appendChild(pick);
        row.appendChild(rename);
        row.appendChild(remove);
        pop.appendChild(row);
      });
    }

    function startRename(row, conversation) {
      row.textContent = '';
      const input = doc.createElement('input');
      input.type = 'text';
      input.className = 'agent-row-input';
      input.value = String(conversation.title || '');
      let done = false;

      const finish = (commit) => {
        if (done) return;
        done = true;
        const value = input.value.trim();
        if (commit && value && handlers.onRename) {
          handlers.onRename(conversation.id, value);
        } else {
          renderPop();
        }
      };

      input.addEventListener('keydown', (event) => {
        if (event.key === 'Enter') {
          event.preventDefault();
          finish(true);
        } else if (event.key === 'Escape') {
          event.preventDefault();
          finish(false);
        }
      });
      input.addEventListener('blur', () => finish(false));

      row.appendChild(input);
      input.focus();
      input.select();
    }

    function bubble(role, text, imagePaths, ephemeral) {
      const wrap = el(doc, 'div', 'agent-msg-wrap is-' + role);
      const bubbleNode = el(doc, 'div', 'agent-msg');
      if (ephemeral) bubbleNode.classList.add('is-ephemeral');

      if (role === 'assistant') {
        const blocks = render.parseMarkdown(text);
        bubbleNode.appendChild(
          render.toDom(blocks, doc, { openImage: handlers.openImage }),
        );
      } else {
        if (text) {
          const paragraph = el(doc, 'p', 'md-p');
          paragraph.textContent = text;
          bubbleNode.appendChild(paragraph);
        }
        (imagePaths || []).forEach((path) => {
          const image = doc.createElement('img');
          image.className = 'md-image';
          image.alt = '图片';
          image.src = handlers.imageSrc ? handlers.imageSrc(path) : path;
          image.addEventListener('click', () => {
            if (handlers.openImage) handlers.openImage(path);
          });
          image.addEventListener('error', () => {
            const missing = doc.createElement('span');
            missing.className = 'md-missing';
            missing.textContent = render.MISSING_IMAGE;
            if (image.parentNode) image.parentNode.replaceChild(missing, image);
          });
          bubbleNode.appendChild(image);
        });
      }
      wrap.appendChild(bubbleNode);
      return wrap;
    }

    // 渲染窗口：**只画最近 N 条**（照参考实现 `INITIAL_MESSAGE_BATCH = 80`）。
    // 全量渲染长会话是卡的第二来源：每来一条新消息都要重解析全部 Markdown。
    const INITIAL_ROWS = 80;
    let rowLimit = INITIAL_ROWS;
    let renderedKey = '';

    function emptyState(title, hint) {
      const empty = el(doc, 'div', 'agent-empty');
      const heading = doc.createElement('h3');
      heading.textContent = title;
      empty.appendChild(heading);
      if (hint) {
        const paragraph = doc.createElement('p');
        paragraph.textContent = hint;
        empty.appendChild(paragraph);
      }
      messages.appendChild(empty);
    }

    function paint(message) {
      messages.appendChild(
        bubble(
          message.role === 'assistant' ? 'assistant' : 'user',
          message.text,
          message.imagePaths,
          false,
        ),
      );
    }

    /** 滚动到底：用 rAF，别在渲染同帧里读写 `scrollHeight`（会强制同步布局）。 */
    function scrollToBottomSoon() {
      window.requestAnimationFrame(() => {
        body.scrollTop = body.scrollHeight;
      });
    }

    /**
     * 同步消息区。**只在真有变化时才动 DOM**，且**只画最近 `rowLimit` 条**。
     *
     * 三个数字说明为什么这么写：
     * - 参考实现只渲染最近 80 条（`INITIAL_MESSAGE_BATCH`），不是全量；
     * - 全量渲染时，每来一条新消息都要重新解析**全部** Markdown 并重建 DOM；
     * - `scrollTop` 同步写会在渲染帧里触发一次强制布局，用 rAF 挪到下一帧。
     */
    function syncMessages() {
      const conversation = store.activeOf(state);
      if (!conversation) {
        messages.textContent = '';
        renderedKey = '';
        emptyState('还没有会话', '点右上角 + 新建一个，然后开始问。');
        return;
      }

      const all = conversation.messages;
      const start = Math.max(0, all.length - rowLimit);
      const visible = all.slice(start);
      const key =
        conversation.id + '|' + start + '|' + visible.map((message) => message.id).join(',');

      if (key === renderedKey) return; // 没有任何变化：一个字节都不动

      messages.textContent = '';
      if (!visible.length) {
        emptyState('有什么可以帮你？', '');
        renderedKey = key;
        return;
      }

      if (start > 0) {
        // 更早的消息不画：给一个明确的入口，而不是让用户以为历史没了
        const more = doc.createElement('button');
        more.type = 'button';
        more.className = 'agent-load-more';
        more.textContent = `显示更早的 ${start} 条`;
        more.addEventListener('click', () => {
          rowLimit += INITIAL_ROWS;
          renderedKey = '';
          syncMessages();
          body.scrollTop = 0;
        });
        messages.appendChild(more);
      }

      visible.forEach(paint);
      renderedKey = key;
      scrollToBottomSoon();
    }

    // ── 事件 ──

    titleBtn.addEventListener('click', () => {
      pop.hidden = !pop.hidden;
      titleBtn.setAttribute('aria-expanded', pop.hidden ? 'false' : 'true');
      if (!pop.hidden) renderPop();
    });

    // 「点别处关闭」：**必须具名**，否则 `destroy()` 摘不掉它 ——
    // 宿主反复 create/destroy（切页面、重开面板）时会一层层叠监听。
    function closePopOnOutsideClick(event) {
      if (pop.hidden) return;
      if (pop.contains(event.target) || titleBtn.contains(event.target)) return;
      pop.hidden = true;
      titleBtn.setAttribute('aria-expanded', 'false');
    }

    doc.addEventListener('click', closePopOnOutsideClick);

    newBtn.addEventListener('click', () => {
      pop.hidden = true;
      if (handlers.onNew) handlers.onNew();
    });
    closeBtn.addEventListener('click', () => {
      if (handlers.onClose) handlers.onClose();
    });

    importBtn.addEventListener('click', () => picker.click());
    picker.addEventListener('change', async () => {
      const files = Array.from(picker.files || []);
      picker.value = '';
      if (!files.length) return;
      if (handlers.onPickFiles) await handlers.onPickFiles(files);
    });

    textarea.addEventListener('paste', async (event) => {
      if (!handlers.onPasteImages) return;
      const paths = await handlers.onPasteImages(event);
      if (paths && paths.length) {
        event.preventDefault();
        addAttachment(paths);
      }
    });

    textarea.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
        event.preventDefault();
        submit();
      }
    });

    sendBtn.addEventListener('click', submit);

    function submit() {
      if (sending) return;
      if (handlers.onSend) handlers.onSend();
    }

    function addAttachment(paths) {
      const list = (paths || []).filter(Boolean);
      list.forEach((path) => {
        if (!attachments.includes(path)) attachments.push(path);
      });
      renderAttachments();
    }

    return {
      setState(next) {
        state = next;
        const conversation = store.activeOf(state);
        titleText.textContent = store.titleOf(conversation);
        syncMessages();
        if (!pop.hidden) renderPop();
      },
      setSending(next) {
        sending = Boolean(next);
        sendBtn.disabled = sending;
        thinking.hidden = !sending;
        if (sending) scrollToBottomSoon();
      },
      setError(text) {
        errorBar.textContent = text || '';
        errorBar.hidden = !text;
        if (text) body.scrollTop = body.scrollHeight;
      },
      /** 提交后立刻显示的用户气泡（还没落盘）；重建成 state 时会被自动覆盖。 */
      appendPending(text, imagePaths) {
        pendingNode = bubble('user', text, imagePaths || [], true);
        messages.appendChild(pendingNode);
        scrollToBottomSoon();
      },
      dropPending() {
        if (pendingNode && pendingNode.parentNode) pendingNode.parentNode.removeChild(pendingNode);
        pendingNode = null;
      },
      /** `not_sent` 时把内容放回输入框（含图片 chip）。 */
      restoreInput(snapshot) {
        if (!snapshot) return;
        textarea.value = snapshot.text || '';
        attachments = (snapshot.imagePaths || []).slice();
        renderAttachments();
        textarea.focus();
      },
      appendEphemeral(text) {
        messages.appendChild(bubble('assistant', text, [], true));
        scrollToBottomSoon();
      },
      getText() {
        return textarea.value;
      },
      clearInput() {
        textarea.value = '';
        attachments = [];
        renderAttachments();
      },
      addAttachment: addAttachment,
      attachments() {
        return attachments.slice();
      },
      setAttachments(paths) {
        attachments = (paths || []).slice();
        renderAttachments();
      },
      focus() {
        textarea.focus();
      },
      destroy() {
        window.clearTimeout(armTimer);
        doc.removeEventListener('click', closePopOnOutsideClick);
        mount.textContent = '';
      },
      root: root,
    };
  }

  return { create: create, DELETE_ARM_MS: DELETE_ARM_MS };
});
