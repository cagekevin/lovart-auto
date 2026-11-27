# lovart 自动生图

用 Lovart 的**排队模式（unlimited，免费）**批量出图：在图形界面里编辑提示词、垫图，
最多 7 个任务并发，断点续跑，只出图。**一个二进制**：双击开图形界面。

> 本文件写「怎么用」。为什么这么设计、怎么打包、怎么验证 —— 见 [`docs/详细说明.md`](docs/详细说明.md)。
> 其他程序（AI / 脚本）想接入 —— 见 [`docs/接入指南-本机HTTP-其他前端-2026-11-20.md`](docs/接入指南-本机HTTP-其他前端-2026-11-20.md)。

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

## 其他程序怎么用（本机 HTTP）

GUI 启动时会在 `127.0.0.1:17863` 开一个本机 HTTP 服务，token 在 `data/var/http-token`。

```text
① 读 data/var/http-token
② GET  /state        看当前有没有在跑、任务到哪一步
③ POST /tasks        提交（想让人先确认再开跑 ⇒ POST /drafts）
```

**契约细节**（字段、状态表、限制、curl 示例）全在
[`docs/接入指南-本机HTTP-其他前端-2026-11-20.md`](docs/接入指南-本机HTTP-其他前端-2026-11-20.md)，这里不复述。

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
| `model` | string | — | 别名见 GUI发送面板的模型下拉；缺省用设置页的兜底模型 |
| `ratio` | string | — | `w:h`，每边 1–3 位数字；缺省用兜底比例 |
| `size` | string | — | `1K` / `2K` / `4K`；缺省用兜底尺寸 |
| `refs` | string[] | — | 参考图（本机路径 / URL / `data:`），非空数组 |

写错任何一处（未知字段、未知模型、非法 `ratio` / `size`、`prompt` 为空）都会**报错并带行号**，
不会静默兜底。空行跳过。

---

## 出图在哪、出问题看哪

| 想知道 | 看 |
|---|---|
| 图 | `data/output/` |
| 每条状态 | GUI 看板 / `GET /state` / `data/var/state.jsonl` |
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
├── src-tauri/            唯一实现（Rust）：GUI + 本机 HTTP
├── ui/                   前端：index.html · app.js · style.css（无框架、无构建）
├── packaging/            打包输入（图标源 + macOS plist）
├── scripts/              打包与探针脚本
├── fixtures/             黄金用例
└── data/                 唯一的运行数据根（启动自动迁移旧布局）
    ├── .env              凭据
    ├── inbox/            AI 写提示词的落点
    ├── presets/          预设：清单 + 图（plan 18）
    ├── library/          素材库（plan 20：工具条「素材库」）
    ├── chat-history.json AI 对话的会话真相（plan 21，整表原子写）
    ├── chat-system-prompt.txt  AI 对话的系统提示词
    ├── output/           出图
    └── var/              状态 / 报告 / refs / 缓存 / log / lock
```

运行时依赖为 0：`lovart-client` 是自包含二进制，不需要 Node、不需要终端。
