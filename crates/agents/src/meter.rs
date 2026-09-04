//! Usage-anchored token metering (dsh `dsh-token-meter` port).
//!
//! The heuristic `chars/4` price of a prompt can disagree with what the
//! provider actually counted — llama.cpp's real tokenizer differs by up to
//! 20%, and neither the heuristic nor `/tokenize` on concatenated text can
//! see the chat template, the tool schemas, or image parts. The provider's
//! own `usage` numbers can.
//!
//! So after every successful request that carried a `usage` trailer, the
//! meter stores an **anchor**: the envelope fingerprint (system prompt +
//! tool schemas), the seq of the newest surface entry that was sent, and the
//! provider-reported prompt tokens. Before the next request, if the envelope
//! is unchanged the meter prices only what was *added* since the anchor
//! heuristically and takes the rest from the anchor:
//!
//! ```text
//! used = anchor.usage_prompt + Σ approx(entries with seq > anchor.seq)
//! ```
//!
//! The anchor is rejected when the provider reported fewer prompt tokens
//! than the heuristic prices for the same surface — a provider number below
//! the floor is not trustworthy, and the estimate falls back to the pure
//! heuristic. The meter never decides anything for the loop; it only prices.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use llm::tokenize::approx_tokens;
use sica_core::event::SurfaceEntry;

/// One stored anchor point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Anchor {
    envelope_hash: u64,
    /// Seq of the newest surface entry included in the anchored request.
    seq:           u64,
    /// Provider-reported prompt tokens of the anchored request.
    usage_prompt:  u32,
}

/// Per-session token meter. Kept on the chat hub; one per session.
#[derive(Debug, Default)]
pub struct TokenMeter {
    anchor: Option<Anchor>,
}

impl TokenMeter {
    /// Store a new anchor after a successful request that carried a
    /// provider `usage`.
    pub fn record(&mut self, envelope_hash: u64, seq: u64, usage_prompt: u32) {
        if usage_prompt == 0 {
            return;
        }
        self.anchor = Some(Anchor { envelope_hash, seq, usage_prompt });
    }

    /// Drop the anchor (an envelope change does this implicitly through a
    /// hash mismatch, but interrupts and failed turns make it explicit).
    pub fn invalidate(&mut self) {
        self.anchor = None;
    }

    /// Estimate the prompt tokens of the request about to be sent.
    ///
    /// `entries` is the derived surface the request will carry (plus the
    /// system prompt and tools, which the envelope hash covers).
    /// Returns `None` when there is no usable anchor — same envelope and a
    /// plausible provider count are both required.
    pub fn estimate(&self, envelope_hash: u64, entries: &[SurfaceEntry]) -> Option<u32> {
        let a = self.anchor.as_ref()?;
        if a.envelope_hash != envelope_hash {
            return None;
        }
        let anchored_price: u32 = entries
            .iter()
            .filter(|e| e.seq <= a.seq)
            .map(entry_tokens)
            .sum();
        // A provider count below the heuristic floor for the same surface is
        // not an anchor worth standing on.
        if a.usage_prompt < anchored_price {
            return None;
        }
        let delta: u32 = entries
            .iter()
            .filter(|e| e.seq > a.seq)
            .map(entry_tokens)
            .sum();
        Some(a.usage_prompt.saturating_add(delta))
    }
}

/// The heuristic price of one surface entry — the same accounting the
/// trigger and the trimmer use (+4 per-message overhead).
pub fn entry_tokens(e: &SurfaceEntry) -> u32 {
    approx_tokens(&e.message.content) + 4
}

/// Fingerprint of everything in a request that is *not* surface history:
/// the composed system prompt body plus the native `tools` array. When this
/// is unchanged between two requests, the provider's previous prompt count
/// is reusable as an anchor.
pub fn envelope_hash(system_body: &str, tools_json: Option<&serde_json::Value>) -> u64 {
    let mut h = DefaultHasher::new();
    system_body.hash(&mut h);
    match tools_json {
        Some(t) => t.to_string().hash(&mut h),
        None    => 0u8.hash(&mut h),
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sica_core::message::Message;

    fn entry(seq: u64, len: usize) -> SurfaceEntry {
        SurfaceEntry {
            seq,
            message: Message::user(&"x".repeat(len)),
            tool: None,
            context: None,
        }
    }

    #[test]
    fn no_anchor_means_no_estimate() {
        let m = TokenMeter::default();
        assert_eq!(m.estimate(1, &[entry(1, 40)]), None);
    }

    #[test]
    fn matching_envelope_anchors_on_provider_usage_plus_delta() {
        let mut m = TokenMeter::default();
        let entries = vec![entry(1, 400), entry(2, 400)];
        let heuristic: u32 = entries.iter().map(entry_tokens).sum();
        m.record(7, 1, 150); // provider saw 150 prompt tokens incl. template
        let est = m.estimate(7, &entries).expect("anchored");
        // 150 for entry 1 (+ template), plus the heuristic price of entry 2.
        assert_eq!(est, 150 + entry_tokens(&entries[1]));
        assert!(est > heuristic, "template overhead survives the anchor");
    }

    #[test]
    fn envelope_change_falls_back() {
        let mut m = TokenMeter::default();
        m.record(7, 1, 150);
        assert_eq!(m.estimate(8, &[entry(1, 400)]), None);
    }

    #[test]
    fn provider_count_below_heuristic_floor_is_not_trusted() {
        let mut m = TokenMeter::default();
        let entries = vec![entry(1, 4000)];
        m.record(7, 1, 5); // implausibly small
        assert_eq!(m.estimate(7, &entries), None);
    }

    #[test]
    fn invalidate_drops_the_anchor() {
        let mut m = TokenMeter::default();
        m.record(7, 1, 150);
        m.invalidate();
        assert_eq!(m.estimate(7, &[entry(1, 40)]), None);
    }

    #[test]
    fn zero_usage_is_not_recorded() {
        let mut m = TokenMeter::default();
        m.record(7, 1, 0);
        assert_eq!(m.estimate(7, &[entry(1, 40)]), None);
    }

    #[test]
    fn envelope_hash_differs_on_tools_and_body() {
        let tools = serde_json::json!([{"type": "function"}]);
        let a = envelope_hash("sys", None);
        let b = envelope_hash("sys", Some(&tools));
        let c = envelope_hash("sys2", None);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, envelope_hash("sys", None));
    }
}
