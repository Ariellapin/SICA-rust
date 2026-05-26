use std::sync::Arc;

use tokio::sync::mpsc;

use protocol::{MessageDump, Request, Response, SessionDump};
use sica_core::message::Role;

use crate::be_core::{fib, BeState};
use crate::chat::ChatHub;

pub async fn handle(
    req: Request,
    state: &Arc<BeState>,
    chat: &ChatHub,
    idealist_bus: &idealist::TriggerBus,
    shutdown_tx: &mpsc::Sender<()>,
) -> Response {
    match req {
        Request::GetCounter => Response::CounterValue { value: state.counter.get() },
        Request::IncrementCounter { by } => {
            Response::CounterValue { value: state.counter.add(by) }
        }
        Request::ResetCounter => {
            state.counter.set(0);
            Response::Ok
        }
        Request::ComputeFib { n } => {
            match tokio::task::spawn_blocking(move || fib::compute(n)).await {
                Ok(Ok(value)) => Response::FibResult { n, value },
                Ok(Err(e)) => Response::Error { message: e.to_string() },
                Err(e) => Response::Error { message: format!("join: {e}") },
            }
        }
        Request::EchoText { text } => Response::Echoed { text },
        Request::Shutdown => {
            let _ = shutdown_tx.send(()).await;
            Response::Ok
        }
        Request::NewSession => {
            let id = chat.create_session().await;
            Response::SessionCreated { id }
        }
        Request::ListSessions => {
            let sessions = chat.list_sessions().await;
            Response::SessionList { sessions }
        }
        Request::LoadSession { session_id } => match chat.load_session(session_id).await {
            Some(session) => Response::SessionLoaded {
                session: SessionDump {
                    id: session.id,
                    title: session.title,
                    created_at: session.created_at,
                    messages: session
                        .messages
                        .into_iter()
                        .map(|m| MessageDump {
                            role: role_str(m.role).into(),
                            content: m.content,
                            reasoning: m.reasoning,
                            images: m.images,
                        })
                        .collect(),
                },
            },
            None => Response::Error {
                message: format!("session {session_id} not found"),
            },
        },
        Request::DeleteSession { session_id } => {
            chat.delete_session(session_id).await;
            Response::Ok
        }
        Request::ConnectLlm { base_url, model, api_key } => {
            // Spawn so the dispatcher can keep handling other requests while
            // the HTTP round-trip completes. State changes flow back via
            // `LlmStateChanged` events.
            chat.spawn_connect_llm(base_url, model, api_key);
            Response::Ok
        }
        Request::DisconnectLlm => {
            chat.disconnect_llm().await;
            Response::Ok
        }
        Request::SendUserMessage { session_id, text, images } => {
            chat.send_user_message(session_id, text, images).await;
            Response::Ok
        }
        Request::InterruptTurn { session_id } => {
            chat.interrupt_session(session_id).await;
            Response::Ok
        }
        Request::ReportFrontendError { module, message, traceback } => {
            idealist_bus.publish(idealist::Trigger {
                kind: "fe_panic".into(),
                module,
                message,
                traceback,
            });
            Response::Ok
        }
    }
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::Tool => "tool",
    }
}
