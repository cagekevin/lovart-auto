# 两个入口：跑测试（唯一可信的验收）· 打包成 macOS 应用。
#
#   make check      编译 + 全部测试
#   make check-rs   同上（别名）
#   make app        打包 macOS 应用包 lovart.app（双击即用）
#   make clean-rs   清掉 Rust 构建产物
#
# ⚠️ 全绿只证明「和上次一样」，**不证明「规则真的成立」** —— 期望值本身可能是错的。
#    要证明规则成立，用 `scripts/probe.mjs`（先红后绿）：
#      node scripts/probe.mjs --label '…' --file <相对仓库根的路径> \
#        --find '<原实现>' --replace '<写坏>' --run 'cd src-tauri && cargo test' \
#        --expect-exit 101 --expect-out '<哪条测试该红>'
#    （⚠️ cargo test 失败时退出码是 101，不是 vitest 的 1。）

.PHONY: check check-rs app clean-rs

check: check-rs

check-rs:
	cd src-tauri && cargo test

# ⚠️ 改过 `ui/` 也必须走它 —— 前端资源是在 `cargo build` 时嵌进二进制的，
#    只拷旧二进制不会更新界面。
app:
	./scripts/make-app.sh

clean-rs:
	cd src-tauri && cargo clean
