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

use crate::event::{EventKind, RunState, SessionEvent};

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
            used, limit, budget, prompt_tokens, completion_tokens, ..
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

// ---------------------------------------------------------------------------
// Per-turn series (long-session-plan F3)
// ---------------------------------------------------------------------------

/// One turn of [`TurnSeries`]: the numbers that say how a long session is
/// behaving over time — is the prompt growing, is the first token getting
/// slower (a prefix that stopped caching), how often does compaction run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnStat {
    pub turn_id: u64,
    pub source: String,
    pub start_seq: u64,
    /// Hops from `TurnEnd`; `0` while the turn is open.
    pub hops: u8,
    /// `TurnEnd.finish_reason`, empty while the turn is open.
    pub finish_reason: String,
    /// The largest prompt the turn sent: the provider's count when it
    /// reported one, else the meter's `used`. The *largest* rather than
    /// the sum, because the question is how full the window got.
    pub prompt_tokens: u32,
    /// Completion tokens over the turn's hops (provider counts only).
    pub completion_tokens: u32,
    /// Time to first token of the turn's *first* hop. Later hops ride a
    /// warm prefix by construction; the first hop is where a cold prefix
    /// shows.
    pub ttft_ms: Option<u64>,
    /// Compaction summaries that landed during the turn.
    pub compactions: u32,
    /// Tool results pruned to head/tail windows during the turn.
    pub pruned: u32,
    pub retries: u32,
    pub tool_calls: u32,
}

/// What [`TurnSeries`] accumulates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeriesState {
    pub turns: Vec<TurnStat>,
    pub through_seq: u64,
}

/// One row of numbers per turn, oldest first (long-session-plan F3). This
/// is the instrument the plan's later waves are judged with: a `ttft_ms`
/// that climbs with `prompt_tokens` says the prefix is being re-read every
/// turn; one that stays flat says it is cached.
pub struct TurnSeries;

impl Projection for TurnSeries {
    type State = SeriesState;

    fn init() -> SeriesState {
        SeriesState::default()
    }

    fn apply(state: &mut SeriesState, ev: &SessionEvent) {
        state.through_seq = state.through_seq.max(ev.seq);
        match &ev.kind {
            EventKind::TurnStart { turn_id, source } => state.turns.push(TurnStat {
                turn_id: *turn_id,
                source: source.label().to_string(),
                start_seq: ev.seq,
                ..TurnStat::default()
            }),
            EventKind::TurnEnd { finish_reason, hops, .. } => {
                if let Some(row) = state.turns.last_mut() {
                    row.hops = *hops;
                    row.finish_reason = finish_reason.clone();
                }
            }
            EventKind::TokenUsage { used, prompt_tokens, completion_tokens, ttft_ms, .. } => {
                if let Some(row) = state.turns.last_mut() {
                    row.prompt_tokens = row.prompt_tokens.max(prompt_tokens.unwrap_or(*used));
                    row.completion_tokens += completion_tokens.unwrap_or(0);
                    if row.ttft_ms.is_none() {
                        row.ttft_ms = *ttft_ms;
                    }
                }
            }
            EventKind::CompactionSummary { .. } => {
                if let Some(row) = state.turns.last_mut() {
                    row.compactions += 1;
                }
            }
            EventKind::ToolResult { pruned: true, .. } => {
                if let Some(row) = state.turns.last_mut() {
                    row.pruned += 1;
                }
            }
            EventKind::ToolCall { .. } => {
                if let Some(row) = state.turns.last_mut() {
                    row.tool_calls += 1;
                }
            }
            EventKind::LlmRetry { .. } => {
                if let Some(row) = state.turns.last_mut() {
                    row.retries += 1;
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Orchestrated runs (UI guide §6.11)
// ---------------------------------------------------------------------------

/// One member of a run: a child agent the script drove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunMember {
    pub id:    u64,
    pub label: String,
    pub state: RunState,
}

/// A phase of a run. The unnamed phase — a script that never called
/// `phase()` — carries an empty title, and the reader sees its members
/// directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPhase {
    pub title:   String,
    pub members: Vec<RunMember>,
}

/// One orchestrated run, rebuilt from its four kinds of row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub run_id:   u64,
    /// The `ToolCall` this run belongs to.
    pub call_seq: u64,
    pub state:    RunState,
    /// `true` when the run started and neither finished nor failed. Live,
    /// that means running; after the turn it means **interrupted** — which
    /// is exactly the case four rows exist to make visible.
    pub open:     bool,
    pub phases:   Vec<RunPhase>,
}

/// Rebuild every orchestrated run in `events` (§6.11).
///
/// The rows are an append-only account of edges, so the fold is a replay:
/// a run row opens or closes the run, a member row opens or closes a member
/// inside the phase it named. A member end with no start is ignored rather
/// than invented — the log is the truth about what happened, and a row that
/// contradicts it is a bug to see, not to paper over.
pub fn workflow_runs(events: &[SessionEvent]) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    for ev in events {
        let EventKind::WorkflowRun { run_id, call_seq, phase, member, member_id, state } = &ev.kind
        else {
            continue;
        };
        let idx = match out.iter().position(|r| r.run_id == *run_id) {
            Some(i) => i,
            None => {
                // Only a run-level `Started` opens a run. A member row for a
                // run this build never saw the start of is a torn log, and
                // inventing the run around it would hide that.
                if member.is_some() {
                    continue;
                }
                out.push(Run {
                    run_id:   *run_id,
                    call_seq: *call_seq,
                    state:    RunState::Started,
                    open:     true,
                    phases:   Vec::new(),
                });
                out.len() - 1
            }
        };
        let run = &mut out[idx];
        let Some(label) = member.clone() else {
            // A run-level row: the run's own state.
            run.state = *state;
            run.open = matches!(state, RunState::Started);
            continue;
        };
        let title = phase.clone().unwrap_or_default();
        let phase_idx = match run.phases.iter().position(|p| p.title == title) {
            Some(i) => i,
            None => {
                run.phases.push(RunPhase { title, members: Vec::new() });
                run.phases.len() - 1
            }
        };
        let members = &mut run.phases[phase_idx].members;
        match state {
            RunState::Started => members.push(RunMember {
                id: member_id.unwrap_or(0),
                label,
                state: RunState::Started,
            }),
            done => {
                // An end finds its own start by id, falling back to the
                // newest still-running member of the same label.
                let at = member_id
                    .and_then(|id| members.iter().position(|m| m.id == id))
                    .or_else(|| {
                        members
                            .iter()
                            .rposition(|m| m.label == label && m.state == RunState::Started)
                    });
                if let Some(at) = at {
                    members[at].state = *done;
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Reminders (guide §12.8)
// ---------------------------------------------------------------------------

/// One active reminder, folded from the session's `Schedule` rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleRecord {
    pub id:            String,
    pub prompt:        String,
    /// `after` | `at` | `every`.
    pub rule:          String,
    /// Next target, unix seconds UTC. For an `every` record this is the
    /// earliest anchor-aligned occurrence not yet dispatched.
    pub fire_at:       i64,
    pub every_seconds: Option<u64>,
    /// The creation-time anchor an `every` record aligns to.
    pub anchor:        i64,
}

impl ScheduleRecord {
    /// The first anchor-aligned occurrence strictly after `now`, for an
    /// `every` record. Integer arithmetic over the interval, so a record
    /// that missed twenty intervals while the session was cold jumps
    /// straight to the twenty-first — nothing is enumerated or replayed.
    pub fn next_after(&self, now: i64) -> Option<i64> {
        let every = self.every_seconds? as i64;
        if every <= 0 {
            return None;
        }
        if now < self.anchor {
            return Some(self.anchor);
        }
        let elapsed = now - self.anchor;
        let steps = elapsed / every + 1;
        Some(self.anchor.checked_add(steps.checked_mul(every)?)?)
    }
}

/// Rebuild the active reminders in `events`.
///
/// The fold is strict about what it accepts, the way dsh's decoder is: a
/// `delete` or `dispatch` naming an inactive id, or a `create` reusing a
/// live one, is a torn log and is skipped rather than papered over.
pub fn schedules(events: &[SessionEvent]) -> Vec<ScheduleRecord> {
    let mut out: Vec<ScheduleRecord> = Vec::new();
    for ev in events {
        let EventKind::Schedule {
            id, op, prompt, rule, fire_at, after_seconds: _, every_seconds, accepted_at,
        } = &ev.kind
        else {
            continue;
        };
        let pos = out.iter().position(|r| &r.id == id);
        match op.as_str() {
            "create" => {
                if pos.is_some() {
                    continue;
                }
                let (Some(prompt), Some(rule), Some(fire_at)) = (prompt, rule, fire_at) else {
                    continue;
                };
                out.push(ScheduleRecord {
                    id:            id.clone(),
                    prompt:        prompt.clone(),
                    rule:          rule.clone(),
                    fire_at:       *fire_at,
                    every_seconds: *every_seconds,
                    anchor:        *fire_at,
                });
            }
            "delete" => {
                if let Some(i) = pos {
                    out.remove(i);
                }
            }
            "dispatch" => {
                let Some(i) = pos else { continue };
                match (out[i].every_seconds, accepted_at) {
                    // An `every` record advances past the decision time.
                    (Some(_), Some(at)) => match out[i].next_after(*at) {
                        Some(next) => out[i].fire_at = next,
                        None => {
                            out.remove(i);
                        }
                    },
                    // A one-shot dispatch is terminal.
                    _ => {
                        out.remove(i);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The latest rating per assistant message (guide §3.7): `seq_ref` →
/// `(rating, note)`, with a `0` rating clearing the entry.
pub fn feedback(events: &[SessionEvent]) -> std::collections::HashMap<u64, (i8, Option<String>)> {
    let mut out = std::collections::HashMap::new();
    for ev in events {
        if let EventKind::MessageFeedback { seq_ref, rating, note } = &ev.kind {
            if *rating == 0 {
                out.remove(seq_ref);
            } else {
                out.insert(*seq_ref, (*rating, note.clone()));
            }
        }
    }
    out
}

#[cfg(test)]
mod schedule_tests {
    use super::*;

    fn ev(seq: u64, kind: EventKind) -> SessionEvent {
        SessionEvent::now(seq, kind)
    }

    fn create(id: &str, rule: &str, fire_at: i64, every: Option<u64>) -> EventKind {
        EventKind::Schedule {
            id: id.into(),
            op: "create".into(),
            prompt: Some(format!("remind {id}")),
            rule: Some(rule.into()),
            fire_at: Some(fire_at),
            after_seconds: None,
            every_seconds: every,
            accepted_at: None,
        }
    }

    fn row(id: &str, op: &str, accepted_at: Option<i64>) -> EventKind {
        EventKind::Schedule {
            id: id.into(),
            op: op.into(),
            prompt: None,
            rule: None,
            fire_at: None,
            after_seconds: None,
            every_seconds: None,
            accepted_at,
        }
    }

    #[test]
    fn one_shot_dispatch_and_delete_are_terminal() {
        let events = vec![
            ev(1, create("s1", "after", 100, None)),
            ev(2, create("s2", "at", 200, None)),
            ev(3, row("s1", "dispatch", None)),
            ev(4, row("s2", "delete", None)),
            // Torn rows: a delete of something gone, a create reusing an id.
            ev(5, row("s1", "delete", None)),
            ev(6, create("s3", "at", 300, None)),
            ev(7, create("s3", "at", 999, None)),
        ];
        let active = schedules(&events);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "s3");
        assert_eq!(active[0].fire_at, 300);
    }

    #[test]
    fn every_dispatch_advances_to_the_first_aligned_target_after_the_decision() {
        let events = vec![
            ev(1, create("e", "every", 1000, Some(300))),
            // The session was cold for several intervals; one dispatch at
            // t=2050 advances straight to 2200, never 1300/1600/1900.
            ev(2, row("e", "dispatch", Some(2050))),
        ];
        let active = schedules(&events);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].fire_at, 2200);
        assert_eq!(active[0].anchor, 1000);
    }

    #[test]
    fn feedback_latest_wins_and_zero_clears() {
        let events = vec![
            ev(1, EventKind::MessageFeedback { seq_ref: 4, rating: 1, note: None }),
            ev(2, EventKind::MessageFeedback { seq_ref: 4, rating: -1, note: Some("wrong".into()) }),
            ev(3, EventKind::MessageFeedback { seq_ref: 9, rating: 1, note: None }),
            ev(4, EventKind::MessageFeedback { seq_ref: 9, rating: 0, note: None }),
        ];
        let f = feedback(&events);
        assert_eq!(f.get(&4).map(|(r, _)| *r), Some(-1));
        assert!(!f.contains_key(&9));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{SurfaceOp, TurnSource};

    fn run_row(seq: u64, phase: Option<&str>, member: Option<(u64, &str)>, state: RunState) -> SessionEvent {
        ev(seq, 1_000, EventKind::WorkflowRun {
            run_id: 1,
            call_seq: 42,
            phase: phase.map(str::to_string),
            member: member.map(|(_, l)| l.to_string()),
            member_id: member.map(|(id, _)| id),
            state,
        })
    }

    #[test]
    fn a_finished_run_rebuilds_its_phases_and_members() {
        let log = vec![
            run_row(1, None, None, RunState::Started),
            run_row(2, Some("Review"), Some((1, "review:bugs")), RunState::Started),
            run_row(3, Some("Review"), Some((2, "review:perf")), RunState::Started),
            run_row(4, Some("Review"), Some((1, "review:bugs")), RunState::Done),
            run_row(5, Some("Review"), Some((2, "review:perf")), RunState::Failed),
            run_row(6, Some("Verify"), Some((3, "verify:a")), RunState::Started),
            run_row(7, Some("Verify"), Some((3, "verify:a")), RunState::Done),
            run_row(8, None, None, RunState::Done),
        ];
        let runs = workflow_runs(&log);
        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert_eq!(run.call_seq, 42);
        assert_eq!(run.state, RunState::Done);
        assert!(!run.open);
        assert_eq!(run.phases.len(), 2);
        assert_eq!(run.phases[0].title, "Review");
        assert_eq!(run.phases[0].members.len(), 2);
        assert_eq!(run.phases[0].members[0].state, RunState::Done);
        assert_eq!(run.phases[0].members[1].state, RunState::Failed);
        assert_eq!(run.phases[1].members[0].label, "verify:a");
    }

    /// The reason there are four rows and not one summary: a run that was
    /// interrupted left its start behind and nothing else, and that has to
    /// read as interrupted rather than as never having happened.
    #[test]
    fn an_interrupted_run_stays_open_with_its_member_still_running() {
        let log = vec![
            run_row(1, None, None, RunState::Started),
            run_row(2, Some("Review"), Some((1, "review:bugs")), RunState::Started),
        ];
        let runs = workflow_runs(&log);
        assert_eq!(runs.len(), 1);
        assert!(runs[0].open, "a run with no terminal row is still open");
        assert_eq!(runs[0].state, RunState::Started);
        assert_eq!(runs[0].phases[0].members[0].state, RunState::Started);
    }

    #[test]
    fn a_script_without_phases_puts_its_members_in_one_unnamed_phase() {
        let log = vec![
            run_row(1, None, None, RunState::Started),
            run_row(2, None, Some((1, "agent-1")), RunState::Started),
            run_row(3, None, Some((1, "agent-1")), RunState::Done),
            run_row(4, None, None, RunState::Done),
        ];
        let runs = workflow_runs(&log);
        assert_eq!(runs[0].phases.len(), 1);
        assert_eq!(runs[0].phases[0].title, "");
        assert_eq!(runs[0].phases[0].members.len(), 1);
    }

    /// A member row for a run whose start is not in the log is a torn log.
    /// Inventing the run around it would hide that.
    #[test]
    fn a_member_without_its_run_is_dropped() {
        let log = vec![run_row(1, Some("P"), Some((1, "m")), RunState::Started)];
        assert!(workflow_runs(&log).is_empty());
    }
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
                format: crate::event::SESSION_FORMAT, cwd: None,
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
                prompt_tokens: Some(3_800), completion_tokens: Some(200), ttft_ms: None,
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
            prompt_tokens: None, completion_tokens: None, ttft_ms: None,
        }));
        let u = LastTokenUsage::fold(&events);
        assert!(u.seen);
        assert_eq!(u.used, 5_000);
        assert_eq!(u.prompt_tokens, None);
    }

    #[test]
    fn turn_series_reads_one_row_per_turn() {
        use crate::event::{SurfaceOp, TurnSource};
        let events = vec![
            ev(1, 1_000, EventKind::TurnStart { turn_id: 1, source: TurnSource::Human }),
            ev(2, 1_100, EventKind::TokenUsage {
                used: 900, limit: 8_000, budget: 6_000,
                prompt_tokens: Some(1_000), completion_tokens: Some(50), ttft_ms: Some(120),
            }),
            ev(3, 1_200, EventKind::ToolCall {
                name: "read-file".into(), args_preview: "read-file 'x'".into(),
                expectation: String::new(), call_id: None, args_json: None,
            }),
            ev(4, 1_300, EventKind::ToolResult {
                surface: SurfaceOp::Replace { start_seq: 3, end_seq: 3 }, call_seq: 3,
                skill: "read-file".into(), tool_call_id: None, ok: true,
                summary: "pruned".into(), trusted: false, pruned: true,
            }),
            ev(5, 1_400, EventKind::LlmRetry { attempt: 1, max: 5, delay_ms: 500, reason: "503".into() }),
            ev(6, 1_500, EventKind::TokenUsage {
                used: 1_500, limit: 8_000, budget: 6_000,
                prompt_tokens: Some(1_400), completion_tokens: Some(70), ttft_ms: Some(40),
            }),
            ev(7, 1_600, EventKind::CompactionSummary {
                surface: SurfaceOp::Replace { start_seq: 1, end_seq: 2 },
                content: "s".into(), summary: "s".into(), folded: 2, before_tokens: 0, after_tokens: 0,
            }),
            ev(8, 1_700, EventKind::TurnEnd { turn_id: 1, finish_reason: "done".into(), hops: 2 }),
            ev(9, 1_800, EventKind::TurnStart { turn_id: 2, source: TurnSource::GoalRound }),
            // No provider count: the meter's `used` stands in.
            ev(10, 1_900, EventKind::TokenUsage {
                used: 2_000, limit: 8_000, budget: 6_000,
                prompt_tokens: None, completion_tokens: None, ttft_ms: None,
            }),
        ];
        let s = TurnSeries::fold(&events);
        assert_eq!(s.turns.len(), 2);
        let t1 = &s.turns[0];
        assert_eq!((t1.turn_id, t1.source.as_str(), t1.hops, t1.finish_reason.as_str()), (1, "human", 2, "done"));
        assert_eq!(t1.prompt_tokens, 1_400, "the largest prompt, not the sum");
        assert_eq!(t1.completion_tokens, 120);
        assert_eq!(t1.ttft_ms, Some(120), "the first hop's, not the warm one's");
        assert_eq!((t1.compactions, t1.pruned, t1.retries, t1.tool_calls), (1, 1, 1, 1));
        let t2 = &s.turns[1];
        assert_eq!((t2.source.as_str(), t2.hops, t2.prompt_tokens, t2.ttft_ms), ("goal round", 0, 2_000, None));
        assert_eq!(s.through_seq, 10);
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
