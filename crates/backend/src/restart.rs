//! The restart brief (long-session-plan C2): what a backend restart owes the
//! model.
//!
//! The FE rebuilds and restarts the backend as a matter of course, and a
//! crash does the same without asking. Either way the process state is
//! gone — background jobs, the armed goal, the meter anchors — while the
//! log is intact up to the last flushed row. Three things in that log are
//! then wrong for the next request, and each is repaired here, at load,
//! by appending rows (never by editing):
//!
//! 1. **An open turn.** A `TurnStart` with no `TurnEnd` after it: closed
//!    with `finish_reason: "restart"` so the outline, the series and the
//!    verdict logic see a finished turn.
//! 2. **Dangling calls.** A `ToolCall` whose result never landed, or — in
//!    native mode — an assistant message whose `tool_calls` ids have no
//!    `tool` result: answered with a failed [`ABORTED_BY_RESTART`] result,
//!    so the next request's template does not carry a call with no answer.
//!    Only calls of the open turn qualify; an older gap was already what
//!    the model saw when that turn ran.
//! 3. **Lost jobs.** A background job the log says started and never says
//!    finished: marked `JobFinished { status: "lost" }`, whatever turn it
//!    was started in — the registry it lived in died with the process.
//!
//! [`repair`] is a pure function of the log. The brief the model reads is
//! composed by the hub from the [`Repair`] it returns, together with the
//! working notes and the todo list, and queued as injected context for the
//! session's next turn. Nothing here starts a turn.

use sica_core::event::{EventKind, SessionEvent, SurfaceOp};

use crate::sessions_store::SessionLog;

/// dsh's cancellation vocabulary for a call whose body may or may not have
/// run: the process ended before it reported.
pub const ABORTED_BY_RESTART: &str = "ABORTED_BY_RESTART";
/// `JobFinished.status` of a job the restart took with it.
pub const LOST: &str = "lost";
/// `TurnEnd.finish_reason` of a turn the restart cut.
pub const FINISH_RESTART: &str = "restart";

/// What [`repair`] found and fixed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Repair {
    /// The turn that was open, now closed.
    pub cut_turn: Option<u64>,
    /// Tool calls that turn had made before the cut.
    pub hops: u8,
    /// Calls answered as [`ABORTED_BY_RESTART`].
    pub dangling: usize,
    /// Background jobs marked [`LOST`].
    pub lost_jobs: Vec<String>,
}

impl Repair {
    pub fn is_empty(&self) -> bool {
        self.cut_turn.is_none() && self.dangling == 0 && self.lost_jobs.is_empty()
    }
}

/// Repair `log` in place (append-only) and say what was done.
pub fn repair(log: &mut SessionLog) -> Repair {
    let mut out = Repair::default();

    // 1 + 2: the open turn and its dangling calls.
    if let Some((turn_id, start_seq)) = open_turn(&log.events) {
        let after: Vec<&SessionEvent> = log.events.iter().filter(|e| e.seq > start_seq).collect();
        out.hops = after
            .iter()
            .filter(|e| matches!(e.kind, EventKind::ToolCall { .. }))
            .count()
            .min(u8::MAX as usize) as u8;

        // Logged calls whose result never landed.
        let answered: Vec<u64> = after
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::ToolResult { call_seq, .. } => Some(*call_seq),
                _ => None,
            })
            .collect();
        let answered_ids: Vec<String> = after
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::ToolResult { tool_call_id: Some(id), .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        let dangling: Vec<(u64, String, Option<String>)> = after
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::ToolCall { name, call_id, .. } if !answered.contains(&e.seq) => {
                    Some((e.seq, name.clone(), call_id.clone()))
                }
                _ => None,
            })
            .collect();
        // Native calls the assistant made that were never even logged as
        // dispatched (the cut fell between the message and the batch).
        let mut unlogged: Vec<(String, String, String)> = Vec::new();
        if let Some(EventKind::AssistantMessage { tool_calls: Some(json), .. }) =
            after.iter().rev().find_map(|e| match &e.kind {
                k @ EventKind::AssistantMessage { .. } => Some(k),
                _ => None,
            })
        {
            let logged_ids: Vec<String> = after
                .iter()
                .filter_map(|e| match &e.kind {
                    EventKind::ToolCall { call_id: Some(id), .. } => Some(id.clone()),
                    _ => None,
                })
                .collect();
            if let Ok(calls) = serde_json::from_str::<Vec<serde_json::Value>>(json) {
                for c in calls {
                    let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if id.is_empty() || logged_ids.contains(&id) || answered_ids.contains(&id) {
                        continue;
                    }
                    let name = c
                        .pointer("/function/name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let args = c
                        .pointer("/function/arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or("{}")
                        .to_string();
                    unlogged.push((id, name, args));
                }
            }
        }

        for (call_seq, skill, call_id) in dangling {
            log.append(aborted_result(call_seq, &skill, call_id));
            out.dangling += 1;
        }
        for (id, name, args) in unlogged {
            let call_seq = log.append(EventKind::ToolCall {
                name: name.clone(),
                args_preview: format!("{name} {args}"),
                expectation: String::new(),
                call_id: Some(id.clone()),
                args_json: Some(args),
            });
            log.append(aborted_result(call_seq, &name, Some(id)));
            out.dangling += 1;
        }
        log.append(EventKind::TurnEnd {
            turn_id,
            finish_reason: FINISH_RESTART.into(),
            hops: out.hops,
        });
        out.cut_turn = Some(turn_id);
    }

    // 3: jobs that started and never finished.
    for id in unfinished_jobs(&log.events) {
        log.append(EventKind::JobFinished { id: id.clone(), status: LOST.into(), exit_code: None });
        out.lost_jobs.push(id);
    }

    out
}

fn aborted_result(call_seq: u64, skill: &str, tool_call_id: Option<String>) -> EventKind {
    EventKind::ToolResult {
        surface: SurfaceOp::Append,
        call_seq,
        skill: skill.to_string(),
        tool_call_id,
        ok: false,
        summary: format!(
            "{ABORTED_BY_RESTART}: the backend restarted before this call reported; \
             whether it ran is unknown — check the workspace before repeating it"
        ),
        trusted: true,
        pruned: false,
    }
}

/// The newest `TurnStart` with no `TurnEnd` of the same id after it.
fn open_turn(events: &[SessionEvent]) -> Option<(u64, u64)> {
    let (turn_id, start_seq) = events.iter().rev().find_map(|e| match &e.kind {
        EventKind::TurnStart { turn_id, .. } => Some((*turn_id, e.seq)),
        _ => None,
    })?;
    let ended = events.iter().any(|e| {
        e.seq > start_seq
            && matches!(&e.kind, EventKind::TurnEnd { turn_id: t, .. } if *t == turn_id)
    });
    (!ended).then_some((turn_id, start_seq))
}

/// Ids of background jobs the log says started (`run-cli` / `run-pwsh`
/// with `background=true` answer "started job `<id>` …") and never says
/// finished.
fn unfinished_jobs(events: &[SessionEvent]) -> Vec<String> {
    let mut started: Vec<String> = Vec::new();
    let mut finished: Vec<String> = Vec::new();
    for e in events {
        match &e.kind {
            EventKind::ToolResult { ok: true, summary, .. } => {
                if let Some(id) = started_job_id(summary) {
                    if !started.contains(&id) {
                        started.push(id);
                    }
                }
            }
            EventKind::JobFinished { id, .. } => finished.push(id.clone()),
            _ => {}
        }
    }
    started.retain(|id| !finished.contains(id));
    started
}

/// The id in "started job `cli-3` in the background: …", or nothing.
fn started_job_id(summary: &str) -> Option<String> {
    let rest = summary.strip_prefix("started job `")?;
    let end = rest.find('`')?;
    let id = &rest[..end];
    (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .then(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sica_core::event::TurnSource;

    fn user(log: &mut SessionLog, text: &str) {
        log.append(EventKind::UserMessage {
            surface: SurfaceOp::Append,
            content: text.into(),
            images: Vec::new(),
        });
    }

    fn call(log: &mut SessionLog, name: &str, call_id: Option<&str>) -> u64 {
        log.append(EventKind::ToolCall {
            name: name.into(),
            args_preview: format!("{name} 'x'"),
            expectation: String::new(),
            call_id: call_id.map(str::to_string),
            args_json: None,
        })
    }

    fn result(log: &mut SessionLog, call_seq: u64, skill: &str, summary: &str) {
        log.append(EventKind::ToolResult {
            surface: SurfaceOp::Append,
            call_seq,
            skill: skill.into(),
            tool_call_id: None,
            ok: true,
            summary: summary.into(),
            trusted: true,
            pruned: false,
        });
    }

    #[test]
    fn a_finished_log_needs_nothing() {
        let mut log = SessionLog::new(1, "t");
        log.append(EventKind::TurnStart { turn_id: 1, source: TurnSource::Human });
        user(&mut log, "hi");
        let c = call(&mut log, "read-file", None);
        result(&mut log, c, "read-file", "contents");
        log.append(EventKind::TurnEnd { turn_id: 1, finish_reason: "done".into(), hops: 1 });
        let before = log.last_seq();
        let r = repair(&mut log);
        assert!(r.is_empty(), "{r:?}");
        assert_eq!(log.last_seq(), before, "nothing appended");
    }

    #[test]
    fn an_open_turn_is_closed_and_its_dangling_call_answered() {
        let mut log = SessionLog::new(1, "t");
        // An earlier, finished turn with an old gap that must be left alone.
        log.append(EventKind::TurnStart { turn_id: 1, source: TurnSource::Human });
        let old = call(&mut log, "run-cli", None);
        let _ = old;
        log.append(EventKind::TurnEnd { turn_id: 1, finish_reason: "error".into(), hops: 1 });
        // The cut turn: two calls, one answered.
        log.append(EventKind::TurnStart { turn_id: 2, source: TurnSource::Human });
        user(&mut log, "go");
        let a = call(&mut log, "read-file", None);
        result(&mut log, a, "read-file", "ok");
        let b = call(&mut log, "run-cli", None);
        let r = repair(&mut log);
        assert_eq!(r.cut_turn, Some(2));
        assert_eq!(r.hops, 2);
        assert_eq!(r.dangling, 1);
        assert!(r.lost_jobs.is_empty());
        let tail: Vec<&EventKind> = log.events.iter().rev().take(2).map(|e| &e.kind).collect();
        assert!(matches!(tail[0], EventKind::TurnEnd { turn_id: 2, finish_reason, hops: 2 } if finish_reason == FINISH_RESTART));
        assert!(matches!(tail[1], EventKind::ToolResult { call_seq, ok: false, summary, .. } if *call_seq == b && summary.starts_with(ABORTED_BY_RESTART)));
        // Idempotent: a second load finds a closed turn.
        let before = log.last_seq();
        assert!(repair(&mut log).is_empty());
        assert_eq!(log.last_seq(), before);
    }

    #[test]
    fn native_calls_never_dispatched_get_a_logged_call_and_a_failed_result() {
        let mut log = SessionLog::new(1, "t");
        log.append(EventKind::TurnStart { turn_id: 1, source: TurnSource::Human });
        user(&mut log, "go");
        log.append(EventKind::AssistantMessage {
            surface: SurfaceOp::Append,
            content: String::new(),
            reasoning: None,
            tool_calls: Some(
                r#"[{"id":"c1","type":"function","function":{"name":"read-file","arguments":"{\"path\":\"a\"}"}},
                    {"id":"c2","type":"function","function":{"name":"grep","arguments":"{}"}}]"#.into(),
            ),
        });
        // c1 was logged and answered; c2 never got that far.
        let c1 = call(&mut log, "read-file", Some("c1"));
        log.append(EventKind::ToolResult {
            surface: SurfaceOp::Append, call_seq: c1, skill: "read-file".into(),
            tool_call_id: Some("c1".into()), ok: true, summary: "ok".into(), trusted: false, pruned: false,
        });
        let r = repair(&mut log);
        assert_eq!(r.dangling, 1);
        let surface = log.derive_surface();
        let answered: Vec<String> = surface
            .iter()
            .filter_map(|e| e.message.tool_call_id.clone())
            .collect();
        assert_eq!(answered, vec!["c1".to_string(), "c2".to_string()]);
        assert!(surface.last().unwrap().message.content.contains(ABORTED_BY_RESTART));
    }

    #[test]
    fn started_jobs_without_a_finish_are_marked_lost() {
        let mut log = SessionLog::new(1, "t");
        log.append(EventKind::TurnStart { turn_id: 1, source: TurnSource::Human });
        let a = call(&mut log, "run-cli", None);
        result(&mut log, a, "run-cli", "started job `cli-1` in the background: cargo build\nread it with job-output");
        let b = call(&mut log, "run-cli", None);
        result(&mut log, b, "run-cli", "started job `cli-2` in the background: npm test");
        log.append(EventKind::JobFinished { id: "cli-1".into(), status: "exited 0".into(), exit_code: Some(0) });
        log.append(EventKind::TurnEnd { turn_id: 1, finish_reason: "done".into(), hops: 2 });
        let r = repair(&mut log);
        assert_eq!(r.cut_turn, None);
        assert_eq!(r.lost_jobs, vec!["cli-2".to_string()]);
        assert!(matches!(&log.events.last().unwrap().kind, EventKind::JobFinished { id, status, .. } if id == "cli-2" && status == LOST));
        assert!(repair(&mut log).is_empty());
    }

    #[test]
    fn job_ids_are_read_strictly() {
        assert_eq!(started_job_id("started job `cli-3` in the background: x"), Some("cli-3".into()));
        assert_eq!(started_job_id("started job `` in the background"), None);
        assert_eq!(started_job_id("job cli-3 finished"), None);
        assert_eq!(started_job_id("started job `../x`"), None);
    }
}
