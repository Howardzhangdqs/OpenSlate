//! Error types for OpenSlate.

/// Top-level error type for all OpenSlate operations.
#[derive(Debug, thiserror::Error)]
pub enum OpenSlateError {
    /// Errors originating from configuration parsing or validation.
    #[error("config error: {0}")]
    Config(#[from] ConfigError),

    /// Errors originating from LLM provider interactions.
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),

    /// Errors originating from the agent runtime.
    #[error("runtime error: {0}")]
    Runtime(#[from] RuntimeError),

    /// Errors originating from the persistence store.
    #[error("store error: {0}")]
    Store(#[from] StoreError),

    /// Errors originating from tool execution.
    #[error("tool error: {0}")]
    Tool(#[from] ToolError),

    /// Input or state validation failures.
    #[error("validation error: {0}")]
    Validation(String),

    /// Prompt construction or resolution failures.
    #[error("prompt error: {0}")]
    Prompt(String),

    /// Underlying I/O errors.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Configuration-related errors.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("parse error: {0}")]
    ParseError(String),

    #[error("missing field: {0}")]
    MissingField(String),

    #[error("missing required model: {0}")]
    MissingRequiredModel(String),

    #[error("invalid provider reference: {0}")]
    InvalidProviderRef(String),

    #[error("duplicate agent id: {0}")]
    DuplicateAgentId(String),

    #[error("invalid child reference: {0}")]
    InvalidChildRef(String),

    #[error("missing root agent")]
    MissingRootAgent,

    #[error("file not found: {0}")]
    FileNotFound(String),
}

/// LLM provider-related errors.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("request timed out")]
    Timeout,

    #[error("rate limit exceeded")]
    RateLimit,

    #[error("authentication error: {0}")]
    AuthError(String),

    #[error("server error: status {0}")]
    ServerError(u16),

    #[error("malformed response: {0}")]
    MalformedResponse(String),

    #[error("connection error: {0}")]
    ConnectionError(String),

    #[error("not found: {0}")]
    NotFound(String),
}

/// Agent runtime-related errors.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("max steps exceeded: {max}")]
    MaxStepsExceeded { max: u32 },

    #[error("max depth exceeded: {max}")]
    MaxDepthExceeded { max: u32 },

    #[error("max tool calls exceeded: {max}")]
    MaxToolCallsExceeded { max: u32 },

    #[error("max child agent calls exceeded: {max}")]
    MaxChildAgentCallsExceeded { max: u32 },

    #[error("timeout after {timeout_ms}ms")]
    Timeout { timeout_ms: u64 },

    #[error("operation cancelled")]
    Cancelled,

    #[error("agent not found: {0}")]
    AgentNotFound(String),

    #[error("model alias not found: {0}")]
    ModelAliasNotFound(String),

    /// Model returned a response with no content and no tool calls.
    #[error("empty response at step {step}, agent {agent_id}, model {model_alias}")]
    EmptyResponse {
        step: u32,
        agent_id: String,
        model_alias: String,
    },

    /// Model requested a tool that is not registered in the tool registry.
    #[error("unknown tool '{tool_name}' at step {step}, agent {agent_id}")]
    UnknownTool {
        tool_name: String,
        step: u32,
        agent_id: String,
    },

    /// Model returned tool call arguments that could not be parsed or are invalid.
    #[error("malformed tool arguments for '{tool_name}' at step {step}, agent {agent_id}: {details}")]
    ToolArgumentError {
        tool_name: String,
        step: u32,
        agent_id: String,
        details: String,
    },

    /// Tool execution failed (panic or error).
    #[error("tool '{tool_name}' execution failed at step {step}, agent {agent_id}: {reason}")]
    ToolExecutionError {
        tool_name: String,
        step: u32,
        agent_id: String,
        reason: String,
    },

    /// Too many consecutive empty responses from the model.
    #[error("max empty turns exceeded ({count}) at step {step}, agent {agent_id}, model {model_alias}")]
    MaxEmptyTurnsExceeded {
        count: u32,
        step: u32,
        agent_id: String,
        model_alias: String,
    },

    /// Context exceeded maximum bytes mid-run and was truncated.
    #[error("context exceeded {max_bytes} bytes at step {step}, agent {agent_id}; truncated to continue")]
    ContextOverflow {
        max_bytes: u32,
        step: u32,
        agent_id: String,
    },
}

/// Persistence store-related errors.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("connection error: {0}")]
    ConnectionError(String),

    #[error("migration error: {0}")]
    MigrationError(String),

    #[error("query error: {0}")]
    QueryError(String),

    #[error("write error: {0}")]
    WriteError(String),
}

/// Tool execution-related errors.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("tool not found: {0}")]
    NotFound(String),

    #[error("execution error: {0}")]
    ExecutionError(String),

    #[error("security error: {0}")]
    SecurityError(String),

    #[error("output too large: {actual_bytes} bytes (max {max_bytes})")]
    OutputTooLarge { max_bytes: usize, actual_bytes: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_slate_error_display() {
        let err = OpenSlateError::Validation("bad input".into());
        assert_eq!(format!("{err}"), "validation error: bad input");

        let err = OpenSlateError::Prompt("missing template".into());
        assert_eq!(format!("{err}"), "prompt error: missing template");
    }

    #[test]
    fn open_slate_error_from_config() {
        let inner = ConfigError::MissingField("model".into());
        let err = OpenSlateError::from(inner);
        assert!(matches!(err, OpenSlateError::Config(ConfigError::MissingField(_))));
        assert_eq!(format!("{err}"), "config error: missing field: model");
    }

    #[test]
    fn open_slate_error_from_provider() {
        let inner = ProviderError::AuthError("bad key".into());
        let err = OpenSlateError::from(inner);
        assert!(matches!(err, OpenSlateError::Provider(ProviderError::AuthError(_))));
    }

    #[test]
    fn open_slate_error_from_runtime() {
        let inner = RuntimeError::Cancelled;
        let err = OpenSlateError::from(inner);
        assert!(matches!(err, OpenSlateError::Runtime(RuntimeError::Cancelled)));
    }

    #[test]
    fn open_slate_error_from_store() {
        let inner = StoreError::QueryError("syntax".into());
        let err = OpenSlateError::from(inner);
        assert!(matches!(err, OpenSlateError::Store(StoreError::QueryError(_))));
    }

    #[test]
    fn open_slate_error_from_tool() {
        let inner = ToolError::NotFound("bash".into());
        let err = OpenSlateError::from(inner);
        assert!(matches!(err, OpenSlateError::Tool(ToolError::NotFound(_))));
    }

    #[test]
    fn config_error_variants() {
        assert_eq!(
            format!("{}", ConfigError::ParseError("yaml".into())),
            "parse error: yaml"
        );
        assert_eq!(
            format!("{}", ConfigError::MissingRequiredModel("gpt-4".into())),
            "missing required model: gpt-4"
        );
        assert_eq!(
            format!("{}", ConfigError::InvalidProviderRef("bad".into())),
            "invalid provider reference: bad"
        );
        assert_eq!(
            format!("{}", ConfigError::DuplicateAgentId("a1".into())),
            "duplicate agent id: a1"
        );
        assert_eq!(
            format!("{}", ConfigError::InvalidChildRef("c1".into())),
            "invalid child reference: c1"
        );
        assert_eq!(
            format!("{}", ConfigError::MissingRootAgent),
            "missing root agent"
        );
        assert_eq!(
            format!("{}", ConfigError::FileNotFound("/x.toml".into())),
            "file not found: /x.toml"
        );
    }

    #[test]
    fn provider_error_variants() {
        assert_eq!(format!("{}", ProviderError::Timeout), "request timed out");
        assert_eq!(format!("{}", ProviderError::RateLimit), "rate limit exceeded");
        assert_eq!(
            format!("{}", ProviderError::ServerError(503)),
            "server error: status 503"
        );
        assert_eq!(
            format!("{}", ProviderError::MalformedResponse("bad json".into())),
            "malformed response: bad json"
        );
        assert_eq!(
            format!("{}", ProviderError::ConnectionError("refused".into())),
            "connection error: refused"
        );
        assert_eq!(
            format!("{}", ProviderError::NotFound("model-x".into())),
            "not found: model-x"
        );
    }

    #[test]
    fn runtime_error_variants() {
        assert_eq!(
            format!("{}", RuntimeError::MaxStepsExceeded { max: 100 }),
            "max steps exceeded: 100"
        );
        assert_eq!(
            format!("{}", RuntimeError::MaxDepthExceeded { max: 10 }),
            "max depth exceeded: 10"
        );
        assert_eq!(
            format!("{}", RuntimeError::MaxToolCallsExceeded { max: 50 }),
            "max tool calls exceeded: 50"
        );
        assert_eq!(
            format!("{}", RuntimeError::MaxChildAgentCallsExceeded { max: 8 }),
            "max child agent calls exceeded: 8"
        );
        assert_eq!(
            format!("{}", RuntimeError::Timeout { timeout_ms: 5000 }),
            "timeout after 5000ms"
        );
        assert_eq!(format!("{}", RuntimeError::Cancelled), "operation cancelled");
        assert_eq!(
            format!("{}", RuntimeError::AgentNotFound("agent-1".into())),
            "agent not found: agent-1"
        );
        assert_eq!(
            format!("{}", RuntimeError::ModelAliasNotFound("gpt-4".into())),
            "model alias not found: gpt-4"
        );
        assert_eq!(
            format!(
                "{}",
                RuntimeError::EmptyResponse {
                    step: 3,
                    agent_id: "agent-1".into(),
                    model_alias: "gpt-4".into(),
                }
            ),
            "empty response at step 3, agent agent-1, model gpt-4"
        );
        assert_eq!(
            format!(
                "{}",
                RuntimeError::UnknownTool {
                    tool_name: "bad_tool".into(),
                    step: 5,
                    agent_id: "agent-2".into(),
                }
            ),
            "unknown tool 'bad_tool' at step 5, agent agent-2"
        );
        assert_eq!(
            format!(
                "{}",
                RuntimeError::ToolArgumentError {
                    tool_name: "bash".into(),
                    step: 2,
                    agent_id: "agent-1".into(),
                    details: "expected object".into(),
                }
            ),
            "malformed tool arguments for 'bash' at step 2, agent agent-1: expected object"
        );
        assert_eq!(
            format!(
                "{}",
                RuntimeError::ToolExecutionError {
                    tool_name: "bash".into(),
                    step: 7,
                    agent_id: "agent-1".into(),
                    reason: "panic".into(),
                }
            ),
            "tool 'bash' execution failed at step 7, agent agent-1: panic"
        );
        assert_eq!(
            format!(
                "{}",
                RuntimeError::MaxEmptyTurnsExceeded {
                    count: 3,
                    step: 4,
                    agent_id: "agent-1".into(),
                    model_alias: "gpt-4".into(),
                }
            ),
            "max empty turns exceeded (3) at step 4, agent agent-1, model gpt-4"
        );
        assert_eq!(
            format!(
                "{}",
                RuntimeError::ContextOverflow {
                    max_bytes: 64000,
                    step: 6,
                    agent_id: "agent-1".into(),
                }
            ),
            "context exceeded 64000 bytes at step 6, agent agent-1; truncated to continue"
        );
    }

    #[test]
    fn store_error_variants() {
        assert_eq!(
            format!("{}", StoreError::ConnectionError("timeout".into())),
            "connection error: timeout"
        );
        assert_eq!(
            format!("{}", StoreError::MigrationError("v2".into())),
            "migration error: v2"
        );
        assert_eq!(
            format!("{}", StoreError::QueryError("syntax".into())),
            "query error: syntax"
        );
        assert_eq!(
            format!("{}", StoreError::WriteError("disk full".into())),
            "write error: disk full"
        );
    }

    #[test]
    fn tool_error_variants() {
        assert_eq!(
            format!("{}", ToolError::NotFound("bash".into())),
            "tool not found: bash"
        );
        assert_eq!(
            format!("{}", ToolError::ExecutionError("segfault".into())),
            "execution error: segfault"
        );
        assert_eq!(
            format!("{}", ToolError::SecurityError("sandbox escape".into())),
            "security error: sandbox escape"
        );
        assert_eq!(
            format!(
                "{}",
                ToolError::OutputTooLarge {
                    max_bytes: 1024,
                    actual_bytes: 2048
                }
            ),
            "output too large: 2048 bytes (max 1024)"
        );
    }
}
