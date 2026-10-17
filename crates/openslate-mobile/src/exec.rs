//! 手机端 shell 执行层（独立运行模式）。
//!
//! bash 工具按设置多选动态注册（`register_bash_tools`，切换即热生效）：
//!
//! - 只选 native：`bash` = 进程内 Android 系统 sh（toybox）
//! - 只选 termux：`bash` = Termux RUN_COMMAND（宿主执行）
//! - 两个都选：`bash` = native，`termux_bash` = termux
//!
//! 命名固定（`bash` / `termux_bash`，见 bootstrap 默认 root.md 白名单），
//! 变化的是注册与否：未选中的名字直接从注册表注销，模型不可见；运行
//! 中途切换时旧名字调用会得到 NotFound，模型可自纠。
//!
//! ## MCP 接管（termux bash 降级阶梯的 host 侧半边）
//!
//! MCP 聚合 host 在线时其内置 bash server 以裸名 `bash` 列工具，经
//! builtin 小模板展开注册（默认 `termux_bash`；见
//! [`mcp_owned_bash_names`]）。此时 host 侧的 bash 工具（HostTool 壳 /
//! native）对该名让位：不注册、不清理。三个选择点（装配 / reload /
//! set_exec_backends）都先经统一谓词重算，见 runtime.rs。

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use openslate_core::error::ToolError;
use openslate_core::tool::{Tool, ToolRegistry};
use openslate_core::types::{ToolOutput, ToolOutputStatus};
use serde_json::Value;
use tokio::io::AsyncReadExt;

use crate::hostcall::{HostCallRouter, HostTool};

/// 默认命令超时。
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// 调用方可指定超时的上限（超出收敛）。
pub const MAX_TIMEOUT_MS: u64 = 120_000;
/// 合并输出超过该大小即截断（保留头尾）。
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// 截断时头尾各保留的字节数。
const KEEP_BYTES: usize = 24 * 1024;

/// bash 工具名（固定；白名单见 bootstrap DEFAULT_ROOT_AGENT_MD）。
pub const BASH_TOOL: &str = "bash";
pub const TERMUX_BASH_TOOL: &str = "termux_bash";

/// 本工具历史上用过的所有名字（切换时统一清理）。pub：runtime 的
/// reload 让位判定按它界定「bash 工具域」——MCP 新名册里的名字落在
/// 此域内才允许让 host bash 工具让位，其余撞名仍是真冲突（回滚）。
pub const ALL_BASH_NAMES: &[&str] = &[BASH_TOOL, TERMUX_BASH_TOOL, "shell.run", "termux.run"];

// ── MCP 接管判定（termux bash 降级阶梯的统一谓词，三个选择点共用）──

/// builtin 小模板按 `[mcp.servers]` 各键展开上游裸名 `bash` 得到的注册
/// 名候选（每键一个；默认模板 + 键 `termux` → `termux_bash`）。展开走
/// core [`format_tool_name`] 的裸名分支——生效模板统一经
/// `McpConfig::builtin_template_or_default()` 解析（接缝契约），**不硬
/// 编码 `termux_bash` 字面**：用户改 `[mcp].builtin_tool_name_template`
/// 后 MCP 侧注册名随之变化（如 `{prefix}.bash` → `termux.bash`）。
pub fn mcp_bash_expanded_names(mcp: &openslate_core::config::McpConfig) -> Vec<String> {
    let builtin = mcp.builtin_template_or_default();
    mcp.servers
        .keys()
        .map(|prefix| {
            // 上游裸名 `bash` 不含 `__` → format_tool_name 只走 builtin
            // 半边；首参（大模板）在该分支不参与展开，传默认值占位。
            openslate_core::mcp::format_tool_name(
                openslate_core::mcp::DEFAULT_TOOL_NAME_TEMPLATE,
                builtin,
                prefix,
                BASH_TOOL,
            )
        })
        .collect()
}

/// 名册中由 MCP 提供的 bash 工具名（= HostTool 让位名单，降级阶梯的
/// 统一判定谓词；装配期 / reload / set_exec_backends 三处共用）。两条
/// 来源取并集：
/// 1. builtin 模板展开名 ∩ 名册——规范态（模板与名册同源）；
/// 2. 名册 ∩ [`ALL_BASH_NAMES`]——防「模板已改、名册还是旧模板注册的」
///    短暂不一致窗口误杀：那些名字是 MCP 的工具，绝不能 unregister。
///
/// 非空 = MCP 已接管 termux bash：[`register_bash_tools`] 对这些名字
/// 不清理、不注册（让位）；为空 = MCP 缺席，HostTool/native 兜底。
pub fn mcp_owned_bash_names(
    roster: &[String],
    mcp: &openslate_core::config::McpConfig,
) -> Vec<String> {
    let mut owned: Vec<String> = mcp_bash_expanded_names(mcp)
        .into_iter()
        .filter(|n| roster.iter().any(|r| r == n))
        .collect();
    for name in roster {
        if ALL_BASH_NAMES.contains(&name.as_str()) && !owned.contains(name) {
            owned.push(name.clone());
        }
    }
    owned
}

// ── 后端多选状态 ────────────────────────────────────────────────────────────

/// bash 执行后端多选（位标志，供 AtomicU8 存储）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExecSelection {
    pub native: bool,
    pub termux: bool,
}

impl ExecSelection {
    /// 解析设置值（逗号分隔："native"、"termux"、"native,termux"）。
    /// 未知值忽略；全未知 = 空（调用方应回退 native）。
    pub fn parse_csv(s: &str) -> Self {
        let mut sel = Self::default();
        for part in s.split(',') {
            match part.trim() {
                "native" => sel.native = true,
                "termux" => sel.termux = true,
                _ => {}
            }
        }
        sel
    }

    /// 允许全不选（= 不注册任何 bash 工具，模型没有命令执行能力），
    /// 因此不再自动回退；保留 normalized 以示语义（恒等）。
    pub fn normalized(self) -> Self {
        self
    }

    fn bits(self) -> u8 {
        (self.native as u8) | ((self.termux as u8) << 1)
    }

    fn from_bits(b: u8) -> Self {
        Self {
            native: b & 1 != 0,
            termux: b & 2 != 0,
        }
    }
}

/// 运行期可变的多选状态（FFI set_exec_backends 热更新）。
#[derive(Clone)]
pub struct ExecSelectionCell(Arc<AtomicU8>);

impl ExecSelectionCell {
    pub fn new(sel: ExecSelection) -> Self {
        Self(Arc::new(AtomicU8::new(sel.normalized().bits())))
    }

    pub fn get(&self) -> ExecSelection {
        ExecSelection::from_bits(self.0.load(Ordering::SeqCst))
    }

    pub fn set(&self, sel: ExecSelection) {
        self.0.store(sel.normalized().bits(), Ordering::SeqCst);
    }
}

/// 按多选结果（重新）注册 bash 工具：先清理全部旧名字，再按需注册。
/// 切换后端时对同一 registry 重调即可热生效。
///
/// `mcp_owned`（见 [`mcp_owned_bash_names`]）= MCP 已接管的 bash 名单：
/// 这些名字是 MCP 工具的，本函数**不清理也不注册**（让位）——旧实现
/// 无条件 unregister 全部 bash 名，会把 MCP 刚注册的 bash 工具一并拆
/// 掉（降级阶梯在「切后端」处断裂）。
pub fn register_bash_tools(
    registry: &ToolRegistry,
    sel: ExecSelection,
    workspace: PathBuf,
    router: std::sync::Arc<HostCallRouter>,
    mcp_owned: &[String],
) {
    let owned = |n: &str| mcp_owned.iter().any(|o| o.as_str() == n);
    // sel 可为空（全不选）：此时只清理，bash 功能整体下线——但 MCP 已
    // 接管的名字除外（那是 MCP 的工具，其去留由 reload/下次接管重算）。
    for name in ALL_BASH_NAMES {
        if !owned(name) {
            registry.unregister(name);
        }
    }
    if sel.native && !owned(BASH_TOOL) {
        registry.register_boxed(Box::new(NativeShellTool::new(
            BASH_TOOL.to_owned(),
            workspace,
        )));
    }
    if sel.termux {
        // 两个都选时 native 独占 `bash`，termux 用 `termux_bash`；
        // 只选 termux 时由它顶替 `bash`。
        let name = if sel.native {
            TERMUX_BASH_TOOL
        } else {
            BASH_TOOL
        };
        if !owned(name) {
            registry.register_boxed(Box::new(HostTool::new(
                name,
                "Run a shell command inside the local Termux environment (Android) and return its \
                 combined stdout/stderr. The Termux bootstrap (full coreutils, apt, python, ...) is \
                 available; network access from Termux may be restricted by the device network. \
                 Avoid interactive commands. Output beyond ~8KB is truncated.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "shell command to execute" }
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }),
                router,
            )));
        }
    }
}

// ── 原生 bash（进程内 /system/bin/sh）───────────────────────────────────────

/// 进程内原生 shell 工具。cwd 固定到 workspace_dir（App 私有可写目录），
/// 环境变量脱敏后继承，输出格式与桌面 builtin shell 一致（exit_code +
/// stdout/stderr 分节）。
pub struct NativeShellTool {
    name: String,
    root: PathBuf,
}

impl NativeShellTool {
    pub fn new(name: String, root: PathBuf) -> Self {
        Self { name, root }
    }
}

#[async_trait]
impl Tool for NativeShellTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "Run a shell command locally on the device (Android system sh + toybox) and return \
         exit code with separated stdout/stderr. Works fully offline without Termux. The \
         toybox coreutils subset is available (ls, cat, grep, sed, ps, curl, ...); apt and \
         full GNU tools are NOT available. Commands run in the app workspace directory. \
         Avoid interactive commands. Output beyond 64KB is truncated."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "shell command to execute" },
                "timeout_ms": {
                    "type": "integer",
                    "description": "optional timeout in milliseconds (default 30000, max 120000)"
                }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolOutput, ToolError> {
        let started = std::time::Instant::now();
        let error_out = |msg: String| {
            Ok(ToolOutput {
                bytes: msg.len(),
                content: msg,
                duration_ms: started.elapsed().as_millis() as u64,
                status: ToolOutputStatus::Error,
            })
        };

        let command = args
            .get("command")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .trim()
            .to_owned();
        if command.is_empty() {
            return error_out(format!("{}: 'command' must not be empty", self.name));
        }
        let timeout = Duration::from_millis(
            args.get("timeout_ms")
                .and_then(|t| t.as_u64())
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .clamp(1, MAX_TIMEOUT_MS),
        );

        let mut cmd = tokio::process::Command::new(shell_program());
        cmd.arg("-c")
            .arg(&command)
            .current_dir(&self.root)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // 与桌面 builtin shell 同款脱敏：模型可经 `env` 读到继承变量，
        // API key 等敏感命名绝不下发。
        for (name, _value) in std::env::vars() {
            if is_sensitive_env(&name) {
                cmd.env_remove(&name);
            }
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return error_out(format!("{}: failed to spawn: {e}", self.name)),
        };
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let mut stdout_buf: Vec<u8> = Vec::new();
        let mut stderr_buf: Vec<u8> = Vec::new();

        // 并发读管道 + wait（子进程输出超过管道缓冲时会阻塞，不并发即死锁）。
        let run = async {
            let read_out = async {
                if let Some(r) = stdout_pipe.as_mut() {
                    let _ = r.read_to_end(&mut stdout_buf).await;
                }
            };
            let read_err = async {
                if let Some(r) = stderr_pipe.as_mut() {
                    let _ = r.read_to_end(&mut stderr_buf).await;
                }
            };
            let wait = child.wait();
            let ((), (), status) = tokio::join!(read_out, read_err, wait);
            status
        };

        let status = match tokio::time::timeout(timeout, run).await {
            Ok(status) => status,
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                // 收敛已缓冲的输出（200ms 宽限）。
                if let Some(r) = stdout_pipe.as_mut() {
                    let _ = tokio::time::timeout(
                        Duration::from_millis(200),
                        r.read_to_end(&mut stdout_buf),
                    )
                    .await;
                }
                if let Some(r) = stderr_pipe.as_mut() {
                    let _ = tokio::time::timeout(
                        Duration::from_millis(200),
                        r.read_to_end(&mut stderr_buf),
                    )
                    .await;
                }
                let msg = truncate_output(&format!(
                    "TIMEOUT: command killed after {}ms\n\n{}",
                    timeout.as_millis(),
                    format_output(-1, &stdout_buf, &stderr_buf)
                ));
                return error_out(msg);
            }
        };

        let exit_code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
        let output = truncate_output(&format_output(exit_code, &stdout_buf, &stderr_buf));
        Ok(ToolOutput {
            bytes: output.len(),
            content: output,
            duration_ms: started.elapsed().as_millis() as u64,
            status: if exit_code == 0 {
                ToolOutputStatus::Success
            } else {
                ToolOutputStatus::Error
            },
        })
    }
}

/// Android 上必须用绝对路径 `/system/bin/sh`：App 进程 PATH 不含它。
#[cfg(target_os = "android")]
fn shell_program() -> &'static str {
    "/system/bin/sh"
}
#[cfg(not(target_os = "android"))]
fn shell_program() -> &'static str {
    "/bin/sh"
}

fn format_output(exit_code: i32, stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    let mut out = format!("exit_code: {exit_code}\n\n--- stdout ---\n");
    if stdout.is_empty() {
        out.push_str("(empty)\n");
    } else {
        out.push_str(&stdout);
        out.push('\n');
    }
    out.push_str("\n--- stderr ---\n");
    if stderr.is_empty() {
        out.push_str("(empty)\n");
    } else {
        out.push_str(&stderr);
        out.push('\n');
    }
    out
}

/// 超限输出保头尾、按 UTF-8 字符边界切（与桌面 builtin shell 同策略）。
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
        "{}\n... [truncated {dropped} bytes] ...\n{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

/// 敏感环境变量名匹配（与桌面 builtin shell 一致：API_KEY/TOKEN/…）。
const SENSITIVE_ENV_MARKERS: &[&str] = &[
    "API_KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "PRIVATE_KEY",
];

fn is_sensitive_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SENSITIVE_ENV_MARKERS.iter().any(|m| upper.contains(m))
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_only_bash_executes() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = std::sync::mpsc::channel::<String>();
        struct Cb(std::sync::mpsc::Sender<String>);
        impl crate::events::EventCallback for Cb {
            fn on_event(&self, _e: String) {}
        }
        let router = crate::hostcall::HostCallRouter::new(
            crate::events::EventSink::new(std::sync::Arc::new(Cb(tx))),
            Duration::from_secs(2),
        );
        let reg = ToolRegistry::new();
        register_bash_tools(
            &reg,
            ExecSelection {
                native: true,
                termux: false,
            },
            dir.path().to_path_buf(),
            router.clone(),
            &[],
        );
        let names = reg.tool_names();
        assert!(names.contains(&"bash".to_owned()), "{names:?}");
        assert!(!names.contains(&"termux_bash".to_owned()), "{names:?}");

        let out = reg
            .execute("bash", &serde_json::json!({"command": "echo ok"}))
            .await
            .unwrap();
        assert!(out.content.contains("ok"), "{}", out.content);
        assert_eq!(out.status, ToolOutputStatus::Success);
    }

    #[tokio::test]
    async fn both_selected_native_bash_and_termux_bash() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = std::sync::mpsc::channel::<String>();
        struct Cb(std::sync::mpsc::Sender<String>);
        impl crate::events::EventCallback for Cb {
            fn on_event(&self, _e: String) {}
        }
        let router = crate::hostcall::HostCallRouter::new(
            crate::events::EventSink::new(std::sync::Arc::new(Cb(tx))),
            Duration::from_secs(2),
        );
        let reg = ToolRegistry::new();
        register_bash_tools(
            &reg,
            ExecSelection {
                native: true,
                termux: true,
            },
            dir.path().to_path_buf(),
            router,
            &[],
        );
        let names = reg.tool_names();
        assert!(names.contains(&"bash".to_owned()), "{names:?}");
        assert!(names.contains(&"termux_bash".to_owned()), "{names:?}");
        // bash 仍是 native：直接执行成功（不经 host call）。
        let out = reg
            .execute("bash", &serde_json::json!({"command": "pwd"}))
            .await
            .unwrap();
        assert_eq!(out.status, ToolOutputStatus::Success);
    }

    #[tokio::test]
    async fn hot_swap_between_selections() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = std::sync::mpsc::channel::<String>();
        struct Cb(std::sync::mpsc::Sender<String>);
        impl crate::events::EventCallback for Cb {
            fn on_event(&self, _e: String) {}
        }
        let router = crate::hostcall::HostCallRouter::new(
            crate::events::EventSink::new(std::sync::Arc::new(Cb(tx))),
            Duration::from_secs(2),
        );
        let reg = ToolRegistry::new();
        let sel = ExecSelection {
            native: true,
            termux: false,
        };
        register_bash_tools(&reg, sel, dir.path().to_path_buf(), router.clone(), &[]);
        assert!(reg.contains("bash"));
        assert!(!reg.contains("termux_bash"));

        // 切到双选：termux_bash 出现，bash 保持 native。
        register_bash_tools(
            &reg,
            ExecSelection {
                native: true,
                termux: true,
            },
            dir.path().to_path_buf(),
            router.clone(),
            &[],
        );
        assert!(reg.contains("bash") && reg.contains("termux_bash"));

        // 切到仅 termux：bash 由 termux 顶替（host 工具壳），旧 shell.run 清掉。
        register_bash_tools(
            &reg,
            ExecSelection {
                native: false,
                termux: true,
            },
            dir.path().to_path_buf(),
            router,
            &[],
        );
        let names = reg.tool_names();
        assert!(names.contains(&"bash".to_owned()), "{names:?}");
        assert!(!names.contains(&"termux_bash".to_owned()), "{names:?}");
        assert!(!names.contains(&"shell.run".to_owned()), "{names:?}");
        // bash 现在是 host 工具壳：调用会发 host call 信封（这里仅验证非
        // NotFound——真正的往返在 runtime_flow 集成测试覆盖）。
        let _ = reg
            .execute("bash", &serde_json::json!({"command": "x"}))
            .await;
    }

    #[tokio::test]
    async fn empty_selection_removes_all_bash_tools() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = std::sync::mpsc::channel::<String>();
        struct Cb(std::sync::mpsc::Sender<String>);
        impl crate::events::EventCallback for Cb {
            fn on_event(&self, _e: String) {}
        }
        let router = crate::hostcall::HostCallRouter::new(
            crate::events::EventSink::new(std::sync::Arc::new(Cb(tx))),
            Duration::from_secs(2),
        );
        let reg = ToolRegistry::new();
        register_bash_tools(
            &reg,
            ExecSelection {
                native: true,
                termux: false,
            },
            dir.path().to_path_buf(),
            router.clone(),
            &[],
        );
        assert!(reg.contains("bash"));
        // 全不选：bash / termux_bash 全部注销，模型不可见。
        register_bash_tools(
            &reg,
            ExecSelection::default(),
            dir.path().to_path_buf(),
            router,
            &[],
        );
        let names = reg.tool_names();
        assert!(!names.contains(&"bash".to_owned()), "{names:?}");
        assert!(!names.contains(&"termux_bash".to_owned()), "{names:?}");
    }

    #[tokio::test]
    async fn native_exec_basics() {
        let dir = tempfile::tempdir().unwrap();
        let t = NativeShellTool::new("bash".to_owned(), dir.path().to_path_buf());

        let out = t
            .execute(&serde_json::json!({"command": "echo hello-shell"}))
            .await
            .unwrap();
        assert_eq!(out.status, ToolOutputStatus::Success);
        assert!(out.content.contains("exit_code: 0"), "{}", out.content);

        let out = t
            .execute(&serde_json::json!({"command": "echo boom 1>&2; exit 3"}))
            .await
            .unwrap();
        assert_eq!(out.status, ToolOutputStatus::Error);
        assert!(out.content.contains("exit_code: 3"), "{}", out.content);
        assert!(out.content.contains("boom"), "{}", out.content);

        let out = t
            .execute(&serde_json::json!({"command": "  "}))
            .await
            .unwrap();
        assert_eq!(out.status, ToolOutputStatus::Error);

        let started = std::time::Instant::now();
        let out = t
            .execute(&serde_json::json!({"command": "sleep 30", "timeout_ms": 500}))
            .await
            .unwrap();
        assert_eq!(out.status, ToolOutputStatus::Error);
        assert!(out.content.contains("TIMEOUT"), "{}", out.content);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn selection_parse_and_normalize() {
        assert_eq!(
            ExecSelection::parse_csv("native,termux"),
            ExecSelection {
                native: true,
                termux: true
            }
        );
        assert_eq!(
            ExecSelection::parse_csv("termux"),
            ExecSelection {
                native: false,
                termux: true
            }
        );
        // 全空 = 全不选（bash 功能整体下线，不自动回退）。
        assert_eq!(
            ExecSelection::parse_csv("bogus"),
            ExecSelection {
                native: false,
                termux: false
            }
        );
    }

    #[test]
    fn sensitive_env_names_are_scrubbed() {
        assert!(is_sensitive_env("OPENSLATE_API_KEY"));
        assert!(is_sensitive_env("GITHUB_TOKEN"));
        assert!(!is_sensitive_env("PATH"));
        assert!(!is_sensitive_env("HOME"));
    }

    #[test]
    fn truncate_keeps_head_and_tail() {
        let text = format!("{}END", "x".repeat(MAX_OUTPUT_BYTES + 1024));
        let truncated = truncate_output(&text);
        assert!(truncated.starts_with("xxxx"));
        assert!(truncated.ends_with("END"));
        assert!(truncated.contains("[truncated"));
    }

    // ── MCP 接管判定（统一谓词）──────────────────────────────────────

    fn mcp_cfg(template: Option<&str>, prefixes: &[&str]) -> openslate_core::config::McpConfig {
        use openslate_core::config::{McpServerConfig, TransportConfig};
        let mut servers = std::collections::HashMap::new();
        for p in prefixes {
            servers.insert(
                (*p).to_owned(),
                McpServerConfig {
                    enabled: true,
                    transport: TransportConfig::Http {
                        url: "http://127.0.0.1:9/mcp".to_owned(),
                        headers: None,
                    },
                },
            );
        }
        openslate_core::config::McpConfig {
            servers,
            tool_name_template: None,
            builtin_tool_name_template: template.map(str::to_owned),
        }
    }

    /// 展开名跟随 builtin 模板（默认 termux_bash；自定义不硬编码）。
    #[test]
    fn mcp_bash_expanded_names_follow_builtin_template() {
        // 默认 / None / 空白：逐字节 `{prefix}_bash`。
        assert_eq!(
            mcp_bash_expanded_names(&mcp_cfg(None, &["termux"])),
            vec!["termux_bash".to_owned()]
        );
        assert_eq!(
            mcp_bash_expanded_names(&mcp_cfg(Some("   "), &["termux"])),
            vec!["termux_bash".to_owned()]
        );
        // 自定义模板：展开名随之变化（证明不硬编码 termux_bash 字面；
        // 合法模板须含 {tool}，取 {prefix}.{tool} → termux.bash）。
        assert_eq!(
            mcp_bash_expanded_names(&mcp_cfg(Some("{prefix}.{tool}"), &["termux"])),
            vec!["termux.bash".to_owned()]
        );
        assert_eq!(
            mcp_bash_expanded_names(&mcp_cfg(Some("{tool}"), &["termux"])),
            vec!["bash".to_owned()]
        );
        // 多 server 键：每键一个候选。
        let mut names = mcp_bash_expanded_names(&mcp_cfg(None, &["b", "a"]));
        names.sort();
        assert_eq!(names, vec!["a_bash".to_owned(), "b_bash".to_owned()]);
    }

    /// 名册含展开名 → 接管；名册含 bash 域名但与当前模板不一致 → 同样
    /// 视为 MCP 所有（防误杀，不 unregister 别人的工具）。
    #[test]
    fn mcp_owned_bash_names_matches_roster() {
        let roster = |names: &[&str]| {
            names
                .iter()
                .map(|n| (*n).to_owned())
                .collect::<Vec<String>>()
        };
        // 规范态：默认模板 + 名册含 termux_bash。
        assert_eq!(
            mcp_owned_bash_names(
                &roster(&["termux_bash", "termux_read_file"]),
                &mcp_cfg(None, &["termux"])
            ),
            vec!["termux_bash".to_owned()]
        );
        // 自定义模板：接管名 = termux.bash（名册同源）。
        assert_eq!(
            mcp_owned_bash_names(
                &roster(&["termux.bash"]),
                &mcp_cfg(Some("{prefix}.{tool}"), &["termux"])
            ),
            vec!["termux.bash".to_owned()]
        );
        // 模板已改、名册还是旧模板注册的：termux_bash 仍归 MCP（防线）。
        assert_eq!(
            mcp_owned_bash_names(
                &roster(&["termux_bash"]),
                &mcp_cfg(Some("{prefix}.{tool}"), &["termux"])
            ),
            vec!["termux_bash".to_owned()]
        );
        // MCP 缺席：空。
        assert!(mcp_owned_bash_names(&[], &mcp_cfg(None, &["termux"])).is_empty());
        assert!(
            mcp_owned_bash_names(&roster(&["termux_memory__x"]), &mcp_cfg(None, &["termux"]))
                .is_empty()
        );
    }

    /// 让位语义：mcp_owned 的名字不清理、不顶替（原 Arc 存活）。
    #[tokio::test]
    async fn register_bash_tools_yields_to_mcp_owned_names() {
        struct Dummy;
        #[async_trait]
        impl Tool for Dummy {
            fn name(&self) -> &str {
                "termux_bash"
            }
            fn description(&self) -> &str {
                "mcp stand-in"
            }
            fn parameters_schema(&self) -> Value {
                serde_json::json!({})
            }
            async fn execute(&self, _args: &Value) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput {
                    bytes: 0,
                    content: String::new(),
                    duration_ms: 0,
                    status: ToolOutputStatus::Success,
                })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = std::sync::mpsc::channel::<String>();
        struct Cb(std::sync::mpsc::Sender<String>);
        impl crate::events::EventCallback for Cb {
            fn on_event(&self, _e: String) {}
        }
        let router = crate::hostcall::HostCallRouter::new(
            crate::events::EventSink::new(std::sync::Arc::new(Cb(tx))),
            Duration::from_secs(2),
        );
        let reg = ToolRegistry::new();
        reg.register_boxed(Box::new(Dummy));
        let mcp_tool = reg.get("termux_bash").expect("MCP stand-in registered");

        // 双选 + MCP 已接管 termux_bash：HostTool 不得顶替/清理该名，
        // native bash 正常注册。
        register_bash_tools(
            &reg,
            ExecSelection {
                native: true,
                termux: true,
            },
            dir.path().to_path_buf(),
            router.clone(),
            &["termux_bash".to_owned()],
        );
        let after = reg.get("termux_bash").expect("MCP 工具仍在场");
        assert!(
            std::sync::Arc::ptr_eq(&mcp_tool, &after),
            "MCP 的 termux_bash 必须原 Arc 存活（不被 HostTool 顶替）"
        );
        assert!(reg.contains("bash"), "native bash 正常注册");

        // 全不选也不清理 MCP 接管名（去留归 reload/接管重算）。
        register_bash_tools(
            &reg,
            ExecSelection::default(),
            dir.path().to_path_buf(),
            router,
            &["termux_bash".to_owned()],
        );
        assert!(reg.contains("termux_bash"), "全不选不清 MCP 接管名");
        assert!(!reg.contains("bash"));
    }
}
