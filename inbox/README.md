# inbox/ — AI 批量写提示词的落点

> **为什么单独一个目录**：AI 直接覆盖 `prompts.jsonl` 会**毁掉用户正在编辑的内容**。
> 所以给 AI 一个独立落点，不碰编辑区。

## 放什么

`*.jsonl`，**一行一条 JSON 对象**，格式与 `prompts.jsonl` **完全相同**。

📖 **完整契约见 [`../README.md`](../README.md) 的「prompts.jsonl 格式」一节** ——
字段穷举 / 合法值 / 自检流程都在那里（本文**不再抄一份**）。

建议文件名：`<日期>-<主题>-<条数>.jsonl`，例 `2026-10-01-国潮插画-200.jsonl`。

## 怎么用

```bash
# ① 先自检（不联网、不提交、不扣积分；有错按行号报出来）
lovart-client --check   --prompts inbox/2026-10-01-国潮插画-200.jsonl

# ② 再看计划（含每条的行号 / 模型 / 比例 / 尺寸）
lovart-client --dry-run --prompts inbox/2026-10-01-国潮插画-200.jsonl

# ③ 确认无误再跑
lovart-client           --prompts inbox/2026-10-01-国潮插画-200.jsonl
```

> `lovart-client` = `src-tauri/target/release/lovart-client`；**双击它开界面**。

> **AI 只写文件，不替用户提交。** 用户的流程是「**AI 写 → 我检查 → 我自己发**」。
> ⇒ AI 的活儿**到此为止**（写文件 + `--check` 自检）；
> 发任务由用户点 GUI 的「开始」，或自己跑上面的 ③。

> **导入后会归位**：GUI 里的「从 inbox 导入」会把文件**移进 `inbox/imported/`** ——
> 同一个文件因此不会被导入两次（重名加时间戳，**不覆盖**）。
> 需要再导一遍时去 `inbox/imported/` 里拿。

## 为什么不是 txt

文件格式的真实职责是「**AI ↔ 工具**」的契约。自研 txt DSL 要人肉对齐两套语义 ⇒ **必然分叉**；
JSONL 两边都用标准库解析 ⇒ **结构上不可能分叉**。
且旧 DSL 有个自己造出来的坑：`[nano-bn-pro]` 这种裸值会**静默丢配置**（模型退回默认），
跑完几百条才发现。JSON 里不存在这种歧义。

## 不做

- ❌ 目录 watcher 自动执行（AI 一写就跑 = 用户没机会看）
- ❌ 提示词库 / 历史版本 / 去重
- ❌ 双向 RPC / 命令协议（`--check` / `--list-models` / `--status` 已够）
