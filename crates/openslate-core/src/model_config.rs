//! Model configuration resolver.
//!
//! Maps model alias strings (like "main", "fast", "deep-reasoner") to fully
//! resolved model configurations by cross-referencing the `[providers]` and
//! `[models]` sections of `openslate.toml`.

use std::collections::HashMap;

use crate::config::{OpenSlateConfig, ProviderConfig};
use crate::error::ConfigError;

/// A fully resolved model configuration ready for use.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    /// The alias that was resolved (e.g., "main", "fast").
    pub alias: String,
    /// The provider name (e.g., "zhipu", "minimax").
    pub provider_name: String,
    /// The provider configuration.
    pub provider: ProviderConfig,
    /// The model identifier to send to the API (e.g., "glm-5.1").
    pub model_id: String,
    /// Whether this model supports tool/function calling.
    pub supports_tool_call: bool,
    /// Whether this model supports vision/image input.
    pub supports_vision: bool,
    /// Whether this model supports extended reasoning.
    pub supports_reasoning: bool,
    /// Standard input price, USD per million tokens (P2-3).
    pub input_price_per_mtok: Option<f64>,
    /// Standard output price, USD per million tokens (P2-3).
    pub output_price_per_mtok: Option<f64>,
}

impl ResolvedModel {
    /// Pricing snapshot for this model (P2-3) — THE pricing helper shared
    /// by the runtime loop (via `RunConfig::cost`) and the REPL's compact
    /// summary accounting, so every consumer prices usage identically.
    pub fn cost_spec(&self) -> crate::runtime::CostSpec {
        crate::runtime::CostSpec::from_prices(self.input_price_per_mtok, self.output_price_per_mtok)
    }
}

/// Resolve a model alias to a fully resolved model configuration.
///
/// Resolution order (model-mgmt-1 two-layer schema):
/// 1. If `alias` names a level in `[levels]`, it maps to the referenced
///    `[models]` entry. A level pointing at a missing entry yields
///    `ConfigError::InvalidLevelRef`.
/// 2. Otherwise `alias` is looked up directly in `[models]` — the legacy
///    path, unchanged: configs without `[levels]` (or aliases outside it)
///    resolve exactly as before, with `ConfigError::MissingRequiredModel`
///    for unknown names.
///
/// [`ResolvedModel::alias`] stays "the alias that was resolved" verbatim —
/// the level name or the entry name as passed in, never the indirection
/// target.
///
/// # Errors
/// - Returns `ConfigError::MissingRequiredModel` if the alias doesn't exist.
/// - Returns `ConfigError::InvalidLevelRef` if a `[levels]` alias points at
///   a non-existent model entry.
/// - Returns `ConfigError::InvalidProviderRef` if the referenced provider doesn't exist.
pub fn resolve_model(config: &OpenSlateConfig, alias: &str) -> Result<ResolvedModel, ConfigError> {
    let entry_name = match config.levels.get(alias) {
        Some(entry) => entry.as_str(),
        None => alias,
    };
    let model = config.models.get(entry_name).ok_or_else(|| {
        if entry_name != alias {
            ConfigError::InvalidLevelRef(format!(
                "level '{alias}' points to unknown model entry '{entry_name}'"
            ))
        } else {
            ConfigError::MissingRequiredModel(format!(
                "Model alias '{}' not found in configuration",
                alias
            ))
        }
    })?;

    let provider = config.providers.get(&model.provider).ok_or_else(|| {
        ConfigError::InvalidProviderRef(format!(
            "Provider '{}' referenced by model '{}' not found",
            model.provider, alias
        ))
    })?;

    Ok(ResolvedModel {
        alias: alias.to_owned(),
        provider_name: model.provider.clone(),
        provider: provider.clone(),
        model_id: model.model.clone(),
        supports_tool_call: model.supports_tool_call,
        supports_vision: model.supports_vision,
        supports_reasoning: model.supports_reasoning,
        input_price_per_mtok: model.input_price_per_mtok,
        output_price_per_mtok: model.output_price_per_mtok,
    })
}

/// Resolve all model aliases in the configuration.
///
/// Returns a map from alias to resolved model.
/// Returns an error on the first alias that fails to resolve.
///
/// Iterates the `[models]` library entries (model-mgmt-1 keeps this
/// unchanged). Each entry name goes through the regular [`resolve_model`]
/// path, so a name that is also a `[levels]` key resolves via that level —
/// identical to how agents referencing the name would see it.
pub fn resolve_all_models(
    config: &OpenSlateConfig,
) -> Result<HashMap<String, ResolvedModel>, ConfigError> {
    config
        .models
        .keys()
        .map(|alias| resolve_model(config, alias).map(|rm| (alias.clone(), rm)))
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn example_config() -> OpenSlateConfig {
        crate::config::parse_openslate_toml(include_str!("../fixtures/openslate.toml")).unwrap()
    }

    #[test]
    fn resolve_main_alias() {
        let config = example_config();
        let resolved = resolve_model(&config, "main").unwrap();
        assert_eq!(resolved.alias, "main");
        assert_eq!(resolved.provider_name, "zhipu");
        assert_eq!(resolved.model_id, "glm-5.1");
        assert!(resolved.supports_tool_call);
        assert!(resolved.supports_reasoning);
        assert!(!resolved.supports_vision);
    }

    #[test]
    fn resolve_fast_alias() {
        let config = example_config();
        let resolved = resolve_model(&config, "fast").unwrap();
        assert_eq!(resolved.alias, "fast");
        assert_eq!(resolved.provider_name, "minimax");
        assert_eq!(resolved.model_id, "MiniMax-M2.7-highspeed");
        assert!(resolved.supports_tool_call);
        assert!(!resolved.supports_reasoning);
    }

    #[test]
    fn nonexistent_alias_returns_error() {
        let config = example_config();
        let result = resolve_model(&config, "nonexistent");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ConfigError::MissingRequiredModel(_)
        ));
    }

    // ── [levels] indirection (model-mgmt-1) ──────────────────────────────

    #[test]
    fn resolve_via_level_indirection() {
        // `main` is a LEVEL name mapping to the library entry "glm5"; the
        // resolved model carries the level name as `alias` verbatim but the
        // entry's provider/model payload.
        let toml = r#"
[providers.p]
base_url = "http://localhost"
api_key_env = "K"

[models.glm5]
provider = "p"
model = "glm-5.1"

[models.small]
provider = "p"
model = "glm-4-flash"

[levels]
main = "glm5"
fast = "small"
"#;
        let config = crate::config::parse_openslate_toml(toml).unwrap();
        let resolved = resolve_model(&config, "main").unwrap();
        assert_eq!(resolved.alias, "main", "alias stays the level name");
        assert_eq!(resolved.provider_name, "p");
        assert_eq!(resolved.model_id, "glm-5.1");
        let fast = resolve_model(&config, "fast").unwrap();
        assert_eq!(fast.alias, "fast");
        assert_eq!(fast.model_id, "glm-4-flash");
    }

    #[test]
    fn level_pointing_to_missing_entry_is_invalid_level_ref() {
        let toml = r#"
[providers.p]
base_url = "http://localhost"
api_key_env = "K"

[models.main]
provider = "p"
model = "m1"

[levels]
fast = "ghost"
"#;
        let config = crate::config::parse_openslate_toml(toml).unwrap();
        let result = resolve_model(&config, "fast");
        let err = result.unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidLevelRef(_)),
            "got {err:?}"
        );
        assert_eq!(
            err.to_string(),
            "invalid level reference: level 'fast' points to unknown model entry 'ghost'"
        );
    }

    #[test]
    fn level_precedence_over_same_named_entry() {
        // Degenerate config where a models entry shares a level's name but the
        // level points elsewhere: the level wins (resolution checks [levels]
        // first).
        let toml = r#"
[providers.p]
base_url = "http://localhost"
api_key_env = "K"

[models.main]
provider = "p"
model = "direct"

[models.other]
provider = "p"
model = "via-level"

[levels]
main = "other"
"#;
        let config = crate::config::parse_openslate_toml(toml).unwrap();
        let resolved = resolve_model(&config, "main").unwrap();
        assert_eq!(resolved.model_id, "via-level");
    }

    #[test]
    fn legacy_config_without_levels_resolves_directly() {
        // A config with NO [levels] section (and an alias outside any level)
        // keeps the exact legacy behavior: direct [models] lookup and the
        // MissingRequiredModel error for unknown names.
        let toml = r#"
[providers.p]
base_url = "http://localhost"
api_key_env = "K"

[models.main]
provider = "p"
model = "m1"
"#;
        let config = crate::config::parse_openslate_toml(toml).unwrap();
        assert!(config.levels.is_empty());
        let resolved = resolve_model(&config, "main").unwrap();
        assert_eq!(resolved.alias, "main");
        assert_eq!(resolved.model_id, "m1");
        let result = resolve_model(&config, "fast");
        assert!(matches!(
            result.unwrap_err(),
            ConfigError::MissingRequiredModel(_)
        ));
    }

    #[test]
    fn resolve_all_models_returns_all() {
        let config = example_config();
        let all = resolve_all_models(&config).unwrap();
        assert!(all.contains_key("main"));
        assert!(all.contains_key("fast"));
        assert!(all.contains_key("deep-reasoner"));
        assert!(all.contains_key("vision"));
    }

    #[test]
    fn model_references_nonexistent_provider_returns_error() {
        let toml = r#"
[models.ghost]
provider = "nonexistent"
model = "ghost-model"
"#;
        let config = crate::config::parse_openslate_toml(toml).unwrap();
        let result = resolve_model(&config, "ghost");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ConfigError::InvalidProviderRef(_)
        ));
    }

    #[test]
    fn resolved_model_provider_is_cloned() {
        let config = example_config();
        let resolved = resolve_model(&config, "main").unwrap();
        // Verify we get a owned clone of the provider
        assert_eq!(
            resolved.provider.base_url,
            "https://open.bigmodel.cn/api/paas/v4"
        );
        assert_eq!(resolved.provider.api_key_env, "ZHIPU_API_KEY");
    }

    #[test]
    fn cost_spec_resolves_from_model_pricing() {
        // The fixture keeps pricing commented out → unconfigured spec.
        let config = example_config();
        let main = resolve_model(&config, "main").unwrap();
        assert!(!main.cost_spec().is_configured());
        assert_eq!(
            main.cost_spec().cost_of(&crate::types::Usage {
                input_tokens: 5,
                output_tokens: 5,
                cached_input_tokens: None
            }),
            0.0
        );

        // With prices configured, the spec carries both through.
        let toml = r#"
[providers.p]
base_url = "http://localhost"
api_key_env = "K"

[models.priced]
provider = "p"
model = "m"
input_price_per_mtok = 1.5
output_price_per_mtok = 3.0
"#;
        let config = crate::config::parse_openslate_toml(toml).unwrap();
        let resolved = resolve_model(&config, "priced").unwrap();
        let spec = resolved.cost_spec();
        assert!(spec.is_configured());
        assert_eq!(spec.input_price_per_mtok, Some(1.5));
        assert_eq!(spec.output_price_per_mtok, Some(3.0));
        assert!(
            (spec.cost_of(&crate::types::Usage {
                input_tokens: 2_000_000,
                output_tokens: 1_000_000,
                cached_input_tokens: None
            }) - 6.0f64)
                .abs()
                < 1e-12
        );
    }
}
