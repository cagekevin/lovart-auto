/**
 * 提示词预设：**形状 + 仓储 + 名字兜底**（无框架、无 DOM，挂 `window.Presets`）。
 *
 * ── 契约（纯函数，只报事实）──────────────────────────────────────────
 * 不碰 DOM、不弹提示、不读输入框 —— 名字与文本由调用方传入；失败只给 `reason`，
 * 说什么话由界面决定（同一条规则在别处措辞不一样）。
 *
 *   Preset = { id: string, name: string, text: string }
 *   name 可空：空串 ⇒ 显示时用 `displayName` 拿 nameFrom(text) 兜底（派生结果不存）
 *
 *   list()                -> { ok, list } | { ok:false, reason, error, list:[] }
 *   get(id)               -> { ok, preset } | { ok:false, reason }
 *   create(name, text)    -> { ok, list } | { ok:false, reason, error?, list }
 *   update(id, name, text)-> { ok, list } | { ok:false, reason, error?, list }
 *   remove(id)            -> { ok, list } | { ok:false, reason, error?, list }
 *   displayName(preset)   -> string   // 有名字用名字，没名字才派生
 *
 *   `nameFrom(text)`（首行前 NAME_MAX 字）**只是模块内部的兜底**，不导出：
 *   外部一律走 `displayName`，否则"兜底名怎么算"就有了第二个出口。
 *
 *   reason ∈ 'empty' | 'missing' | 'storage'
 *   ⚠️ `empty` 只针对**正文**：名字空不算错（有兜底名，预设照样分得清先后）。
 *
 * ── 为什么 id 是主键，名字不是 ────────────────────────────────────────
 * 名字会重复、会随内容变；拿名字定位 ⇒ 改名即换身份、"同名"被当成"同一条"
 * （旧版就因此把第二条悄悄覆盖成第一条）。`id` 创建时生成，**永不改变**。
 * 名字只是给人看的标签：可以重名、可以随时改、**不参与定位**。
 *
 * ── 存储与兼容 ────────────────────────────────────────────────────────
 * v2：`lovart.promptPresets.v2` = `[{id,name,text}]`，存储顺序即展示顺序（新的在前）。
 *   `name` 是后加的字段：老 v2 数据没有它 ⇒ 读成 `''`，走兜底名，**不需要升版本**。
 * v1：`{name,text}[]`，**只在 v2 键不存在时**作为初始值读出来（`name` 现在留用）。
 *   ⚠️ 判据必须是"v2 键存在与否"，不能是"v2 是否为空"：用户把预设删光后
 *      v2 = `[]`，按"空就读 v1"会把删掉的旧数据**复活**。
 * 读一律不写盘：v1 的转换结果只在内存里，第一次 create/update/remove 才写 v2。
 * **不删 v1**（用户数据不擅自销毁）。
 *
 * ⚠️ 这些数字与规则是**前端自限**，不是后端契约：CLI 不认识预设，后端也不读这份存储。
 */
(function () {
  'use strict';

  const KEY_V2 = 'lovart.promptPresets.v2';
  const KEY_V1 = 'lovart.promptPresets.v1';
  /** 兜底名长度：列表一行放得下；超了截断加省略号（全文由界面挂 title）。 */
  const NAME_MAX = 16;

  /** 名字规整：单行标签，连续空白压成一个空格；**不截断**（截断是偷偷改用户写的东西）。 */
  function cleanName(name) {
    return String(name == null ? '' : name).replace(/\s+/g, ' ').trim();
  }

  /** 兜底名 = 首行前 `NAME_MAX` 字（连续空白压成一个空格）。 */
  function nameFrom(text) {
    const firstLine = String(text || '').split('\n').find((line) => line.trim()) || '';
    const name = cleanName(firstLine);
    return name.length > NAME_MAX ? `${name.slice(0, NAME_MAX)}…` : name;
  }

  /** 显示用名字：**有名字就用名字**，没名字才从正文派生。 */
  function displayName(preset) {
    if (!preset) return '';
    return preset.name || nameFrom(preset.text);
  }

  /** 生成 id：时间戳 + 随机后缀，与现有 id 撞了就重来（同毫秒连点两次的情况）。 */
  function newId(taken) {
    for (let attempt = 0; attempt < 5; attempt += 1) {
      const id = `p${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`;
      if (!taken.has(id)) return id;
    }
    // 极端情况（连撞 5 次）退化成递增编号，保证**一定**不重复
    let n = taken.size + 1;
    while (taken.has(`p${n}`)) n += 1;
    return `p${n}`;
  }

  function fail(reason, error) {
    // 失败形状**不带 list**：把 error 折成 [] 会让消费者把“读不到”当成“还没有”
    return { ok: false, reason, error };
  }

  /**
   * 解析一份原始 JSON：逐项校验，坏项丢弃（不因一条坏数据废掉整张表）。
   * `trustId` = 这份数据里的 id 可信（v2）；v1 没有 id，全部重新生成。
   */
  function parse(raw, trustId) {
    let data;
    try {
      data = JSON.parse(raw);
    } catch (error) {
      return fail('storage', error);
    }
    if (!Array.isArray(data)) return { ok: true, list: [] };

    const taken = new Set();
    const items = [];
    for (const entry of data) {
      const text = entry && typeof entry.text === 'string' ? entry.text.trim() : '';
      if (!text) continue;
      let id = trustId && entry && typeof entry.id === 'string' ? entry.id : '';
      if (!id || taken.has(id)) id = newId(taken);
      taken.add(id);
      items.push({ id, name: cleanName(entry && entry.name), text });
    }
    return { ok: true, list: items };
  }

  /**
   * 当前列表。
   * ⚠️ **v2 键存在（哪怕是 `[]`）就以 v2 为准**；只有键不存在时才退到 v1。
   */
  function list() {
    let v2 = null;
    try {
      v2 = window.localStorage.getItem(KEY_V2);
    } catch (error) {
      // 存储被禁用（隐私模式 / 策略）时连读都会抛 ⇒ 不能说成"还没有预设"
      return fail('storage', error);
    }
    if (v2 !== null) return parse(v2, true);

    let v1 = null;
    try {
      v1 = window.localStorage.getItem(KEY_V1);
    } catch (error) {
      return fail('storage', error);
    }
    if (v1 === null) return { ok: true, list: [] };
    return parse(v1, false); // v1 的 name 留用，id 重新生成
  }

  function write(items) {
    try {
      window.localStorage.setItem(KEY_V2, JSON.stringify(items));
      return { ok: true, list: items };
    } catch (error) {
      return fail('storage', error);
    }
  }

  function get(id) {
    const current = list();
    if (!current.ok) return fail(current.reason, current.error);
    const preset = current.list.find((item) => item.id === id);
    if (!preset) return { ok: false, reason: 'missing' };
    return { ok: true, preset };
  }

  /** 新建：置顶。正文 trim 后为空 ⇒ `empty`，不落盘（名字空无所谓）。 */
  function create(name, text) {
    const clean = String(text == null ? '' : text).trim();
    if (!clean) return fail('empty');

    const current = list();
    if (!current.ok) return fail('storage', current.error);

    const items = current.list.slice();
    items.unshift({
      id: newId(new Set(items.map((item) => item.id))),
      name: cleanName(name),
      text: clean,
    });
    return write(items);
  }

  /** 改：**原位替换**（位置不动），id 不变；名字与正文一起换。 */
  function update(id, name, text) {
    const clean = String(text == null ? '' : text).trim();
    if (!clean) return fail('empty');

    const current = list();
    if (!current.ok) return fail('storage', current.error);

    const at = current.list.findIndex((item) => item.id === id);
    if (at < 0) return fail('missing');

    const items = current.list.slice();
    items[at] = { id, name: cleanName(name), text: clean };
    return write(items);
  }

  function remove(id) {
    const current = list();
    if (!current.ok) return fail('storage', current.error);
    if (!current.list.some((item) => item.id === id)) return fail('missing');
    return write(current.list.filter((item) => item.id !== id));
  }

  window.Presets = { list, get, create, update, remove, displayName };
})();
