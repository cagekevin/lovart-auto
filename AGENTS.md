# AGENTS.md · lovart 自动生图的干活规则

> 本仓的**流程规则**只写在这里。`daily/架构日志/index.md`、`债务.md`、`scripts/*.mjs` 的注释都指向本文件，别在别处再写一份。
> 判据不在这里：用户要什么看 `README.md` 与 `docs/详细说明.md`；架构判据看 `docs/adr/`；写计划看 `docs/写plan的方法-删除测试.md`；找债看 `docs/找债务的方法-存在理由审查.md`；区域日志怎么填看 `daily/架构日志/_template.md`。

## 开工确认（硬性）

动任何文件之前，先读：

1. `docs/详细说明.md §5 验证规则`（一次就够，除非验证口径变了）。
2. `docs/plan/` 里**未结案**的那条施工契约 —— 它就是当前在做的活。
3. `docs/adr/README.md` 里与本次能力相关的条目；`daily/架构日志/index.md` 看该区审到哪一步。

第一次回复的第一行必须有确认：

```text
已读完 详细说明 §5 + plan/<NN> + ADR-0NNN；本次最相关判据：<条号或原句>
```

并附上那几条的**原文要点**（照原文写，不许凭记忆复述）。找不到对应判据时，先说明「这一条还没有判据」，再决定是补判据还是直接做 —— 不许假装读过。

---

## 1. 命令

| 目的 | 命令 | 别当成 |
| --- | --- | --- |
| 全量回归 | `make check`（= `cargo test --workspace`） | **行为验证**。它只防回归，不是每轮的闸；失败退出码是 **101**，不是 1 |
| 后端定点 | `cargo test --workspace <过滤名>` | 全量替代。改 `src-tauri/` 用它，不是每轮跑全量 |
| 前端 | `node --check ui/app.js` | 编译。改 `ui/` 的闸就这一条（1 秒），不为验前端跑编译 |
| 探针 | `node scripts/probe.mjs --label '…' --file <路径> --find '<原>' --replace '<坏>' --run '<命令>' --expect-exit 101 --expect-out '<该红的用例>'` | 测试。它证明「这条测试有鉴别力」，注入点 0 处或多处都会拒跑 |
| 判据体检 | `node scripts/adr.mjs audit` · `doctor` · `refs --check` | 闸。只读，出清单 |
| 账本 | `node scripts/debt.mjs list / add / resolve / audit / archive` | 手改表格。**唯一写入口** |
| 区域索引 | `node scripts/arch-index.mjs` | 手改 `index.md`。幂等生成 |
| 体检事实 | `node scripts/scan.mjs`（`--json` · `--changed` · `--limit` · `--threshold`） | 合规检查。只出事实、恒 exit 0；CC 是**近似值**（不建 AST）；没有 `lcov.info` 时覆盖率按 0% |
| 契约对账 | `node scripts/typegen.ts` | 类型生成。它比 `prompts.jsonl` 字段在「Rust 白名单 / README 表 / inbox 实际用法」三处是否一致 |
| 发布 | `node scripts/deploy.ts [--to <工作区>]` | 构建。它只把**已打好**的产物搬到工作区；缺产物会叫你去 `make app` / `make win` |
| 搬移事实 | `node scripts/mv-sync-refs.mjs move / move-dir / rename …` | 重构。它只改文件位置与 import 说明符，不判对错、不改语义（ADR-0009 认定的事实工具） |
| 打包 | `make app`（macOS）· `make win`（Windows，**必须 `pwsh` 7**，5.1 按 GBK 解码无 BOM 脚本 ⇒ 中文乱码）· `make clean-rs`（清构建产物）。实现见 `scripts/make-app.sh` / `scripts/make-exe.ps1`（`build-win.bat` 是同一条 PowerShell 的批处理包装） | 验证。改过 `ui/` 必须重打包（前端是编译期嵌进二进制的） |

为什么这些不是闸、每轮到底该跑什么 —— 见 `docs/详细说明.md §5`，这里不复述。**编译不是测试；全绿只证明「和上次一样」，不证明「规则真的成立」。**

## 2. Map

| 路径 | 拥有 |
| --- | --- |
| `crates/lovart-core/` | 领域与引擎：`assistant/`（LLM 对话：会话真相文件的唯一写者 · 请求组装 · 非流式信封，**不属于出图网关**）、`fsio.rs`（`write_atomic`：tmp → rename，横跨两层共用 ⇒ 放 crate 根，不许塞进 `engine/` 或 `lovart/`）、`lovart/`（client · contract · models · prompt · task · attachments · config · connection · errors · media_type · project）、`engine/`（scheduler · state · settings · intake · refs · download · group · report · runlog · session · lockfile · text）、`service/`（api · files · library · meta · refs · snapshot · wiring · workbench）。**这个 crate 的 `Cargo.toml` 里没有 tauri** ⇒ 写 `use tauri::…` 是编译错误，不需要任何 grep 检查 |
| `src-tauri/` | 只留入口与适配：`gui.rs`（GUI）、`http.rs`（本机 HTTP）、`main.rs`、`lib.rs`。CLI 已删除（2026-11-27） |
| `ui/` | 无框架前端：`index.html` · `app.js` · `presets.js` · `library.js` · `tauri.js` · `refStrip.js` · `promptChips.js` · `promptMention.js` · `style.css` · `assistant-plugin/`（AI 对话插件：自包含、零本仓 import，靠 adapters 接宿主）· `tests/`（前端零依赖单测，`node --test`）。编译期嵌入二进制 |
| `fixtures/` | 黄金用例（JSONL）：回放断言用，改行为要同步 |
| `data/` | **唯一的运行数据根**（`docs/plan/19`）：`.env` · `prompts.jsonl` · `inbox/` · `presets/` · `library/`（素材库）· `output/` · `var/`（state / report / run.log / refs / cache / lock）· `chat-history.json`（AI 对话会话真相，整表原子写）· `chat-system-prompt.txt`（对话系统提示词，纯文本）。启动幂等迁移旧根目录布局（`service::workbench::migrate_layout`）；`git clean -xdf` 会删 `data/` ⇒ 动之前先备份 |
| `prompt-library/` | 提示词库与配图（数据，不是代码）；代码 0 引用，留在仓库根 |
| `scripts/` | 账本与判据：`adr.mjs` · `debt.mjs` · `arch-index.mjs` · `probe.mjs`；事实：`scan.mjs`（复杂度 × 覆盖率 + 结构重复，零依赖，扫 Rust 与 `ui/`）· `typegen.ts`（`prompts.jsonl` 字段对账）；发布：`deploy.ts`（产物落位，不构建）；打包：`make-app.sh` · `make-exe.ps1` · `build-win.bat`。全部 `node scripts/<名字>` 直接跑 —— Node 24 原生跑 `.ts`，不吃 `tsx`/`ts-node` |
| `docs/adr/` | 判据真源；`README.md` 是 `adr.mjs index --write` 生成的索引 |
| `docs/plan/` | 一次性的施工图：落地即结案（见 `15-…-结案记录`），结论并进 ADR 或 `详细说明` |
| `docs/接入指南-本机HTTP-其他前端-*.md` | 其他前端的 HTTP 接入契约：token / 三端点 / 状态表 / 限制 |
| `daily/架构日志/` | 轮次日志 = 探债明细真源；`债务.md` = 一行摘要；`index.md` = 生成物。三者不互相复述 |
| `packaging/` | `icon.png`（图标源）· `macos/Info.plist`：打包输入（唯一根，原 `assets/` 已并入） |

数据流概要：`data/inbox/*.jsonl` →（GUI 导入 / `POST /tasks` 校验字段 / 模型别名 / ratio / size）→ `engine::scheduler` 提交 Lovart → `data/var/state.jsonl`（`FileLock` 独占，**单写者**）→ `data/output/` + `data/var/report.csv` + `data/var/run.log`。没有 `docs/architecture.md`；要补数据流就到本文这张表下补，别再开第三个文档。

**目录归属判据（唯一一处，`docs/plan/19`）**：运行数据全部在 `data/` 下按用途分子目录；源码与打包输入不进 `data/`；新增目录先改本节 Map + `.gitignore`，再让 `Workbench` 加具名访问器（`src-tauri/` 里不许裸用 `cwd()` 拼业务目录）。

## 3. 分层与冲突裁决

| 层 | 在哪 | 速度 | 冲突时 |
| --- | --- | --- | --- |
| 意图（用户要什么） | `README.md`、`docs/详细说明.md` | 慢 | 与代码冲突 ⇒ **代码错**，除非用户改口 |
| 判据（每次动这块都照做） | `docs/adr/` | 只增不改 | **判据赢**；代码不符 ⇒ 登记债，报用户裁定 |
| 行为（这一次怎么改） | `docs/plan/` | 一次性 | 落地即作废，不留在仓里当第二份真相 |
| 实现 | `crates/` · `src-tauri/` · `ui/` | 快 | 代码是现状，不是判据 |
| 账与证据 | `daily/架构日志/` | 按轮 | 轮次日志是明细真源，账本只存摘要 |

三条铁律：

- **判据只能人来改。** 新 ADR 必须 `--user-approved`；旧条只改状态（`status --to 已取代 --by`），不改历史。修正原文要留痕（日期 + 原作 ⇒ 现作 + 为什么）。
- **一份事实只放一处。** 同一知识在 README / 详细说明 / AGENTS / ADR 各说一遍 ⇒ 当场删到一处。
- **结构优先于文档。** 能让写错编译不过（类型、可见性、唯一入口），就别只写一句约定（ADR-0004）。

## 4. 改东西的顺序

0. **先写 plan**：`docs/plan/<NN>-<能力>-<YYYY-MM-DD>.md`，照 `docs/写plan的方法-删除测试.md` 的六段写（第一性问题 → 最小切片 → 紧邻 → 之后 → 不做 → 验收）。**没 plan 不许动代码**；落地写「结案记录」，别把 plan 留在仓里当第二份真相。
1. **改行为**：先改 plan 里的形状与契约，再改代码。测试先写 —— 先看着它因为**预期原因**红，再改到绿；红不了就别写一个坏的。
2. **纯重构 / 笔误**：行为不变，不写 plan，不动判据。批量改名 / 搬目录用 `node scripts/mv-sync-refs.mjs move-dir …`（它同步 import 说明符；跑完仍要编译 —— 它不是验证）。
3. **判据变了**（分层、依赖方向、跨模块不变量）：同一批提交里改 ADR；推翻既有判据写新条并把旧条 `status`。
4. **范围外的发现**：另开一债，不混进本次（ADR-0001 检查项 6）。
5. **改契约字段**（`prompt.rs` 的 `ALLOWED_KEYS`、README 的 `prompts.jsonl` 表）：同一批跑 `node scripts/typegen.ts` 对账 —— 两处不一致就是文档漂移，别等下一个 AI 写错才发现。
6. **出包与发布**：`make app` / `make win` 之后用 `node scripts/deploy.ts --to <工作区>` 把产物落位；它**不构建**，也不新增打包入口。

卡住时两条，先想第二条：

- **造杠杆**：一眼看不明白，就先把「做这件事的工具」造出来 —— 先手工做一个单元，再让工具去对齐它。
- **攻前提**：两个共享同一前提的修法，在同一道门禁上双双失败 ⇒ 把这个前提炼成一句话写下来，去掉那个指定，再开始下一个修法。在这之前不许试第三个。

## 5. 体检

口令：**「体检」/「找债」/「存在理由审查」**。方法真源是 `docs/找债务的方法-存在理由审查.md`（判据、证据来源、六类债、扫描≠判定）。这里只定**两个时刻**。

### 5.1 动手前（找债）

按 `docs/adr/` 索引 → 相关 plan 的契约 → `daily/架构日志/index.md` 的分层 → 债务账本，逐条对代码，找「写的和做的不一样」。

挑从哪看起用 `node scripts/scan.mjs`：它的 CRAP 表与重复对只**排优先级**，不判对错 —— 拿它选文件，拿 ADR 与契约判对错。

- **不是跑测试。** `make check` 全绿是另一件事，不许拿它当体检结论。
- **默认不改行为。** 只出清单（条款 → 现状 → 偏差 → 处置）；要改代码先等用户点头。
- 偏差分三类：**代码错**（不符判据 / 不变量）、**文档错**（文档落后于代码，或 plan 里写成了实现细节）、**缺一环**（两边都对但少了衔接）。
- 行为不符时**不许自己判谁对**：判据是意图，只有人能改意图 ⇒ 报给用户裁定。

### 5.2 动手后（三件事，缺一件不算做完）

**① 垂直删净**：`pub` 声明 / 导出 → 注册与分发（`mod.rs`、`main.rs` 的分支、`tauri::command` 注册表）→ 实现与状态 → 导入与构建清单（`Cargo.toml`、`ui/` 嵌入路径）→ `fixtures/` 用例 → 文档与索引（README、详细说明、ADR 落点、区域日志）→ 专属测试 → 不再需要的依赖与脚本。

**② 分环验证，分开报**：

1. **残留检查**：grep 被删的名字 / 字符串 / 路径 / 参数，计数必须为 0（含注释与文档）。
2. **决定性检查**：删错就会红的那个 —— `cargo test --workspace <过滤名>`、`node --check ui/app.js`，或出包后手跑一条。
3. **探针**：`scripts/probe.mjs` 注入 → 精确断言（退出码 + 命中串）→ 无条件还原。证明「这条检查真的会拦」。
4. **边界对比**：对外承诺的形状逐字段比 —— `prompts.jsonl` 字段、`GET /state` 输出、`var/state.jsonl` 行形状。
5. **diff 审计**：读失败与清理路径、二次运行（`var/state.jsonl` 已存在时重跑）、每个调用点。
6. **复杂度与重复**：跑 `node scripts/scan.mjs`，把改动前后 CRAP 最高的几行、重复对数比一比 —— 数字变差说明复杂度只是搬到了别处，回去改结构，不要改表。

> **单测绿 ≠ 能交付。** 分环的结果要分条报，不许合并成一句「测试通过」。

**③ 出回执**：改了哪个边界 / 基线 · 退休的义务（旧入口、旧参数、旧字段）· 实际净效果 · 行为变化 · 验证命令与结果 · 剩余风险 · 保留的候选 · **怎么撤**。撤销路径必须与副作用匹配：源码可回退，`output/` 与已被消耗的额度**撤不回**，已出包的要单独写恢复步骤。

### 5.3 合并同类项（去重体检）

用户说「简化 / 合并同类项」时：「简化」不是删行数，是**减少必须同时保持一致的概念数**。

先列**覆盖地图**（in-scope 的入口、边界，以及没看的盲区），再按 `_template.md §二` 的九个透镜找候选 —— 不要停在第一个看起来能删的东西上。证据阶梯：闻到味道 → 静态线索 → 消费者地图 → 契约证据 → 行为证据；**前两级不是删除授权**。每条写两格：**Proof**（能暴露「删错了」的最小检查）、**净效果**（删掉的概念 − 新增的迁移机制；为负就是没简化，只是搬了家）。

**长得像不等于重复。** 两份实现可能在隔离失败域或遵守不同契约 —— 合并前先答「它们的所有权边界是同一个吗」。删掉一个可达能力（CLI 参数、JSON 字段、已存数据格式）是产品决定，单独授权，不许混在体检里。

## 6. 治理

### 6.1 ADR

ADR 是**现行判据集**，不是决策日志。写一条，当且仅当三条都成立：约束的是**反复发生**的行为；**类型 / 结构 / 测试拦不住**；不写就会烂。

- 正文一页（>80 行 `audit` 会报信号，但那是信号不是病 —— 先问「这条要让下一个 AI 做出什么不同判断」，别为过闸删字）。
- `add` 之前先逐条答脚本要求的那几个问题，**把答卷贴进对话**给用户看；用户点头才加 `--user-approved`（缺它脚本拒绝落盘）。不许自己敲个 flag 就算「用户裁定」。
- 状态只认 `草案 / 生效 / 已取代 / 已弃用 / 已否决 / 已毕业`；「决策变了」写新条并 `status --to 已取代 --by`，「原文写错」直接回改并留痕。
- 能升为自动检查的判据就毕业：`status --to 已毕业 --note "<载体:文件:行>"`。现行集因此有上界。
- 新增 / 改状态 / 改索引一律走脚本，**禁手写 `README.md` 的索引表**。

```bash
node scripts/adr.mjs list --all
node scripts/adr.mjs add --title "…" --conclusion "…" --user-approved
node scripts/adr.mjs status 0005 --to 已取代 --by 0006 --note "…"
node scripts/adr.mjs index --write
node scripts/adr.mjs audit
node scripts/adr.mjs refs --check     # 全仓断链：引用了不存在的编号 ⇒ exit 1
```

### 6.2 债务（区号表）

碰到既有代码违反 ADR / 契约 / 不变量：**先登记债，继续手上的活**，不许顺手改，也不许只在回答里提一句。

- 唯一账本 `daily/架构日志/债务.md`，唯一写入口 `node scripts/debt.mjs`；**禁手写表格行**。已完成项由 `archive` 移入 `债务-归档.md`。
- 摘要 ≤120 字，禁裸 `|`，引号用「」（ASCII 双引号会被 PowerShell 当引号边界截断参数）。
- 锚点必须是 `daily/架构日志/<NN>-…-<日期>.md` 且**真实存在**、且文件里能搜到该债号 —— `audit` 会报断链（存在 ≠ 有关）。
- 回答里只给债号（`TD-<区号>-<序号>`），不复述全文；`show` 给完整解法与下一跳。
- 审计可以结论「无债」，不许为凑数登记假债。

区号（**必须与 `scripts/arch-index.mjs` 的 `AREAS` 一致**；改一处就得改两处）：

| 区号 | 区域 | 层 |
| --- | --- | --- |
| 01 | 领域与业务规则 | 地基 |
| 02 | 数据与持久化 | 地基 |
| 03 | 接口与装配 | 中间 |
| 04 | UI 与交互 | 上层 |
| 05 | 测试与验收 | 元层 |
| 06 | 构建与发布 | 元层 |
| 07 | 文档与治理 | 元层 |

```bash
node scripts/debt.mjs list                       # 主表待办
node scripts/debt.mjs area 03                    # 该区全部历史（含归档）
node scripts/debt.mjs search SSOT               # 跨区找同类（无精确命中时自动同义扩展）
node scripts/debt.mjs show TD-03-1
node scripts/debt.mjs add --area 03 --summary "…" --class 增债 --rate 中 --anchor 03-…-2026-11-20.md
node scripts/debt.mjs resolve TD-03-1 --note "销账证据"
node scripts/debt.mjs archive
node scripts/debt.mjs audit [--liveness]
node scripts/arch-index.mjs
```

### 6.3 交付前四问（硬性）

每次回答都把答案写进回复里；答不出或答成「应该没有」按未完成处理：

1. 这轮动了哪个入口？**旧入口还在吗**？
2. 旧入口如果还在，它的消费者**全部**迁完了吗？没迁完 = 两条线 = **未完成**。
3. 同一能力数出几个入口？同前缀并存（`Xxx` 与 `XxxOr`）或旧入口仍被调用 ⇒ **两条线**（ADR-0005）。
4. **这段代码，下一个 AI 看得懂吗**？看不懂且没有唯一范本可照抄 ⇒ 不许交付（ADR-0006）。

出现两条线 ⇒ 不许写「已完成 / 已修复 / 已收口」，回去删掉其中一条。

### 6.4 沟通

1. 第一行给动作方案：下一步做什么、改哪里、什么结论；不写「让我看看 / 好的 / 我来分析」。
2. 多步任务编号 `1. 2. 3.`，每步一个动作。
3. 报错就事论事：原因 + 修法。
4. 结尾给一个能立刻动手的下一步：一条命令、一次点击，或一个文件路径。不给「下一步可以考虑…」。
5. **例行维护直接做完再报告**：跑体检、跑 `adr.mjs doctor`、把同一条改动铺到一批文件 —— 不要先出方案、不要问要不要做。只有要删一个可达能力、要改对外契约（JSON 字段 / CLI 参数）、或要花额度跑一批时才停下来问。

## Done

改动完成 = 每个改过的路径端到端可用，本文件每条规则在改过的文件里成立，且对应的检查**真跑过并如实报告**（没跑的说不跑，红了照原文报）。

交付前另跑两项**只读巡检**并贴结果：`node scripts/debt.mjs audit`、`node scripts/adr.mjs refs --check`。它们不进 `make check`，也不判代码对错 —— 只回答「账本有没有断链、引用有没有指向不存在的编号」。

交出去之前，用新上下文把整份 diff 读一遍：失败与清理路径、在已有 `var/state.jsonl` 上的二次运行、以及每个改动点的调用者，发现就修。
