//! LLM connection state machine actor. Owns a single `LlmClient` and broadcasts
//! `LlmState` transitions to listeners. Drives Connect/Disconnect on demand.

use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};
use tracing::{info, warn};

use protocol::LlmState;

use crate::client::LlmClient;

#[derive(Debug, Clone)]
pub enum LlmCommand {
    Connect    { base_url: String, model: String, api_key: Option<String> },
    Disconnect,
}

#[derive(Debug, Clone)]
pub enum LlmEvent {
    State(LlmState),
}

pub struct LlmConnection {
    pub client: Arc<RwLock<Option<LlmClient>>>,
    pub state:  Arc<RwLock<LlmState>>,
}

impl LlmConnection {
    pub fn new() -> Self {
        Self {
            client: Arc::new(RwLock::new(None)),
            state:  Arc::new(RwLock::new(LlmState::Disconnected)),
        }
    }

    /// Spawn the state-machine actor. Send `LlmCommand`s via the returned tx;
    /// `LlmEvent`s arrive on the second channel.
    pub fn spawn(
        self: Arc<Self>,
    ) -> (mpsc::UnboundedSender<LlmCommand>, mpsc::UnboundedReceiver<LlmEvent>) {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<LlmCommand>();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<LlmEvent>();
        let me = self;
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    LlmCommand::Connect { base_url, model, api_key } => {
                        Self::set_state(&me, &ev_tx, LlmState::Connecting).await;
                        let client = LlmClient::new(base_url, model.clone(), api_key);
                        match client.health().await {
                            Ok(()) => {
                                let ctx_window = 24_000; // models.json default
                                *me.client.write().await = Some(client);
                                let st = LlmState::Ready {
                                    model:          model.clone(),
                                    context_window: ctx_window,
                                };
                                Self::set_state(&me, &ev_tx, st).await;
                                info!("LLM connected: {model}");
                            }
                            Err(e) => {
                                let st = LlmState::Error { message: format!("connect: {e}") };
                                Self::set_state(&me, &ev_tx, st).await;
                                warn!(error = %e, "LLM connect failed");
                            }
                        }
                    }
                    LlmCommand::Disconnect => {
                        *me.client.write().await = None;
                        Self::set_state(&me, &ev_tx, LlmState::Disconnected).await;
                    }
                }
            }
        });
        (cmd_tx, ev_rx)
    }

    async fn set_state(
        me: &Arc<Self>,
        tx: &mpsc::UnboundedSender<LlmEvent>,
        st: LlmState,
    ) {
        *me.state.write().await = st.clone();
        let _ = tx.send(LlmEvent::State(st));
    }
}
