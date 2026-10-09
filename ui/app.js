/**
 * lovart —— 前端逻辑。
 *
 * ── 三条设计约束 ────────────────────────────────────────────────────
 * ① **不重新实现业务规则**：校验走 Rust 的 `parse_prompts`，看板数据走
 *    `build_status_json`，与 CLI 是同一份代码。这里只负责画。
 * ② **轮询而不是事件**：状态文件是唯一真相，每秒重读一次最省事、不会漂移。
 * ③ **绝不自动开跑**：只有用户点了「发布任务」才会提交。
 *
 * ── 零内联样式（硬约束）──────────────────────────────────────────────
 * 本文件**不出现任何 `el.style.*`**，也不拼 `style="…"`：
 *   · 显示/隐藏 → `hidden` 属性；
 *   · 选中态 → `classList`；
 *   · 进度 → 纯 CSS 动画（本来就没有真实进度可显示，见 style.css 的说明）。
 */

const { invoke } = window.__TAURI__.core;

// ── 常量 ─────────────────────────────────────────────────────────────

const RATIOS = ['1:1', '3:4', '4:3', '16:9'];
const SIZES = ['1K', '2K', '4K'];

/**
 * ⚠️ **这里刻意不定义任何状态清单。**
 *
 * 分组（`group`）· 异常三件套（`abnormal`）· **可做的动作（`actions`）**·
 * 说完成却没图（`missingArtifact`）**全部由 Rust 下发**
 * （`run.rs` 的 `build_status_json`，判据一律读 `engine/state.rs` 的 `STATUS_TABLE`）。
 *
 * ⚠️ 曾经下发的是一个 `retryable` 布尔 —— 它与 `actions` 说的是同一件事
 * （"这条能不能重跑"），是**同一事实的两份投影**，已删；界面改读 `actions`。
 * 不要再往这个 payload 里加"某状态能不能做某事"的布尔。
 *
 * 更早这里还手写过 `IN_FLIGHT` / `ACTIVE` / `ABNORMAL` / `RETRYABLE` 四份集合 ——
 * 那是**第二份真源**：Rust 那边已经有一份，口径还得两边各自维护。
 * 本仓栽过的坑就是这个（两处同名不同口径，终端与窗口各说各话）。
 * 前端只负责画，不负责判。
 */

// ── 状态 ─────────────────────────────────────────────────────────────

// ⚠️ `refs` 的元素是 **`{ id, url, label }`**，不是裸字符串：提示词里的 `@` 芯片按 `id`
//    引用垫图（token 形如 `@{ref-3:猫.png}`）—— 只存 url 的话，"删掉第 2 张"会让后面
//    所有芯片**错位到别的图**上（芯片是文本里的固定 token，不会跟着数组下标走）。
const draft = { model: '', ratio: '3:4', size: '2K', refs: [] };

/** 垫图 id 计数器：**只增不减**（删一张不复用编号 ⇒ 已有芯片不会突然指到新图上）。 */
let refSeq = 0;
const queue = [];
let models = [];
let inboxFiles = [];
let doneOpen = false;
let queuedOpen = false;
let busy = false;

// ── 小工具 ───────────────────────────────────────────────────────────

const $ = (id) => document.getElementById(id);

/** 建元素。**只接受 class / text**，不接 style —— 这是零内联样式的守门人。 */
function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = String(text);
  return node;
}

function clear(node) {
  while (node.firstChild) node.removeChild(node.firstChild);
}

function show(node, visible) {
  node.hidden = !visible;
}

/**
 * 报告一次「用户主动发起的动作失败了」。
 *
 * ⚠️ 为什么不能静默：这些动作（复制 / 拖放 / 打开文件夹 / 读模型表）**都是用户点的**，
 * 静默 = 让他以为成功了。本仓头号伪装型债就是「失败伪装成成功」。
 *
 * 用弹窗而不是提示条，是因为提示条会被每秒一次的 `refreshBoard` 立刻清掉
 * （那一条只用于「读不到任务状态」这个会持续存在的状况）。
 */
function notice(message) {
  const box = el('div', 'toast', message);
  box.addEventListener('click', () => box.remove());
  document.body.appendChild(box);
  setTimeout(() => box.remove(), 8000);
}

/**
 * 应用内确认框 —— 返回 `Promise<boolean>`。
 *
 * ⚠️ **不能用 `window.confirm`**：Tauri 的 webview **不实现**它 —— `confirm()` 直接
 *    返回 `false` ⇒ 所有"确认后才继续"的动作**全部静默取消**
 *    （2026-10-01 实测：「确认并继续」点了没反应）。花钱的动作更不能押在它上面。
 */
function askConfirm(title, detail) {
  return new Promise((resolve) => {
    const overlay = el('div', 'modal');
    const panel = el('div', 'modal-panel');
    panel.appendChild(el('div', 'modal-title', title));
    if (detail) panel.appendChild(el('pre', 'modal-detail', detail));

    const row = el('div', 'modal-actions');
    const cancel = el('button', 'btn btn-sm btn-quiet', '取消');
    const ok = el('button', 'btn btn-sm btn-primary', '确定');
    cancel.type = 'button';
    ok.type = 'button';

    const close = (answer) => {
      overlay.remove();
      resolve(answer);
    };
    cancel.addEventListener('click', () => close(false));
    ok.addEventListener('click', () => close(true));
    overlay.addEventListener('click', (event) => {
      if (event.target === overlay) close(false);
    });

    row.appendChild(cancel);
    row.appendChild(ok);
    panel.appendChild(row);
    overlay.appendChild(panel);
    document.body.appendChild(overlay);
    ok.focus();
  });
}

// ── 看板 ─────────────────────────────────────────────────────────────

let lastSignature = '';

/**
 * 结构签名 —— **只有它变了才重建 DOM**。
 *
 * ⚠️ 刻意**不含「已跑多久」**：那个每秒都在变，含进去就等于每秒把整块列表
 * 拆了重建（⋯ 菜单一秒就没、滚动位置每秒被重置、图片每秒重新请求）。
 * 时长单独原地更新（见下面的 `[data-elapsed]`）。
 */
function boardSignature(data) {
  const tasks = data.tasks || [];
  const body = tasks
    .map((task) => {
      const present = (task.filesPresent || []).filter((entry) => entry.present).length;
      // ⚠️ `following` **必须进签名**：批次开始/结束时，任务状态一个字都没变，
      //    变的是「谁在被跟」。签名漏了它就等于进度条停不下来（结构没变 ⇒ 不重建 DOM）。
      // 垫图条数同理：换垫图重跑时状态可能原地不动，但卡片内容必须变。
      return `${task.id}:${task.status}:${present}:${task.following ? 'f' : '-'}:${(task.refUrls || []).length}`;
    })
    .join('|');
  return [
    tasks.length,
    body,
    data.runningBatch ? 'r' : '-',
    doneOpen ? 'D' : '-',
    queuedOpen ? 'Q' : '-',
    // 上游探针的结论也进签名：它到了 ⇒ 要重画（否则标题行永远停在「正在探测…」）。
    upstreamSummary(),
  ].join('#');
}

/**
 * 上游探针结果 —— **自动探测**，不靠人去查。
 *
 * ── 为什么前端必须自己探 ──────────────────────────────────────────
 * `state.jsonl` 里的 `running` 只说明「本地上次提交过」，**不说明上游现在是什么样**：
 * 可能早就出图了（本地没去取）、可能还在排队、也可能已被中止 —— 三种情况下一步动作
 * 完全不同。没有它，想知道答案只能**手工拿密钥去上游问一遍**；
 * **要人去查，就是设计缺陷。**
 *
 * `key` = 本批数据的指纹（id+status）：**同一批数据只探一次**，不跟着每秒的轮询打上游。
 */
let upstream = { key: '', byId: new Map(), error: '' };

function upstreamKey(data) {
  return (data.tasks || []).map((task) => `${task.id}:${task.status}`).join('|');
}

/** 有人没跟的未完成任务 ⇒ 探一次上游；都有人跟 / 没有未完成 ⇒ 不探。 */
async function probeUpstreamIfNeeded(data) {
  const tasks = data.tasks || [];
  const unfinished = tasks.filter((task) => task.group !== 'finished');
  const key = upstreamKey(data);
  const needed = unfinished.some((task) => !task.following);
  if (upstream.key === key) return; // 这版数据探过了
  upstream = { key, byId: new Map(), error: '' };
  if (!needed) return;
  try {
    const probes = await invoke('probe_unfinished');
    for (const probe of probes) upstream.byId.set(probe.id, probe);
  } catch (error) {
    // 探测失败**必须可见** —— 否则界面会拿"探不到"当"没事"（读不到 ≠ 没有）。
    upstream.error = String(error);
  }
}

/** 把探针结论压成一句人话（进了签名，所以只在真的变时重画）。 */
function upstreamSummary() {
  if (upstream.error) return `上游探测失败：${upstream.error}`;
  if (upstream.byId.size === 0) return '探测中…';
  const label = {
    done: '已出图',
    running: '仍在跑',
    abort: '已中止',
    pending_confirmation: '待确认',
  };
  const tally = new Map();
  for (const probe of upstream.byId.values()) {
    const key = probe.error ? '探测失败' : probe.upstream || '未提交过';
    tally.set(key, (tally.get(key) || 0) + 1);
  }
  return `上游：${[...tally].map(([key, n]) => `${label[key] || key} ${n}`).join('、')}`;
}

/**
 * 取走**尚未读过的**批次级告警并显示。
 *
 * ⚠️ 这是后端 `take_alerts` 的**唯一消费者** —— 与「有产无消」的旧 `get_log`
 * 相对（那次是：后端写了 300 条日志，前端一次都没调，于是批次失败在界面上完全不可见）。
 *
 * 取走即清（后端 `std::mem::take`），所以不会重复弹。
 * 多条合并成一次弹窗 —— 逐条弹会连点好几次「确定」，反而让人不去看内容。
 */
async function flushAlerts() {
  let alerts = [];
  try {
    alerts = await invoke('take_alerts');
  } catch (error) {
    // 取告警这条**通道本身**坏了：不能静默（否则告警机制一坏就回到「什么都看不见」）。
    // 但也**不能**再借告警通道报告 —— 那正是它坏掉的地方。
    $('boardNoticeText').textContent = `读不到告警通道：${error}`;
    show($('boardNotice'), true);
    return;
  }
  if (!alerts.length) return;
  notice(alerts.join('\n'));
}

async function refreshBoard() {
  let data;
  try {
    data = await invoke('get_state');
  } catch (error) {
    // 查7：读不到 ≠ 没有。保留错误 + 给重试，绝不渲染成空态。
    $('boardNoticeText').textContent = `读不到任务状态：${error}`;
    show($('boardNotice'), true);
    show($('boardMain'), false);
    return;
  }
  show($('boardNotice'), false);
  show($('boardMain'), true);

  // 批次级告警：**必须显示**，所以放在「结构没变就早退」之前 ——
  // 告警的出现与任务列表的结构无关（跑批失败时列表往往一点没变）。
  await flushAlerts();

  // **自动探测上游**（只读、同一批数据只探一次）：给「本地没跟的那几条」一个交代，
  // 而不是让用户（或维护者）自己拿密钥去上游问一遍。放在算签名之前 ——
  // 探到的结论要进签名，标题行才会跟着更新。
  await probeUpstreamIfNeeded(data);

  const signature = boardSignature(data);
  if (signature === lastSignature) {
    // 结构没变 ⇒ 只把「已跑多久」原地改掉
    for (const node of document.querySelectorAll('[data-elapsed]')) {
      const task = (data.tasks || []).find((item) => item.id === node.dataset.elapsed);
      if (task) node.textContent = `已持续 ${task.stateSinceText || '—'}`;
    }
    return;
  }
  lastSignature = signature;

  // 重建会丢滚动位置 —— 记下来再放回去
  const scroller = $('boardBody');
  const keepScroll = scroller.scrollTop;
  renderBoard(data);
  scroller.scrollTop = keepScroll;
}

/** 最近一次拿到的任务列表（面板头的「清理已结束」要用）。 */
let lastTasks = [];

function renderBoard(data) {
  const tasks = data.tasks || [];
  lastTasks = tasks;
  // 分组判据来自 Rust（见文件头的说明），前端不自己算。
  const running = tasks.filter((task) => task.group === 'active');
  const queued = tasks.filter((task) => task.group === 'queued');
  const finished = tasks.filter((task) => task.group === 'finished');

  // ⚠️ 「在跑」的判据**由后端给**（`get_state` 已把「磁盘未完成」与「本地有批次在跟」
  // 收口成一个 `running` 布尔）。前端**不再**从两个字段自己推 ——
  // 那正是「左边写 5 在跑、右边停止却点不了」的成因：两句话各来自一个来源。
  //
  // `running === false` 时，`active` 组里的那几条真实含义是
  // 「**上游可能还在生成，本地没跟**」，不是「正在跑」。
  const live = Boolean(data.running);

  // ⚠️ **逐条**看有没有人跟（`task.following` 由 Rust 下发，见 `get_state`）——
  //    批次级那个 `live` 不够用：批次在跑 ≠ 这几条都在被跟。
  //    残记录（上次被中断留下的 `running`，不在本批次的提示词里）就是"有批次在跑、
  //    但它永远没人查"的那种，必须在界面上被点出来，而不是混进「5 在跑」。
  const followed = running.filter((task) => task.following).length;
  // 没批次时，`active` + `queued` 全都是待续跑的；有批次时，只有没人跟的那几条要接。
  const stranded = live ? running.length - followed : running.length + queued.length;

  $('boardCount').textContent = `${tasks.length} 条`;
  $('runningTitle').textContent = live ? '处理中' : '未完成';
  // 本地没人跟的那几条：把**上游的真实情况**一并说出来（自动探测的结论）。
  // 只说「未跟进」只讲了本地半边 —— 用户真正要知道的是"那几条还有没有戏"。
  const upstreamNote = stranded > 0 ? ` · ${upstreamSummary()}` : '';
  $('runningMeta').textContent =
    (live
      ? `${followed} 在跑 · ${queued.length} 排队${stranded > 0 ? ` · ${stranded} 未跟进` : ''}`
      : `${running.length + queued.length} 条待续跑 · 本地未跟进`) + upstreamNote;
  $('runningDot').classList.toggle('dot-idle', !live);

  // 「接着查」= README 里「下次自动复查」的兑现按钮。
  // 只在**真有没人跟的**未完成记录时出现：正常跑批时它根本不显示，不会诱导乱点。
  show($('btnResume'), stranded > 0);
  $('btnResume').textContent = `接着查 ${stranded}`;

  // 在跑：卡片
  const runningList = $('runningList');
  clear(runningList);
  if (running.length === 0) {
    runningList.appendChild(el('div', 'empty', tasks.length ? '无进行中任务' : '暂无任务'));
  } else {
    for (const task of running) runningList.appendChild(taskCard(task));
  }

  // 「要你处理」—— **你不点它就不动**的那些（判据在 Rust：`task_group` 的 `waiting`）。
  // 刻意**排在最上面、默认展开**：它原先躺在「已结束」里，用户以为"它自己完成了"。
  const waiting = tasks.filter((task) => task.group === 'waiting');
  show($('waitingGroup'), waiting.length > 0);
  $('waitingMeta').textContent = `${waiting.length} 条`;
  const waitingList = $('waitingList');
  clear(waitingList);
  for (const task of waiting) waitingList.appendChild(taskCard(task));

  // 排队中：一个计数条
  show($('btnQueued'), queued.length > 0);
  $('queuedCount').textContent = queued.length;
  $('btnQueued').classList.toggle('is-open', queuedOpen);
  $('queuedHint').textContent = queuedOpen ? '收起' : '展开';
  const queuedList = $('queuedList');
  show(queuedList, queuedOpen && queued.length > 0);
  clear(queuedList);
  if (queuedOpen) for (const task of queued) queuedList.appendChild(taskCard(task));

  // 已完成
  // ⚠️ 判据用 Rust 下发的 `actions`（状态表那一列），不再读 `retryable` ——
  //    后者只是它的投影（`is_retryable` 现在就是"actions 里有没有 Retry"），
  //    留一份在 JSON 里给外部脚本，界面这边只认 `actions`。
  const retryable = finished.filter((task) => (task.actions || []).includes('retry'));
  $('doneCount').textContent = finished.length;
  $('btnDone').classList.toggle('is-open', doneOpen);
  $('btnDone').setAttribute('aria-expanded', String(doneOpen));
  show($('btnRetry'), retryable.length > 0);
  $('btnRetry').textContent = `重跑异常 ${retryable.length}`;

  const doneList = $('doneList');
  show(doneList, doneOpen && finished.length > 0);
  clear(doneList);
  if (doneOpen) {
    if (finished.length === 0) {
      doneList.appendChild(el('div', 'empty', '暂无已结束任务'));
    } else {
      for (const task of finished) doneList.appendChild(taskCard(task));
    }
  }

  // 「停止」只在**真的能停**的时候出现 —— 没有批次在跑时它是个死按钮，
  // 常驻只会让人以为「点了能停」（上一版就是这样：显示 5 在跑 + 灰按钮）。
  // 藏起来不是「少了功能」：那 5 条要的不是停止，是**续跑**（再发布一次就会续上）。
  const stop = $('btnStop');
  stop.hidden = !live;
  stop.disabled = !live;
  stop.textContent = '停止';
}

/** 这条能不能做某动作 —— **判据来自 Rust 下发的 `actions`**（状态表那一列），前端不判。 */
const canDo = (task, name) => (task.actions || []).includes(name);

/**
 * 「确认并继续」的完整流程（卡片上那个常显按钮用它）。
 *
 * **刻意两步**：① 先问上游「要花什么」（`preview_confirm`，**只读、不花钱**）；
 * ② 把上游原话摊给用户看，他明确点了「确定」才真的确认。
 * 官方要求就是这个顺序（先告诉花费、等他回答）—— 一步到位等于替他做了决定。
 */
async function confirmFlow(task) {
  try {
    const preview = await invoke('preview_confirm', { id: task.id });
    // Rust 那边刻意分成 `found` + `detail` 两件（不归一成一个 null）：
    // 「上游没给这个字段」与「上游给了个 null」是两回事，这里分开说。
    const shown = preview && preview.found
      ? JSON.stringify(preview.detail, null, 2)
      : '上游未返回确认明细';
    const ok = await askConfirm('将消耗积分，确认继续？', `上游原话：\n${shown}`);
    if (!ok) return;
    const note = await invoke('confirm_task', { id: task.id });
    if (note) notice(note);
    await refreshBoard();
  } catch (error) {
    notice(`确认失败：${error}`);
  }
}

/**
 * 任务卡（处理中 / 排队中 / 已完成**共用同一套**）。
 *
 * ⚠️ 这是**图片工具**，每条任务的主角是**那张图**。
 * 参照实现（maomao 的 TaskCenter）：缩略图**整行宽**、点击开大图，
 * 文字只是图下面的说明。做成「一行文字 + 32px 小方块」那是日志行，不是任务卡。
 */
function taskCard(task) {
  const card = el('div', 'task-card');

  // 顶行：状态点 + 状态文案 · 模型 | 操作
  const head = el('div', 'task-head');
  head.appendChild(el('span', `dot ${dotClass(task)}`));
  head.appendChild(el('span', 'task-status', task.statusLabel || task.status));
  head.appendChild(el('span', 'sep', '·'));
  head.appendChild(el('span', 'task-model', `${task.model} · ${task.aspectRatio} ${task.imageSize}`));

  const first = (task.filesPresent || []).find((entry) => entry.present);

  const actions = el('div', 'head-actions');
  actions.appendChild(iconButton('i-copy', '复制提示词', () => copyPrompt(task.prompt)));
  // ⚠️ ⋯ **所有任务都给**（含还没出图的）—— 原先这里是 `if (first)`：
  //    只有已经出了图的任务才有菜单 ⇒ 没图的未完成任务**一个操作入口都没有**。
  //    而「删除」恰恰是那些残记录最需要的东西（后端早就允许删，是界面把它藏了）。
  actions.appendChild(moreButton(task, first ? first.file : null));
  head.appendChild(actions);

  // 右侧一列：状态行 + 提示词 + 时间（+「已跑多久」）。
  const main = el('div', 'task-main');
  main.appendChild(head);
  main.appendChild(el('div', 'task-prompt', task.prompt));
  main.appendChild(el('div', 'task-time', formatTime(task.submittedAt || task.updatedAt)));

  // ── 事实行：这一条任务的**真实数据** ────────────────────────────────
  //
  // ⚠️ 规则是「**有就显示**」，**不按状态挑字段** —— 挑字段就是前端在做状态判断，
  //    而状态判断只许有一份（`engine/state.rs` 的 `STATUS_TABLE`）。
  //    数据本来就都在下发的记录里，原先**一个字都没显示**。
  // ⚠️ 卡片里**不画任何进度条、不留扫动动画**（旧的 `.bar` + `@keyframes sweep` 已删）：
  //    转的永远不是进度，只会看着像卡住。单条能给的真话是下面这几格。
  const facts = el('div', 'task-facts');

  // ① 时间 —— **只有没结束的才"走"**。
  //
  // ⚠️ 原先不问状态一律显示 `已持续 X`：已完成的任务照样一分一秒往上加，
  //    看起来像「还在跑」（2026-10-01 实测反馈）。
  //    `已持续` 的含义是"这条**当前状态**从何时开始" —— 对终态就是"结束到现在多久"，
  //    那不是用户要的信息。终态给**静态的结束时刻**。
  //    判据用 Rust 下发的 `group`（`finished` = 终态），前端不判状态。
  if (task.group === 'finished') {
    if (task.stateSince) {
      facts.appendChild(el('span', 'fact', `结束 ${formatTime(task.stateSince)}`));
    }
  } else if (task.stateSinceText && task.stateSinceText !== '—') {
    const since = el('span', 'fact', `已持续 ${task.stateSinceText}`);
    // data-elapsed：刷新时**原地**改这一格，不必重建整块 DOM
    since.dataset.elapsed = task.id;
    facts.appendChild(since);
  }
  // ② 试了几次 / 超时几次（1 次不算信息，0 次不显示）
  if (task.attempts > 1) facts.appendChild(el('span', 'fact', `第 ${task.attempts} 次尝试`));
  if (task.timeoutCount > 0) facts.appendChild(el('span', 'fact', `超时 ${task.timeoutCount} 次`));
  // ③ 产物文件名 —— 现在窗口里只看到图，**看不到它存成了什么名字**（终端看板有）
  for (const entry of task.filesPresent || []) {
    const name = entry.file.split('/').pop();
    facts.appendChild(
      el('span', `fact${entry.present ? '' : ' is-missing'}`,
        `${name}${entry.present ? '' : '（缺失）'}`),
    );
  }
  if (facts.childNodes.length) main.appendChild(facts);

  // 顶排：**垫图在最左**，文字列在右。
  //
  // ⚠️ 垫图（参考图）是任务发出去以后**唯一能核对"我当时垫的是哪张"**的东西 ——
  //    垫图选错，整张图就全错。所以它给到**卡片左上角、给大**，不加"垫图"两个字：
  //    结果图在下面整行宽、垫图在左上角是个方块，谁是谁一眼就分得出来。
  //
  // 地址来自记录里的 `refUrls`（`TaskRecord::ref_urls`），不是每次现算：
  //    原先记录里只有指纹，指纹是单向哈希、**反推不出图** ⇒ 界面上连看一眼都做不到。
  //    地址仍拿得到：悬停缩略图看 tooltip，或 ⋯ 菜单里的「复制垫图地址」。
  // 没有 `refUrls` 就不渲染这一列（文生图不该多出一块空白）。
  const refUrls = task.refUrls || [];
  const top = el('div', 'task-top');
  if (refUrls.length) {
    const refs = el('div', 'task-refs');
    for (const url of refUrls) {
      const item = el('button', 'task-ref');
      item.type = 'button';
      item.title = `垫图：${url}\n（点击看大图）`;
      const img = document.createElement('img');
      img.src = url;
      img.alt = '垫图';
      img.loading = 'lazy';
      item.appendChild(img);
      item.addEventListener('click', () => openViewerRemote(url, task.prompt));
      refs.appendChild(item);
    }
    top.appendChild(refs);
  }
  top.appendChild(main);
  card.appendChild(top);

  // 异常：带底色的说明块（文案来自 Rust，正常状态为 null ⇒ 不渲染）。
  // ⚠️ 只渲染**一句人话** —— 状态名在卡片头部，这里不再重复一遍。
  const bad = task.abnormal;
  if (bad) {
    const box = el('div', `task-error is-${bad.kind}`);
    box.appendChild(el('span', '', bad.reason));
    card.appendChild(box);
  }

  // 上游原文 —— **原先一个字都没显示**：出问题时你只看到一句人话（`abnormal.reason`），
  // 看不到**上游到底说了什么**。人话是给用户看的，这一句是证据。
  // 规则同上：**有就显示**，不按状态挑。
  if (task.error) {
    // **默认收起**：它是证据、不是正文。常显会把卡片撑满，而"人话"那句已经在上面了。
    const raw = el('details', 'task-raw');
    raw.appendChild(el('summary', 'raw-label', '上游原文'));
    raw.appendChild(el('code', 'raw-text', task.error));
    card.appendChild(raw);
  }

  // 「确认并继续」—— **常显在卡上**，不藏 ⋯ 菜单：
  //    它是**花钱的那一下**，藏起来就等于"没有确认按钮"（2026-10-01 实测反馈）。
  //    判据来自 Rust 下发的 `actions`（状态表那一列），前端不判。
  if (canDo(task, 'confirm')) {
    const row = el('div', 'task-actions');
    const go = el('button', 'btn btn-sm btn-primary', '消费积分继续');
    go.type = 'button';
    go.addEventListener('click', () => confirmFlow(task));
    row.appendChild(go);
    card.appendChild(row);
  }

  // 缩略图 —— **整行宽**，查看大图。这是「怎么看图片」的入口。
  if (first) {
    const thumb = el('button', 'task-thumb');
    thumb.type = 'button';
    thumb.title = '查看大图';
    const img = document.createElement('img');
    img.src = toFileUrl(first.file);
    img.alt = '';
    img.loading = 'lazy';
    thumb.appendChild(img);
    thumb.addEventListener('click', () => openViewer(first.file, task.prompt));
    card.appendChild(thumb);
  } else if (task.missingArtifact) {
    // 说完成却没图 —— 不许静默留白（查7：读不到 ≠ 没有）。
    // ⚠️ 判据是 Rust 下发的 `missingArtifact`（表列 `not_done` 算出来的），
    //    不是前端比状态字面量（原先写的是 `task.status === 'done'` ——
    //    "done 是什么"不该由前端知道）。
    const thumb = el('div', 'task-thumb');
    const holder = el('div', 'placeholder');
    holder.appendChild(svgIcon('i-img', 'icon'));
    thumb.appendChild(holder);
    card.appendChild(thumb);
  }

  return card;
}

/** 状态点配色 —— 判据全部来自 Rust 下发的 `group` / `abnormal`。 */
function dotClass(task) {
  // 占着槽位但**没人跟** ⇒ 蓝点会撒谎（蓝点 = 正在跑），退成灰点与标题行一致。
  if (task.group === 'active') return task.following ? '' : 'dot-idle';
  if (task.group === 'queued') return 'dot-idle';
  if (task.abnormal) return task.abnormal.kind === 'bad' ? 'dot-bad' : 'dot-warn';
  return 'dot-ok';
}

function svgIcon(symbolId, className) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('class', className);
  const use = document.createElementNS('http://www.w3.org/2000/svg', 'use');
  use.setAttribute('href', `#${symbolId}`);
  svg.appendChild(use);
  return svg;
}

function iconButton(symbolId, title, onClick) {
  const button = el('button', 'icon-btn');
  button.type = 'button';
  button.title = title;
  button.appendChild(svgIcon(symbolId, ''));
  button.addEventListener('click', onClick);
  return button;
}

/**
 * ⋯ 菜单：**常显入口**（查8），动作不藏在 hover 里。
 *
 * ⚠️ 关闭监听**全局只挂一次**（见文件末尾的 `document.addEventListener`）。
 * 曾经这里每建一张卡就 `document.addEventListener('click', close)` ——
 * 而列表**每秒重建**，一分钟能挂上千个监听器，全都不回收。
 */
let openMenu = null;

function closeMenu() {
  if (openMenu) {
    openMenu.remove();
    openMenu = null;
  }
}

function moreButton(task, file) {
  const anchor = el('div', 'menu-anchor');
  const button = el('button', 'icon-btn');
  button.type = 'button';
  button.title = '更多操作';
  button.appendChild(svgIcon('i-more', ''));
  anchor.appendChild(button);

  button.addEventListener('click', (event) => {
    // 必须拦住冒泡，否则立刻被全局那个 closeMenu 关掉
    event.stopPropagation();
    if (openMenu) {
      closeMenu();
      return;
    }
    const menu = el('div', 'menu');
    // 「打开所在位置」只在**真有图**时给 —— 没有文件时点了也只会报错。
    if (file) {
      menu.appendChild(menuItem('打开所在位置', () => {
        closeMenu();
        invoke('reveal_path', { path: file }).catch((error) => notice(`打开所在位置失败：${error}`));
      }));
    }
    menu.appendChild(menuItem('复制提示词', () => {
      closeMenu();
      copyPrompt(task.prompt);
    }));
    // threadId —— 报问题 / 去上游核对时要的就是它。放菜单里，不占卡片版面。
    if (task.threadId) {
      menu.appendChild(menuItem('复制 threadId', () => {
        closeMenu();
        copyPrompt(task.threadId);
      }));
    }
    const actions = [];
    // 垫图地址：**"我需要能查"** 的出口 —— 复制走，粘到哪儿都能打开。
    if ((task.refUrls || []).length) {
      actions.push(menuItem('复制垫图地址', () => {
        closeMenu();
        copyPrompt((task.refUrls || []).join('\n'));
      }));
    }
    // 「重跑 / 接着查」**由 Rust 下发的 `actions` 决定**（状态表那一列）——
    // 前端不再自己判"这个状态能不能重跑"（原先读的是 `task.retryable`）。
    // 两者的差别是钱：重跑 = 重新生成（再花一次额度）；接着查 = 不重复提交。
    //
    // ⚠️ 「确认并继续」**不在这里** —— 它是**花钱的那一下**，必须常显在卡上
    //    （见 `taskCard` 里的 `.task-actions` 按钮）：藏进 ⋯ 菜单等于"没有按钮"
    //    （2026-10-01 实测反馈）。**一个动作只留一个入口。**
    if (canDo(task, 'retry')) {
      actions.push(menuItem('重跑这一条', () => {
        closeMenu();
        invoke('retry_one', { id: task.id }).catch((error) => notice(`重跑失败：${error}`));
      }));
    }
    if (canDo(task, 'resume')) {
      actions.push(menuItem('接着查这一条', () => {
        closeMenu();
        invoke('resume_one', { id: task.id })
          .then(() => refreshBoard())
          .catch((error) => notice(`接着查失败：${error}`));
      }));
    }
    // 删除入口对**所有**任务开放。
    //
    // 原先后端拒绝删未完成的（理由「删了会复活」），前端也就干脆不给入口 —— 结果：
    // 程序重启后 `state.jsonl` 里留下的 `running` 残记录，**既显示成「处理中」、又删不掉**。
    // 而复活只在跑批时发生，跑批时本来就不许删 ⇒ 那道闸只剩下「让你收拾不掉烂摊子」。
    //
    // 代价（上游可能还在生成、那张图会变成孤儿）放在**确认框**里说，
    // 而不是靠「不给按钮」把它藏起来 —— 藏起来用户只会以为程序坏了。
    actions.push(menuItem('删除这条', async () => {
      closeMenu();
      if (task.group !== 'finished' && !(await askConfirm(
        '任务未结束',
        '删除后本地不再获取该任务产物；上游可能仍在生成。',
      ))) return;
      removeTasks([task.id], '删除');
    }, true));
    if (actions.length) {
      menu.appendChild(el('div', 'menu-sep'));
      for (const item of actions) menu.appendChild(item);
    }

    anchor.appendChild(menu);
    openMenu = menu;
  });

  return anchor;
}

/**
 * 删任务。**只删记录，不动磁盘上的图。**
 *
 * ⚠️ 后端只有一道闸（**跑批中不许删** —— 删除要重写整个状态文件，和跑批写的是同一个），
 * 失败会带原因回来，这里必须显示出来、不能吞掉：删不掉而界面看着像删掉了，是最坏的一种。
 */
async function removeTasks(ids, label) {
  if (!ids.length) return;
  try {
    await invoke('remove_tasks', { ids });
  } catch (error) {
    notice(`${label}失败：${error}`);
  }
  await refreshBoard();
}

function menuItem(label, onClick, danger) {
  const button = el('button', `menu-item${danger ? ' is-danger' : ''}`, label);
  button.type = 'button';
  button.addEventListener('click', onClick);
  return button;
}

// ── 看大图 ───────────────────────────────────────────────────────────

let viewerFile = '';
let viewerPrompt = '';

function openViewer(file, prompt) {
  openViewerSrc(toFileUrl(file), file, prompt);
}

/** 看**网上**的图（垫图在 Lovart CDN 上）。传不了本地路径 ⇒ 禁用「打开所在位置」。 */
function openViewerRemote(url, prompt) {
  openViewerSrc(url, '', prompt);
}

function openViewerSrc(src, file, prompt) {
  viewerFile = file;
  viewerPrompt = prompt || '';
  $('viewerImg').src = src;
  $('viewerDims').textContent = '';
  $('viewer').classList.remove('is-actual');
  $('viewerFit').classList.add('is-on');
  $('viewerActual').classList.remove('is-on');
  // ⚠️ 没有本地文件时必须**禁用**这个按钮：点了只会报错，比没有按钮更糟。
  $('viewerReveal').disabled = !file;
  show($('viewer'), true);
}

function closeViewer() {
  show($('viewer'), false);
  $('viewerImg').removeAttribute('src');
}

async function copyPrompt(text) {
  try {
    await navigator.clipboard.writeText(text || '');
  } catch (error) {
    // ⚠️ 原先这里是空 `catch`（注释写「不是关键路径」）—— 但对用户来说**是**：
    //    他点了「复制提示词」，以为复制好了，粘出来却是旧内容。
    //    失败必须说，只是不必用弹窗打断（用提示条）。
    notice(`复制失败（剪贴板不可用）：${error}`);
  }
}

function formatTime(iso) {
  if (!iso) return '';
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return String(iso);
  const pad = (value) => String(value).padStart(2, '0');
  return `${date.getMonth() + 1}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

/**
 * 本机绝对路径 → 能让 `<img>` 加载的 URL。
 *
 * ⚠️ **必须走 `convertFileSrc`，不能自己拼 `file://`** ——
 * Tauri v2 的 webview **默认禁用 `file://`**（安全策略），拼出来的地址
 * 图片一律加载失败、且不报错，只是白板。`convertFileSrc` 走的是
 * `asset:` 协议，配合 `tauri.conf.json` 里的 `assetProtocol.enable` 才通。
 */
function toFileUrl(path) {
  return window.__TAURI__.core.convertFileSrc(path);
}

/**
 * 垫图的可显示地址（三种形态都要认）。
 *
 * ⚠️ `data:`（文件选择器给）· `http(s):`（网址）· **本机绝对路径**（拖进窗口给的是路径）。
 *    原先只认 `data:` ⇒ **拖进来的垫图在界面上是一块空白**（缩略图渲染不出来），
 *    用户根本看不到自己垫的是哪张 —— 那条抱怨就是从这儿来的。
 *    本机路径必须走 `convertFileSrc`（[`toFileUrl`]），不能自己拼 `file://`：
 *    webview 默认禁用 `file://`，拼出来的地址图片一律白板、且不报错。
 */
function refSrc(ref) {
  if (/^(data:|https?:)/.test(ref)) return ref;
  return toFileUrl(ref);
}

// ── 待发队列 ─────────────────────────────────────────────────────────

function renderQueue() {
  $('queueCount').textContent = `${queue.length} 条`;
  $('btnPublish').disabled = queue.length === 0 || busy;

  const list = $('queueList');
  clear(list);
  if (queue.length === 0) {
    list.appendChild(el('div', 'empty', '待发为空'));
    return;
  }
  queue.forEach((item, index) => {
    const row = el('div', 'queue-row');

    // **左上角那块 = 这张要发的垫图本身**（没有垫图才退回占位图标）。
    //
    // ⚠️ 原先左边永远是个占位图标，垫图只在下面写一句「垫图 1 张」——
    //    等于让用户在**发布前看不见自己垫的是哪张**。而垫图选错、整张图就全错，
    //    这是待发这一步唯一该核对的东西 ⇒ 给到左上角、给大、**一个字也不写**。
    const thumb = el('div', 'queue-thumb');
    const firstRef = (item.refs || [])[0];
    if (firstRef) {
      thumb.classList.add('is-ref');
      const img = document.createElement('img');
      img.src = refSrc(firstRef.url);
      img.alt = '垫图';
      img.loading = 'lazy';
      if (item.refs.length > 1) {
        img.title = `这条有 ${item.refs.length} 张垫图，这里显示的是第一张`;
      }
      thumb.appendChild(img);
    } else {
      thumb.appendChild(svgIcon('i-img', 'icon'));
    }
    row.appendChild(thumb);

    const text = el('div', 'task-text');
    text.appendChild(el('div', 'task-prompt', item.prompt));
    // ⚠️ 「垫图 N 张」这几个字**已删**：图就在左边摆着，再写一遍是重复。
    text.appendChild(el('div', 'task-reason', [item.model, item.ratio, item.size].join(' · ')));
    row.appendChild(text);

    // 查8：删除键**常显**（不做 hover 才出现）
    const remove = el('button', 'queue-remove', '×');
    remove.title = '从待发移除';
    remove.addEventListener('click', () => {
      queue.splice(index, 1);
      renderQueue();
    });
    row.appendChild(remove);
    list.appendChild(row);
  });
}

function addToQueue() {
  const input = $('promptInput');
  const chips = window.PromptChips;
  // 输入框是**富文本**（含 `@` 芯片）⇒ 先序列化成 `@{id:label}` 串，再解析成
  // 「纯文本 + 参考图」。芯片在文本里的位置换成 `[img=图片N]`（N = 首次出现顺序）。
  const resolved = chips.resolvePromptChips(chips.serializeDOM(input), draft.refs);
  if (!resolved.text) return;
  const labelOf = (url) => {
    const hit = draft.refs.find((ref) => ref.url === url);
    return hit ? hit.label : '';
  };
  queue.push({
    prompt: resolved.text,
    model: draft.model,
    ratio: draft.ratio,
    size: draft.size,
    // ⚠️ **顺序是语义**：`@` 芯片引用的在前（对应文本里的「图片N」编号），其余垫图在后；
    //    按 url 去重（同一张图既是芯片又整张垫着，只算一次）。
    refs: chips
      .mergeReferenceImageUrls(resolved.refImages, draft.refs)
      .map((url) => ({ url, label: labelOf(url) })),
  });
  clear(input);
  closeMentions();
  input.focus();
  renderQueue();
}

// ── 垫图 ─────────────────────────────────────────────────────────────

/** 垫图的可读名字（芯片上显示、`@` 弹层里显示）。 */
function refLabelOf(url, index) {
  if (/^https?:/.test(url)) {
    const name = url.split('?')[0].split('/').pop();
    if (name) return decodeURIComponent(name);
  }
  if (/^data:/.test(url)) return `图片${index}`;
  const name = url.split('/').pop() || '';
  return name.replace(/\.[^.]+$/, '') || `图片${index}`;
}

/** 加垫图：**按 url 去重**（同一张图加两次没有意义，还会占两份参考图额度）。 */
function addRefs(list) {
  for (const url of list) {
    if (typeof url !== 'string' || !url) continue;
    if (draft.refs.some((ref) => ref.url === url)) continue;
    refSeq += 1;
    draft.refs.push({
      id: `ref-${refSeq}`,
      url,
      label: refLabelOf(url, draft.refs.length + 1),
    });
  }
  renderRefs();
  renderMentions(); // `@` 弹层正开着的话，那份列表要跟着更新
}

function renderRefs() {
  const box = $('refs');
  clear(box);
  for (const ref of draft.refs) {
    const node = el('div', 'ref');
    // ⚠️ 走 [`refSrc`]：拖进来的垫图是**本机路径**，原来只认 `data:` ⇒ 一片空白。
    const img = document.createElement('img');
    img.src = refSrc(ref.url);
    img.alt = ref.label;
    img.loading = 'lazy';
    node.appendChild(img);

    // 查8：删除键**常显**
    const remove = el('button', 'ref-remove', '×');
    remove.title = '移除参考图';
    remove.addEventListener('click', () => {
      // ⚠️ 按 **id** 删，不是按下标：提示词里可能已经有指向它的芯片，
      //    按下标删会让"删掉的那张"与"芯片指的那张"错开（静默指错图）。
      const at = draft.refs.findIndex((item) => item.id === ref.id);
      if (at >= 0) draft.refs.splice(at, 1);
      removeChipsById(ref.id); // 引用它的芯片一并删（理由见 removeChipsById）
      renderRefs();
      renderMentions();
    });
    node.appendChild(remove);
    box.appendChild(node);
  }

  // 「+」和缩略图一起换行 —— 垫图可以有任意多张，它必须跟在最后而不是被挤出去。
  const add = el('button', 'ref-add', '+');
  add.type = 'button';
  add.title = '添加参考图（可多选）';
  add.addEventListener('click', () => $('refPicker').click());
  box.appendChild(add);
}

// ── 提示词富文本：`@` 调出垫图 → 行内芯片 ──────────────────────────────
//
// 交互与格式**整套**照搬 maomao 生图节点的 `PromptInput` + `promptMention`
// （见 `ui/promptChips.js` / `ui/promptMention.js` 的文件头）。这里只做 DOM 与事件 ——
// 「该不该弹」「弹在哪」「怎么序列化」全在那两个纯逻辑文件里，
// **不许在本文件另写一份 `lastIndexOf('@')` 或自己算坐标**（两份必然漂移）。
//
// 与那边逐条对齐的几件事（都是踩出来的，不是装饰）：
//   · 弹层 **portal 到 body + `position: fixed`** ⇒ 不被面板的 `overflow` 裁掉、不被盖住；
//   · 锚点是「**`@` 字符的矩形**」，默认**向上弹**（底边贴 `@` 行上方 4px），上方不够才翻到下方；
//   · `Esc` 关掉后**记住那个 `@`**（`dismissedAtIndex`）—— 同一处不再反复弹；
//   · **输入法组字中不判定** —— 中文输入不会被候选弹层打断；
//   · 打字会按 `@` 后面已输入的字**过滤候选**（`@` + 名字前缀）；
//   · `Backspace` / `Delete` 贴着芯片时删的是**整个芯片**，不是半个 token；
//   · 粘贴只取纯文本，且 `stopPropagation`。

let mentionOpen = false;
let mentionQuery = '';
let mentionIndex = 0;
let mentionPos = null;
/** 本次弹层对应的 `@` 下标 —— `Esc` 关掉后记进 `dismissedAtIndex`。 */
let mentionAtIndex = -1;
let dismissedAtIndex = -1;
/** 打开弹层那一刻的选区：点候选前先恢复它，芯片才会落在 `@` 的位置。 */
let savedRange = null;
let composing = false;

/** 候选素材（本工具的素材池 = 已加的垫图）。 */
function mentionAssets() {
  return draft.refs.map((ref) => ({ id: ref.id, label: ref.label, url: ref.url }));
}

/** 按 `@` 后面已输入的字过滤（大小写不敏感；空 query = 全部）。 */
function mentionCandidates() {
  const all = mentionAssets();
  const query = mentionQuery.trim().toLowerCase();
  return query ? all.filter((item) => item.label.toLowerCase().includes(query)) : all;
}

/**
 * 占位符显隐。
 *
 * ⚠️ `contentEditable` **没有 placeholder** ⇒ 用一个绝对定位的兄弟节点当占位符。
 *    判据是**序列化后的文本**，不是 `innerHTML` —— 只剩一个空 `<br>` 或零宽空格时，
 *    `innerHTML` 非空但内容其实是空的（那时占位符必须继续显示）。
 */
function syncPromptPlaceholder() {
  show($('promptPlaceholder'), window.PromptChips.serializeDOM($('promptInput')) === '');
}

/** 取「`@` 字符」的视口矩形（fixed 弹层的锚点）；找不到时退回光标矩形。 */
function mentionAnchorRect() {
  const selection = window.getSelection();
  if (!selection || selection.rangeCount === 0) return null;
  const range = selection.getRangeAt(0);
  let rect = null;
  if (range.collapsed && range.startContainer.nodeType === Node.TEXT_NODE) {
    const text = range.startContainer.textContent || '';
    const at = text.slice(0, range.startOffset).lastIndexOf('@');
    if (at >= 0) {
      const charRange = document.createRange();
      charRange.setStart(range.startContainer, at);
      charRange.setEnd(range.startContainer, at + 1);
      rect = charRange.getBoundingClientRect();
    }
  }
  if (!rect || (rect.width === 0 && rect.height === 0)) rect = range.getBoundingClientRect();
  return rect ? { top: rect.top, bottom: rect.bottom, left: rect.left } : null;
}

/** 判定该不该弹 + 该弹在哪（两个判定都在 `promptMention.js` 里，这里只做接线）。 */
function detectMention() {
  const input = $('promptInput');
  if (!input || composing) return;
  const selection = window.getSelection();
  if (!selection || selection.rangeCount === 0) return closeMentions();
  const range = selection.getRangeAt(0);
  const node = range.startContainer;
  // 光标不在文本节点里（落在芯片 / `<br>` / 元素边界）⇒ 没有 `@` 可判
  if (!node || node.nodeType !== Node.TEXT_NODE) return closeMentions();

  const before = (node.textContent || '').slice(0, range.startOffset);
  const found = window.PromptMention.detectMentionQuery(before);
  if (!found.active || mentionAssets().length === 0) return closeMentions();
  if (found.atIndex === dismissedAtIndex) return; // Esc 关过的那个 `@` 不再重弹

  dismissedAtIndex = -1;
  const anchor = mentionAnchorRect();
  mentionPos = anchor ? window.PromptMention.computeMentionPlacement(anchor) : null;
  mentionAtIndex = found.atIndex;
  mentionQuery = found.query;
  mentionIndex = 0;
  savedRange = range.cloneRange();
  mentionOpen = true;
  renderMentions();
}

/**
 * 渲染弹层（位置 + 列表）。
 *
 * ⚠️ 弹层 portal 到 `body` 且 `position: fixed` ⇒ 坐标是**视口坐标**，
 *    所以定位规则用 `--mention-*` 自定义属性交给 `style.css`（不写 `style.left/top`，
 *    也不受 `position: absolute` 相对谁的影响）。
 */
function renderMentions() {
  const menu = $('mentionMenu');
  if (!mentionOpen || !mentionPos) {
    show(menu, false);
    return;
  }
  const mention = window.PromptMention;
  menu.style.setProperty('--mention-w', `${mention.MENTION_PANEL_W}px`);
  menu.style.setProperty('--mention-x', `${mentionPos.left}px`);
  menu.style.setProperty('--mention-max-h', `${mentionPos.height}px`);
  menu.style.setProperty('--mention-top', mentionPos.top == null ? 'auto' : `${mentionPos.top}px`);
  menu.style.setProperty(
    '--mention-bottom',
    mentionPos.bottom == null ? 'auto' : `${mentionPos.bottom}px`,
  );
  menu.dataset.placement = mentionPos.placement;

  clear(menu);
  const list = mentionCandidates();
  if (list.length === 0) {
    menu.appendChild(el('div', 'mention-empty', '无匹配垫图'));
    show(menu, true);
    return;
  }
  for (const [index, item] of list.entries()) {
    const row = el('div', `mention-item${index === mentionIndex ? ' is-on' : ''}`);
    const thumb = el('div', 'mention-thumb');
    const img = document.createElement('img');
    img.src = refSrc(item.url);
    img.alt = '';
    thumb.appendChild(img);
    row.appendChild(thumb);
    row.appendChild(el('span', 'mention-label', item.label));
    row.addEventListener('mouseenter', () => {
      mentionIndex = index;
      renderMentions();
    });
    // ⚠️ `mousedown` 而非 `click`：`click` 之前输入框先失焦、选区就没了
    //    （芯片会插到输入框开头，而不是用户打 `@` 的地方）。
    row.addEventListener('mousedown', (event) => {
      event.preventDefault();
      pickMention(item);
    });
    menu.appendChild(row);
  }
  show(menu, true);
}

function closeMentions() {
  mentionOpen = false;
  mentionQuery = '';
  mentionIndex = 0;
  mentionPos = null;
  savedRange = null;
  show($('mentionMenu'), false);
}

/** 选中候选：先恢复打开弹层时的选区、删掉 `@query` 那段，再把芯片插进去。 */
function pickMention(item) {
  const selection = window.getSelection();
  if (selection && savedRange) {
    selection.removeAllRanges();
    selection.addRange(savedRange);
  }
  const range = selection && selection.rangeCount ? selection.getRangeAt(0) : null;
  if (range && range.startContainer.nodeType === Node.TEXT_NODE) {
    const cursor = range.startOffset;
    const text = range.startContainer.textContent || '';
    const at = text.slice(0, cursor).lastIndexOf('@');
    if (at >= 0) {
      const del = document.createRange();
      del.setStart(range.startContainer, at);
      del.setEnd(range.startContainer, cursor);
      del.deleteContents();
      del.collapse(true);
      selection.removeAllRanges();
      selection.addRange(del);
    }
  }
  insertChipAtCursor(item);
  closeMentions();
  syncPromptPlaceholder();
}

/** 把芯片插到**当前光标处**（光标不在编辑器里就插到末尾）。 */
function insertChipAtCursor(item) {
  const input = $('promptInput');
  const selection = window.getSelection();
  let range = selection && selection.rangeCount ? selection.getRangeAt(0) : null;
  if (!range || !input.contains(range.startContainer)) {
    range = document.createRange();
    range.selectNodeContents(input);
    range.collapse(false);
    if (selection) {
      selection.removeAllRanges();
      selection.addRange(range);
    }
  }
  const chip = window.PromptChips.buildChipEl(item.id, item.label, refSrc(item.url));
  range.insertNode(chip);
  // 芯片前补光标落点（行首芯片否则点不进去），芯片后把光标挪过去
  window.PromptChips.ensureCaretSlotBeforeChip(chip);
  range.setStartAfter(chip);
  range.collapse(true);
  if (selection) {
    selection.removeAllRanges();
    selection.addRange(range);
  }
  input.focus();
}

/** `Backspace`/`Delete` 贴着芯片 ⇒ 删**整个芯片**（返回是否删了）。 */
function deleteChipNearCursor() {
  const chips = window.PromptChips;
  const selection = window.getSelection();
  if (!selection || selection.rangeCount === 0) return false;
  const range = selection.getRangeAt(0);
  if (!range.collapsed) return false;
  const node = range.startContainer;
  const offset = range.startOffset;
  if (node.nodeType === Node.TEXT_NODE && offset === 0) {
    const previous = node.previousSibling;
    if (previous && chips.isChipEl(previous)) {
      previous.remove();
      return true;
    }
  }
  if (
    node.nodeType === Node.ELEMENT_NODE &&
    offset > 0 &&
    chips.isChipEl(node.childNodes[offset - 1])
  ) {
    node.childNodes[offset - 1].remove();
    return true;
  }
  if (
    node.nodeType === Node.TEXT_NODE &&
    offset === (node.textContent || '').length &&
    chips.isChipEl(node.nextSibling)
  ) {
    node.nextSibling.remove();
    return true;
  }
  return false;
}

/**
 * 垫图被移除 ⇒ 富文本里引用它的芯片**一并删掉**。
 *
 * 不删的话，富文本里会残留一个指向已删垫图的芯片（缩略图还在、其实已经没了），
 * 发出去时被 `resolvePromptChips` 判成"找不到素材"而**静默消失** —— 用户以为垫了图。
 */
function removeChipsById(id) {
  const input = $('promptInput');
  if (!input) return;
  let changed = false;
  for (const chip of Array.from(input.querySelectorAll(`[data-ref-id="${id}"]`))) {
    chip.remove();
    changed = true;
  }
  if (changed) {
    window.PromptChips.normalizeChipSlots(input);
    syncPromptPlaceholder();
  }
}

/** 拖进窗口 = 加垫图（不用瞄准某个小方框）。 */
async function wireDropZone() {
  try {
    const { getCurrentWebviewWindow } = window.__TAURI__.webviewWindow;
    await getCurrentWebviewWindow().onDragDropEvent((event) => {
      if (event.payload.type === 'drop') addRefs(event.payload.paths || []);
    });
  } catch (error) {
    // ⚠️ 原先这里是空 `catch`（注释写「不影响其它功能」）—— 但它影响的是
    //    **垫图**：拖放通道拿不到时，用户把图拖进窗口**什么都不会发生**，
    //    而界面上的「添加图片」按钮还在 ⇒ 用户会以为拖放是支持的。
    //    必须说清「拖放不可用，请用按钮」。
    notice(`拖放垫图不可用（请用「添加图片」按钮）：${error}`);
  }
}

// ── 分段控件 ─────────────────────────────────────────────────────────

function pickRatio(value) {
  draft.ratio = value;
  renderSeg('ratioSeg', RATIOS, draft.ratio, pickRatio);
}

function pickSize(value) {
  draft.size = value;
  renderSeg('sizeSeg', SIZES, draft.size, pickSize);
}

function renderSeg(id, values, current, onPick) {
  const box = $(id);
  clear(box);
  for (const value of values) {
    const button = el('button', value === current ? 'is-on' : '', value);
    button.type = 'button';
    button.addEventListener('click', () => onPick(value));
    box.appendChild(button);
  }
}

// ── 弹窗 ─────────────────────────────────────────────────────────────

function openMask(id) { show($(id), true); }
function closeMask(id) { show($(id), false); }

// ── 动作 ─────────────────────────────────────────────────────────────

async function publish() {
  if (busy || queue.length === 0) return;
  closeMask('maskPublish');
  busy = true;
  renderQueue();
  try {
    await invoke('publish_queue', { tasks: queue.map((item) => ({
      prompt: item.prompt,
      model: item.model,
      ratio: item.ratio,
      size: item.size,
      // 后端要的是 **url 字符串数组**（`write_queue_file` 按 `as_str` 取）——
      // 前端内部的 `{url,label}` 只活在界面上，不往契约里塞。
      refs: (item.refs || []).map((ref) => ref.url),
    })) });
    queue.length = 0;
    draft.refs = [];
    renderRefs();
    closeMentions();
    await refreshBoard();
  } catch (error) {
    $('boardNoticeText').textContent = `发布失败：${error}`;
    show($('boardNotice'), true);
    show($('boardMain'), false);
  } finally {
    busy = false;
    renderQueue();
  }
}

async function stopRun() {
  // 立刻改文案：取消是**协作式**的，最坏要等当前那次 HTTP 请求超时（60 秒）才真的退出。
  // 不写这一句，用户点完看到什么都没变，就会以为按钮是坏的（上一版正是如此）。
  const stop = $('btnStop');
  stop.disabled = true;
  stop.textContent = '正在停止…';
  try {
    await invoke('stop_run');
  } catch (error) {
    $('boardNoticeText').textContent = `停止失败：${error}`;
    show($('boardNotice'), true);
  }
  await refreshBoard();
}

async function openImport() {
  let data;
  try {
    data = await invoke('list_inbox');
  } catch (error) {
    notice(`读 inbox 失败：${error}`);
    return;
  }
  inboxFiles = data.files || [];
  const list = $('importList');
  clear(list);
  if (inboxFiles.length === 0) {
    list.appendChild(el('div', 'empty', `inbox 里还没有提示词文件。\n把 .jsonl 放进 ${data.dir} 再打开这里。`));
  }
  inboxFiles.forEach((file, index) => {
    const row = el('div', 'import-row');
    const text = el('div', 'import-text');
    text.appendChild(el('div', 'import-name', file.name));
    text.appendChild(el('div', 'import-meta', file.ok ? `${file.count} 条 · 检查通过` : `${(file.errors || []).length} 处问题`));
    if (!file.ok) {
      const ul = el('ul', 'import-errors');
      for (const message of (file.errors || []).slice(0, 4)) ul.appendChild(el('li', '', message));
      text.appendChild(ul);
    }
    row.appendChild(text);

    const button = el('button', 'btn btn-sm', '导入');
    button.disabled = !file.ok;
    button.addEventListener('click', () => {
      importFile(index);
      closeMask('maskImport');
    });
    row.appendChild(button);
    list.appendChild(row);
  });
  openMask('maskImport');
}

async function importFile(index) {
  const file = inboxFiles[index];
  if (!file) return;
  try {
    // 后端一次做完「读 + 校验 + 归位」（移进 inbox/imported/）——
    // 归位失败会抛错，不会静默留下一份还能被再导入一次的文件（TD-05-12）。
    const tasks = await invoke('import_inbox', { file: file.path });
    for (const task of tasks) {
      queue.push({
        prompt: task.prompt,
        model: task.model,
        ratio: task.aspectRatio,
        size: task.imageSize,
        refs: task.refs || [],
      });
    }
    renderQueue();
  } catch (error) {
    notice(`导入失败：${error}`);
  }
}

async function openSettings() {
  try {
    const value = await invoke('get_settings');
    $('setAccessKey').value = value.accessKey || '';
    $('setSecretKey').value = value.secretKey || '';
    $('setConcurrency').value = value.concurrency || 7;
    $('setOutput').value = value.outputDir || 'output';
  } catch (error) {
    notice(`读设置失败：${error}`);
  }
  openMask('maskSettings');
}

async function saveSettings() {
  try {
    await invoke('save_settings', {
      payload: {
        accessKey: $('setAccessKey').value.trim(),
        secretKey: $('setSecretKey').value.trim(),
        concurrency: Number($('setConcurrency').value) || 7,
        outputDir: $('setOutput').value.trim() || 'output',
      },
    });
  } catch (error) {
    notice(`保存失败：${error}`);
    return;
  }
  closeMask('maskSettings');
}

// ── 启动 ─────────────────────────────────────────────────────────────

async function loadModels() {
  try {
    models = await invoke('list_models');
  } catch (error) {
    // ⚠️ 原先这里是空 `catch` ⇒ `models = []` ⇒ 用户看到一个**空的下拉**，
    //    分不清是「坏了」还是「没有可用模型」。空列表本身就是异常（模型表是代码里的常量）。
    models = [];
    notice(`读不到模型列表（下拉会是空的）：${error}`);
  }
  const select = $('modelSelect');
  clear(select);
  for (const model of models) {
    // 只显示**本名**。别名（`option.value`）照旧落盘，但它在这里**没有功能作用** ——
    // 摆出来只会把宽度撑到最长那条的两倍（`<select>` 宽度由最宽的选项决定）。
    // 要查别名 ⇒ `--list-models`，方案指定的权威来源，UI 不重复承担这个职责。
    const option = el('option', '', model.name || model.alias);
    option.value = model.alias;
    select.appendChild(option);
  }
  // 默认选**工具默认模型**，不是列表第一个 —— 列表第一个是 gpt-image-2-low，
  // 选中它会让「没动过下拉」的任务悄悄用上另一个模型。
  const DEFAULT_MODEL = 'nano-bn-2';
  draft.model = models.some((model) => model.alias === DEFAULT_MODEL)
    ? DEFAULT_MODEL
    : (models[0]?.alias ?? DEFAULT_MODEL);
  select.value = draft.model;
  select.addEventListener('change', () => { draft.model = select.value; });
}

window.addEventListener('DOMContentLoaded', async () => {
  await loadModels();

  renderSeg('ratioSeg', RATIOS, draft.ratio, pickRatio);
  renderSeg('sizeSeg', SIZES, draft.size, pickSize);

  // 提示词是**富文本**（contentEditable）⇒ 事件自己接（整套对齐 maomao 的 PromptInput）。
  //
  // ⚠️ 弹层 **portal 到 `body`**：`position: fixed` 的坐标是视口坐标，
  //    且不会被面板的 `overflow` 裁掉 / 被别的层盖住。搬这一次放在这里（DOM 就绪后），
  //    不写在 index.html 里 —— 那是布局文件，不该记"搬运"这种事。
  document.body.appendChild($('mentionMenu'));

  const promptInput = $('promptInput');
  promptInput.addEventListener('input', () => {
    syncPromptPlaceholder();
    if (composing) return; // 输入法组字中不判定（中文输入不会被候选弹层打断）
    detectMention();
  });
  promptInput.addEventListener('keydown', (event) => {
    if (event.isComposing) return;
    // 弹层开着：上下键在候选里走，不让光标在正文里跑
    if (mentionOpen && (event.key === 'ArrowDown' || event.key === 'ArrowUp')) {
      const list = mentionCandidates();
      if (list.length > 0) {
        event.preventDefault();
        const max = list.length - 1;
        mentionIndex =
          event.key === 'ArrowDown'
            ? (mentionIndex >= max ? 0 : mentionIndex + 1)
            : (mentionIndex <= 0 ? max : mentionIndex - 1);
        renderMentions();
        return;
      }
    }
    if (mentionOpen && event.key === 'Enter' && !event.shiftKey) {
      const list = mentionCandidates();
      if (list.length > 0) {
        event.preventDefault();
        pickMention(list[Math.min(mentionIndex, list.length - 1)]);
        return;
      }
    }
    if (mentionOpen && event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation(); // 不冒泡（Esc 在别处还有"关弹窗"的语义）
      dismissedAtIndex = mentionAtIndex; // 记住：同一个 `@` 不再重弹
      closeMentions();
      return;
    }
    // Shift+Enter = 换行（插 `<br>`；末尾要补一个 filler，否则新行点不进去）
    if (event.key === 'Enter' && event.shiftKey) {
      event.preventDefault();
      const selection = window.getSelection();
      if (!selection || selection.rangeCount === 0) return;
      const range = selection.getRangeAt(0);
      range.deleteContents();
      const br = document.createElement('br');
      range.insertNode(br);
      if (!br.nextSibling) {
        const filler = document.createElement('br');
        br.parentNode.insertBefore(filler, null);
        range.setStartBefore(filler);
      } else {
        range.setStartAfter(br);
      }
      range.collapse(true);
      selection.removeAllRanges();
      selection.addRange(range);
      syncPromptPlaceholder();
      return;
    }
    if (event.key === 'Backspace' || event.key === 'Delete') {
      if (deleteChipNearCursor()) {
        event.preventDefault();
        window.PromptChips.normalizeChipSlots(promptInput);
        syncPromptPlaceholder();
        return;
      }
    }
    // ⚠️ Enter = 加入待发（本工具的老习惯）；换行走 Shift+Enter（上面已处理）。
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      addToQueue();
    }
  });
  promptInput.addEventListener('paste', (event) => {
    // 只取纯文本：富文本粘贴会把 `<span style=…>` 之类的东西带进编辑器。
    event.preventDefault();
    event.stopPropagation();
    const text = event.clipboardData.getData('text/plain');
    const selection = window.getSelection();
    if (!selection || selection.rangeCount === 0) return;
    const range = selection.getRangeAt(0);
    range.deleteContents();
    range.insertNode(document.createTextNode(text));
    range.collapse(false);
    selection.removeAllRanges();
    selection.addRange(range);
    syncPromptPlaceholder();
    detectMention();
  });
  promptInput.addEventListener('compositionstart', () => {
    composing = true;
    closeMentions();
  });
  promptInput.addEventListener('compositionend', () => {
    composing = false;
  });
  promptInput.addEventListener('blur', () => {
    syncPromptPlaceholder();
    // 延后一拍：候选行的 `mousedown` 要先跑完（它 preventDefault 了默认的失焦）
    setTimeout(closeMentions, 0);
  });
  // 点输入框与弹层之外 ⇒ 收弹层（弹层在 body 上，所以两处都要判）
  document.addEventListener('mousedown', (event) => {
    if (!mentionOpen) return;
    const menu = $('mentionMenu');
    if (promptInput.contains(event.target) || menu.contains(event.target)) return;
    closeMentions();
  });
  syncPromptPlaceholder();

  $('refPicker').addEventListener('change', (event) => {
    const files = Array.from(event.target.files || []);
    let pending = files.length;
    if (!pending) return;
    for (const file of files) {
      const reader = new FileReader();
      reader.onload = () => {
        addRefs([reader.result]);
        if (--pending === 0) event.target.value = '';
      };
      reader.readAsDataURL(file);
    }
  });

  $('btnImport').addEventListener('click', openImport);
  $('btnImportCancel').addEventListener('click', () => closeMask('maskImport'));

  $('btnClear').addEventListener('click', () => {
    $('clearText').textContent = `待发里的 ${queue.length} 条会被删掉，不能撤销。`;
    openMask('maskClear');
  });
  $('btnClearCancel').addEventListener('click', () => closeMask('maskClear'));
  $('btnClearOk').addEventListener('click', () => {
    queue.length = 0;
    renderQueue();
    closeMask('maskClear');
  });

  $('btnPublish').addEventListener('click', () => {
    $('publishText').textContent = `提交后这 ${queue.length} 条会出现在左侧「处理中」。`;
    openMask('maskPublish');
  });
  $('btnPublishCancel').addEventListener('click', () => closeMask('maskPublish'));
  $('btnPublishOk').addEventListener('click', publish);

  $('btnStop').addEventListener('click', stopRun);
  $('btnRetry').addEventListener('click', async () => {
    try {
      // 后端会回报「跳过了什么」—— 带垫图的重跑还原不出垫图，必须让用户知道
      const note = await invoke('retry_abnormal');
      if (note) notice(note);
    } catch (error) {
      notice(`重跑失败：${error}`);
    }
    await refreshBoard();
  });

  $('btnResume').addEventListener('click', async () => {
    try {
      // 与「重跑异常」一样回报「跳过了什么」—— 带垫图的残记录接不回来，必须说清。
      const note = await invoke('resume_unfinished');
      if (note) notice(note);
    } catch (error) {
      notice(`接着查失败：${error}`);
    }
    await refreshBoard();
  });

  // 「打开输出文件夹」是跑完批后第一个动作 —— 失败了必须说，不能点完毫无反应。
  $('btnOpenOutput').addEventListener('click', () =>
    invoke('open_output_dir').catch((error) => notice(`打开输出文件夹失败：${error}`)),
  );

  $('btnDone').addEventListener('click', () => {
    doneOpen = !doneOpen;
    refreshBoard();
  });
  $('btnQueued').addEventListener('click', () => {
    queuedOpen = !queuedOpen;
    refreshBoard();
  });

  $('btnSettings').addEventListener('click', openSettings);
  $('btnSettingsCancel').addEventListener('click', () => closeMask('maskSettings'));
  $('btnSettingsSave').addEventListener('click', saveSettings);

  $('btnRetryRead').addEventListener('click', refreshBoard);

  // ── 看大图 ──
  $('viewerImg').addEventListener('load', (event) => {
    const img = event.currentTarget;
    if (img.naturalWidth) $('viewerDims').textContent = `${img.naturalWidth}×${img.naturalHeight}`;
  });
  $('viewerClose').addEventListener('click', closeViewer);
  $('viewerStage').addEventListener('click', (event) => {
    if (event.target === $('viewerStage')) closeViewer();
  });
  $('viewerFit').addEventListener('click', () => {
    $('viewer').classList.remove('is-actual');
    $('viewerFit').classList.add('is-on');
    $('viewerActual').classList.remove('is-on');
  });
  $('viewerActual').addEventListener('click', () => {
    $('viewer').classList.add('is-actual');
    $('viewerActual').classList.add('is-on');
    $('viewerFit').classList.remove('is-on');
  });
  $('viewerReveal').addEventListener('click', () =>
    invoke('reveal_path', { path: viewerFile }).catch((error) => notice(`打开所在位置失败：${error}`)),
  );
  $('viewerCopy').addEventListener('click', () => copyPrompt(viewerPrompt));
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && !$('viewer').hidden) closeViewer();
  });

  // ⋯ 菜单的「点别处关闭」—— **全局只挂这一个**（理由见 moreButton 的注释）
  document.addEventListener('click', closeMenu);

  await wireDropZone();
  renderRefs();
  renderQueue();
  await refreshBoard();
  setInterval(refreshBoard, 1000);
});
