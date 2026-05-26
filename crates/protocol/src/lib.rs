//! Wire protocol shared by backend and frontend.
//!
//! A single duplex stream carries every message. Each `Frame` carries a
//! correlation `id` (0 for unsolicited events) and a tagged `Payload`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROTOCOL_VERSION: u32 = 6;

/// One image attached to a user message. `data_base64` is the raw image bytes
/// base64-encoded (no `data:` URL prefix). `mime` is the MIME type, e.g.
/// `image/png`, `image/jpeg`. Used both on the wire (`SendUserMessage`) and
/// in persisted session storage (via `sica_core::message::Message`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserImage {
    pub mime: String,
    pub data_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub id: u64,
    pub payload: Payload,
}

// Note: externally-tagged enums are used so bincode (v1) can deserialize.
// bincode doesn't support `#[serde(tag, content)]`-style internal tags because
// it doesn't preserve field names on the wire.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Payload {
    ClientHello { protocol_version: u32 },
    ServerHello { protocol_version: u32, pid: u32, version: String },
    Request(Request),
    Response(Response),
    Event(Event),
    Ping,
    Pong,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    // Legacy demo requests (still used in the Communication tab).
    GetCounter,
    IncrementCounter { by: i64 },
    ResetCounter,
    ComputeFib { n: u32 },
    EchoText { text: String },
    Shutdown,

    // Chat / LLM control.
    SendUserMessage { session_id: u64, text: String, images: Vec<UserImage> },
    InterruptTurn   { session_id: u64 },
    NewSession,
    ListSessions,
    LoadSession   { session_id: u64 },
    DeleteSession { session_id: u64 },
    ConnectLlm    { base_url: String, model: String, api_key: Option<String> },
    DisconnectLlm,

    // Frontend telemetry — feeds the idealist's classifier.
    ReportFrontendError { module: String, message: String, traceback: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    CounterValue { value: i64 },
    FibResult    { n: u32, value: u128 },
    Echoed       { text: String },
    Ok,
    Error        { message: String },
    SessionList    { sessions: Vec<SessionMeta> },
    SessionCreated { id: u64 },
    SessionLoaded  { session: SessionDump },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: u64,
    pub title: String,
    pub created_at: i64,
}

/// Wire-format dump of a full session's history. Kept separate from
/// `sica_core::session::Session` so the `protocol` crate stays leaf-level
/// (no dep on `sica-core`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDump {
    pub id: u64,
    pub title: String,
    pub created_at: i64,
    pub messages: Vec<MessageDump>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDump {
    pub role: String,
    pub content: String,
    pub reasoning: Option<String>,
    #[serde(default)]
    pub images: Vec<UserImage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LlmState {
    Disconnected,
    Connecting,
    Ready { model: String, context_window: u32 },
    Error { message: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TicketKind {
    BeFix,
    FeBug,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Heartbeat { uptime_secs: u64, counter: i64 },
    Progress  { request_id: u64, percent: u8 },
    LogLine   { level: String, message: String },

    // LLM connection state.
    LlmStateChanged { state: LlmState },

    // Streaming turn lifecycle.
    TurnStarted   { session_id: u64, turn_id: u64 },
    AssistantDelta {
        session_id: u64,
        turn_id: u64,
        content: String,
        reasoning: String,
    },
    TurnFinished {
        session_id: u64,
        turn_id: u64,
        finish_reason: String,
    },

    /// Emitted after the auto-title agent renames a session. Lets the FE
    /// sidebar update without polling `ListSessions`.
    SessionTitleChanged { session_id: u64, title: String },

    // Live token meter (fixes the stale-meter bug from Python).
    TokenUsage { session_id: u64, used: u32, limit: u32 },

    // Tool-call / sub-agent UI events. Nested calls inherit parent_id.
    //
    // `args_preview` carries the natural-language rendering of the call as
    // emitted by the model (e.g. `read-file 'skills/run-cli.md'`), and
    // `expectation` carries the text after the `>` separator — what the main
    // agent wants the sub-agent to focus its summary on.
    ToolCallStarted {
        id: u64,
        parent_id: Option<u64>,
        depth: u8,
        name: String,
        args_preview: String,
        expectation: String,
    },
    ToolCallFinished {
        id: u64,
        ok: bool,
        summary: String,
    },

    // Idealist daemon signals.
    IdealistStatus {
        activity: String,
        severity: Severity,
        last_ticket: Option<String>,
    },
    IdealistTicketWritten {
        path: String,
        kind: TicketKind,
    },
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("encode: {0}")]
    Encode(#[from] bincode::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl Frame {
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        Ok(bincode::serialize(self)?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        Ok(bincode::deserialize(bytes)?)
    }

    pub fn event(event: Event) -> Self {
        Self { id: 0, payload: Payload::Event(event) }
    }

    pub fn response(id: u64, response: Response) -> Self {
        Self { id, payload: Payload::Response(response) }
    }

    pub fn request(id: u64, request: Request) -> Self {
        Self { id, payload: Payload::Request(request) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_request() {
        let f = Frame::request(7, Request::IncrementCounter { by: 3 });
        let bytes = f.encode().unwrap();
        let back = Frame::decode(&bytes).unwrap();
        assert_eq!(back.id, 7);
        matches!(back.payload, Payload::Request(Request::IncrementCounter { by: 3 }));
    }

    #[test]
    fn roundtrip_event() {
        let f = Frame::event(Event::Heartbeat { uptime_secs: 12, counter: 3 });
        let bytes = f.encode().unwrap();
        let back = Frame::decode(&bytes).unwrap();
        assert_eq!(back.id, 0);
        matches!(back.payload, Payload::Event(Event::Heartbeat { .. }));
    }

    #[test]
    fn roundtrip_token_usage() {
        let f = Frame::event(Event::TokenUsage { session_id: 1, used: 1234, limit: 24000 });
        let bytes = f.encode().unwrap();
        let back = Frame::decode(&bytes).unwrap();
        matches!(back.payload, Payload::Event(Event::TokenUsage { .. }));
    }

    #[test]
    fn roundtrip_tool_call() {
        let f = Frame::event(Event::ToolCallStarted {
            id: 1,
            parent_id: None,
            depth: 0,
            name: "cmd".into(),
            args_preview: "cmd 'echo hi'".into(),
            expectation: "confirm it ran".into(),
        });
        let bytes = f.encode().unwrap();
        let back = Frame::decode(&bytes).unwrap();
        matches!(back.payload, Payload::Event(Event::ToolCallStarted { .. }));
    }

    #[test]
    fn roundtrip_llm_state() {
        let f = Frame::event(Event::LlmStateChanged {
            state: LlmState::Ready { model: "qwen".into(), context_window: 24000 },
        });
        let bytes = f.encode().unwrap();
        let back = Frame::decode(&bytes).unwrap();
        matches!(back.payload, Payload::Event(Event::LlmStateChanged { .. }));
    }
}
