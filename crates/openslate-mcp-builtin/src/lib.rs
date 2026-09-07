//! Built-in MCP servers for OpenSlate.
//!
//! This crate hosts the in-process MCP servers that replace the legacy
//! built-in tools (`read_file` / `write_file` in `openslate-core::tool`):
//!
//! - [`fs`]: `read_file` / `write_file`, sandboxed to a workspace root
//!   (faithful port of the original tools' semantics and messages).
//! - [`shell`]: `shell`, runs a command in the workspace root with timeout
//!   and output truncation.
//! - [`edit`]: `edit_file`, applies a small context patch (with `@@` anchors)
//!   to an existing file via the pure engine in [`edit::editor`].
//!
//! Each server is an `rmcp` `ServerHandler` intended to be served in-process
//! (no stdio / HTTP transport). Everything here is a library: nothing writes
//! to stdout/stderr, so serving these over stdio stays protocol-clean.

pub mod edit;
pub mod fs;
pub mod shell;

#[cfg(test)]
pub(crate) mod testing {
    //! Shared helpers for in-process server tests: an mpsc "tuple transport"
    //! pair connecting a server to a client inside one process (zero
    //! serialization), plus small result-inspection helpers.

    use futures::channel::mpsc;
    use rmcp::service::TxJsonRpcMessage;
    use rmcp::{
        RoleClient, RoleServer, ServerHandler, ServiceExt, model::CallToolResult,
        service::RunningService,
    };

    /// Serve `server` on an in-process tuple transport and return a connected
    /// client. The server task is detached; it exits when the client cancels
    /// or drops the connection.
    pub(crate) async fn spawn_server<S>(server: S) -> RunningService<RoleClient, ()>
    where
        S: ServerHandler + 'static,
    {
        // client -> server direction: elements typed as client tx / server rx.
        let (a_tx, a_rx) = mpsc::channel::<TxJsonRpcMessage<RoleClient>>(16);
        // server -> client direction.
        let (b_tx, b_rx) = mpsc::channel::<TxJsonRpcMessage<RoleServer>>(16);

        let server_transport = (b_tx, a_rx);
        let client_transport = (a_tx, b_rx);

        tokio::spawn(async move {
            let service = server.serve(server_transport).await.expect("serve");
            service.waiting().await.expect("waiting");
        });
        ().serve(client_transport).await.expect("client serve")
    }

    /// Extract the first text block of a tool result (empty string if none).
    pub(crate) fn first_text(result: &CallToolResult) -> String {
        result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap_or_default()
    }

    /// True when the result carries `is_error: true`.
    pub(crate) fn is_error(result: &CallToolResult) -> bool {
        result.is_error == Some(true)
    }
}
