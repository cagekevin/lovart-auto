#!/usr/bin/env bash
#
# 打包 macOS 应用包 —— **双击即用**的 `lovart.app`。
#
#   ./scripts/make-app.sh          # 或：make app
#
# 产出：仓库根目录下的 `lovart.app`（同一份二进制；带参数跑就是 CLI）。
#
# ── 为什么要有这个脚本（而不是手敲命令）──────────────────────────────
# 手敲过一次，两次之间就会漂：漏了图标、忘了 ad-hoc 签名、Info.plist 少一个键，
# 表现都是"能开但不对劲"（图标空白 / 首次打开被 Gatekeeper 拦）。
# 打包是**可复现动作**，就该有唯一的脚本入口。
#
# ── 没有用 tauri-cli 的原因 ────────────────────────────────────────
# `cargo tauri build` 要额外装 CLI（一棵不轻的依赖树），而本工具**只需要一个
# .app 外壳**：前端资源在编译期已由 `tauri-build` 嵌进二进制（`frontendDist: ../ui`），
# 运行时不需要任何外部文件。所以外壳自己拼即可。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP="$ROOT/lovart.app"
ICON_SRC="$ROOT/assets/icon.png"
PLIST_SRC="$ROOT/packaging/macos/Info.plist"

# ── 1. 编译 ─────────────────────────────────────────────────────────
# 前端资源（`ui/`）是在**这一步**被嵌进去的 ⇒ 改过 `ui/` 必须重新 build，
# 只拷旧二进制不会更新界面。
echo "==> cargo build --release"
(cd "$ROOT/src-tauri" && cargo build --release)
BIN="$ROOT/src-tauri/target/release/lovart-client"
[ -x "$BIN" ] || { echo "构建产物不存在：$BIN" >&2; exit 1; }

# ── 2. 图标 ─────────────────────────────────────────────────────────
# `.icns` 只能由**标准命名的 iconset 目录**生成（`iconutil` 不认别的命名）。
echo "==> 生成 icon.icns"
ICONSET="$(mktemp -d)/lovart.iconset"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  retina=$((size * 2))
  sips -z "$size" "$size" "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  sips -z "$retina" "$retina" "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$ICONSET/icon.icns"

# ── 3. 组装 .app ────────────────────────────────────────────────────
echo "==> 组装 $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/lovart-client"
cp "$ICONSET/icon.icns" "$APP/Contents/Resources/icon.icns"
cp "$PLIST_SRC" "$APP/Contents/Info.plist"
# PkgInfo 是复古产物，但缺了它某些 Finder 视图会把包显示成文件夹。
printf 'APPL????' > "$APP/Contents/PkgInfo"
chmod +x "$APP/Contents/MacOS/lovart-client"

# ── 4. ad-hoc 签名 ──────────────────────────────────────────────────
# 本地自用不需要开发者证书，但**必须签**：不签的话改了内容之后 Gatekeeper
# 会直接说"已损坏"（而不是"未验证开发者"），比不签更难开。
echo "==> ad-hoc 签名"
codesign --force --deep --sign - "$APP"
codesign --verify --deep --strict "$APP"
plutil -lint "$APP/Contents/Info.plist" >/dev/null

echo
echo "✅ 完成：$APP"
echo "   双击 = 图形界面；命令行 = $APP/Contents/MacOS/lovart-client --help"
echo
echo "⚠️ 工作区（.env / inbox / var / output 的根）由程序自己判定，顺序见"
echo "   src-tauri/src/gui.rs 的 workspace_dir()：.app 同级目录是仓库 ⇒ 就用它。"
