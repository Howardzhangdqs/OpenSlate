# 05 · openslate-app：装配层（组合根）

> 面向接手本项目的 AI agent。openslate-app 是**共享组合根**（库，无二进制）：
> 配置/存储/agent 树/工具/RunManager 的装配链 + provider 工厂 + 审批策略推导。
> cli / server / mobile 三个宿主都站在它上面；tui 不依赖它（纯客户端）。

---

## 1. 依赖方向与共享逻辑落点

```text
openslate-cli ─┬─▶ openslate-app ──▶ openslate-core ──▶ openslate-ptc
openslate-server ─┤        │                │
openslate-mobile ┘        ▼                ▼
                   openslate-model-genai   openslate-mcp-builtin
（tui 不依赖 app：纯 server 客户端）
```

| 共享的东西 | 在哪 |
|-----------|------|
| 装配链（config→store→agents→tools→RunManager） | `openslate-app/src/wiring.rs::build_app_context(_with)` |
| Provider 工厂 | `openslate-app/src/provider.rs::{build_provider_for_model, build_genai_provider}`（见 `03-models/model-genai.md` §4） |
| 审批策略推导 | `wiring.rs::apply_approval` |
| 会话状态机 / 回合引擎 / ClientMsg 派发 | `openslate-session`（server WS 与 mobile FFI 共用） |
| 客户端-服务端协议类型 | `openslate-protocol`（server 与 tui 共用） |

## 2. AppContext（wiring.rs:32-57）

```rust
pub struct AppContext {
    pub config: OpenSlateConfig,
    pub agents: AgentsConfig,
    pub store: Option<SqliteStore>,
    pub agent_tree: AgentTree,
    pub manager: RunManager,
    pub skills: SkillsCatalog,        // 惰性诊断数据；RunManager 持自己的副本做提示注入
    pub mcp_connections: McpConnectionGuard,   // ⚠ 声明在 manager 之后（见 §4）；builtin MCP 句柄
    pub mcp_external: McpConnectionGuard,      // 外部 [mcp.servers] 句柄（mobile 热重载整体换；CLI/server 用 Guard::append 合回 mcp_connections）
    pub mcp_tool_names: Vec<String>,           // 外部 MCP 已注册工具名全集（mobile 热重载 unregister 名单）
    pub config_path: PathBuf,
    pub global_config_path: Option<PathBuf>,   // TUI 模型管理写全局库的通道
    pub agents_path: PathBuf,
}
```

**字段声明序 = drop 序**（Rust 结构体按声明逆序 drop）：`mcp_connections` 声明在
`manager` **之后**——drop 时工具（及其 `McpTool` 持有的 `ServerSink` clone）先于传输
消亡，**不留「工具比连接活得久」的窗口**。`manager`（含 ToolRegistry）在
`mcp_connections` 之前 drop。新增字段时想清楚 drop 序，别把连接守卫挪到 manager 前面。

## 3. build_app_context_with 装配链（wiring.rs:451-694，顺序即代码序）

`build_app_context(config_flag)`（L444）= `build_app_context_with(config_flag, Vec::new())`。
后者是 **mobile 接缝**：`extra_tools` 在 builtin/external MCP 注册之后、RunManager 构造
之前注入注册表（desktop 传空，行为一致；mobile 用来注入 host tools，**plain overwrite
语义**——嵌入前端拥有这些命名空间，如 `mobile.*` / `android.*`）。

| 步 | 代码 | 说明 |
|----|------|------|
| 1 | `load_effective_config(config_flag)`（L163-188） | 显式 `--config` = **单文件语义**（无合并，global=None）；缺省发现链 = 全局库（`~/.config/openslate/openslate.toml`）垫在 active 配置下面合并（`select_config_files` L119 + `load_config_layered` L138 → `merge_configs`）。返回 `(config_path, global_config_path, config)` |
| 1.5 | `load_dotenv` ×2（L468-469） | 先全局配置目录再 active 目录；dotenvy **不覆盖**已设变量（shell export > 本地 .env > 全局 .env）。缺失/畸形 = soft warn |
| 3 | `load_agents`（L478） | `agents/` 目录 → `AgentsConfig`；目录不存在 = 硬错误 |
| 3.5 | `load_skills`（L483） | 见 §3.1 |
| 4 | `validate_config`（L488-497） | 有 error 即 bail（逐条 log） |
| 5 | `init_store`（L500，impl L376-426） | SQLite（`[database].path` 相对 → global_data_dir 下；缺省 `paths.database_path`）；建父目录 + `run_migrations` |
| 6 | `AgentTree::from_configs`（L503） | |
| 7 | `ToolRegistry::new()` → 见 §3.2 | |
| 7.6 | PTC 诊断（L637-658） | `[ptc.tool_modes]` 精确名匹配不到已注册工具 → warn（配置漂移） |
| 7.7 | `extra_tools` 注入（L663-667） | mobile host tools（overwrite） |
| 8 | root agent 解析（L671-677） | informational（provider 每 run/turn 重建） |
| 9 | `RunManager::new(config, agent_tree, registry, skills)`（L680） | |

### 3.1 技能发现（4 源，低→高优先级）

`skills_sources(config_path, cwd)`（L215-242）：

1. `~/.agents/skills`（用户全局、工具无关；无 home 静默跳过）；
2. `~/.config/openslate/skills`（XDG 用户原生）；
3. `cwd/.agents/skills`（项目本地、工具无关）；
4. `{config dir}/skills`（项目原生，如 `.openslate/skills`）。

每个源先 `canonical_source`（L248：相对路径 absolutize 到 cwd + 存在即 canonicalize）
**再**去重（保后出现的——更高优先级）：防相对 `--config` 拼写 / 符号链接目录逃逸去重、
双扫描同一物理目录产生假 "shadows" 警告。目录喂 RunManager（系统提示注入）和 builtin
`read_skill` server。`[skills] enabled = false` → 空目录（也关掉 read_skill server）。

### 3.2 工具注册（7.2 / 7.5）

- **builtin**（L517-543）：`connect_builtin_servers(&cwd, &config.builtin_tools, skill_infos)`
  → bare 名 `try_register`；**撞名 = 硬错误 bail**（外部 MCP server 抢了内置名）。
- **external**（`connect_external_mcp`，原 L549-629 循环抽出为 pub fn）：`[mcp.servers.*]`，disabled 的 log+skip；每个启用的 server
  一个并发连接 task，经 unbounded channel **按完成序**注册（不等全部连完才 log；
  注册本身仍在装配 task 上串行做纯内存 insert）。失败 warn+skip（一个坏 server 不拖垮
  启动）；撞名 bail。服务句柄进独立的 `mcp_external` guard。**mobile 热重载共用此函数**
  （先注册进 scratch registry 做批内冲突检查，再迁移到真实注册表，见 06-mobile）。
  工具注册名按 `[mcp].tool_name_template` 展开（模板解析为 owned String 传入各 spawn
  任务——`&str` 借用逃不出 `'static` 任务，E0521；空白视同未设置回默认）。
- 连接超时矩阵见 `04-tools/mcp-builtin.md` / core `mcp.rs`（握手/清单 30s，调用 300s）。

## 4. 审批策略推导（wiring.rs:293-370）

- `REPL_DEFAULT_APPROVAL_TOOLS = ["shell", "run_code"]`（L298，锁定产品决策）。
- `derive_effective_policy(configured, interactive, yes)`（L307-327）：优先级
  `--yes` > `[approval].policy` > 默认。默认：interactive 无配置 →
  `AutoExcept(["shell","run_code"])`；非交互 → `Auto`。纯函数，全矩阵单测
  （L1112-1156）。
- `NonInteractiveGate`（L334-344）：非交互 run 没人可问——High 风险 deny（文案提示
  `--yes` 覆盖），其余 approve。由 `apply_approval`（L351-370）在「非交互 + 非 auto
  + 无 --yes」时装上 + 一条启动 WARN。
- server 侧（build_state）：`interactive=true` 推导后，再挂 `SessionApprovalBridge`
  回调（审批请求广播给 WS 客户端应答）。

## 5. 共享逻辑归属原则（新代码放哪）

| 想加的东西 | 放哪 | 不要放哪 |
|-----------|------|---------|
| 新前端/新入口的组合根 | 复用 `build_app_context(_with)` | 复制装配链 |
| 交互语义（审批/快照/事件序） | openslate-session | server 或 mobile 各写一份 |
| 协议消息 | openslate-protocol | server/tui 私有枚举 |
| provider 构造差异 | `build_genai_provider` 的参数 | 各前端自己 new GenaiProvider |
| mobile host tools | `extra_tools` 接缝 | 在 app 里 if mobile |

## 6. 改动检查清单

- 动 `AppContext` 字段 → 检查 drop 序（`mcp_connections` 必须在 `manager` 之后）。
- 动装配顺序 → 步骤间有隐式依赖（validate 在 store 前、skills 喂 builtin server、
  extra_tools 在 MCP 后 RunManager 前），调换前读注释。
- 动审批默认 → `REPL_DEFAULT_APPROVAL_TOOLS` 是锁定决策；改矩阵先改全矩阵单测。
- 动配置发现/合并 → `validate` 命令复用 `select_config_files`/`load_config_layered`
  但保留自己的报错文案——语义改动要保持两条路径一致。
