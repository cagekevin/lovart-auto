/**
 * 发布：把**已打好的产物**放到运行目录（工作区）—— 本仓没有云端发布环节，所以不构建、不上传。
 *
 * 【为什么保留这个文件名】原先它是 Vite+ 脚手架里的
 *   `vp exec --filter @my-app/web void deploy --platform cloudflare`：
 *   那个命令属于一个**部署到 Cloudflare 的 Web 应用**，本仓没有 Web 服务、也没有 Cloudflare。
 *   本仓的「发布」只有一个含义：`make app` / `make win` 打出来的产物，要和 `.env`、`inbox/`、`var/`
 *   **放在同一层**（`README.md`「快速开始」），双击即用。本文件就干这一件事。
 *
 * 【职责边界】
 *   · **不构建**：产物不存在就报错并给出构建命令（构建的唯一入口是 `make app` / `make win`）。
 *   · **不新增打包入口**：这里只搬文件，打包逻辑仍在 `scripts/make-app.sh` / `scripts/make-exe.ps1`。
 *   · 覆盖同名产物是**本命令的语义**（发布 = 换新），所以会先删旧目标再复制；目标与源同一位置时拒跑。
 *
 * 用法：
 *   node scripts/deploy.ts                      # 本平台产物 → 仓库根（工作区）
 *   node scripts/deploy.ts --to ../runtime      # → 指定工作区
 *   node scripts/deploy.ts --from dist/x.exe    # 指定产物（默认按平台推断）
 *   node scripts/deploy.ts --dry                # 只报「会做什么」，不落盘
 */
import { cpSync, existsSync, rmSync, statSync } from "node:fs";
import { basename, dirname, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");

const value = (name: string, fallback: string) => {
  const i = process.argv.indexOf(name);
  const next = process.argv[i + 1];
  return i >= 0 && next && !next.startsWith("--") ? next : fallback;
};
const dry = process.argv.includes("--dry");

const artifactName = process.platform === "darwin" ? "lovart.app" : "lovart-client.exe";
const buildHint =
  process.platform === "darwin"
    ? "make app（→ lovart.app）"
    : "make win（→ lovart-client.exe；必须 pwsh 7，不能用 powershell 5.1）";

const from = resolve(root, value("--from", artifactName));
const to = resolve(root, value("--to", "."));

if (!existsSync(from)) {
  console.error(`❌ deploy：没有产物 ${from}`);
  console.error(`   ↳ 先打包：${buildHint}`);
  process.exit(1);
}

const fromStat = statSync(from);
const target = join(to, basename(from));
const mb = (bytes: number) => `${(bytes / 1024 / 1024).toFixed(1)} MB`;
// 目录（.app）没有直接的「总大小」字段：递归求和是另一件事，这里只报「目录 + 修改时间」。
const size = fromStat.isDirectory() ? "目录" : mb(fromStat.size);

if (resolve(target) === from) {
  console.log(`✅ 已在位：${from}`);
  console.log(`   ${size} · ${fromStat.mtime.toISOString()}（发布目标就是它所在目录，无需搬动）`);
  process.exit(0);
}
if (target.startsWith(from + sep)) {
  console.error(`❌ deploy：目标 ${target} 在源目录里 —— 会把自己复制进自己。`);
  process.exit(1);
}
if (!existsSync(to)) {
  console.error(`❌ deploy：目标目录不存在（${to}）`);
  console.error("   ↳ 工作区要自己先建：放 .env / inbox/ / var/ / output/ 的那一层。不替你猜路径。");
  process.exit(1);
}

if (dry) {
  console.log(`（dry）${from} → ${target}`);
  console.log(`      类型 ${fromStat.isDirectory() ? "目录" : "文件"} · ${size}`);
  process.exit(0);
}

if (existsSync(target)) rmSync(target, { recursive: true, force: true });
cpSync(from, target, { recursive: fromStat.isDirectory(), force: true });

const after = statSync(target);
console.log(`✅ 已发布：${target}`);
console.log(`   来源 ${from}`);
console.log(`   类型 ${after.isDirectory() ? "目录" : "文件"} · ${after.isDirectory() ? size : mb(after.size)}`);
console.log(`   修改时间 ${after.mtime.toISOString()}`);
console.log("   ↳ 工作区要有 .env / inbox/ / var/ / output/；缺 .env 会起不来（见 README「快速开始」）。");
