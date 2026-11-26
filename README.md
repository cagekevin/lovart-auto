# lovart 自动生图

用 Lovart 的**排队模式（unlimited，免费）**批量出图：读一批提示词，最多 7 个任务并发，
断点续跑，只出图。**一个二进制**：双击 = 图形界面；带参数 = 命令行。

> 本文件是**AI 操作这个工具的说明书**：只写 AI 要做的事，四步，没有分支。
> 界面怎么用、为什么这么设计、怎么打包、怎么验证 —— 见 [`docs/详细说明.md`](docs/详细说明.md)。

---

## AI 只做四步

| # | 做什么 | 命令 | 花钱 |
|---|---|---|---|
| 1 | 写提示词，一行一条 JSON | 落到 `data/inbox/<日期>-<主题>.jsonl` | 否 |
| 2 | **先检查**，按报错改到通过 | `lovart-client --check --prompts data/inbox/x.jsonl` | 否 |
| 3 | 检查通过后生成 | `lovart-client --prompts data/inbox/x.jsonl` | 占额度 |
| 4 | 查进度、确认产出 | `lovart-client --status --json` | 否 |

**就这四步。** 检查不通过不许跑第 3 步；跑完把「成功几条、失败哪几条」报给用户，不自行重跑。

### 不要做的四件事

1. **不传运行参数。** `--concurrency`、`--task-interval-ms`、`--max-*`、`--*-timeout`、
   `--download-retries` 都不存在（传了报错退出）。并发 / 间隔 / 重试 / 超时只由用户在界面「设置」里改。
2. **不加 `--auto-confirm`。** 自动确认会**扣积分**，那是用户的决定。
3. **不手改** `data/var/state.jsonl`、`data/var/report.csv`、`data/.env`。状态文件只有一个写者（调度器）。
4. **不猜。** 模型别名用 `--list-models` 查；报错照原文回报，不要自己改参数兜底。

```bash
./lovart-client --check --prompts data/inbox/x.jsonl # 有错：非零退出 + 行号
./lovart-client --list-models                     # 权威模型别名表
```

---

## 快速开始（人）

```bash
cp .env.example data/.env   # 填 LOVART_ACCESS_KEY / LOVART_SECRET_KEY（旧位置 .env 会自动迁移）
make app                    # macOS → lovart.app（双击开界面）
make win                    # Windows → lovart-client.exe
```

工作区 = 放 `data/` 的那一层（`data/` 内是 `.env` / `inbox/` / `var/` / `output/` 等，
旧布局会在启动时自动一次性迁移进去）。macOS 上 `.app` 要和工作区同级，Windows 上 exe 同样。

> ⚠️ 改过 `ui/` 必须重新 `make app` / `make win` —— 前端是**编译期**嵌进二进制的。

### AI 对话面板（可选，右上角「AI 对话」）

纯文本对话：**不出图、不发任务、不入待发队列**，但**与出图同一个额度账户**（每发一句就是一次计费调用）。

1. 写 `data/chat-system-prompt.txt`（纯文本，一句话即可）。
   **缺这个文件就不许发**，程序不会替你编一句默认提示词。
2. **不需要新的键**：对话走 Lovart 自有通道，复用 `data/.env` 里已有的
   `LOVART_ACCESS_KEY` / `LOVART_SECRET_KEY`（与出图同一对）。
3. 重开程序（凭据走进程环境，重开才载入 `.env`）。

会话存在 `data/chat-history.json`（整表原子写）。会话图片只存**来源路径**、不复制：
原图被删除后，那一轮请求里会显示成 `[图片缺失]`，对话照常能继续。

---

## prompts.jsonl

一行一条 JSON 对象：

```jsonl
{"prompt":"一只戴帽子的猫，水彩风格"}
{"prompt":"赛博朋克城市夜景","model":"nano-bn-pro","ratio":"16:9"}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `prompt` | string | ✅ | 非空 |
| `model` | string | — | 别名见 `--list-models`；缺省用设置页的兜底模型 |
| `ratio` | string | — | `w:h`，每边 1–3 位数字；缺省用兜底比例 |
| `size` | string | — | `1K` / `2K` / `4K`；缺省用兜底尺寸 |
| `refs` | string[] | — | 参考图（本机路径 / URL / `data:`），非空数组 |

写错任何一处（未知字段、未知模型、非法 `ratio` / `size`、`prompt` 为空）都会**报错并带行号**，
不会静默兜底。空行跳过。

---

## 参数

| 参数 | 默认 | 说明 |
|---|---|---|
| `--prompts <file>` | `data/prompts.jsonl` | 提示词文件（JSONL） |
| `--check` | — | 只校验，不联网、不提交 |
| `--dry-run` | — | 打印计划，不提交 |
| `--status` / `--json` / `--full` | — | 任务看板（只读状态文件，随时可跑） |
| `--list-models` / `--json` | — | 模型别名表（从代码生成） |
| `--status-table` | — | 状态机全表（从代码渲染） |
| `--output <dir>` | `data/output` | 出图目录 |
| `--state <file>` | `data/var/state.jsonl` | 断点续跑状态 |
| `--report <file>` / `--no-report` | `data/var/report.csv` | 结果报告（CSV，带 BOM） |
| `--ref <路径\|URL\|data:>` | — | 参考图，可重复或逗号分隔 |
| `--refs-cache <file>` / `--no-refs-cache` | `data/var/refs-cache.json` | 参考图缓存 |
| `--model` / `--ratio` / `--size` | 设置页兜底 | 本次运行的兜底参数 |
| `--retry-failed` | — | 重置无 threadId 的失败并重跑 |
| `--allow-unknown-model` | — | 未知模型只警告（**默认终止**） |
| `--auto-confirm` / `--no-auto-confirm` | `data/.env` | 自动确认（**会扣积分**） |

运行参数（并发 / 提交间隔 / 重试 / 超时）**没有命令行入口**：只能由用户在界面「设置」里改
（写入 `data/.env`）。传了会报错并提示去设置页，不是静默忽略。

---

## 出图在哪、出问题看哪

| 想知道 | 看 |
|---|---|
| 图 | `data/output/` |
| 每条状态 | `--status`（或 `data/var/state.jsonl`） |
| 结果汇总 | `data/var/report.csv` |
| 几点断的、断在哪一步 | `data/var/run.log`（带毫秒时间戳，超 8MB 轮转） |

```bash
tail -n 50 data/var/run.log      # 昨晚最后 50 行 = 断在哪一步
grep -c "已提交" data/var/run.log # 那一晚提交成功几条
```

断点续跑是自动的：任务 id = 提示词内容哈希，中断后重跑不会重复提交。

---

## 目录结构

```text
lovart 自动/
├── lovart.app/           macOS 应用包（`make app` 生成，不进版本库）
├── lovart-client.exe     Windows 单文件交付（`make win` 生成，不进版本库）
├── src-tauri/            唯一实现（Rust）：GUI 与 CLI 是同一个二进制
├── ui/                   前端：index.html · app.js · style.css（无框架、无构建）
├── packaging/            打包输入（图标源 + macOS plist）
├── scripts/              打包与探针脚本
├── fixtures/             黄金用例
└── data/                 唯一的运行数据根（启动自动迁移旧布局）
    ├── .env              凭据
    ├── prompts.jsonl     提示词文件（CLI 默认）
    ├── inbox/            AI 写提示词的落点
    ├── presets/          预设：清单 + 图（plan 18）
    ├── library/          素材库（plan 20：工具条「素材库」）
    ├── chat-history.json AI 对话的会话真相（plan 21，整表原子写）
    ├── chat-system-prompt.txt  AI 对话的系统提示词
    ├── output/           出图
    └── var/              状态 / 报告 / refs / 缓存 / log / lock
```

运行时依赖为 0：`lovart-client` 是自包含二进制，不需要 Node、不需要终端。
