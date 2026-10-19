# 06-mobile · mobile-ffi（UniFFI 边界）

> 面向接手本项目的 AI agent。本章覆盖 `crates/openslate-mobile-ffi`：Rust `MobileRuntime` 与 Kotlin 宿主之间的唯一 FFI 边界（UniFFI 0.32，proc-macro 模式）。

## 1. crate 结构

- 单库文件 `src/lib.rs`（296 行）+ bindgen 二进制 `src/bin/uniffi_bindgen.rs`（3 行：`fn main() { uniffi::uniffi_bindgen_main() }`）。
- `Cargo.toml` 关键项：
  - `[lib] crate-type = ["cdylib", "lib"]`、`name = "openslate_mobile"` → 产物 `libopenslate_mobile.so`（cdylib 给 Android，lib 给 host bindgen 跑元数据）；
  - `uniffi = { version = "0.32", features = ["build"] }`；
  - `[[bin]] name = "uniffi-bindgen"`、`required-features = ["bindgen"]`；feature `bindgen = ["uniffi/cli"]` —— bindgen 只在显式开 feature 时可编译，不进常规构建。
- `uniffi.toml`：

  ```toml
  [bindings.kotlin]
  package_name = "uniffi.openslate.mobile"
  generate_immutable_records = true
  ```

- 入口脚手架：`uniffi::setup_scaffolding!("openslate_mobile")`（lib.rs:26）。
- 设计意图：**本 crate 是唯一允许依赖 uniffi 的层**——`OpenSlateRuntime` 包一层 `Arc<MobileRuntime>`，不污染下层 crate。

## 2. 完整暴露面清单

改这里任何一项后**必须重跑 `scripts/uniffi-gen.sh` 再编 APK**，否则 Kotlin 侧接口对不上（见 [../07-build-dev/android-build.md](../07-build-dev/android-build.md)）。

### 2.1 类型映射表

| Rust 侧（lib.rs 定义） | Kotlin 侧 | 说明 |
|------|------|------|
| `MobilePathsDto`（`#[derive(uniffi::Record)]`，4 个 String 字段：config/workspace/data/cache dir） | data class | 宿主注入路径 |
| `EventCallback`（`#[uniffi::export(callback_interface)]`，`fn on_event(&self, event: String)`） | interface（经 JNA 上调） | 事件出口，运行在 Rust 泵线程 |
| `MobileError`（`#[derive(Debug, thiserror::Error, uniffi::Error)]`） | 异常类（**单一 catch 类型**） | 两个变体：`CreateFailed{reason}` / `SendFailed{reason}`；细节放 reason 字符串 |
| `OpenSlateRuntime`（`#[derive(uniffi::Object)]`） | class | 包 `Arc<MobileRuntime>` |
| `String` / `Vec<String>` / `Option<String>` / `u64` / `u32` / `i32` / `bool` | String / List\<String\> / String? / ULong / UInt / Int / Boolean | 标量透传（u64 在 Kotlin 是 ULong） |

### 2.2 `OpenSlateRuntime` 方法（lib.rs:83-273）

JSON 字符串进出的方法占多数——**复杂结构不逐字段映射**，协议 JSON / 结果信封 `{"ok":true,…}` / `{"ok":false,"error":"…"}` 直接透传，Kotlin 侧 `org.json` 解析。

| 方法（Kotlin 名） | 行号 | 语义要点 |
|------|------|---------|
| `create(paths, callback)`（constructor） | 86-116 | 装配 + 首份 snapshot；失败抛 `MobileError.CreateFailed` |
| `send(msg: String)` | 119-127 | ClientMsg JSON；非阻塞（内部 spawn） |
| `setApiKey(provider, value)` | 130-132 | 内存注入，不落盘 |
| `setHttpProxy(url)` | 136-146 | 设/清（空串清除）进程级 `OPENSLATE_HTTP_PROXY` env；受限网络下宿主可经 adb reverse 共享电脑代理；**约定由宿主主线程在回合间调用** |
| `setExecBackends(backends)` | 151-153 | CSV："native" / "termux" / "native,termux"；热生效 |
| `listTools(): String` | 159-162 | 工具清单 JSON 数组（name/description/input_schema，按 name 排序） |
| `getAgentToolVisibility(): String` | 166-169 | `{"tools":[…],"hidden_tools":[…]}` |
| `setAgentToolVisibility(tools, hiddenTools): String` | 175-180 | 全量覆写 + 热生效；返回 ok 信封 |
| `reloadMcpServers(): String` | — | 热重载外部 MCP：按最新落盘配置重连 `[mcp.servers]` 并重注册工具（scratch registry 先做批内冲突检查，撞名完整回滚）；返回 `{"ok","tools","servers":{name:count},"failed":[…],"error"?}`；设置页保存 MCP 清单后调用 |
| `getMcpToolNameTemplate(): String` | — | 读当前生效工具名模板（落盘 `[mcp].tool_name_template`，缺省返回字面 `{prefix}_{server}__{tool}`；三占位符：prefix=前缀/服务键名、server=MCP 服务器名、tool=裸工具名） |
| `getMcpBuiltinToolNameTemplate(): String` | — | 读当前生效 builtin（裸名）工具名模板（落盘 `[mcp].builtin_tool_name_template`，缺省/空白返回字面 `{prefix}_{tool}`；占位符仅 prefix/tool，禁 `{server}` 与字面 `__`）。作用于不含 `__` 的裸上游名——host-builtin 裸名与桌面直连裸名同一条展开分支；解析经 core `McpConfig::builtin_template_or_default()`（接缝契约）。Phase C Kotlin 侧（判断 MCP 名册中的 bash 名）经它读取 |
| `setMcpBuiltinToolNameTemplate(template: String): String` | — | 校验（复用 core `validate_mcp_builtin_tool_name_template`：必须含 `{tool}`、占位符仅 `{prefix}`/`{tool}`、拒 `{server}`、禁字面 `__`——防 FFI 收下坏值致启动 fatal）+ 落盘（空串删键回默认）；返回 ok 信封；**不自动 reload**——调用方显式走 `reloadMcpServers` 热生效（reload 按新模板展开名交接 HostTool 让位/兜底，见 openslate-mobile.md §7.1） |
| `setMcpToolNameTemplate(template: String): String` | — | 校验（复用 core `validate_mcp_tool_name_template`，防 FFI 收下坏值致启动 fatal）+ 落盘（空串删键回默认）；返回 ok 信封；**不自动 reload**——调用方显式走 `reloadMcpServers` 热生效 |
| `getMcpServerName(): String` | — | 读 `[mcp.servers]` 单键名（多键回退 termux，空/失败回退 termux）——**`{prefix}` 占位符的展开值** |
| `setMcpServerName(name: String): String` | — | 校验（`validate_mcp_server_name`：非空、`[A-Za-z0-9_.-]`、不含 `__`）+ `rename_mcp_server` 整子表搬移（transport/headers 随迁，旧键消失）；幂等；返回 ok 信封；不自动 reload |
| `resolveHostCall(id: ULong, ok: Boolean, payload: String): Boolean` | 184-186 | false = 无此在途调用 |
| `listSessions(offset: UInt): String` | 190-193 | 每页 50；offset 递增（0、50、100…） |
| `listProviderModels(provider): String` | 198-201 | GET /models；**含 10s 超时，IO 线程调用** |
| `registrySources(): String` | 206-209 | 数据源列表 + 本地状态 |
| `registryUpdateSource(id): String` | 213-216 | **阻塞最长 120s，IO 线程调用** |
| `registryScheduleAutoUpdate()` | 219-222 | 后台 spawn，非阻塞 |
| `registrySearch(sourceId, query, limit: Int): String` | 232-237 | 纯本地，首次读盘解压，**建议 IO 线程**；`limit<=0` 按 50 |
| `lookupModelMeta(modelId): String` | 240-243 | 本地优先；miss 在线兜底（10s），**IO 线程调用** |
| `currentRunId(): String?` | 246-248 | 无进行中会话返回 null |
| `toolOutputByCallId(callId): String?` | — | 按 call_id 拉工具调用**完整输出**（详情弹窗懒加载）：内存 history 直读（锁内 clone，无网络/磁盘）；null = 未命中（id 未知 / history 被压缩 / 子代理工具），调用方回退 preview |
| `openSession(runId)` | 251-258 | 切换历史会话；失败抛 `MobileError.SendFailed`（reason 带 `open_session:` 前缀） |
| `deleteSession(runId): Boolean` | 264-267 | false = 拒绝/失败（活动会话不可删等）；Kotlin 负责同步删自己的 transcript 文件 |
| `shutdown()` | 270-272 | 优雅关停，幂等 |

### 2.3 namespace 级函数

- `setMcpHostToken(token)`（lib.rs:281-285）——进程级 MCP host 鉴权（`Authorization: Bearer`，对所有 HTTP MCP server 生效，toml 不持久化 token）。**必须在 `create` 之前调用**：MCP 连接在装配期建立，之后注入不影响已建连接。它配置的是进程而非 runtime 实例，所以是 namespace 函数。

## 3. panic 防线（硬约束）

1. workspace `release` profile 是 `panic="abort"`；本 crate 以 **`mobile` profile**（继承 release 但 `panic="unwind"`，根 `Cargo.toml [profile.mobile]`）构建。Android 构建必须 `--profile mobile`（`scripts/build-android.sh` 已内置）。
2. 每个返回 `Result` 的入口经 `catch()`（lib.rs:61-73）：`std::panic::catch_unwind(AssertUnwindSafe(f))` → panic 文本 downcast → `MobileError`；普通 `anyhow::Error` 链（`{e:#}` 带上下文）也转 `MobileError`。**绝不让 Rust panic 穿越 FFI（UB）**。
3. `err_dup(err, reason)`（lib.rs:75-81）：把 reason 填进同类错误的克隆——保持 Kotlin 侧单一 catch 类型（错误分类只有 CreateFailed/SendFailed 两档，粒度靠 reason 文本）。
4. 新增 FFI 入口必须套同样的 `catch()` 模式。

## 4. 泵线程契约（回调线程模型）

- `callback.on_event` 从 Rust 侧泵线程（`openslate-event-pump`，见 [openslate-mobile.md](openslate-mobile.md) §5）调用——宿主实现必须**线程安全、尽快返回**，重活转投宿主队列（Kotlin 侧 `RuntimeBridge` 用 `Channel.trySend` 只入队）。
- 所有方法立即返回；`send` 内部 spawn；只有 `shutdown` 允许有限等待（Rust 侧 ~5s）。
- doc 注释里标了 IO 线程要求的阻塞方法（`listProviderModels` / `registryUpdateSource` / `registrySearch` / `lookupModelMeta`）——Kotlin 侧必须遵守，违者 ANR。
- 事件全序由 Rust 单队列 + 单泵线程保证；宿主侧单消费协程续保（不要在中间插并发路径）。

## 5. 回调适配（UniFFI 0.32 细节）

- UniFFI 0.32 的 callback interface 在 Rust 侧以 `Box<dyn EventCallback>` 抬升进 constructor；`UniffiCallbackAdapter(Box<dyn EventCallback>)`（lib.rs:289-296）把它转成 `Arc` 语义的 mobile `EventCallback` 交给 runtime（生命周期由 runtime 侧 Arc 持有）。
- **FFI 最薄处不做任何解析**：适配器 `on_event` 直接转发字符串；JSON 解析全部留给 Kotlin。

## 6. 绑定再生成流程

产物：`android/app/src/main/java/uniffi/openslate/mobile/openslate_mobile.kt`（生成文件，**禁手改**，当前 ~2339 行）。

```bash
scripts/uniffi-gen.sh        # 仅重生成绑定
scripts/build-android.sh     # 交叉编译 .so + 绑定 + jniLibs 落位（全流程）
```

uniffi-gen 等价命令（library 模式，**0.32 的 CLI 已移除 source 模式**——直接传 `src/lib.rs` 会报 "Unknown library format"）：

```bash
cargo build -p openslate-mobile-ffi          # host 上编出 target/debug/libopenslate_mobile.so
cargo run -p openslate-mobile-ffi --features bindgen --bin uniffi-bindgen -- generate \
    --library target/debug/libopenslate_mobile.so \
    --language kotlin \
    --out-dir <tmp> \
    --config crates/openslate-mobile-ffi/uniffi.toml
# 产物拷到 android/app/src/main/java/uniffi/openslate/mobile/
```

详见 [../07-build-dev/android-build.md](../07-build-dev/android-build.md)。

## 7. 接手检查清单

1. 新增/修改导出方法、Record、Error、callback → 重跑 `scripts/uniffi-gen.sh` + 重编 APK；只改 `openslate-mobile` 内部实现（不动 FFI 面）→ 只需 `scripts/build-android.sh` 重编 `.so`。
2. 新增返回复杂结构的方法 → 优先返回 JSON 字符串（信封风格），避免 Record 爆炸（见 [../08-conventions/ffi-rules.md](../08-conventions/ffi-rules.md)）。
3. 新增阻塞方法 → doc 注释标「IO 线程调用」+ 内部设超时上限。
4. Kotlin 侧 `runtime.resolveHostCall(id.toULong(), …)`——注意 id 是 `ULong`。
