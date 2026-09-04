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
    /// Retrying would reproduce the same answer: bad request (typically a
    /// prompt over the context limit), auth, unknown model.
    Fatal(String),
}

impl Failure {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Failure::Retryable(_))
    }

    pub fn reason(&self) -> &str {
        match self {
            Failure::Retryable(r) | Failure::Fatal(r) => r,
        }
    }
}

/// Classify a `chat_stream` error. `reqwest` errors are inspected directly
/// (they survive the `anyhow` conversion intact); anything else is judged
/// by the message prefixes `client.rs` uses.
pub fn classify(err: &anyhow::Error) -> Failure {
    let text = format!("{err:#}");
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
}
