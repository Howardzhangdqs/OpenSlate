# 07-build-dev · 本地（desktop）构建与测试

> 面向接手本项目的 AI agent。Android 交叉编译 / APK 构建见 [android-build.md](android-build.md)；工具链前提见 [toolchain.md](toolchain.md)。

## 1. cargo 常用命令

```bash
cargo build                        # 全 workspace
cargo build -p openslate-mobile    # 单 crate（mobile 组合层）
cargo test                         # 全部测试
cargo test -p openslate-mobile     # mobile 组合层集成测试（desktop 可跑，无 Android 依赖）
cargo test -p openslate-mobile-ffi # FFI 边界测试
```

- `vendor/genai` 被 workspace exclude——它的测试要 `cd vendor/genai && cargo test` 单独跑。
- `crates/openslate-mobile/tests/runtime_flow.rs` 五个场景（closed_loop_streaming / host_call_roundtrip / hello_version_check / shutdown_rejects_send / tool_visibility_roundtrip）+ 各模块内联 `#[cfg(test)]` 单元测试，全部 desktop 可跑：改 mobile 相关 Rust 代码**先跑这层**（快，不用出 Android 产物）。

## 2. profile（根 `Cargo.toml`）

```toml
[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
panic = "abort"
strip = true

[profile.mobile]
inherits = "release"
panic = "unwind"
```

| profile | 用途 | 关键差异 |
|---------|------|---------|
| `release` | 桌面发布产物 / mcp-host 静态构建 | 体积优先（opt-level=z + lto + strip + codegen-units=1）；**panic=abort** |
| `mobile` | **Android `.so` 专用** | 继承 release 的体积优化，但 **panic=unwind**——FFI 边界的 `catch_unwind` 防线依赖它（panic=abort 下 catch_unwind 抓不住，panic 直接穿越 FFI = UB） |

**硬规则**：给 Android 编 `openslate-mobile-ffi` 必须用 `--profile mobile`（`scripts/build-android.sh` 已内置；手动 cargo build 时别写成 `--release`）。

## 3. Docker 辅助（CLI 调试用）

`scripts/dev.sh build|run|shell|logs|clean`——host 上编 `openslate-cli` 再拷进最小镜像，**不在容器内编译**。与 mobile 无关，CLI 侧联调用。

## 4. vendor / patch 与网络（勿"修复"）

根 `Cargo.toml` 的 `[patch.crates-io]` 各条都带注释说明原因与「何时可移除」，当前有三类：

| patch | 原因 | 移除条件 |
|-------|------|---------|
| askama 系列（GitHub 同版本 git 源） | cargo 拉取长期 TLS 失败（大陆网络） | 网络恢复后 |
| `vendor/rquickjs-sys`（path） | 上游缺 aarch64-linux-android 绑定，硬失败 | 上游合入后 |
| `vendor/genai`（path，0.6.5 patched，两条本地补丁） | ① anthropic adapter 回传 thinking block（多轮 extended-thinking）；② openai adapter User Binary 分支补 `video_url` 块 + `Binary::is_video()`（照抄上游 0.7.0 语义，智谱多模态） | 视上游发布（升级 0.7 前先 rebase 两条补丁） |

vendored genai 补丁明细（补丁点均搜 `OpenSlate patch`）见
`03-models/model-genai.md` §8；其测试在 `vendor/genai` 目录内独立跑（见 §2）。

详见 [troubleshooting.md](troubleshooting.md)。

## 5. 推荐迭代回路（改 Rust 侧）

```bash
# 1. desktop 测试先行（快）
cargo test -p openslate-mobile
# 2. 全量出 Android 产物（.so + 绑定 + jniLibs）
scripts/build-android.sh
# 3. 装 + 起 + 看日志
scripts/build-apk.sh --install --launch
adb logcat -s OpenSlateRust OpenSlateBridge
```

改 Kotlin UI 则只需第 3 步（gradle 增量，不用重出 .so）。改了 mcp-host 代码则加跑 `scripts/build-mcp-host.sh` 再编 APK（assets 重新打包）。
