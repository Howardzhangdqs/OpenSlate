//! MCP (Model Context Protocol) client adapter.
//!
//! Each MCP server declared in `openslate.toml` (`[mcp.servers.*]`) is connected
//! at startup; every tool it exposes is wrapped as an [`McpTool`] and registered
//! into the `ToolRegistry`, making external tools transparent to the provider
//! layer (which only ever sees the unified `Tool` trait).
//!
//! Built on the official `rmcp` crate (protocol `2025-11-25`), supporting both
//! stdio (subprocess) and Streamable HTTP transports. The `mcp` cargo feature
//! gates this entire module.
//!
//! # v1 scope
//! - No hot-reload: tools are listed once at connect time (`()`-less handler
//!   means we don't react to `notifications/tools/list_changed`).
//! - Runtime connect/call failures are non-fatal (warn + skip that server).
//! - `Resource`/`ResourceLink` content blocks emit placeholders (no implicit
//!   `resources/read`); `Image`/`Audio` emit size placeholders (kept out of the
//!   LLM context window).

use std::time::Duration;

use async_trait::async_trait;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientInfo, ContentBlock, Implementation,
    JsonObject, Tool as RmcpTool,
};
use rmcp::service::{RunningService, RoleClient, ServerSink, ServiceExt, ServiceError};
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use tokio::process::Command;

use crate::config::{McpServerConfig, TransportConfig};
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
                out.push_str(&format!(
                    "[audio: {}, {} bytes]",
                    a.mime_type,
                    a.data.len()
                ));
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
    /// servers — and from builtins — never collide.
    pub(crate) fn from_definition(def: RmcpTool, client: ServerSink, server_name: &str) -> Self {
        let definition_name = def.name.to_string();
        let exposed_name = format!("{server_name}_{definition_name}");
        let description = def.description.as_deref().unwrap_or("").to_owned();
        let schema = def.schema_as_json_value();
        Self {
            exposed_name,
            definition_name,
            description,
            schema,
            client,
            call_timeout: DEFAULT_CALL_TIMEOUT,
        }
    }

    /// The name the LLM/tool-registry sees (with prefix applied).
    pub fn exposed_name(&self) -> &str {
        &self.exposed_name
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.exposed_name
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
        let result = match tokio::time::timeout(self.call_timeout, self.client.call_tool(request))
            .await
        {
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
/// The handler type is fixed to `ClientInfo` (we advertise ourselves to
/// servers); this keeps the `Vec` element type concrete.
pub struct McpConnectionGuard {
    services: Vec<RunningService<RoleClient, ClientInfo>>,
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
    pub fn push(&mut self, svc: RunningService<RoleClient, ClientInfo>) {
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
) -> Result<(Vec<McpTool>, RunningService<RoleClient, ClientInfo>), McpConnectError> {
    let client_info = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("openslate", env!("CARGO_PKG_VERSION")),
    );
    let secs = CONNECT_TIMEOUT.as_secs();

    // Build transport + handshake. `serve` performs the MCP `initialize`
    // exchange internally; the awaited result is a ready-to-use service.
    // (client_info is moved into exactly one of the two mutually-exclusive arms.)
    let service = match &config.transport {
        TransportConfig::Stdio {
            command,
            args,
            env,
        } => {
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
        TransportConfig::Http { url } => {
            let transport = StreamableHttpClientTransport::from_uri(url.as_str());
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
        .map(|t| McpTool::from_definition(t, service.peer().clone(), server_name))
        .collect();

    Ok((mcp_tools, service))
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
}
