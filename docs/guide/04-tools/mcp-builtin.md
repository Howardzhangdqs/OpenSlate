# 04 · openslate-mcp-builtin：内置 MCP server（fs / shell / edit / skill）

> 面向接手本项目的 AI agent。四个**进程内** MCP server（rmcp `ServerHandler`），
> 取代 legacy 的 `openslate-core::tool` 内置工具。全部是库代码：不写 stdout/stderr，
> 理论上也可以走 stdio 对外提供而协议保持干净。crate 只依赖 `rmcp`，**不依赖
> openslate-core**（避免环）。

---

## 1. server 清单

| 模块 | server 类型 | 工具 | 沙箱 | 行数 |
|------|------------|------|------|------|
| `src/fs.rs` | `FsServer::new(root)` | `read_file` / `write_file` | workspace root | 716 |
| `src/shell.rs` | `ShellServer::new(root)` | `shell` | cwd = workspace root | 562 |
| `src/edit/mod.rs` + `edit/editor.rs` | `EditServer::new(root)` | `edit_file` | workspace-relative，文件必须已存在 | 440 + 875 |
| `src/skill.rs` | `SkillServer::new(skills: Vec<SkillInfo>)` | `read_skill` | 无（内存目录快照） | 549 |

### 1.1 rmcp 宏套路（每个 server 一致）

```rust
#[tool_router]
impl FsServer {
    pub fn new(root: impl AsRef<Path>) -> Self { /* canonicalize 一次；失败保留原路径，首次使用时报错 */ }
    #[tool(description = "…")]
    async fn read_file(&self, Parameters(args): Parameters<ReadFileParams>)
        -> Result<CallToolResult, McpError> { … }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for FsServer {}
```

参数结构体 derive `serde::Deserialize + schemars::JsonSchema`，schema 自动生成，
字段用 `#[schemars(description = "…")]` 给模型提示。

### 1.2 错误约定（所有 server 一致）

- **业务失败**（文件缺失、沙箱拒绝、IO 错误、非零退出码）→
  `Ok(CallToolResult::error(vec![ContentBlock::text(msg)]))`（`error_text` helper），
  错误文本回喂 LLM 让它自纠；
- **结构性非法参数**（空 `path` / `command` / `patch`）→
  `Err(McpError::invalid_params(...))`（协议层）。

## 2. fs.rs：workspace 沙箱

### 2.1 `resolve_workspace_path(workspace_root, target_path)`（fs.rs:60-94）

规则（顺序即代码序）：

1. 任何 `..` 组件 → 拒（`PathError::Security("Path traversal not allowed")`）；
2. root 无法 canonicalize → `Execution("Failed to resolve workspace root: …")`；
3. 绝对路径原样用（**workspace 内的绝对路径允许**）、相对路径 join 到 canonical root；
4. 目标不存在 → `canonicalize_via_ancestor`（L104-130）：找最长**存在**祖先 canonicalize
   + 剩余组件倒序拼接。**双保险**：既让 `write_file` 能创建新文件，又解析已存在部分的
   符号链接——`ws/link → /tmp/evil` 时 `ws/link/new.txt` 解析到 `/tmp/evil/new.txt`
   被 containment 检查拒掉（纯词法回退会放过这种写逃逸）；
5. 最终不在 canonical root 下 → `"Path is outside workspace"`。

`PathError`（L30-44）：`Security(&'static str)` / `Execution(String)`，都映射为
tool-level error result（镜像 legacy `ToolError::SecurityError/ExecutionError`）。

### 2.2 工具行为

- `read_file`（L181-201）：`path` trim 空 → invalid_params；沙箱解析失败 → error result
  （文本如 "Path is outside workspace"）；读失败 → `Failed to read '{path}': {e}`。
- `write_file`（L204-236）：父目录 `create_dir_all`（legacy 行为）；成功文案
  `Successfully wrote {N} bytes to '{path}'`。

## 3. shell.rs：限额与安全

### 3.1 常量（shell.rs:25-33）

```rust
const DEFAULT_TIMEOUT_MS: u64 = 30_000;        // 默认命令超时
const MAX_TIMEOUT_MS: u64 = 120_000;           // 调用方传值的钳制上限
const MAX_OUTPUT_BYTES: usize = 64 * 1024;     // 合并输出截断阈值
const KEEP_BYTES: usize = 24 * 1024;           // 截断时保留的头/尾字节数
const DRAIN_GRACE: Duration = Duration::from_millis(200);  // 超时 kill 后的管道排水宽限
```

- `effective_timeout(timeout_ms)`（L113-119）：`None` → 默认；调用方值
  `.clamp(1, MAX_TIMEOUT_MS)`——0 不会立即杀，超大值不能钉死 agent 循环。
- `truncate_output`（L60-81）：>64KB 时保留头尾各 24KB，中间插
  `\n... [truncated {dropped} bytes] ...\n`；**UTF-8 字符边界安全**（`is_char_boundary`
  回退/前进）。
- 平台 shell（L272-278）：Unix `sh -c` / Windows `cmd /C`；`current_dir` 钉在 root、
  `kill_on_drop(true)`、stdin null、stdout/stderr piped。
- **环境脱敏**（L123-141 + L187-191）：`SENSITIVE_ENV_MARKERS = ["API_KEY","TOKEN","SECRET",
  "PASSWORD","PASSWD","CREDENTIAL","PRIVATE_KEY"]`，对 env 变量名**大小写不敏感**子串匹配；
  命中的变量 `env_remove`——模型驱动的子进程不能 `env | grep -i key` 把宿主注入的
  `.env` 密钥读进上下文。普通变量（PATH/HOME/LANG…）放行（白名单会弄坏正常命令）。
- **超时路径**（L222-250）：kill + reap 后各给 200ms 排水宽限读已缓冲的管道数据；
  返回 `TIMEOUT: command killed after {N}ms` + `format_output(-1, …)`，超长再截断。
- **非零退出码** = error result 但**携带完整输出**（L258-264）——LLM 能看到失败原因自纠。
- 输出格式（`format_output` L84-100）：`exit_code: N` + `--- stdout ---` / `--- stderr ---`
  分节，空节标 `(empty)`；stdout/stderr 分开捕获（读管道与 wait 用 `tokio::join!` 并发，
  防管道缓冲区写满阻塞，L212-220）。

## 4. edit：纯引擎 + 薄 MCP 层

### 4.1 edit/mod.rs（MCP 层）

- `EditFileParams { path, patch }`（L20-25）。比 fs **更严**：只接受 workspace-relative
  路径（绝对 → `PATH_ERROR: path must be workspace-relative`，L96-98）；文件必须已存在
  （NotFound → `PATH_ERROR: file not found`）；canonicalize 后不在 root 下 →
  `PATH_ERROR: path escapes the workspace`；非普通文件 → `PATH_ERROR: target is not a
  regular file`（L114-120）。
- 成功文案：`OK | {hunks} hunks | +{additions} -{deletions}`（L123-126）。

### 4.2 edit/editor.rs（纯逻辑、无 MCP 依赖、可独立单测）

- `apply_context_patch(path, patch) -> Result<EditResult, EditError>`（L354）。
  补丁格式：带 `@@` 锚点的小上下文补丁。`EditResult { hunks, additions, deletions }`（L48）。
- `EditError` 变体（L56）→ LLM 可自纠文案（mod.rs:45-63 的 `error_message`）：
  - `NO_MATCH { hunk }`：「读目标区域，带更准确的上下文重试」；
  - `ANCHOR_NOT_FOUND { hunk }`：**@@ 行必须是原文短子串**（复制粘贴级别），不是
    unified diff 头（`@@ -1,5 +1,5 @@` 这种）；
  - `AMBIGUOUS_MATCH { hunk, matches }`：多处命中，加锚点或更多上下文；
  - `OVERLAPPING_HUNKS`；`INVALID_PATCH(String)`；`Io(io::Error)` → `IO_ERROR`。

## 5. skill.rs：`read_skill`

- `SkillInfo { name, description, body, dir }`（L68-75）——core 技能目录在**连接时刻**的
  快照（`openslate-core/src/mcp.rs` 里有 `From<&Skill>` 转换）。server 从内存出 body、
  **不重读 SKILL.md**（运行内输出稳定；用户级技能目录在 workspace 沙箱外也能读）。
- **body 上限 256KB**（`MAX_BODY_BYTES = 256 * 1024`，L37）：超限字符边界截断 +
  可见标记 `--- SKILL BODY TRUNCATED ({kept} of {total} bytes) ---`（`truncate_body`
  L51-64）。SKILL.md 整文件在发现时载入、从内存供给，一个病态多 MB body 单次工具调用
  就会灌爆上下文，故设常量上限（与 shell 的 64KB 同理：限额不进 server 构造参数）。
- **资源清单**（L139-159）：递归收集技能目录下文件（跳过 `SKILL.md` 本身与点前缀
  `.git`/`.DS_Store`），posix 分隔符，排序后取前 `MAX_RESOURCE_ENTRIES = 50` 条
  （L28）；超出标注 `(listing truncated, N more files)`。目录在快照后消失 → 清空清单，
  调用仍成功（best-effort）。
- 输出：`<skill name="…">` XML 包裹（name 做 attribute 转义 `& < > "`，
  `xml_attribute_escape` L41-46）+ description + body + 技能目录路径 +
  `<skill_resources>` 文件清单。unknown skill → error result 列出可用技能名（排序后
  join，L121-134）。
- **注意**：`read_skill` 故意**没有** `[builtin_tools]` 开关，它的门是 skills 目录非空
  （上游 `[skills] enabled = false` 产出空目录即关掉）。

## 6. 注册与过滤（core 侧，见 mcp.rs 消费端）

`crates/openslate-core/src/mcp.rs::connect_builtin_servers`（L578-631）——deny-by-default：

```rust
let any_tool = cfg.enabled && (cfg.read_file || cfg.write_file || cfg.shell || cfg.edit_file);
if !any_tool && skills.is_empty() { return Ok((Vec::new(), Vec::new())); }
```

- fs server 的工具按 `exposed_name()` **过滤**（L593-607）：`read_file`→`cfg.read_file`、
  `write_file`→`cfg.write_file`，**其它名字 warn 并隐藏**——新增 fs 工具必须配显式
  `[builtin_tools]` 开关（`BuiltinToolsConfig`：`enabled` + `read_file`/`write_file`/
  `shell`/`edit_file` 四布尔），绝不因 server 启动就静默暴露。skill server 是唯一例外
  （门 = 目录非空）。
- 内置工具注册用 **bare 名**；与外部 MCP server 撞名是**硬错误**（wiring 层
  `build_app_context_with` 里 `bail!`，见 `05-frontends/app-wiring.md`）——内置失败 =
  bug，不像外部 server 是环境问题。
- 连接方式：`connect_builtin_server`（L493）——in-process tuple transport（mpsc 双通道，
  零序列化），握手 + `list_all_tools` 各包 30s `CONNECT_TIMEOUT`。

## 7. 测试基建（lib.rs:27-76，`#[cfg(test)] pub(crate) mod testing`）

- `spawn_server(server) -> RunningService<RoleClient, ()>`：mpsc 双向通道
  （`TxJsonRpcMessage<RoleClient/RoleServer>`）把 server 与 client 连在同一进程内，
  零序列化；server 任务 detach，client 取消/掉线即退出。
- `first_text(result)`：第一个 text block 的文本（无则空串）。
- `is_error(result)`：`result.is_error == Some(true)`。
- 用法示例：`edit/mod.rs` 的测试（`client_in(dir)` → `edit_file(client, …)` →
  断言 `first_text` / `is_error`）。

## 8. 改动检查清单

- 新增内置工具 → 按 `adding-a-tool.md` 手把手流程；deny-by-default 开关别忘了。
- 动沙箱 → `resolve_workspace_path` 的 5 条规则与单测是安全边界，符号链接逃逸用例
  必须继续过。
- 动 shell 限额 → 常量集中在 shell.rs 顶部；截断必须字符边界安全（多字节用例有测试）。
- 动 skill 上限 → `MAX_BODY_BYTES` / `MAX_RESOURCE_ENTRIES` 是上下文保护，不是可调配置。
