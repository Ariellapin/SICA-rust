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
                    _ => {
                        info!("idealist: trigger bus closed — daemon loop exiting");
                        break;
                    }
                };

                info!(
                    kind = %trigger.kind,
                    module = %trigger.module,
                    "idealist: received trigger"
                );
                me.events.emit(Event::LogLine {
                    level: "INFO".into(),
                    message: format!(
                        "idealist: analyzing trigger kind=`{}` module=`{}`",
                        trigger.kind, trigger.module
                    ),
                });
                me.events.emit(Event::IdealistStatus {
                    activity: format!("analyzing: {}", trigger.kind),
                    severity: protocol::Severity::Info,
                    last_ticket: None,
                });

                let src = classify(&trigger);
                let auto_apply = me.settings.lock().await.auto_apply_be;
                info!(
                    source = ?src,
                    auto_apply,
                    "idealist: classified trigger"
                );
                me.events.emit(Event::LogLine {
                    level: "INFO".into(),
                    message: format!(
                        "idealist: classified as {:?} (auto_apply_be={})",
                        src, auto_apply
                    ),
                });

                let ticket_path = match src {
                    TriggerSource::Frontend => {
                        match fe_ticket::write_fe_ticket(&trigger) {
                            Ok(p) => Some(p),
                            Err(e) => {
                                warn!(error = %e, "idealist: write_fe_ticket failed");
                                me.events.emit(Event::LogLine {
                                    level: "ERROR".into(),
                                    message: format!("idealist: write_fe_ticket failed — {e}"),
                                });
                                None
                            }
                        }
                    }
                    // SubAgentTool failures share the BE write path: they
                    // produce an Improvement-BE-*.md ticket with the
                    // analyzer's suggested skill swap surfaced inline.
                    _ => {
                        match be_autofix::write_be_ticket(&trigger, auto_apply) {
                            Ok(p) => Some(p),
                            Err(e) => {
                                warn!(error = %e, "idealist: write_be_ticket failed");
                                me.events.emit(Event::LogLine {
                                    level: "ERROR".into(),
                                    message: format!("idealist: write_be_ticket failed — {e}"),
                                });
                                None
                            }
                        }
                    }
                };
                if let Some(path) = ticket_path {
                    let kind = match src {
                        TriggerSource::Frontend => protocol::TicketKind::FeBug,
                        _ => protocol::TicketKind::BeFix,
                    };
                    let path_str = path.to_string_lossy().to_string();
                    info!(path = %path_str, ?kind, "idealist: ticket written");
                    me.events.emit(Event::LogLine {
                        level: "INFO".into(),
                        message: format!("idealist: ticket written ({:?}) → {}", kind, path_str),
                    });
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
