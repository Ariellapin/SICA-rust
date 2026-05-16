//! Idealist daemon: classifies BE vs FE failures, writes improvement tickets,
//! and (when explicitly enabled) attempts auto-patches for BE issues.
//!
//! Default policy:
//! - **BE issues** → write `Improvement-BE-<kind>-<iso>.md`. Auto-apply only if
//!   the runtime toggle is on.
//! - **FE issues** → write `Improvement-FE-<iso>.md`. Never auto-patch.

pub mod analyzer;
pub mod be_autofix;
pub mod classifier;
pub mod fe_ticket;
pub mod trigger_bus;

pub use classifier::{classify, TriggerSource};
pub use trigger_bus::{Trigger, TriggerBus};

use std::sync::Arc;

use protocol::Event;
use tokio::sync::Mutex;

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
}

impl Idealist {
    pub fn new(events: Arc<dyn IdealistEventSink>) -> Self {
        Self {
            bus:      TriggerBus::new(),
            settings: Arc::new(Mutex::new(Settings { auto_apply_be: false })),
            events,
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
                    _ => break,
                };

                me.events.emit(Event::IdealistStatus {
                    activity: format!("analyzing: {}", trigger.kind),
                    severity: protocol::Severity::Info,
                    last_ticket: None,
                });

                let src = classify(&trigger);
                let auto_apply = me.settings.lock().await.auto_apply_be;
                let ticket_path = match src {
                    TriggerSource::Frontend => {
                        fe_ticket::write_fe_ticket(&trigger).ok()
                    }
                    // SubAgentTool failures share the BE write path: they
                    // produce an Improvement-BE-*.md ticket with the
                    // analyzer's suggested skill swap surfaced inline.
                    _ => {
                        be_autofix::write_be_ticket(&trigger, auto_apply).ok()
                    }
                };
                if let Some(path) = ticket_path {
                    let kind = match src {
                        TriggerSource::Frontend => protocol::TicketKind::FeBug,
                        _ => protocol::TicketKind::BeFix,
                    };
                    let path_str = path.to_string_lossy().to_string();
                    me.events.emit(Event::IdealistTicketWritten {
                        path: path_str.clone(),
                        kind,
                    });
                    me.events.emit(Event::IdealistStatus {
                        activity: "idle".into(),
                        severity: protocol::Severity::Info,
                        last_ticket: Some(path_str),
                    });
                }
            }
        });
    }
}
