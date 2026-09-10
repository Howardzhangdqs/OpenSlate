//! Retry / exponential-backoff policy for transient provider failures.
//!
//! # Semantics (Phase 2 network-resilience design)
//!
//! - Retries live INSIDE `GenaiProvider::generate` / `generate_stream`.
//! - Every attempt is an independent HTTP request with its own per-request
//!   reqwest timeout (`GenaiConfig::timeout_secs`, 60s by default). Total
//!   wall clock is still bounded by the runtime's outer
//!   `tokio::time::timeout(remaining)`: a call whose retries burn the
//!   remaining budget now fails with a run-level `Timeout` instead of an
//!   immediate `ProviderError` — a deliberate, documented failure-mode
//!   drift from the pre-retry behaviour.
//! - **Retryable**: HTTP 429, HTTP 5xx, transient network/connect errors
//!   (including `genai::Error::WebStream` transport failures that occur
//!   before any output has been forwarded).
//! - **Not retryable**: permanent 4xx (401/403/400/404…), request timeouts
//!   (a single attempt already spent the full per-request timeout), and any
//!   stream error observed AFTER the first `Delta` or `Reasoning` event has
//!   been forwarded to the consumer — retrying then would replay
//!   already-displayed UI output. Errors after `Usage`/`Done` are moot (the
//!   stream is terminal by then).
//! - Backoff: `retry_base_ms * 2^(attempt-1) + jitter` for the 1-based
//!   number of the attempt that just failed; a single sleep is capped at
//!   10s. `max_attempts` is the TOTAL number of attempts, including the
//!   first (`0` is clamped to `1`).
//!
//! # Retry-After is NOT honored
//!
//! `genai::Error::HttpError` — the error shape produced by the SSE stream
//! parser, i.e. the only shape that carries mid-call 429s — exposes only the
//! status, canonical reason, and body: no response headers. The
//! non-streaming `webc::Error::ResponseFailedStatus` does retain a
//! `HeaderMap`, but plumbing it out would fork the two paths' retry
//! behaviour; without forking `genai` (out of scope) a streaming 429 can
//! never honor `Retry-After`, so neither path does. Plain exponential
//! backoff is the accepted trade-off.

use std::time::Duration;

use openslate_core::error::ProviderError;

/// Longest single backoff sleep, regardless of the computed exponential
/// delay.
const MAX_BACKOFF_MS: u64 = 10_000;

/// Retry configuration carried by [`crate::GenaiProvider`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetryPolicy {
    /// Total attempts per call, including the first. Comes from
    /// `[providers.X].max_attempts` (default 3). `0` is clamped to `1`.
    max_attempts: u32,
    /// Exponential backoff base in milliseconds
    /// (`retry_base_ms * 2^(attempt-1) + jitter`).
    retry_base_ms: u64,
}

impl RetryPolicy {
    pub(crate) fn new(max_attempts: u32, retry_base_ms: u64) -> Self {
        Self {
            max_attempts,
            retry_base_ms,
        }
    }

    /// Total number of attempts, at least one.
    pub(crate) fn max_attempts(&self) -> u32 {
        self.max_attempts.max(1)
    }

    /// Backoff delay after the `failed_attempt`-th (1-based) failed attempt:
    /// `retry_base_ms * 2^(failed_attempt-1) + jitter`, capped at 10s.
    pub(crate) fn backoff_delay(&self, failed_attempt: u32) -> Duration {
        // Cap the shift well below the u64 bit width so the exponent cannot
        // panic; the 10s sleep cap makes larger exponents moot anyway.
        let exp = self.retry_base_ms.saturating_mul(
            1u64.checked_shl(failed_attempt.saturating_sub(1).min(32))
                .unwrap_or(u64::MAX),
        );
        let delay_ms = exp.saturating_add(jitter_ms(self.retry_base_ms));
        Duration::from_millis(delay_ms.min(MAX_BACKOFF_MS))
    }
}

/// Should a call that failed with this `genai::Error` be retried?
///
/// Classification happens on the original `genai::Error` (before
/// [`crate::error::map_error`] consumes it) so that, e.g., a generic
/// `ConnectionError` stemming from deterministic input validation is not
/// mistaken for a transient network failure.
pub(crate) fn is_retryable(e: &genai::Error) -> bool {
    match e {
        // Non-streaming / adapter-level HTTP failures.
        genai::Error::WebModelCall { webc_error, .. }
        | genai::Error::WebAdapterCall { webc_error, .. } => match webc_error {
            genai::webc::Error::ResponseFailedStatus { status, .. } => is_retryable_status(*status),
            // Reqwest transport failure: transient (connect reset, TLS
            // hiccup, …) unless it is the per-request timeout — a timeout
            // already consumed the full request budget, so retrying would
            // only multiply the wait (and the runtime wall-clock guard
            // would turn it into a run Timeout).
            genai::webc::Error::Reqwest(re) => !re.is_timeout(),
            _ => false,
        },

        // Mid-stream HTTP error event (constructed by the SSE parser from a
        // non-2xx response status; arrives before any content event).
        genai::Error::HttpError { status, .. } => is_retryable_status(*status),

        // Stream-level transport error. The caller enforces the
        // "no retry after the first forwarded Delta/Reasoning" boundary;
        // here we only decide whether the error class is transient.
        genai::Error::WebStream { error, .. } => {
            if let Some(genai::Error::HttpError { status, .. }) =
                error.downcast_ref::<genai::Error>()
            {
                return is_retryable_status(*status);
            }
            if let Some(re) = error.downcast_ref::<reqwest::Error>() {
                return !re.is_timeout();
            }
            // Unidentified transport failure mid-stream: assume transient
            // (the before-first-output boundary still protects the UI).
            true
        }

        // Auth, malformed responses, input validation, resolver errors, …
        // are all deterministic — never retry.
        _ => false,
    }
}

fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status.as_u16() == 429 || status.is_server_error()
}

/// Build the error returned when the retry budget is exhausted.
///
/// The dedicated [`ProviderError::RetryExhausted`] variant carries the
/// attempt count and the last error verbatim (Phase 2 spec) — the last
/// error's own text (e.g. `rate limit exceeded`) is preserved as-is, with
/// no string wrapping and no double prefix.
pub(crate) fn exhausted_error(attempts: u32, last: ProviderError) -> ProviderError {
    ProviderError::RetryExhausted {
        attempts,
        last: last.to_string(),
    }
}

/// Sleep for `delay`, or return early (`true`) when the stream consumer has
/// dropped its receiver — there is no point burning further attempts then.
pub(crate) async fn backoff_or_consumer_gone(
    tx: &tokio::sync::mpsc::Sender<Result<openslate_core::types::ModelStreamEvent, ProviderError>>,
    delay: Duration,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = tx.closed() => true,
    }
}

/// Jitter in `[0, bound_ms)`, derived from the wall clock's sub-second
/// nanoseconds — dependency-free and sufficient to de-synchronize
/// single-process retry storms.
fn jitter_ms(bound_ms: u64) -> u64 {
    if bound_ms == 0 {
        return 0;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    nanos % bound_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_attempts_clamps_to_one() {
        assert_eq!(RetryPolicy::new(0, 500).max_attempts(), 1);
        assert_eq!(RetryPolicy::new(3, 500).max_attempts(), 3);
    }

    #[test]
    fn backoff_delay_is_exponential_with_jitter_bound() {
        let policy = RetryPolicy::new(3, 100);
        for (attempt, floor) in [(1, 100u64), (2, 200), (3, 400)] {
            let delay = policy.backoff_delay(attempt);
            let ms = delay.as_millis() as u64;
            assert!(
                ms >= floor && ms < floor + 100,
                "attempt {attempt}: expected [{floor}, {}) ms, got {ms}",
                floor + 100
            );
        }
    }

    #[test]
    fn backoff_delay_caps_at_10s() {
        let policy = RetryPolicy::new(50, 5_000);
        // 5000 * 2^(attempt-1) overflows the cap almost immediately.
        for attempt in 1..=40 {
            assert!(
                policy.backoff_delay(attempt) <= Duration::from_millis(MAX_BACKOFF_MS),
                "attempt {attempt} exceeded the 10s single-sleep cap"
            );
        }
    }

    #[test]
    fn backoff_delay_zero_base_is_near_zero() {
        let policy = RetryPolicy::new(3, 0);
        assert_eq!(policy.backoff_delay(1), Duration::from_millis(0));
    }

    fn http_error(status: u16) -> genai::Error {
        genai::Error::HttpError {
            status: reqwest::StatusCode::from_u16(status).expect("valid status"),
            canonical_reason: String::new(),
            body: String::new(),
        }
    }

    #[test]
    fn retryable_http_statuses() {
        for status in [429, 500, 502, 503, 504] {
            assert!(
                is_retryable(&http_error(status)),
                "{status} should be retryable"
            );
        }
    }

    #[test]
    fn permanent_http_statuses_are_not_retryable() {
        for status in [400, 401, 403, 404, 422] {
            assert!(
                !is_retryable(&http_error(status)),
                "{status} should not be retryable"
            );
        }
    }

    #[test]
    fn deterministic_genai_errors_are_not_retryable() {
        // Auth-resolution failures are permanent.
        assert!(!is_retryable(&genai::Error::RequiresApiKey {
            model_iden: fake_model_iden(),
        }));
        // Malformed response bodies are deterministic.
        assert!(!is_retryable(&genai::Error::StreamParse {
            model_iden: fake_model_iden(),
            serde_error: serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
        }));
    }

    fn fake_model_iden() -> genai::ModelIden {
        genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "test-model")
    }

    #[test]
    fn exhausted_error_carries_attempts_and_last_error() {
        let err = exhausted_error(3, ProviderError::RateLimit);
        assert!(
            matches!(
                err,
                ProviderError::RetryExhausted {
                    attempts: 3,
                    ref last
                } if last == "rate limit exceeded"
            ),
            "expected RetryExhausted(3, rate limit exceeded), got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("after 3 attempt(s)"), "msg: {msg}");
        assert!(msg.contains("rate limit exceeded"), "msg: {msg}");
    }

    #[test]
    fn exhausted_error_keeps_connection_detail_verbatim() {
        let err = exhausted_error(2, ProviderError::ConnectionError("reset by peer".into()));
        assert!(
            matches!(
                err,
                ProviderError::RetryExhausted {
                    attempts: 2,
                    ref last
                } if last == "connection error: reset by peer"
            ),
            "expected the last error's Display verbatim, got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("after 2 attempt(s)"), "msg: {msg}");
        assert!(msg.contains("reset by peer"), "msg: {msg}");
    }
}
