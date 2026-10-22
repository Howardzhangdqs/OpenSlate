//! Model configuration resolver.
//!
//! Maps model alias strings (like "main", "fast", "deep-reasoner") to fully
//! resolved model configurations by cross-referencing the `[providers]` and
//! `[models]` sections of `openslate.toml`.

use std::collections::HashMap;

use crate::config::{OpenSlateConfig, ProviderConfig};
use crate::error::ConfigError;

// ── Capabilities（功能 → 代号）───────────────────────────────────────────
//
// `[capabilities]` 把"功能"（谁在用模型）与"代号"（用哪个模型档位）
// 解耦：代号是用户自由命名的 `[levels]` key（fast / main / vision …，
// 想配几个配几个），功能是固定 id。缺省映射仅为历史兼容值：main 沿用
// "main"；compact 的"未绑定 → fast"缺省已不再被 session 引擎使用
// （未绑定 = 继承会话当前模型，见 openslate-session engine.rs
// `run_auto_compact`，其取值不走本模块的 `capability_alias`）。

/// 功能 id：主对话（正常 API 请求）。
pub const CAP_MAIN: &str = "main";
/// 功能 id：上下文压缩（auto-compact 摘要）。
pub const CAP_COMPACT: &str = "compact";
/// 功能 id：会话标题生成。
pub const CAP_TITLE: &str = "title";
/// 功能 id：审批 AI 审查（mobile 审批装饰器的 LLM 审查器，
/// tool-approval P1a）。**不做 capability_alias 硬缺省**：未绑定时使用点
/// 继承会话模型（compact 先例——`run_auto_compact` 不走本模块缺省），
/// 消费方在 mobile 层（P1b）。
pub const CAP_REVIEW: &str = "review";

/// 全部功能 id（展示顺序即 UI 默认顺序）。
pub const CAPABILITIES: &[&str] = &[CAP_MAIN, CAP_COMPACT, CAP_TITLE, CAP_REVIEW];

/// 功能的中文名（UI 展示 / 事件文案）。
pub fn capability_title(cap: &str) -> &'static str {
    match cap {
        CAP_MAIN => "主对话",
        CAP_COMPACT => "上下文压缩",
        CAP_TITLE => "标题生成",
        CAP_REVIEW => "审批审查",
        _ => "未知功能",
    }
}

/// 功能 → 代号（含缺省回退）。
///
/// `[capabilities]` 有值用值；没有该节/该 key 时回退到历史默认
/// （`main`→"main"、`compact`/`title`→"fast"）。**注意**：`compact`
/// 分支的 "fast" 缺省仅保留兼容旧调用方——session 引擎已不再依赖它，
/// 未绑定 compact 时直接继承会话当前模型（openslate-session
/// `engine.rs::run_auto_compact` 自行取值，不走本函数缺省）。`review`
/// 同款约定：`_` 兜底臂的 "fast" 仅为 map 通用展示（ConfigView 透出），
/// 运行时未绑定 review 一律**继承会话模型**（mobile P1b 使用点决定），
/// 不依赖本函数缺省。返回的代号交给 [`resolve_model`] 解析；解析失败
/// 由调用方按各功能自己的兜底策略处理（compact → 机械拼接、title →
/// prompt 截断）。
pub fn capability_alias(config: &OpenSlateConfig, cap: &str) -> String {
    let fallback = match cap {
        CAP_MAIN => "main",
        CAP_COMPACT | CAP_TITLE => "fast",
        _ => "fast",
    };
    config
        .capabilities
        .get(cap)
        .cloned()
        .unwrap_or_else(|| fallback.to_owned())
}

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
    /// Thinking effort（推理力度）：六档全集 `"minimal" | "low" | "medium"
    /// | "high" | "xhigh" | "max"`，`None` = 跟随 provider 默认（wire 上
    /// 不传该字段）。非法配置值已在 [`resolve_model`] 处 warn + 归一为
    /// None，消费端拿到的恒为合法值或 None。
    pub reasoning_effort: Option<String>,
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

    // thinking effort 容忍归一：接受七档全集 none/minimal/low/medium/
    // high/xhigh/max（契约钉死；各 provider 实际档位不一，存全集、由 UI
    // 按模型能力裁剪）。"none" = 显式发 `reasoning_effort:"none"` 硬关闭
    // 思考（GLM-5.2/Gemini 2.5 支持；GLM-5.3/DeepSeek 不支持），与缺省
    // 的 Auto（不传参、跟随 provider 默认）是两种语义。其他值（手改
    // TOML 拼错、大小写漂移等）warn 后按 None——配置瑕疵不应炸掉
    // provider 构建或会话，请求少带该字段即回落 provider 默认（Auto）。
    let reasoning_effort = match model.reasoning_effort.as_deref() {
        Some(
            "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max",
        ) => model.reasoning_effort.clone(),
        Some(invalid) => {
            tracing::warn!(
                "models entry '{entry_name}': invalid reasoning_effort '{invalid}' \
                 (expected none/minimal/low/medium/high/xhigh/max); ignoring"
            );
            None
        }
        None => None,
    };

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
        reasoning_effort,
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

    // ── reasoning effort（thinking effort）容忍归一 ─────────────────────

    #[test]
    fn resolve_reasoning_effort_passes_through_and_tolerates_invalid() {
        // 七档全集逐档透传（契约：存全集、UI 按模型能力裁剪；"none" 是
        // 显式硬关闭思考档，区别于缺省 Auto）。
        for level in [
            "none", "minimal", "low", "medium", "high", "xhigh", "max",
        ] {
            let toml = format!(
                r#"
[providers.p]
base_url = "http://localhost"
api_key_env = "K"

[models.m]
provider = "p"
model = "m1"
reasoning_effort = "{level}"
"#
            );
            let config = crate::config::parse_openslate_toml(&toml).unwrap();
            assert_eq!(
                resolve_model(&config, "m").unwrap().reasoning_effort,
                Some(level.to_owned()),
                "level '{level}' must pass through verbatim"
            );
        }

        let toml = r#"
[providers.p]
base_url = "http://localhost"
api_key_env = "K"

[models.bad]
provider = "p"
model = "m2"
reasoning_effort = "ultra"

[models.unset]
provider = "p"
model = "m3"
"#;
        let config = crate::config::parse_openslate_toml(toml).unwrap();
        // 非法值：warn + 归一 None（不 panic、不阻断解析）。
        assert_eq!(resolve_model(&config, "bad").unwrap().reasoning_effort, None);
        // 未配置 = None（Auto，跟随 provider 默认）。
        assert_eq!(resolve_model(&config, "unset").unwrap().reasoning_effort, None);
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
    fn capability_alias_defaults_without_section() {
        // 无 [capabilities]：回退历史默认（main→"main"，compact/title→"fast"）。
        // 注：compact 的 "fast" 缺省仅保留函数兼容——session 引擎未绑定
        // 时已改为继承会话当前模型，不再消费该缺省值。
        let toml = r#"
[providers.p]
base_url = "http://localhost"
api_key_env = "K"

[models.main]
provider = "p"
model = "m1"

[models.fast]
provider = "p"
model = "m2"
"#;
        let config = crate::config::parse_openslate_toml(toml).unwrap();
        assert_eq!(capability_alias(&config, CAP_MAIN), "main");
        assert_eq!(capability_alias(&config, CAP_COMPACT), "fast");
        assert_eq!(capability_alias(&config, CAP_TITLE), "fast");
        // review 无硬缺省：未绑定时仅落通用 `_` 兜底（ConfigView 展示用），
        // 运行时使用点（mobile P1b）继承会话模型，不消费该值。
        assert_eq!(capability_alias(&config, CAP_REVIEW), "fast");
    }

    #[test]
    fn review_capability_registered() {
        // tool-approval P1a：CAPABILITIES += review（SetCapability 校验 /
        // ConfigView 透出自动放行），有中文标题。
        assert!(CAPABILITIES.contains(&CAP_REVIEW));
        assert_eq!(CAP_REVIEW, "review");
        assert_eq!(capability_title(CAP_REVIEW), "审批审查");
    }

    #[test]
    fn capability_alias_override_and_resolve() {
        // 显式 [capabilities] 覆盖默认；代号可自由命名（vision）。
        let toml = r#"
[providers.p]
base_url = "http://localhost"
api_key_env = "K"

[models.big]
provider = "p"
model = "big-m"

[models.small]
provider = "p"
model = "small-m"

[levels]
main = "big"
fast = "small"
vision = "big"

[capabilities]
main = "vision"
compact = "fast"
title = "fast"
"#;
        let config = crate::config::parse_openslate_toml(toml).unwrap();
        assert_eq!(capability_alias(&config, CAP_MAIN), "vision");
        let resolved = resolve_model(&config, &capability_alias(&config, CAP_MAIN)).unwrap();
        assert_eq!(resolved.model_id, "big-m", "capability main → vision → big");
        assert_eq!(
            resolve_model(&config, &capability_alias(&config, CAP_COMPACT))
                .unwrap()
                .model_id,
            "small-m"
        );
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
                cached_input_tokens: None,
                reasoning_tokens: None,
                image_tokens: None,
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
                cached_input_tokens: None,
                reasoning_tokens: None,
                image_tokens: None,
            }) - 6.0f64)
                .abs()
                < 1e-12
        );
    }
}
