use crate::chat::{CompletionTokensDetails, PromptTokensDetails, Usage};
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, skip_serializing_none};

/// Normalized token usage across providers (OpenAI-compatible).
///
/// - Deserialization treats 0 as None for cross-provider consistency. OpenAI often returns 0 for non-applicable counters.
///
/// - `prompt_tokens` and `output_tokens` are the total input/output tokens. The corresponding `*_details` carry provider-specific breakdowns.
///
/// - Gemini: `candidatesTokenCount` excludes "thoughts" (reasoning) tokens. We normalize:
///   `output_tokens = candidatesTokenCount + thoughts_token_count`, and
///   `output_tokens_details.reasoning_tokens = thoughts_token_count`.
#[serde_as]
#[skip_serializing_none]
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct RespUsage {
	/// Total input tokens (formerly `input_tokens`).
	pub input_tokens: Option<i32>,
	pub input_tokens_details: Option<InputTokensDetails>,

	/// Total output (completion) tokens.
	pub output_tokens: Option<i32>,
	pub output_tokens_details: Option<OutputTokensDetails>,

	/// Total tokens as reported by the API, or computed as prompt + completion
	/// (including cache read/creation tokens when applicable).
	pub total_tokens: Option<i32>,
}

impl RespUsage {
	/// Remove detail objects that contain only `None` fields.
	pub fn compact_details(&mut self) {
		if self.input_tokens_details.as_ref().is_some_and(|d| d.is_empty()) {
			self.input_tokens_details = None;
		}

		if self.output_tokens_details.as_ref().is_some_and(|d| d.is_empty()) {
			self.output_tokens_details = None;
		}
	}
}

#[serde_as]
#[skip_serializing_none]
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct InputTokensDetails {
	/// Anthropic: `cache_creation_input_tokens`.
	/// Tokens used to build the cache (not yet cached). These may incur a small surcharge; subsequent requests benefit via `cached_tokens`.
	#[serde(default, deserialize_with = "crate::support::zero_as_none")]
	pub cache_creation_tokens: Option<i32>,
	/// Anthropic: `cache_read_input_tokens`.
	#[serde(default, deserialize_with = "crate::support::zero_as_none")]
	pub cached_tokens: Option<i32>,
	#[serde(default, deserialize_with = "crate::support::zero_as_none")]
	pub audio_tokens: Option<i32>,
	/// OpenAI Responses API: `input_tokens_details.image_tokens`
	/// (VENDOR PATCH #3, OpenSlate — lifted to `Usage::image_input_tokens`).
	#[serde(default, deserialize_with = "crate::support::zero_as_none")]
	pub image_tokens: Option<i32>,
}

impl InputTokensDetails {
	/// True if all fields are `None`.
	pub fn is_empty(&self) -> bool {
		self.cache_creation_tokens.is_none()
			&& self.cached_tokens.is_none()
			&& self.audio_tokens.is_none()
			&& self.image_tokens.is_none()
	}
}

#[serde_as]
#[skip_serializing_none]
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct OutputTokensDetails {
	#[serde(default, deserialize_with = "crate::support::zero_as_none")]
	pub accepted_prediction_tokens: Option<i32>,
	#[serde(default, deserialize_with = "crate::support::zero_as_none")]
	pub rejected_prediction_tokens: Option<i32>,
	#[serde(default, deserialize_with = "crate::support::zero_as_none")]
	pub reasoning_tokens: Option<i32>,
	#[serde(default, deserialize_with = "crate::support::zero_as_none")]
	pub audio_tokens: Option<i32>,
}

impl OutputTokensDetails {
	/// True if all fields are `None`.
	pub fn is_empty(&self) -> bool {
		self.accepted_prediction_tokens.is_none()
			&& self.rejected_prediction_tokens.is_none()
			&& self.reasoning_tokens.is_none()
			&& self.audio_tokens.is_none()
	}
}

// region:    --- Intos

impl From<InputTokensDetails> for PromptTokensDetails {
	fn from(value: InputTokensDetails) -> Self {
		PromptTokensDetails {
			cache_creation_tokens: value.cache_creation_tokens,
			cache_creation_details: None,
			cached_tokens: value.cached_tokens,
			audio_tokens: value.audio_tokens,
		}
	}
}

impl From<OutputTokensDetails> for CompletionTokensDetails {
	fn from(value: OutputTokensDetails) -> Self {
		CompletionTokensDetails {
			accepted_prediction_tokens: value.accepted_prediction_tokens,
			rejected_prediction_tokens: value.rejected_prediction_tokens,
			reasoning_tokens: value.reasoning_tokens,
			audio_tokens: value.audio_tokens,
		}
	}
}

impl From<RespUsage> for Usage {
	fn from(value: RespUsage) -> Self {
		// VENDOR PATCH #3 (OpenSlate): Responses API per-modality input
		// breakdown lifted to the normalized top-level counter (before the
		// details are moved into prompt_tokens_details).
		let image_input_tokens = value
			.input_tokens_details
			.as_ref()
			.and_then(|d| d.image_tokens)
			.filter(|v| *v > 0)
			.map(|v| v as u64);
		Usage {
			prompt_tokens: value.input_tokens,
			prompt_tokens_details: value.input_tokens_details.map(Into::into),
			completion_tokens: value.output_tokens,
			completion_tokens_details: value.output_tokens_details.map(Into::into),
			total_tokens: value.total_tokens,
			image_input_tokens,
		}
	}
}

// endregion: --- Intos

// region:    --- Tests (VENDOR PATCH #3, OpenSlate) ---------------------------

#[cfg(test)]
mod tests {
	use super::*;

	/// `input_tokens_details.image_tokens` deserializes and lifts to the
	/// normalized `Usage::image_input_tokens`; absent / 0 variants stay None.
	#[test]
	fn test_resp_usage_image_tokens_lift() {
		let resp: RespUsage = serde_json::from_value(serde_json::json!({
			"input_tokens": 1200,
			"output_tokens": 30,
			"total_tokens": 1230,
			"input_tokens_details": { "cached_tokens": 64, "image_tokens": 728 }
		}))
		.unwrap();
		let usage = Usage::from(resp);
		assert_eq!(usage.image_input_tokens, Some(728));
		assert_eq!(usage.prompt_tokens, Some(1200));

		let resp: RespUsage = serde_json::from_value(serde_json::json!({
			"input_tokens": 100,
			"output_tokens": 5,
			"total_tokens": 105
		}))
		.unwrap();
		assert_eq!(Usage::from(resp).image_input_tokens, None);

		let resp: RespUsage = serde_json::from_value(serde_json::json!({
			"input_tokens_details": { "image_tokens": 0 }
		}))
		.unwrap();
		assert_eq!(Usage::from(resp).image_input_tokens, None);
	}
}

// endregion: --- Tests (VENDOR PATCH #3, OpenSlate) --------------------------
