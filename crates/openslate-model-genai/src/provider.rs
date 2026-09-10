//! [`GenaiProvider`] — the genai-backed `ModelProvider` implementation.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{ChatOptions, ChatStreamEvent};
use genai::resolver::{AuthData, Endpoint};
use genai::Headers;
use genai::{Client, ModelIden, ServiceTarget};
use tokio::sync::mpsc;
use tracing::warn;

use openslate_core::error::ProviderError;
use openslate_core::provider::{GenerateRequest, ModelProvider};
use openslate_core::types::{ModelResponse, ModelStreamEvent};

use crate::convert::{from_chat_response, from_stream_end, to_chat_request};
use crate::error::{map_error, GenaiBuildError};
use crate::retry::{backoff_or_consumer_gone, exhausted_error, is_retryable, RetryPolicy};

/// `User-Agent` sent with every model request. Stamped twice, deliberately:
/// as a default header on the injected reqwest client (covers the
/// non-streaming `exec_chat` path) AND via `ChatOptions::extra_headers`
/// (merged in both `exec_chat` and `exec_chat_stream`). The double stamp
/// exists because genai 0.6.5's streaming path drops the reqwest client's
/// default headers — without `extra_headers` the UA vanishes from streaming
/// requests on the wire.
const USER_AGENT: &str = concat!("openslate/", env!("CARGO_PKG_VERSION"));

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
    /// Per-request HTTP timeout, in seconds. Every retry attempt is a fresh
    /// HTTP request and gets this timeout independently.
    pub timeout_secs: u64,
    /// Total attempts per call for transient failures (HTTP 429/5xx,
    /// network errors), INCLUDING the first attempt. `0` is clamped to `1`.
    /// Maps to `[providers.X].max_attempts` (default 3).
    pub max_attempts: u32,
    /// Exponential backoff base in milliseconds:
    /// `retry_base_ms * 2^(attempt-1) + jitter`, single sleep capped at 10s.
    /// Maps to `[providers.X].retry_base_ms` (default 500).
    pub retry_base_ms: u64,
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
    /// Transient-failure retry policy (see [`crate::retry`]).
    retry: RetryPolicy,
}

impl GenaiProvider {
    /// Construct a new provider from the given config.
    pub fn new(config: GenaiConfig) -> Result<Self, GenaiBuildError> {
        // Inject a custom reqwest client: genai's default has no timeout, and it
        // honors proxy env vars by default (OpenSlate's OpenAI provider uses
        // `.no_proxy()`). Match that behaviour, add the timeout, and stamp the
        // project User-Agent on every model request.
        let reqwest_client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
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
            // genai's streaming path drops the reqwest client's default
            // headers, so the UA must ALSO travel as an explicit per-request
            // header — `extra_headers` is merged in both exec_chat and
            // exec_chat_stream (genai client_impl).
            extra_headers: Some(Headers::from(("user-agent", USER_AGENT))),
            ..Default::default()
        };

        Ok(Self {
            client,
            model: config.model,
            provider_name: config.provider_name,
            default_chat_options,
            retry: RetryPolicy::new(config.max_attempts, config.retry_base_ms),
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
        let max_attempts = self.retry.max_attempts();

        // Retry loop (Phase 2): total attempts = `max_attempts`, each attempt
        // an independent HTTP request with its own per-request reqwest
        // timeout. NOTE the failure-mode drift: the runtime's outer
        // `tokio::time::timeout(remaining)` still bounds the total wall
        // clock, so a call whose retries burn the remaining budget now fails
        // with a run-level Timeout instead of an immediate ProviderError.
        let mut attempt: u32 = 1;
        loop {
            // The ChatRequest is consumed by exec_chat; rebuild it per
            // attempt from the (Clone) original.
            let chat_req = to_chat_request(&request);
            match self
                .client
                .exec_chat(&self.model, chat_req, Some(&opts))
                .await
            {
                Ok(res) => return Ok(from_chat_response(res)),
                Err(e) => {
                    let retryable = is_retryable(&e);
                    let mapped = map_error(e);
                    if !retryable {
                        // Permanent failure (401/403/400/404, timeout,
                        // malformed response, …) — fail fast, unchanged.
                        return Err(mapped);
                    }
                    if attempt >= max_attempts {
                        // Budget exhausted: report attempt count + last error.
                        return Err(exhausted_error(attempt, mapped));
                    }
                    let delay = self.retry.backoff_delay(attempt);
                    warn!(
                        attempt,
                        max_attempts,
                        delay_ms = delay.as_millis() as u64,
                        error = %mapped,
                        "transient provider error; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn generate_stream(
        &self,
        request: GenerateRequest,
    ) -> mpsc::Receiver<Result<ModelStreamEvent, ProviderError>> {
        let (tx, rx) = mpsc::channel(64);
        let opts = self.chat_options_for(&request);
        let model = self.model.clone();
        let client = self.client.clone();
        let retry = self.retry;

        // The whole attempt loop — request establishment, event polling, and
        // retry/backoff — runs in a spawned task so `generate_stream`
        // returns a receiver immediately (failures surface as the first
        // stream event). Backoff sleeps therefore happen while the runtime's
        // timeout-wrapped receive loop is waiting: retries that consume the
        // remaining wall clock surface as a run-level Timeout (documented
        // failure-mode drift — see crate::retry).
        tokio::spawn(async move {
            let max_attempts = retry.max_attempts();
            let mut attempt: u32 = 1;
            loop {
                let chat_req = to_chat_request(&request);
                let mut stream = match client.exec_chat_stream(&model, chat_req, Some(&opts)).await
                {
                    Ok(sr) => sr.stream,
                    Err(e) => {
                        let retryable = is_retryable(&e);
                        let mapped = map_error(e);
                        let err = if retryable && attempt < max_attempts {
                            None // fall through to the backoff below
                        } else if retryable {
                            Some(exhausted_error(attempt, mapped))
                        } else {
                            Some(mapped)
                        };
                        match err {
                            Some(err) => {
                                let _ = tx.send(Err(err)).await;
                                return;
                            }
                            None => {
                                if backoff_or_consumer_gone(&tx, retry.backoff_delay(attempt)).await
                                {
                                    return; // receiver dropped — stop retrying
                                }
                                attempt += 1;
                                continue;
                            }
                        }
                    }
                };

                // True once the first Delta or Reasoning event has been
                // FORWARDED to the consumer. After that boundary a retry
                // would replay already-displayed output, so any subsequent
                // stream error is terminal (no retry, no duplication).
                // ToolCall chunks are not forwarded live (they only surface
                // in the terminal Done), and Usage/Done end the stream, so
                // Delta/Reasoning is exactly the boundary that matters.
                let mut sent_output = false;
                let mut failure: Option<(bool, ProviderError)> = None;

                while let Some(ev) = stream.next().await {
                    let mapped: Option<Result<ModelStreamEvent, ProviderError>> = match ev {
                        Ok(ChatStreamEvent::Start) => None,
                        Ok(ChatStreamEvent::Chunk(c)) => {
                            sent_output = true;
                            Some(Ok(ModelStreamEvent::Delta(c.content)))
                        }
                        // Stream the model's chain-of-thought (e.g. DeepSeek-style
                        // `reasoning_content`) as Reasoning events so callers can
                        // display it distinctly from the answer.
                        Ok(ChatStreamEvent::ReasoningChunk(c)) => {
                            sent_output = true;
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
                                    return;
                                }
                            }
                            // Always emit Done on a clean End so the runtime loop
                            // terminates promptly.
                            Some(Ok(ModelStreamEvent::Done(response)))
                        }
                        Err(e) => {
                            let retryable = is_retryable(&e);
                            failure = Some((retryable, map_error(e)));
                            break;
                        }
                    };

                    if let Some(event) = mapped {
                        // Cancellation: if the receiver was dropped, stop polling.
                        if tx.send(event).await.is_err() {
                            return;
                        }
                    }
                }

                match failure {
                    None => return, // clean End: Done was forwarded.
                    Some((retryable, mapped)) => {
                        // Retry ONLY transient failures that happened before
                        // any Delta/Reasoning reached the consumer.
                        if retryable && !sent_output && attempt < max_attempts {
                            drop(stream);
                            if backoff_or_consumer_gone(&tx, retry.backoff_delay(attempt)).await {
                                return;
                            }
                            attempt += 1;
                            continue;
                        }
                        let err = if retryable && !sent_output {
                            exhausted_error(attempt, mapped)
                        } else {
                            mapped
                        };
                        let _ = tx.send(Err(err)).await;
                        return;
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
            max_attempts: 3,
            retry_base_ms: 500,
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
            max_attempts: 3,
            retry_base_ms: 500,
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

    #[test]
    fn user_agent_is_project_name_plus_version() {
        assert_eq!(
            USER_AGENT,
            format!("openslate/{}", env!("CARGO_PKG_VERSION"))
        );
    }

    /// E2E: the reqwest client's default User-Agent (`openslate/<version>`) and
    /// the adapter's Bearer auth actually reach the wire through the full genai
    /// stack. Mockito only matches when BOTH headers are present, so a missing
    /// User-Agent makes the request unmatched and `generate` fails.
    #[tokio::test]
    async fn model_request_carries_openslate_user_agent() {
        use openslate_core::provider::GenerateRequest;

        let mut server = mockito::Server::new_async().await;

        let expected_ua = format!("openslate/{}", env!("CARGO_PKG_VERSION"));
        let _mock = server
            .mock("POST", "/chat/completions")
            .match_header("user-agent", mockito::Matcher::Exact(expected_ua))
            .match_header("authorization", "Bearer test-key")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"id":"chatcmpl-1","object":"chat.completion","created":0,
                    "model":"test-model",
                    "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},
                    "finish_reason":"stop"}],
                    "usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}}"#,
            )
            .create_async()
            .await;

        let provider = GenaiProvider::new(GenaiConfig {
            provider_name: "test".into(),
            model: "test-model".into(),
            api_key: Some("test-key".into()),
            base_url: Some(server.url()),
            adapter: Some("openai".into()),
            timeout_secs: 30,
            max_attempts: 3,
            retry_base_ms: 500,
        })
        .expect("constructs");

        let res = provider
            .generate(GenerateRequest {
                model_id: "test-model".into(),
                system_prompt: None,
                messages: vec![],
                tools: vec![],
                max_tokens: None,
                temperature: None,
            })
            .await
            .expect("request matched the mock (UA + Bearer present)");

        assert_eq!(res.content.as_deref(), Some("hi"));

        _mock.assert_async().await;
    }

    // ── Retry / backoff behaviour (Phase 2) ──────────────────────────────

    mod retry_behaviour {
        use super::*;
        use openslate_core::types::ModelStreamEvent;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        fn test_config(base_url: String, max_attempts: u32, retry_base_ms: u64) -> GenaiConfig {
            GenaiConfig {
                provider_name: "test".into(),
                model: "test-model".into(),
                api_key: Some("test-key".into()),
                base_url: Some(base_url),
                adapter: Some("openai".into()),
                timeout_secs: 30,
                max_attempts,
                retry_base_ms,
            }
        }

        fn empty_request() -> GenerateRequest {
            GenerateRequest {
                model_id: "test-model".into(),
                system_prompt: None,
                messages: vec![],
                tools: vec![],
                max_tokens: None,
                temperature: None,
            }
        }

        async fn collect_stream(
            mut rx: tokio::sync::mpsc::Receiver<Result<ModelStreamEvent, ProviderError>>,
        ) -> Vec<Result<ModelStreamEvent, ProviderError>> {
            let mut events = Vec::new();
            while let Some(ev) = rx.recv().await {
                events.push(ev);
            }
            events
        }

        fn ok_completion_body() -> &'static str {
            r#"{"id":"chatcmpl-1","object":"chat.completion","created":0,
                "model":"test-model",
                "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},
                "finish_reason":"stop"}],
                "usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}}"#
        }

        /// A raw HTTP server that accepts connections, counts them, and
        /// handles each in a spawned task so the accept loop (and the
        /// counter) never blocks behind a slow handler.
        async fn spawn_raw_server<F, Fut>(handler: F) -> (String, Arc<AtomicUsize>)
        where
            F: Fn(tokio::net::TcpStream) -> Fut + Send + 'static,
            Fut: std::future::Future<Output = ()> + Send + 'static,
        {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind raw server");
            let addr = listener.local_addr().expect("local addr");
            let counter = Arc::new(AtomicUsize::new(0));
            let counter_for_task = Arc::clone(&counter);
            tokio::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((socket, _)) => {
                            counter_for_task.fetch_add(1, Ordering::SeqCst);
                            let fut = handler(socket);
                            tokio::spawn(fut);
                        }
                        Err(_) => continue,
                    }
                }
            });
            (format!("http://{addr}"), counter)
        }

        /// Drain (part of) an incoming HTTP request so the client's write
        /// side does not stall, then leave the socket to the handler.
        async fn drain_request(socket: &mut tokio::net::TcpStream) {
            let mut buf = vec![0u8; 16 * 1024];
            let _ = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut buf)).await;
        }

        /// 429 → 200: the first attempt is rate-limited, the retry succeeds.
        /// `max_attempts = 3` (total attempts), tiny backoff base.
        #[tokio::test]
        async fn generate_retries_429_then_succeeds() {
            let mut server = mockito::Server::new_async().await;
            let rate_limited = server
                .mock("POST", "/chat/completions")
                .match_body(mockito::Matcher::Any)
                .with_status(429)
                .with_header("content-type", "application/json")
                .with_body(r#"{"error":{"message":"rate limited"}}"#)
                .expect(1)
                .create_async()
                .await;
            let ok = server
                .mock("POST", "/chat/completions")
                .match_body(mockito::Matcher::Any)
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(ok_completion_body())
                .expect(1)
                .create_async()
                .await;

            let provider = GenaiProvider::new(test_config(server.url(), 3, 1)).expect("constructs");

            let res = provider
                .generate(empty_request())
                .await
                .expect("retry after 429 succeeds");

            assert_eq!(res.content.as_deref(), Some("hi"));
            rate_limited.assert_async().await;
            ok.assert_async().await;
        }

        /// 500 → 200: server errors are equally retryable.
        #[tokio::test]
        async fn generate_retries_500_then_succeeds() {
            let mut server = mockito::Server::new_async().await;
            let broken = server
                .mock("POST", "/chat/completions")
                .match_body(mockito::Matcher::Any)
                .with_status(500)
                .with_header("content-type", "application/json")
                .with_body(r#"{"error":{"message":"boom"}}"#)
                .expect(1)
                .create_async()
                .await;
            let ok = server
                .mock("POST", "/chat/completions")
                .match_body(mockito::Matcher::Any)
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(ok_completion_body())
                .expect(1)
                .create_async()
                .await;

            let provider = GenaiProvider::new(test_config(server.url(), 3, 1)).expect("constructs");

            let res = provider
                .generate(empty_request())
                .await
                .expect("retry after 500 succeeds");

            assert_eq!(res.content.as_deref(), Some("hi"));
            broken.assert_async().await;
            ok.assert_async().await;
        }

        /// 401 is permanent: the call fails immediately as AuthError and
        /// exactly ONE request hits the wire (mockito `expect(1)` fails the
        /// test on any second hit).
        #[tokio::test]
        async fn generate_401_fails_immediately_without_retry() {
            let mut server = mockito::Server::new_async().await;
            let unauthorized = server
                .mock("POST", "/chat/completions")
                .match_body(mockito::Matcher::Any)
                .with_status(401)
                .with_header("content-type", "application/json")
                .with_body(r#"{"error":{"message":"bad key"}}"#)
                .expect(1)
                .create_async()
                .await;

            let provider = GenaiProvider::new(test_config(server.url(), 3, 1)).expect("constructs");

            let err = provider
                .generate(empty_request())
                .await
                .expect_err("401 must fail");

            assert!(
                matches!(err, ProviderError::AuthError(_)),
                "expected AuthError, got {err:?}"
            );
            // Exactly one request: a retry would push the hit count past 1
            // and fail this assertion.
            unauthorized.assert_async().await;
        }

        /// Retry budget exhausted: the error message carries the attempt
        /// count and the last error verbatim, and the wire saw exactly
        /// `max_attempts` requests.
        #[tokio::test]
        async fn generate_retry_exhaustion_reports_attempt_count() {
            let mut server = mockito::Server::new_async().await;
            let always_429 = server
                .mock("POST", "/chat/completions")
                .match_body(mockito::Matcher::Any)
                .with_status(429)
                .with_header("content-type", "application/json")
                .with_body(r#"{"error":{"message":"rate limited"}}"#)
                .expect(3)
                .create_async()
                .await;

            let provider = GenaiProvider::new(test_config(server.url(), 3, 1)).expect("constructs");

            let err = provider
                .generate(empty_request())
                .await
                .expect_err("sustained 429 must fail");

            let msg = err.to_string();
            assert!(msg.contains("after 3 attempt(s)"), "msg: {msg}");
            assert!(msg.contains("rate limit exceeded"), "msg: {msg}");
            always_429.assert_async().await;
        }

        /// A per-request timeout (reqwest-level, one attempt = one full
        /// `timeout_secs` burn) is NOT retried: the error is
        /// `ProviderError::Timeout` and the server saw exactly one
        /// connection.
        #[tokio::test]
        async fn generate_timeout_is_not_retried() {
            let (base_url, connections) = spawn_raw_server(|mut socket| async move {
                drain_request(&mut socket).await;
                // Hold the connection open past the client's 1s per-request
                // timeout, then drop without responding.
                tokio::time::sleep(Duration::from_millis(1500)).await;
            })
            .await;

            let mut cfg = test_config(base_url, 3, 1);
            cfg.timeout_secs = 1; // per-request timeout shorter than the hold
            let provider = GenaiProvider::new(cfg).expect("constructs");

            let err = provider
                .generate(empty_request())
                .await
                .expect_err("hanging server must time out");

            assert!(
                matches!(err, ProviderError::Timeout),
                "expected Timeout, got {err:?}"
            );

            // Grace period: a (buggy) retry would open a second connection
            // within milliseconds given the 1ms backoff base.
            tokio::time::sleep(Duration::from_millis(250)).await;
            assert_eq!(
                connections.load(Ordering::SeqCst),
                1,
                "timeout must not be retried"
            );
        }

        /// Streaming: a 429 that arrives BEFORE any output is retried; the
        /// second attempt streams normally.
        #[tokio::test]
        async fn stream_retries_429_before_first_delta_and_succeeds() {
            let mut server = mockito::Server::new_async().await;
            let rate_limited = server
                .mock("POST", "/chat/completions")
                .match_body(mockito::Matcher::Any)
                .with_status(429)
                .with_header("content-type", "application/json")
                .with_body(r#"{"error":{"message":"rate limited"}}"#)
                .expect(1)
                .create_async()
                .await;
            let streamed = server
                .mock("POST", "/chat/completions")
                .match_body(mockito::Matcher::Any)
                .with_status(200)
                .with_header("content-type", "text/event-stream")
                .with_body(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n\
                     data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\n\
                     data: [DONE]\n\n",
                )
                .expect(1)
                .create_async()
                .await;

            let provider = GenaiProvider::new(test_config(server.url(), 3, 1)).expect("constructs");

            let rx = provider.generate_stream(empty_request()).await;
            let events = collect_stream(rx).await;

            let deltas: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    Ok(ModelStreamEvent::Delta(t)) => Some(t.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(deltas, vec!["Hi".to_string()], "events: {events:?}");
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, Ok(ModelStreamEvent::Done(_)))),
                "expected a Done event, got: {events:?}"
            );
            assert!(
                events.iter().all(|e| e.is_ok()),
                "no error events expected, got: {events:?}"
            );
            rate_limited.assert_async().await;
            streamed.assert_async().await;
        }

        /// A raw SSE server that streams one event, then truncates the
        /// chunked body mid-chunk so the client sees a mid-stream transport
        /// error (retryable class). Used to pin the no-retry-after-output
        /// boundary.
        async fn spawn_truncating_sse_server(first_event_json: &str) -> (String, Arc<AtomicUsize>) {
            let first_event = format!("data: {first_event_json}\n\n");
            spawn_raw_server(move |mut socket| {
                let first_event = first_event.clone();
                async move {
                    drain_request(&mut socket).await;
                    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
                    let chunk = format!("{:x}\r\n{}\r\n", first_event.len(), first_event);
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(chunk.as_bytes()).await;
                    // Announce a chunk we will never finish: hyper surfaces a
                    // decode error mid-body (transient-error class).
                    let _ = socket.write_all(b"20\r\npartial").await;
                    let _ = socket.shutdown().await;
                }
            })
            .await
        }

        /// Streaming: a transport error AFTER the first Delta must NOT be
        /// retried — the consumer sees exactly one Delta, one error, and the
        /// server saw exactly one connection (no duplicated output).
        #[tokio::test]
        async fn stream_does_not_retry_after_first_delta() {
            let (base_url, connections) =
                spawn_truncating_sse_server(r#"{"choices":[{"delta":{"content":"Hi"}}]}"#).await;

            let provider = GenaiProvider::new(test_config(base_url, 3, 1)).expect("constructs");

            let rx = provider.generate_stream(empty_request()).await;
            let events = collect_stream(rx).await;

            let deltas: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    Ok(ModelStreamEvent::Delta(t)) => Some(t.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(deltas, vec!["Hi".to_string()], "events: {events:?}");
            assert!(
                events.iter().any(|e| e.is_err()),
                "expected the transport error to surface, got: {events:?}"
            );
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, Err(ProviderError::ConnectionError(_)))),
                "expected ConnectionError, got: {events:?}"
            );

            tokio::time::sleep(Duration::from_millis(250)).await;
            assert_eq!(
                connections.load(Ordering::SeqCst),
                1,
                "no retry after the first forwarded Delta"
            );
        }

        /// Same boundary, Reasoning side: an error after the first
        /// Reasoning chunk is terminal too (retrying would replay displayed
        /// chain-of-thought).
        #[tokio::test]
        async fn stream_does_not_retry_after_first_reasoning() {
            let (base_url, connections) = spawn_truncating_sse_server(
                r#"{"choices":[{"delta":{"reasoning_content":"pondering"}}]}"#,
            )
            .await;

            let provider = GenaiProvider::new(test_config(base_url, 3, 1)).expect("constructs");

            let rx = provider.generate_stream(empty_request()).await;
            let events = collect_stream(rx).await;

            let reasoning: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    Ok(ModelStreamEvent::Reasoning(t)) => Some(t.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                reasoning,
                vec!["pondering".to_string()],
                "events: {events:?}"
            );
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, Err(ProviderError::ConnectionError(_)))),
                "expected the transport error to surface, got: {events:?}"
            );

            tokio::time::sleep(Duration::from_millis(250)).await;
            assert_eq!(
                connections.load(Ordering::SeqCst),
                1,
                "no retry after the first forwarded Reasoning"
            );
        }
    }
}
