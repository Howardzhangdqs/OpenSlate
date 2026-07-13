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

pub mod convert;
mod error;
pub mod provider;

pub use error::GenaiBuildError;
pub use provider::{GenaiConfig, GenaiProvider};

// Re-export so downstream wiring (when the `genai` cargo feature is on) can name
// the underlying `genai::adapter::AdapterKind` without depending on `genai`
// directly.
pub use genai;
