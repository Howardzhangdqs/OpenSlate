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
//! **Failure-mode drift**: every attempt is an independent HTTP request,
//! and the total wall clock remains bounded by the runtime's
//! `tokio::time::timeout(remaining)`. A call whose retries burn the
//! remaining budget therefore fails with a run-level `Timeout` where it
//! previously failed immediately with a `ProviderError` — expected and
//! documented behaviour.
//!
//! # Timeout semantics (fix-20)
//!
//! `GenaiConfig::timeout_ms` (from `[limits].timeout_ms`, 60s by default)
//! has DUAL semantics:
//!
//! - **Non-streaming** (`generate`): a TOTAL per-attempt budget — the whole
//!   request/response must complete within it. Enforced by a
//!   `tokio::time::timeout` wrapper around each `exec_chat` attempt; the
//!   underlying reqwest client carries NO total timeout.
//! - **Streaming** (`generate_stream`): an IDLE budget — the maximum
//!   allowed silence while waiting for response headers or between stream
//!   events. Tokens flowing keep the stream healthy however long the total
//!   duration; only a stall beyond the budget times out (error message:
//!   `stream idle timeout after Xms`). Idle timeouts are terminal, never
//!   retried.
//!
//! Connection establishment on both paths is bounded by a fixed 15s
//! `connect_timeout`. The client-level total timeout was REMOVED because
//! reqwest applies it to the entire response body — it killed long
//! streaming answers mid-flight.
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
