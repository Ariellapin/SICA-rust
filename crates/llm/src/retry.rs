//! Retry policy for chat requests.
//!
//! The policy only *classifies* and *schedules* — it never wraps
//! `chat_stream`. The agent loop applies it at the step boundary: a failed
//! attempt persists nothing, so re-entering the loop rebuilds the identical
//! request over the same durable history. That is what makes a retry safe:
//! the model sees exactly what it would have seen had the first attempt
//! never happened.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Attempts after the first. Six requests total before giving up.
pub const RETRY_MAX: u32 = 5;
const INITIAL_DELAY_MS: u64 = 500;
const MAX_DELAY_MS: u64 = 10_000;

/// Why an attempt failed, in terms the loop and the operator can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// Worth another attempt: transport dropped, server overloaded/broken,
    /// stream cut mid-way, or the server answered with nothing at all.
    Retryable(String),
    /// Retrying would reproduce the same answer: bad request, auth,
    /// unknown model.
    Fatal(String),
    /// The prompt did not fit the server's context window. Repeating the
    /// request reproduces the refusal; *shrinking* it does not — the loop
    /// compacts the history and tries again. `limit` is the window the
    /// server named in its message, when it named one (vLLM's "maximum
    /// context length is N tokens", llama.cpp's `"n_ctx":N`), so the
    /// loop can adopt the real number instead of guessing.
    ContextOverflow { reason: String, limit: Option<u32> },
}

impl Failure {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Failure::Retryable(_))
    }

    /// The request has to be made smaller before it can succeed.
    pub fn is_context_overflow(&self) -> bool {
        matches!(self, Failure::ContextOverflow { .. })
    }

    /// The context window the provider named in an overflow refusal.
    pub fn context_limit(&self) -> Option<u32> {
        match self {
            Failure::ContextOverflow { limit, .. } => *limit,
            _ => None,
        }
    }

    pub fn reason(&self) -> &str {
        match self {
            Failure::Retryable(r) | Failure::Fatal(r) => r,
            Failure::ContextOverflow { reason, .. } => reason,
        }
    }
}

/// Does an error text say the prompt did not fit the context? Matched on
/// the provider's own wording — llama.cpp's `exceed_context_size_error`
/// / "exceeds the available context size", vLLM's and OpenAI's
/// `context_length_exceeded` / "maximum context length is N tokens",
/// Anthropic-compatible "prompt is too long", and the generic "context …
/// exceeded / too long / too large / too many tokens" family.
pub fn is_context_overflow_text(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    if t.contains("context_length_exceeded")
        || t.contains("exceed_context_size")
        || t.contains("prompt is too long")
        || t.contains("reduce the length of the messages")
        || t.contains("too many tokens")
    {
        return true;
    }
    (t.contains("context length") || t.contains("context size") || t.contains("context window"))
        && (t.contains("exceed") || t.contains("too long") || t.contains("too large")
            || t.contains("maximum") || t.contains("larger than") || t.contains("greater than"))
}

/// The window an overflow refusal names, if it names one. Reads the first
/// number after any of a few provider phrasings; the *requested* size
/// (vLLM's "However, you requested N tokens") comes later in the message
/// and is never picked up.
pub fn context_limit_in(text: &str) -> Option<u32> {
    let t = text.to_ascii_lowercase();
    const KEYS: &[&str] = &[
        "\"n_ctx\"",
        "maximum context length is",
        "maximum context length of",
        "context length is",
        "context length of",
        "context window of",
        "context window is",
        "context size of",
        "context size is",
        "tokens >",
    ];
    KEYS.iter()
        .find_map(|k| number_after(&t, k))
        .filter(|&n| n >= 512)
}

/// The first run of digits after `key`, skipping the punctuation and
/// filler between (`": `, ` is `, ` `).
fn number_after(text: &str, key: &str) -> Option<u32> {
    let idx = text.find(key)? + key.len();
    let rest = text[idx..].trim_start_matches(|c: char| !c.is_ascii_digit());
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Classify a `chat_stream` error. `reqwest` errors are inspected directly
/// (they survive the `anyhow` conversion intact); anything else is judged
/// by the message prefixes `client.rs` uses.
pub fn classify(err: &anyhow::Error) -> Failure {
    let text = format!("{err:#}");
    // Judged before the status code: a context overflow *is* an HTTP 400
    // on every provider, and it is the one 400 the loop can act on.
    if is_context_overflow_text(&text) {
        return Failure::ContextOverflow {
            reason: text.clone(),
            limit: context_limit_in(&text),
        };
    }
    if let Some(re) = err.downcast_ref::<reqwest::Error>() {
        if let Some(status) = re.status() {
            let code = status.as_u16();
            return if code == 429 || (500..=599).contains(&code) {
                Failure::Retryable(format!("HTTP {code}"))
            } else {
                Failure::Fatal(format!("HTTP {code}"))
            };
        }
        // No status: the request never got an answer (connect, timeout,
        // reset mid-body, redirect loop). Every one of those is transient.
        return Failure::Retryable(text);
    }
    if text.starts_with("sse decode") || text.contains("task panicked") {
        return Failure::Retryable(text);
    }
    Failure::Fatal(text)
}

/// The delay a `429` / `503` answer asked for (dsh honours `Retry-After`).
/// The client folds the header into the error's context as
/// `retry-after=<seconds>`; this reads it back so the loop can wait what
/// the server said instead of what the backoff table guesses. Capped at
/// two minutes: a server asking for an hour is a server to give up on.
pub fn retry_after(err: &anyhow::Error) -> Option<Duration> {
    let text = format!("{err:#}");
    let idx = text.find("retry-after=")?;
    let rest = &text[idx + "retry-after=".len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let secs: u64 = digits.parse().ok()?;
    Some(Duration::from_secs(secs.min(120)))
}

/// The failure recorded when a request completes cleanly but delivers no
/// content, no reasoning and no tool call. Local servers do this under
/// memory pressure; treating it as a real (empty) reply would persist a
/// blank assistant turn.
pub fn empty_response() -> Failure {
    Failure::Retryable("empty response".into())
}

/// Delay before `attempt` (1-based): `500ms · 2^(attempt-1)` capped at 10 s,
/// with symmetric ±50 % jitter so several sessions that failed together do
/// not retry in lockstep, re-capped at 10 s.
pub fn backoff(attempt: u32) -> Duration {
    let exp = attempt.saturating_sub(1).min(16);
    let base = INITIAL_DELAY_MS.saturating_mul(1u64 << exp).min(MAX_DELAY_MS);
    let jitter_span = base / 2;
    // Cheap entropy without a new dependency: sub-second clock bits.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    let offset = if jitter_span == 0 { 0 } else { nanos % (jitter_span * 2 + 1) };
    let ms = (base - jitter_span + offset).min(MAX_DELAY_MS);
    Duration::from_millis(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        for attempt in 1..=8 {
            let d = backoff(attempt).as_millis() as u64;
            let base = (INITIAL_DELAY_MS << (attempt - 1)).min(MAX_DELAY_MS);
            assert!(d >= base / 2, "attempt {attempt}: {d} < {}", base / 2);
            assert!(d <= MAX_DELAY_MS, "attempt {attempt}: {d} > cap");
            assert!(d <= base + base / 2, "attempt {attempt}: {d} above jitter band");
        }
        assert!(backoff(u32::MAX).as_millis() as u64 <= MAX_DELAY_MS);
    }

    #[test]
    fn sse_and_panic_messages_are_retryable() {
        assert!(classify(&anyhow::anyhow!("sse decode: unexpected EOF")).is_retryable());
        assert!(classify(&anyhow::anyhow!("chat_stream task panicked: x")).is_retryable());
    }

    #[test]
    fn unknown_messages_are_fatal() {
        assert!(!classify(&anyhow::anyhow!("model not found")).is_retryable());
    }

    #[test]
    fn http_status_split() {
        // Build real reqwest errors via a response with the given status.
        let make = |code: u16| -> anyhow::Error {
            let resp = http::Response::builder().status(code).body("").unwrap();
            reqwest::Response::from(resp).error_for_status().unwrap_err().into()
        };
        assert_eq!(classify(&make(429)), Failure::Retryable("HTTP 429".into()));
        assert_eq!(classify(&make(503)), Failure::Retryable("HTTP 503".into()));
        assert_eq!(classify(&make(400)), Failure::Fatal("HTTP 400".into()));
        assert_eq!(classify(&make(401)), Failure::Fatal("HTTP 401".into()));
    }

    #[test]
    fn empty_response_is_retryable() {
        assert!(empty_response().is_retryable());
    }

    #[test]
    fn vllm_overflow_names_its_limit() {
        let err = anyhow::anyhow!(
            "HTTP 400: {{\"object\":\"error\",\"message\":\"This model's maximum context \
             length is 8192 tokens. However, you requested 9107 tokens (8595 in the \
             messages, 512 in the completion). Please reduce the length of the messages \
             or completion.\",\"type\":\"BadRequestError\",\"code\":400}}"
        );
        let f = classify(&err);
        assert!(f.is_context_overflow(), "{f:?}");
        assert!(!f.is_retryable());
        assert_eq!(f.context_limit(), Some(8192));
    }

    #[test]
    fn llama_cpp_overflow_names_n_ctx() {
        let err = anyhow::anyhow!(
            "HTTP 400: {{\"error\":{{\"code\":400,\"message\":\"the request exceeds the \
             available context size. try increasing the context size or enable context \
             shift\",\"type\":\"exceed_context_size_error\",\"n_prompt_tokens\":5210,\
             \"n_ctx\":4096}}}}"
        );
        let f = classify(&err);
        assert!(f.is_context_overflow(), "{f:?}");
        assert_eq!(f.context_limit(), Some(4096));
    }

    #[test]
    fn an_overflow_without_a_number_still_classifies() {
        let err = anyhow::anyhow!(
            "HTTP 400: {{\"error\":\"the request exceeds the available context size\"}}"
        );
        let f = classify(&err);
        assert!(f.is_context_overflow(), "{f:?}");
        assert_eq!(f.context_limit(), None);
    }

    #[test]
    fn anthropic_style_overflow_names_the_maximum() {
        let err = anyhow::anyhow!("HTTP 400: prompt is too long: 213000 tokens > 200000 maximum");
        let f = classify(&err);
        assert!(f.is_context_overflow(), "{f:?}");
        assert_eq!(f.context_limit(), Some(200000));
    }

    #[test]
    fn a_400_that_is_not_about_context_stays_fatal() {
        let err = anyhow::anyhow!("HTTP 400: {{\"error\":\"your messages are malformed\"}}");
        assert_eq!(classify(&err), Failure::Fatal(format!("{err:#}")));
        assert!(!classify(&anyhow::anyhow!("model not found")).is_context_overflow());
    }

    #[test]
    fn an_overflow_body_on_a_real_400_beats_the_status_split() {
        // What `chat_stream` actually produces: the reqwest status error
        // wrapped with the body excerpt as context.
        let resp = http::Response::builder().status(400).body("").unwrap();
        let inner: anyhow::Error =
            reqwest::Response::from(resp).error_for_status().unwrap_err().into();
        let err = inner.context("HTTP 400: maximum context length is 32768 tokens");
        let f = classify(&err);
        assert!(f.is_context_overflow(), "{f:?}");
        assert_eq!(f.context_limit(), Some(32768));
    }

    #[test]
    fn a_tiny_number_is_not_a_window() {
        assert_eq!(context_limit_in("context length is 3 tokens"), None);
    }
}
