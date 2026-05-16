//! Chat session bookkeeping + LLM connection wiring used by the dispatcher.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use protocol::{Event, Frame, LlmState, SessionMeta};
use tokio::sync::{mpsc, Mutex};
use tracing::warn;

use agents::EventSink;
use llm::client::LlmClient;
use sica_core::session::Session;

#[derive(Clone)]
pub struct ChatHub {
    pub sessions:   Arc<Mutex<HashMap<u64, Session>>>,
    pub next_id:    Arc<AtomicU64>,
    pub next_turn:  Arc<AtomicU64>,
    pub llm:        Arc<Mutex<Option<LlmClient>>>,
    pub llm_state:  Arc<Mutex<LlmState>>,
    pub out_tx:     mpsc::UnboundedSender<Frame>,
    pub event_sink: Arc<dyn EventSink>,
}

impl ChatHub {
    pub fn new(out_tx: mpsc::UnboundedSender<Frame>) -> Self {
        let sink: Arc<dyn EventSink> = Arc::new(OutSink { tx: out_tx.clone() });
        Self {
            sessions:   Arc::new(Mutex::new(HashMap::new())),
            next_id:    Arc::new(AtomicU64::new(1)),
            next_turn:  Arc::new(AtomicU64::new(1)),
            llm:        Arc::new(Mutex::new(None)),
            llm_state:  Arc::new(Mutex::new(LlmState::Disconnected)),
            out_tx,
            event_sink: sink,
        }
    }

    pub async fn list_sessions(&self) -> Vec<SessionMeta> {
        let g = self.sessions.lock().await;
        g.values()
            .map(|s| SessionMeta {
                id: s.id,
                title: s.title.clone(),
                created_at: s.created_at,
            })
            .collect()
    }

    pub async fn create_session(&self) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let s = Session::new(id, format!("Session {id}"));
        self.sessions.lock().await.insert(id, s);
        id
    }

    pub async fn set_llm_state(&self, st: LlmState) {
        *self.llm_state.lock().await = st.clone();
        let _ = self
            .out_tx
            .send(Frame::event(Event::LlmStateChanged { state: st }));
    }

    pub async fn connect_llm(&self, base_url: String, model: String) {
        self.set_llm_state(LlmState::Connecting).await;
        let client = LlmClient::new(base_url, model.clone());
        match client.health().await {
            Ok(()) => {
                *self.llm.lock().await = Some(client);
                self.set_llm_state(LlmState::Ready {
                    model,
                    context_window: 24_000,
                })
                .await;
            }
            Err(e) => {
                self.set_llm_state(LlmState::Error { message: format!("{e}") }).await;
                warn!(error = %e, "LLM connect failed");
            }
        }
    }

    pub async fn disconnect_llm(&self) {
        *self.llm.lock().await = None;
        self.set_llm_state(LlmState::Disconnected).await;
    }

    pub async fn send_user_message(&self, session_id: u64, text: String) {
        let Some(client) = self.llm.lock().await.clone() else {
            self.event_sink.emit(Event::LogLine {
                level: "WARN".into(),
                message: "no LLM connected — cannot send".into(),
            });
            return;
        };

        // Ensure session exists.
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .entry(session_id)
            .or_insert_with(|| Session::new(session_id, format!("Session {session_id}")));
        session.messages.push(sica_core::message::Message::user(text.clone()));
        let history = session
            .messages
            .iter()
            .map(|m| llm::client::ChatMessage {
                role: match m.role {
                    sica_core::message::Role::User => "user".into(),
                    sica_core::message::Role::Assistant => "assistant".into(),
                    sica_core::message::Role::System => "system".into(),
                    sica_core::message::Role::Tool => "tool".into(),
                },
                content: m.content.clone(),
            })
            .collect();
        drop(sessions);

        let turn_id = self.next_turn.fetch_add(1, Ordering::Relaxed);
        let events = self.event_sink.clone();
        tokio::spawn(async move {
            agents::turn::run_turn(
                client,
                events,
                agents::turn::TurnInput {
                    session_id,
                    turn_id,
                    messages: history,
                    limit: 24_000,
                },
            )
            .await;
        });
    }
}

struct OutSink {
    tx: mpsc::UnboundedSender<Frame>,
}

impl EventSink for OutSink {
    fn emit(&self, ev: Event) {
        let _ = self.tx.send(Frame::event(ev));
    }
}

impl idealist::IdealistEventSink for OutSink {
    fn emit(&self, ev: Event) {
        let _ = self.tx.send(Frame::event(ev));
    }
}
