#!/usr/bin/env bash
# 构建 mcp-host 的 aarch64 musl 静态二进制并嵌入 APK assets。
#
# 产物：android/app/src/main/assets/mcp-host-aarch64（App "自动化操作"
# 按钮经 /sdcard 中转安装到 Termux ~/.openslate/mcp-host）。
#
# 静态链接（rust-lld + self-contained musl crt）：Termux 侧零运行时依赖。
# 注意：host crate 的 rmcp 依赖是独立最小 feature 集（无 reqwest/ring），
# 见 crates/openslate-mcp-host/Cargo.toml 内注释。
set -euo pipefail
cd "$(dirname "$0")/.."

rustup target add aarch64-unknown-linux-musl
RUSTFLAGS="-C linker=rust-lld" cargo build --release \
  -p openslate-mcp-host --bin mcp-host --target aarch64-unknown-linux-musl

OUT=android/app/src/main/assets/mcp-host-aarch64
mkdir -p "$(dirname "$OUT")"
cp target/aarch64-unknown-linux-musl/release/mcp-host "$OUT"
file "$OUT"
echo "→ embedded: $OUT"
