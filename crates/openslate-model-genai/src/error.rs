use openslate_core::error::ProviderError;

/// Errors that can occur while constructing a [`crate::GenaiProvider`].
#[derive(Debug, thiserror::Error)]
pub enum GenaiBuildError {
    /// The `adapter` config value did not map to a known genai `AdapterKind`.
    #[error(
        "unknown genai adapter '{0}'; expected a lowercase provider name \
         (e.g. 'anthropic', 'openai', 'gemini', 'ollama', 'deepseek', 'openrouter', 'groq')"
    )]
    UnknownAdapter(String),

    /// The injected `reqwest::Client` could not be built.
    #[error("failed to build HTTP client: {0}")]
    ReqwestBuild(String),
}

/// Map a `genai::Error` into OpenSlate's `ProviderError`.
///
/// This is a free function (not a `From` impl) because the orphan rule forbids
/// implementing `From` between two types that are both foreign to this crate.
///
/// Two HTTP-failure shapes exist in genai 0.6.x:
/// - `WebModelCall`/`WebAdapterCall` carry a `webc::Error` (non-streaming path),
/// - `HttpError` is constructed by the SSE stream parser (`web_stream.rs`) and
///   arrives as `Err(e)` from a streaming `ChatStream`.
///
/// Both are mapped through the same status-code ladder so that, e.g., a
/// mid-stream 401 surfaces as `AuthError` rather than being demoted to
/// `ConnectionError`.
pub(crate) fn map_error(e: genai::Error) -> ProviderError {
    match e {
        // Non-streaming / adapter-level HTTP failures.
        genai::Error::WebModelCall { webc_error, .. }
        | genai::Error::WebAdapterCall { webc_error, .. } => map_webc(webc_error),

        // Mid-stream HTTP error event (constructed by the SSE parser).
        genai::Error::HttpError { status, body, .. } => map_status(status, body),

            // Stream-level errors.
            //
            // genai's SSE streamer boxes HTTP-status errors (web_stream.rs) into a
            // `WebStream { cause, error }`, so a mid-stream 401/429/5xx would
            // otherwise lose its status code. Try to recover it via downcast
            // before falling back to a generic connection error.
            genai::Error::WebStream { cause, error, .. } => {
                if let Some(inner) = error.downcast_ref::<genai::Error>() {
                    if let genai::Error::HttpError { status, body, .. } = inner {
                        return map_status(*status, body.clone());
                    }
                }
                ProviderError::ConnectionError(cause)
            }
            genai::Error::StreamParse { serde_error, .. } => {
                ProviderError::MalformedResponse(serde_error.to_string())
            }

        // Auth.
        genai::Error::RequiresApiKey { .. }
        | genai::Error::NoAuthResolver { .. }
        | genai::Error::NoAuthData { .. } => {
            ProviderError::AuthError("missing API key".to_string())
        }

        // Response-building failures.
        genai::Error::ChatResponseGeneration { cause, .. } => {
            ProviderError::MalformedResponse(cause)
        }
        genai::Error::ChatResponse { body, .. } => {
            ProviderError::MalformedResponse(body.to_string())
        }

        // Anything else (input validation, resolver, serde, etc.).
        other => ProviderError::ConnectionError(other.to_string()),
    }
}

fn map_webc(e: genai::webc::Error) -> ProviderError {
    match e {
        genai::webc::Error::ResponseFailedStatus { status, body, .. } => map_status(status, body),
        genai::webc::Error::Reqwest(re) => {
            if re.is_timeout() {
                ProviderError::Timeout
            } else {
                ProviderError::ConnectionError(re.to_string())
            }
        }
        genai::webc::Error::ResponseFailedInvalidJson { body, .. }
        | genai::webc::Error::ResponseFailedNotJson { body, .. } => {
            ProviderError::MalformedResponse(body)
        }
        other => ProviderError::ConnectionError(other.to_string()),
    }
}

/// Shared status-code ladder for both `ResponseFailedStatus` (non-streaming)
/// and `HttpError` (streaming).
fn map_status(status: reqwest::StatusCode, body: String) -> ProviderError {
    match status.as_u16() {
        429 => ProviderError::RateLimit,
        401 | 403 => ProviderError::AuthError(snippet(&body)),
        404 => ProviderError::NotFound(snippet(&body)),
        s => ProviderError::ServerError(s),
    }
}

/// Truncate a response body for inclusion in an error message.
fn snippet(s: &str) -> String {
    const MAX: usize = 200;
    if s.len() <= MAX {
        s.to_string()
    } else {
        // Fall back to a char-boundary-safe truncation.
        match s.char_indices().nth(MAX) {
            Some((idx, _)) => format!("{}…", &s[..idx]),
            None => s.to_string(),
        }
    }
}
