use std::sync::Arc;

use tokio::sync::mpsc;

use protocol::{Request, Response};

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
        Request::ConnectLlm { base_url, model } => {
            chat.connect_llm(base_url, model).await;
            Response::Ok
        }
        Request::DisconnectLlm => {
            chat.disconnect_llm().await;
            Response::Ok
        }
        Request::SendUserMessage { session_id, text } => {
            chat.send_user_message(session_id, text).await;
            Response::Ok
        }
        Request::InterruptTurn { session_id: _ } => Response::Ok,
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
