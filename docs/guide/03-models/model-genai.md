# 03 · 模型接入：openslate-model-genai 适配层与 vendor/genai 补丁

> 面向接手本项目的 AI agent。本文讲清楚 `ModelProvider` 的唯一实现
> （openslate-model-genai）、它包装的 genai crate 的行为补丁（vendor/genai）、
> 以及谁在构造 provider（工厂层）。模型元数据 registry 见 `model-registry.md`，
> `/models` 自动检测见 `capability-detection.md`。

---

## 1. 在全链路中的位置

```text
openslate.toml                    openslate-core                openslate-app                openslate-model-genai           vendor/genai
─────────────                     ──────────────                ─────────────                ────────────────────           ───────────
[providers.zhipu] ─┐
base_url          │              model_config.rs               provider.rs                  provider.rs                     src/adapter/adapters/*
api_key_env       ├─ resolve_model(alias) → ResolvedModel ─▶  build_genai_provider() ─▶   GenaiConfig → GenaiProvider ─▶  AdapterKind(openai/anthropic/
adapter           │              (provider_name, model_id,     (key 注入接缝，见 §4)        ::new(cfg)                       gemini/ollama/…) → HTTP
[models.main]     │               api_key_env …)
model = "glm-5.1" ┘
```

- 运行时调用侧：`openslate-core/src/runtime.rs::execute_run` 拿 `Arc<dyn ModelProvider>`，
  构造 `GenerateRequest`，消费 `ModelStreamEvent`（`Delta` / `Reasoning` / `Usage` / `Done`）。
- Provider trait 定义在 `crates/openslate-core/src/provider.rs:21-51`：

```rust
#[async_trait::async_trait]
pub trait ModelProvider: Send + Sync {
    async fn generate(&self, request: GenerateRequest) -> Result<ModelResponse, ProviderError>;

    /// 默认实现：包一层 generate，发 Usage + Done 两个事件（无实时流）。
    async fn generate_stream(
        &self,
        request: GenerateRequest,
    ) -> tokio::sync::mpsc::Receiver<Result<ModelStreamEvent, ProviderError>>;

    fn provider_name(&self) -> &str;
}
```

- 同文件还有 `ToolDefinition`（L5-9，`name`/`description`/`parameters: serde_json::Value`）、
  `GenerateRequest`（L11-19，`model_id`/`system_prompt`/`messages`/`tools`/`max_tokens: Option<u32>`/`temperature: Option<f32>`）、
  `ProgressCallback`（L57-89，CLI spinner / server 转写镜像都实现它；含 `on_input_estimate`、
  `on_reasoning`、`on_step_end` 等带默认实现的方法）。

## 2. crate 职责边界（crates/openslate-model-genai/）

**唯一**的 `ModelProvider` 实现，包装 [genai](https://crates.io/crates/genai) crate（0.6.5）。
所有 genai 类型被封死在 crate 内部（`pub use genai` 仅为下游能命名 `AdapterKind`），
`openslate-core` 保持 provider 无关。**换掉 genai 只动这一个 crate。**

模块划分：

| 文件 | 行数 | 内容 |
|------|------|------|
| `src/provider.rs` | 1277 | `GenaiConfig`（L56-126，10 字段，含 `reasoning_effort`）、`GenaiProvider` + 构造 + trait 实现；`retry_behaviour` 测试模块（L687 起） |
| `src/convert.rs` | 704 | `to_chat_request` / `from_chat_response` / `from_stream_end`（OpenSlate 消息 ↔ genai ChatRequest/Response/StreamEnd） |
| `src/error.rs` | 119 | `GenaiBuildError`（L5-16，含 `UnknownAdapter`）、`map_error` → `ProviderError`（L31-76） |
| `src/retry.rs` | 298 | `RetryPolicy`（L49）、`is_retryable`（L91）、`exhausted_error`（L143）、`backoff_or_consumer_gone`（L152） |

## 3. GenaiConfig 与 GenaiProvider 构造

`crates/openslate-model-genai/src/provider.rs:56-98`：

```rust
pub struct GenaiConfig {
    pub provider_name: String,   // 展示名（"zhipu"）
    pub model: String,           // 传给 genai 的模型 id
    pub api_key: Option<String>, // None → genai 自己的环境变量解析
    pub base_url: Option<String>,// 端点覆盖（代理/网关）
    pub proxy: Option<String>,   // mobile 经 adb reverse → mihomo 的出站代理
    pub adapter: Option<String>, // 协议（openai/anthropic/gemini/ollama…），强烈建议显式
    pub timeout_ms: u64,         // 双语义预算，见 §5
    pub max_attempts: u32,       // 总尝试次数（含首次），默认 3；0 被钳到 1
    pub retry_base_ms: u64,      // 指数退避基数，默认 500
    pub reasoning_effort: Option<String>, // thinking effort（条目级 [models.X].reasoning_effort）：
    //                          七档 none/minimal/low/medium/high/xhigh/max，None=Auto 不传参。
    //                          none=显式关闭思考（GLM-5.2/Gemini 2.5 等支持的家族），
    //                          与 Auto（跟随 provider 默认）语义不同。
    //                          烘进默认 ChatOptions（§3 第 7 步），映射 genai ReasoningEffort
    //                          同名变体；各 provider 实际档位不一（GLM-5.3 仅 low/high/max），
    //                          存全集、UI 按模型能力裁剪（家族能力表见 06-mobile）。
}
```

`GenaiProvider::new(config)`（provider.rs:121-236）按序做：

1. **reqwest client 注入**：`user_agent(USER_AGENT)` + `connect_timeout(CONNECT_TIMEOUT)`。
   - `USER_AGENT = concat!("openslate/", env!("CARGO_PKG_VERSION"))`（L30）。
   - `CONNECT_TIMEOUT = 15s`（L37）——TCP+TLS 建立固定预算，**不是**用户面向的 `timeout_ms`。
   - **fix-20：reqwest client 层没有总 `.timeout()`**。reqwest 的总超时覆盖整个请求-响应周期
     （含流式 body），任何长于预算的流会被腰斩（历史 bug：「timeout after 60000ms」杀长回答）。
     预算改在调用点强制：`generate` 用 `tokio::time::timeout` 包每次尝试，`generate_stream`
     在流事件间强制空闲预算。
2. **Android TLS**（L142-153，`#[cfg(target_os = "android")]`）：webpki 静态根证书
   （`rustls::RootCertStore` + `webpki_roots::TLS_SERVER_ROOTS`）预配置 rustls
   （`aws_lc_rs::default_provider`）。原因：`rustls-platform-verifier` 需要 JNI 宿主初始化，
   纯 .so 场景未初始化即 panic——这是历史「空回合」bug 的根因之一。desktop 保持系统证书链。
3. **代理语义**（L156-163）：`config.proxy = Some(url)` → `reqwest::Proxy::all(url)`；
   `None` → `.no_proxy()`（desktop 默认直连，**不吃**环境变量 `http_proxy` 等）。
4. **adapter 显式绑定**（L173-184）：`AdapterKind::from_lower_str(adapter_str)`，
   未知值返回 `GenaiBuildError::UnknownAdapter`。`None` 时只 warn——genai 会从模型名前缀
   推断协议，未知前缀**静默回落 Ollama**（这就是 app 层缺省 `"openai"` 的原因，见 §4）。
5. **key 注入**（L188-192）：`with_auth_resolver_fn` 闭包返回
   `AuthData::from_single(api_key.clone())`；`None` 时 genai 走自己的 env 解析。
6. **base_url 尾斜杠归一化**（L201-212）：`with_service_target_resolver_fn` 里
   `if !url.ends_with('/') { url.push('/') }`。**为什么**：genai 用 `reqwest::Url::join(suffix)`
   拼服务 URL，按 RFC 3986，`.../v1` + `chat/completions` 会**替换**掉 `v1`
   （得到 `.../chat/completions`——错的）；`.../v1/` 才是追加。
7. **默认 ChatOptions**（L216-226）：`capture_content/capture_tool_calls/capture_usage = Some(true)` +
   `extra_headers: user-agent`；`reasoning_effort`（若 GenaiConfig 配置）在此一并烘进
   （`reasoning_effort_from_str` 六档映射，wire 键 `reasoning_effort` 与档位名同形，
   单测 `reasoning_effort_string_maps_to_genai_variants` 钉死）。单测
   `default_chat_options_have_capture_flags`（L499）守着。

### 3.1 必须知道的行为约定（改这里之前先读 `src/lib.rs` crate 文档）

1. **capture 标志**：genai 只在这些标志打开时填充 `StreamEnd.captured_*`——关掉它们
   流式 `Done` 事件就是空的，**流式工具调用会静默失败**。
2. **User-Agent 双写**：`openslate/<version>` 同时写 reqwest client 默认头（覆盖非流式
   `exec_chat`）**和** `ChatOptions::extra_headers`（`exec_chat`/`exec_chat_stream` 都合并）。
   双写原因：genai 0.6.5 的流式路径会丢 client 默认头。E2E 单测
   `model_request_carries_openslate_user_agent`（provider.rs:556，mockito 同时匹配 UA + Bearer）。
3. **usage 解析**：OpenAI 的 `usage.prompt_tokens_details.cached_tokens` 经 genai 进
   `Usage::cached_input_tokens`；服务端不给 details 对象则为 `None`
   （单测 `usage_cached_tokens_parsed_from_wire`，L613）。

## 4. 谁构造 GenaiProvider（工厂，crates/openslate-app/src/provider.rs）

- `build_provider_for_model(config, alias)`（L19-25）— desktop 语义：
  `build_genai_provider(config, alias, None)`，key 从 `api_key_env` 环境变量读
  （.env 自动加载发生在 wiring 层）。
- `build_genai_provider(config, alias, api_key: Option<String>)`（L30-93）— key 注入版
  （**mobile 接缝**）：`Some(key)` 直接用（Android Keystore → FFI `set_api_key` →
  `openslate-mobile/src/provider.rs` 的内存 map）；`None` 保持环境变量语义。
- 所有 provider 都走 genai——`ProviderConfig.kind` 只是信息性提示，**不再选择实现**。
- `adapter` 取 `resolved.provider.adapter`，缺省 `"openai"`（L51-55）——多数 OpenAI 兼容端
  （zhipu/minimax/internlm…）不显式配 adapter，不设的话 genai 会按模型名静默回落 Ollama。
- `timeout_ms` 收敛规则（L64-68）：`[limits].timeout_ms == u64::MAX`（配置里「无总时长预算」的
  mobile thinking 语义）→ 60s；`>0` → 原值；缺失/0 → 60s。
- `proxy` 来自 `std::env::var("OPENSLATE_HTTP_PROXY")`（L77，非空过滤）——mobile 宿主经
  adb reverse 共享电脑代理；desktop 无此 env 行为不变。

模型别名解析在 `crates/openslate-core/src/model_config.rs:111`：
`resolve_model(config, alias) -> Result<ResolvedModel, ConfigError>`。

```rust
// model_config.rs:61-85
pub struct ResolvedModel {
    pub alias: String,             // 原样别名（level 名或条目名，不追踪间接目标）
    pub provider_name: String,
    pub provider: ProviderConfig,  // 整个 provider 配置（base_url/api_key_env/limits…）
    pub model_id: String,          // 真实模型 id（如 "glm-5.1"）
    pub supports_tool_call: bool,
    pub supports_vision: bool,
    pub supports_reasoning: bool,
    pub input_price_per_mtok: Option<f64>,   // P2-3 计价
    pub output_price_per_mtok: Option<f64>,
    pub reasoning_effort: Option<String>,    // thinking effort 七档（none=显式关闭）；resolve_model 容忍归一
}
```

解析顺序（model-mgmt-1 两层 schema）：`[levels]` 别名 → `[models]` 条目 → 直接 `[models]`
（legacy）。错误：`MissingRequiredModel` / `InvalidLevelRef`（level 指向不存在的条目）/
`InvalidProviderRef`。`supports_*` 能力字段给 runtime 决定是否注入工具定义；
`cost_spec()`（L87-90）是 runtime 循环与 REPL compact 摘要共用的计价快照。

> **capability 语义注记（compact 未绑定 = 继承会话模型）**：
> `[capabilities].compact` 显式绑定时用绑定代号；**未绑定时代码（session 引擎
> `run_auto_compact`）直接继承会话当前 `model_alias`**，不走
> `capability_alias` 的 "fast" 缺省（该缺省仅为函数兼容保留）。解析/构建失败
> 仍走机械拼接兜底（不变）。`main` / `title` 解析不受此改动影响。

> **UI 层注记（mobile）**：levels 语义在 core 侧原样保留（resolve_model 先查
> levels、`[capabilities]` 值合法形态仍是代号或条目两种），但 **mobile UI 已不
> 暴露 levels**——设置页功能绑定 / 对话页模型选择器一律直绑 `[models]` 条目，
> 存量「capability → 代号」引用由启动迁移一次性改写为条目直绑（RuntimeBridge
> `maybeMigrateCapabilityDirect`，见 `06-mobile/android-app.md`）；`[levels]` 表
> 不删不写，仅作 resolve 的遗留兼容层。

## 5. timeout_ms 双语义（fix-20）与重试边界

### 5.1 双语义

- **非流式 `generate`**（provider.rs:250-304）：**每次尝试的总预算**
  （`tokio::time::timeout(self.timeout, client.exec_chat(...))` 包住，L271-275）。
  `Err(_elapsed)` → 立即返回 `ProviderError::Timeout`，**不重试**（尝试已烧完全部预算）。
- **流式 `generate_stream`**（provider.rs:306-487）：**空闲预算**——
  (a) 等响应头（L338-341 包 `exec_chat_stream`）；(b) 任意两个流事件之间
  （L395 包 `stream.next()`）。token 持续到达 = 流健康，总时长无限制。
  超时报 `stream_idle_timeout_error(idle)`（L47-52）：`ProviderError::ConnectionError(
  "stream idle timeout after {X}ms (no stream data received within the idle budget)")`，终态不重试。
- 连接建立固定 `CONNECT_TIMEOUT = 15s`；reqwest client 层**没有**总 timeout。
- 映射自 `[limits].timeout_ms`（默认 60000）。

回归测试钉子（`retry_behaviour` 模块）：
`stream_flowing_longer_than_budget_succeeds`（L1129，400ms 间隔事件流 vs 700ms 预算，
总时长 1.2s 超预算仍成功——fix-20 回归钉）、`stream_idle_timeout_on_mid_stream_silence`
（L1189）、`stream_idle_timeout_when_headers_never_arrive`（L1243）。

### 5.2 重试边界（retry.rs）

- **可重试**：HTTP 429 / 5xx / 传输类网络错误（`reqwest::Error` 且 `!is_timeout()`）。
  `is_retryable`（retry.rs:91-131）在 `genai::Error` 上分类，覆盖三形态：
  `WebModelCall`/`WebAdapterCall`（非流式，内嵌 `webc::Error`）、`HttpError`（SSE 解析器
  造的流中 HTTP 错误）、`WebStream`（流级传输错误，downcast 挖内层状态码）。
- **不可重试**：永久 4xx（401/403/400/404/422）、超时（单次尝试已耗尽预算）、
  确定性错误（auth 解析失败 `RequiresApiKey`、`StreamParse` 畸形响应）。
- **流式特例——`sent_output` 标志**（provider.rs:383-482）：首个 `Delta`/`Reasoning`
  **已转发给消费者**之后的任何错误都不重试（重放会复制已显示的输出）。
  ToolCall chunk 不实时转发（只在终态 `Done` 出现），Usage/Done 本身结束流，
  所以 Delta/Reasoning 恰是唯一要紧的边界。测试：`stream_does_not_retry_after_first_delta`
  （L1026）、`stream_does_not_retry_after_first_reasoning`（L1066）——均断言服务器只见
  **1 个连接**。
- **退避公式**（retry.rs:73-82）：`retry_base_ms * 2^(failed_attempt-1) + jitter`，
  单次 sleep 上限 `MAX_BACKOFF_MS = 10s`（L45）。jitter 取墙上时钟亚秒纳秒
  `[0, retry_base_ms)`（L165-174，零依赖）。指数位移 `checked_shl` 钳 32 防溢出。
- **预算耗尽**：`exhausted_error(attempts, last)` → `ProviderError::RetryExhausted { attempts,
  last }`，文案 `after {N} attempt(s)` + 末次错误原文逐字保留（无双重前缀）。
- **`backoff_or_consumer_gone`**（retry.rs:152-160）：`select! { sleep, tx.closed() }`——
  消费者已弃接收者（如 runtime 超时）就提前停止重试。
- **Retry-After 不遵守**（retry.rs:28-37 文档）：`HttpError` 不带响应头；
  `ResponseFailedStatus` 有 HeaderMap 但两条路径都取会 fork 行为。纯指数退避是既定取舍。
- **失败模式漂移**（刻意，有档）：runtime 外层 `tokio::time::timeout(remaining)` 仍
  bound 总墙钟——重试烧完剩余预算的调用现在以 run 级 Timeout 失败，而非立即
  ProviderError。

## 6. 错误映射（error.rs）

`map_error(e: genai::Error) -> ProviderError`（error.rs:31-76），orphan rule 所以是自由
函数而非 `From`。两条 HTTP 失败形态走**同一个状态码阶梯** `map_status`（L98-105）：

| 状态 | ProviderError |
|------|---------------|
| 429 | `RateLimit` |
| 401 / 403 | `AuthError(body 摘要)` |
| 404 | `NotFound(body 摘要)` |
| 其余 | `ServerError(status)` |

`WebStream` 先 downcast 内层 `HttpError` 恢复状态码（否则流中 401 被降级为
ConnectionError）。`snippet`（L108-118）对错误 body 做 200 字符 UTF-8 边界安全截断。
`StreamParse`/`ChatResponse*` → `MalformedResponse`；auth 三形态 → `AuthError("missing API key")`；
兜底 → `ConnectionError`。

## 7. 「空回合」防御（流在 End 之前关闭）

provider.rs:450-462：流以 `None`（结束）而非 `End` 事件收尾时——典型：网关返回
非 SSE 的 JSON 错误体，genai SSE 解析零事件即静默结束——显式返回
`ProviderError::MalformedResponse("stream closed before Done — gateway returned a
non-SSE/empty body (check base_url path; anthropic 网关通常需要以 /v1 结尾)")`。
上游不再看到无从排查的「空回合」。

## 8. vendor/genai 补丁（改 genai 行为的唯一入口）

根 `Cargo.toml`（L113-128，`[patch.crates-io]` 节）：

```toml
[patch.crates-io]
# 本机网络对 crates CDN 拉 askama 系列长期 TLS 失败；同版本 git 源补齐（网络恢复后可移除）。
askama = { git = "https://github.com/askama-rs/askama.git", tag = "v0.16.1" }
askama_derive = { ... } askama_macros = { ... } askama_parser = { ... }
# rquickjs-sys 0.12.2 未随包发布 aarch64-linux-android 绑定（Android 编译硬失败）；
# vendor 副本补充该绑定（照抄 gnu 版，quickjs 为纯 C ABI，二进制布局一致）。上游合入后可移除。
rquickjs-sys = { path = "vendor/rquickjs-sys" }
# genai 0.6.5 vendor：anthropic adapter 原本丢弃历史 ReasoningContent——
# patch 为回传 thinking block（多轮 extended-thinking 语义）。
genai = { path = "vendor/genai" }
```

- **genai 补丁点**：`vendor/genai/src/adapter/adapters/anthropic/adapter_shared.rs:277-286`
  （搜 `OpenSlate patch`）——assistant 消息里的 `ContentPart::ReasoningContent(rc)` 回传为
  `{"type":"thinking","thinking":rc,"signature":""}`。signature 留空：兼容不校验的网关；
  官方严格 API 若拒，上游应不携带 reasoning。补丁使多轮对话保留 thinking block
  （Anthropic extended-thinking 语义）。
- **`vendor/genai` 被 workspace `exclude`**（根 Cargo.toml L21，它自带 `[workspace]` 表）；
  它的测试要在 vendor/genai 目录里独立跑（见 `07-build-dev/build-and-test.md`）。
- `vendor/rquickjs-sys` 与模型层无关（PTC 的 Android 绑定补齐，见 `04-tools/ptc.md`）。

## 9. 改动检查清单

- 动了 genai 行为 → 改 `vendor/genai`（并确认 `[patch.crates-io]` 仍指向它），
  不要在 model-genai 里 fork 逻辑绕。
- 动了 `[limits].timeout_ms` 语义 / 重试边界 → 先读 `openslate-model-genai/src/lib.rs`
  顶部文档（fix-20 / Phase 2 失败模式漂移都有记录）；回归测试集中在
  `provider.rs::retry_behaviour` 模块，先改测试预期再改实现。
- 动了 Android TLS / 代理 → `model_registry.rs`、`provider.rs::list_remote_models`（core）
  是**同款处理**（webpki + `OPENSLATE_HTTP_PROXY`），三处一起改。
- 新增 adapter → genai `AdapterKind::from_lower_str` 认识它才行；否则工厂层报
  `UnknownAdapter`。
