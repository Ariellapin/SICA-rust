//! Wire protocol shared by backend and frontend.
//!
//! A single duplex stream carries every message. Each `Frame` carries a
//! correlation `id` (0 for unsolicited events) and a tagged `Payload`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROTOCOL_VERSION: u32 = 1;

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
    GetCounter,
    IncrementCounter { by: i64 },
    ResetCounter,
    ComputeFib { n: u32 },
    EchoText { text: String },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    CounterValue { value: i64 },
    FibResult { n: u32, value: u128 },
    Echoed { text: String },
    Ok,
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Heartbeat { uptime_secs: u64, counter: i64 },
    Progress { request_id: u64, percent: u8 },
    LogLine { level: String, message: String },
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
}
