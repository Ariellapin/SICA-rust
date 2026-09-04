//! The raw event log, flattened for the Trajectory view (UI guide §10).
//!
//! `LoadSession` answers with the *derived* surface — what the model sees.
//! This module answers with the log itself: every line, shadowed ones
//! included, because the view exists precisely to show what the fold threw
//! away and why a request cost what it did.
//!
//! The mapping is one-way and lossy on purpose. A ledger row needs a kind
//! tag, one line of text, and a body for the inspector; it does not need the
//! typed event, which could not cross the bincode pipe anyway (`EventKind`
//! is internally tagged). The event's own JSON rides along in `raw` for the
//! inspector's last tab, so nothing is actually lost.

use std::collections::HashSet;

use protocol::{EventDump, EventTag};
use sica_core::event::{EventKind, SessionEvent, SurfaceOp};

use crate::sessions_store::SessionLog;

/// Hard cap on one page, whatever the client asks for. The view pages with
/// `next_seq`; a client asking for a long session's whole log should still
/// not be able to make one frame carry it.
pub const MAX_PAGE: u32 = 500;

/// Page size when the client asks for `0`.
pub const DEFAULT_PAGE: u32 = 200;

/// One page of `log`, starting at the first event with `seq >= from_seq`.
/// Returns the rows, the log's total length, and the seq to ask for next.
pub fn page(log: &SessionLog, from_seq: u64, limit: u32) -> (Vec<EventDump>, u32, Option<u64>) {
    let limit = match limit {
        0 => DEFAULT_PAGE,
        n => n.min(MAX_PAGE),
    } as usize;

    // Which surface events survive the fold. Anything that contributes a
    // message but is missing here was shadowed by a later `Replace` or
    // `Rewind` — the fold is the authority on that, so ask it rather than
    // re-implementing the span arithmetic beside it.
    let live: HashSet<u64> = log.derive_surface().iter().map(|e| e.seq).collect();

    // Turn ids are carried forward from the enclosing `TurnStart`, which is
    // why the whole prefix is walked even when the page starts late.
    let mut turn_id: Option<u64> = None;
    let mut rows = Vec::new();
    let mut next_seq = None;
    for ev in &log.events {
        if let EventKind::TurnStart { turn_id: id, .. } = &ev.kind {
            turn_id = Some(*id);
        }
        let ends_turn = matches!(ev.kind, EventKind::TurnEnd { .. });
        if ev.seq >= from_seq {
            if rows.len() == limit {
                next_seq = Some(ev.seq);
                break;
            }
            rows.push(dump(ev, turn_id, &live));
        }
        if ends_turn {
            turn_id = None;
        }
    }
    (rows, log.events.len() as u32, next_seq)
}

fn dump(ev: &SessionEvent, turn_id: Option<u64>, live: &HashSet<u64>) -> EventDump {
    let d = describe(&ev.kind);
    let shadows = match &ev.kind {
        EventKind::Rewind { start_seq, end_seq } => Some((*start_seq, *end_seq)),
        other => match other.surface() {
            Some(SurfaceOp::Replace { start_seq, end_seq }) => Some((*start_seq, *end_seq)),
            _ => None,
        },
    };
    EventDump {
        seq: ev.seq,
        ts: ev.ts,
        tag: d.tag,
        text: d.text,
        payload: d.payload,
        result: d.result,
        tokens_in: d.tokens_in,
        tokens_out: d.tokens_out,
        ok: d.ok,
        // Only surface-producing events can be shadowed; bookkeeping rows (a
        // `ToolCall`, a `Command`) are never in the derived view to begin
        // with and must not render as struck through.
        shadowed: ev.kind.surface().is_some() && !live.contains(&ev.seq),
        shadows,
        call_seq: d.call_seq,
        turn_id,
        raw: serde_json::to_string(ev).unwrap_or_default(),
    }
}

/// The per-kind mapping, minus the fields every row computes the same way.
struct Described {
    tag: EventTag,
    text: String,
    payload: String,
    result: String,
    ok: Option<bool>,
    call_seq: Option<u64>,
    tokens_in: u32,
    tokens_out: u32,
}

fn row(tag: EventTag, text: String) -> Described {
    Described { tag, text, ..Described::empty() }
}

impl Described {
    fn empty() -> Self {
        Self {
            tag: EventTag::Other,
            text: String::new(),
            payload: String::new(),
            result: String::new(),
            ok: None,
            call_seq: None,
            tokens_in: 0,
            tokens_out: 0,
        }
    }
    fn payload(mut self, p: impl Into<String>) -> Self {
        self.payload = p.into();
        self
    }
    fn result(mut self, r: impl Into<String>) -> Self {
        self.result = r.into();
        self
    }
    fn ok(mut self, ok: bool) -> Self {
        self.ok = Some(ok);
        self
    }
    fn call(mut self, seq: u64) -> Self {
        self.call_seq = Some(seq);
        self
    }
    fn tokens(mut self, in_: u32, out: u32) -> Self {
        self.tokens_in = in_;
        self.tokens_out = out;
        self
    }
}

fn describe(kind: &EventKind) -> Described {
    match kind {
        EventKind::SessionCreated { id, title, .. } => {
            row(EventTag::System, format!("session {id} created · {title}")).payload(title.clone())
        }
        EventKind::SessionTitle { title } => {
            row(EventTag::System, format!("retitled · {title}")).payload(title.clone())
        }
        EventKind::SessionArchived => row(EventTag::System, "archived".into()),
        EventKind::TurnStart { turn_id, source } => row(
            EventTag::Turn,
            format!("turn {turn_id} started · {}", source.label()),
        ),
        EventKind::TurnEnd { turn_id, finish_reason, hops } => row(
            EventTag::Turn,
            format!("turn {turn_id} ended · {finish_reason} · {hops} hop(s)"),
        )
        .ok(finish_reason != "error"),
        EventKind::UserMessage { content, images, .. } => {
            let text = if images.is_empty() {
                one_line(content)
            } else {
                format!("{} · {} image(s)", one_line(content), images.len())
            };
            row(EventTag::User, text).payload(content.clone())
        }
        EventKind::AssistantMessage { content, reasoning, tool_calls, .. } => {
            let mut text = one_line(content);
            if text.is_empty() {
                text = "(no content)".into();
            }
            let payload = match tool_calls {
                Some(tc) => format!("{content}\n\ntool_calls: {tc}"),
                None => content.clone(),
            };
            row(EventTag::Assistant, text)
                .payload(payload)
                .result(reasoning.clone().unwrap_or_default())
        }
        EventKind::ToolCall { name, args_preview, expectation, args_json, .. } => row(
            EventTag::Tool,
            format!("{name} {}", one_line(args_preview)),
        )
        .payload(args_json.clone().unwrap_or_else(|| args_preview.clone()))
        .result(expectation.clone()),
        EventKind::ToolResult { call_seq, skill, ok, summary, pruned, trusted, .. } => {
            let mut text = format!("{skill} · {}", one_line(summary));
            if *pruned {
                text.push_str(" · pruned");
            }
            if !*trusted {
                text.push_str(" · untrusted");
            }
            row(EventTag::ToolResult, text)
                .result(summary.clone())
                .ok(*ok)
                .call(*call_seq)
        }
        EventKind::ContextInjected { source, content, .. } => row(
            EventTag::Context,
            format!("{} · {}", source.label(), one_line(content)),
        )
        .payload(content.clone()),
        EventKind::CompactionSummary {
            summary, folded, before_tokens, after_tokens, content, ..
        } => row(
            EventTag::Compacted,
            format!("folded {folded} · {before_tokens} → {after_tokens} tok"),
        )
        .payload(content.clone())
        .result(summary.clone())
        .tokens(*before_tokens, *after_tokens),
        EventKind::Rewind { start_seq, end_seq } => row(
            EventTag::Compacted,
            format!("rewound #{start_seq}–#{end_seq}"),
        ),
        EventKind::LlmRetry { attempt, max, delay_ms, reason } => row(
            EventTag::Retry,
            format!("attempt {attempt}/{max} failed · {reason} · retry in {delay_ms} ms"),
        )
        .payload(reason.clone())
        .ok(false),
        EventKind::TokenUsage { used, limit, budget, prompt_tokens, completion_tokens } => row(
            EventTag::Usage,
            format!(
                "{used} / {budget} tok (window {limit}){}",
                if prompt_tokens.is_some() { "" } else { " · estimated" }
            ),
        )
        .tokens(
            prompt_tokens.unwrap_or(*used),
            completion_tokens.unwrap_or(0),
        ),
        EventKind::LegacyMessage { message, .. } => row(
            EventTag::Other,
            format!("legacy {:?} · {}", message.role, one_line(&message.content)),
        )
        .payload(message.content.clone()),
        EventKind::Command { name, input, ok } => {
            row(EventTag::Command, format!("/{name} {}", one_line(input)))
                .payload(input.clone())
                .ok(*ok)
        }
        EventKind::Approval { skill, args_preview, decision } => {
            row(EventTag::Approval, format!("{skill} · {decision}"))
                .payload(args_preview.clone())
                .result(decision.clone())
                .ok(decision == "allowed-once")
        }
        EventKind::PermissionMode { mode } => row(
            EventTag::Command,
            format!("permission mode · {}", mode.label()),
        ),
        EventKind::PlanMode { active } => row(
            EventTag::Command,
            format!("plan mode · {}", if *active { "on" } else { "off" }),
        ),
        EventKind::TodoWrite { items } => {
            row(EventTag::Command, format!("todos · {} item(s)", items.len())).payload(
                items
                    .iter()
                    .map(|i| format!("[{}] {}", i.status.label(), i.content))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
        EventKind::GoalChange {
            goal_id,
            revision,
            objective,
            phase,
            rounds_started,
            max_rounds,
            blocker,
        } => row(
            EventTag::Goal,
            format!("goal {goal_id} r{revision} · {phase:?} · round {rounds_started}/{max_rounds}"),
        )
        .payload(objective.clone())
        .result(blocker.clone().unwrap_or_default()),
        EventKind::JobFinished { id, status, exit_code } => {
            let text = match exit_code {
                Some(c) => format!("job {id} · {status} · exit {c}"),
                None => format!("job {id} · {status}"),
            };
            row(EventTag::Job, text).ok(exit_code.map(|c| c == 0).unwrap_or(false))
        }
        EventKind::Unknown => row(
            EventTag::Other,
            "unknown event (written by a newer backend)".into(),
        ),
    }
}

/// One line, collapsed and clipped — the ledger's text column is 30 px tall
/// and never wraps.
fn one_line(s: &str) -> String {
    const CAP: usize = 160;
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= CAP {
        return flat;
    }
    let mut out: String = flat.chars().take(CAP - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use sica_core::event::ContextSource;

    fn log_with(kinds: Vec<EventKind>) -> SessionLog {
        let mut log = SessionLog::new(1, "t");
        for k in kinds {
            log.append(k);
        }
        log
    }

    fn user(content: &str) -> EventKind {
        EventKind::UserMessage {
            surface: SurfaceOp::Append,
            content: content.into(),
            images: vec![],
        }
    }

    #[test]
    fn pages_and_reports_the_next_seq() {
        let log = log_with(vec![
            EventKind::TurnStart { turn_id: 1, source: Default::default() },
            user("hi"),
            EventKind::TurnEnd { turn_id: 1, finish_reason: "stop".into(), hops: 1 },
        ]);
        let (rows, total, next) = page(&log, 1, 2);
        assert_eq!(total, 4);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].seq, 1);
        assert_eq!(next, Some(3));

        let (rest, _, next) = page(&log, 3, 100);
        assert_eq!(rest.len(), 2);
        assert_eq!(next, None);
    }

    #[test]
    fn carries_the_turn_id_onto_every_row_inside_the_turn() {
        let log = log_with(vec![
            EventKind::TurnStart { turn_id: 7, source: Default::default() },
            user("hi"),
            EventKind::TurnEnd { turn_id: 7, finish_reason: "stop".into(), hops: 1 },
            EventKind::SessionTitle { title: "after".into() },
        ]);
        let (rows, _, _) = page(&log, 0, 0);
        let by_seq = |s: u64| rows.iter().find(|r| r.seq == s).unwrap().clone();
        assert_eq!(by_seq(3).turn_id, Some(7));
        assert_eq!(by_seq(4).turn_id, Some(7), "the TurnEnd itself is in the turn");
        assert_eq!(by_seq(5).turn_id, None, "a row after TurnEnd belongs to no turn");
    }

    #[test]
    fn a_page_starting_mid_turn_still_knows_its_turn() {
        let log = log_with(vec![
            EventKind::TurnStart { turn_id: 3, source: Default::default() },
            user("hi"),
        ]);
        let (rows, _, _) = page(&log, 3, 10);
        assert_eq!(rows[0].seq, 3);
        assert_eq!(rows[0].turn_id, Some(3));
    }

    #[test]
    fn marks_shadowed_surface_events_and_names_the_span() {
        // A compaction replacing seqs 2..=3 shadows them; its own row is not
        // shadowed, and it names what it erased.
        let log = log_with(vec![
            user("one"),
            EventKind::AssistantMessage {
                surface: SurfaceOp::Append,
                content: "two".into(),
                reasoning: None,
                tool_calls: None,
            },
            EventKind::CompactionSummary {
                surface: SurfaceOp::Replace { start_seq: 2, end_seq: 3 },
                content: "<compacted-summary>".into(),
                summary: "s".into(),
                folded: 2,
                before_tokens: 100,
                after_tokens: 10,
            },
        ]);
        let (rows, _, _) = page(&log, 0, 0);
        let by_seq = |s: u64| rows.iter().find(|r| r.seq == s).unwrap().clone();
        assert!(by_seq(2).shadowed);
        assert!(by_seq(3).shadowed);
        assert!(!by_seq(4).shadowed);
        assert_eq!(by_seq(4).shadows, Some((2, 3)));
        assert_eq!(by_seq(4).tag, EventTag::Compacted);
    }

    #[test]
    fn a_rewind_shadows_its_span_without_contributing_a_message() {
        let log = log_with(vec![user("first"), EventKind::Rewind { start_seq: 2, end_seq: 2 }]);
        let (rows, _, _) = page(&log, 0, 0);
        assert!(rows.iter().find(|r| r.seq == 2).unwrap().shadowed);
        let rewind = rows.iter().find(|r| r.seq == 3).unwrap();
        assert!(!rewind.shadowed, "the rewind is bookkeeping, not surface");
        assert_eq!(rewind.shadows, Some((2, 2)));
    }

    #[test]
    fn a_tool_result_joins_its_call() {
        let log = log_with(vec![
            EventKind::ToolCall {
                name: "read-file".into(),
                args_preview: "'README.md'".into(),
                expectation: "the file".into(),
                call_id: None,
                args_json: Some(r#"{"path":"README.md"}"#.into()),
            },
            EventKind::ToolResult {
                surface: SurfaceOp::Append,
                call_seq: 2,
                skill: "read-file".into(),
                tool_call_id: None,
                ok: true,
                summary: "contents".into(),
                trusted: false,
                pruned: false,
            },
        ]);
        let (rows, _, _) = page(&log, 0, 0);
        let call = rows.iter().find(|r| r.seq == 2).unwrap();
        let result = rows.iter().find(|r| r.seq == 3).unwrap();
        assert_eq!(call.tag, EventTag::Tool);
        assert_eq!(call.payload, r#"{"path":"README.md"}"#);
        assert!(!call.shadowed, "a ToolCall never reaches the surface");
        assert_eq!(result.call_seq, Some(2));
        assert_eq!(result.ok, Some(true));
        assert!(result.text.contains("untrusted"));
    }

    #[test]
    fn usage_rows_carry_the_providers_own_numbers() {
        let log = log_with(vec![EventKind::TokenUsage {
            used: 1200,
            limit: 8192,
            budget: 3600,
            prompt_tokens: Some(1000),
            completion_tokens: Some(200),
        }]);
        let (rows, _, _) = page(&log, 2, 0);
        assert_eq!(rows[0].tag, EventTag::Usage);
        assert_eq!((rows[0].tokens_in, rows[0].tokens_out), (1000, 200));
        assert!(!rows[0].text.contains("estimated"));
    }

    #[test]
    fn a_heuristic_usage_row_says_so() {
        let log = log_with(vec![EventKind::TokenUsage {
            used: 900,
            limit: 8192,
            budget: 3600,
            prompt_tokens: None,
            completion_tokens: None,
        }]);
        let (rows, _, _) = page(&log, 2, 0);
        assert!(rows[0].text.contains("estimated"));
        assert_eq!(rows[0].tokens_in, 900);
    }

    #[test]
    fn context_rows_name_their_source() {
        let log = log_with(vec![EventKind::ContextInjected {
            surface: SurfaceOp::Append,
            source: ContextSource::SkillInvocation { name: "review".into() },
            content: "body".into(),
        }]);
        let (rows, _, _) = page(&log, 2, 0);
        assert_eq!(rows[0].tag, EventTag::Context);
        assert!(rows[0].text.starts_with("/review"));
    }

    #[test]
    fn the_raw_field_round_trips_the_event() {
        let log = log_with(vec![EventKind::SessionTitle { title: "x".into() }]);
        let (rows, _, _) = page(&log, 2, 0);
        let back: SessionEvent = serde_json::from_str(&rows[0].raw).unwrap();
        assert_eq!(back.seq, 2);
        assert_eq!(back.kind, EventKind::SessionTitle { title: "x".into() });
    }

    #[test]
    fn the_page_cap_is_enforced_over_the_clients_ask() {
        let log = log_with((0..MAX_PAGE + 50).map(|i| user(&format!("m{i}"))).collect());
        let (rows, _, next) = page(&log, 0, u32::MAX);
        assert_eq!(rows.len(), MAX_PAGE as usize);
        assert!(next.is_some());
    }
}
