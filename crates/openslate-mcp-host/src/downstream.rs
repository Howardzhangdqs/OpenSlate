//! 下游 stdio MCP server 管理：懒连接、失效重连、工具清单聚合。
//!
//! 保活策略（v1）：惰性重连 —— `call`/`list` 时的传输错误会弃置连接，
//! 下一次调用自动重新 spawn 子进程并握手。比常驻守护循环简单且不会误杀
//! 慢启动的 server；代价是故障后的首个请求承担重连延迟（本地子进程 +
//! 握手通常 < 1s）。

use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, Implementation, Tool,
};
use rmcp::service::{RunningService, RoleClient};
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::ServiceExt;
use serde::Deserialize;

/// 下游 spawn + 握手超时（npx 首次解析较慢，给足）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 单次工具调用超时（与 OpenSlate 侧 per-call 300s 对齐）。
const CALL_TIMEOUT: Duration = Duration::from_secs(300);

/// 工具名前缀分隔符：`alias__tool`。
pub const PREFIX_SEP: &str = "__";

/// 单个下游 stdio server 的启动规格（清单 `[servers.<alias>]` 表）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSpec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// 追加到子进程环境的键值（不替换整个环境 —— PATH 等继承自 Termux）。
    pub env: Option<HashMap<String, String>>,
}

type Peer = Arc<RunningService<RoleClient, ClientConfig>>;

struct Slot {
    spec: ServerSpec,
    peer: Option<Peer>,
}

/// 全部下游的连接池。多线程共享（`Arc<DownstreamHub>`）。
pub struct DownstreamHub {
    // std Mutex：临界区只做指针存取/规格读取，绝不跨 await 持锁。
    slots: Mutex<HashMap<String, Slot>>,
}

/// 下游调用失败：区分"未知 server"与"连接/传输故障"。
#[derive(Debug)]
pub enum CallFailure {
    /// 未知 alias（无法路由）。
    UnknownServer,
    /// 连接/握手失败、调用超时或被下游 JSON-RPC 层拒绝。
    Transport(String),
}

impl std::fmt::Display for CallFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallFailure::UnknownServer => write!(f, "unknown downstream server"),
            CallFailure::Transport(m) => write!(f, "downstream transport failure: {m}"),
        }
    }
}

async fn spawn_peer(spec: &ServerSpec) -> Result<Peer, String> {
    // tokio Command（std Command 无 From<Command> → CommandWrap 转换）。
    let mut cmd = tokio::process::Command::new(&spec.command);
    cmd.args(&spec.args);
    if let Some(env) = &spec.env {
        for (k, v) in env {
            cmd.env(k, v);
        }
    }
    // stderr 丢弃：MCP server 的启动横幅/日志会刷屏（协议走 stdout）。
    let (transport, _stderr) = TokioChildProcess::builder(cmd)
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn '{}': {e}", spec.command))?;

    let client = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("openslate-mcp-host", env!("CARGO_PKG_VERSION")),
    );
    let service = tokio::time::timeout(CONNECT_TIMEOUT, client.serve(transport))
        .await
        .map_err(|_| "handshake timeout".to_string())?
        .map_err(|e| format!("handshake: {e}"))?;
    Ok(Arc::new(service))
}

impl DownstreamHub {
    pub fn new(specs: BTreeMap<String, ServerSpec>) -> Self {
        let slots = specs
            .into_iter()
            .map(|(alias, spec)| {
                (
                    alias,
                    Slot {
                        spec,
                        peer: None,
                    },
                )
            })
            .collect();
        Self {
            slots: Mutex::new(slots),
        }
    }

    /// 已配置的 alias 列表（清单顺序）。
    pub fn aliases(&self) -> Vec<String> {
        self.slots.lock().unwrap().keys().cloned().collect()
    }

    /// alias 是否存在（路由前的快速检查）。
    pub fn contains(&self, alias: &str) -> bool {
        self.slots.lock().unwrap().contains_key(alias)
    }

    /// 取（或建立）下游连接。连接失败时槽位保持空，下次调用重试。
    async fn peer(&self, alias: &str) -> Result<Peer, String> {
        // 快路径：已有连接。
        {
            let slots = self.slots.lock().unwrap();
            if let Some(slot) = slots.get(alias) {
                if let Some(peer) = &slot.peer {
                    return Ok(peer.clone());
                }
            }
        }
        // 慢路径：取规格（立即放锁）→ spawn + 握手 → 存回。
        let spec = {
            let mut slots = self.slots.lock().unwrap();
            match slots.get_mut(alias) {
                Some(slot) => {
                    if let Some(peer) = &slot.peer {
                        return Ok(peer.clone()); // 并发窗口内别人已连上
                    }
                    slot.spec.clone()
                }
                None => return Err(format!("unknown server '{alias}'")),
            }
        };
        let peer = spawn_peer(&spec).await?;
        self.slots
            .lock()
            .unwrap()
            .get_mut(alias)
            .map(|s| s.peer = Some(peer.clone()));
        Ok(peer)
    }

    /// 弃置连接（调用方在传输错误后调用，触发下次重连）。
    fn invalidate(&self, alias: &str) {
        if let Ok(mut slots) = self.slots.lock() {
            if let Some(slot) = slots.get_mut(alias) {
                slot.peer = None;
            }
        }
    }

    /// 聚合所有下游的 tools，加 `alias__` 前缀。单个下游失败只跳过并记
    /// warn（与 OpenSlate 侧"连接失败不拖垮启动"的语义一致）。
    pub async fn collect_tools(&self) -> Vec<Tool> {
        let aliases = self.aliases();
        let mut all = Vec::new();
        for alias in &aliases {
            let tools = self.tools_of(alias).await;
            all.extend(tools);
        }
        all
    }

    async fn tools_of(&self, alias: &str) -> Vec<Tool> {
        let peer = match self.peer(alias).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(target: "mcp_host", "downstream '{alias}' connect failed: {e}");
                return Vec::new();
            }
        };
        match tokio::time::timeout(CALL_TIMEOUT, peer.list_all_tools()).await {
            Ok(Ok(tools)) => {
                // Tool 为 non_exhaustive：clone 后改字段（不能 struct literal）。
                let mut prefixed: Vec<Tool> = Vec::with_capacity(tools.len());
                for mut t in tools {
                    t.name = format!("{alias}{PREFIX_SEP}{}", t.name).into();
                    prefixed.push(t);
                }
                prefixed
            }
            Ok(Err(e)) => {
                tracing::warn!(target: "mcp_host", "downstream '{alias}' list_tools failed: {e}");
                self.invalidate(alias);
                Vec::new()
            }
            Err(_) => {
                tracing::warn!(target: "mcp_host", "downstream '{alias}' list_tools timeout");
                self.invalidate(alias);
                Vec::new()
            }
        }
    }

    /// 路由一次工具调用：传输类失败重连一次再试，仍失败则报错。
    pub async fn call(
        &self,
        alias: &str,
        tool: &str,
        arguments: Option<rmcp::model::JsonObject>,
    ) -> Result<CallToolResult, CallFailure> {
        if !self.contains(alias) {
            return Err(CallFailure::UnknownServer);
        }
        let mut params = CallToolRequestParams::new(tool.to_owned());
        if let Some(args) = arguments {
            params = params.with_arguments(args);
        }
        let mut last_err = String::new();
        // 传输类失败重连一次（首次可能只是子进程刚死）。
        for _attempt in 0..2 {
            let peer = match self.peer(alias).await {
                Ok(p) => p,
                Err(e) => {
                    last_err = e;
                    continue;
                }
            };
            match tokio::time::timeout(CALL_TIMEOUT, peer.call_tool(params.clone())).await {
                Ok(Ok(result)) => return Ok(result),
                // JSON-RPC 层错误（工具参数被拒等）：连接大概率仍可用，
                // 不重连，直接把错误信息带给上游。
                Ok(Err(e)) => return Err(CallFailure::Transport(format!("call rejected: {e}"))),
                Err(_) => {
                    last_err = "call timeout".into();
                    self.invalidate(alias);
                }
            }
        }
        Err(CallFailure::Transport(last_err))
    }
}
