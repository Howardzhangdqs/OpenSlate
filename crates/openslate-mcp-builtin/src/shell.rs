//! Shell MCP server: run a shell command in the workspace root.
//!
//! Executes via the platform shell (`sh -c` on Unix, `cmd /C` on Windows)
//! with `current_dir` pinned to the workspace root, separate stdout/stderr
//! capture, a configurable timeout (default 30s, capped at 120s) and output
//! truncation (combined output above 64KB keeps the first and last 24KB).
//!
//! A non-zero exit code is reported as a tool-level error result — with the
//! full output included — so the LLM can see what failed and self-correct.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_handler, tool_router,
};
use tokio::io::AsyncReadExt;

/// Default command timeout.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Upper bound for a caller-supplied timeout (larger values are clamped).
const MAX_TIMEOUT_MS: u64 = 120_000;
/// Combined output above this size is truncated.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// Bytes kept at the head and at the tail when truncating.
const KEEP_BYTES: usize = 24 * 1024;
/// Grace period for draining pipe data after a timeout kill.
const DRAIN_GRACE: Duration = Duration::from_millis(200);

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ShellParams {
    #[schemars(description = "Shell command to run")]
    pub command: String,
    #[schemars(description = "Optional timeout in milliseconds (default 30000, max 120000)")]
    pub timeout_ms: Option<u64>,
}

/// In-process MCP server exposing the `shell` tool in a workspace root.
#[derive(Clone)]
pub struct ShellServer {
    root: PathBuf,
    tool_router: ToolRouter<Self>,
}

fn error_text(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

fn success_text(content: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(content)])
}

/// Keep the head and tail of oversized output, noting how much was dropped.
/// Splits at UTF-8 character boundaries (input comes from lossy conversion).
fn truncate_output(text: &str) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text.to_owned();
    }
    let dropped = text.len() - 2 * KEEP_BYTES;

    let mut head_end = KEEP_BYTES;
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len() - KEEP_BYTES;
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }

    format!(
        "{}\n... [truncated {} bytes] ...\n{}",
        &text[..head_end],
        dropped,
        &text[tail_start..]
    )
}

/// Render the combined command output.
fn format_output(exit_code: i32, stdout: &str, stderr: &str) -> String {
    let mut out = format!("exit_code: {}\n\n--- stdout ---\n", exit_code);
    if stdout.is_empty() {
        out.push_str("(empty)\n");
    } else {
        out.push_str(stdout);
        out.push('\n');
    }
    out.push_str("\n--- stderr ---\n");
    if stderr.is_empty() {
        out.push_str("(empty)\n");
    } else {
        out.push_str(stderr);
        out.push('\n');
    }
    out
}

/// Read a piped child stream fully into `buf`.
async fn read_pipe<R: AsyncReadExt + Unpin>(pipe: Option<&mut R>, buf: &mut Vec<u8>) {
    if let Some(r) = pipe {
        // Errors here mean the pipe broke (child died); keep what we have.
        let _ = r.read_to_end(buf).await;
    }
}

/// Effective timeout for a call: `None` → default; caller-supplied values
/// are clamped to `[1ms, MAX_TIMEOUT_MS]` so a zero does not kill instantly
/// and oversized values cannot pin the agent loop forever.
fn effective_timeout(timeout_ms: Option<u64>) -> Duration {
    Duration::from_millis(
        timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1, MAX_TIMEOUT_MS),
    )
}

/// Substrings marking an environment variable as secret-bearing. Matched
/// case-insensitively against the variable name.
const SENSITIVE_ENV_MARKERS: &[&str] = &[
    "API_KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "PRIVATE_KEY",
];

/// Whether an environment variable holds a secret and must not leak into a
/// model-driven child process (e.g. `OPENSLATE_API_KEY` injected from `.env`
/// — a spawned `env | grep -i key` would otherwise exfiltrate it). Ordinary
/// variables (`PATH`, `HOME`, `LANG`, ...) pass through: a whitelist would
/// break normal commands.
fn is_sensitive_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SENSITIVE_ENV_MARKERS.iter().any(|m| upper.contains(m))
}

#[tool_router]
impl ShellServer {
    /// Create a server that runs commands with `root` as working directory.
    /// The root is canonicalized once; on failure the raw path is kept and
    /// the error surfaces at spawn time instead.
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root
            .as_ref()
            .canonicalize()
            .unwrap_or_else(|_| root.as_ref().to_path_buf());
        Self {
            root,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Run a shell command in the workspace root and return exit code, stdout and stderr"
    )]
    async fn shell(
        &self,
        Parameters(args): Parameters<ShellParams>,
    ) -> Result<CallToolResult, McpError> {
        if args.command.trim().is_empty() {
            return Err(McpError::invalid_params(
                "'command' must not be empty",
                None,
            ));
        }
        let timeout = effective_timeout(args.timeout_ms);

        let mut command = tokio::process::Command::new(SHELL_PROGRAM);
        command
            .arg(SHELL_FLAG)
            .arg(&args.command)
            .current_dir(&self.root)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Scrub secret-bearing variables (API keys, tokens, ...) inherited
        // from this process so the model cannot read them out of the child
        // (e.g. `env | grep -i key`) into its context.
        for (name, _value) in std::env::vars() {
            if is_sensitive_env(&name) {
                command.env_remove(&name);
            }
        }

        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Ok(error_text(format!(
                    "Failed to run command '{}': {}",
                    args.command, e
                )));
            }
        };

        // Keep the pipe handles and buffers outside the timed future so a
        // timeout cannot drop already-captured output.
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let mut stdout_buf: Vec<u8> = Vec::new();
        let mut stderr_buf: Vec<u8> = Vec::new();

        // Wait and pipe reads must run concurrently: a child producing more
        // than the pipe buffer blocks until we read.
        let run = async {
            let read_out = read_pipe(stdout_pipe.as_mut(), &mut stdout_buf);
            let read_err = read_pipe(stderr_pipe.as_mut(), &mut stderr_buf);
            let wait = child.wait();
            // `join!` polls all three; borrows (pipes/buffers vs child) are
            // pairwise disjoint.
            let ((), (), status) = tokio::join!(read_out, read_err, wait);
            status
        };

        let status = match tokio::time::timeout(timeout, run).await {
            Ok(status) => status,
            Err(_elapsed) => {
                // Kill the (direct) shell process, reap it, then drain any
                // pipe data already buffered.
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = tokio::time::timeout(
                    DRAIN_GRACE,
                    read_pipe(stdout_pipe.as_mut(), &mut stdout_buf),
                )
                .await;
                let _ = tokio::time::timeout(
                    DRAIN_GRACE,
                    read_pipe(stderr_pipe.as_mut(), &mut stderr_buf),
                )
                .await;
                let stdout = String::from_utf8_lossy(&stdout_buf);
                let stderr = String::from_utf8_lossy(&stderr_buf);
                let mut msg = format!(
                    "TIMEOUT: command killed after {}ms\n\n{}",
                    timeout.as_millis(),
                    format_output(-1, &stdout, &stderr)
                );
                if msg.len() > MAX_OUTPUT_BYTES {
                    msg = truncate_output(&msg);
                }
                return Ok(error_text(msg));
            }
        };

        let exit_code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&stdout_buf);
        let stderr = String::from_utf8_lossy(&stderr_buf);

        let output = truncate_output(&format_output(exit_code, &stdout, &stderr));
        if exit_code == 0 {
            Ok(success_text(output))
        } else {
            // Non-zero exit: error result, but the full output is included so
            // the LLM can diagnose and retry.
            Ok(error_text(output))
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ShellServer {}

#[cfg(unix)]
const SHELL_PROGRAM: &str = "sh";
#[cfg(unix)]
const SHELL_FLAG: &str = "-c";
#[cfg(windows)]
const SHELL_PROGRAM: &str = "cmd";
#[cfg(windows)]
const SHELL_FLAG: &str = "/C";

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{first_text, is_error, spawn_server};
    use rmcp::model::CallToolRequestParams;

    async fn client_in(
        dir: &tempfile::TempDir,
    ) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
        spawn_server(ShellServer::new(dir.path())).await
    }

    async fn shell(
        client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
        command: &str,
        timeout_ms: Option<u64>,
    ) -> rmcp::model::CallToolResult {
        let mut args = rmcp::object!({"command": command});
        if let Some(ms) = timeout_ms {
            args.insert("timeout_ms".into(), ms.into());
        }
        client
            .call_tool(CallToolRequestParams::new("shell").with_arguments(args))
            .await
            .expect("call_tool ok")
    }

    // Command-level tests use sh syntax / sleep / seq / pwd — Unix only
    // (the implementation targets `sh -c` there and `cmd /C` on Windows).

    #[cfg(unix)]
    #[tokio::test]
    async fn echo_succeeds() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        // timeout_ms omitted → default (30s); a short command must succeed.
        let result = shell(&client, "echo hello-shell", None).await;
        assert!(!is_error(&result), "{}", first_text(&result));
        let text = first_text(&result);
        assert!(text.contains("exit_code: 0"), "{text}");
        assert!(text.contains("hello-shell"), "{text}");
        assert!(text.contains("--- stdout ---"), "{text}");
        assert!(text.contains("--- stderr ---"), "{text}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stderr_is_captured_separately() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = shell(&client, "echo out-msg; echo err-msg 1>&2", None).await;
        let text = first_text(&result);
        assert!(!is_error(&result), "{text}");
        assert!(text.contains("out-msg"), "{text}");
        assert!(text.contains("err-msg"), "{text}");
        // stdout section must not contain the stderr message and vice versa.
        let stdout_sec = text
            .split("--- stdout ---")
            .nth(1)
            .unwrap()
            .split("--- stderr ---")
            .next()
            .unwrap();
        assert!(stdout_sec.contains("out-msg"), "{text}");
        assert!(!stdout_sec.contains("err-msg"), "{text}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_exit_is_error_but_output_visible() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = shell(&client, "echo boom 1>&2; exit 3", None).await;
        assert!(is_error(&result));
        let text = first_text(&result);
        assert!(text.contains("exit_code: 3"), "{text}");
        assert!(text.contains("boom"), "{text}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_command() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let started = std::time::Instant::now();
        let result = shell(&client, "sleep 30", Some(500)).await;
        assert!(is_error(&result));
        let text = first_text(&result);
        assert!(text.contains("TIMEOUT"), "{text}");
        // Returned promptly (kill + drain grace), not after the sleep.
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kill_prevents_followup_commands() {
        // If the kill is removed (or no-ops), `touch marker` runs ~5s later
        // and this test fails: the process must actually die at timeout.
        let dir = tempfile::TempDir::new().unwrap();
        let marker = dir.path().join("marker");
        let client = client_in(&dir).await;

        let result = shell(&client, "sleep 5; touch marker", Some(500)).await;
        assert!(is_error(&result));
        assert!(first_text(&result).contains("TIMEOUT"));

        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            !marker.exists(),
            "command survived the timeout kill and created the marker"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_partial_output_is_included() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = shell(&client, "echo before-hang; sleep 30", Some(500)).await;
        assert!(is_error(&result));
        let text = first_text(&result);
        assert!(text.contains("TIMEOUT"), "{text}");
        assert!(text.contains("before-hang"), "{text}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn oversized_output_completes_and_is_truncated() {
        // Deadlock canary: >64KB of pipe output with an explicit, short
        // timeout. If wait-before-read ever regresses, the pipe fills, the
        // child blocks, and this fails after 5s (not the 30s default).
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        // ~109KB of numbered lines: distinctive head and tail.
        let result = shell(&client, "seq 1 20000", Some(5000)).await;
        let text = first_text(&result);
        assert!(!is_error(&result), "{text}");
        assert!(text.contains("exit_code: 0"), "{text}");
        assert!(text.contains("[truncated"), "{text}");
        // Head kept: the very first numbers.
        assert!(text.contains("\n1\n2\n3\n"), "{text}");
        // Tail kept: the very last number.
        assert!(text.contains("20000"), "{text}");
        // Rough size check: comfortably below the untruncated ~109KB.
        assert!(
            text.len() < MAX_OUTPUT_BYTES + 2 * 1024,
            "len={}",
            text.len()
        );
    }

    #[tokio::test]
    async fn empty_command_is_invalid_params() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let err = client
            .call_tool(
                CallToolRequestParams::new("shell")
                    .with_arguments(rmcp::object!({"command": "  "})),
            )
            .await
            .expect_err("empty command must be invalid params");
        assert!(format!("{err}").contains("command"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_in_workspace_root() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = shell(&client, "pwd", None).await;
        let text = first_text(&result);
        let root = dir.path().canonicalize().unwrap();
        assert!(text.contains(root.to_str().unwrap()), "{text}");
    }

    // ── pure unit tests (platform-independent) ──

    #[test]
    fn effective_timeout_defaults_and_clamps() {
        // Omitted → 30s default.
        assert_eq!(effective_timeout(None), Duration::from_millis(30_000));
        // In-range values pass through.
        assert_eq!(effective_timeout(Some(500)), Duration::from_millis(500));
        assert_eq!(
            effective_timeout(Some(120_000)),
            Duration::from_millis(120_000)
        );
        // Oversized clamps down to the 120s cap.
        assert_eq!(
            effective_timeout(Some(1_000_000)),
            Duration::from_millis(120_000)
        );
        // Zero clamps up to 1ms (no instant kill).
        assert_eq!(effective_timeout(Some(0)), Duration::from_millis(1));
    }

    #[test]
    fn sensitive_env_names_are_scrubbed() {
        // The exact leak this guards against (key injected by wiring.rs).
        assert!(is_sensitive_env("OPENSLATE_API_KEY"));
        assert!(is_sensitive_env("GITHUB_TOKEN"));
        assert!(is_sensitive_env("DB_PASSWORD"));
        // All other markers, incl. word-internal and suffix matches.
        assert!(is_sensitive_env("PASSWD"));
        assert!(is_sensitive_env("CLIENT_SECRET"));
        assert!(is_sensitive_env("AWS_CREDENTIAL"));
        assert!(is_sensitive_env("SSH_PRIVATE_KEY"));
        // Case-insensitive.
        assert!(is_sensitive_env("openslate_api_key"));
        assert!(is_sensitive_env("Github_Token"));
    }

    #[test]
    fn ordinary_env_names_are_kept() {
        assert!(!is_sensitive_env("PATH"));
        assert!(!is_sensitive_env("HOME"));
        assert!(!is_sensitive_env("LANG"));
        assert!(!is_sensitive_env("TERM"));
        assert!(!is_sensitive_env("PWD"));
        assert!(!is_sensitive_env("SHELL"));
        assert!(!is_sensitive_env("USER"));
        assert!(!is_sensitive_env("TMPDIR"));
    }

    #[tokio::test]
    async fn truncate_output_keeps_head_and_tail() {
        let text = format!("{}END", "x".repeat(MAX_OUTPUT_BYTES + 1024));
        let truncated = truncate_output(&text);
        assert!(truncated.starts_with("xxxx"), "head kept");
        assert!(truncated.ends_with("END"), "tail kept");
        assert!(truncated.contains("[truncated"), "marker present");
        assert!(truncated.len() < text.len());
    }

    #[test]
    fn truncate_output_noop_under_limit() {
        let text = "x".repeat(100);
        assert_eq!(truncate_output(&text), text);
    }

    #[test]
    fn truncate_output_drops_exactly_middle_bytes() {
        // Known shape: ASCII (1 byte/char) so byte offsets are exact.
        let total = MAX_OUTPUT_BYTES + 10 * 1024; // 74KB
        let text = "x".repeat(total);
        let truncated = truncate_output(&text);
        let dropped = total - 2 * KEEP_BYTES;
        let expected = format!(
            "{}\n... [truncated {} bytes] ...\n{}",
            &text[..KEEP_BYTES],
            dropped,
            &text[total - KEEP_BYTES..]
        );
        assert_eq!(truncated, expected);
    }

    #[test]
    fn truncate_output_multibyte_cut_point_no_panic() {
        // 3-byte chars with a 1-byte prefix, so the 24KB cut lands mid-char:
        // the boundary walk must adjust instead of slicing panically.
        let text = format!("a{}", "中".repeat(30_000)); // 1 + 90_000 bytes
        assert!(text.len() > MAX_OUTPUT_BYTES);
        let truncated = truncate_output(&text);
        // Head starts at the true beginning; tail ends with whole chars.
        assert!(truncated.starts_with("a中中"), "head intact");
        assert!(truncated.ends_with('中'), "tail intact");
        assert!(truncated.contains("[truncated"), "marker present");
        // The kept head is a strict prefix of the original text (char-safe).
        let head_len = truncated.find("\n... [truncated").unwrap();
        assert!(text.starts_with(&truncated[..head_len]));
    }
}
