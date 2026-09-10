//! genai-based multi-provider adapter for OpenSlate.
//!
//! Implements [`openslate_core::provider::ModelProvider`] by wrapping the
//! [`genai`](https://crates.io/crates/genai) crate, giving access to Anthropic,
//! Gemini, DeepSeek, OpenRouter, Ollama, Groq, and many other providers behind a
//! single adapter.
//!
//! # Design
//!
//! All `genai` types are kept **internal** to this crate. They never leak into
//! `openslate-core`, so the agent runtime stays provider-agnostic. If `genai` is
//! ever swapped for another library (or self-rolled HTTP), only this crate
//! changes.
//!
//! # Streaming capture flags
//!
//! genai only populates `StreamEnd.captured_content` / `captured_tool_calls` /
//! `captured_usage` when the corresponding `ChatOptions.capture_*` flags are set.
//! [`GenaiProvider`][provider::GenaiProvider] hard-codes these to `true` in its
//! default options; without them the terminal `Done` event would be empty and
//! streaming tool-calling would silently fail.
//!
//! # Network retry / backoff (Phase 2)
//!
//! `generate` / `generate_stream` retry transient failures (HTTP 429, 5xx,
//! network/connect errors) internally, with exponential backoff
//! (`retry_base_ms * 2^(attempt-1) + jitter`, single sleep capped at 10s,
//! total attempts = `max_attempts`). Permanent failures (401/403/400/404,
//! request timeouts, malformed responses) fail fast, and a stream error
//! after the first forwarded `Delta`/`Reasoning` event is never retried
//! (replaying it would duplicate displayed output).
//!
//! **Failure-mode drift**: every attempt is an independent HTTP request with
//! its own per-request timeout (`timeout_secs`, 60s by default), while the
//! total wall clock remains bounded by the runtime's
//! `tokio::time::timeout(remaining)`. A call whose retries burn the
//! remaining budget therefore fails with a run-level `Timeout` where it
//! previously failed immediately with a `ProviderError` — expected and
//! documented behaviour.
//!
//! `Retry-After` headers are not honored: `genai::Error::HttpError` (the
//! streaming error shape) carries no response headers, so a streaming 429
//! cannot honor the header without forking `genai`. See the private `retry`
//! module for details.

pub mod convert;
mod error;
pub mod provider;
mod retry;

pub use error::GenaiBuildError;
pub use provider::{GenaiConfig, GenaiProvider};

// Re-export so downstream wiring (when the `genai` cargo feature is on) can name
// the underlying `genai::adapter::AdapterKind` without depending on `genai`
// directly.
pub use genai;
