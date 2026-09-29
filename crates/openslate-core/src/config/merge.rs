//! Global → local config merging (model-mgmt-1).
//!
//! OpenSlate layers configuration: the model library (providers / models /
//! API keys) lives in the user-global `~/.config/openslate/openslate.toml`
//! while project-specific bits (levels, database, limits, …) live in the
//! local `.openslate/openslate.toml`. [`merge_configs`] folds the two parsed
//! configs into one effective config.

use std::collections::HashMap;

use super::OpenSlateConfig;

/// Merge a global (base) and a local (override) config into an owned
/// effective config.
///
/// Layering rules:
/// - `providers`, `models` and `levels` are shallow-merged per key: a local
///   entry with the same name replaces the global entry **wholesale** (no
///   field-level deep merge); local-only entries are added; global-only
///   entries are kept.
/// - Every other section (`project`, `database`, `prompts`, `limits`,
///   `trace`, `mcp`, `builtin_tools`, `skills`, `ptc`, `approval`, `tui`)
///   is taken from local when present, else from global. For `Option`
///   sections that means `local = None` falls back to global; non-`Option`
///   sections take the local value verbatim (local always wins — a local
///   file without the section keeps the section's defaults).
pub fn merge_configs(global: &OpenSlateConfig, local: &OpenSlateConfig) -> OpenSlateConfig {
    OpenSlateConfig {
        project: local.project.clone().or_else(|| global.project.clone()),
        database: local.database.clone().or_else(|| global.database.clone()),
        prompts: local.prompts.clone().or_else(|| global.prompts.clone()),
        limits: local.limits.clone().or_else(|| global.limits.clone()),
        providers: merge_map(&global.providers, &local.providers),
        models: merge_map(&global.models, &local.models),
        levels: merge_map(&global.levels, &local.levels),
        trace: local.trace.clone().or_else(|| global.trace.clone()),
        mcp: local.mcp.clone().or_else(|| global.mcp.clone()),
        builtin_tools: local.builtin_tools.clone(),
        skills: local.skills.clone(),
        ptc: local.ptc.clone(),
        approval: local.approval.clone().or_else(|| global.approval.clone()),
        tui: local.tui.clone(),
    }
}

/// Shallow per-key merge: global entries first, local entries overwrite
/// same-named keys wholesale and add new ones.
fn merge_map<T: Clone>(
    global: &HashMap<String, T>,
    local: &HashMap<String, T>,
) -> HashMap<String, T> {
    let mut merged = global.clone();
    for (key, value) in local {
        merged.insert(key.clone(), value.clone());
    }
    merged
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(toml: &str) -> OpenSlateConfig {
        crate::config::parse_openslate_toml(toml).expect("test toml should parse")
    }

    /// Global: two providers, two models, a level, limits + approval set.
    fn global_config() -> OpenSlateConfig {
        config_with(
            r#"
[providers.globalp]
base_url = "https://global.example.com"
api_key_env = "GLOBAL_KEY"
max_attempts = 5

[providers.shared]
base_url = "https://shared-global.example.com"
api_key_env = "SHARED_GLOBAL_KEY"

[models.gmain]
provider = "globalp"
model = "g-model"

[models.shared]
provider = "shared"
model = "shared-old"

[levels]
main = "gmain"

[limits]
max_steps = 10
timeout_ms = 30000

[approval]
policy = "manual"
"#,
        )
    }

    /// Local: overrides `shared` wholesale, adds a local-only provider/model,
    /// remaps the level, leaves its own `[limits]` unset.
    fn local_config() -> OpenSlateConfig {
        config_with(
            r#"
[providers.shared]
base_url = "https://shared-local.example.com"
api_key_env = "SHARED_LOCAL_KEY"

[models.shared]
provider = "shared"
model = "shared-new"

[models.lmain]
provider = "shared"
model = "l-model"

[levels]
main = "lmain"
extra = "shared"
"#,
        )
    }

    #[test]
    fn merge_local_overrides_same_name_entries_wholesale() {
        let merged = merge_configs(&global_config(), &local_config());

        let shared = merged.providers.get("shared").expect("shared provider");
        assert_eq!(shared.base_url, "https://shared-local.example.com");
        assert_eq!(shared.api_key_env, "SHARED_LOCAL_KEY");
        assert_eq!(
            shared.max_attempts, 3,
            "local entry replaces global wholesale (no field-level merge): \
             the global max_attempts=5 must NOT leak through"
        );

        let model = merged.models.get("shared").expect("shared model");
        assert_eq!(model.model, "shared-new");
    }

    #[test]
    fn merge_local_adds_and_global_only_entries_survive() {
        let merged = merge_configs(&global_config(), &local_config());

        // local-only additions
        let lmodel = merged.models.get("lmain").expect("local-only model");
        assert_eq!(lmodel.model, "l-model");

        // global-only entries preserved
        let globalp = merged.providers.get("globalp").expect("global provider");
        assert_eq!(globalp.base_url, "https://global.example.com");
        let gmain = merged.models.get("gmain").expect("global-only model");
        assert_eq!(gmain.model, "g-model");
    }

    #[test]
    fn merge_levels_local_maps_override_global() {
        let merged = merge_configs(&global_config(), &local_config());
        assert_eq!(
            merged.levels.get("main").map(String::as_str),
            Some("lmain"),
            "local level mapping wins"
        );
        assert_eq!(
            merged.levels.get("extra").map(String::as_str),
            Some("shared"),
            "local-only level added"
        );
        // Entries themselves remain merged per key (levels is a plain map).
        assert_eq!(merged.levels.len(), 2);
    }

    #[test]
    fn merge_option_sections_fall_back_to_global() {
        let merged = merge_configs(&global_config(), &local_config());

        // local has no [limits] → global's limits apply
        let limits = merged.limits.as_ref().expect("global limits survive");
        assert_eq!(limits.max_steps, 10);
        assert_eq!(limits.timeout_ms, 30_000);

        // local has no [approval] → global's approval applies
        let approval = merged.approval.as_ref().expect("global approval survives");
        assert!(matches!(
            approval.to_policy(),
            crate::approval::ApprovalPolicy::Manual
        ));
    }

    #[test]
    fn merge_option_sections_local_wins_when_present() {
        let local = config_with(
            r#"
[providers.p]
base_url = "https://local.example.com"
api_key_env = "K"

[models.main]
provider = "p"
model = "m"

[limits]
max_steps = 3
timeout_ms = 1000

[approval]
policy = "auto"
"#,
        );
        let merged = merge_configs(&global_config(), &local);
        let limits = merged.limits.as_ref().expect("local limits win");
        assert_eq!(limits.max_steps, 3);
        let approval = merged.approval.as_ref().expect("local approval wins");
        assert!(matches!(
            approval.to_policy(),
            crate::approval::ApprovalPolicy::Auto
        ));
    }

    #[test]
    fn merge_non_option_sections_local_always_wins() {
        // Non-Option sections (builtin_tools/skills/ptc/tui) take local's
        // value verbatim — a local file without the section resets them to
        // defaults even when global configured them.
        let global = config_with(
            r#"
[providers.p]
base_url = "https://global.example.com"
api_key_env = "K"

[models.main]
provider = "p"
model = "m"

[builtin_tools]
shell = false

[skills]
max_list_chars = 42
"#,
        );
        let local = config_with(
            r#"
[providers.p]
base_url = "https://global.example.com"
api_key_env = "K"

[models.main]
provider = "p"
model = "m"
"#,
        );
        let merged = merge_configs(&global, &local);
        assert!(
            merged.builtin_tools.shell,
            "local wins verbatim: global's shell=false is dropped"
        );
        assert_eq!(
            merged.skills.max_list_chars, 8000,
            "local wins verbatim: global's 42 is dropped"
        );

        // And when local DOES carry the section, its values apply.
        let local2 = config_with(
            r#"
[providers.p]
base_url = "https://global.example.com"
api_key_env = "K"

[models.main]
provider = "p"
model = "m"

[builtin_tools]
shell = false
"#,
        );
        let merged2 = merge_configs(&global, &local2);
        assert!(!merged2.builtin_tools.shell);
    }
}
