//! Idealist daemon: files every failure the harness reports as an
//! improvement ticket, one ticket per *kind* of failure.
//!
//! - [`trigger_bus`] carries `Trigger`s from wherever things go wrong.
//! - [`classifier`] and [`analyzer`] give each one a source, a category and
//!   a heuristic fix (a skill swap, where one is known).
//! - [`ticket`] files it: a fingerprint-derived id, so repeats bump one
//!   ticket and a resolved ticket that fires again reopens.
//! - [`ledger`] remembers which tickets each session raised, for the
//!   end-of-session investigator (`backend::investigate`), which is the only
//!   part that talks to an LLM and so lives in `backend`.
//! - [`lessons`] keeps what investigations taught, for the prompt.
//!
//! Nothing here edits source. FE tickets are never auto-patched.

pub mod analyzer;
pub mod classifier;
pub mod config;
pub mod ledger;
pub mod lessons;
pub mod ticket;
pub mod trigger_bus;

pub use classifier::{classify, TriggerSource};
pub use config::IdealistConfig;
pub use ledger::{Ledger, LedgerEntry, SessionLedger};
pub use ticket::{Diagnosis, Ticket, TicketStatus, TicketStore};
pub use trigger_bus::{Trigger, TriggerBus, TriggerOrigin};

use std::sync::Arc;

use protocol::{Event, TicketSummary};
use tokio::sync::Mutex;
use tracing::{info, warn};

pub trait IdealistEventSink: Send + Sync {
    fn emit(&self, ev: Event);
}

pub struct Settings {
    pub auto_apply_be: bool,
}

pub struct Idealist {
    pub bus:      TriggerBus,
    pub settings: Arc<Mutex<Settings>>,
    pub events:   Arc<dyn IdealistEventSink>,
    pub store:    TicketStore,
}

/// The wire shape of a ticket.
pub fn summary(t: &Ticket, store: &TicketStore) -> TicketSummary {
    let m = &t.meta;
    TicketSummary {
        id:           m.id.clone(),
        status:       m.status.as_str().into(),
        origin:       m.origin.as_str().into(),
        module:       m.module.clone(),
        severity:     m.severity.clone(),
        occurrences:  m.occurrences,
        regressions:  m.regressions,
        first_seen:   m.first_seen.clone(),
        last_seen:    m.last_seen.clone(),
        sessions:     m.sessions.clone(),
        last_message: m.last_message.clone(),
        category:     m.diagnosis.as_ref().map(|d| d.category.clone()),
        confidence:   m.diagnosis.as_ref().map(|d| d.confidence.clone()),
        lesson:       m.diagnosis.as_ref().and_then(|d| d.lesson.clone()),
        path:         store.path(&m.id).to_string_lossy().into_owned(),
    }
}

impl Idealist {
    pub fn new(events: Arc<dyn IdealistEventSink>) -> Self {
        Self::with_store(events, TicketStore::open_default())
    }

    pub fn with_store(events: Arc<dyn IdealistEventSink>, store: TicketStore) -> Self {
        Self {
            bus: TriggerBus::new(),
            settings: Arc::new(Mutex::new(Settings { auto_apply_be: false })),
            events,
            store,
        }
    }

    /// File one trigger. Public so a test (or a caller with no daemon) can
    /// run the same path synchronously.
    pub fn handle(&self, trigger: &Trigger) {
        if trigger.origin == TriggerOrigin::Investigator {
            // The investigator's own failures are its run's problem; filing
            // them would let one investigation schedule the next.
            return;
        }
        info!(
            kind = %trigger.kind,
            module = %trigger.module,
            origin = trigger.origin.as_str(),
            "idealist: received trigger"
        );
        let src = classify(trigger);
        let analysis = analyzer::analyze(trigger);
        match self.store.upsert(trigger, &analysis, src) {
            Ok(up) => {
                let path = up.path.to_string_lossy().to_string();
                let kind = match src {
                    TriggerSource::Frontend => protocol::TicketKind::FeBug,
                    _ => protocol::TicketKind::BeFix,
                };
                if up.created || up.reopened {
                    let what = if up.reopened { "reopened (regression)" } else { "opened" };
                    info!(id = %up.id, path = %path, ?kind, "idealist: ticket {what}");
                    self.events.emit(Event::LogLine {
                        level:   if up.reopened { "WARN".into() } else { "INFO".into() },
                        message: format!(
                            "idealist: ticket {} {what} — {} · {}",
                            up.id, trigger.module, path
                        ),
                    });
                }
                self.events.emit(Event::IdealistTicketWritten {
                    path: path.clone(),
                    kind,
                    ticket_id: up.id.clone(),
                    occurrences: up.occurrences,
                    reopened: up.reopened,
                });
                self.events.emit(Event::IdealistStatus {
                    activity: "idle".into(),
                    severity: protocol::Severity::Info,
                    last_ticket: Some(path),
                });
            }
            Err(e) => {
                warn!(error = %e, "idealist: filing ticket failed");
                self.events.emit(Event::LogLine {
                    level:   "ERROR".into(),
                    message: format!("idealist: filing ticket failed — {e}"),
                });
            }
        }
    }

    /// Spawn the daemon loop. Returns after the bus is dropped or the task is
    /// aborted by the runtime.
    pub fn spawn(self: Arc<Self>) {
        let me = self;
        tokio::spawn(async move {
            let bus_rx = me.bus.subscribe();
            loop {
                let trigger = match tokio::task::spawn_blocking({
                    let rx = bus_rx.clone();
                    move || rx.recv().ok()
                })
                .await
                {
                    Ok(Some(t)) => t,
                    _ => {
                        info!("idealist: trigger bus closed — daemon loop exiting");
                        break;
                    }
                };
                let me2 = Arc::clone(&me);
                // File I/O under a std mutex: off the async workers.
                let _ = tokio::task::spawn_blocking(move || me2.handle(&trigger)).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct Capture(StdMutex<Vec<Event>>);
    impl IdealistEventSink for Capture {
        fn emit(&self, ev: Event) {
            self.0.lock().unwrap().push(ev);
        }
    }

    #[test]
    fn handle_files_and_reports_repeats() {
        let sink = Arc::new(Capture::default());
        let store = TicketStore::at(ticket::tests::scratch("daemon"));
        let d = Idealist::with_store(sink.clone(), store.clone());
        let t = Trigger {
            kind: "turn_error".into(),
            module: "backend::turn::llm".into(),
            message: "LLM request failed (500) — giving up after 3 retries".into(),
            origin: TriggerOrigin::TurnError,
            session_id: Some(5),
            ..Default::default()
        };
        d.handle(&t);
        d.handle(&t);
        let evs = sink.0.lock().unwrap();
        let written: Vec<u32> = evs
            .iter()
            .filter_map(|e| match e {
                Event::IdealistTicketWritten { occurrences, .. } => Some(*occurrences),
                _ => None,
            })
            .collect();
        assert_eq!(written, vec![1, 2]);
        let list = store.list();
        assert_eq!(list.len(), 1);
        let s = summary(&list[0], &store);
        assert_eq!(s.status, "open");
        assert_eq!(s.origin, "turn_error");
        assert_eq!(s.sessions, vec![5]);
    }

    #[test]
    fn investigator_triggers_are_dropped() {
        let sink = Arc::new(Capture::default());
        let store = TicketStore::at(ticket::tests::scratch("daemon-inv"));
        let d = Idealist::with_store(sink.clone(), store.clone());
        d.handle(&Trigger {
            module: "agents::tool::read-file".into(),
            message: "boom".into(),
            origin: TriggerOrigin::Investigator,
            ..Default::default()
        });
        assert!(store.list().is_empty());
        assert!(sink.0.lock().unwrap().is_empty());
    }
}
