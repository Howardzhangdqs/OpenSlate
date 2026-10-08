# 04 · 手把手：如何新增一个内置工具

> 面向接手本项目的 AI agent。以「给 fs server 加一个新工具」和「新建一个 server」
> 两种情形为例，列出全部改动点、注册点、开关、测试。跳过任何一步的后果都写在旁边。

---

## 0. 决策先行

1. 工具挂在哪个 server？扩展现有 server（fs/shell/edit/skill）还是新建？
   - 判据：沙箱与参数域是否一致（fs = workspace 路径；shell = 命令；edit = 补丁；
     skill = 目录快照）。
2. 是否需要 `[builtin_tools]` 开关？——**fs server 的工具必须配**（deny-by-default，
   见 §3）；shell/edit 的工具跟 server 开关走（server 起则工具在）；skill 没有
   config 开关（门 = 目录非空）。
3. 工具的 ToolCallMode（PTC）要不要非默认？默认 `Both`；要限就加 `[ptc.tool_modes]`
   glob（这是用户配置，不是代码改动，但文档要提）。

## 1. 写工具本体（crates/openslate-mcp-builtin/src/）

### 1a. 扩展现有 server（例：fs.rs 加 `list_dir`）

```rust
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ListDirParams {
    #[schemars(description = "Directory path (absolute or relative to workspace)")]
    pub path: String,
}

#[tool_router]
impl FsServer {
    #[tool(description = "List entries of a directory within the workspace")]
    async fn list_dir(
        &self,
        Parameters(args): Parameters<ListDirParams>,
    ) -> Result<CallToolResult, McpError> {
        if args.path.trim().is_empty() {
            return Err(McpError::invalid_params("'path' must not be empty", None));
        }
        let full_path = match resolve_workspace_path(&self.root, &args.path) {
            Ok(p) => p,
            Err(e) => return Ok(error_text(e.message())),   // 沙箱拒绝回喂 LLM
        };
        // … tokio::fs::read_dir …
        Ok(success_text(out))
    }
}
```

宏套路（对齐现有四个 server）：

- `#[tool_router]` 打在 `impl` 上；`#[tool(description = "…")]` 打在方法上——description
  是模型看到的工具说明，写清楚参数语义与边界；
- 参数走 `Parameters<T>` 包装（rmcp 自动按 `JsonSchema` 反序列化并校验）；
- **错误约定**：业务失败 → `Ok(error_text(...))`（错误文本回喂 LLM 自纠）；结构性
  非法参数（空串等）→ `Err(McpError::invalid_params(...))`；
- 限额用模块顶部 `const`（对齐 shell.rs 的 `MAX_OUTPUT_BYTES` 风格；截断一律
  UTF-8 字符边界安全）。

### 1b. 新建 server（例：`src/net.rs` → `NetServer`）

- `crates/openslate-mcp-builtin/src/lib.rs` 加 `pub mod net;`（+ 按需 re-export）。
- 结构对齐：`#[derive(Clone)] pub struct NetServer { …, tool_router: ToolRouter<Self> }`
  + `new()`（构造时 canonicalize / 快照）+ `#[tool_router] impl` + 
  `#[tool_handler(router = self.tool_router)] impl ServerHandler for NetServer {}`。
- crate **不依赖 openslate-core**（避免环）——需要 core 类型（如 `SkillInfo`）就在
  core 的 `mcp.rs` 里做 `From` 转换，方向是 core → builtin 传普通数据。

## 2. 核心注册（crates/openslate-core/src/mcp.rs）

### 2a. 扩展现有 server：`connect_builtin_servers`（L578-631）的过滤表

fs 分支的 filter（L593-607）现在长这样：

```rust
tools.extend(fs_tools.into_iter().filter(|t| match t.exposed_name() {
    "read_file" => cfg.read_file,
    "write_file" => cfg.write_file,
    other => { /* warn + false —— deny-by-default */ }
}));
```

新增 `"list_dir" => cfg.list_dir`，并在 `BuiltinToolsConfig`
（`crates/openslate-core/src/config/` 里，serde `deny_unknown_fields`）加对应布尔字段
+ Default。**不给开关的下场**：工具被 warn 隐藏，模型永远看不到，白写。

shell/edit 风格（工具跟 server 开关走）则只需 server 起时工具自然出现——不需要
改过滤表，但要确认这个语义是产品想要的。

### 2b. 新 server：`connect_builtin_servers` 里接一段

```rust
if cfg.enabled && cfg.net {
    let (net_tools, service) = connect_builtin_server(NetServer::new(...)).await?;
    tools.extend(net_tools);
    services.push(service);
}
```

`connect_builtin_server`（L493）是通用 in-process 连接器（tuple transport + 30s 握手/
清单超时），不用改。

## 3. deny-by-default 教训（最重要的两条约定）

1. `connect_builtin_servers` 对 fs server 的工具按 `exposed_name()` 过滤——**没有对应
   `[builtin_tools]` 开关的新工具会被 warn 并隐藏**。绝不因 server 启动就静默暴露。
   skill server 是唯一例外（门 = 目录非空）。
2. 内置工具注册用 **bare 名**；与外部 MCP server 撞名是**硬错误**（app wiring 的
   `build_app_context_with` 里 `bail!`）——内置失败 = bug，不像外部 server 是环境问题。
   选工具名时避开常见外部 server 的名字（`read_file` 已占；`search`/`fetch` 这类
   泛名也别用，外部一撞就启动失败）。

## 4. 装配与配置面

- 开关字段加进 `BuiltinToolsConfig` 后：`validate_config`（core）如有内置工具相关校验
  要同步；`default_openslate_toml`（cli `init` 的模板）按需带出。
- 审计 / 风险等级：`openslate-core/src/runner.rs` 的工具调用审计与
  `openslate-core/src/approval` 的 RiskLevel 映射——高危工具（如 shell 级别）要挂进
  `[approval]` 的默认门（`REPL_DEFAULT_APPROVAL_TOOLS`，见 `05-frontends/app-wiring.md`）。
- PTC：默认 `Both`。若工具不宜进沙箱（如长任务/大输出），文档里给
  `[ptc.tool_modes] "your_tool" = "direct"` 示例。

## 5. 测试

- **单元/集成**（mcp-builtin crate 内）：用 `lib.rs` 的 `testing` 模块（`#[cfg(test)]`）：
  ```rust
  let client = spawn_server(FsServer::new(dir.path())).await;
  let result = client.call_tool(CallToolRequestParams::new("list_dir".into())
      .with_arguments(json!({"path": "."}).try_into().unwrap())).await.unwrap();
  assert!(!is_error(&result));
  assert!(first_text(&result).contains("src"));
  ```
  断言助手：`first_text`（首 text block）/ `is_error`（`is_error == Some(true)`）。
  沙箱边界（路径逃逸 / 超限截断 / `..` 拒绝）必须有反例测试——参考 fs.rs 现有测试。
- **core 侧**：`connect_builtin_servers` 的开关矩阵测试（L700+ 现有
  `connect_builtin_servers_*` 系列照抄模式：全关 → 空工具集；只开新开关 → 只有新工具）。
- 跑法：`cargo test -p openslate-mcp-builtin -p openslate-core`（详见 `07-build-dev/build-and-test.md`）。

## 6. 文档同步（AGENTS.md 规则）

- 本目录 `mcp-builtin.md` 的 server 清单表加行（工具 / 沙箱 / 限额常量）；
- 若动了 `[builtin_tools]` 配置结构 → `02-core/openslate-core.md`（配置章）对应节；
- 若工具改变安全边界 → 根 `AGENTS.md` 的安全相关约定处补一句。

## 7. 检查清单（提交前过一遍）

- [ ] 工具名 bare、不与常见外部 MCP server 撞名；
- [ ] 业务错误 `Ok(error_text)` / 结构错误 `Err(invalid_params)` 分清；
- [ ] 输出有上限常量 + 字符边界安全截断；
- [ ] fs 工具配了 `[builtin_tools]` 布尔 + 过滤表分支 + Default；
- [ ] 开关矩阵测试 + 沙箱反例测试；
- [ ] `mcp-builtin.md` 清单表更新；
- [ ] 高危工具挂进审批默认门（如需）。
