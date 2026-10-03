//! MCP (Model Context Protocol) adapter: external client + builtin servers.
//!
//! Two kinds of servers feed tools into the `ToolRegistry`:
//!
//! - **External** servers declared in `openslate.toml` (`[mcp.servers.*]`),
//!   connected at startup over stdio (subprocess) or Streamable HTTP. Every
//!   tool they expose is wrapped as an [`McpTool`] namespaced as
//!   `{server}_{tool}`.
//! - **Builtin** in-process servers ([`connect_builtin_servers`]): the
//!   workspace-sandboxed `read_file` / `write_file`, `shell` and `edit_file`
//!   tools from `openslate-mcp-builtin`, served over an in-process tuple
//!   transport (no subprocess, no serialization). Their tools register under
//!   bare names — they are OpenSlate's own tools, so namespacing would only
//!   confuse the LLM.
//!
//! Either way the provider layer only ever sees the unified `Tool` trait.
//!
//! Built on the official `rmcp` crate (protocol `2025-11-25`).
//!
//! # v1 scope
//! - No hot-reload: tools are listed once at connect time (the `ClientConfig`
//!   handler doesn't react to `notifications/tools/list_changed`).
//! - Runtime connect/call failures are non-fatal (warn + skip that server).
//! - `Resource`/`ResourceLink` content blocks emit placeholders (no implicit
//!   `resources/read`); `Image`/`Audio` emit size placeholders (kept out of the
//!   LLM context window).

use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use openslate_mcp_builtin::edit::EditServer;
use openslate_mcp_builtin::fs::FsServer;
use openslate_mcp_builtin::shell::ShellServer;
use openslate_mcp_builtin::skill::SkillServer;
// Re-exported so downstream crates (CLI wiring) can name the skill snapshot
// type without depending on openslate-mcp-builtin directly.
pub use openslate_mcp_builtin::SkillInfo;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, ContentBlock, Implementation,
    JsonObject, Tool as RmcpTool,
};
use rmcp::service::{RoleClient, RoleServer, RunningService, ServerSink, ServiceError, ServiceExt};
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::ServerHandler;
use tokio::process::Command;

use crate::config::{BuiltinToolsConfig, McpServerConfig, TransportConfig};
use crate::error::ToolError;
use crate::tool::Tool;
use crate::types::{ToolOutput, ToolOutputStatus};

/// Per-call timeout for an MCP tool invocation (matches rig's default; prevents
/// a hung Streamable HTTP session from stalling the agent loop forever).
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(300);
/// Timeout for the initial handshake (`serve`) and tool listing (`list_all_tools`)
/// at connect time. A single unreachable server must not block startup.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

// ── pure helpers (kept module-private + unit-testable) ──────────────────────

/// Convert the agent's tool-call arguments (`serde_json::Value`) into the
/// `JsonObject` (`Map<String, Value>`) shape MCP's `call_tool` expects.
/// Non-object values (null/scalar/array) map to `None` (= "no arguments").
fn args_to_json_object(args: &serde_json::Value) -> Option<JsonObject> {
    args.as_object().cloned()
}

/// Collapse an MCP `CallToolResult.content` (`Vec<ContentBlock>`) into the
/// single `String` that `ToolOutput.content` requires.
///
/// `Text` blocks are concatenated verbatim. Multimedia/resource blocks become
/// compact placeholders so they stay out of the LLM context window while still
/// signalling their presence. See module docs for the v1 rationale.
fn downgrade_content_blocks(blocks: Vec<ContentBlock>) -> String {
    let mut out = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text(t) => out.push_str(&t.text),
            ContentBlock::Image(img) => {
                out.push_str(&format!(
                    "[image: {}, {} bytes]",
                    img.mime_type,
                    img.data.len()
                ));
            }
            ContentBlock::Audio(a) => {
                out.push_str(&format!("[audio: {}, {} bytes]", a.mime_type, a.data.len()));
            }
            ContentBlock::Resource(_) => {
                tracing::warn!(
                    target: "openslate_mcp",
                    "MCP resource content block emitted as placeholder (auto-read disabled in v1)"
                );
                out.push_str("[resource content: auto-read disabled]");
            }
            ContentBlock::ResourceLink(_) => {
                out.push_str("[resource link]");
            }
            // `ContentBlock` is #[non_exhaustive]; guard against future variants.
            other => {
                tracing::warn!(
                    target: "openslate_mcp",
                    "unsupported MCP content block emitted as placeholder: {other:?}"
                );
                out.push_str(&format!("[unsupported content block: {other:?}]"));
            }
        }
    }
    out
}

// ── McpTool: one Tool per remote MCP tool ───────────────────────────────────

/// A [`Tool`] backed by a remote MCP server tool.
///
/// `exposed_name` is what the LLM sees (namespaced as `{server}_{tool}`);
/// `definition_name` is the original MCP tool name sent back in `tools/call`.
pub struct McpTool {
    exposed_name: String,
    definition_name: String,
    /// Server alias the exposed name is prefixed with (`None` for the
    /// builtin in-process servers). Drives [`Tool::namespace`] so PTC
    /// exposes the tool as `tools.<server>_<tool>` → `tools.<server>.<tool>`.
    server_alias: Option<String>,
    description: String,
    schema: serde_json::Value,
    client: ServerSink,
    call_timeout: Duration,
}

impl McpTool {
    /// Wrap an `rmcp::model::Tool` discovered on `client`.
    ///
    /// `server_name` namespaces the exposed name as `{server_name}_{tool_name}`
    /// (the original name is kept for `call_tool`), so tools from different MCP
    /// servers never collide. `None` keeps the bare definition name — used for
    /// the builtin in-process servers whose tools are OpenSlate's own.
    pub(crate) fn from_definition(
        def: RmcpTool,
        client: ServerSink,
        server_name: Option<&str>,
    ) -> Self {
        let definition_name = def.name.to_string();
        let exposed_name = match server_name {
            Some(prefix) => format!("{prefix}_{definition_name}"),
            None => definition_name.clone(),
        };
        let description = def.description.as_deref().unwrap_or("").to_owned();
        let schema = def.schema_as_json_value();
        Self {
            exposed_name,
            definition_name,
            server_alias: server_name.map(str::to_owned),
            description,
            schema,
            client,
            call_timeout: DEFAULT_CALL_TIMEOUT,
        }
    }

    /// The name the LLM/tool-registry sees (with prefix applied, if any).
    pub fn exposed_name(&self) -> &str {
        &self.exposed_name
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.exposed_name
    }

    fn namespace(&self) -> Option<String> {
        self.server_alias.clone()
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        let started = std::time::Instant::now();

        // Build the call request, attaching arguments only when the agent
        // supplied a JSON object (mirrors rig's parse_mcp_arguments semantics).
        let mut request = CallToolRequestParams::new(self.definition_name.clone());
        if let Some(obj) = args_to_json_object(args) {
            request = request.with_arguments(obj);
        }

        // Call the remote tool with a bounded timeout.
        let result =
            match tokio::time::timeout(self.call_timeout, self.client.call_tool(request)).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    return Err(ToolError::ExecutionError(format!(
                        "MCP tool '{}' call failed: {e}",
                        self.exposed_name
                    )));
                }
                Err(_) => {
                    return Err(ToolError::ExecutionError(format!(
                        "MCP tool '{}' timed out after {:?}",
                        self.exposed_name, self.call_timeout
                    )));
                }
            };

        let content = downgrade_content_blocks(result.content);
        let bytes = content.len();

        // MCP's own is_error flag is surfaced as ToolOutputStatus::Error, NOT as
        // an Err — the agent loop relies on a ToolOutput to feed the error text
        // back to the LLM (mapping to Err would lose the structured content).
        let status = if result.is_error == Some(true) {
            ToolOutputStatus::Error
        } else {
            ToolOutputStatus::Success
        };

        Ok(ToolOutput {
            content,
            bytes,
            duration_ms: started.elapsed().as_millis() as u64,
            status,
        })
    }
}

// ── McpConnectionGuard: owns RunningService handles ─────────────────────────

/// Owns the live MCP connections so they outlive the `ToolRegistry` / tools.
///
/// Each `RunningService` holds the transport (and, for stdio, the subprocess).
/// Dropping the guard drops the services, which cancels the connections. For
/// deterministic cleanup prefer [`McpConnectionGuard::graceful_shutdown`].
///
/// The handler type is fixed to `ClientConfig` (we advertise ourselves to
/// servers); this keeps the `Vec` element type concrete.
pub struct McpConnectionGuard {
    services: Vec<RunningService<RoleClient, ClientConfig>>,
}

impl McpConnectionGuard {
    pub fn new() -> Self {
        Self {
            services: Vec::new(),
        }
    }

    /// Number of live MCP connections held.
    pub fn len(&self) -> usize {
        self.services.len()
    }

    pub fn is_empty(&self) -> bool {
        self.services.is_empty()
    }

    /// Take ownership of a successfully connected service.
    pub fn push(&mut self, svc: RunningService<RoleClient, ClientConfig>) {
        self.services.push(svc);
    }

    /// Gracefully cancel every connection (sends shutdown; kills subprocesses).
    /// Best-effort: per-service errors are logged, not propagated.
    pub async fn graceful_shutdown(mut self) {
        for svc in self.services.drain(..) {
            if let Err(e) = svc.cancel().await {
                tracing::warn!(target: "openslate_mcp", "MCP service cancel error: {e}");
            }
        }
    }
}

impl Default for McpConnectionGuard {
    fn default() -> Self {
        Self::new()
    }
}

// ── connect_mcp_server + errors ─────────────────────────────────────────────

/// Errors that can occur while connecting to a single MCP server.
///
/// These are intentionally **runtime** errors: per the v1 design they map to a
/// warn + skip at the wiring layer (one bad server must not abort startup).
/// Static/config errors (empty command, bad URL) are caught earlier by
/// `validation::validate_config`.
#[derive(Debug, thiserror::Error)]
pub enum McpConnectError {
    #[error("MCP server '{server}': failed to spawn subprocess: {source}")]
    Spawn {
        server: String,
        #[source]
        source: std::io::Error,
    },

    #[error("MCP server '{server}': {phase} timed out after {secs}s")]
    Timeout {
        server: String,
        phase: &'static str,
        secs: u64,
    },

    #[error("MCP server '{server}': handshake failed: {source}")]
    Handshake {
        server: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("MCP server '{server}': list tools failed: {source}")]
    ListTools {
        server: String,
        #[source]
        source: ServiceError,
    },
}

/// Connect to one MCP server, complete the handshake, list its tools, and build
/// `McpTool` wrappers (exposed names auto-namespaced as `{server_name}_*`).
///
/// Returns the tools plus the live `RunningService` (which the caller must keep
/// alive for the tools to keep working — see [`McpConnectionGuard`]).
pub async fn connect_mcp_server(
    server_name: &str,
    config: &McpServerConfig,
) -> Result<(Vec<McpTool>, RunningService<RoleClient, ClientConfig>), McpConnectError> {
    let client_info = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("openslate", env!("CARGO_PKG_VERSION")),
    );
    let secs = CONNECT_TIMEOUT.as_secs();

    // Build transport + handshake. `serve` performs the MCP `initialize`
    // exchange internally; the awaited result is a ready-to-use service.
    // (client_info is moved into exactly one of the two mutually-exclusive arms.)
    let service = match &config.transport {
        TransportConfig::Stdio { command, args, env } => {
            let mut cmd = Command::new(command);
            cmd.args(args);
            if let Some(env_map) = env {
                for (k, v) in env_map {
                    cmd.env(k, v);
                }
            }
            // Spawn with stderr discarded: MCP servers log startup banners and
            // progress to stderr ("Starting default (STDIO) server...", etc.)
            // which would otherwise interleave with OpenSlate's output. The MCP
            // protocol travels over stdout (unaffected); a server that fails to
            // start is still caught by the handshake timeout/error below.
            // (`TokioChildProcess::new` forces stderr=inherit, so we use the
            // Builder to override it.)
            let (transport, _stderr) = TokioChildProcess::builder(cmd)
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| McpConnectError::Spawn {
                    server: server_name.into(),
                    source: e,
                })?;
            tokio::time::timeout(CONNECT_TIMEOUT, client_info.serve(transport))
                .await
                .map_err(|_| McpConnectError::Timeout {
                    server: server_name.into(),
                    phase: "handshake",
                    secs,
                })?
                .map_err(|e| McpConnectError::Handshake {
                    server: server_name.into(),
                    source: e.into(),
                })?
        }
        TransportConfig::Http { url, headers } => {
            // 自定义头（如 mcp-host 的 Bearer token）经 custom_headers 原样
            // 注入 —— rmcp 仅保留 accept/session/protocol-version/
            // last-event-id，Authorization 等鉴权头可安全透传。
            let transport = match headers {
                None => StreamableHttpClientTransport::from_uri(url.as_str()),
                Some(map) if map.is_empty() => {
                    StreamableHttpClientTransport::from_uri(url.as_str())
                }
                Some(map) => {
                    let mut config =
                        rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::default();
                    config.uri = url.as_str().into();
                    for (name, value) in map {
                        let (name, value) = (http::HeaderName::try_from(name)
                            .map_err(|e| McpConnectError::Handshake {
                                server: server_name.into(),
                                source: Box::new(e),
                            })?,
                        http::HeaderValue::try_from(value.as_str())
                            .map_err(|e| McpConnectError::Handshake {
                                server: server_name.into(),
                                source: Box::new(e),
                            })?);
                        config.custom_headers.insert(name, value);
                    }
                    StreamableHttpClientTransport::with_client(
                        reqwest::Client::new(),
                        config,
                    )
                }
            };
            tokio::time::timeout(CONNECT_TIMEOUT, client_info.serve(transport))
                .await
                .map_err(|_| McpConnectError::Timeout {
                    server: server_name.into(),
                    phase: "handshake",
                    secs,
                })?
                .map_err(|e| McpConnectError::Handshake {
                    server: server_name.into(),
                    source: e.into(),
                })?
        }
    };

    // List tools (auto-paginated). Bounded so a silent server can't hang startup.
    let tools = tokio::time::timeout(CONNECT_TIMEOUT, service.peer().list_all_tools())
        .await
        .map_err(|_| McpConnectError::Timeout {
            server: server_name.into(),
            phase: "list_tools",
            secs,
        })?
        .map_err(|e| McpConnectError::ListTools {
            server: server_name.into(),
            source: e,
        })?;

    let mcp_tools = tools
        .into_iter()
        .map(|t| McpTool::from_definition(t, service.peer().clone(), Some(server_name)))
        .collect();

    Ok((mcp_tools, service))
}

// ── Builtin in-process servers ──────────────────────────────────────────────

/// Serve one builtin MCP server over an in-process tuple transport (a pair of
/// futures mpsc channels — zero serialization, no subprocess) and return the
/// connected client plus the server's tools wrapped as bare-named [`McpTool`]s.
///
/// The spawned server task exits when the client cancels or drops; the caller
/// must keep the returned `RunningService` alive for the tools to keep working.
async fn connect_builtin_server<S>(
    server: S,
) -> anyhow::Result<(Vec<McpTool>, RunningService<RoleClient, ClientConfig>)>
where
    S: ServerHandler + Send + 'static,
{
    use futures::channel::mpsc;
    use rmcp::service::TxJsonRpcMessage;

    // client -> server direction, then server -> client.
    let (a_tx, a_rx) = mpsc::channel::<TxJsonRpcMessage<RoleClient>>(16);
    let (b_tx, b_rx) = mpsc::channel::<TxJsonRpcMessage<RoleServer>>(16);
    let server_transport = (b_tx, a_rx);
    let client_transport = (a_tx, b_rx);

    tokio::spawn(async move {
        match server.serve(server_transport).await {
            Ok(service) => {
                if let Err(e) = service.waiting().await {
                    tracing::warn!(target: "openslate_mcp", "builtin MCP server terminated: {e}");
                }
            }
            Err(e) => {
                tracing::warn!(target: "openslate_mcp", "builtin MCP server failed to serve: {e}");
            }
        }
    });

    // Same handshake as external servers, bounded by the same timeout so a
    // wedged builtin (should never happen, but) cannot hang startup.
    let client_info = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("openslate", env!("CARGO_PKG_VERSION")),
    );
    let client = tokio::time::timeout(CONNECT_TIMEOUT, client_info.serve(client_transport))
        .await
        .context("builtin MCP server handshake timed out")?
        .context("builtin MCP server handshake failed")?;

    let defs = tokio::time::timeout(CONNECT_TIMEOUT, client.peer().list_all_tools())
        .await
        .context("builtin MCP tool listing timed out")?
        .context("builtin MCP tool listing failed")?;

    let tools = defs
        .into_iter()
        .map(|def| McpTool::from_definition(def, client.peer().clone(), None))
        .collect();

    Ok((tools, client))
}

/// Convert a core [`Skill`](crate::skills::Skill) into the builtin skill
/// server's [`SkillInfo`] snapshot. Only name/description/body/dir cross the
/// boundary — the server serves the body from memory, never re-reading the
/// SKILL.md from disk.
impl From<&crate::skills::Skill> for SkillInfo {
    fn from(skill: &crate::skills::Skill) -> Self {
        SkillInfo {
            name: skill.name.clone(),
            description: skill.description.clone(),
            body: skill.body.clone(),
            dir: skill.dir.clone(),
        }
    }
}

/// Connect the in-process builtin tool servers and return their tools under
/// **bare** names (no `{server}_` prefix), ready to be registered into the
/// `ToolRegistry` by the caller.
///
/// Servers are constructed only as needed per `cfg`:
/// - `enabled = false` (or all four tool flags false) → empty result, no
///   server spawned;
/// - the fs server is started when either `read_file` or `write_file` is on,
///   with the disabled sibling filtered out of the returned tools;
/// - `shell` / `edit_file` each get their own server when enabled.
///
/// The skill server is independent of `[builtin_tools]`: it is started iff
/// `skills` is non-empty (see the comment at the call site below).
///
/// The returned services must outlive the registered tools (see
/// [`McpConnectionGuard`]). Unlike external servers, a builtin failure is a
/// hard error: these servers are in-process, so a failure indicates a bug
/// rather than an environment problem.
pub async fn connect_builtin_servers(
    root: &Path,
    cfg: &BuiltinToolsConfig,
    skills: Vec<SkillInfo>,
) -> anyhow::Result<(Vec<McpTool>, Vec<RunningService<RoleClient, ClientConfig>>)> {
    let any_tool = cfg.enabled && (cfg.read_file || cfg.write_file || cfg.shell || cfg.edit_file);
    if !any_tool && skills.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut tools = Vec::new();
    let mut services = Vec::new();

    if cfg.enabled && (cfg.read_file || cfg.write_file) {
        let (fs_tools, service) = connect_builtin_server(FsServer::new(root)).await?;
        tools.extend(fs_tools.into_iter().filter(|t| match t.exposed_name() {
            "read_file" => cfg.read_file,
            "write_file" => cfg.write_file,
            // Deny-by-default: a tool added to FsServer in the future must be
            // wired to an explicit [builtin_tools] switch, never silently
            // exposed just because the fs server happens to be up.
            other => {
                tracing::warn!(
                    target: "openslate_mcp",
                    "builtin fs server exposes tool '{other}' with no [builtin_tools] \
                     switch; hiding it (add a config flag to enable it)"
                );
                false
            }
        }));
        services.push(service);
    }
    if cfg.enabled && cfg.shell {
        let (shell_tools, service) = connect_builtin_server(ShellServer::new(root)).await?;
        tools.extend(shell_tools);
        services.push(service);
    }
    if cfg.enabled && cfg.edit_file {
        let (edit_tools, service) = connect_builtin_server(EditServer::new(root)).await?;
        tools.extend(edit_tools);
        services.push(service);
    }
    // `read_skill` deliberately does NOT get a `[builtin_tools]` switch (the
    // deny-by-default rule for the fs server above does not apply): its gate
    // is the skills catalog being non-empty, which upstream wiring already
    // drives via `[skills] enabled = false` producing an empty catalog.
    if !skills.is_empty() {
        let (skill_tools, service) = connect_builtin_server(SkillServer::new(skills)).await?;
        tools.extend(skill_tools);
        services.push(service);
    }

    Ok((tools, services))
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{AudioContent, ImageContent, TextContent};
    use serde_json::json;

    #[test]
    fn downgrade_text_blocks_concatenate() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("hello ")),
            ContentBlock::Text(TextContent::new("world")),
        ];
        assert_eq!(downgrade_content_blocks(blocks), "hello world");
    }

    #[test]
    fn downgrade_image_audio_emit_size_placeholders() {
        let blocks = vec![
            ContentBlock::Image(ImageContent::new("AAAA", "image/png")),
            ContentBlock::Audio(AudioContent::new("BBBB", "audio/wav")),
        ];
        let s = downgrade_content_blocks(blocks);
        assert!(s.contains("[image: image/png, 4 bytes]"), "{s}");
        assert!(s.contains("[audio: audio/wav, 4 bytes]"), "{s}");
    }

    #[test]
    fn downgrade_empty_blocks_yields_empty_string() {
        assert_eq!(downgrade_content_blocks(Vec::new()), "");
    }

    #[test]
    fn args_object_passes_through() {
        let obj = args_to_json_object(&json!({"repo": ".", "n": 3}));
        let obj = obj.expect("object maps to Some");
        assert_eq!(obj.get("repo").and_then(|v| v.as_str()), Some("."));
        assert_eq!(obj.get("n").and_then(|v| v.as_i64()), Some(3));
    }

    #[test]
    fn args_non_object_becomes_none() {
        assert!(args_to_json_object(&json!(null)).is_none());
        assert!(args_to_json_object(&json!("string")).is_none());
        assert!(args_to_json_object(&json!(42)).is_none());
        assert!(args_to_json_object(&json!([1, 2, 3])).is_none());
        // Empty object is still Some(empty map), not None — matches MCP semantics
        // where an empty argument object is a legitimate "no parameters" payload.
        assert!(args_to_json_object(&json!({})).is_some());
    }

    // ── connect_builtin_servers (in-process tuple transport) ─────────────

    use crate::config::BuiltinToolsConfig;

    fn names(tools: &[McpTool]) -> Vec<String> {
        tools.iter().map(|t| t.exposed_name().to_owned()).collect()
    }

    #[tokio::test]
    async fn builtin_servers_expose_bare_names_by_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let (tools, services) =
            connect_builtin_servers(dir.path(), &BuiltinToolsConfig::default(), Vec::new())
                .await
                .expect("in-process connect should succeed");
        let mut got = names(&tools);
        got.sort();
        assert_eq!(got, vec!["edit_file", "read_file", "shell", "write_file"]);
        assert_eq!(services.len(), 3, "fs + shell + edit servers");
    }

    #[tokio::test]
    async fn builtin_servers_respect_master_switch() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = BuiltinToolsConfig {
            enabled: false,
            ..BuiltinToolsConfig::default()
        };
        let (tools, services) = connect_builtin_servers(dir.path(), &cfg, Vec::new())
            .await
            .expect("disabled → empty, not an error");
        assert!(tools.is_empty());
        assert!(services.is_empty());
    }

    #[tokio::test]
    async fn builtin_servers_all_tools_off_is_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = BuiltinToolsConfig {
            enabled: true,
            read_file: false,
            write_file: false,
            shell: false,
            edit_file: false,
        };
        let (tools, services) = connect_builtin_servers(dir.path(), &cfg, Vec::new())
            .await
            .unwrap();
        assert!(tools.is_empty());
        assert!(services.is_empty());
    }

    #[tokio::test]
    async fn builtin_servers_filter_per_tool_flags() {
        let dir = tempfile::TempDir::new().unwrap();
        // read_file off, write_file on → fs server started, read_file filtered.
        let cfg = BuiltinToolsConfig {
            read_file: false,
            ..BuiltinToolsConfig::default()
        };
        let (tools, services) = connect_builtin_servers(dir.path(), &cfg, Vec::new())
            .await
            .unwrap();
        let mut got = names(&tools);
        got.sort();
        assert_eq!(got, vec!["edit_file", "shell", "write_file"]);
        // fs (write_file only) + shell + edit.
        assert_eq!(services.len(), 3);
    }

    #[tokio::test]
    async fn builtin_read_write_work_end_to_end() {
        let dir = tempfile::TempDir::new().unwrap();
        let (tools, _services) =
            connect_builtin_servers(dir.path(), &BuiltinToolsConfig::default(), Vec::new())
                .await
                .unwrap();
        let write = tools
            .iter()
            .find(|t| t.exposed_name() == "write_file")
            .expect("write_file present");
        let out = write
            .execute(&json!({"path": "hello.txt", "content": "hi"}))
            .await
            .expect("call succeeds");
        assert_eq!(out.status, ToolOutputStatus::Success);
        let read = tools
            .iter()
            .find(|t| t.exposed_name() == "read_file")
            .expect("read_file present");
        let out = read.execute(&json!({"path": "hello.txt"})).await.unwrap();
        assert_eq!(out.content, "hi");
    }

    #[tokio::test]
    async fn builtin_servers_disable_write_file_only() {
        let dir = tempfile::TempDir::new().unwrap();
        // Mirror of builtin_servers_filter_per_tool_flags: write_file off,
        // read_file on → fs server started, write_file filtered.
        let cfg = BuiltinToolsConfig {
            write_file: false,
            ..BuiltinToolsConfig::default()
        };
        let (tools, services) = connect_builtin_servers(dir.path(), &cfg, Vec::new())
            .await
            .unwrap();
        let mut got = names(&tools);
        got.sort();
        assert_eq!(got, vec!["edit_file", "read_file", "shell"]);
        assert_eq!(services.len(), 3, "fs + shell + edit servers");
    }

    // ── skill server (catalog-gated, not [builtin_tools]-gated) ───────────

    fn skill_info(name: &str, dir: &Path) -> SkillInfo {
        SkillInfo {
            name: name.to_owned(),
            description: format!("{name} description"),
            body: format!("{name} body"),
            dir: dir.to_path_buf(),
        }
    }

    #[tokio::test]
    async fn builtin_skill_server_skipped_when_catalog_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let (tools, services) =
            connect_builtin_servers(dir.path(), &BuiltinToolsConfig::default(), Vec::new())
                .await
                .unwrap();
        assert!(
            !tools.iter().any(|t| t.exposed_name() == "read_skill"),
            "empty catalog → no read_skill tool"
        );
        assert_eq!(services.len(), 3, "fs + shell + edit only");
    }

    #[tokio::test]
    async fn builtin_skill_server_started_when_catalog_non_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let (tools, services) = connect_builtin_servers(
            dir.path(),
            &BuiltinToolsConfig::default(),
            vec![skill_info("demo", dir.path())],
        )
        .await
        .unwrap();
        assert!(
            tools.iter().any(|t| t.exposed_name() == "read_skill"),
            "non-empty catalog → read_skill registered under its bare name"
        );
        assert_eq!(services.len(), 4, "fs + shell + edit + skill servers");
    }

    #[tokio::test]
    async fn builtin_skill_server_ignores_builtin_tools_master_switch() {
        // read_skill's gate is the catalog, not [builtin_tools]: with every
        // builtin tool disabled but a non-empty catalog, the skill server
        // still starts and exposes exactly one tool.
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = BuiltinToolsConfig {
            enabled: false,
            ..BuiltinToolsConfig::default()
        };
        let (tools, services) =
            connect_builtin_servers(dir.path(), &cfg, vec![skill_info("demo", dir.path())])
                .await
                .unwrap();
        let got = names(&tools);
        assert_eq!(got, vec!["read_skill".to_owned()]);
        assert_eq!(services.len(), 1, "skill server only");
    }

    #[tokio::test]
    async fn builtin_skill_from_conversion_maps_all_fields() {
        let dir = tempfile::TempDir::new().unwrap();
        let skill_md = dir.path().join("demo").join("SKILL.md");
        std::fs::create_dir_all(skill_md.parent().unwrap()).unwrap();
        let parsed = crate::skills::parse_skill_markdown(
            "---\nname: demo\ndescription: d\n---\nbody text\n",
            &skill_md,
        )
        .unwrap();
        let info = SkillInfo::from(&parsed);
        assert_eq!(info.name, "demo");
        assert_eq!(info.description, "d");
        assert_eq!(info.body, "body text");
        assert_eq!(info.dir, skill_md.parent().unwrap());
    }

    #[tokio::test]
    async fn builtin_read_skill_works_end_to_end_from_fixture() {
        // Full path: discover the checked-in fixture catalog → convert →
        // connect → the agent-visible `read_skill` tool returns the body.
        let fixture_dir =
            std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/skills"));
        let (catalog, warnings) =
            crate::skills::discover_skills(std::slice::from_ref(&fixture_dir));
        assert!(warnings.is_empty(), "fixture warnings: {warnings:?}");
        assert_eq!(catalog.skills().len(), 1, "exactly the pdf fixture");
        let infos: Vec<SkillInfo> = catalog.skills().iter().map(SkillInfo::from).collect();

        let dir = tempfile::TempDir::new().unwrap();
        let (tools, _services) =
            connect_builtin_servers(dir.path(), &BuiltinToolsConfig::default(), infos)
                .await
                .unwrap();
        let read_skill = tools
            .iter()
            .find(|t| t.exposed_name() == "read_skill")
            .expect("read_skill present");
        let out = read_skill
            .execute(&json!({"name": "pdf-processing"}))
            .await
            .expect("read_skill call succeeds");
        assert_eq!(out.status, ToolOutputStatus::Success);
        assert!(
            out.content.contains("<skill name=\"pdf-processing\">"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("scripts/extract.py"),
            "body should reference the bundled script: {}",
            out.content
        );
        // The resource listing includes the nested stub script.
        assert!(
            out.content.contains("<file>scripts/extract.py</file>"),
            "{}",
            out.content
        );
    }

    // ── from_definition namespacing (pure constructor behavior) ──────────

    #[tokio::test]
    async fn from_definition_with_prefix_namespaces_exposed_name() {
        let dir = tempfile::TempDir::new().unwrap();
        // Grab a real tool definition + live sink from an in-process server.
        let (_tools, service) = connect_builtin_server(FsServer::new(dir.path()))
            .await
            .expect("in-process connect should succeed");
        let defs = service
            .peer()
            .list_all_tools()
            .await
            .expect("list tools on live peer");
        let read_def = defs
            .into_iter()
            .find(|d| d.name.as_ref() == "read_file")
            .expect("fs server exposes read_file");

        let tool = McpTool::from_definition(read_def.clone(), service.peer().clone(), Some("srv"));
        // The LLM sees the namespaced form; call_tool keeps the original name.
        assert_eq!(tool.exposed_name(), "srv_read_file");
        assert_eq!(tool.definition_name, "read_file");
        assert_eq!(tool.name(), "srv_read_file");
        // PTC sandbox exposure follows the server alias (tools.srv.read_file).
        assert_eq!(tool.namespace().as_deref(), Some("srv"));
    }

    #[tokio::test]
    async fn from_definition_without_prefix_keeps_bare_name() {
        let dir = tempfile::TempDir::new().unwrap();
        let (_tools, service) = connect_builtin_server(FsServer::new(dir.path()))
            .await
            .expect("in-process connect should succeed");
        let defs = service
            .peer()
            .list_all_tools()
            .await
            .expect("list tools on live peer");
        let read_def = defs
            .into_iter()
            .find(|d| d.name.as_ref() == "read_file")
            .expect("fs server exposes read_file");

        let tool = McpTool::from_definition(read_def, service.peer().clone(), None);
        assert_eq!(tool.exposed_name(), "read_file");
        assert_eq!(tool.definition_name, "read_file");
        // Builtin tools stay flat in the PTC sandbox (no namespace).
        assert_eq!(tool.namespace(), None);
    }

    // ── is_error → ToolOutputStatus::Error e2e (agent-loop feeding contract) ──

    #[tokio::test]
    async fn builtin_business_failure_maps_to_error_status_not_err() {
        let dir = tempfile::TempDir::new().unwrap();
        let (tools, _services) =
            connect_builtin_servers(dir.path(), &BuiltinToolsConfig::default(), Vec::new())
                .await
                .unwrap();
        let read = tools
            .iter()
            .find(|t| t.exposed_name() == "read_file")
            .expect("read_file present");

        // A sandbox violation is a *business* failure: the server answers
        // CallToolResult::error, which the adapter must surface as
        // Ok(ToolOutput{status: Error}) — never Err — so the agent loop can
        // feed the error text back to the LLM instead of aborting the run.
        let out = read
            .execute(&json!({"path": "/etc/hostname"}))
            .await
            .expect("adapter must not return Err on business failures");
        assert_eq!(out.status, ToolOutputStatus::Error);
        assert!(
            out.content.contains("outside workspace"),
            "expected sandbox rejection text, got: {}",
            out.content
        );
    }
}
