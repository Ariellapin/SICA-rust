//! IPC client: connect to the BE pipe (with backoff), then run reader+writer
//! tasks until either side disconnects or `kill_rx` fires.
//!
//! Behavior changes vs. the original demo:
//! - The reader does *not* log a Log mirror line for every frame. Only the
//!   typed `forward_event` / `IpcFrame` event is dispatched so the chat panel
//!   stays clean.
//! - Heartbeats are forwarded as `UiEvent::Heartbeat` (drives the 5s watchdog
//!   on the IPC dot) instead of being logged.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use interprocess::local_socket::{
    tokio::{prelude::*, Stream as IpcStream},
    GenericNamespaced, ToNsName,
};
use protocol::{Frame, Payload};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::warn;

use crate::supervisor::{forward_event, UiBridge, UiEvent};

pub fn spawn(
    pipe_name: String,
    bridge: Arc<UiBridge>,
    request_rx: mpsc::UnboundedReceiver<Frame>,
    kill_rx: oneshot::Receiver<()>,
) {
    tokio::spawn(async move {
        let stream = match connect_with_backoff(&pipe_name, &bridge).await {
            Some(s) => s,
            None => return,
        };
        bridge.send(UiEvent::IpcConnected);

        let (r, w) = tokio::io::split(stream);
        let read_handle = tokio::spawn(read_loop(r, bridge.clone()));
        let write_handle = tokio::spawn(write_loop(w, request_rx, bridge.clone()));

        let disconnect_reason: Option<String> = tokio::select! {
            _ = kill_rx => None,
            res = read_handle => res.err().map(|e| e.to_string()),
            res = write_handle => res.err().map(|e| e.to_string()),
        };
        bridge.send(UiEvent::IpcDisconnected { error: disconnect_reason });
    });
}

async fn connect_with_backoff(pipe_name: &str, bridge: &Arc<UiBridge>) -> Option<IpcStream> {
    let raw = pipe_name.strip_prefix(r"\\.\pipe\").unwrap_or(pipe_name);
    let delays_ms = [50u64, 100, 250, 500, 1000, 2000, 2000, 2000, 2000, 2000, 2000];
    let mut total = Duration::ZERO;
    let cap = Duration::from_secs(30);
    for &d in delays_ms.iter().cycle() {
        let ns = match raw.to_ns_name::<GenericNamespaced>() {
            Ok(n) => n,
            Err(e) => {
                bridge.send(UiEvent::Log(format!("bad pipe name: {e}")));
                return None;
            }
        };
        match IpcStream::connect(ns).await {
            Ok(s) => return Some(s),
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(d)).await;
                total += Duration::from_millis(d);
                if total > cap {
                    bridge.send(UiEvent::Log("IPC connect timed out".into()));
                    return None;
                }
            }
        }
    }
    None
}

async fn read_loop(r: tokio::io::ReadHalf<IpcStream>, bridge: Arc<UiBridge>) {
    let mut framed = FramedRead::new(r, LengthDelimitedCodec::new());
    while let Some(item) = framed.next().await {
        match item {
            Ok(bytes) => match Frame::decode(&bytes) {
                Ok(frame) => {
                    // Dispatch typed Events directly; non-event payloads still
                    // get a one-line summary in the Communication log panel.
                    match frame.payload.clone() {
                        Payload::Event(ev) => forward_event(&bridge, ev),
                        Payload::ServerHello { protocol_version, pid, version } => {
                            bridge.send(UiEvent::Log(format!(
                                "SRV  hello version={version} pid={pid} proto={protocol_version}"
                            )));
                            bridge.send(UiEvent::ServerHello {
                                protocol_version,
                                pid,
                                version,
                            });
                        }
                        Payload::Response(r) => {
                            bridge.send(UiEvent::Log(format!("RSP#{} {:?}", frame.id, r)));
                        }
                        Payload::Pong => {
                            bridge.send(UiEvent::Log(format!("PONG#{}", frame.id)));
                        }
                        other => bridge.send(UiEvent::Log(format!("???  {other:?}"))),
                    }
                    bridge.send(UiEvent::IpcFrame(frame));
                }
                Err(e) => warn!(error = %e, "FE decode error"),
            },
            Err(e) => {
                warn!(error = %e, "FE read error");
                break;
            }
        }
    }
}

async fn write_loop(
    w: tokio::io::WriteHalf<IpcStream>,
    mut request_rx: mpsc::UnboundedReceiver<Frame>,
    bridge: Arc<UiBridge>,
) {
    let mut framed = FramedWrite::new(w, LengthDelimitedCodec::new());

    let hello = Frame {
        id: 0,
        payload: Payload::ClientHello { protocol_version: protocol::PROTOCOL_VERSION },
    };
    match hello.encode() {
        Ok(bytes) => {
            if let Err(e) = framed.send(bytes.into()).await {
                bridge.send(UiEvent::Log(format!("hello send failed: {e}")));
                let _ = framed.get_mut().shutdown().await;
                return;
            }
        }
        Err(e) => warn!(error = %e, "encode hello"),
    }

    while let Some(frame) = request_rx.recv().await {
        match frame.encode() {
            Ok(bytes) => {
                if let Err(e) = framed.send(bytes.into()).await {
                    bridge.send(UiEvent::Log(format!("write failed: {e}")));
                    break;
                }
            }
            Err(e) => warn!(error = %e, "encode frame"),
        }
    }
    let _ = framed.get_mut().shutdown().await;
}
