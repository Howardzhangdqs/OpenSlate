//! builtin server 自动拉起：host 启动时以 in-process tuple transport 连接
//! openslate-mcp-builtin 的 BashServer / FsServer / EditServer，其工具以
//! **裸名**（无 `alias__` 前缀）并入聚合清单。
//!
//! 为什么在 host 内嵌 builtin：termux bash 从 RUN_COMMAND Intent 逐命令
//! 桥接升级为 MCP over HTTP——host 是 Termux 内常驻进程，bash/fs/edit
//! 直接进程内供给，App 侧把裸名经 core 模板展开成 `termux_bash` 等注册
//! 名，模型白名单 / UI / 历史迁移零变动。
//!
//! **skill server 有意不嵌入**（防未来 agent"补全四件套"）：SkillServer
//! 的构造需要 core 的 Skill 快照（wiring 层从技能目录构建 SkillInfo），
//! host 依赖 core 即破层——host 的白名单豁免只覆盖 mcp-builtin。
//!
//! 连接方式照抄 core 的 `connect_builtin_server`（core/mcp.rs:591）：
//! futures mpsc 双通道零序列化，握手与列工具各包 CONNECT_TIMEOUT。
//! mcp-builtin 的 `testing::spawn_server` 是 `#[cfg(test)]` 专用、不可
//! 跨 crate 复用，host 侧自备等价实现。

use std::path::PathBuf;
use std::sync::Arc;

use futures::channel::mpsc;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, Implementation,
    JsonObject, Tool,
};
use rmcp::service::{RunningService, RoleClient, RoleServer, TxJsonRpcMessage};
use rmcp::ServiceExt;
use rmcp::ServerHandler;

use openslate_mcp_builtin::bash::BashServer;
use openslate_mcp_builtin::edit::EditServer;
use openslate_mcp_builtin::fs::FsServer;

use crate::BuiltinConfig;
use crate::downstream::{CALL_TIMEOUT, CONNECT_TIMEOUT};

/// Termux `$PREFIX` 的缺省值（bash 路径解析用）。
const DEFAULT_TERMUX_PREFIX: &str = "/data/data/com.termux/files/usr";

type Peer = Arc<RunningService<RoleClient, ClientConfig>>;

/// 一台已连接的 builtin server：工具快照 + 常驻连接。（server 名只在
/// 连接阶段的错误/日志文案里出现，不常驻。）
struct Slot {
    /// 握手时列到的工具（裸名；builtin server 的工具集是静态的）。
    tools: Vec<Tool>,
    peer: Peer,
}

/// builtin server 群：一次 spawn、常驻进程内，工具以裸名暴露。
/// 结构在构造后不可变（无重连语义——in-process server 没有连接可断）。
pub struct BuiltinHub {
    slots: Vec<Slot>,
}

/// builtin 调用失败：未知裸名工具 vs 调用被拒/超时。
#[derive(Debug)]
pub enum BuiltinFailure {
    /// 裸名不在 builtin 清单（路由失败）。
    UnknownTool,
    /// 调用被 JSON-RPC 层拒绝（如 invalid_params）或超时。
    Call(String),
}

/// 解析 bash 可执行路径：`$PREFIX/bin/bash`（`$PREFIX` 缺省用 Termux
/// 标准前缀）；都不可用时回退 PATH 上的 `bash`——解析延迟到子进程
/// spawn，缺 bash 的环境在调用时以工具错误回喂 LLM。放在 host 侧是
/// 因为 `$PREFIX` 是 Termux 环境知识，不进通用 crate（mcp-builtin）。
fn resolve_bash_path() -> PathBuf {
    let prefix = std::env::var_os("PREFIX")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_TERMUX_PREFIX));
    let candidate = prefix.join("bin").join("bash");
    if candidate.exists() {
        return candidate;
    }
    tracing::debug!(
        target: "mcp_host",
        "no bash at {} (falling back to PATH lookup)",
        candidate.display()
    );
    PathBuf::from("bash")
}

/// builtin 工作目录缺省：`$HOME`（与 bin 的默认清单路径同一 Termux 回退）。
fn default_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/data/data/com.termux/files/home"))
}

/// 以 in-process tuple transport 连接一台 builtin server（模式照抄 core
/// 的 `connect_builtin_server`）：mpsc 双通道零序列化，握手与列工具各包
/// CONNECT_TIMEOUT，防病态 server 钉死启动。
async fn connect_builtin<S>(name: &'static str, server: S) -> Result<Slot, String>
where
    S: ServerHandler + Send + 'static,
{
    // client -> server 方向，随后 server -> client。
    let (a_tx, a_rx) = mpsc::channel::<TxJsonRpcMessage<RoleClient>>(16);
    let (b_tx, b_rx) = mpsc::channel::<TxJsonRpcMessage<RoleServer>>(16);
    let server_transport = (b_tx, a_rx);
    let client_transport = (a_tx, b_rx);

    tokio::spawn(async move {
        match server.serve(server_transport).await {
            Ok(service) => {
                if let Err(e) = service.waiting().await {
                    tracing::warn!(target: "mcp_host", "builtin server '{name}' terminated: {e}");
                }
            }
            Err(e) => {
                tracing::warn!(target: "mcp_host", "builtin server '{name}' failed to serve: {e}");
            }
        }
    });

    let client_info = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("openslate-mcp-host", env!("CARGO_PKG_VERSION")),
    );
    let client = tokio::time::timeout(CONNECT_TIMEOUT, client_info.serve(client_transport))
        .await
        .map_err(|_| format!("builtin '{name}': handshake timeout"))?
        .map_err(|e| format!("builtin '{name}': handshake: {e}"))?;
    let tools = tokio::time::timeout(CONNECT_TIMEOUT, client.peer().list_all_tools())
        .await
        .map_err(|_| format!("builtin '{name}': tool listing timeout"))?
        .map_err(|e| format!("builtin '{name}': tool listing: {e}"))?;
    Ok(Slot {
        tools,
        peer: Arc::new(client),
    })
}

impl BuiltinHub {
    /// 按 `[builtin]` 配置拉起 server 群。`enabled = false` → 空群（不
    /// spawn 任何东西）。fs/edit 嵌入只跟随 `enabled`；bash 嵌入 =
    /// `enabled && bash`。
    ///
    /// in-process server 启动失败视为 bug（与 core 侧 builtin 同语义：
    /// 进程内、与环境无关，失败即代码问题）→ 硬错误，不像外部下游
    /// "失败即跳过"。
    pub async fn spawn(cfg: &BuiltinConfig) -> anyhow::Result<Self> {
        let mut slots = Vec::new();
        if !cfg.enabled {
            return Ok(Self { slots });
        }
        let root = cfg.root.clone().unwrap_or_else(default_root);
        if cfg.bash {
            let bash_path = resolve_bash_path();
            tracing::debug!(target: "mcp_host", "builtin bash program: {}", bash_path.display());
            slots.push(
                connect_builtin("bash", BashServer::new(&root, bash_path))
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?,
            );
        }
        slots.push(
            connect_builtin("fs", FsServer::new(&root))
                .await
                .map_err(|e| anyhow::anyhow!(e))?,
        );
        slots.push(
            connect_builtin("edit", EditServer::new(&root))
                .await
                .map_err(|e| anyhow::anyhow!(e))?,
        );
        Ok(Self { slots })
    }

    /// 空群（builtin 全关时的廉价占位；集成测试也用它隔离下游断言）。
    pub fn empty() -> Self {
        Self { slots: Vec::new() }
    }

    /// 裸名工具清单（`bash` / `read_file` / …，含 schema）。
    pub fn tools(&self) -> Vec<Tool> {
        self.slots
            .iter()
            .flat_map(|s| s.tools.iter().cloned())
            .collect()
    }

    /// 裸名工具名列表（未知工具的错误文案用）。
    pub fn tool_names(&self) -> Vec<String> {
        self.slots
            .iter()
            .flat_map(|s| s.tools.iter().map(|t| t.name.to_string()))
            .collect()
    }

    /// 路由一次裸名调用。in-process 无连接可断：JSON-RPC 层错误不重连，
    /// 直接带给上游（与下游 hub 的重连语义差异是有意的——那里子进程
    /// 可能刚死，这里没有子进程 server）。
    pub async fn call(
        &self,
        tool: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, BuiltinFailure> {
        let Some(slot) = self
            .slots
            .iter()
            .find(|s| s.tools.iter().any(|t| t.name.as_ref() == tool))
        else {
            return Err(BuiltinFailure::UnknownTool);
        };
        let mut params = CallToolRequestParams::new(tool.to_owned());
        if let Some(args) = arguments {
            params = params.with_arguments(args);
        }
        // bash 自带 120s 钳制；300s 外包是防御 in-process server 卡死的
        // 保险（与下游 CALL_TIMEOUT 对齐）。
        match tokio::time::timeout(CALL_TIMEOUT, slot.peer.call_tool(params)).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(e)) => Err(BuiltinFailure::Call(format!("call rejected: {e}"))),
            Err(_) => Err(BuiltinFailure::Call("call timeout".into())),
        }
    }
}
