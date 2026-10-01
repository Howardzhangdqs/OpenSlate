//! Provider construction — builds the LLM provider for a model alias.
//!
//! Extracted verbatim from `openslate-cli`'s `cmd::run` module (P2a, no
//! behavior change): the CLI rebuilds the provider per run/turn, and the
//! TUI will reuse the same construction for `/model` switches.

use anyhow::{Context, Result};
use openslate_core::provider::ModelProvider;

/// Build a provider for a specific model alias.
///
/// All providers are routed through the genai adapter — the sole provider
/// implementation. `ProviderConfig.kind` is retained as an informational hint
/// (e.g. `"openai_compatible"`, `"genai"`) but no longer selects an
/// implementation. The genai `adapter` protocol (openai/anthropic/gemini/ollama)
/// is taken from `ProviderConfig.adapter`, defaulting to `"openai"` when unset
/// (the common case for OpenAI-compatible endpoints) to avoid genai's silent
/// Ollama fallthrough for unrecognized model names.
pub fn build_provider_for_model(
    config: &openslate_core::config::OpenSlateConfig,
    model_alias: &str,
) -> Result<Box<dyn ModelProvider>> {
    // Desktop 语义：key 从环境变量读取（.env / shell export）。
    build_genai_provider(config, model_alias, None)
}

/// [`build_provider_for_model`] 的 key 注入版（mobile secret provider 接缝，
/// PLAN §18）：`api_key = None` 保持 desktop 环境变量语义；`Some(key)` 直接
/// 使用注入值（Android Keystore → FFI set_api_key → 内存 map）。
pub fn build_genai_provider(
    config: &openslate_core::config::OpenSlateConfig,
    model_alias: &str,
    api_key: Option<String>,
) -> Result<Box<dyn ModelProvider>> {
    let resolved = openslate_core::model_config::resolve_model(config, model_alias)
        .with_context(|| format!("Failed to resolve model alias '{}'", model_alias))?;

    let api_key = match api_key {
        Some(k) => k,
        None => std::env::var(&resolved.provider.api_key_env).with_context(|| {
            format!(
                "API key not found: set environment variable '{}'",
                resolved.provider.api_key_env
            )
        })?,
    };

    // Default to the OpenAI adapter when unset: most OpenAI-compatible
    // providers (zhipu, minimax, internlm, …) don't set `adapter` explicitly,
    // and genai would otherwise infer Ollama from the model name.
    let adapter = resolved
        .provider
        .adapter
        .clone()
        .or_else(|| Some("openai".to_owned()));

    // Provider HTTP timeout (fix-20 dual semantics): passes the effective
    // `[limits].timeout_ms` through in milliseconds. Non-streaming requests
    // treat it as a TOTAL per-attempt budget; streaming requests treat it
    // as an IDLE budget (max silence between stream events — tokens flowing
    // keep a long stream alive). A missing [limits] section (or a degenerate
    // 0) falls back to 60s. `u64::MAX` = 无总时长预算（mobile thinking
    // 语义）——provider 层仍需有限预算：流式=空闲 60s / 非流式=单次 60s。
    let timeout_ms = match config.limits.as_ref().map(|l| l.timeout_ms) {
        Some(ms) if ms == u64::MAX => 60_000,
        Some(ms) if ms > 0 => ms,
        _ => 60_000,
    };

    let cfg = openslate_model_genai::GenaiConfig {
        provider_name: resolved.provider_name.clone(),
        model: resolved.model_id.clone(),
        api_key: Some(api_key),
        base_url: Some(resolved.provider.base_url.clone()),
        // 受限网络的出站代理（mobile 宿主经 adb reverse 共享电脑代理；
        // desktop 无此 env，行为不变）。
        proxy: std::env::var("OPENSLATE_HTTP_PROXY").ok().filter(|u| !u.is_empty()),
        adapter,
        timeout_ms,
        max_attempts: resolved.provider.max_attempts,
        retry_base_ms: resolved.provider.retry_base_ms,
    };

    let provider = openslate_model_genai::GenaiProvider::new(cfg).map_err(|e| {
        anyhow::anyhow!(
            "Failed to build genai provider for '{}': {}",
            resolved.provider_name,
            e
        )
    })?;

    Ok(Box::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Create a temp dir with valid config + agents for testing.
    fn temp_project() -> TempDir {
        let tmp = TempDir::new().expect("create temp dir");
        let openslate_dir = tmp.path().join(".openslate");
        std::fs::create_dir(&openslate_dir).expect("create .openslate dir");

        let toml = r#"
[providers.zhipu]
base_url = "https://example.com"
api_key_env = "TEST_API_KEY"

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
        let agents_dir = openslate_dir.join("agents");
        std::fs::create_dir(&agents_dir).expect("create agents dir");
        let agent_md = "---\nid: root\nname: Root Agent\nmodel: main\ntools:\n  - read_file\n---\nYou are the root agent.\n";
        std::fs::write(agents_dir.join("root.md"), agent_md).expect("write root.md");
        tmp
    }

    #[test]
    fn test_build_provider_for_model_missing_env_var() {
        let tmp = temp_project();
        let config =
            crate::wiring::load_config(&tmp.path().join(".openslate/openslate.toml")).unwrap();
        match build_provider_for_model(&config, "main") {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("TEST_API_KEY"),
                    "error should mention env var name: {msg}"
                );
            }
            Ok(_) => panic!("expected error when env var is not set"),
        }
    }

    /// A genai-backed provider config must construct a `GenaiProvider` successfully.
    #[test]
    fn test_genai_provider_constructs_with_feature() {
        // Unique env var name to avoid races with parallel tests.
        // SAFETY of env mutation: this var is not read by any other test.
        std::env::set_var("GENAI_TEST_KEY", "sk-test");
        let toml = r#"
[providers.anthropic_prod]
base_url = "https://api.anthropic.com"
api_key_env = "GENAI_TEST_KEY"
adapter = "anthropic"

[models.main]
provider = "anthropic_prod"
model = "claude-sonnet-4-5"
"#;
        let config = openslate_core::config::parse_openslate_toml(toml).unwrap();
        let provider = build_provider_for_model(&config, "main").expect("genai provider builds");
        assert_eq!(provider.provider_name(), "anthropic_prod");
    }
}
