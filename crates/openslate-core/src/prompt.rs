//! Prompt resolution with profile system.
//!
//! Directory structure:
//!   prompts/default/prompt.md     → root prompt for default profile
//!   prompts/{profile}/prompt.md   → root prompt for named profile
//!   prompts/{profile}/agents/{agent_id}.md → agent-specific prompt override
//!
//! Fallback chain:
//!   Root prompt: profile → default → built-in
//!   Agent prompt: profile agents/ → default agents/ → agent config default_prompt → built-in

use std::collections::HashMap;
use std::path::PathBuf;

/// Where a prompt was loaded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptSource {
    /// Loaded from the specified profile directory.
    ProfileFile { path: PathBuf },
    /// Loaded from the default profile directory.
    DefaultFile { path: PathBuf },
    /// Fell back to agent config's default_prompt.
    AgentDefault,
    /// Hardcoded built-in fallback.
    Builtin,
}

/// A resolved prompt with its source tracked.
#[derive(Debug, Clone)]
pub struct ResolvedPrompt {
    pub content: String,
    pub source: PromptSource,
    pub profile_name: String,
    pub agent_id: Option<String>,
}

/// Errors during prompt resolution.
#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    #[error("Prompt directory not found: {0}")]
    DirectoryNotFound(String),
    #[error("Unknown template variable: {0}")]
    UnknownVariable(String),
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

const BUILTIN_PROMPT: &str = "You are a helpful AI assistant.";

/// Read a file to string, returning `None` if the file doesn't exist
/// or cannot be read.
fn read_file_optional(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Resolve the root prompt for a given profile.
///
/// Looks for:
/// 1. `prompts/{profile}/prompt.md`
/// 2. `prompts/default/prompt.md`
/// 3. Built-in fallback: "You are a helpful AI assistant."
pub fn resolve_root_prompt(
    prompts_dir: &std::path::Path,
    profile: &str,
) -> ResolvedPrompt {
    // 1. Try profile-specific file (for non-default profiles)
    if profile != "default" {
        let profile_path = prompts_dir.join(profile).join("prompt.md");
        if let Some(content) = read_file_optional(&profile_path) {
            return ResolvedPrompt {
                content,
                source: PromptSource::ProfileFile { path: profile_path },
                profile_name: profile.to_owned(),
                agent_id: None,
            };
        }
    }

    // 2. Try default file
    let default_path = prompts_dir.join("default").join("prompt.md");
    if let Some(content) = read_file_optional(&default_path) {
        return ResolvedPrompt {
            content,
            source: PromptSource::DefaultFile { path: default_path },
            profile_name: profile.to_owned(),
            agent_id: None,
        };
    }

    // 3. Built-in fallback
    ResolvedPrompt {
        content: BUILTIN_PROMPT.to_owned(),
        source: PromptSource::Builtin,
        profile_name: profile.to_owned(),
        agent_id: None,
    }
}

/// Resolve the agent-specific prompt.
///
/// Looks for:
/// 1. `prompts/{profile}/agents/{agent_id}.md`
/// 2. `prompts/default/agents/{agent_id}.md`
/// 3. The agent config's `default_prompt` field
/// 4. Built-in fallback
pub fn resolve_agent_prompt(
    prompts_dir: &std::path::Path,
    profile: &str,
    agent_id: &str,
    agent_default_prompt: Option<&str>,
) -> ResolvedPrompt {
    // 1. Try profile-specific agent file (for non-default profiles)
    if profile != "default" {
        let profile_agent_path = prompts_dir
            .join(profile)
            .join("agents")
            .join(format!("{agent_id}.md"));
        if let Some(content) = read_file_optional(&profile_agent_path) {
            return ResolvedPrompt {
                content,
                source: PromptSource::ProfileFile {
                    path: profile_agent_path,
                },
                profile_name: profile.to_owned(),
                agent_id: Some(agent_id.to_owned()),
            };
        }
    }

    // 2. Try default agent file
    let default_agent_path = prompts_dir
        .join("default")
        .join("agents")
        .join(format!("{agent_id}.md"));
    if let Some(content) = read_file_optional(&default_agent_path) {
        return ResolvedPrompt {
            content,
            source: PromptSource::DefaultFile {
                path: default_agent_path,
            },
            profile_name: profile.to_owned(),
            agent_id: Some(agent_id.to_owned()),
        };
    }

    // 3. Agent config's default_prompt
    if let Some(default_prompt) = agent_default_prompt {
        if !default_prompt.is_empty() {
            return ResolvedPrompt {
                content: default_prompt.to_owned(),
                source: PromptSource::AgentDefault,
                profile_name: profile.to_owned(),
                agent_id: Some(agent_id.to_owned()),
            };
        }
    }

    // 4. Built-in fallback
    ResolvedPrompt {
        content: BUILTIN_PROMPT.to_owned(),
        source: PromptSource::Builtin,
        profile_name: profile.to_owned(),
        agent_id: Some(agent_id.to_owned()),
    }
}

/// Apply template variable substitution.
///
/// Supported variables:
/// - `{{ agent.name }}` — Agent display name
/// - `{{ agent.id }}` — Agent identifier
/// - `{{ runtime.date }}` — Current date (YYYY-MM-DD)
/// - `{{ runtime.time }}` — Current time (HH:MM:SS)
/// - `{{ runtime.cwd }}` — Current working directory
///
/// In strict mode, unknown variables return an error.
/// In non-strict mode, unknown variables are left as-is.
pub fn apply_template(
    content: &str,
    variables: &HashMap<String, String>,
    strict: bool,
) -> Result<String, PromptError> {
    let mut result = String::with_capacity(content.len());
    let mut remaining = content;

    while let Some(start) = remaining.find("{{") {
        // Push everything before the {{
        result.push_str(&remaining[..start]);
        remaining = &remaining[start + 2..];

        if let Some(end) = remaining.find("}}") {
            let key = remaining[..end].trim();
            remaining = &remaining[end + 2..];

            if let Some(value) = variables.get(key) {
                result.push_str(value);
            } else if strict {
                return Err(PromptError::UnknownVariable(key.to_owned()));
            } else {
                // Leave as-is in non-strict mode
                result.push_str("{{ ");
                result.push_str(key);
                result.push_str(" }}");
            }
        } else {
            // No closing }}, treat as literal
            result.push_str("{{");
            result.push_str(remaining);
            remaining = "";
        }
    }

    result.push_str(remaining);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // Helper to create a file with parent directories.
    fn write_file(path: &std::path::Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    // --- Root prompt tests ---

    #[test]
    fn test_default_profile_loads() {
        let dir = TempDir::new().unwrap();
        let prompts = dir.path().join("prompts");
        write_file(&prompts.join("default").join("prompt.md"), "Default prompt");

        let result = resolve_root_prompt(&prompts, "default");
        assert_eq!(result.content, "Default prompt");
        assert_eq!(
            result.source,
            PromptSource::DefaultFile {
                path: prompts.join("default").join("prompt.md")
            }
        );
        assert_eq!(result.profile_name, "default");
        assert!(result.agent_id.is_none());
    }

    #[test]
    fn test_named_profile_loads() {
        let dir = TempDir::new().unwrap();
        let prompts = dir.path().join("prompts");
        write_file(&prompts.join("coding").join("prompt.md"), "Coding prompt");

        let result = resolve_root_prompt(&prompts, "coding");
        assert_eq!(result.content, "Coding prompt");
        assert_eq!(
            result.source,
            PromptSource::ProfileFile {
                path: prompts.join("coding").join("prompt.md")
            }
        );
        assert_eq!(result.profile_name, "coding");
    }

    #[test]
    fn test_profile_falls_back_to_default() {
        let dir = TempDir::new().unwrap();
        let prompts = dir.path().join("prompts");
        // Only default exists, no "coding" profile
        write_file(&prompts.join("default").join("prompt.md"), "Fallback default");

        let result = resolve_root_prompt(&prompts, "coding");
        assert_eq!(result.content, "Fallback default");
        assert_eq!(
            result.source,
            PromptSource::DefaultFile {
                path: prompts.join("default").join("prompt.md")
            }
        );
    }

    #[test]
    fn test_default_falls_back_to_builtin() {
        let dir = TempDir::new().unwrap();
        let prompts = dir.path().join("prompts");
        // No files created at all

        let result = resolve_root_prompt(&prompts, "coding");
        assert_eq!(result.content, BUILTIN_PROMPT);
        assert_eq!(result.source, PromptSource::Builtin);
    }

    // --- Agent prompt tests ---

    #[test]
    fn test_agent_prompt_from_profile() {
        let dir = TempDir::new().unwrap();
        let prompts = dir.path().join("prompts");
        write_file(
            &prompts.join("coding").join("agents").join("root.md"),
            "Coding root agent",
        );

        let result = resolve_agent_prompt(&prompts, "coding", "root", None);
        assert_eq!(result.content, "Coding root agent");
        assert_eq!(
            result.source,
            PromptSource::ProfileFile {
                path: prompts.join("coding").join("agents").join("root.md")
            }
        );
        assert_eq!(result.agent_id.as_deref(), Some("root"));
    }

    #[test]
    fn test_agent_prompt_falls_back_to_default_file() {
        let dir = TempDir::new().unwrap();
        let prompts = dir.path().join("prompts");
        // Only default has the agent override, not the "coding" profile
        write_file(
            &prompts
                .join("default")
                .join("agents")
                .join("root.md"),
            "Default root agent",
        );

        let result = resolve_agent_prompt(&prompts, "coding", "root", None);
        assert_eq!(result.content, "Default root agent");
        assert_eq!(
            result.source,
            PromptSource::DefaultFile {
                path: prompts.join("default").join("agents").join("root.md")
            }
        );
    }

    #[test]
    fn test_agent_prompt_falls_back_to_config() {
        let dir = TempDir::new().unwrap();
        let prompts = dir.path().join("prompts");
        // No files, but we pass a default_prompt

        let result = resolve_agent_prompt(
            &prompts,
            "coding",
            "root",
            Some("Config default prompt"),
        );
        assert_eq!(result.content, "Config default prompt");
        assert_eq!(result.source, PromptSource::AgentDefault);
    }

    #[test]
    fn test_agent_prompt_falls_back_to_builtin() {
        let dir = TempDir::new().unwrap();
        let prompts = dir.path().join("prompts");
        // No files, no default_prompt

        let result = resolve_agent_prompt(&prompts, "coding", "root", None);
        assert_eq!(result.content, BUILTIN_PROMPT);
        assert_eq!(result.source, PromptSource::Builtin);
    }

    // --- Template tests ---

    #[test]
    fn test_template_substitution() {
        let mut vars = HashMap::new();
        vars.insert("agent.name".to_owned(), "CodeAgent".to_owned());
        vars.insert("agent.id".to_owned(), "root".to_owned());
        vars.insert("runtime.date".to_owned(), "2025-01-15".to_owned());

        let content = "Hello {{ agent.name }} ({{ agent.id }}). Today is {{ runtime.date }}.";
        let result = apply_template(content, &vars, false).unwrap();
        assert_eq!(result, "Hello CodeAgent (root). Today is 2025-01-15.");
    }

    #[test]
    fn test_template_unknown_variable_strict() {
        let vars = HashMap::new();
        let content = "Hello {{ unknown.var }}";

        let result = apply_template(content, &vars, true);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, PromptError::UnknownVariable(v) if v == "unknown.var"));
    }

    #[test]
    fn test_template_unknown_variable_non_strict() {
        let vars = HashMap::new();
        let content = "Hello {{ unknown.var }} world";

        let result = apply_template(content, &vars, false).unwrap();
        assert_eq!(result, "Hello {{ unknown.var }} world");
    }

    #[test]
    fn test_template_no_variables() {
        let vars = HashMap::new();
        let content = "No variables here!";

        let result = apply_template(content, &vars, false).unwrap();
        assert_eq!(result, "No variables here!");
    }
}
