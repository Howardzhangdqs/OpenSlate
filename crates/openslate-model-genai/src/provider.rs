//! [`GenaiProvider`] — the genai-backed `ModelProvider` implementation.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{ChatOptions, ChatStreamEvent};
use genai::resolver::{AuthData, Endpoint};
use genai::{Client, ModelIden, ServiceTarget};
use tokio::sync::mpsc;
use tracing::warn;

use openslate_core::error::ProviderError;
use openslate_core::provider::{GenerateRequest, ModelProvider};
use openslate_core::types::{ModelResponse, ModelStreamEvent};

use crate::convert::{from_chat_response, from_stream_end, to_chat_request};
use crate::error::{map_error, GenaiBuildError};

/// Configuration for constructing a [`GenaiProvider`].
#[derive(Debug, Clone)]
pub struct GenaiConfig {
    /// Display name for this provider (e.g. `"anthropic"`, `"gemini"`).
    pub provider_name: String,
    /// Model identifier passed to genai (e.g. `"claude-sonnet-4-5"`).
    pub model: String,
    /// Resolved API key value, if any. When `None`, genai falls back to its own
    /// env-var resolution.
    pub api_key: Option<String>,
    /// Optional endpoint override (rarely needed for native providers; useful for
    /// proxies / gateways).
    pub base_url: Option<String>,
    /// genai adapter protocol (e.g. `"anthropic"`, `"gemini"`, `"openai"`,
    /// `"ollama"`). If `None`, genai infers the protocol from the model name —
    /// but unknown prefixes silently fall through to Ollama, so an explicit
    /// `adapter` is strongly recommended.
    pub adapter: Option<String>,
    /// Per-request HTTP timeout, in seconds.
    pub timeout_secs: u64,
}

/// A genai-backed implementation of [`ModelProvider`].
///
/// All `genai` types are contained within this struct; none are exposed through
/// the `ModelProvider` trait surface.
pub struct GenaiProvider {
    client: Client,
    model: String,
    provider_name: String,
    /// Default options for every call. The `capture_*` flags MUST remain
    /// `Some(true)` — without them the streaming `Done` event is empty.
    default_chat_options: ChatOptions,
}

impl GenaiProvider {
    /// Construct a new provider from the given config.
    pub fn new(config: GenaiConfig) -> Result<Self, GenaiBuildError> {
        // Inject a custom reqwest client: genai's default has no timeout, and it
        // honors proxy env vars by default (OpenSlate's OpenAI provider uses
        // `.no_proxy()`). Match that behaviour and add the timeout.
        let reqwest_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .no_proxy()
            .build()
            .map_err(|e| GenaiBuildError::ReqwestBuild(e.to_string()))?;

        let mut builder = Client::builder().with_reqwest(reqwest_client);

        // Explicit adapter binding avoids genai's silent Ollama fallthrough for
        // unknown model-name prefixes.
        if let Some(adapter_str) = &config.adapter {
            let kind = AdapterKind::from_lower_str(adapter_str)
                .ok_or_else(|| GenaiBuildError::UnknownAdapter(adapter_str.clone()))?;
            builder = builder.with_adapter_kind(kind);
        } else {
            warn!(
                model = %config.model,
                "GenaiProvider created without an explicit `adapter`; genai will infer the \
                 protocol from the model name. Unknown prefixes silently fall through to \
                 Ollama — set `adapter` explicitly to avoid misrouting."
            );
        }

        // Auth: provide the resolved key via a resolver closure. The closure is
        // Clone (it only clones the captured String internally) and Send+Sync.
        if let Some(api_key) = config.api_key.clone() {
            builder = builder.with_auth_resolver_fn(move |_iden: ModelIden| {
                Ok(Some(AuthData::from_single(api_key.clone())))
            });
        }

        // Optional endpoint override.
        //
        // NOTE: genai builds service URLs with `reqwest::Url::join(suffix)`. Per
        // RFC 3986, joining ".../v1" + "chat/completions" REPLACES "v1" (yielding
        // ".../chat/completions" — wrong), whereas ".../v1/" appends correctly.
        // genai's own default OpenAI endpoint ends with "/". Normalize here so a
        // base_url like "https://host/api/v1" works correctly with Url::join.
        if let Some(base_url) = config.base_url.clone() {
            builder = builder.with_service_target_resolver_fn(
                move |mut target: ServiceTarget| -> genai::resolver::Result<ServiceTarget> {
                    let mut url = base_url.clone();
                    if !url.ends_with('/') {
                        url.push('/');
                    }
                    target.endpoint = Endpoint::from_owned(url);
                    Ok(target)
                },
            );
        }

        let client = builder.build();

        let default_chat_options = ChatOptions {
            capture_content: Some(true),
            capture_tool_calls: Some(true),
            capture_usage: Some(true),
            ..Default::default()
        };

        Ok(Self {
            client,
            model: config.model,
            provider_name: config.provider_name,
            default_chat_options,
        })
    }

    /// Build per-call options, layering the request's max_tokens/temperature on
    /// top of the capture-flag defaults.
    fn chat_options_for(&self, req: &GenerateRequest) -> ChatOptions {
        let mut opts = self.default_chat_options.clone();
        opts.max_tokens = req.max_tokens;
        opts.temperature = req.temperature.map(|t| t as f64);
        opts
    }
}

#[async_trait]
impl ModelProvider for GenaiProvider {
    async fn generate(&self, request: GenerateRequest) -> Result<ModelResponse, ProviderError> {
        let opts = self.chat_options_for(&request);
        let chat_req = to_chat_request(&request);

        let res = self
            .client
            .exec_chat(&self.model, chat_req, Some(&opts))
            .await
            .map_err(map_error)?;

        Ok(from_chat_response(res))
    }

    async fn generate_stream(
        &self,
        request: GenerateRequest,
    ) -> mpsc::Receiver<Result<ModelStreamEvent, ProviderError>> {
        let (tx, rx) = mpsc::channel(64);
        let opts = self.chat_options_for(&request);
        let chat_req = to_chat_request(&request);
        let model = self.model.clone();

        // Establish the connection here (so an immediate failure surfaces as the
        // first stream event). The actual event polling happens in a spawned
        // task so the receiver can be returned promptly.
        let stream_res = self
            .client
            .exec_chat_stream(&model, chat_req, Some(&opts))
            .await;

        tokio::spawn(async move {
            let mut stream = match stream_res {
                Ok(sr) => sr.stream,
                Err(e) => {
                    let _ = tx.send(Err(map_error(e))).await;
                    return;
                }
            };

            while let Some(ev) = stream.next().await {
                let mapped: Option<Result<ModelStreamEvent, ProviderError>> = match ev {
                    Ok(ChatStreamEvent::Start) => None,
                    Ok(ChatStreamEvent::Chunk(c)) => {
                        Some(Ok(ModelStreamEvent::Delta(c.content)))
                    }
                    // Stream the model's chain-of-thought (e.g. DeepSeek-style
                    // `reasoning_content`) as Reasoning events so callers can
                    // display it distinctly from the answer.
                    Ok(ChatStreamEvent::ReasoningChunk(c)) => {
                        Some(Ok(ModelStreamEvent::Reasoning(c.content)))
                    }
                    // ThoughtSignature chunks (e.g. Gemini) have no OpenSlate
                    // equivalent yet — drop for v1.
                    Ok(ChatStreamEvent::ThoughtSignatureChunk(_)) => None,
                    Ok(ChatStreamEvent::ToolCallChunk(_)) => None,
                    Ok(ChatStreamEvent::End(end)) => {
                        let (usage_event, response) = from_stream_end(end);
                        if let Some(u) = usage_event {
                            if tx.send(Ok(ModelStreamEvent::Usage(u))).await.is_err() {
                                // Receiver dropped (e.g. runtime timeout) → cancel.
                                break;
                            }
                        }
                        // Always emit Done on a clean End so the runtime loop
                        // terminates promptly.
                        Some(Ok(ModelStreamEvent::Done(response)))
                    }
                    Err(e) => Some(Err(map_error(e))),
                };

                if let Some(event) = mapped {
                    // Cancellation: if the receiver was dropped, stop polling.
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
            }
        });

        rx
    }

    fn provider_name(&self) -> &str {
        &self.provider_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_chat_options_have_capture_flags() {
        // G2 guard: without these flags the streaming Done event is empty.
        let cfg = GenaiConfig {
            provider_name: "test".into(),
            model: "claude-sonnet-4-5".into(),
            api_key: Some("k".into()),
            base_url: None,
            adapter: Some("anthropic".into()),
            timeout_secs: 60,
        };
        let provider = GenaiProvider::new(cfg).expect("constructs");
        assert_eq!(provider.default_chat_options.capture_content, Some(true));
        assert_eq!(provider.default_chat_options.capture_tool_calls, Some(true));
        assert_eq!(provider.default_chat_options.capture_usage, Some(true));
    }

    #[test]
    fn unknown_adapter_is_rejected() {
        let cfg = GenaiConfig {
            provider_name: "test".into(),
            model: "m".into(),
            api_key: None,
            base_url: None,
            adapter: Some("not-a-real-adapter".into()),
            timeout_secs: 60,
        };
        match GenaiProvider::new(cfg) {
            Ok(_) => panic!("expected an error for an unknown adapter"),
            Err(err) => {
                assert!(
                    matches!(err, GenaiBuildError::UnknownAdapter(_)),
                    "expected UnknownAdapter, got {err:?}"
                );
                assert!(err.to_string().contains("not-a-real-adapter"));
            }
        }
    }
}
