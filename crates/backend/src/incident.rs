//! The one place the backend reports a failure to the idealist.
//!
//! Before this, only failed tool calls and FE panics became tickets; a turn
//! that ended on an LLM error, a prompt that failed to assemble, a backend
//! panic or an invariant violation was an ERROR `LogLine` and nothing more.
//! Every one of those now goes through [`report`] as well, which:
//!
//! 1. names the ticket (the id is derived from the failure's fingerprint,
//!    so no round-trip to the daemon is needed),
//! 2. the first time that ticket fires in a session, appends a
//!    `TicketOpened` row to the session log — where the end-of-session
//!    investigator will look,
//! 3. notes it in the session's ledger (`idealist::ledger`), and
//! 4. publishes the trigger for the daemon to file.
//!
//! A process global, like `invariants::ENABLED`, because the call sites
//! are inside the turn task, a panic hook and a `ToolFailureSink` — none of
//! which can reach the hub. Not installed (tests, tools) means every call
//! is a no-op, so a unit test never writes into the real workspace.

use std::collections::HashMap;
use std::sync::OnceLock;

use idealist::{Ledger, Trigger, TriggerBus, TriggerOrigin};
use sica_core::event::EventKind;
use tokio::sync::Mutex;
use tracing::warn;

use crate::chat::{append_event, Sessions};

struct Reporter {
    bus:      TriggerBus,
    sessions: Sessions,
    ledger:   Ledger,
    handle:   tokio::runtime::Handle,
    /// Serialises "is this the first time in this session?" with the append
    /// that answers it, so two failures in the same breath log one row.
    order:    Mutex<()>,
}

static REPORTER: OnceLock<Reporter> = OnceLock::new();

/// Wire the reporter. Called once from `main`, inside the runtime.
pub fn install(bus: TriggerBus, sessions: Sessions) {
    let _ = REPORTER.set(Reporter {
        bus,
        sessions,
        ledger: Ledger::open_default(),
        handle: tokio::runtime::Handle::current(),
        order: Mutex::new(()),
    });
}

pub fn installed() -> bool {
    REPORTER.get().is_some()
}

/// Report one failure. Never blocks and never fails: reporting a problem
/// must not become a second problem.
pub fn report(t: Trigger) {
    let Some(r) = REPORTER.get() else { return };
    if t.session_id.is_none() {
        r.bus.publish(t);
        return;
    }
    r.handle.spawn(file(r, t));
}

/// Log, ledger, publish — in that order, so the `TicketOpened` row is in
/// the log before the daemon hears about it.
async fn file(r: &'static Reporter, mut t: Trigger) {
    let Some(session_id) = t.session_id else {
        r.bus.publish(t);
        return;
    };
    let _g = r.order.lock().await;
    let id = idealist::ticket::ticket_id(&t);
    let seq = if r.ledger.is_new(session_id, &id) {
        append_event(&r.sessions, session_id, EventKind::TicketOpened {
            ticket_id: id.clone(),
            origin:    t.origin.as_str().into(),
            module:    t.module.clone(),
            turn_id:   t.turn_id,
        })
        .await
    } else {
        let g = r.sessions.lock().await;
        g.get(&session_id).and_then(|l| l.events.last()).map(|e| e.seq)
    };
    let skill = idealist::ticket::tool_skill(&t.module);
    if let Err(e) = r.ledger.record(session_id, &id, t.origin, skill, seq) {
        warn!(session_id, error = %e, "incident: ledger write failed");
    }
    t.seq = seq;
    r.bus.publish(t);
}

/// A turn ended with `finish_reason = "error"`. `reason` becomes the module
/// suffix (`backend::turn::<reason>`), so each distinct way a turn dies is
/// its own ticket. Awaited rather than spawned so the `TicketOpened` row
/// lands before the turn's `TurnEnd` — the investigator reads the log in
/// order.
pub async fn turn_error(session_id: u64, turn_id: u64, reason: &str, message: impl Into<String>) {
    let Some(r) = REPORTER.get() else { return };
    file(r, Trigger {
        kind:       "turn_error".into(),
        module:     format!("backend::turn::{reason}"),
        message:    message.into(),
        origin:     TriggerOrigin::TurnError,
        session_id: Some(session_id),
        turn_id:    Some(turn_id),
        ..Default::default()
    })
    .await;
}

/// A failure outside any turn (a connect, a config file).
pub fn outside_turn(origin: TriggerOrigin, module: &str, message: impl Into<String>) {
    report(Trigger {
        kind: origin.as_str().into(),
        module: module.into(),
        message: message.into(),
        origin,
        ..Default::default()
    });
}

/// Fold a finished turn into the session's ledger: for each skill that
/// failed during it, whether a later call of the same skill succeeded.
pub async fn turn_ended(session_id: u64, turn_id: u64) {
    let Some(r) = REPORTER.get() else { return };
    let (since, outcome): (u64, HashMap<String, bool>) = {
        let g = r.sessions.lock().await;
        let Some(log) = g.get(&session_id) else { return };
        let Some(start) = log.events.iter().rposition(|e| {
            matches!(e.kind, EventKind::TurnStart { turn_id: t, .. } if t == turn_id)
        }) else {
            return;
        };
        let results = log.events[start..].iter().filter_map(|e| match &e.kind {
            EventKind::ToolResult { skill, ok, pruned: false, .. } => Some((skill.as_str(), *ok)),
            _ => None,
        });
        (log.events[start].seq, idealist::ledger::turn_recovery(results))
    };
    if outcome.is_empty() {
        return;
    }
    let ledger = r.ledger.clone();
    let _ = tokio::task::spawn_blocking(move || {
        if let Err(e) = ledger.apply_turn(session_id, since, &outcome) {
            warn!(session_id, error = %e, "incident: ledger turn update failed");
        }
    })
    .await;
}

/// Chain a panic hook that files every backend panic. Writes a crash file
/// synchronously first: after a panic the runtime (and so the daemon) may
/// already be gone, and the file is the only record that survives it.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "panic with a non-string payload".to_string()
        };
        let (file, line) = info
            .location()
            .map(|l| (l.file().replace('\\', "/"), l.line()))
            .unwrap_or_else(|| ("unknown".into(), 0));
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let traceback = format!("at {file}:{line} (thread {thread})\n\n{backtrace}");

        let dir = sica_core::paths::idealist_workspace();
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            dir.join(format!("crash-{stamp}.md")),
            format!("# Backend panic\n\n```\n{message}\n```\n\n```\n{traceback}\n```\n"),
        );

        // Module carries the file, not the line: the same panic after an
        // unrelated edit above it is still the same ticket.
        report(Trigger {
            kind:      "panic".into(),
            module:    format!("backend::panic::{file}"),
            message,
            traceback: Some(traceback),
            origin:    TriggerOrigin::Panic,
            ..Default::default()
        });
        previous(info);
    }));
}
