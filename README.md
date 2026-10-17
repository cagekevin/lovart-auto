# lovart 免费自动生图

用 Lovart 的**排队模式（unlimited，免费）**批量出图的独立工具。
读一批提示词，最多 7 个任务并发，断点续跑，只出图。

**一个二进制**：双击 = 图形界面；带子命令 = 命令行。

---

## 快速开始

```bash
# 1. 配置凭据
cp .env.example .env        # 填 LOVART_ACCESS_KEY / LOVART_SECRET_KEY

# 2. 编译 + 跑测试（改动过代码的话，它是唯一可信的验收）
make check                  # = cd src-tauri && cargo test

# 3. 先预览计划（不联网、不提交任何任务）
./src-tauri/target/debug/lovart-client --prompts prompts.jsonl --dry-run

# 4. 正式跑
./src-tauri/target/debug/lovart-client --prompts prompts.jsonl

# 5. 打成 macOS 应用（双击即用）
make app                    # → 仓库根目录下的 lovart.app
```

`cargo build --release` 后是 `src-tauri/target/release/lovart-client`（Windows 上 `.exe`）——
**双击它开界面**，或带参数当命令行用。

**macOS 上的正式交付形态是 `lovart.app`**（`make app` 生成，见 `scripts/make-app.sh`）：
双击开图形界面，`lovart.app/Contents/MacOS/lovart-client --help` 仍是同一个二进制。

> ⚠️ **改过 `ui/` 必须重新 `make app` / `cargo build`** —— 前端资源是在**编译期**
> 由 `tauri-build` 嵌进二进制的（`frontendDist: ../ui`），运行时不读磁盘上的 `ui/`。
> 只换二进制而不重新编译，界面会停在上一版（这个坑踩过一次）。

> ⚠️ **双击启动时的工作区由程序自己定**（Finder 给的当前目录是 `/`，不是仓库）。
> 判定顺序见 `src-tauri/src/gui.rs` 的 `workspace_dir()`：`.app` 同级目录是仓库 ⇒ 用它。
> 所以 `.app` 要**和 `.env` / `inbox/` / `var/` / `output/` 放同一层**。

**Windows 上的正式交付形态是仓库根下的 `lovart-client.exe`**（`scripts/make-exe.ps1` 生成）：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File ./scripts/make-exe.ps1   # 或：make win
```

它做三件事：从 `assets/icon.png` 生成 `src-tauri/icons/icon.ico` → 载入 MSVC 环境编译
（前端资源同样是**编译期**嵌入，改过 `ui/` 必须重跑）→ 把 exe 拷到仓库根。
双击 = 图形界面；`.\lovart-client.exe --help` = 命令行。产物是**自包含单文件**，运行时依赖为 0。

> ⚠️ Windows 上没有 `.app` 那层外壳，`bundle_sibling_dir()` 不适用 —— 工作区判定走
> `workspace_dir()` 的「**当前目录是工作区**」分支：双击时当前目录 = exe 所在目录，
> 所以 exe 要和 `.env` / `inbox/` / `var/` / `output/` 放同一层（即仓库根）。

**命令行写法**：`--key value` 与 `--key=value` 等价；`--ref` 可重复也可逗号分隔；
`--` 之后的内容不再当参数解析；`-h` / `--help` 打印本文件。**未知参数会报错终止**（不会静默忽略）。

---

## 参数

| 参数 | 默认 | 说明 |
|---|---|---|
| `--prompts <file>` | `prompts.jsonl` | 提示词文件（JSONL） |
| `--output <dir>` | `output` | 图片输出目录 |
| `--state <file>` | `var/state.jsonl` | 断点续跑状态 |
| `--report <file>` | `var/report.csv` | 结果报告（CSV，带 BOM，Excel 直接开） |
| `--no-report` | — | 不写报告文件 |
| `--ref <路径\|URL\|data:>` | — | 参考图（图生图），可重复或用逗号分隔 |
| `--refs-cache <file>` | `var/refs-cache.json` | 参考图 CDN 缓存（按内容哈希去重） |
| `--no-refs-cache` | — | 不读写参考图缓存 |
| `--concurrency <n>` | `7` | 并发任务数，**硬上限 8** |
| `--model <alias>` | `nano-bn-2` | 默认模型 |
| `--ratio <w:h>` | `3:4` | 默认比例 |
| `--size <1K\|2K\|4K>` | `2K` | 默认尺寸档位，原样透传给上游 |
| `--max-attempts <n>` | `3` | 可重试失败上限 |
| `--max-busy-retries <n>` | `60` | 「服务端并发满」排队重投上限（≈1 小时 @60s 封顶） |
| `--max-timeouts <n>` | `3` | 累计超时多少次后**释放槽位**转 `unresolved` |
| `--download-retries <n>` | `3` | 单张产物下载重试次数（含首次） |
| `--task-timeout <分钟>` | `90` | 单任务轮询总超时 |
| `--recheck-timeout <分钟>` | `30` | 复查已超时任务时的轮询超时 |
| `--allow-unknown-model` | — | 未知模型名只警告不终止（**默认终止**，防整批静默跑错模型） |
| `--auto-confirm` | — | 自动确认高成本操作（**会扣积分**；每任务每批最多一次，失败转人工不重试） |
| `--no-auto-confirm` | — | 关闭自动确认（覆盖 `.env` 的 `LOVART_AUTO_CONFIRM`） |
| `--check` | — | **只校验提示词文件**（给 AI 自检用）：不联网、不提交；有错则非零退出并报行号 |
| `--dry-run` | — | 只打印计划，不提交 |
| `--list-models` | — | 列出可用的**出图**模型别名（从代码生成，权威来源）；配 `--json` 机器可读 |
| `--status` | — | **任务看板**：读状态文件，列出在跑/已结束任务与对应提示词（不联网、不需凭据） |
| `--full` | — | 配合 `--status`：提示词不截断 |
| `--json` | — | 配合 `--status` / `--list-models`：输出机器可读 JSON |

**结果怎么看**：`--status` 随时可跑（生成进程正在跑时也能安全查看 —— 只读打开，不压实、不干扰追加写）。

---

## prompts.jsonl 格式

**一行一条 JSON 对象**（JSONL）：

```jsonl
{"prompt":"一只戴帽子的猫，水彩风格"}
{"prompt":"赛博朋克城市夜景","model":"nano-bn-pro","ratio":"16:9"}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `prompt` | string | ✅ | 非空 |
| `model` | string | — | 模型别名；缺省用 `--model`。**可用别名见 `--list-models`** |
| `ratio` | string | — | `w:h`，每边 1–3 位数字；缺省 `3:4` |
| `size` | string | — | `1K` / `2K` / `4K`；缺省 `2K` |
| `refs` | string[] | — | 参考图（路径 / URL / `data:`），**非空数组** |

**fail-loud**：非法 JSON · 非对象 · **未知字段** · `prompt` 缺失或空 · 类型不符 ·
**未知模型** · 非法 `ratio` / `size` —— 一律**报错并带行号**，不猜、不兜底。空行跳过。

**自检**（AI 的循环：写 → `--check` → 读报错 → 改 → 再 `--check` → 交给你）：

```bash
lovart-client --check --prompts inbox/x.jsonl
lovart-client --list-models            # 权威别名表，不会像文档那样过期
```

**为什么是 JSONL，不是自研语法**：文件格式的真实职责是「**AI ↔ 工具**」的契约。
自研 DSL 要人肉对齐两套语义 ⇒ 必然分叉；JSONL 两边都用标准库解析 ⇒ **结构上不可能分叉**。
且旧语法有个自己造出来的坑：`[nano-bn-pro]` 这种裸值会**静默丢配置**（模型退回默认），跑完几百条才发现。

---

## 断点续跑

状态写在 `var/state.jsonl`（每行一条，同 id 以**最后一条**为准）。

**状态表在代码里**：`src-tauri/src/engine/state.rs` 的 `STATUS_TABLE`
—— 一行 = 一个状态的全部后果（占不占槽 / 进不进队列 / 界面能做什么 /
异常怎么说 / **再跑一次会怎样**）。要看当前这份，直接跑：

```bash
./lovart-client --status-table
```

每个状态的**含义**写在 `TaskStatus` 各变体的注释里（同一个文件）。

⚠️ 这里**刻意不再抄一份表**：抄的那份会漂移 —— 历史上第 4 列就与代码对不上过，
`download_failed` 的短名也两处不同。**文档只放指针。**

提示词 id 由**内容哈希**生成，所以增删行、调整顺序都不会打乱已有进度。
`Ctrl+C` 中断时，正在跑的任务会保留 `threadId`，下次自动复查。

⚠️ **改了模型 / 比例 / 尺寸会重跑**：id 只哈希提示词文本，所以启动时会比对这三个参数，
对不上就把该条重置为 `pending`（**旧图不覆盖**，会自动加 `_r1` 后缀）。日志会打印「参数变更重跑 N 条」。

---

## 设计要点（容易踩的坑）

1. **并发上限是「服务端槽位」，不是 worker 数。** 槽位 = 「已提交未到终态」的任务数，
   **从状态派生**（`count(SLOT_HOLDING_STATUSES)`），**不维护计数器** —— 手动计数器必然漂。
   超时被放走的任务**仍占槽位**，所以要优先复查它，而不是立刻补新任务。
2. **超时 ≠ 失败。** 轮询超时只释放 worker，任务转 `timeout` 保留 threadId，收尾再查。
3. **默认不自动确认 `pending_confirmation`。** 那是高成本操作要用户批准才扣积分的闸门。
   顶栏的**全局开关**（`.env` 的 `LOVART_AUTO_CONFIRM`）打开后才会自动确认并继续，
   且**同一任务每批最多一次**；确认失败一律转人工、**不重试**（请求可能已在服务端生效，
   重试就是重复扣费）。关着时遇到它仍记 `needs_confirm` 并跳过。
4. **每次提交前重设排队模式。** 服务端模式是**账号级持久设置**，可能被别处（如 AI Canvas）改回 fast。
5. **不要与 AI Canvas 同时用 Lovart**，两边会互相改模式。
6. **尺寸只给档位标签，不给像素。** 提示词里只写 `resolution: 2K, aspect_ratio: 3:4`，
   输出尺寸由模型自己决定。代价是**实际出图尺寸可能和档位不完全一致**（实测 `3:4 + 2K` 出 1792x2400）。
7. **槽位必须能降下来，否则并发会被拖死。** 本地给「超时保留」设上限（`--max-timeouts`，
   默认 3 次 = 90+30+30 分钟）就释放槽位转 `unresolved`，交给收尾兜底复查。
8. **下载失败 ≠ 重新生成。** 生成成功时服务端槽位就已释放，此时下载失败只是网络问题：
   立即释放本地槽位，把产物 URL 记进 `state`，收尾和下次**只补下载**。
   补下载幂等（同名同大小直接复用），且**绝不覆盖**已有图（重名加 `_rN`）。
9. **收尾阶段只跑一轮。** `unresolved` 兜底复查与 `download_failed` 补下载各只做一次，
   不满足就留在原状态等下次 —— 避免「复查 → 超时 → 再复查」的死循环。
10. **「服务端并发满」是排队状态，不是失败。** 上游返回 402 / 409 / 429 / 2011 / 2012 / 1429 时，
    任务回 `pending`，按**本任务自己的**退避时间（5s→60s 封顶 + jitter）重投，
    计入独立的 `--max-busy-retries` 而**不是** `--max-attempts`（后者 3 次会让「另一批占着几小时」直接判死）。
    **任何任务到达终态 ⇒ 立刻解除等待** —— 有任务完成说明服务端确实空出了槽位。
    ⚠️ 必须**按任务**记退避时间：用全局闸门会被最后一次写入覆盖，一开就齐撞。

---

## 目录结构

```text
lovart 自动/
├── lovart.app/           macOS 应用包（`make app` 生成，**不进版本库**）
├── lovart-client.exe     Windows 单文件交付（`make win` 生成，**不进版本库**）
├── src-tauri/            唯一实现（Rust）：GUI 与无头 CLI 是**同一个二进制**
│   ├── src/
│   │   ├── main.rs             双击 = GUI；带参数 = 无头 CLI
│   │   ├── cli.rs              命令行解析（纯函数，可测）
│   │   ├── run.rs              命令行主流程 + 看板 + 报告
│   │   ├── gui.rs              Tauri 壳（前端唯一入口，不重算任何业务规则）
│   │   ├── engine/             调度器 · 状态 · 参考图 · 下载 · 文本读写
│   │   └── lovart/             协议层（HMAC 签名 / 轮询状态机 / project / 附件 / 错误码表）
│   └── tests/                  回归测试（`cargo test`）
├── ui/                   前端：index.html · app.js · style.css · logo.png（无框架、无构建）
├── assets/icon.png       图标源（1024，打包脚本从它生成 .icns / .ico）
├── packaging/macos/      `.app` 外壳样板（Info.plist）
├── scripts/
│   ├── make-app.sh       打包 `lovart.app`（`make app`）
│   ├── make-exe.ps1      打包 `lovart-client.exe`（`make win`）
│   └── probe.mjs         「先红后绿」探针执行器
├── fixtures/             黄金用例：prompts + 脚本化上游 + 期望状态
├── inbox/                AI 写提示词的落点（见 inbox/README.md）
├── var/                  运行产物（state / report / refs-cache / log）
└── output/               出图
```

---

## 界面里几个容易误解的动作

界面上没有"进度条"，这是**刻意的**：上游 `/chat/status` 只回 `status`
（`{"data":{"thread_id":"…","status":"running"}}`，2026-10-01 直连实测）——
没有百分比、没有排队位次。所以「这一条生成到几成」**没有真值**，
画一条按时间猜的条就是编数字；扫动式装饰动画则永远到不了头，看着像卡死。
单条能回答的真话只有两件：**现在哪一步**（状态标签）+ **已经等了多久**。

| 动作 | 它到底做什么 | 什么时候用 |
|---|---|---|
| **垫图（参考图）** | 每条图生图任务卡上显示**这次用的垫图**（缩略图 + 地址），点缩略图看大图，`⋯ → 复制垫图地址` 可带走。地址来自记录里的 `refUrls`（`TaskRecord::ref_urls`），是那次上传到 Lovart CDN 的**真实产物地址** | 想核对"我垫的是哪张图" |
| **自动探测上游** | 打开界面时，对**本地没人跟的未完成任务**逐条 `GET /chat/status`（**只读**，不提交、不花钱）。结论写在左侧标题行：`上游：已中止 4、仍在跑 1` | 自动，无需操作 |
| **接着查** | 把**未完成**的记录重新交给调度器：有 `threadId` 就走**复查**（不重复提交），产物已出则只补下载。未完成的记录里若有垫图，记录里没存垫图本身 ⇒ 会被跳过并告知 | 标题行出现「N 未跟进」时 |
| **重跑异常** | 把**已到终态的异常**（失败 / 未产出…）重新**生成** —— **会再花一次额度** | 明确知道要重做 |
| **删除这条** | **真删**：读 → 滤 → **重写整个 `var/state.jsonl`**（临时文件 + 原子 rename）。只删记录，**不动磁盘上的图** | 收拾残记录 / 不想要了 |

> ⚠️ **删除有一道闸：跑批中不许删。** 删除要重写整个状态文件，而调度器正在往同一个
> 文件**追加写** —— 并发会让窗口期内写的行被**静默冲掉**（不报错、不告警，就是少了）。
> 这条前置条件在 `StateStore::remove` 的注释里写明，由调用方保证。

> ⚠️ **垫图记录的两个坑（2026-10-01 已修，别再踩）**：
> ① `new_record` 曾把 `ref_signature` 写死成 `None` ⇒ **第一次跑的图生图任务记录里
>    什么垫图线索都没有**（后面只在"参数变了"的补丁分支才写，而新任务走不到那里）；
> ② 记录里只存**指纹**（单向哈希）⇒ 反推不出图，界面上连看一眼都做不到。
> 现在的口径：**指纹 + CDN 地址都存**（`refSignature` + `refUrls`），
> 且在"参数变了"时**同进同退**（只改指纹不改地址 = 界面拿旧垫图冒充新的）。
> 这两个坑合起来的真实后果很重：任务重跑时 `same_params` 判定成立而垫图其实已丢，
> 于是**图生图静默退化成文生图** —— 出的完全不是一回事，而且不报错。

> ⚠️ **为什么会有"没人跟"的未完成任务**：调度器的队列**只由当前提示词文件派生**。
> 上一次被中断（`Ctrl+C` / 进程被杀）留下的 `running`，如果不在下一批的提示词里，
> 就**永远没人再查** —— 既不会完成，也不会变红。所以界面按
> 「**本批次有没有接管它**」（`following`）而不是「状态是什么」来画卡片，
> 并提供「接着查」把它们接回来（README 里「下次自动复查」的兑现口）。


**运行时依赖为 0**：`lovart-client` 是自包含二进制，不需要 Node、不需要终端。

> **迁移期已结束**：早先的 TS 参照实现（`ts-reference/`）已退场 —— Rust 侧回放 `fixtures/`
> 与 TS 产出逐条一致后，它就没有存在的必要了。本仓现在是**一份实现**。
>
> ⚠️ 由此带来一个后果：**`fixtures/*/expected-state.jsonl`（黄金）不能再自动生成**
> （生成器是 TS 侧的 `GOLDEN_WRITE=1 npm run check`）。要改黄金只能手写，
> 所以 `tests/golden_replay.rs` 里除比对外还有**显式规则断言** —— 黄金本身可能是错的。

**验证「测试真有鉴别力」**：用 `scripts/probe.mjs`（注入一处 → 跑命令 → 精确断言 → 自动还原）。

```bash
node scripts/probe.mjs --label '…' --file src-tauri/src/engine/state.rs \
  --find '<原实现>' --replace '<写坏>' --run 'cd src-tauri && cargo test' \
  --expect-exit 101 --expect-out '<哪条测试该红>'
```

它比"跑一遍看红不红"强在四处：**强制注入点唯一**（0 处 / >1 处都拒绝）·
**基线必绿前置**（防"本来就红"被当成"先红成功"）· **`--expect-out` 精确要求红在那一条** ·
**journal 兜底 + 还原自校验**（被 Ctrl+C 也不会把注入态留在工作区）。

> ⚠️ `cargo test` 失败时退出码是 **101**（不是 vitest 的 1）。
> ⚠️ `make check` 全绿只证明「和上次一样」，**不证明「规则真的成立」**。
