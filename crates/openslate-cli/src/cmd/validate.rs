//! `openslate validate` command.
//!
//! Validates `openslate.toml` and agent configuration files in the `agents/` directory,
//! printing errors and warnings to stdout and returning an appropriate exit code.

use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use openslate_core::config::validation::{validate_config, validate_strict, ValidationError};
use openslate_core::config::{
    parse_agents_dir, parse_openslate_toml, AgentsConfig, OpenSlateConfig,
};
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

/// Print an informational line (dim if color is supported).
fn print_info(msg: &str) {
    if supports_color() {
        println!("\x1b[2mINFO\x1b[0m: {msg}");
    } else {
        println!("[INFO] {msg}");
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

/// Load and parse agents from the `agents/` directory.
fn load_agents(agents_dir: &Path) -> Result<AgentsConfig> {
    if !agents_dir.is_dir() {
        anyhow::bail!("Agents directory not found: {}", agents_dir.display());
    }
    parse_agents_dir(agents_dir).with_context(|| {
        format!(
            "Failed to parse agents directory '{}'",
            agents_dir.display()
        )
    })
}

/// Resolve and load the merged configuration for `validate` on DEFAULT
/// discovery (model-mgmt-1 layering).
///
/// Returns `(config_path, config, merged_global_path)`: the user-global
/// library (`~/.config/openslate/`) is merged underneath the active config
/// via the shared wiring helpers; `merged_global_path` is `Some` only when
/// both files existed and were merged, so the caller can print the
/// provenance diagnostic. A missing config file surfaces through the loader
/// exactly as before ("Failed to read config file '<path>'").
///
/// Explicit `--config` does NOT go through here — `run_validate` keeps the
/// legacy single-file path for it (path not existence-checked, no global
/// library, identical error printing).
pub fn resolve_effective_config() -> Result<(PathBuf, OpenSlateConfig, Option<PathBuf>)> {
    let cwd = env::current_dir().context("Failed to get current directory")?;
    let paths = resolve_paths(&cwd);
    let (path, global) = openslate_app::wiring::select_config_files(
        paths.local_config_dir.as_deref(),
        &paths.global_config_dir,
    );
    let config = openslate_app::wiring::load_config_layered(&path, global.as_deref())?;
    Ok((path, config, global))
}

/// Entry point for `openslate validate` (main.rs).
///
/// - explicit `--config`: the legacy single-file path, byte-identical to the
///   pre-layering command (resolution, load-error printing, no merging);
/// - default discovery: the global library is merged underneath the active
///   config, with a provenance line when a merge happened.
pub fn run_validate(config_flag: Option<&str>, strict: bool) -> Result<()> {
    if let Some(flag) = config_flag {
        let config_path = resolve_config_path(Some(flag))?;
        let agents_path =
            resolve_agents_path(config_path.parent().unwrap_or_else(|| Path::new(".")));
        return run_validate_command(&config_path, &agents_path, strict);
    }

    let (config_path, config, merged_global) = match resolve_effective_config() {
        Ok(ok) => ok,
        Err(e) => {
            print_error(&format!("Failed to load config: {}", e));
            return Err(e);
        }
    };
    if let Some(global_path) = &merged_global {
        print_info(&format!("merged global library: {}", global_path.display()));
    }
    let agents_path = resolve_agents_path(config_path.parent().unwrap_or_else(|| Path::new(".")));
    run_validate_loaded(config, &config_path, &agents_path, strict)
}

/// Run the validate command against a config file on disk (legacy
/// single-file path: exact load-error printing preserved).
pub fn run_validate_command(config_path: &Path, agents_path: &Path, strict: bool) -> Result<()> {
    // Load config
    let config = match load_config(config_path) {
        Ok(c) => c,
        Err(e) => {
            print_error(&format!("Failed to load config: {}", e));
            return Err(e);
        }
    };
    run_validate_loaded(config, config_path, agents_path, strict)
}

/// Run validation over an already-loaded (possibly globally merged) config.
fn run_validate_loaded(
    config: OpenSlateConfig,
    config_path: &Path,
    agents_path: &Path,
    strict: bool,
) -> Result<()> {
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

    // Skills diagnostics: mirror the wiring's discovery so validation sees
    // the same catalog a run would. Warnings never fail non-strict mode
    // (lenient spec: a broken skill is skipped at runtime, not fatal).
    let mut skills_warning_count = 0usize;
    let mut catalog = openslate_core::skills::SkillsCatalog::default();
    if config.skills.enabled {
        let cwd = env::current_dir().context("Failed to get current directory")?;
        let sources = crate::wiring::skills_sources(config_path, &cwd);
        let (discovered, skill_warnings) = openslate_core::skills::discover_skills(&sources);
        for warning in &skill_warnings {
            print_warning(&format!("skills: {warning}"));
        }
        skills_warning_count += skill_warnings.len();

        // Static check: a non-empty `tools:` whitelist without `read_skill`
        // means the agent's system prompt will advertise skills it cannot
        // load (an empty whitelist exposes every tool, so it is fine).
        if !discovered.is_empty() {
            for agent in &agents.agents {
                if !agent.tools.is_empty() && !agent.tools.iter().any(|t| t == "read_skill") {
                    print_warning(&format!(
                        "skills: agent '{}' tool whitelist excludes 'read_skill' but skills \
                         are enabled — its system prompt will advertise skills it cannot load",
                        agent.id.0
                    ));
                    skills_warning_count += 1;
                }
            }
        }
        catalog = discovered;
    } else {
        print_info("skills disabled, skipping");
    }

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
    } else if strict && (has_warnings || skills_warning_count > 0) {
        // In strict mode, warnings (including skill diagnostics) cause exit 1
        Err(anyhow::anyhow!(
            "Configuration validation failed (warnings treated as errors in --strict mode)"
        ))
    } else {
        print_success(&format!(
            "Configuration is valid: {} agents, {} skills, {} warning(s)",
            agents.agents.len(),
            catalog.skills().len(),
            warnings.len() + skills_warning_count,
        ));
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

/// Resolve agents directory path from config directory.
pub fn resolve_agents_path(config_dir: &Path) -> PathBuf {
    config_dir.join("agents")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Helper: create a temp dir with valid openslate.toml and agents directory
    fn temp_project_with_valid_config() -> TempDir {
        let tmp = TempDir::new().expect("create temp dir");
        let openslate_dir = tmp.path().join(".openslate");
        std::fs::create_dir(&openslate_dir).expect("create .openslate dir");

        let toml = r#"
[providers.zhipu]
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
        std::fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");

        std::fs::create_dir(openslate_dir.join("agents")).expect("create agents dir");
        let agent_md = "---\nid: root\nname: Root Agent\nmodel: main\ndefault_prompt: You are the root agent.\n---\n";
        std::fs::write(openslate_dir.join("agents").join("root.md"), agent_md)
            .expect("write root.md");
        tmp
    }

    // Helper: create a temp dir with invalid config (missing models.main)
    fn temp_project_with_missing_main() -> TempDir {
        let tmp = TempDir::new().expect("create temp dir");
        let openslate_dir = tmp.path().join(".openslate");
        std::fs::create_dir(&openslate_dir).expect("create .openslate dir");

        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.fast]
provider = "zhipu"
model = "m2"
"#;
        std::fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");

        std::fs::create_dir(openslate_dir.join("agents")).expect("create agents dir");
        let agent_md = "---\nid: root\nname: Root Agent\nmodel: main\ndefault_prompt: You are the root agent.\n---\n";
        std::fs::write(openslate_dir.join("agents").join("root.md"), agent_md)
            .expect("write root.md");
        tmp
    }

    // Helper: create a temp dir with duplicate agent IDs
    fn temp_project_with_duplicate_agents() -> TempDir {
        let tmp = TempDir::new().expect("create temp dir");
        let openslate_dir = tmp.path().join(".openslate");
        std::fs::create_dir(&openslate_dir).expect("create .openslate dir");

        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"

[models.fast]
provider = "zhipu"
model = "m2"
"#;
        std::fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");

        let agents_dir = openslate_dir.join("agents");
        std::fs::create_dir(&agents_dir).expect("create agents dir");
        let root_md = "---\nid: root\nname: Root Agent\nmodel: main\n---\nRoot.\n";
        let dup_md = "---\nid: root\nname: Duplicate Agent\nmodel: fast\n---\nDuplicate.\n";
        std::fs::write(agents_dir.join("root.md"), root_md).expect("write root.md");
        std::fs::write(agents_dir.join("dup.md"), dup_md).expect("write dup.md");
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
        let agents_path = tmp.path().join(".openslate/agents");

        let result = run_validate_command(&config_path, &agents_path, false);
        assert!(result.is_ok(), "valid config should pass: {:?}", result);
    }

    #[test]
    fn test_missing_main_model_exits_error() {
        let tmp = temp_project_with_missing_main();
        let config_path = tmp.path().join(".openslate/openslate.toml");
        let agents_path = tmp.path().join(".openslate/agents");

        let result = run_validate_command(&config_path, &agents_path, false);
        assert!(result.is_err(), "missing main model should fail");
    }

    #[test]
    fn test_duplicate_agent_ids_exits_error() {
        let tmp = temp_project_with_duplicate_agents();
        let config_path = tmp.path().join(".openslate/openslate.toml");
        let agents_path = tmp.path().join(".openslate/agents");

        let result = run_validate_command(&config_path, &agents_path, false);
        assert!(result.is_err(), "duplicate agent IDs should fail");
    }

    #[test]
    fn test_config_file_not_found_gives_clear_error() {
        let tmp = temp_project_empty();
        let config_path = tmp.path().join("nonexistent/openslate.toml");
        let agents_path = tmp.path().join(".openslate/agents");

        let result = run_validate_command(&config_path, &agents_path, false);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to load config") || err_msg.contains("Failed to read"),
            "error should mention config loading failure: {err_msg}"
        );
    }

    // ── model-mgmt-1: global library merged on default discovery ─────────

    /// The orchestrator's integration scenario, driven in-process through the
    /// same composition `resolve_effective_config` uses on default discovery
    /// (`select_config_files` + `load_config_layered`), with explicit temp
    /// dirs instead of `XDG_CONFIG_HOME` env mutation (parallel-test safe).
    ///
    /// Global library: providers.g + models.good + models.bad (→ ghost
    /// provider) + levels main=good fast=bad. Local project: ONLY
    /// `[levels] main = "bad"` (a level remap of a global entry).
    ///
    /// Expected: the merged validate run reports the ghost PROVIDER for
    /// models.bad (the global library WAS merged), not "levels.main points
    /// to non-existent model entry 'bad'" (which a single-file read of the
    /// local config would produce).
    #[test]
    fn test_validate_merges_global_library_under_local_override() {
        let project = TempDir::new().expect("tmp project");
        let global_dir = TempDir::new().expect("tmp global");

        std::fs::write(
            global_dir.path().join("openslate.toml"),
            "[providers.g]\nbase_url = \"https://g.example.com\"\napi_key_env = \"GK\"\n\n\
             [models.good]\nprovider = \"g\"\nmodel = \"m-good\"\n\n\
             [models.bad]\nprovider = \"ghost\"\nmodel = \"m-bad\"\n\n\
             [levels]\nmain = \"good\"\nfast = \"bad\"\n",
        )
        .expect("global toml");
        let local_dir = project.path().join(".openslate");
        std::fs::create_dir(&local_dir).expect("mkdir");
        std::fs::write(
            local_dir.join("openslate.toml"),
            "[levels]\nmain = \"bad\"\n",
        )
        .expect("local toml");
        let agents_dir = local_dir.join("agents");
        std::fs::create_dir(&agents_dir).expect("mkdir");
        std::fs::write(
            agents_dir.join("root.md"),
            "---\nid: root\nname: Root\nmodel: main\ndefault_prompt: hi\n---\n",
        )
        .expect("agent file");
        let agents = parse_agents_dir(&agents_dir).expect("agents parse");

        // Same composition as resolve_effective_config's default branch.
        let (active, global) =
            openslate_app::wiring::select_config_files(Some(&local_dir), global_dir.path());
        assert_eq!(active, local_dir.join("openslate.toml"));
        assert_eq!(
            global.as_deref(),
            Some(global_dir.path().join("openslate.toml").as_path())
        );
        let merged = openslate_app::wiring::load_config_layered(&active, global.as_deref())
            .expect("layered load");

        // p1: local override main→bad + global model library merged in.
        assert_eq!(
            merged.levels.get("main").map(String::as_str),
            Some("bad"),
            "local level mapping wins"
        );
        assert!(merged.models.contains_key("bad"), "global model merged");
        assert!(merged.models.contains_key("good"), "global model merged");

        let errors = validate_config(&merged, &agents);
        let ghost_err = errors.iter().find(|e| e.message.contains("ghost"));
        assert!(
            ghost_err.is_some(),
            "merged validate must report the ghost provider: {errors:?}"
        );
        assert!(
            !errors
                .iter()
                .any(|e| e.message.contains("points to non-existent model entry")),
            "the local level remap must resolve against the MERGED library: {errors:?}"
        );

        // Contrast: the local file ALONE (pre-layering behavior) would flag
        // the dangling level — this is the bug this integration fixes.
        let local_alone =
            openslate_app::wiring::load_config_layered(&active, None).expect("single-file load");
        let errors_alone = validate_config(&local_alone, &agents);
        assert!(
            errors_alone
                .iter()
                .any(|e| e.message.contains("points to non-existent model entry")),
            "sanity: single-file read does NOT see the global library: {errors_alone:?}"
        );
    }

    /// p2 companion scenario: a global library with only valid entries and a
    /// local `[levels]` remap onto them validates clean under the merged
    /// load — the level requirements are satisfied entirely through the
    /// global library.
    #[test]
    fn test_validate_global_only_library_with_local_level_passes() {
        let project = TempDir::new().expect("tmp project");
        let global_dir = TempDir::new().expect("tmp global");

        std::fs::write(
            global_dir.path().join("openslate.toml"),
            "[providers.g]\nbase_url = \"https://g.example.com\"\napi_key_env = \"GK\"\n\n\
             [models.good]\nprovider = \"g\"\nmodel = \"m-good\"\n\n\
             [levels]\nfast = \"good\"\n",
        )
        .expect("global toml");
        let local_dir = project.path().join(".openslate");
        std::fs::create_dir(&local_dir).expect("mkdir");
        std::fs::write(
            local_dir.join("openslate.toml"),
            "[levels]\nmain = \"good\"\n",
        )
        .expect("local toml");

        let (active, global) =
            openslate_app::wiring::select_config_files(Some(&local_dir), global_dir.path());
        let merged = openslate_app::wiring::load_config_layered(&active, global.as_deref())
            .expect("layered load");
        assert!(merged.levels.contains_key("main"));

        let agents_dir = local_dir.join("agents");
        std::fs::create_dir(&agents_dir).expect("mkdir");
        std::fs::write(
            agents_dir.join("root.md"),
            "---\nid: root\nname: Root\nmodel: main\ndefault_prompt: hi\n---\n",
        )
        .expect("agent file");
        let agents = parse_agents_dir(&agents_dir).expect("agents parse");

        let errors = validate_config(&merged, &agents);
        assert!(
            errors.is_empty(),
            "merged global library + local levels validate clean: {errors:?}"
        );
    }

    #[test]
    fn test_explicit_config_stays_single_file() {
        // An explicit --config must NOT consult any global library: this
        // config has a dangling level and no matching model, which only
        // fails via the plain single-file finding (no merge can rescue it).
        let tmp = TempDir::new().expect("tmp");
        let config_path = tmp.path().join("only-levels.toml");
        std::fs::write(&config_path, "[levels]\nmain = \"ghost2\"\n").expect("write toml");
        let agents_path = tmp.path().join("agents");
        std::fs::create_dir(&agents_path).expect("mkdir");
        std::fs::write(
            agents_path.join("root.md"),
            "---\nid: root\nname: Root\nmodel: main\ndefault_prompt: hi\n---\n",
        )
        .expect("agent file");

        let config = load_config(&config_path).expect("config parses");
        let agents = parse_agents_dir(&agents_path).expect("agents parse");
        let errors = validate_config(&config, &agents);
        assert!(
            errors.iter().any(|e| e.message.contains("ghost2")
                && e.message.contains("points to non-existent model entry")),
            "explicit single file sees only itself (dangling level): {errors:?}"
        );
        assert!(
            run_validate_command(&config_path, &agents_path, false).is_err(),
            "explicit --config with a dangling level must fail"
        );
    }

    #[test]
    fn test_run_validate_loaded_surfaces_validation_failure() {
        // End-to-end through run_validate_loaded with a merged config: the
        // ghost-provider finding fails the command.
        let tmp = temp_project_with_valid_config();
        let openslate_dir = tmp.path().join(".openslate");
        let config =
            openslate_app::wiring::load_config_layered(&openslate_dir.join("openslate.toml"), None)
                .expect("load");
        let agents_path = openslate_dir.join("agents");
        // Corrupt a provider reference to force a validation failure.
        let mut config = config;
        if let Some(m) = config.models.get_mut("main") {
            m.provider = "ghost".into();
        }
        let result = run_validate_loaded(
            config,
            &openslate_dir.join("openslate.toml"),
            &agents_path,
            false,
        );
        assert!(result.is_err(), "ghost provider must fail validate");
    }

    #[test]
    fn test_strict_flag_warns_on_unused_model() {
        let tmp = temp_project_with_valid_config();
        let config_path = tmp.path().join(".openslate/openslate.toml");
        let agents_path = tmp.path().join(".openslate/agents");

        // In non-strict mode, this should pass
        let result = run_validate_command(&config_path, &agents_path, false);
        assert!(result.is_ok(), "non-strict should pass with unused model");
    }

    #[test]
    fn test_strict_mode_detects_unused_model() {
        let tmp = temp_project_with_valid_config();
        let config_path = tmp.path().join(".openslate/openslate.toml");
        let agents_path = tmp.path().join(".openslate/agents");

        // In strict mode, an unused model should produce a warning that causes failure
        let result = run_validate_command(&config_path, &agents_path, true);
        // The valid config has unused models (main is used but fast is not), so strict mode should warn
        // But we need to check: in our fixture, 'main' is used but 'fast' is not
        // Actually wait - in valid config we create, main is used by root agent, but fast is not used
        assert!(
            result.is_err(),
            "strict mode with unused model should warn and fail: {:?}",
            result
        );
    }

    #[test]
    fn test_strict_mode_passes_with_all_models_used() {
        let tmp = TempDir::new().expect("create temp dir");
        let openslate_dir = tmp.path().join(".openslate");
        std::fs::create_dir(&openslate_dir).expect("create .openslate dir");

        // Config where all models are used
        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"

[models.fast]
provider = "zhipu"
model = "m2"
"#;
        std::fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");

        let agents_dir = openslate_dir.join("agents");
        std::fs::create_dir(&agents_dir).expect("create agents dir");
        let root_md = "---\nid: root\nname: Root Agent\nmodel: main\n---\nRoot.\n";
        let worker_md = "---\nid: worker\nname: Worker Agent\nmodel: fast\n---\nWorker.\n";
        std::fs::write(agents_dir.join("root.md"), root_md).expect("write root.md");
        std::fs::write(agents_dir.join("worker.md"), worker_md).expect("write worker.md");

        let config_path = openslate_dir.join("openslate.toml");
        let agents_path = openslate_dir.join("agents");

        let result = run_validate_command(&config_path, &agents_path, true);
        assert!(
            result.is_ok(),
            "strict mode should pass when all models are used: {:?}",
            result
        );
    }

    // ── skills diagnostics ────────────────────────────────────────────────

    /// Temp project with one skill (`.openslate/skills/demo/SKILL.md`), a
    /// single model used by the root agent (so strict mode's unused-model
    /// check stays quiet), and a configurable root tool whitelist.
    fn temp_project_with_skill(toml_extra: &str, tools_yaml: &str) -> TempDir {
        let tmp = TempDir::new().expect("create temp dir");
        let openslate_dir = tmp.path().join(".openslate");
        std::fs::create_dir_all(openslate_dir.join("skills/demo")).expect("create dirs");
        std::fs::write(
            openslate_dir.join("skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: demo skill\n---\ndemo body\n",
        )
        .expect("write SKILL.md");

        let toml = format!(
            r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "KEY"

[models.main]
provider = "zhipu"
model = "m1"

[models.fast]
provider = "zhipu"
model = "m2"
{toml_extra}"#
        );
        std::fs::write(openslate_dir.join("openslate.toml"), toml).expect("write toml");

        let agents_dir = openslate_dir.join("agents");
        std::fs::create_dir(&agents_dir).expect("create agents dir");
        let root_md =
            format!("---\nid: root\nname: Root Agent\nmodel: main\n{tools_yaml}---\nRoot.\n");
        std::fs::write(agents_dir.join("root.md"), root_md).expect("write root.md");
        // fast is used by the worker so strict mode's unused-model check
        // stays quiet and the tests isolate the skills diagnostics.
        let worker_md = "---\nid: worker\nname: Worker Agent\nmodel: fast\n---\nWorker.\n";
        std::fs::write(agents_dir.join("worker.md"), worker_md).expect("write worker.md");
        tmp
    }

    #[test]
    fn test_skills_whitelist_excluding_read_skill_warns() {
        // Warn path: skills are enabled and discovered, but the root agent's
        // whitelist lacks `read_skill`. Non-strict passes (WARN only)...
        let tmp = temp_project_with_skill("", "tools:\n  - read_file\n");
        let config_path = tmp.path().join(".openslate/openslate.toml");
        let agents_path = tmp.path().join(".openslate/agents");
        assert!(
            run_validate_command(&config_path, &agents_path, false).is_ok(),
            "non-strict treats skill warnings as WARN only"
        );
        // ...while strict escalates them to a failure.
        assert!(
            run_validate_command(&config_path, &agents_path, true).is_err(),
            "strict mode escalates the whitelist warning to an error"
        );
    }

    #[test]
    fn test_skills_whitelist_including_read_skill_is_clean() {
        // Clean path: whitelist includes `read_skill` → no warning, strict
        // passes (single model, used).
        let tmp = temp_project_with_skill("", "tools:\n  - read_file\n  - read_skill\n");
        let config_path = tmp.path().join(".openslate/openslate.toml");
        let agents_path = tmp.path().join(".openslate/agents");
        let result = run_validate_command(&config_path, &agents_path, true);
        assert!(
            result.is_ok(),
            "clean path should pass strict: {:?}",
            result
        );
    }

    #[test]
    fn test_skills_disabled_skips_diagnostics() {
        // Disabled: no discovery, no whitelist check — an offending whitelist
        // is fine because no skills will be advertised.
        let tmp = temp_project_with_skill("[skills]\nenabled = false\n", "tools:\n  - read_file\n");
        let config_path = tmp.path().join(".openslate/openslate.toml");
        let agents_path = tmp.path().join(".openslate/agents");
        let result = run_validate_command(&config_path, &agents_path, true);
        assert!(
            result.is_ok(),
            "disabled skills skip all checks: {:?}",
            result
        );
    }

    #[test]
    fn test_skills_parse_warning_is_non_fatal_by_default() {
        // A broken SKILL.md warns but never blocks (lenient spec); strict
        // escalates it like any other warning.
        let tmp = temp_project_with_skill("", "tools:\n  - read_file\n  - read_skill\n");
        let bad_dir = tmp.path().join(".openslate/skills/bad");
        std::fs::create_dir_all(&bad_dir).expect("create bad dir");
        std::fs::write(bad_dir.join("SKILL.md"), "name: bad\n(no closing ---\n")
            .expect("write bad");

        let config_path = tmp.path().join(".openslate/openslate.toml");
        let agents_path = tmp.path().join(".openslate/agents");
        assert!(
            run_validate_command(&config_path, &agents_path, false).is_ok(),
            "non-strict: parse warnings are WARN, skill skipped at runtime"
        );
        assert!(
            run_validate_command(&config_path, &agents_path, true).is_err(),
            "strict escalates skill parse warnings"
        );
    }
}
