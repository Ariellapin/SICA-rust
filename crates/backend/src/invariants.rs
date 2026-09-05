//! Runtime invariant companions (guide §14.3).
//!
//! dsh's rule for publishing one: **only when independent observations can
//! diverge.** "The service is up" is not an invariant — nothing else claims
//! otherwise. "The request I dispatched is reconstructable from the log"
//! is: the loop built the request from a derivation, and the log is a
//! second, durable record of the same thing, so the two *can* disagree, and
//! if they ever do then compaction replay, prompt editing and the
//! Trajectory view are all quietly lying.
//!
//! Three ship here, one per relationship the codebase relies on and cannot
//! otherwise observe:
//!
//! 1. [`check_request_matches_log`] — what went on the wire is a suffix of
//!    what the log derives. Suffix rather than equality because the trimmer
//!    legitimately amputates the front to fit the window; anything *extra*,
//!    or any disagreement in the tail, is a defect.
//! 2. [`check_compaction_span_balanced`] — a `Replace` span never splits a
//!    tool call from its result. A shadowed call whose result survives
//!    leaves the model reading an answer to a question it cannot see.
//! 3. [`check_retry_appended_nothing`] — a retried attempt persisted
//!    nothing. That is the entire reason a retry is safe: re-entering the
//!    loop must rebuild the identical request.
//!
//! Off unless the backend was started with `--invariants`, because each
//! check re-derives the log. Failures are ERROR `LogLine`s naming the
//! invariant — never panics: an invariant that takes the app down turns a
//! reporting tool into an outage.

use std::sync::atomic::{AtomicBool, Ordering};

use llm::ChatMessage;
use sica_core::event::{EventKind, SessionEvent, SurfaceOp};

/// Set once from `--invariants`. A global rather than a field because the
/// checks are called from inside the turn task, five call layers below the
/// hub that would otherwise have to thread the flag down.
static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// A violation, in the terms the operator needs: which invariant, and what
/// the two observations actually were.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Short stable name, so a report can be grepped for.
    pub invariant: &'static str,
    pub detail:    String,
}

impl Violation {
    fn new(invariant: &'static str, detail: impl Into<String>) -> Self {
        Self { invariant, detail: detail.into() }
    }

    pub fn message(&self) -> String {
        format!("invariant `{}` violated: {}", self.invariant, self.detail)
    }
}

/// Report violations as ERROR log lines. The caller passes the sink because
/// the checks themselves stay pure — they are tested without one.
pub fn report(events: &dyn agents::EventSink, violations: Vec<Violation>) {
    for v in violations {
        tracing::error!(invariant = v.invariant, detail = %v.detail, "invariant violated");
        events.emit(protocol::Event::LogLine {
            level:   "ERROR".into(),
            message: v.message(),
        });
    }
}

/// **request-matches-log.** The messages that went on the wire, minus the
/// composed system prompt and minus the trimmer's wire-only notice, must be
/// a suffix of the wire form of the log's own derived history.
///
/// `sent` is what the request carried; `from_log` is the same mapping
/// applied to a *fresh* derivation. Divergence means a request the log
/// cannot reproduce.
pub fn check_request_matches_log(
    sent: &[ChatMessage],
    from_log: &[ChatMessage],
) -> Vec<Violation> {
    let sent: Vec<&ChatMessage> = sent
        .iter()
        .filter(|m| m.role != "system")
        .filter(|m| !is_trim_notice(m))
        .collect();
    if sent.len() > from_log.len() {
        return vec![Violation::new(
            "request-matches-log",
            format!(
                "the request carried {} message(s) but the log derives only {} — \
                 something was sent that the log cannot reproduce",
                sent.len(),
                from_log.len()
            ),
        )];
    }
    // Compare against the tail: the trimmer drops from the front.
    let tail = &from_log[from_log.len() - sent.len()..];
    for (i, (a, b)) in sent.iter().zip(tail.iter()).enumerate() {
        if a.role != b.role || a.content.text() != b.content.text() {
            return vec![Violation::new(
                "request-matches-log",
                format!(
                    "message {i} of the request does not match the log: sent \
                     {:?}/{:.80?}, log has {:?}/{:.80?}",
                    a.role,
                    a.content.text(),
                    b.role,
                    b.content.text()
                ),
            )];
        }
    }
    Vec::new()
}

/// The trimmer's notice is inserted into the wire history and never enters
/// the log, so it is not a divergence.
fn is_trim_notice(m: &ChatMessage) -> bool {
    m.content.text().starts_with(agents::context::CONTEXT_NOTICE_PREFIX)
}

/// **compaction-span-balanced.** Every `Replace` span in `events` shadows
/// tool calls and their results together. A span holding a `ToolResult`
/// whose `ToolCall` is outside it (or the reverse) leaves the derived
/// history with half a pair.
pub fn check_compaction_span_balanced(events: &[SessionEvent]) -> Vec<Violation> {
    let mut out = Vec::new();
    for ev in events {
        let Some(SurfaceOp::Replace { start_seq, end_seq }) = ev.kind.surface() else {
            continue;
        };
        let (start, end) = (*start_seq, *end_seq);
        // A summary replaces a *prefix*: the pairing rule only has to hold
        // for results whose call is in the log at all.
        for inner in events.iter().filter(|e| e.seq >= start && e.seq <= end) {
            if let EventKind::ToolResult { call_seq, skill, .. } = &inner.kind {
                if *call_seq != 0 && (*call_seq < start || *call_seq > end) {
                    out.push(Violation::new(
                        "compaction-span-balanced",
                        format!(
                            "span {start}..={end} (from seq {}) shadows the result of \
                             `{skill}` at seq {} but not its call at seq {call_seq}",
                            ev.seq, inner.seq
                        ),
                    ));
                }
            }
        }
        // The other half: a call inside the span whose result is outside it.
        for outer in events.iter().filter(|e| e.seq < start || e.seq > end) {
            if let EventKind::ToolResult { call_seq, skill, .. } = &outer.kind {
                if *call_seq >= start && *call_seq <= end {
                    out.push(Violation::new(
                        "compaction-span-balanced",
                        format!(
                            "span {start}..={end} (from seq {}) shadows the call of \
                             `{skill}` at seq {call_seq} but not its result at seq {}",
                            ev.seq, outer.seq
                        ),
                    ));
                }
            }
        }
    }
    out
}

/// **retry-appends-nothing.** Between the seq the log stood at when an
/// attempt began and the `LlmRetry` that records its failure, no *surface*
/// event may have been appended: a retry that persisted something is no
/// longer a re-run of the same request.
///
/// Bookkeeping (the retry row itself, a token-usage row) is fine — it does
/// not reach the model.
pub fn check_retry_appended_nothing(
    events: &[SessionEvent],
    seq_before_attempt: u64,
) -> Vec<Violation> {
    let offenders: Vec<String> = events
        .iter()
        .filter(|e| e.seq > seq_before_attempt)
        .filter(|e| e.kind.surface().is_some())
        .map(|e| format!("seq {} ({})", e.seq, kind_name(&e.kind)))
        .collect();
    if offenders.is_empty() {
        return Vec::new();
    }
    vec![Violation::new(
        "retry-appends-nothing",
        format!(
            "a failed attempt persisted model-visible history before the retry: {} — \
             the retry will not rebuild the same request",
            offenders.join(", ")
        ),
    )]
}

fn kind_name(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::UserMessage { .. } => "UserMessage",
        EventKind::AssistantMessage { .. } => "AssistantMessage",
        EventKind::ToolResult { .. } => "ToolResult",
        EventKind::CompactionSummary { .. } => "CompactionSummary",
        EventKind::ContextInjected { .. } => "ContextInjected",
        EventKind::LegacyMessage { .. } => "LegacyMessage",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::ChatContent;

    fn msg(role: &str, text: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: ChatContent::Text(text.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn ev(seq: u64, kind: EventKind) -> SessionEvent {
        SessionEvent { seq, ts: seq as i64 * 10, kind }
    }

    fn call(seq: u64, name: &str) -> SessionEvent {
        ev(seq, EventKind::ToolCall {
            name: name.into(),
            args_preview: String::new(),
            expectation: String::new(),
            call_id: None,
            args_json: None,
        })
    }

    fn result(seq: u64, call_seq: u64, skill: &str) -> SessionEvent {
        ev(seq, EventKind::ToolResult {
            surface: SurfaceOp::Append,
            call_seq,
            skill: skill.into(),
            tool_call_id: None,
            ok: true,
            summary: "ok".into(),
            trusted: true,
            pruned: false,
        })
    }

    fn user(seq: u64, text: &str) -> SessionEvent {
        ev(seq, EventKind::UserMessage {
            surface: SurfaceOp::Append,
            content: text.into(),
            images: Vec::new(),
        })
    }

    // -- request-matches-log ------------------------------------------------

    #[test]
    fn an_untrimmed_request_matches_the_log_exactly() {
        let log = vec![msg("user", "a"), msg("assistant", "b")];
        let sent = vec![msg("system", "prompt"), msg("user", "a"), msg("assistant", "b")];
        assert!(check_request_matches_log(&sent, &log).is_empty());
    }

    #[test]
    fn a_trimmed_request_is_still_a_suffix_of_the_log() {
        // The trimmer amputating the front is normal operation, not a
        // divergence — the invariant would be useless if it fired there.
        let log = vec![msg("user", "old"), msg("user", "a"), msg("assistant", "b")];
        let sent = vec![
            msg("system", "prompt"),
            msg(
                "user",
                &format!("{} 1 older message dropped]", agents::context::CONTEXT_NOTICE_PREFIX),
            ),
            msg("user", "a"),
            msg("assistant", "b"),
        ];
        assert!(
            check_request_matches_log(&sent, &log).is_empty(),
            "the wire-only trim notice must not read as an extra message"
        );
    }

    #[test]
    fn a_message_the_log_does_not_hold_is_a_violation() {
        let log = vec![msg("user", "a")];
        let sent = vec![msg("user", "a"), msg("user", "smuggled in")];
        let v = check_request_matches_log(&sent, &log);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].invariant, "request-matches-log");
        assert!(v[0].detail.contains("cannot reproduce"), "{}", v[0].detail);
    }

    #[test]
    fn a_tail_that_disagrees_is_a_violation() {
        let log = vec![msg("user", "a"), msg("assistant", "what the log says")];
        let sent = vec![msg("user", "a"), msg("assistant", "what was sent")];
        let v = check_request_matches_log(&sent, &log);
        assert_eq!(v.len(), 1);
        assert!(v[0].detail.contains("does not match"), "{}", v[0].detail);
    }

    // -- compaction-span-balanced -------------------------------------------

    #[test]
    fn a_span_covering_whole_pairs_is_balanced() {
        let events = vec![
            user(1, "hi"),
            call(2, "read-file"),
            result(3, 2, "read-file"),
            ev(4, EventKind::CompactionSummary {
                surface: SurfaceOp::Replace { start_seq: 1, end_seq: 3 },
                content: "[summary]".into(),
                summary: "s".into(),
                folded: 3,
                before_tokens: 100,
                after_tokens: 10,
            }),
        ];
        assert!(check_compaction_span_balanced(&events).is_empty());
    }

    #[test]
    fn a_span_that_swallows_a_call_but_not_its_result_is_a_violation() {
        // The model would read an answer to a question it can no longer see.
        let events = vec![
            call(1, "run-cli"),
            result(2, 1, "run-cli"),
            ev(3, EventKind::CompactionSummary {
                surface: SurfaceOp::Replace { start_seq: 1, end_seq: 1 },
                content: "[summary]".into(),
                summary: "s".into(),
                folded: 1,
                before_tokens: 100,
                after_tokens: 10,
            }),
        ];
        let v = check_compaction_span_balanced(&events);
        assert_eq!(v.len(), 1, "{v:?}");
        assert_eq!(v[0].invariant, "compaction-span-balanced");
        assert!(v[0].detail.contains("but not its result"), "{}", v[0].detail);
    }

    #[test]
    fn a_span_that_swallows_a_result_but_not_its_call_is_a_violation() {
        let events = vec![
            call(1, "run-cli"),
            result(2, 1, "run-cli"),
            ev(3, EventKind::CompactionSummary {
                surface: SurfaceOp::Replace { start_seq: 2, end_seq: 2 },
                content: "[summary]".into(),
                summary: "s".into(),
                folded: 1,
                before_tokens: 100,
                after_tokens: 10,
            }),
        ];
        let v = check_compaction_span_balanced(&events);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].detail.contains("but not its call"), "{}", v[0].detail);
    }

    #[test]
    fn a_log_with_no_replace_span_has_nothing_to_check() {
        let events = vec![user(1, "hi"), call(2, "glob"), result(3, 2, "glob")];
        assert!(check_compaction_span_balanced(&events).is_empty());
    }

    // -- retry-appends-nothing ----------------------------------------------

    #[test]
    fn a_retry_over_bookkeeping_only_is_clean() {
        // A retry row and a usage row are not model-visible, so they do not
        // change the request the next attempt rebuilds.
        let events = vec![
            user(1, "hi"),
            ev(2, EventKind::LlmRetry {
                attempt: 1, max: 5, delay_ms: 500, reason: "HTTP 500".into(),
            }),
            ev(3, EventKind::TokenUsage {
                used: 10, limit: 100, budget: 80,
                prompt_tokens: None, completion_tokens: None,
            }),
        ];
        assert!(check_retry_appended_nothing(&events, 1).is_empty());
    }

    #[test]
    fn a_retry_after_a_persisted_reply_is_a_violation() {
        let events = vec![
            user(1, "hi"),
            ev(2, EventKind::AssistantMessage {
                surface: SurfaceOp::Append,
                content: "half an answer".into(),
                reasoning: None,
                tool_calls: None,
            }),
            ev(3, EventKind::LlmRetry {
                attempt: 1, max: 5, delay_ms: 500, reason: "sse decode".into(),
            }),
        ];
        let v = check_retry_appended_nothing(&events, 1);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].invariant, "retry-appends-nothing");
        assert!(v[0].detail.contains("AssistantMessage"), "{}", v[0].detail);
    }

    #[test]
    fn the_message_names_the_invariant_so_it_can_be_grepped() {
        let v = Violation::new("retry-appends-nothing", "because");
        assert_eq!(v.message(), "invariant `retry-appends-nothing` violated: because");
    }

    #[test]
    fn the_checks_are_off_until_the_flag_turns_them_on() {
        // Not `enable()` here — that would leak into the other tests in this
        // binary, which is exactly the kind of shared-state bug the flag
        // being a global invites.
        assert!(!enabled() || cfg!(feature = "never"));
    }
}
