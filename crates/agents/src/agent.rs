//! `MainAgent`: drives one chat turn. Owns the streaming loop and the live
//! token-usage emitter that fixes the stale-meter bug from the Python version.

use std::sync::Arc;

use protocol::Event;

/// Anything that can receive `protocol::Event`s. The backend's IPC writer
/// implements this and forwards each event to the FE.
pub trait EventSink: Send + Sync {
    fn emit(&self, ev: Event);
}

#[derive(Clone)]
pub struct MainAgent {
    pub events: Arc<dyn EventSink>,
}

impl MainAgent {
    pub fn new(events: Arc<dyn EventSink>) -> Self {
        Self { events }
    }

    pub fn emit(&self, ev: Event) {
        self.events.emit(ev);
    }
}
