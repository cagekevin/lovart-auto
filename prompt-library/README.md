# 外部提示词库（产品/电商/生活化摄影方向）

> 下载日期：2026-10-22
> 筛选标准：GitHub 高星仓库 + 官方文档；只保留与产品摄影、电商图、商业/广告摄影、生活方式产品场景相关的条目。
> 所有仓库均为 `git clone --depth 1` 完整下载，原始文件在 `repos/` 下，可随时本地检索。

## 已下载的仓库（repos/）

| 仓库 | 星标 | 内容 | 本地路径 |
|---|---|---|---|
| YouMind-OpenLab/awesome-nano-banana-pro-prompts | 13,659 | 15,798 条 Nano Banana Pro 提示词，含 E-commerce Main Image / Product Marketing 专门分类 | `repos/youmind-awesome-nano-banana-pro-prompts/` |
| ZeroLu/awesome-nanobanana-pro | 10,360 | 精选提示词，含 E-commerce & Virtual Studio 分类 | `repos/zerolu-awesome-nanobanana-pro/` |
| songguoxs/gpt4o-image-prompts | 3,854 | 中文提示词案例 1000+ 条（Nano Banana/GPT-4o/GPT-5/豆包） | `repos/songguoxs-gpt4o-image-prompts/` |
| xianyu110/awesome-nanobananapro-prompts | 815 | 908 个中文案例，附效果图 | `repos/xianyu110-awesome-nanobananapro-prompts/` |
| VigoZhao/AI-Visual-Prompt-Cookbook | 801 | 106+ 种 JSON 风格提示词（含产品海报/品牌广告风格） | `repos/vigozhao-ai-visual-prompt-cookbook/` |

## 已提取的产品/电商条目（本目录）

| 文件 | 来源 | 条数 | 匹配方式 |
|---|---|---|---|
| `提取-YouMind-产品电商.md` | YouMind（13.6k★） | 37 | 条目标题匹配 |
| `提取-songguoxs-产品电商.md` | songguoxs（3.8k★） | 36 | 案例标题匹配 |
| `提取-xianyu110-产品电商.md` | xianyu110（815★） | 13 | 正文匹配 |
| `01-ZeroLu-awesome-nanobanana-pro_产品电商.md` | ZeroLu（10.3k★） | 6 | E-commerce & Virtual Studio 分类全量 |
| `vigozhao-product-styles/` | VigoZhao（801★） | 6 个风格 JSON | 产品/品牌/广告类风格原文件 |
| `00-官方指南-Google-Nano-Banana-Pro提示词技巧.md` | Google 官方博客 | — | 官方提示词框架与摄影术语 |

## 说明

- 匹配关键词：`产品 / 商品 / 电商 / 商拍 / 静物 / 平铺 / 俯拍 / 拍摄 / 广告摄影 / product / e-commerce / commercial / packshot / still life / flat lay / lifestyle`
- 提取脚本：`extract.ps1`（可改关键词后重跑）
- 若要更高召回（标题没写但正文相关的条目），直接检索 `repos/` 里的原始文件；`提取-*.md` 优先保证精准。
