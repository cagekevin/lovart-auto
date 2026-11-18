# lovart 自动生图

用 Lovart 的**排队模式（unlimited，免费）**批量出图：读一批提示词，最多 7 个任务并发，
断点续跑，只出图。**一个二进制**：双击 = 图形界面；带参数 = 命令行。

> 本文件是**AI 操作这个工具的说明书**：只写 AI 要做的事，四步，没有分支。
> 界面怎么用、为什么这么设计、怎么打包、怎么验证 —— 见 [`docs/详细说明.md`](docs/详细说明.md)。

---

## AI 只做四步

| # | 做什么 | 命令 | 花钱 |
|---|---|---|---|
| 1 | 写提示词，一行一条 JSON | 落到 `inbox/<日期>-<主题>.jsonl` | 否 |
| 2 | **先检查**，按报错改到通过 | `lovart-client --check --prompts inbox/x.jsonl` | 否 |
| 3 | 检查通过后生成 | `lovart-client --prompts inbox/x.jsonl` | 占额度 |
| 4 | 查进度、确认产出 | `lovart-client --status --json` | 否 |

**就这四步。** 检查不通过不许跑第 3 步；跑完把「成功几条、失败哪几条」报给用户，不自行重跑。

### 不要做的四件事

1. **不传运行参数。** `--concurrency`、`--task-interval-ms`、`--max-*`、`--*-timeout`、
   `--download-retries` 都不存在（传了报错退出）。并发 / 间隔 / 重试 / 超时只由用户在界面「设置」里改。
2. **不加 `--auto-confirm`。** 自动确认会**扣积分**，那是用户的决定。
3. **不手改** `var/state.jsonl`、`var/report.csv`、`.env`。状态文件只有一个写者（调度器）。
4. **不猜。** 模型别名用 `--list-models` 查；报错照原文回报，不要自己改参数兜底。

```bash
./lovart-client --check --prompts inbox/x.jsonl   # 有错：非零退出 + 行号
./lovart-client --list-models                     # 权威模型别名表
```

---

## 快速开始（人）

```bash
cp .env.example .env        # 填 LOVART_ACCESS_KEY / LOVART_SECRET_KEY
make app                    # macOS → lovart.app（双击开界面）
make win                    # Windows → lovart-client.exe
```

工作区 = 放 `.env` / `inbox/` / `var/` / `output/` 的那一层。macOS 上 `.app` 要和它们同级，
Windows 上 exe 要和它们同级。

> ⚠️ 改过 `ui/` 必须重新 `make app` / `make win` —— 前端是**编译期**嵌进二进制的。

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
| `--prompts <file>` | `prompts.jsonl` | 提示词文件（JSONL） |
| `--check` | — | 只校验，不联网、不提交 |
| `--dry-run` | — | 打印计划，不提交 |
| `--status` / `--json` / `--full` | — | 任务看板（只读状态文件，随时可跑） |
| `--list-models` / `--json` | — | 模型别名表（从代码生成） |
| `--status-table` | — | 状态机全表（从代码渲染） |
| `--output <dir>` | `output` | 出图目录 |
| `--state <file>` | `var/state.jsonl` | 断点续跑状态 |
| `--report <file>` / `--no-report` | `var/report.csv` | 结果报告（CSV，带 BOM） |
| `--ref <路径\|URL\|data:>` | — | 参考图，可重复或逗号分隔 |
| `--refs-cache <file>` / `--no-refs-cache` | `var/refs-cache.json` | 参考图缓存 |
| `--model` / `--ratio` / `--size` | 设置页兜底 | 本次运行的兜底参数 |
| `--retry-failed` | — | 重置无 threadId 的失败并重跑 |
| `--allow-unknown-model` | — | 未知模型只警告（**默认终止**） |
| `--auto-confirm` / `--no-auto-confirm` | `.env` | 自动确认（**会扣积分**） |

运行参数（并发 / 提交间隔 / 重试 / 超时）**没有命令行入口**：只能由用户在界面「设置」里改
（写入 `.env`）。传了会报错并提示去设置页，不是静默忽略。

---

## 出图在哪、出问题看哪

| 想知道 | 看 |
|---|---|
| 图 | `output/` |
| 每条状态 | `--status`（或 `var/state.jsonl`） |
| 结果汇总 | `var/report.csv` |
| 几点断的、断在哪一步 | `var/run.log`（带毫秒时间戳，超 8MB 轮转） |

```bash
tail -n 50 var/run.log          # 昨晚最后 50 行 = 断在哪一步
grep -c "已提交" var/run.log     # 那一晚提交成功几条
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
├── assets/icon.png       图标源
├── scripts/              打包与探针脚本
├── fixtures/             黄金用例
├── inbox/                AI 写提示词的落点
├── var/                  运行产物（state / report / refs-cache / log）
└── output/               出图
```

运行时依赖为 0：`lovart-client` 是自包含二进制，不需要 Node、不需要终端。
