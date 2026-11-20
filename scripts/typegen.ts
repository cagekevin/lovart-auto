/**
 * 契约对账（**只出事实 · 不是闸**）—— 本仓没有 TS 类型生成环节，同一条命令改成对账。
 *
 * 【为什么保留这个文件名】原先它是 Vite+ 脚手架里的 `vite.resolveConfig()` 调用：
 *   目的是让 TanStack Router / Start 在 `configResolved` 阶段把路由类型写进 `apps/web`。
 *   本仓前端是**无框架、无构建**的 `ui/*.js`，后端是 Rust ⇒ **没有类型生成这一步**。
 *   但这个文件名被 `package.json` 时代的脚本链引用过，直接删掉会留下「以前有个 typegen」的悬念；
 *   修法是**换成它在本仓唯一还成立的那件事**：`prompts.jsonl` 的字段契约在三处各写一遍
 *   （Rust 白名单 / README 表 / 实际 inbox 文件），**对账**比生成更值钱。
 *
 * 【口径】
 *   · 真源 = `crates/lovart-core/src/lovart/prompt.rs` 的 `ALLOWED_KEYS`（代码即契约）。
 *   · `README.md` 的「prompts.jsonl」字段表是它的**副本**，不一致就是文档漂移。
 *   · `inbox/*.jsonl` 里出现过的键，超出白名单 = AI 正在写非法字段（运行时会 fail-loud，但提前看到更好）。
 *
 * 【边界】恒 `exit 0`（只有「读不到真源」才 exit 2）：它回答「三处是否一致」，不判合规、
 *   不阻断、不进 `make check` —— 与 `scan.mjs` / `probe.mjs` 同一纪律（`docs/adr/ADR-0009`）。
 *
 * 用法：
 *   node scripts/typegen.ts            # 对账并打印
 *   node scripts/typegen.ts --json     # 给 agent 读
 */
import { readFileSync, readdirSync, existsSync } from "node:fs";
import { dirname, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const json = process.argv.includes("--json");

const fail = (message: string, fix: string) => {
  console.error(`❌ typegen（契约对账）：${message}`);
  console.error(`   ↳ ${fix}`);
  process.exit(2);
};

const read = (relativePath: string) => {
  const full = join(root, relativePath);
  if (!existsSync(full)) return null;
  return readFileSync(full, "utf8");
};

/** 真源：Rust 的字段白名单 */
const codeSource = read(join("crates", "lovart-core", "src", "lovart", "prompt.rs"));
if (codeSource === null) {
  fail(
    "读不到 crates/lovart-core/src/lovart/prompt.rs",
    "确认 crate 路径没变（拆 crate 后这里改过一次）；本命令不会猜。"
  );
}
const keysBlock = codeSource.match(/const\s+ALLOWED_KEYS\s*:[^=]*=\s*\[([^\]]*)\]/);
if (!keysBlock) {
  fail(
    "prompt.rs 里找不到 `const ALLOWED_KEYS = [...]`",
    "字段白名单搬走了就同步改本脚本的抽取正则 —— 不要退回到「读文档」当真源。"
  );
}
const codeKeys = [...keysBlock[1].matchAll(/"([^"]+)"/g)].map((m) => m[1]);

/** 副本：README 的字段表 */
const readmeSource = read("README.md") ?? "";
const readmeKeys = [...readmeSource.matchAll(/^\|\s*`([a-z_]+)`\s*\|/gm)].map((m) => m[1]);

/** 实际用法：inbox 里真的写过哪些键 */
const inboxDir = join(root, "inbox");
/** inbox 是分层的（`inbox/imported/*.jsonl`），必须递归 —— 只看第一层会得出「没写过」的假事实 */
const jsonlUnder = (dir: string, found: string[] = []): string[] => {
  if (!existsSync(dir)) return found;
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) jsonlUnder(full, found);
    else if (entry.name.endsWith(".jsonl")) found.push(full);
  }
  return found;
};

const usedKeys = new Map<string, Set<string>>();
if (existsSync(inboxDir)) {
  for (const full of jsonlUnder(inboxDir)) {
    const name = full.slice(inboxDir.length + 1).split(sep).join("/");
    const text = readFileSync(full, "utf8");
    for (const line of text.split(/\r?\n/)) {
      if (!line.trim()) continue;
      let parsed: unknown;
      try {
        parsed = JSON.parse(line);
      } catch {
        continue;
      }
      if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) continue;
      for (const key of Object.keys(parsed as Record<string, unknown>)) {
        if (!usedKeys.has(key)) usedKeys.set(key, new Set());
        usedKeys.get(key)!.add(name);
      }
    }
  }
}

const missingInReadme = codeKeys.filter((key) => !readmeKeys.includes(key));
const extraInReadme = readmeKeys.filter((key) => !codeKeys.includes(key));
const unknownInInbox = [...usedKeys.keys()].filter((key) => !codeKeys.includes(key));
const unusedKeys = codeKeys.filter((key) => !usedKeys.has(key));

const report = {
  generatedAt: new Date().toISOString(),
  sources: {
    code: "crates/lovart-core/src/lovart/prompt.rs · ALLOWED_KEYS",
    doc: "README.md · prompts.jsonl 字段表",
    usage: "inbox/*.jsonl",
  },
  codeKeys,
  readmeKeys,
  missingInReadme,
  extraInReadme,
  unknownInInbox,
  unusedKeys,
  inboxFiles: usedKeys.size === 0 ? 0 : new Set([...usedKeys.values()].flatMap((set) => [...set])).size,
};

if (json) {
  console.log(JSON.stringify(report, null, 2));
} else {
  console.log("🔎 typegen（契约对账 · 只出事实）｜ prompts.jsonl 字段");
  console.log(`   真源（代码）：${codeKeys.join(" / ") || "（空）"}`);
  console.log(`   副本（README）：${readmeKeys.join(" / ") || "（空）"}`);
  console.log(`   实际用法（inbox ${report.inboxFiles} 个文件）：${[...usedKeys.keys()].join(" / ") || "（没写过）"}`);

  if (missingInReadme.length === 0 && extraInReadme.length === 0) {
    console.log("\n   ✅ 代码白名单与 README 表一致");
  } else {
    if (missingInReadme.length > 0) {
      console.log(`\n   ⚠️ README 少了：${missingInReadme.join(" / ")}（代码已接受，文档没写 ⇒ AI 会不知道能写）`);
    }
    if (extraInReadme.length > 0) {
      console.log(`   ⚠️ README 多了：${extraInReadme.join(" / ")}（文档承诺了代码不认的字段 ⇒ 写了会被拒）`);
    }
  }
  if (unknownInInbox.length > 0) {
    console.log(`\n   ⚠️ inbox 里出现过白名单外的键：${unknownInInbox.join(" / ")}（运行时会报错并带行号）`);
  }
  if (unusedKeys.length > 0) {
    console.log(`\n   · 到现在没被用过的字段：${unusedKeys.join(" / ")}（不是错，是事实）`);
  }
  console.log("\n   ↳ 这是对账单，不是罚单：不一致时先改文档或改代码，别把脚本改松。");
}

process.exit(0);
