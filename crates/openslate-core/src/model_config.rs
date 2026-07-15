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
}

/// Resolve a model alias to a fully resolved model configuration.
///
/// # Errors
/// - Returns `ConfigError::MissingRequiredModel` if the alias doesn't exist.
/// - Returns `ConfigError::InvalidProviderRef` if the referenced provider doesn't exist.
pub fn resolve_model(
    config: &OpenSlateConfig,
    alias: &str,
) -> Result<ResolvedModel, ConfigError> {
    let model = config.models.get(alias).ok_or_else(|| {
        ConfigError::MissingRequiredModel(format!("Model alias '{}' not found in configuration", alias))
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
    })
}

/// Resolve all model aliases in the configuration.
///
/// Returns a map from alias to resolved model.
/// Returns an error on the first alias that fails to resolve.
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
        crate::config::parse_openslate_toml(include_str!("../fixtures/openslate.toml"))
        .unwrap()
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
}
