//! Session projections (guide §3.3): pure folds over the event log.
//!
//! A projection is a fold `{ init, apply(state, event) }` that turns the
//! append-only log into a finished typed value a client can render without
//! knowing anything about `EventKind`. The rule that makes them safe is the
//! same one that makes the log worth keeping: **nothing here reads outside
//! the log**, so two observers folding the same rows always agree, and a
//! projection can be recomputed from scratch at any time.
//!
//! dsh persists checkpoints in a projection cache ("may be stale — its
//! `seq` says how stale — but never wrong"). At our session sizes the fold
//! is microseconds over a few thousand rows, so we fold on demand and keep
//! the `seq` the state was folded through on the state itself — a client
//! that wants to know how current a value is reads that.
//!
//! Three ship here:
//!
//! - [`SessionStats`] — the counters under a session title.
//! - [`TurnOutline`] — one row per turn, the "jump to turn" list.
//! - [`LastTokenUsage`] — the newest `TokenUsage`, for the context meter.

use crate::event::{EventKind, SessionEvent};

/// A pure fold over the log.
///
/// Implementors are zero-sized markers: the state is the value, the impl is
/// the transition. `fold` is the only entry point most callers need.
pub trait Projection {
    /// What the fold accumulates. Also what clients read.
    type State;

    /// The value of an empty log.
    fn init() -> Self::State;

    /// Advance by one event. Must be a pure function of `(state, ev)` —
    /// no clock, no filesystem, no ambient state.
    fn apply(state: &mut Self::State, ev: &SessionEvent);

    /// Fold a whole log. Events are expected in seq order, which is the
    /// order the JSONL holds them in.
    fn fold(events: &[SessionEvent]) -> Self::State {
        let mut state = Self::init();
        for ev in events {
            Self::apply(&mut state, ev);
        }
        state
    }
}

/// What [`SessionStats`] accumulates.
///
/// Counts *events*, not surface entries: a message that compaction later
/// shadowed still happened, and a stats line that shrank when the context
/// was compacted would be lying about the session's history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatsState {
    pub user_msgs:     u32,
    pub assistant_msgs: u32,
    pub tool_calls:    u32,
    /// `ToolResult`s with `ok: false`. Denials count — from the log's point
    /// of view a refused call is a call that did not do its work.
    pub tool_failures: u32,
    /// `LlmRetry` events: failed attempts that were re-run.
    pub retries:       u32,
    pub turns:         u32,
    /// Wall time from the first event to the last, in milliseconds.
    pub wall_ms:       i64,
    /// Seq this state was folded through — how current the value is.
    pub through_seq:   u64,
    first_ts:          Option<i64>,
    last_ts:           i64,
}

/// Counters for the line under a session title (guide §3.3).
pub struct SessionStats;

impl Projection for SessionStats {
    type State = StatsState;

    fn init() -> StatsState {
        StatsState::default()
    }

    fn apply(state: &mut StatsState, ev: &SessionEvent) {
        state.through_seq = state.through_seq.max(ev.seq);
        if state.first_ts.is_none() {
            state.first_ts = Some(ev.ts);
        }
        state.last_ts = state.last_ts.max(ev.ts);
        state.wall_ms = state.last_ts - state.first_ts.unwrap_or(state.last_ts);

        match &ev.kind {
            EventKind::UserMessage { .. } => state.user_msgs += 1,
            EventKind::AssistantMessage { .. } => state.assistant_msgs += 1,
            EventKind::ToolCall { .. } => state.tool_calls += 1,
            EventKind::ToolResult { ok, .. } => {
                if !ok {
                    state.tool_failures += 1;
                }
            }
            EventKind::LlmRetry { .. } => state.retries += 1,
            EventKind::TurnStart { .. } => state.turns += 1,
            _ => {}
        }
    }
}

/// One turn of [`TurnOutline`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRow {
    pub turn_id: u64,
    /// First line of the user message that opened the turn, trimmed. Empty
    /// for a turn no user message followed (a goal round opens one).
    pub first_user_line: String,
    /// Who opened it (`human` / `goal round` / `followup`).
    pub source: String,
    /// Hops the turn took, from its `TurnEnd`. `0` while it is running.
    pub hops: u8,
    /// `TurnEnd.finish_reason`, or empty while the turn is still open.
    pub finish_reason: String,
    /// Seq of the `TurnStart` — the handle a "jump to turn" click carries.
    pub start_seq: u64,
    pub ts_start: i64,
    /// `ts` of the `TurnEnd`, or of the newest event in the turn while it
    /// is still running.
    pub ts_end: i64,
    pub tool_calls: u32,
}

/// What [`TurnOutline`] accumulates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutlineState {
    pub turns: Vec<TurnRow>,
    pub through_seq: u64,
}

/// One row per turn, for the sidebar's jump list (guide §3.3).
pub struct TurnOutline;

/// First non-empty line of `text`, capped at `max` chars (not bytes — the
/// cap exists so a row fits, and a char boundary is what `truncate` needs).
fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if line.chars().count() <= max {
        return line.to_string();
    }
    let cut: String = line.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

impl Projection for TurnOutline {
    type State = OutlineState;

    fn init() -> OutlineState {
        OutlineState::default()
    }

    fn apply(state: &mut OutlineState, ev: &SessionEvent) {
        state.through_seq = state.through_seq.max(ev.seq);
        match &ev.kind {
            EventKind::TurnStart { turn_id, source } => {
                state.turns.push(TurnRow {
                    turn_id: *turn_id,
                    first_user_line: String::new(),
                    source: source.label().to_string(),
                    hops: 0,
                    finish_reason: String::new(),
                    start_seq: ev.seq,
                    ts_start: ev.ts,
                    ts_end: ev.ts,
                    tool_calls: 0,
                });
            }
            EventKind::TurnEnd { turn_id, finish_reason, hops } => {
                // Address by id rather than by position: a log can hold a
                // `TurnEnd` whose `TurnStart` predates a truncated page.
                if let Some(row) = state.turns.iter_mut().rev().find(|r| r.turn_id == *turn_id) {
                    row.finish_reason = finish_reason.clone();
                    row.hops = *hops;
                    row.ts_end = ev.ts;
                }
            }
            EventKind::UserMessage { content, .. } => {
                if let Some(row) = state.turns.last_mut() {
                    row.ts_end = row.ts_end.max(ev.ts);
                    if row.first_user_line.is_empty() {
                        row.first_user_line = first_line(content, 60);
                    }
                }
            }
            EventKind::ToolCall { .. } => {
                if let Some(row) = state.turns.last_mut() {
                    row.tool_calls += 1;
                    row.ts_end = row.ts_end.max(ev.ts);
                }
            }
            _ => {
                if let Some(row) = state.turns.last_mut() {
                    row.ts_end = row.ts_end.max(ev.ts);
                }
            }
        }
    }
}

/// What [`LastTokenUsage`] accumulates: the newest `TokenUsage`, or `None`
/// on a log that has not finished a hop yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageState {
    pub used: u32,
    pub limit: u32,
    pub budget: u32,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    /// `false` until the log carries a `TokenUsage`.
    pub seen: bool,
    pub through_seq: u64,
}

/// The newest prompt-size reading (guide §3.3).
pub struct LastTokenUsage;

impl Projection for LastTokenUsage {
    type State = UsageState;

    fn init() -> UsageState {
        UsageState::default()
    }

    fn apply(state: &mut UsageState, ev: &SessionEvent) {
        state.through_seq = state.through_seq.max(ev.seq);
        if let EventKind::TokenUsage {
            used, limit, budget, prompt_tokens, completion_tokens,
        } = &ev.kind
        {
            state.used = *used;
            state.limit = *limit;
            state.budget = *budget;
            state.prompt_tokens = *prompt_tokens;
            state.completion_tokens = *completion_tokens;
            state.seen = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{SurfaceOp, TurnSource};

    fn ev(seq: u64, ts: i64, kind: EventKind) -> SessionEvent {
        SessionEvent { seq, ts, kind }
    }

    fn user(seq: u64, ts: i64, text: &str) -> SessionEvent {
        ev(seq, ts, EventKind::UserMessage {
            surface: SurfaceOp::Append,
            content: text.into(),
            images:  Vec::new(),
        })
    }

    fn log() -> Vec<SessionEvent> {
        vec![
            ev(1, 1_000, EventKind::SessionCreated {
                id: 1, title: "Session 1".into(), created_at: 1_000,
            }),
            ev(2, 1_100, EventKind::TurnStart { turn_id: 1, source: TurnSource::Human }),
            user(3, 1_100, "  \nlist the crates\nand say why"),
            ev(4, 1_200, EventKind::ToolCall {
                name: "run-cli".into(),
                args_preview: "ls".into(),
                expectation: "listing".into(),
                call_id: None,
                args_json: None,
            }),
            ev(5, 1_300, EventKind::ToolResult {
                surface: SurfaceOp::Append,
                call_seq: 4,
                skill: "run-cli".into(),
                tool_call_id: None,
                ok: false,
                summary: "no such command".into(),
                trusted: false,
                pruned: false,
            }),
            ev(6, 1_400, EventKind::LlmRetry {
                attempt: 1, max: 3, delay_ms: 500, reason: "500".into(),
            }),
            ev(7, 1_500, EventKind::AssistantMessage {
                surface: SurfaceOp::Append,
                content: "seven crates".into(),
                reasoning: None,
                tool_calls: None,
            }),
            ev(8, 1_600, EventKind::TokenUsage {
                used: 4_000, limit: 32_000, budget: 25_600,
                prompt_tokens: Some(3_800), completion_tokens: Some(200),
            }),
            ev(9, 1_700, EventKind::TurnEnd {
                turn_id: 1, finish_reason: "stop".into(), hops: 2,
            }),
        ]
    }

    #[test]
    fn stats_count_events_not_surface_entries() {
        let s = SessionStats::fold(&log());
        assert_eq!(s.user_msgs, 1);
        assert_eq!(s.assistant_msgs, 1);
        assert_eq!(s.tool_calls, 1);
        assert_eq!(s.tool_failures, 1);
        assert_eq!(s.retries, 1);
        assert_eq!(s.turns, 1);
        assert_eq!(s.wall_ms, 700);
        assert_eq!(s.through_seq, 9);
    }

    #[test]
    fn a_compacted_span_does_not_shrink_the_counters() {
        // The point of counting events: the session still had those turns.
        let mut events = log();
        let before = SessionStats::fold(&events);
        events.push(ev(10, 1_800, EventKind::CompactionSummary {
            surface: SurfaceOp::Replace { start_seq: 3, end_seq: 7 },
            content: "[summary]".into(),
            summary: "summary".into(),
            folded: 4,
            before_tokens: 4_000,
            after_tokens: 900,
        }));
        let after = SessionStats::fold(&events);
        assert_eq!(before.user_msgs, after.user_msgs);
        assert_eq!(before.assistant_msgs, after.assistant_msgs);
    }

    #[test]
    fn the_outline_takes_the_first_non_empty_user_line() {
        let o = TurnOutline::fold(&log());
        assert_eq!(o.turns.len(), 1);
        let row = &o.turns[0];
        assert_eq!(row.first_user_line, "list the crates");
        assert_eq!(row.source, "human");
        assert_eq!(row.hops, 2);
        assert_eq!(row.finish_reason, "stop");
        assert_eq!(row.start_seq, 2);
        assert_eq!(row.tool_calls, 1);
        assert_eq!(row.ts_start, 1_100);
        assert_eq!(row.ts_end, 1_700);
    }

    #[test]
    fn a_running_turn_has_no_finish_reason_and_ends_at_its_newest_event() {
        let mut events = log();
        events.truncate(8); // drop the TurnEnd
        let o = TurnOutline::fold(&events);
        let row = &o.turns[0];
        assert_eq!(row.hops, 0);
        assert!(row.finish_reason.is_empty());
        assert_eq!(row.ts_end, 1_600);
    }

    #[test]
    fn a_turn_end_finds_its_start_by_id_not_by_position() {
        let events = vec![
            ev(1, 10, EventKind::TurnStart { turn_id: 7, source: TurnSource::Human }),
            ev(2, 20, EventKind::TurnStart { turn_id: 8, source: TurnSource::GoalRound }),
            ev(3, 30, EventKind::TurnEnd {
                turn_id: 7, finish_reason: "stop".into(), hops: 1,
            }),
        ];
        let o = TurnOutline::fold(&events);
        assert_eq!(o.turns[0].finish_reason, "stop");
        assert!(o.turns[1].finish_reason.is_empty());
        assert_eq!(o.turns[1].source, "goal round");
    }

    #[test]
    fn usage_keeps_the_newest_reading() {
        let mut events = log();
        events.push(ev(10, 1_900, EventKind::TokenUsage {
            used: 5_000, limit: 32_000, budget: 25_600,
            prompt_tokens: None, completion_tokens: None,
        }));
        let u = LastTokenUsage::fold(&events);
        assert!(u.seen);
        assert_eq!(u.used, 5_000);
        assert_eq!(u.prompt_tokens, None);
    }

    #[test]
    fn an_empty_log_folds_to_the_zero_value() {
        assert_eq!(SessionStats::fold(&[]), StatsState::default());
        assert_eq!(TurnOutline::fold(&[]).turns.len(), 0);
        assert!(!LastTokenUsage::fold(&[]).seen);
    }

    #[test]
    fn folding_is_incremental() {
        // The property the cache would rely on: applying the tail to the
        // state folded from the head equals folding the whole log.
        let events = log();
        let (head, tail) = events.split_at(5);
        let mut state = SessionStats::fold(head);
        for e in tail {
            SessionStats::apply(&mut state, e);
        }
        assert_eq!(state, SessionStats::fold(&events));
    }
}
