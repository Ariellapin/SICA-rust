use crossbeam_channel::{unbounded, Receiver, Sender};
use serde::{Deserialize, Serialize};

/// Where a trigger came from. Part of the ticket fingerprint, so the same
/// text raised by two different paths (a tool body and the turn loop) stays
/// two tickets — they have two different fixes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerOrigin {
    /// A failed sub-agent tool call (`ToolFailureSink`).
    ToolCall,
    /// A turn that ended with `finish_reason = "error"`.
    TurnError,
    /// `ConnectLlm` failed.
    LlmConnect,
    /// A backend panic caught by the process panic hook.
    Panic,
    /// A `backend::invariants` violation (guide §14.3).
    Invariant,
    /// `Request::ReportFrontendError`.
    Frontend,
    /// A configuration file that could not be loaded (a preset, hooks,
    /// MCP). Ticketed for the record, never investigated automatically.
    Config,
    /// Raised by the investigator's own run. Dropped at the daemon: an
    /// investigation that opened tickets could recurse forever.
    Investigator,
    #[default]
    Other,
}

impl TriggerOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            TriggerOrigin::ToolCall => "tool_call",
            TriggerOrigin::TurnError => "turn_error",
            TriggerOrigin::LlmConnect => "llm_connect",
            TriggerOrigin::Panic => "panic",
            TriggerOrigin::Invariant => "invariant",
            TriggerOrigin::Frontend => "frontend",
            TriggerOrigin::Config => "config",
            TriggerOrigin::Investigator => "investigator",
            TriggerOrigin::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Trigger {
    pub kind:       String,
    pub module:     String,
    pub message:    String,
    pub traceback:  Option<String>,
    pub origin:     TriggerOrigin,
    /// Session the failure happened in. `None` for failures outside any
    /// session (a connect, a panic on a worker with no turn).
    pub session_id: Option<u64>,
    pub turn_id:    Option<u64>,
    /// Session-log seq that marks the failure — the `TicketOpened` row, or
    /// the newest event when the ticket was already open in this session.
    pub seq:        Option<u64>,
}

#[derive(Clone)]
pub struct TriggerBus {
    pub tx: Sender<Trigger>,
    pub rx: Receiver<Trigger>,
}

impl Default for TriggerBus {
    fn default() -> Self {
        Self::new()
    }
}

impl TriggerBus {
    pub fn new() -> Self {
        let (tx, rx) = unbounded();
        Self { tx, rx }
    }

    pub fn publish(&self, t: Trigger) {
        let _ = self.tx.send(t);
    }

    pub fn subscribe(&self) -> Receiver<Trigger> {
        self.rx.clone()
    }
}
