# OpenSlate PTC（Programmatic Tool Calling）功能规划

> 状态：草案 v1（2026-09-08），待评审
> 背景：让模型以「写一段代码编排工具调用」替代逐个 JSON tool call，并支持按工具粒度配置调用模式（仅 PTC / 仅普通 tool call / 两者皆可）。
> 调研材料：`/tmp/opencode/refs/{codex,opencode,cf-agents}`（浅克隆，供实现期查阅）。

---

## 1. 背景与调研结论

### 1.1 概念

PTC / Code Mode：模型把动作写成一段真实代码（JS/Python），在受限沙箱中执行；工具以**函数/API 形式**暴露给代码。模型只采样一次，循环/过滤/批量在沙箱内确定性完成，中间结果不进模型上下文。

### 1.2 业界实现对比（详表见调研会话，此处摘要）

| 实现 | 模型侧形态 | runtime | 按工具粒度控制 |
|---|---|---|---|
| Anthropic PTC（GA） | `code_execution` 工具 + 工具定义 `allowed_callers` | 托管 Python 容器 | ✅ `allowed_callers: ["direct"]/["code_execution_…"]/两者`（非硬边界） |
| OpenAI PTC | `programmatic_tool_calling` + `tools.<name>()` | 托管 V8 | ✅ `allowed_callers: ["programmatic"]` |
| Cloudflare Code Mode | N 工具 → 1 个 `{code}` 工具，schema→TS 注入 description | workerd isolate（断网，凭据只留 host） | 部分 |
| Codex code mode | `exec` freeform 工具，全局 `tools.*` | 独立 V8 宿主进程 | ✅ `ToolExposure` 位掩码 + `excluded/direct_only_tool_namespaces` + `ToolMode` |
| OpenCode code mode（实验 flag） | `execute` 工具，MCP 工具整体 defer | 自研 tree-walking JS 解释器 | ❌ 全有或全无 |
| smolagents / CodeAct | 每步动作 = Python 代码 blob | AST 解释器 / docker | ❌ |

关键数据：Anthropic 生产流量（10–49 工具）input tokens −20~40%；CF 工具定义 150K→2K；CodeAct +20% 成功率；programmatic 中间结果不计费。**反例**：顺序依赖型任务 +8% 成本（τ²-bench）。

### 1.3 值得照抄的协议细节（实战产物）

1. **错误即数据**：跨沙箱边界绝不 throw，host 侧错误以 `{result|error, logs}` 数据返回，沙箱内再转成 JS 异常供模型 try/catch（CF）。
2. **暂停-恢复语义**：代码调工具挂起、结果回传恢复（Anthropic 4min pending / 90s cell 上限参考值）。
3. **代码归一化**：剥 markdown fence、箭头函数直通、末表达式改 return（CF normalize.ts）。
4. **结果截断**：默认 ~6000 token（4 chars/token 估算）+ `--- TRUNCATED ---` 标记 + 原大小提示。
5. **凭据边界**：token 只留 host，沙箱零网络，「binding 即权限」。
6. **渐进披露**：目录（name+description）+ 按需查全量签名（CF durable `search`/`describe`、Anthropic 文件树、Claude Code ToolSearch）。

### 1.4 与本项目需求的对应

「按工具配置 仅PTC / 仅 tool call / 两者」= **Anthropic `allowed_callers` 的语义 + Codex `ToolExposure` 的配置面组织**。且我们同时控制两条通道（模型工具列表 + 沙箱绑定），可以做成**真隔离**（比 Anthropic 的「引导性」更强）。

---

## 2. 目标与非目标

**目标**
1. 模型可写 JavaScript，在受限沙箱内以函数形式调用工具（`tools.<name>(input)`）；
2. 按工具粒度配置调用模式 `direct_only | ptc_only | both`，glob 匹配，与 agent `tools` 白名单正交组合；
3. v1 零 provider 协议改动、零 `Message` 类型改动（代码只是 `run_code` 的 `{code}` 参数）。

**非目标（v1）**：REPL 状态跨 run_code 持久化、代码内审批暂停、Python runtime、`tool_search` 全文检索。

---

## 3. 总体架构

```
模型视图（每轮 GenerateRequest）
  tools: [ direct/both 模式工具的 ToolDefinition ..., run_code ]        ← 收口①
                                    │ {code: "..."}
                                    ▼
AgentRunner::execute（runner.rs:688）拦截 "run_code"（同 call_agent 模式）
                                    │
        ┌───────────────────────────┴────────────────────────────┐
        │ PtcExecutor（新 crate openslate-ptc，rquickjs/quickjs-ng）│
        │  · 沙箱全局 tools.<name>(args) → host 函数桥（JSON 序列化）  │ ← 收口②
        │  · console.log 捕获 → logs；返回值 → result                │
        │  · 硬超时（interrupt handler）+ 内存上限 + 调用数上限         │ ← 收口③
        └───────────────────────────┬────────────────────────────┘
                                    │ 每次工具调用回投（错误即数据）
                                    ▼
        AgentRunner 既有分发：registry.execute / call_agent
        （复用超时、截断、审计；收口④：对 ptc_only 工具的直调 → 明确报错）
```

**为什么 run_code 不做成 mcp-builtin server**：builtin server 只持 ServerSink，拿不到 registry 回调句柄（exp-1 已确认）。core 内运行时拦截（`ChildAgentCallable` 先例）天然解决循环引用，且白名单/审计/限制全复用。

**为什么选 rquickjs（quickjs-ng 后端）**：进程内、毫秒级启动、纯 C 静态链（符合本项目二进制依赖干净原则）、**硬超时 + 内存上限**优于 CF 的协作式超时、无网络能力天然满足「binding 即权限」。备选：deno_core（真 V8，构建重）；Python sidecar（多一个运行时依赖，P3 再评估）。

---

## 4. 配置设计

`openslate.toml` 新增 `[ptc]` 节（照抄 `[skills]` 模式：`serde(default) + deny_unknown_fields` + parse 测试，`config/mod.rs:269-286` 同款）：

```toml
[ptc]
enabled = false                     # 总开关；false 时一切工具都是 direct
timeout_ms = 60_000                 # 单次 run_code 硬超时（QuickJS interrupt handler）
memory_limit_bytes = 67108864       # QuickJS 堆上限（64MB）
max_output_bytes = 65536            # result+logs 截断（与全局 max_output_bytes 对齐）
max_tool_calls_per_run = 16         # 单脚本内工具调用上限（防循环轰炸）
max_list_chars = 8000               # 注入 run_code description 的类型块字符预算
disclosure = "auto"                 # full | catalog | auto（见 §5）

[ptc.tool_modes]                    # glob 模式 → 调用模式
"*" = "both"                        # 默认两者皆可
"shell" = "direct_only"             # 高危工具仅直调（对齐 OpenAI「写操作走 direct」建议）
"github_*" = "ptc_only"             # schema 不进 prompt，token 直降
```

**模式枚举**：`direct_only | ptc_only | both`（serde rename `direct/ptc/both`）。

**求值规则**：
- 取**最长匹配** glob 的模式（复用 `tool.rs:159 tool_name_matches` 匹配原语，优先级规则需补实现）；
- 未命中 → `enabled=true` 时 `both`，`enabled=false` 时一律 `direct_only`；
- `call_agent` 在 PTC 通道**固定排除**（默认 direct_only，禁止代码内递归委派）。

**与 agent 白名单交互**：最终 PTC 可见集 = agent `tools` 白名单 ∩ 模式 ∈ {ptc_only, both}。v1 不改 `AgentFrontmatter`（无 deny_unknown_fields，后续可加 per-agent 覆盖）。

**validate 集成**（`validate.rs:95-184`）：
- `enabled=true` 且存在 `ptc_only` 工具，但某 agent 白名单非空且不含 `run_code` → WARN（仿 read_skill 模式）；
- `tool_modes` 精确名引用不存在的工具 → WARN；
- 模式值/glob 非法 → error（serde + 自定义校验兜底）。

---

## 5. 模型侧展示：TS 类型注入与渐进披露

### 5.1 形态（注入 run_code 的 description，不动 system prompt）

```ts
declare const tools: {
  // ── 内置工具在根级 ──
  /** Read a file from the workspace */
  read_file: (input: { path: string }) => Promise<any>;
  // ── 外部 MCP 按 server 分组（利用现有 {server}_{tool} 命名）──
  github: {
    /** List pull requests */
    list_prs: (input: {
      state?: "open" | "closed" | "all";
      limit?: number;
    }) => Promise<any>;
  };
}
```

设计要点（相对 CF 的调整）：
- **参数对象内联进签名**，不生成 `XInput`/`XOutput` 中间类型（CF 的两段式跳转对小 schema 是负担）；
- **Output 一律 `Promise<any>`**（MCP 工具基本不声明 output schema，CF 也只是生成 `unknown` 占位；模板里说明「返回值为 tool result 的 JSON/文本」）；
- 嵌套深度限制 2–3 层，更深/循环引用 → `any` + JSDoc 提示；
- JSDoc 由工具/字段 description 生成（`*/` 转义，防注释逃逸）；
- 工具名 sanitize：`-`/`.`/空格 → `_`，保留字加 `_` 后缀（CF utils.ts 规则）。

### 5.2 三档披露（disclosure）

| 档 | 内容 | 适用 |
|---|---|---|
| `full` | 全部 PTC 可见工具的完整 TS 签名平铺 | 工具少（预算内），零额外往返，最优 |
| `catalog` | 只注入目录（name + description 首行）+ 沙箱内 `list_tools`/`describe_tool` 按需查 | 工具多，token 敏感 |
| `auto`（默认） | 先 full；超 `max_list_chars` 预算先把 `both` 工具降级为目录行（其 schema 已在直调列表，防双份）；再超则整体 catalog | 自适应 |

### 5.3 沙箱内建查询函数（P2）

```ts
// discovery：查目录，可批量，不占 max_tool_calls（单独宽松计数防刷爆）
await list_tools("github.*")
// → github.list_prs: List pull requests
//   github.get_file: Get file contents

// usage：查单个工具完整签名（JSDoc 内嵌 description，自包含）
await describe_tool("github.list_prs")
// → 完整 TS 签名 + 末尾按 schema 字段类型自动合成 Example 调用
//   （number→0, string→"...", boolean→true；照抄 CF mcp.ts:179-200）
```

设计理由：放沙箱内而非模型面工具 → 零新增模型面工具、可一次 run_code 批量查 + 顺带调用已知工具；代价是弱模型要多一跳（查签名→下一轮写正式调用），若实测不顺，P3 再加模型面 `describe_tool`（read_skill 同款模式）。

### 5.4 description 模板（run_code 工具）

```
Execute JavaScript to orchestrate tool calls.

Available:
{{types}}

Write an async arrow function. Do NOT use TypeScript syntax — no type
annotations, interfaces, or generics. Do NOT define named functions.
Example: async () => { const r = await tools.read_file({ path: "x" }); return r.content; }
Tool errors throw inside code — use try/catch when needed.
Use console.log for intermediate diagnostics; only the final return value
and logs are shown back to you.
```

---

## 6. run_code 执行协议

### 6.1 输入归一化（启发式子集，v1 不引 JS parser）

剥 markdown fence → 已是箭头函数则直通 → 兜底包 `async () => { ... }` →（可选）末尾裸表达式改 return。语法错 → 整体包装重试一次，再错则把 parser 错误作为 tool result 回模型自愈（smolagents 模式）。

### 6.2 沙箱生命周期与限制

| 项 | 值 | 机制 |
|---|---|---|
| isolate | 每次 run_code 新建，用完即弃 | rquickjs AsyncRuntime per call |
| 硬超时 | `timeout_ms`（默认 60s） | interrupt handler（AtomicBool + 计时线程；**非** CF 的 Promise.race 协作式） |
| 内存 | `memory_limit_bytes`（默认 64MB） | QuickJS memory limit，超限返回错误而非进程 OOM |
| 工具调用数 | `max_tool_calls_per_run`（默认 16） | host 桥计数；同时并入全局 `max_tool_calls`（顺带修复主循环不递增计数的缺口，runtime.rs check_limits） |
| 网络 | 无 | QuickJS 无网络内建，「binding 即权限」 |

### 6.3 工具桥（host ↔ 沙箱）

- 每个允许工具 → 沙箱全局 `tools[<ns>][<name>]`（Proxy 或显式对象）→ host 函数：`args JSON.stringify → runner.execute → {result|error} JSON 回沙箱`；
- host 侧错误**作为数据**返回；沙箱 wrapper 检测 `error` 字段后 `throw new Error(...)`，模型可 try/catch（CF 协议）；
- `run_code` 自身与 `call_agent` 不可在代码内调用（防递归）；
- v1 顺序 `await` 即可；`Promise.all` 并行依赖 rquickjs async 桥验证结果（见 §10 spike）。

### 6.4 结果格式（回给模型的 tool result 文本）

```
[logs]
hi from script
[result]
{"total": 3, "items": [...]}        ← 超过 max_output_bytes 截断，附
                                       --- TRUNCATED (原 N bytes) --- 标记
```

错误时：`[error] <message>` + `[logs]`（错误不打断 agent loop，作为 observation 回模型自愈，限重试）。

### 6.5 审计与可观测

- 复用 `ToolAuditRecord`（tool.rs:238），嵌套调用记 `caller: run_code`，trace 折叠展示（对标 Anthropic `caller` 字段）；
- 沙箱内工具调用照常走 `execute_tool_safely` panic 防护与 `limit_tool_output` 截断。

---

## 7. 代码结构与文件落点

**新 crate `openslate-ptc`**（workspace 成员）：
- `src/ts_types.rs` — JSON Schema → TS 生成器（移植 CF json-schema-types.ts：enum/anyOf/required/additionalProperties/$ref 深度防护/循环→any、JSDoc、名字 sanitize）
- `src/prompt.rs` — description 模板 + `{{types}}` 组装 + 预算分档
- `src/normalize.rs` — 代码归一化
- `src/executor.rs` — rquickjs 沙箱、host 桥、超时/内存/计数、console 捕获、结果格式化
- `src/describe.rs`（P2）— list_tools/describe_tool + Example 合成

**openslate-core 改动**：
- `runner.rs:173-187 tool_definitions_for` — 模式过滤 + 动态注入 run_code definition（收口①）
- `runner.rs:688-707 execute` — 拦截 `run_code` → 调 PtcExecutor（注入 executor 句柄，注意与 registry 的循环引用用拦截模式规避）
- `runtime.rs` — `max_tool_calls` 计数补齐；`RuntimeLimits`/`check_limits`（:49）扩展 ptc 字段
- `config/mod.rs` — `PtcConfig`（照抄 SkillsConfig 模式 :269-286）+ parse 测试组
- `config/validation.rs` + `cli/cmd/validate.rs` — §4 校验规则
- `types.rs` / `provider.rs` / genai `convert.rs` — **不动**（G1 不变量保持）

**测试落点**：`openslate-ptc` 单测（TS 生成快照、超时/内存/错误即数据）；`runner.rs` 测试组（ScriptedProvider：ptc_only 不出现在 definitions、幻觉直调被拒、代码内调用计数）；`tests/integration_run.rs` 端到端；`fixtures/openslate.toml` 同步新节。

---

## 8. 分阶段计划

| 阶段 | 内容 | 交付物 |
|---|---|---|
| **P0 spike**（先行） | ✅ 完成（§10）：rquickjs 0.12.2 全 8 项验证通过，坑位记录在案 | spike 工程 /tmp/opencode/ptc-spike |
| **P1 MVP** | ✅ 完成（2026-09-08）：新 crate `openslate-ptc`（executor/ts_types/prompt/normalize，42 测试）；`PtcConfig` 配置节 + fixtures；runner 集成（模型视图过滤、run_code 拦截、owned Arc 桥、幻觉直调守卫、全局 tool-call 计数、wiring 无效 tool_modes 告警，7 新测试）；端到端集成测试 + CLI validate 冒烟通过。**P1 为平铺模式（namespace=None）**，命名空间展示推迟 P2。遗留基线（非本次引入）：genai/error.rs 与 store-sqlite/query.rs 各 1 个 clippy 告警 | 工作区未提交改动（含 PTC_PLAN.md） |
| **P2 披露与健壮性** | ✅ 完成（2026-09-08）：命名空间管线（`Tool::namespace()` + McpTool server 别名 → `tools.github.list_prs` 组合路径）；`disclosure` 三档（full/catalog/auto + 预算降级）；沙箱内建 `list_tools`/`describe_tool`（含 Example 合成，`max_lookup_calls` 预算）；`examples/ptc_demo.rs` 确定性演示；真实 LLM 验证通过（intern-latest：主动 run_code 编排、ptc-only 工具代码内调用成功、直调幻觉被守卫拦截后自愈） | 同上 |
| **P3 可选** | per-agent tool_modes 覆盖、模型面 describe_tool、代码内审批暂停、REPL 状态持久化、全文 tool_search（复用 skills max_list_chars 预算机制） | 按需 |

---

## 9. 风险与开放问题

1. ~~**rquickjs 依赖**~~：✅ 已通过 P0 spike 验证（§10），async 桥/硬超时/内存上限全部可用；坑位已记录。
2. **真实模型（intern-latest）写 JS 能力**：不保证用好 run_code；demo 用脚本化 provider 兜底（同 child_agent_demo 策略）。
3. **顺序任务负优化**（τ²-bench +8%）：文档写明适用场景；默认 `enabled=false`。
4. **注入面**：tool result 字符串进沙箱（Anthropic 明示风险）；v1 仅截断不消毒，记为已知限制。
5. **glob 优先级语义**（最长匹配）P1 定稿并写进 validate 提示。
6. **token 双份问题**：both 工具同时出现在直调列表与 TS 类型块 → auto 档降级为目录行解决。
7. **命名空间分层**：内置工具固定在 `tools` 根级、MCP server 工具固定在二级组（`tools.<server>.<tool>`），互不同层天然不撞名；JS 标识符 sanitize 后的撞名由 registry 现有 `try_register` 冲突检查兜底。沙箱侧名字 → 注册名的反向映射在绑定 host 函数时以闭包捕获注册名（如 `tools.github.list_prs` → `"github_list_prs"`）实现，dispatch 与直调完全共用 `AgentRunner::execute` 链路（超时/截断/审计/计数自动继承）。

---

## 10. P0 Spike 验证清单（rquickjs）— ✅ 已完成（2026-09-08，/tmp/opencode/ptc-spike）

**环境**：rquickjs v0.12.2（quickjs-ng 后端，feature `full-async`），rustc 1.95.0。

### 10.1 结果

| # | 验证项 | 结果 | 实测 |
|---|---|---|---|
| 1 | 本机编译（cc + quickjs-ng） | ✅ | 首次 debug 构建 21.5s（含 C 编译）；增量 0.5s；release 45s（318% CPU） |
| 2 | 基本 eval + host 函数绑定 | ✅ | `add`/`echo`/`fail` JSON 往返正确 |
| 3 | 异步模型：async 箭头 + `Promise.all` | ✅ | 并行调用与 await 结果均正确 |
| 4 | 内存上限 | ✅ | 4MB 限制下 50MB 分配 → 异常返回（486µs），进程不崩 |
| 5 | 硬超时（interrupt handler） | ✅ | `while(true){}` 200ms 打断，实测 200.2ms，误差 <1ms |
| 6 | console 捕获 | ✅ | logs 完整回收，不外泄宿主 stdout |
| 7 | 错误即数据 | ✅ | host `{error}` → 沙箱 throw → JS try/catch 捕获 |
| 8 | 桥延迟 | ✅ | 1000 次顺序 host 调用 23.9ms ≈ **24µs/call**，非瓶颈 |

**编译产物**：release 二进制 2.56MB（spike 含 tokio full + serde_json，openslate 已有这些依赖，边际增量更小）；`ldd` 仅 libc/libgcc_s/libm——与现有二进制依赖画像一致，不影响 dev.sh 容器流程；全依赖 73 crate。

**结论：rquickjs 方案可行，P1 按此执行。**

### 10.2 API 坑位记录（0.12.2，P1 实现必读）

1. **`AsyncRuntime::set_memory_limit` / `set_interrupt_handler` 是 `async fn`**（内部拿 future 锁）——必须 `.await`，否则 future 被静默丢弃（编译器仅 warning "futures do nothing unless you .await"），限制根本不生效。spike 首轮 T4/T6 失败即此因。
2. **`Promise<'js>` 句柄不能逃出 `ctx.with(|c| ...)` 闭包**（生命周期限制）——正确模式：eval 触发 async 函数把结果写 `globalThis.__result/__err` → `rt.idle().await` 驱动 job 队列 → 再次 `with` 读取全局。该模式同时也是生产形态。
3. `async_with!` 宏已废弃 → 用 `AsyncContext::with(|c| ...).await`（闭包可同步返回）。
4. 引擎错误以 `Exception generated by QuickJS` 形式抛出——生产代码须用 `CaughtError`/`CatchResultExt` 提取异常 message 回给模型（自愈），不能只回这个笼统字符串。
5. 每个 `run_code` 新建 `AsyncRuntime` + `AsyncContext`（每执行一个 isolate，用完即弃），创建开销实测可忽略。

---

## 11. 参考资料

- Anthropic PTC: https://platform.claude.com/docs/en/agents-and-tools/tool-use/programmatic-tool-calling
- Anthropic advanced tool use: https://www.anthropic.com/engineering/advanced-tool-use
- OpenAI PTC: https://developers.openai.com/api/docs/guides/tools-programmatic-tool-calling
- Cloudflare Code Mode: https://blog.cloudflare.com/code-mode/ ；源码 `refs/cf-agents/packages/codemode/`
- Codex code mode: `refs/codex/codex-rs/{code-mode*,core/src/tools/spec_plan.rs}`
- OpenCode code mode: `refs/opencode/packages/opencode/src/tool/code-mode.ts` + `packages/codemode/codemode.md`
- CodeAct: https://arxiv.org/abs/2402.01030
- smolagents: https://huggingface.co/docs/smolagents/en/tutorials/secure_code_execution
