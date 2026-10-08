# 08-conventions · FFI 边界硬约定

> 从源码提炼（`crates/openslate-mobile-ffi` + `crates/openslate-mobile` + Kotlin `bridge/`），硬性。背景见 [../06-mobile/mobile-ffi.md](../06-mobile/mobile-ffi.md)（完整暴露面）。

## 1. panic 不穿 FFI

- `openslate-mobile-ffi` 每个返回 `Result` 的入口经 `catch()`（`src/lib.rs:61-73`，`catch_unwind(AssertUnwindSafe(f))`）→ `MobileError`。
- Android 构建必须用 **`mobile` profile（panic=unwind）**——workspace `release` 是 `panic=abort`，abort 下 catch_unwind 抓不住，panic 直接穿越 FFI = UB。
- **新增 FFI 入口必须套同样的 `catch()` 模式**；错误细节放 `reason` 字符串（`err_dup` 克隆填值，保持 Kotlin 侧单一 catch 类型）。

## 2. 方法非阻塞

- `send` 内部 spawn；只有 `shutdown` 允许有限等待（Rust 侧 ~5s）。
- 阻塞型方法（`listProviderModels` 10s、`registryUpdateSource` 120s、`registrySearch` 首读盘、`lookupModelMeta` 在线兜底 10s）**必须在 doc 注释里标注「IO 线程调用」**，且内部设超时上限；Kotlin 侧必须包 `Dispatchers.IO`，违者 ANR。

## 3. 复杂结构走 JSON 字符串

- FFI **不逐字段映射业务结构**：`ClientMsg`/`ServerMsg` 协议 JSON、`{"ok":true,…}` / `{"ok":false,"error":"…"}` 结果信封、工具清单 / registry 查询结果全部以 JSON 字符串透传，Kotlin 侧用 `org.json` 解析。
- 新增 FFI 方法**优先沿用此风格**，避免 Record 爆炸（每加一个 Record 就多一份双端字段同步负担）。只有像 `MobilePathsDto` 这种「宿主注入的原语字段集」才值得 Record。

## 4. 回调线程契约

- `EventCallback.on_event` 在 Rust 泵线程（`openslate-event-pump`）上调用——宿主实现必须**线程安全、尽快返回**。
- Kotlin 侧经 `Channel.trySend` 转单消费协程（`RuntimeBridge`），**不得在回调里直接改 UI 状态**。
- FFI 最薄处不做任何解析（`UniffiCallbackAdapter` 直接转发字符串）。

## 5. 事件全序

- `EventSink` 单队列 + 泵线程保证「同一连接内事件全序」；host call 信封与 ServerMsg 走**同一条**流（`emit_raw`）。
- **不要在中间插并发路径**（Rust 侧不得绕过 EventSink 直调回调；Kotlin 侧不得起第二个消费者）。

## 6. 命名

| 对象 | 约定 | 例 |
|------|------|-----|
| FFI Record | `XxxDto` 后缀 | `MobilePathsDto` |
| Kotlin UI 投影 | `XxxUi` 后缀 | `ProviderUi` / `ModelUi` / `ToolInfoUi` |
| JSON 信封 `type` 字段 | snake_case | `host_call_requested` |

## 7. UniFFI 生成文件禁手改

- `android/app/src/main/java/uniffi/…` 只能由 `scripts/uniffi-gen.sh` 重新生成。
- 改了 UniFFI 面（导出方法 / Record / Error / callback）**必须同任务内**重生成绑定 + 重编 `.so` + 重编 APK（`scripts/build-android.sh` 全流程），否则 Kotlin 侧接口对不上。

## 8. secret 与协议封禁

- API key 只走 FFI `setApiKey`（内存注入 + 宿主 Keystore 持久化）；协议消息 `ClientMsg::SetApiKey` 在 mobile 被机制化封禁（会明文写 `.env`），Rust 侧广播 `set_api_key_forbidden` 拒绝。
- MCP token 走 `setMcpHostToken`，**必须在 `create` 之前**调用（装配期建连）。
