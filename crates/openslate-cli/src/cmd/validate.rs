//! `openslate validate` command.
//!
//! Validates `openslate.toml` and `agents.yaml` configuration files,
//! printing errors and warnings to stdout and returning an appropriate exit code.

use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use openslate_core::config::validation::{validate_config, validate_strict, ValidationError};
use openslate_core::config::{parse_agents_yaml, parse_openslate_toml, AgentsConfig, OpenSlateConfig};
use openslate_core::paths::resolve_paths;

/// Indicates whether the terminal supports color output.
fn supports_color() -> bool {
    // Respect NO_COLOR environment variable (https://no-color.org/)
    if env::var_os("NO_COLOR").is_some() {
        return false;
    }
    // Check if stdout is a terminal
    std::io::stdout().is_terminal()
}

/// Print an error line (red if color is supported).
fn print_error(msg: &str) {
    if supports_color() {
        eprintln!("\x1b[31mERROR\x1b[0m: {msg}");
    } else {
        eprintln!("[ERROR] {msg}");
    }
}

/// Print a warning line (yellow if color is supported).
fn print_warning(msg: &str) {
    if supports_color() {
        eprintln!("\x1b[33mWARN\x1b[0m: {msg}");
    } else {
        eprintln!("[WARN] {msg}");
    }
}

/// Print a success line (green if color is supported).
fn print_success(msg: &str) {
    if supports_color() {
        println!("\x1b[32m✓\x1b[0m {msg}");
    } else {
        println!("[OK] {msg}");
    }
}

/// Format a validation error for display.
fn format_validation_error(err: &ValidationError) -> String {
    format!("{}: {}", err.field, err.message)
}

/// Load and parse `openslate.toml` from the given path.
fn load_config(config_path: &Path) -> Result<OpenSlateConfig> {
    let content = fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read config file '{}'", config_path.display()))?;
    parse_openslate_toml(&content)
        .with_context(|| format!("Failed to parse config file '{}'", config_path.display()))
}

/// Load and parse `agents.yaml` from the given path.
fn load_agents(agents_path: &Path) -> Result<AgentsConfig> {
    let content = fs::read_to_string(agents_path)
        .with_context(|| format!("Failed to read agents file '{}'", agents_path.display()))?;
    parse_agents_yaml(&content)
        .with_context(|| format!("Failed to parse agents file '{}'", agents_path.display()))
}

/// Run the validate command.
///
/// Loads config from `config_path` and agents from `agents_path`,
/// runs validation, prints results, and returns `Ok(())` on success
/// or `Err(..)` on failure.
pub fn run_validate_command(config_path: &Path, agents_path: &Path, strict: bool) -> Result<()> {
    // Load config
    let config = match load_config(config_path) {
        Ok(c) => c,
        Err(e) => {
            print_error(&format!("Failed to load config: {}", e));
            return Err(e);
        }
    };

    // Load agents
    let agents = match load_agents(agents_path) {
        Ok(a) => a,
        Err(e) => {
            print_error(&format!("Failed to load agents: {}", e));
            return Err(e);
        }
    };

    // Run validation
    let (errors, warnings) = if strict {
        validate_strict(&config, &agents)
    } else {
        (validate_config(&config, &agents), Vec::new())
    };

    // Print errors
    let has_errors = !errors.is_empty();
    for err in &errors {
        print_error(&format_validation_error(err));
    }

    // Print warnings
    let has_warnings = !warnings.is_empty();
    for warn in &warnings {
        print_warning(&format_validation_error(warn));
    }

    // Exit code logic
    if has_errors {
        Err(anyhow::anyhow!("Configuration validation failed"))
    } else if strict && has_warnings {
        // In strict mode, warnings cause exit 1
        Err(anyhow::anyhow!("Configuration validation failed (warnings treated as errors in --strict mode)"))
    } else {
        print_success("Configuration is valid");
        Ok(())
    }
}

/// Resolve config file path from CLI `--config` flag or default.
pub fn resolve_config_path(config_flag: Option<&str>) -> Result<std::path::PathBuf> {
    let cwd = std::env::current_dir().context("Failed to get current directory")?;
    let paths = resolve_paths(&cwd);

    let config_path = if let Some(flag) = config_flag {
        Path::new(flag).to_path_buf()
    } else {
        paths.config_file
    };

    Ok(config_path)
}

/// Resolve agents file path from config directory.
pub fn resolve_agents_path(config_dir: &Path) -> PathBuf {
    config_dir.join("agents.yaml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Helper: create a temp dir with valid openslate.toml and agents.yaml
    fn temp_project_with_valid_config() -> TempDir {
        let tmp = TempDir::new().expect("create temp dir");
        let openslate_dir = tmp.path().join(".openslate");
        std::fs::create_dir(&openslate_dir).expect("create .openslate dir");

        let toml = r#"
[providers.zhipu]
kind = "openai_compatible"
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"

[models.fast]
provider = "zhipu"
model = "m2"

[limits]
max_steps = 10
max_depth = 3
max_tool_calls = 20
max_child_agent_calls = 5
timeout_ms = 30000
max_context_messages = 16
max_context_bytes = 64000
max_output_bytes = 65536
"#;
        let agents = r#"
agents:
  - id: root
    name: Root Agent
    model: main
    default_prompt: "You are the root agent."
"#;
        std::fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");
        std::fs::write(openslate_dir.join("agents.yaml"), agents).expect("write agents.yaml");
        tmp
    }

    // Helper: create a temp dir with invalid config (missing models.main)
    fn temp_project_with_missing_main() -> TempDir {
        let tmp = TempDir::new().expect("create temp dir");
        let openslate_dir = tmp.path().join(".openslate");
        std::fs::create_dir(&openslate_dir).expect("create .openslate dir");

        let toml = r#"
[providers.zhipu]
kind = "openai_compatible"
base_url = "https://example.com"
api_key_env = "KEY"

[models.fast]
provider = "zhipu"
model = "m2"
"#;
        let agents = r#"
agents:
  - id: root
    name: Root Agent
    model: main
    default_prompt: "You are the root agent."
"#;
        std::fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");
        std::fs::write(openslate_dir.join("agents.yaml"), agents).expect("write agents.yaml");
        tmp
    }

    // Helper: create a temp dir with duplicate agent IDs
    fn temp_project_with_duplicate_agents() -> TempDir {
        let tmp = TempDir::new().expect("create temp dir");
        let openslate_dir = tmp.path().join(".openslate");
        std::fs::create_dir(&openslate_dir).expect("create .openslate dir");

        let toml = r#"
[providers.zhipu]
kind = "openai_compatible"
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"

[models.fast]
provider = "zhipu"
model = "m2"
"#;
        let agents = r#"
agents:
  - id: root
    name: Root Agent
    model: main
    default_prompt: "You are the root agent."
  - id: root
    name: Duplicate Agent
    model: fast
    default_prompt: "Duplicate."
"#;
        std::fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");
        std::fs::write(openslate_dir.join("agents.yaml"), agents).expect("write agents.yaml");
        tmp
    }

    // Helper: create a temp dir with no config at all
    fn temp_project_empty() -> TempDir {
        TempDir::new().expect("create temp dir")
    }

    #[test]
    fn test_valid_config_exits_ok() {
        let tmp = temp_project_with_valid_config();
        let config_path = tmp.path().join(".openslate/openslate.toml");
        let agents_path = tmp.path().join(".openslate/agents.yaml");

        let result = run_validate_command(&config_path, &agents_path, false);
        assert!(result.is_ok(), "valid config should pass: {:?}", result);
    }

    #[test]
    fn test_missing_main_model_exits_error() {
        let tmp = temp_project_with_missing_main();
        let config_path = tmp.path().join(".openslate/openslate.toml");
        let agents_path = tmp.path().join(".openslate/agents.yaml");

        let result = run_validate_command(&config_path, &agents_path, false);
        assert!(result.is_err(), "missing main model should fail");
    }

    #[test]
    fn test_duplicate_agent_ids_exits_error() {
        let tmp = temp_project_with_duplicate_agents();
        let config_path = tmp.path().join(".openslate/openslate.toml");
        let agents_path = tmp.path().join(".openslate/agents.yaml");

        let result = run_validate_command(&config_path, &agents_path, false);
        assert!(result.is_err(), "duplicate agent IDs should fail");
    }

    #[test]
    fn test_config_file_not_found_gives_clear_error() {
        let tmp = temp_project_empty();
        let config_path = tmp.path().join("nonexistent/openslate.toml");
        let agents_path = tmp.path().join(".openslate/agents.yaml");

        let result = run_validate_command(&config_path, &agents_path, false);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to load config") || err_msg.contains("Failed to read"),
            "error should mention config loading failure: {err_msg}"
        );
    }

    #[test]
    fn test_strict_flag_warns_on_unused_model() {
        let tmp = temp_project_with_valid_config();
        let config_path = tmp.path().join(".openslate/openslate.toml");
        let agents_path = tmp.path().join(".openslate/agents.yaml");

        // In non-strict mode, this should pass
        let result = run_validate_command(&config_path, &agents_path, false);
        assert!(result.is_ok(), "non-strict should pass with unused model");
    }

    #[test]
    fn test_strict_mode_detects_unused_model() {
        let tmp = temp_project_with_valid_config();
        let config_path = tmp.path().join(".openslate/openslate.toml");
        let agents_path = tmp.path().join(".openslate/agents.yaml");

        // In strict mode, an unused model should produce a warning that causes failure
        let result = run_validate_command(&config_path, &agents_path, true);
        // The valid config has unused models (main is used but fast is not), so strict mode should warn
        // But we need to check: in our fixture, 'main' is used but 'fast' is not
        // Actually wait - in valid config we create, main is used by root agent, but fast is not used
        assert!(result.is_err(), "strict mode with unused model should warn and fail: {:?}", result);
    }

    #[test]
    fn test_strict_mode_passes_with_all_models_used() {
        let tmp = TempDir::new().expect("create temp dir");
        let openslate_dir = tmp.path().join(".openslate");
        std::fs::create_dir(&openslate_dir).expect("create .openslate dir");

        // Config where all models are used
        let toml = r#"
[providers.zhipu]
kind = "openai_compatible"
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"

[models.fast]
provider = "zhipu"
model = "m2"
"#;
        // Both main and fast are used
        let agents = r#"
agents:
  - id: root
    name: Root Agent
    model: main
    default_prompt: "Root."
  - id: worker
    name: Worker Agent
    model: fast
    default_prompt: "Worker."
"#;
        std::fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");
        std::fs::write(openslate_dir.join("agents.yaml"), agents).expect("write agents.yaml");

        let config_path = openslate_dir.join("openslate.toml");
        let agents_path = openslate_dir.join("agents.yaml");

        let result = run_validate_command(&config_path, &agents_path, true);
        assert!(result.is_ok(), "strict mode should pass when all models are used: {:?}", result);
    }
}
