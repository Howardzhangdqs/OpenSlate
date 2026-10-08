# 04 · openslate-ptc：Programmatic Tool Calling（QuickJS 沙箱）

> 面向接手本项目的 AI agent。PTC：模型不再逐个发 JSON 工具调用，而是写一个
> **JS async 箭头函数**编排工具调用，跑在 rquickjs（quickjs-ng）沙箱里。
> 设计文档：仓库根 `PTC_PLAN.md`。

---

## 1. 是什么 / 不是什么

- **硬约束：本 crate 不得依赖 openslate-core**（core 依赖它），公开 API 全部是普通
  数据类型。
- 沙箱四硬属性（动代码前保住）：**无网络**；**interrupt-handler 硬超时**（bridge 调用
  进行中也生效）；**堆内存上限**；**凭据永不进沙箱**（bridge 调用留在宿主侧）。
- vendor 依赖：rquickjs 的 `rquickjs-sys` 在根 `Cargo.toml [patch.crates-io]` 指向
  `vendor/rquickjs-sys`——上游 0.12.2 没随包发布 aarch64-linux-android 绑定（Android
  编译硬失败），vendor 副本补齐该绑定（照抄 gnu 版，quickjs 纯 C ABI 布局一致）；
  上游合入后可移除。

## 2. 模块地图（crates/openslate-ptc/src/）

| 文件 | 行数 | 职责 |
|------|------|------|
| `src/lib.rs` | 397 | 共享类型与模式解析（见 §3） |
| `src/executor.rs` | 1460 | `run_code(code, tools, catalog, limits, bridge) -> PtcOutcome`（L130）——沙箱执行本体 |
| `src/normalize.rs` | 125 | `prepare(code) -> Vec<String>`：剥 markdown fence、箭头函数启发式、产出有序候选（语法错触发下一个候选重试） |
| `src/ts_types.rs` | 1354 | JSON Schema → TS 声明（`generate_declarations`）、`plan_tool_bindings` 去碰撞绑定规划、`sanitize_ident`、保留字处理 |
| `src/describe.rs` | 318 | 沙箱内发现助手 `list_tools(pattern)` / `describe_tool(name)` 的格式化（catalog 行 = `path: 描述首句`） |
| `src/prompt.rs` | 341 | `run_code` 工具描述拼装（`TEMPLATE` L30 的 `{{types}}` 占位）+ disclosure 三档降级 |

## 3. 核心类型（lib.rs）

```rust
pub const RUN_CODE_TOOL: &str = "run_code";              // L37

pub enum ToolCallMode {                                   // L46-57（serde: "direct"/"ptc"/"both"）
    DirectOnly,  // 仅直连；不进沙箱
    PtcOnly,     // 仅沙箱；从模型直连工具清单隐藏
    Both,        // 两者皆可（PTC 启用时的默认）
}
impl ToolCallMode {
    pub fn direct_visible(self) -> bool;  // !PtcOnly
    pub fn ptc_callable(self) -> bool;    // !DirectOnly
}

pub fn resolve_tool_mode(patterns: &BTreeMap<String, ToolCallMode>,
                         tool_name: &str, ptc_enabled: bool) -> ToolCallMode;  // L77
```

- `resolve_tool_mode`：`[ptc.tool_modes]` glob 匹配，**最长模式胜**（等长取字典序更小者
  ——BTreeMap 迭代序保证确定性）；无匹配时 PTC 启用 → `Both`，禁用 → `DirectOnly`
  （第一行短路）。
- `wildcard_match`（L112-143）：与 core `tool.rs::tool_name_matches` **同语义双实现**
  （注释互相指认，改一处查另一处）：无元字符 = 精确匹配；`*` 匹配任意长度（含空）、
  `?` 恰一字符。经典双指针 + 单星回溯，**线性时间**——`list_tools(pattern)` 会把模型
  控制的 pattern 喂进 host 回调（interrupt handler 无法抢占的区段），ReDoS 式模式
  （`*a*a*a*…*b`）必须即时完成（测试 L328-339）。

```rust
pub enum Disclosure { Full, Catalog, Auto(默认) }        // L151-163，serde lowercase

pub struct PtcToolInfo {                                  // L170-185
    pub name: String,          // registry 名（派发键），如 github_list_prs / read_file
    pub namespace: Option<String>, // Some(ns) → 沙箱内 tools.<ns>.<method>（method = name 剥 {ns}_ 前缀）
    pub description: String,   // 成为 JSDoc 注释
    pub parameters: serde_json::Value,  // JSON Schema（可为 Null）
}

pub struct PtcBoundTool { pub name: String, pub namespace: Option<String> }  // L195-200

pub struct PtcLimits {                                    // L204-216
    pub timeout_ms: u64,               // 墙钟硬超时（interrupt handler）
    pub memory_limit_bytes: usize,     // QuickJS 堆上限
    pub max_output_bytes: usize,       // result+logs 截断预算
    pub max_tool_calls_per_run: usize, // 每 run bridge 工具调用数
    pub max_lookup_calls: usize,       // list_tools/describe_tool 独立预算
}
impl Default for PtcLimits {                              // L218-228
    timeout_ms: 60_000, memory_limit_bytes: 64MB, max_output_bytes: 64KB,
    max_tool_calls_per_run: 16, max_lookup_calls: 50,
}

pub struct PtcOutcome {                                   // L233-245
    pub result: Option<String>,  // 脚本返回值 JSON（出错 None）
    pub error: Option<String>,   // 错误是数据不是 Err——调用方格式化进 tool result 供模型自纠
    pub logs: Vec<String>,       // console.log/warn/error（warn/error 带前缀）
    pub tool_calls: usize,       // bridge 实际尝试数（envelope 错误、超预算的都算）
    pub timed_out: bool,
}

pub type ToolBridge = Arc<dyn Fn(&str, &str) -> BoxFuture<'static, String>
                              + Send + Sync>;             // L251：(registry_name, args_json) → envelope
pub fn envelope_result(value) -> String;                  // {"result": …}
pub fn envelope_error(msg) -> String;                     // {"error": "…"}
```

**errors-as-data 协议**：bridge 永远返回 JSON envelope；沙箱侧 wrapper 把 error envelope
转成 JS 异常供模型 try/catch。宿主 panic 不穿 FFI：bridge 调用包在 `catch_unwind` 里。

## 4. 执行模型（executor.rs，改前必读其模块文档 L1-51）

1. `run_code`（L130）抓环境 tokio `Handle`，整个执行搬 `spawn_blocking`——QuickJS 必须
   用**同步** Runtime，async 上下文里嵌 `block_on` 会 panic。无 runtime → 直接
   `PtcOutcome { error: "no tokio runtime" }`。
2. 每次调用新建 `Runtime` + `Context`，用完即弃（**one isolate per run**，无跨 run 状态泄漏）。
3. 资源限额：
   - `Runtime::set_memory_limit(limits.memory_limit_bytes)`（L332）；
   - 墙钟超时：`Arc<AtomicBool>` + 守护线程的 `set_interrupt_handler`（L333-334）。
     **bridge 工具调用进行中也硬生效**——bridge 的 `block_on` 被剩余预算 bound
     （L499-508 附近），永不 resolve 的工具 future 活不过 run；
   - 每 run 工具调用预算（bridge 内 `AtomicUsize` 计数，L487-492：超预算返回
     `envelope_error("tool call budget exceeded (N/M)")`）；独立的 lookup 预算。
4. **prelude**（`PRELUDE_HEAD` L77-98 / `PRELUDE_TAIL` L102-121）：
   - console 捕获进 `__logs`（warn/error 加 `[warn]`/`[error]` 前缀）；
   - `__mkTool(display, registry)`：`(input) => …` 包装器，调 `__call_tool_raw(registry,
     JSON.stringify(input ?? {}))`，error envelope → `throw new Error("tool X failed: …")`；
   - `__mkNs(entries, nsDisplay, nsPrefix)`：Proxy 命名空间对象，**未知成员**落 bridge
     派发组合名 `nsPrefix + "_" + member`（与 McpTool 组合真成员的方式一致）——得到规范的
     "tool not available" envelope，而不是跨命名空间够到同名平铺工具；
   - 顶层 `tools` Proxy：未知成员同样走 bridge（宿主 allow-list 产出规范错误）；
   - `list_tools` / `describe_tool` 同时暴露为顶层函数和 `tools.list_tools` /
     `tools.describe_tool` 成员（模型自然写后者；两种拼法共享同一 lookup 预算）。
5. 触发表达式在 async IIFE 里求值 `(CODE)()`，结果存 `globalThis.__result/__err`；
   返回的 promise **不 await**，由 pending-job 队列驱动到 settle——绕开 rquickjs 的
   promise 句柄不能逃出 `Context::with` 闭包的生命周期限制。`JSON.stringify(undefined)`
   留 undefined 的边界有专门处理（L438 附近）。
6. 绑定来自共享去碰撞规划 `ts_types::plan_tool_bindings`：sanitized 键与其它工具 /
   命名空间组 / 保留助手名（`list_tools`/`describe_tool`）撞车时确定性改名
   （`_2`、`_3`…），prelude 里以 `// renamed from X (collision)` 注释 surfaced。
7. 语法错重试：normalize 的候选序列逐个试，语法错触发下一个候选（`TriggerResult::
   {Triggerged, TimedOut, Failed}`，L176 附近）。

## 5. `run_code` 工具描述（prompt.rs）

- `TEMPLATE`（L30）：固定用法模板——「sandbox 是纯 JS 无 Node：无 require/import、
  无 fs/process/fetch，只有 tools 对象 / list_tools / describe_tool / console /
  标准内建」；「每次工具调用 resolve 为 tool-result 文本字符串（不是 {stdout} 式对象），
  像 JSON 就 JSON.parse」；「写 async 箭头函数、**不要 TS 语法**、不要命名函数」；
  「工具错误会 throw，用 try/catch」；「console.log 打中间诊断，只有返回值和 logs 会
  回给你」。
- `HELPERS_HINT`（L56）附加在每个 disclosure 档的工具块后——实测发现助手极易被漏看
  （Full 档描述从不提它们，模型自然会试 `tools.list_tools(...)`）。
- **Disclosure 三档**（`[ptc].disclosure`）决定描述里内联多少工具面：
  - `Full`：全量 TS 签名；
  - `Catalog`：只有 catalog 行（`path: 描述首句`），完整签名沙箱内 `describe_tool` 查；
  - `Auto`（默认）：先 Full；超 `max_list_chars` 先把 **both 模式**工具（schema 已在直连
    清单付过费）降级成 catalog 行，再超退纯 catalog。块超预算带显式截断标记。

## 6. 配置（openslate-core/src/config/mod.rs::PtcConfig，L403-447）

```toml
[ptc]
enabled = false                    # 总开关（默认关；关掉时一切工具按 direct-only）
timeout_ms = 60_000
memory_limit_bytes = 67_108_864    # 64MB
max_output_bytes = 65_536
max_tool_calls_per_run = 16
max_list_chars = 8000              # 描述里 TS 声明字符预算；0 = 不限
disclosure = "auto"                # full / catalog / auto
max_lookup_calls = 50
[ptc.tool_modes]                   # glob → direct/ptc/both，最长匹配胜
"shell" = "direct"
"github_*" = "ptc"
```

**别把配置解析逻辑复制进 ptc crate**：`PtcConfig` 在 core，`PtcLimits`/`Disclosure`/
`ToolCallMode` 在 ptc crate。

## 7. core 侧集成（拦截点）

- `openslate-core/src/runtime.rs`：工具调用名 == `RUN_CODE_TOOL` 时拦截（L1291），
  进 `runner.rs::AgentRunner::handle_run_code`（L923）+ `record_tool_audit(RUN_CODE_TOOL,
  TOOL_CALLER_DIRECT, …)`。
- `handle_run_code` 做三件事：
  1. **绑定集取自发起调用的 agent**（`current_frame()`，无 frame 时 root）的 ptc-callable
     工具：`ptc_code_tool_defs`（L464-479：`name != RUN_CODE_TOOL` 且 `resolve_tool_mode
     (…).ptc_callable()` 的 defs）→ `ptc_infos`（L491）→ registry 里的 `Arc<dyn Tool>`
     句柄表——`spawn_blocking` 边界要求 `'static`；
  2. 构造 bridge：查表 miss / 参数 JSON 非对象 / 工具 Err / Error 状态 ToolOutput →
     全部 `envelope_error`；成功输出经引擎级同一截断（`truncate_bridge_output`，L1088）
     后 `envelope_result`。每次调用写审计日志（`caller: run_code` 归因）；
  3. `run_code` 结果折叠成 ToolOutput：成功 `[logs]`+`[result]`，失败 `[error]`（+logs）；
     bridge 调用数并入全局工具调用预算。
- **可见性过滤**（runner 组装每轮工具定义时）：`PtcOnly` 从模型直连清单隐藏（L422、
  L1303-1307——直接调用 ptc-only 工具会得到「call it from code inside run_code」错误）；
  `DirectOnly` 不进沙箱。`generate_declarations` / catalog 相关调用同样走过滤后的集合。
- 装配期诊断（app/wiring.rs L631-658）：`[ptc.tool_modes]` 里**精确名**（无 `*`/`?`）
  匹配不到任何已注册工具 → warn（配置漂移信号）；glob 不警告（暂时无匹配是合法的）。

## 8. 改动检查清单

- 动沙箱 → 保住四条硬属性（无网络 / 硬超时含 bridge in-flight / 内存上限 / 凭据不进沙箱）；
  `wildcard_match` 必须与 core `tool.rs::tool_name_matches` 保持同语义（两个实现，改一处
  查另一处）。
- 动 `[ptc]` 配置结构 → `PtcConfig` 在 core，`PtcLimits`/`Disclosure` 在 ptc crate，
  别复制解析逻辑。
- 动 prelude / 绑定规划 → `// renamed from X (collision)` 注释与 `plan_tool_bindings`
  的确定性改名是模型可读契约。
- 动描述模板 → `HELPERS_HINT` 的教训（助手不可见）别重蹈；`Auto` 档先降级 both 模式
  工具的顺序是省 token 设计。
