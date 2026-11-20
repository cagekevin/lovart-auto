#!/usr/bin/env node
/**
 * 代码体检扫描器（**只出事实 · 不是闸**）——「复杂度 × 覆盖率」+「结构重复」。
 *
 * 【为什么重写（2026-11-20）】原文件是治理 kit 从 **Node/Vite 脚手架**带来的复制件：
 *   `import { parseSync } from 'oxc-parser'` + 读 `pnpm-workspace.yaml` + 读 vitest 的 LCOV。
 *   本仓是 **Rust（cargo workspace）+ 无框架前端**，没有 `package.json` / `node_modules`
 *   ⇒ `node scripts/scan.mjs` 直接 `ERR_MODULE_NOT_FOUND`（旧账 TD-06-1）。
 *   修法不是删，而是**把它对准本仓骨架**，且**零依赖**（只用 Node 内置模块）。
 *
 * 【机制定位：只出事实，不是闸】恒 `exit 0`（参数/环境错才 exit 2），不进 `make check`、
 *   不接提交钩子、不判合规。同一个理由见 `docs/adr/ADR-0009`：正则式「某处不许出现 X」的闸
 *   会被换个写法绕过，只出事实的扫描没有绕路激励。⇒ **一条纪律：不要为了让这张表好看而改码。**
 *
 * 【口径（写死在这里，便于复核）】—— 没有 AST，全部是**带遮蔽的关键字/字符计数**，
 *   所以数字是**近似值**，只用来排序「最该先看的地方」，不能当判据。
 *
 *   · 扫描范围：`crates/**` 与 `src-tauri/src/**` 的 `.rs`；`ui/**` 的 `.js`。
 *     跳过 `target/` `node_modules/` `.git/` `dist/` `.metrics/` `tests/`（测试不算业务代码）。
 *   · 条目：`fn NAME(` / `function NAME(` / `const NAME = (…) =>` / `const NAME = function`。
 *     **匿名闭包并入外层**；函数体用**花括号配对**定位（跳过注释与字符串）。
 *   · CC（圈复杂度）= 1 + 决策点：`if`（`else if` 只算一次）· `match` 的 `=>` 臂 ·
 *     `&&` `||` · `for` `while` `loop` · `?` 提前返回 · JS 的三元。
 *   · 覆盖率：读 `lcov.info`（`cargo llvm-cov --lcov` 产出），按条目行区间取 DA/BRDA；
 *     找不到报告 ⇒ 全按 0%，并在输出里说明（**不要**把它读成「覆盖率真的 0」）。
 *   · CRAP = CC² × (1 − 覆盖率)³ + CC，从高到低排。
 *   · 重复：把函数体归一化成 token 序列（局部名 → `ID`、字面量 → `LIT`、被调用名保留），
 *     取 4-gram 集合的 **Jaccard** 相似度；默认门槛 0.82，条目至少 4 行 / 20 token。
 *   · 相似度**不可跨工具比较**（原实现用 oxc 的 AST 形状，本实现用 token 序列）。
 *
 * 【用法】
 *   node scripts/scan.mjs                  # 打印表 + 写 .metrics/scan.json
 *   node scripts/scan.mjs --json           # 只输出 JSON
 *   node scripts/scan.mjs --limit 50       # 表里最多 50 行（默认 20）
 *   node scripts/scan.mjs --changed        # 只扫 git 改动碰过的文件
 *   node scripts/scan.mjs --threshold 0.9  # 重复门槛
 *   node scripts/scan.mjs --no-write       # 不写 .metrics/scan.json
 *   node scripts/scan.mjs --root ../other  # 扫别的目录（默认本脚本所在项目的根）
 *
 * 【与其它入口的边界】`make check` 答「对不对」，`probe.mjs` 答「这条检查会不会红」，
 *   本脚本只答「哪里又大又没测 / 哪里长得像」。三者互不替代。
 */
import { execFileSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const scriptRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");

const SKIP_DIRS = new Set([
  ".git",
  ".metrics",
  "node_modules",
  "target",
  "dist",
  "build",
  "vendor",
  "tests",
  "__tests__",
  "fixtures",
]);

const die = (message, fix) => {
  console.error(`❌ scan：${message}`);
  if (fix) console.error(`   ↳ ${fix}`);
  process.exit(2);
};
const flag = (name) => process.argv.includes(name);
const value = (name, fallback) => {
  const i = process.argv.indexOf(name);
  const next = process.argv[i + 1];
  return i >= 0 && next && !next.startsWith("--") ? next : fallback;
};
const number = (name, fallback) => {
  const raw = value(name, "");
  if (raw === "") return fallback;
  const n = Number(raw);
  if (!Number.isFinite(n)) die(`${name} 需要数字，收到「${raw}」`);
  return n;
};

const root = resolve(value("--root", scriptRoot));
if (!existsSync(root)) die(`项目根不存在：${root}`, "传 --root 指向项目根，或从项目内运行本脚本");
const limit = number("--limit", 20);
const threshold = number("--threshold", 0.82);
const minLines = number("--min-lines", 4);
const minNodes = number("--min-nodes", 20);
const posix = (p) => p.split(sep).join("/");

// ── 文件枚举 ────────────────────────────────────────────────────────────────
const sourceFiles = [];
const walk = (dir) => {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (!SKIP_DIRS.has(entry.name)) walk(full);
      continue;
    }
    const rel = posix(relative(root, full));
    const isRust = entry.name.endsWith(".rs");
    const isUi = entry.name.endsWith(".js") && rel.startsWith("ui/");
    if (!(isRust || isUi)) continue;
    if (/(?:^|\/)(?:tests?|__tests__)\//.test(rel)) continue;
    if (/\.(?:test|spec)\.[^./]+$/u.test(entry.name)) continue;
    sourceFiles.push({ abs: full, rel, kind: isRust ? "rs" : "js" });
  }
};
walk(root);

const changedFiles = () => {
  const out = new Set();
  for (const args of [
    ["diff", "--name-only", "HEAD"],
    ["ls-files", "--others", "--exclude-standard"],
  ]) {
    try {
      // stderr 丢掉：git 的 CRLF 警告会混进体检报告，读表的人会以为那是扫描结论
      const text = execFileSync("git", args, {
        cwd: root,
        encoding: "utf8",
        stdio: ["ignore", "pipe", "ignore"],
      });
      for (const line of text.split("\n")) if (line.trim()) out.add(posix(line.trim()));
    } catch {
      // 非 git 环境 → 退化为全量扫（--changed 只在有 git 时有意义）
    }
  }
  return out;
};
const changed = flag("--changed");
const touched = changed ? changedFiles() : undefined;
for (const file of sourceFiles) {
  file.selected = touched ? touched.has(file.rel) : true;
}
const selected = sourceFiles.filter((f) => f.selected);

// ── 注释与字符串遮蔽（保持字节长度，行号不变）──────────────────────────────
const mask = (text) => {
  const out = text.split("");
  const blank = (from, to) => {
    for (let i = from; i < to && i < out.length; i += 1) {
      if (out[i] !== "\n") out[i] = " ";
    }
  };
  let i = 0;
  while (i < text.length) {
    const ch = text[i];
    const next = text[i + 1];
    if (ch === "/" && next === "/") {
      const end = text.indexOf("\n", i);
      const stop = end === -1 ? text.length : end;
      blank(i, stop);
      i = stop;
      continue;
    }
    if (ch === "/" && next === "*") {
      let depth = 1;
      let j = i + 2;
      while (j < text.length && depth > 0) {
        if (text[j] === "/" && text[j + 1] === "*") {
          depth += 1;
          j += 2;
        } else if (text[j] === "*" && text[j + 1] === "/") {
          depth -= 1;
          j += 2;
        } else {
          j += 1;
        }
      }
      blank(i, j);
      i = j;
      continue;
    }
    if (ch === '"' || (ch === "r" && /[#"']/.test(next ?? "")) || (ch === "b" && next === '"')) {
      // 原始字符串 r"…" / r#"…"# / br#"…"#
      const raw = text.slice(i).match(/^b?r(#*)"/);
      if (raw) {
        const close = `"${raw[1]}`;
        const end = text.indexOf(close, i + raw[0].length);
        const stop = end === -1 ? text.length : end + close.length;
        blank(i, stop);
        i = stop;
        continue;
      }
      if (ch === '"' || ch === "b") {
        let j = i + (ch === "b" ? 2 : 1);
        while (j < text.length) {
          if (text[j] === "\\") {
            j += 2;
            continue;
          }
          if (text[j] === '"' || text[j] === "\n") {
            j += 1;
            break;
          }
          j += 1;
        }
        blank(i, j);
        i = j;
        continue;
      }
    }
    if (ch === "'" && /[^\\\n]/.test(text[i + 1] ?? "") && text[i + 2] === "'") {
      blank(i, i + 3); // 字符字面量（不含生命周期）
      i += 3;
      continue;
    }
    if (ch === "`") {
      // 模板字符串（JS）：遮蔽但保留 `${…}` 之外的内容
      let j = i + 1;
      while (j < text.length) {
        if (text[j] === "\\") {
          j += 2;
          continue;
        }
        if (text[j] === "`") {
          j += 1;
          break;
        }
        j += 1;
      }
      blank(i, j);
      i = j;
      continue;
    }
    i += 1;
  }
  return out.join("");
};

/** 行号索引：offset → 1 起行号 */
const lineOf = (text) => {
  const starts = [0];
  for (let i = 0; i < text.length; i += 1) if (text[i] === "\n") starts.push(i + 1);
  return (offset) => {
    let lo = 0;
    let hi = starts.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (starts[mid] <= offset) lo = mid;
      else hi = mid - 1;
    }
    return lo + 1;
  };
};

const ITEM_RE =
  /\b(?:fn\s+([A-Za-z_]\w*))|\b(?:function\s+([A-Za-z_]\w*))|\bconst\s+([A-Za-z_]\w*)\s*=\s*(?:async\s*)?(?:function\b|\()/g;

const matchingBrace = (masked, openAt) => {
  let depth = 0;
  for (let i = openAt; i < masked.length; i += 1) {
    const ch = masked[i];
    if (ch === "{") depth += 1;
    else if (ch === "}") {
      depth -= 1;
      if (depth === 0) return i;
    }
  }
  return -1;
};

const DECISIONS = [
  /\bfor\b/g,
  /\bwhile\b/g,
  /\bloop\b/g,
  /\bmatch\b/g,
  /&&/g,
  /\|\|/g,
  /(?<=[A-Za-z0-9_)\]"'])\s*\?(?![.\w])/g, // 提前返回 / 三元
  /\bcase\b/g,
];

const countDecisions = (body) => {
  let ifs = (body.match(/\bif\b/g) ?? []).length;
  const elseIfs = (body.match(/\belse\s+if\b/g) ?? []).length;
  ifs -= elseIfs;
  const arrows = (body.match(/=>/g) ?? []).length;
  let total = ifs + arrows;
  for (const re of DECISIONS) total += (body.match(re) ?? []).length;
  return total;
};

const entriesInFile = (file) => {
  let text;
  try {
    text = readFileSync(file.abs, "utf8");
  } catch {
    return [];
  }
  const masked = mask(text);
  const atLine = lineOf(masked);
  // 文件内的测试模块（`#[cfg(test)] mod tests { … }`）不算业务代码 —— 与排除 tests/ 目录同口径
  const testRanges = [];
  const TEST_MOD_RE = /#\[cfg\(\s*test\s*\)\]\s*(?:pub\s+)?mod\s+[A-Za-z_]\w*\s*\{/g;
  let testMatch;
  while ((testMatch = TEST_MOD_RE.exec(masked)) !== null) {
    const openAt = masked.indexOf("{", testMatch.index);
    const closeAt = matchingBrace(masked, openAt);
    if (closeAt !== -1) testRanges.push([openAt, closeAt]);
  }
  const inTest = (offset) => testRanges.some(([from, to]) => offset > from && offset < to);
  const out = [];
  ITEM_RE.lastIndex = 0;
  let match;
  while ((match = ITEM_RE.exec(masked)) !== null) {
    if (inTest(match.index)) continue;
    const name = match[1] ?? match[2] ?? match[3] ?? "(anon)";
    // 找函数体起点：从签名后第一个 `{`；遇到 `;` 先到 ⇒ 是声明（trait / 函数指针），跳过
    let cursor = match.index + match[0].length;
    let openAt = -1;
    for (; cursor < masked.length; cursor += 1) {
      const ch = masked[cursor];
      if (ch === ";" || ch === "=") break;
      if (ch === "{") {
        openAt = cursor;
        break;
      }
    }
    if (openAt === -1) continue;
    const closeAt = matchingBrace(masked, openAt);
    if (closeAt === -1) continue;
    const body = masked.slice(openAt + 1, closeAt);
    const startLine = atLine(match.index);
    const endLine = atLine(closeAt);
    out.push({
      path: file.rel,
      name,
      startLine,
      endLine,
      lines: endLine - startLine + 1,
      complexity: 1 + countDecisions(body),
      body,
    });
    ITEM_RE.lastIndex = closeAt; // 不把内层闭包当条目
  }
  return out;
};

// ── 覆盖率（LCOV）──────────────────────────────────────────────────────────
const coverageReports = () => {
  const candidates = [
    "lcov.info",
    join("target", "coverage", "lcov.info"),
    join("coverage", "lcov.info"),
    join("crates", "lcov.info"),
  ].map((p) => join(root, p));
  return candidates.filter((p) => existsSync(p));
};

const parseLcov = (text) => {
  const files = new Map();
  let current = null;
  const flush = () => {
    if (current && current.lines.size > 0) files.set(current.path, current.lines);
  };
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim();
    if (line.startsWith("SF:")) {
      flush();
      current = { path: posix(line.slice(3)), lines: new Map() };
    } else if (line.startsWith("DA:") && current) {
      const [lineNo, hits] = line.slice(3).split(",");
      const n = Number(lineNo);
      if (Number.isFinite(n)) current.lines.set(n, Number(hits) || 0);
    } else if (line === "end_of_record") {
      flush();
      current = null;
    }
  }
  flush();
  return files;
};

/** 路径键：以「相对仓库根」为主，读不到就退到后缀匹配（LCOV 里的路径常带绝对前缀）。 */
const coverageIndex = (() => {
  const index = new Map();
  for (const report of coverageReports()) {
    let parsed;
    try {
      parsed = parseLcov(readFileSync(report, "utf8"));
    } catch {
      continue;
    }
    for (const [path, lines] of parsed) {
      if (!index.has(path)) index.set(path, lines);
    }
  }
  return index;
})();

const linesFor = (entry) => {
  for (const [path, lines] of coverageIndex) {
    if (path === entry.path || path.endsWith(`/${entry.path}`) || entry.path.endsWith(`/${path}`)) {
      return lines;
    }
  }
  return null;
};

/** 条目行区间内的覆盖率 = 命中行 / 有记录的行；区间内没有记录 ⇒ 0%（不知道 ≠ 覆盖） */
const coverageFor = (entry) => {
  const lines = linesFor(entry);
  if (!lines) return 0;
  let hit = 0;
  let recorded = 0;
  for (const [lineNo, hits] of lines) {
    if (lineNo < entry.startLine || lineNo > entry.endLine) continue;
    recorded += 1;
    if (hits > 0) hit += 1;
  }
  return recorded === 0 ? 0 : hit / recorded;
};

const crapScore = (cc, coverage) => cc * cc * Math.pow(1 - coverage, 3) + cc;

// ── 重复形状 ────────────────────────────────────────────────────────────────
const tokenize = (body) => {
  const tokens = [];
  const re = /[A-Za-z_]\w*|\d+(?:\.\d+)?|[^\s\w]/g;
  const raw = body.match(re) ?? [];
  for (let i = 0; i < raw.length; i += 1) {
    const token = raw[i];
    if (/^\d/.test(token)) {
      tokens.push("LIT");
    } else if (/^[A-Za-z_]/.test(token)) {
      const isCallee = raw[i + 1] === "(";
      const isKeyword =
        /^(?:fn|let|mut|if|else|match|for|while|loop|return|pub|crate|self|use|impl|struct|enum|trait|const|static|move|ref|as|in|where|async|await|dyn|fn|function|const|var|new|typeof|instanceof|of|catch|try|finally|throw|switch|case|default|break|continue|class|this|super|true|false|null|undefined|void|defer|do|yield)$/.test(
          token
        );
      if (isKeyword) tokens.push(token);
      else if (isCallee) tokens.push(`f:${token}`);
      else tokens.push("ID");
    } else {
      tokens.push(token);
    }
  }
  return tokens;
};

const fingerprints = (tokens) => {
  const set = new Set();
  for (let i = 0; i + 4 <= tokens.length; i += 1) {
    set.add(tokens.slice(i, i + 4).join(" "));
  }
  return set;
};
const jaccard = (left, right) => {
  if (left.size === 0 || right.size === 0) return 0;
  let shared = 0;
  for (const item of left) if (right.has(item)) shared += 1;
  return shared / (left.size + right.size - shared);
};

// ── 主流程 ──────────────────────────────────────────────────────────────────
const entries = [];
for (const file of selected) entries.push(...entriesInFile(file));

const rows = entries
  .map((entry) => {
    const coverage = coverageFor(entry);
    return {
      path: entry.path,
      name: entry.name,
      startLine: entry.startLine,
      endLine: entry.endLine,
      lines: entry.lines,
      complexity: entry.complexity,
      coverage,
      crap: Number(crapScore(entry.complexity, coverage).toFixed(1)),
    };
  })
  .sort((a, b) => b.crap - a.crap);

const comparable = entries
  .filter((e) => e.lines >= minLines)
  .map((e) => {
    const tokens = tokenize(e.body);
    return { entry: e, size: tokens.length, prints: fingerprints(tokens) };
  })
  .filter((e) => e.size >= minNodes);

const duplicates = [];
for (let i = 0; i < comparable.length; i += 1) {
  for (let j = i + 1; j < comparable.length; j += 1) {
    const a = comparable[i];
    const b = comparable[j];
    if (Math.min(a.size, b.size) / Math.max(a.size, b.size) < 0.6) continue;
    const similarity = jaccard(a.prints, b.prints);
    if (similarity < threshold) continue;
    duplicates.push({
      similarity: Number(similarity.toFixed(3)),
      left: {
        path: a.entry.path,
        name: a.entry.name,
        startLine: a.entry.startLine,
        endLine: a.entry.endLine,
      },
      right: {
        path: b.entry.path,
        name: b.entry.name,
        startLine: b.entry.startLine,
        endLine: b.entry.endLine,
      },
    });
  }
}
duplicates.sort((a, b) => b.similarity - a.similarity);

const hasCoverage = coverageIndex.size > 0;
const report = {
  generatedAt: new Date().toISOString(),
  root,
  files: selected.length,
  entries: entries.length,
  coverage: hasCoverage ? coverageReports().map((p) => posix(relative(root, p))) : [],
  crapTop: rows.slice(0, limit),
  duplicates: duplicates.slice(0, limit),
};

if (flag("--json")) {
  console.log(JSON.stringify(report, null, 2));
} else {
  console.log(
    `📊 scan（只出事实 · **不是闸**）｜ 文件 ${selected.length} · 条目 ${entries.length} · CC 为近似值（不建 AST）`
  );
  console.log(
    hasCoverage
      ? `   覆盖率来源：${report.coverage.join(" · ")}`
      : "   ⚠️ 没找到 lcov.info ⇒ 覆盖率全按 0% 计（CRAP 只反映复杂度）。生成：cargo llvm-cov --lcov --output-path lcov.info"
  );
  console.log("\n【CRAP 最高】（CC² × (1 − 覆盖率)³ + CC）");
  for (const row of report.crapTop) {
    console.log(
      `   ${String(row.crap).padStart(8)}  CC ${String(row.complexity).padStart(3)}  cov ${(row.coverage * 100).toFixed(0).padStart(3)}%  ${row.path}:${row.startLine}  ${row.name}`
    );
  }
  console.log(
    `\n【结构重复】相似度 ≥ ${threshold} · 至少 ${minLines} 行 / ${minNodes} token`
  );
  if (duplicates.length === 0) console.log("   （无）");
  for (const pair of report.duplicates) {
    console.log(`   ${pair.similarity}  ${pair.left.path}:${pair.left.startLine} ${pair.left.name}`);
    console.log(`           ${pair.right.path}:${pair.right.startLine} ${pair.right.name}`);
  }
  if (rows.length > limit) console.log(`\n   …… 另有 ${rows.length - limit} 个条目（--limit 调整）`);
  if (duplicates.length > limit) console.log(`   …… 另有 ${duplicates.length - limit} 对（--limit 调整）`);
  console.log("\n   ↳ 这是体检单，不是罚单：不要为了让表好看而改码。");
}

if (!flag("--no-write")) {
  try {
    const outDir = join(root, ".metrics");
    mkdirSync(outDir, { recursive: true });
    writeFileSync(join(outDir, "scan.json"), `${JSON.stringify(report, null, 2)}\n`);
  } catch (error) {
    console.error(`⚠️ 写 .metrics/scan.json 失败（不影响结论）：${error.message}`);
  }
}

process.exit(0);
