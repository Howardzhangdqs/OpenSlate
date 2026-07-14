//! Configuration parsing for OpenSlate.
//!
//! Supports TOML for main config (`openslate.toml`) and Markdown with YAML
//! frontmatter for agent definitions (`agents/*.md`). All structs implement
//! `serde::Deserialize`.
//!
//! # Schema Overview
//!
//! ## `openslate.toml` (TOML)
//!
//! | Section     | Type                | Required | Description                        |
//! |-------------|---------------------|----------|------------------------------------|
//! | `project`   | `ProjectConfig`     | no       | Project metadata                   |
//! | `database`  | `DatabaseConfig`    | no       | SQLite database settings           |
//! | `prompts`   | `PromptsConfig`     | no       | Prompt template paths              |
//! | `limits`    | `LimitsConfig`      | no       | Execution limit defaults           |
//! | `providers` | `Map<String, ProviderConfig>` | yes | LLM provider endpoints |
//! | `models`    | `Map<String, ModelConfig>`    | yes | Named model aliases (`main`, `fast` required) |
//! | `trace`     | `TraceConfig`       | no       | Observability settings             |
//!
//! ## `agents/*.md` (Markdown + YAML frontmatter)
//!
//! Each `.md` file defines one [`AgentConfig`](crate::types::AgentConfig).
//! The YAML frontmatter requires: `name`, `model`. Optional: `id` (defaults
//! to filename), `children` (list of agent ids), `tools` (list of tool names).
//! The body after the closing `---` becomes `default_prompt`.
//!
//! # Validation
//!
//! Use [`validation::validate_config`] for error-only checks,
//! [`validation::validate_strict`] for errors + warnings, or
//! [`validation::validate_config_full`] for a structured result.

pub mod validation;

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::error::ConfigError;
use crate::types::{AgentConfig, AgentId};

// ── Config structs ───────────────────────────────────────────────────────────

/// Top-level configuration parsed from `openslate.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenSlateConfig {
    #[serde(default)]
    pub project: Option<ProjectConfig>,
    #[serde(default)]
    pub database: Option<DatabaseConfig>,
    #[serde(default)]
    pub prompts: Option<PromptsConfig>,
    #[serde(default)]
    pub limits: Option<LimitsConfig>,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,
    #[serde(default)]
    pub trace: Option<TraceConfig>,
    /// MCP (Model Context Protocol) client servers.
    #[serde(default)]
    pub mcp: Option<McpConfig>,
}

/// Project metadata.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub name: Option<String>,
}

/// Database connection settings.
#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default = "default_true")]
    pub wal: bool,
    #[serde(default = "default_busy_timeout")]
    pub busy_timeout_ms: u64,
}

/// Prompt template settings.
#[derive(Debug, Clone, Deserialize)]
pub struct PromptsConfig {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default = "default_profile")]
    pub default_profile: String,
    #[serde(default)]
    pub hot_reload: bool,
}

/// Execution limit defaults.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    pub max_steps: u32,
    pub max_depth: u32,
    pub max_tool_calls: u32,
    pub max_child_agent_calls: u32,
    pub timeout_ms: u64,
    pub max_context_messages: u32,
    pub max_context_bytes: u32,
    pub max_output_bytes: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_steps: 0,
            max_depth: 4,
            max_tool_calls: 20,
            max_child_agent_calls: 8,
            timeout_ms: 60_000,
            max_context_messages: 16,
            max_context_bytes: 64_000,
            max_output_bytes: 65_536,
        }
    }
}

/// A single LLM provider endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    /// Which OpenSlate adapter crate handles this provider:
    /// `"openai_compatible"` (hand-rolled OpenAI Chat Completions client) or
    /// `"genai"` (genai-backed multi-provider adapter, behind the `genai` cargo
    /// feature).
    pub kind: String,
    pub base_url: String,
    pub api_key_env: String,
    /// For `kind = "genai"`: the genai adapter protocol
    /// (e.g. `"anthropic"`, `"gemini"`, `"openai"`, `"ollama"`). When omitted,
    /// genai infers the protocol from the model name (with a warning, since
    /// unknown prefixes fall through to Ollama). Ignored for `kind = "openai_compatible"`.
    #[serde(default)]
    pub adapter: Option<String>,
}

/// A named model alias referencing a provider.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub max_context_tokens: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default = "default_true")]
    pub supports_tool_call: bool,
    #[serde(default)]
    pub supports_vision: bool,
    #[serde(default)]
    pub supports_reasoning: bool,
}

/// Tracing / observability settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TraceConfig {
    pub enabled: bool,
    pub store_sqlite: bool,
    pub default_export_format: String,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            store_sqlite: true,
            default_export_format: "chrome-json".to_owned(),
        }
    }
}

/// MCP (Model Context Protocol) client configuration.
///
/// Declares external MCP servers whose tools are registered into the
/// `ToolRegistry` at startup. See [`TransportConfig`] for connection options.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: HashMap<String, McpServerConfig>,
}

/// A single MCP server connection.
///
/// `enabled` defaults to `true`. Each tool this server exposes is automatically
/// namespaced as `{server_name}_{tool_name}` when registered, so collisions
/// across servers (and with builtins) are avoided without any per-server config.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(flatten)]
    pub transport: TransportConfig,
}

/// How to reach an MCP server.
///
/// Internally-tagged by the `transport` field. This enum intentionally does NOT
/// use `#[serde(deny_unknown_fields)]` — that is incompatible with serde's
/// internally-tagged enum representation.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "transport")]
pub enum TransportConfig {
    /// Spawn a local subprocess speaking MCP over stdio.
    #[serde(rename = "stdio")]
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Option<HashMap<String, String>>,
    },
    /// Connect to a remote MCP server via Streamable HTTP.
    #[serde(rename = "http")]
    Http {
        url: String,
    },
}

/// Wrapper for the agents YAML file.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentsConfig {
    pub agents: Vec<AgentConfig>,
}

// ── Default helpers ──────────────────────────────────────────────────────────

fn default_true() -> bool {
    true
}

fn default_busy_timeout() -> u64 {
    5000
}

fn default_profile() -> String {
    "default".to_owned()
}

// ── Parsing functions ────────────────────────────────────────────────────────

/// Parse the main `openslate.toml` content into an `OpenSlateConfig`.
pub fn parse_openslate_toml(content: &str) -> Result<OpenSlateConfig, ConfigError> {
    toml::from_str(content).map_err(|e| ConfigError::ParseError(e.to_string()))
}

// ── Markdown frontmatter parsing ─────────────────────────────────────────────

/// Frontmatter fields deserialized from the YAML header of an agent `.md` file.
///
/// Unlike [`AgentConfig`], the `id` is optional (falls back to the filename)
/// and `default_prompt` is absent (it comes from the markdown body).
#[derive(Debug, Clone, Deserialize)]
pub struct AgentFrontmatter {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    pub model: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub children: Vec<String>,
}

/// Derive an agent id from a markdown filename.
///
/// Strips the `.md` extension, keeps only `[a-zA-Z0-9_]`, replaces other
/// characters with `_`, and converts to lowercase.
pub fn derive_id_from_filename(filename: &str) -> String {
    let stem = filename.strip_suffix(".md").unwrap_or(filename);
    stem.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Parse a single agent markdown file (with YAML frontmatter) into an [`AgentConfig`].
///
/// Expected format:
/// ```markdown
/// ---
/// name: My Agent
/// model: main
/// tools: [bash, read_file]
/// ---
/// You are a helpful assistant.
/// ```
///
/// - The `id` field is optional in the frontmatter; if absent it is derived
///   from `filename` via [`derive_id_from_filename`].
/// - The body after the closing `---` becomes `default_prompt`.
/// - UTF-8 BOM (`\u{feff}`) at the start is stripped before parsing.
pub fn parse_agent_markdown(
    content: &str,
    filename: &str,
) -> Result<AgentConfig, ConfigError> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);

    let content = content
        .strip_prefix("---")
        .ok_or_else(|| ConfigError::ParseError(format!("{filename}: no frontmatter delimiter")))?;

    let (yaml_str, body) = match content.find("\n---") {
        Some(pos) => {
            let yaml = &content[..pos];
            let body = &content[pos + "\n---".len()..];
            (yaml, body)
        }
        None => {
            return Err(ConfigError::ParseError(format!(
                "{filename}: unclosed frontmatter (missing closing ---)"
            )));
        }
    };

    let fm: AgentFrontmatter = serde_yml::from_str(yaml_str).map_err(|e| {
        ConfigError::ParseError(format!("{filename}: invalid frontmatter YAML: {e}"))
    })?;

    let id_string = fm
        .id
        .unwrap_or_else(|| derive_id_from_filename(filename));
    let id = AgentId(id_string);
    let children = fm.children.into_iter().map(AgentId).collect();
    let default_prompt = body.trim().to_owned();

    Ok(AgentConfig {
        id,
        name: fm.name,
        model: fm.model,
        children,
        tools: fm.tools,
        default_prompt,
    })
}

/// Parse all `.md` files in a directory into an [`AgentsConfig`].
///
/// Files are filtered by the `.md` extension, parsed individually via
/// [`parse_agent_markdown`], and the resulting agents are sorted by `id`
/// alphabetically for deterministic output.
pub fn parse_agents_dir(dir: &Path) -> Result<AgentsConfig, ConfigError> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        ConfigError::FileNotFound(format!("{}: {e}", dir.display()))
    })?;

    let mut agents = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            ConfigError::ParseError(format!("{}: read_dir entry: {e}", dir.display()))
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown.md");
            let content = std::fs::read_to_string(&path).map_err(|e| {
                ConfigError::FileNotFound(format!("{}: {e}", path.display()))
            })?;
            agents.push(parse_agent_markdown(&content, filename)?);
        }
    }

    agents.sort_by(|a, b| a.id.0.cmp(&b.id.0));

    Ok(AgentsConfig { agents })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Default value tests ──────────────────────────────────────────────

    #[test]
    fn default_limits_config() {
        let limits = LimitsConfig::default();
        assert_eq!(limits.max_steps, 0); // 0 = unlimited
        assert_eq!(limits.max_depth, 4);
        assert_eq!(limits.max_tool_calls, 20);
        assert_eq!(limits.max_child_agent_calls, 8);
        assert_eq!(limits.timeout_ms, 60_000);
        assert_eq!(limits.max_context_messages, 16);
        assert_eq!(limits.max_context_bytes, 64_000);
        assert_eq!(limits.max_output_bytes, 65_536);
    }

    #[test]
    fn default_trace_config() {
        let trace = TraceConfig::default();
        assert!(trace.enabled);
        assert!(trace.store_sqlite);
        assert_eq!(trace.default_export_format, "chrome-json");
    }

    // ── TOML parsing tests ───────────────────────────────────────────────

    #[test]
    fn parse_minimal_toml() {
        let toml = "";
        let config = parse_openslate_toml(toml).expect("empty toml should parse");
        assert!(config.project.is_none());
        assert!(config.database.is_none());
        assert!(config.models.is_empty());
        assert!(config.providers.is_empty());
    }

    #[test]
    fn parse_missing_models_section_empty_hashmap() {
        let toml = r#"
[project]
name = "test"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert!(config.models.is_empty());
        assert!(config.providers.is_empty());
    }

    #[test]
    fn parse_invalid_toml_syntax() {
        let toml = "this is [ not valid { toml";
        let result = parse_openslate_toml(toml);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ConfigError::ParseError(_)),
            "expected ParseError, got {err:?}"
        );
    }

    #[test]
    fn parse_provider_config() {
        let toml = r#"
[providers.zhipu]
kind = "openai_compatible"
base_url = "https://open.bigmodel.cn/api/paas/v4"
api_key_env = "ZHIPU_API_KEY"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let zhipu = config.providers.get("zhipu").expect("provider zhipu");
        assert_eq!(zhipu.kind, "openai_compatible");
        assert_eq!(zhipu.base_url, "https://open.bigmodel.cn/api/paas/v4");
        assert_eq!(zhipu.api_key_env, "ZHIPU_API_KEY");
    }

    #[test]
    fn parse_model_config_with_optional_fields() {
        let toml = r#"
[models.main]
provider = "zhipu"
model = "glm-5.1"
max_context_tokens = 200000
max_output_tokens = 131072
supports_tool_call = true
supports_vision = false
supports_reasoning = true
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let main = config.models.get("main").expect("model main");
        assert_eq!(main.provider, "zhipu");
        assert_eq!(main.model, "glm-5.1");
        assert_eq!(main.max_context_tokens, Some(200_000));
        assert_eq!(main.max_output_tokens, Some(131_072));
        assert!(main.supports_tool_call);
        assert!(!main.supports_vision);
        assert!(main.supports_reasoning);
    }

    #[test]
    fn parse_model_config_optional_fields_missing() {
        let toml = r#"
[models.bare]
provider = "p"
model = "m"
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let bare = config.models.get("bare").expect("model bare");
        assert_eq!(bare.provider, "p");
        assert_eq!(bare.model, "m");
        assert_eq!(bare.max_context_tokens, None);
        assert_eq!(bare.max_output_tokens, None);
        assert!(bare.supports_tool_call);
        assert!(!bare.supports_vision);
        assert!(!bare.supports_reasoning);
    }

    #[test]
    fn parse_database_config_defaults() {
        let toml = r#"
[database]
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let db = config.database.as_ref().expect("database section");
        assert!(db.path.is_none());
        assert!(db.wal);
        assert_eq!(db.busy_timeout_ms, 5000);
    }

    #[test]
    fn parse_trace_config_defaults() {
        let toml = r#"
[trace]
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        let trace = config.trace.as_ref().expect("trace section");
        assert!(trace.enabled);
        assert!(trace.store_sqlite);
        assert_eq!(trace.default_export_format, "chrome-json");
    }

    // ── Full example TOML ────────────────────────────────────────────────

    #[test]
    fn parse_full_example_toml() {
        let toml_content = include_str!("../../fixtures/openslate.toml");
        let config = parse_openslate_toml(toml_content).expect("example toml should parse");

        let project = config.project.as_ref().expect("project");
        assert_eq!(project.name.as_deref(), Some("example-openslate-project"));

        let db = config.database.as_ref().expect("database");
        assert_eq!(db.path, None);
        assert!(db.wal);
        assert_eq!(db.busy_timeout_ms, 5000);

        let prompts = config.prompts.as_ref().expect("prompts");
        assert_eq!(prompts.path.as_deref(), Some("./prompts"));
        assert_eq!(prompts.default_profile, "default");
        assert!(prompts.hot_reload);

        let limits = config.limits.as_ref().expect("limits");
        assert_eq!(limits.max_steps, 12);
        assert_eq!(limits.max_depth, 4);

        assert_eq!(config.providers.len(), 2);
        let zhipu = config.providers.get("zhipu").expect("zhipu provider");
        assert_eq!(zhipu.kind, "openai_compatible");
        let minimax = config.providers.get("minimax").expect("minimax provider");
        assert_eq!(minimax.kind, "openai_compatible");

        assert_eq!(config.models.len(), 4);
        let main = config.models.get("main").expect("main model");
        assert_eq!(main.provider, "zhipu");
        assert_eq!(main.model, "glm-5.1");
        assert_eq!(main.max_context_tokens, Some(200_000));
        assert!(main.supports_tool_call);
        assert!(main.supports_reasoning);

        let fast = config.models.get("fast").expect("fast model");
        assert_eq!(fast.provider, "minimax");
        assert!(!fast.supports_reasoning);

        let vision = config.models.get("vision").expect("vision model");
        assert!(vision.supports_vision);

        let trace = config.trace.as_ref().expect("trace");
        assert!(trace.enabled);
        assert!(trace.store_sqlite);
        assert_eq!(trace.default_export_format, "chrome-json");
    }

    // ── Markdown agent parsing tests ─────────────────────────────────────

    #[test]
    fn md_parse_invalid_frontmatter() {
        let md = "---\nname: [broken\nmodel: main\n---\nbody\n";
        let result = parse_agent_markdown(md, "broken.md");
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), ConfigError::ParseError(_)),
        );
    }

    #[test]
    fn md_parse_single_agent() {
        let md = "---\nid: root\nname: Root Agent\nmodel: main\n---\nYou are the root agent.\n";
        let agent = parse_agent_markdown(md, "root.md").expect("should parse");
        assert_eq!(agent.id.0, "root");
        assert_eq!(agent.name, "Root Agent");
        assert_eq!(agent.model, "main");
        assert!(agent.children.is_empty());
        assert!(agent.tools.is_empty());
        assert_eq!(agent.default_prompt, "You are the root agent.");
    }

    #[test]
    fn md_parse_children_and_tools() {
        let md = "---\nid: root\nname: Root Agent\nmodel: main\nchildren:\n  - researcher\n  - writer\ntools:\n  - current_time\n  - read_file\n---\nYou are the root coordinator agent.\n";
        let agent = parse_agent_markdown(md, "root.md").expect("should parse");
        assert_eq!(agent.children.len(), 2);
        assert_eq!(agent.children[0].0, "researcher");
        assert_eq!(agent.children[1].0, "writer");
        assert_eq!(agent.tools.len(), 2);
        assert_eq!(agent.tools[0], "current_time");
        assert_eq!(agent.tools[1], "read_file");
    }

    #[test]
    fn parse_full_example_agents_dir() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/agents");
        let config =
            parse_agents_dir(Path::new(dir))
                .expect("example agents dir should parse");

        assert_eq!(config.agents.len(), 6);

        let root = config
            .agents
            .iter()
            .find(|a| a.id.0 == "root")
            .expect("root agent");
        assert_eq!(root.name, "Root Agent");
        assert_eq!(root.model, "main");
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].0, "researcher");
        assert_eq!(root.children[1].0, "writer");
        assert_eq!(root.tools.len(), 3);

        let researcher = config
            .agents
            .iter()
            .find(|a| a.id.0 == "researcher")
            .expect("researcher agent");
        assert_eq!(researcher.model, "fast");
        assert_eq!(researcher.children.len(), 1);
        assert_eq!(researcher.children[0].0, "verifier");

        let verifier = config
            .agents
            .iter()
            .find(|a| a.id.0 == "verifier")
            .expect("verifier agent");
        assert!(verifier.children.is_empty());

        let writer = config
            .agents
            .iter()
            .find(|a| a.id.0 == "writer")
            .expect("writer agent");
        assert!(writer.tools.is_empty());

        let analyst = config
            .agents
            .iter()
            .find(|a| a.id.0 == "deep-analyst")
            .expect("deep-analyst agent");
        assert_eq!(analyst.model, "deep-reasoner");

        let inspector = config
            .agents
            .iter()
            .find(|a| a.id.0 == "visual-inspector")
            .expect("visual-inspector agent");
        assert_eq!(inspector.model, "vision");
    }

    // ── Multiple providers and models ────────────────────────────────────

    #[test]
    fn parse_multiple_providers_and_models() {
        let toml = r#"
[providers.openai]
kind = "openai_compatible"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"

[models.gpt4]
provider = "openai"
model = "gpt-4-turbo"
max_context_tokens = 128000

[models.claude]
provider = "anthropic"
model = "claude-3-opus"
supports_tool_call = true
supports_vision = true
"#;
        let config = parse_openslate_toml(toml).expect("should parse");
        assert_eq!(config.providers.len(), 2);
        assert_eq!(config.models.len(), 2);

        let gpt4 = config.models.get("gpt4").expect("gpt4");
        assert_eq!(gpt4.max_context_tokens, Some(128_000));
        assert_eq!(gpt4.max_output_tokens, None);

        let claude = config.models.get("claude").expect("claude");
        assert!(claude.supports_tool_call);
        assert!(claude.supports_vision);
    }

    // ── Markdown frontmatter parsing tests ───────────────────────────────

    #[test]
    fn md_parse_normal_frontmatter() {
        let md = "---\nname: Root Agent\nmodel: main\ntools:\n  - bash\n  - read_file\n---\nYou are the root agent.\n";
        let agent = parse_agent_markdown(md, "root.md").expect("should parse");
        assert_eq!(agent.id.0, "root");
        assert_eq!(agent.name, "Root Agent");
        assert_eq!(agent.model, "main");
        assert_eq!(agent.tools, vec!["bash", "read_file"]);
        assert!(agent.children.is_empty());
        assert_eq!(agent.default_prompt, "You are the root agent.");
    }

    #[test]
    fn md_parse_no_id_falls_back_to_filename() {
        let md = "---\nname: Writer\nmodel: fast\n---\nWrite stuff.\n";
        let agent = parse_agent_markdown(md, "my-writer.md").expect("should parse");
        assert_eq!(agent.id.0, "my_writer");
        assert_eq!(agent.name, "Writer");
        assert_eq!(agent.model, "fast");
    }

    #[test]
    fn md_parse_empty_body() {
        let md = "---\nname: Empty\nmodel: main\n---\n";
        let agent = parse_agent_markdown(md, "empty.md").expect("should parse");
        assert_eq!(agent.id.0, "empty");
        assert_eq!(agent.default_prompt, "");
    }

    #[test]
    fn md_parse_no_frontmatter_delimiter_error() {
        let md = "Just some plain text without frontmatter.\n";
        let result = parse_agent_markdown(md, "plain.md");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ConfigError::ParseError(_)));
        assert!(err.to_string().contains("no frontmatter delimiter"));
    }

    #[test]
    fn md_parse_bad_yaml_error() {
        let md = "---\nname: [broken\nmodel: main\n---\nbody\n";
        let result = parse_agent_markdown(md, "bad.md");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ConfigError::ParseError(_)));
        assert!(err.to_string().contains("bad.md"));
    }

    #[test]
    fn md_parse_thematic_break_in_body() {
        let md = "---\nname: Agent\nmodel: main\n---\nSome prompt text.\n\n---\n\nMore text after thematic break.\n";
        let agent = parse_agent_markdown(md, "thematic.md").expect("should parse");
        assert_eq!(agent.id.0, "thematic");
        assert!(agent.default_prompt.contains("---"));
        assert!(agent.default_prompt.contains("More text after thematic break."));
    }

    #[test]
    fn md_parse_crlf_line_endings() {
        let md = "---\r\nname: CRLF Agent\r\nmodel: main\r\n---\r\nHello from Windows.\r\n";
        let agent = parse_agent_markdown(md, "crlf.md").expect("should parse");
        assert_eq!(agent.id.0, "crlf");
        assert_eq!(agent.name, "CRLF Agent");
    }

    #[test]
    fn md_parse_bom_prefix() {
        let bom = "\u{feff}";
        let md = format!("{bom}---\nname: BOM Agent\nmodel: main\n---\nBOM content.\n");
        let agent = parse_agent_markdown(&md, "bom.md").expect("should parse");
        assert_eq!(agent.id.0, "bom");
        assert_eq!(agent.name, "BOM Agent");
    }

    #[test]
    fn md_parse_explicit_id_overrides_filename() {
        let md = "---\nid: custom-id\nname: Custom\nmodel: fast\n---\nCustom prompt.\n";
        let agent = parse_agent_markdown(md, "some-file.md").expect("should parse");
        assert_eq!(agent.id.0, "custom-id");
    }

    #[test]
    fn md_parse_children_converted_to_agent_ids() {
        let md = "---\nname: Parent\nmodel: main\nchildren:\n  - researcher\n  - writer\n---\nPrompt.\n";
        let agent = parse_agent_markdown(md, "parent.md").expect("should parse");
        assert_eq!(agent.children.len(), 2);
        assert_eq!(agent.children[0].0, "researcher");
        assert_eq!(agent.children[1].0, "writer");
    }

    #[test]
    fn derive_id_basic() {
        assert_eq!(derive_id_from_filename("root.md"), "root");
    }

    #[test]
    fn derive_id_special_chars() {
        assert_eq!(derive_id_from_filename("my-cool agent.md"), "my_cool_agent");
    }

    #[test]
    fn derive_id_no_md_extension() {
        assert_eq!(derive_id_from_filename("agent"), "agent");
    }

    #[test]
    fn derive_id_uppercase() {
        assert_eq!(derive_id_from_filename("MyAgent.md"), "myagent");
    }

    #[test]
    fn parse_agents_dir_nonexistent() {
        let result = parse_agents_dir(Path::new("/nonexistent/path/xyz"));
        assert!(result.is_err());
    }
}
