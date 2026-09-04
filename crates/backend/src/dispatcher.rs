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
        Request::LoadSession { session_id } => match chat.dump_session(session_id).await {
            Some(session) => Response::SessionLoaded { session },
            None => Response::Error {
                message: format!("session {session_id} not found"),
            },
        },
        Request::LoadSessionEvents { session_id, from_seq, limit } => {
            match chat.dump_events(session_id, from_seq, limit).await {
                Some((events, total, next_seq)) => {
                    Response::SessionEvents { session_id, events, total, next_seq }
                }
                None => Response::Error {
                    message: format!("session {session_id} not found"),
                },
            }
        }
        Request::ListCatalog => Response::Catalog {
            entries: crate::catalog::build_from_workspace(&chat.skills),
        },
        Request::DeleteSession { session_id } => {
            chat.delete_session(session_id).await;
            Response::Ok
        }
        Request::RenameSession { session_id, title } => {
            if chat.rename_session(session_id, &title).await {
                Response::Ok
            } else {
                Response::Error {
                    message: format!("session {session_id} could not be renamed"),
                }
            }
        }
        Request::ForkSession { session_id } => match chat.fork_session(session_id).await {
            Some(id) => Response::SessionCreated { id },
            None => Response::Error {
                message: format!("session {session_id} has no completed turn to fork"),
            },
        },
        Request::ArchiveSession { session_id } => {
            chat.archive_session(session_id).await;
            Response::Ok
        }
        Request::SearchSessions { query } => Response::SessionSearch {
            hits: chat.search_sessions(&query).await,
        },
        Request::ConnectLlm { base_url, model, api_key, options } => {
            // Spawn so the dispatcher can keep handling other requests while
            // the HTTP round-trip completes. State changes flow back via
            // `LlmStateChanged` events.
            chat.spawn_connect_llm(base_url, model, api_key, options);
            Response::Ok
        }
        Request::ListModels { base_url, api_key } => {
            chat.spawn_list_models(base_url, api_key);
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
        Request::EditUserMessage { session_id, seq, text } => {
            match chat.edit_user_message(session_id, seq, text).await {
                Ok(()) => Response::Ok,
                Err(message) => Response::Error { message },
            }
        }
        Request::InterruptTurn { session_id } => {
            chat.interrupt_session(session_id).await;
            Response::Ok
        }
        Request::RunCommand { session_id, name, input } => {
            let text = chat.run_command(session_id, &name, &input).await;
            Response::CommandResult { text }
        }
        Request::SetPermissionMode { session_id, mode } => {
            chat.set_permission_mode(session_id, mode).await;
            Response::Ok
        }
        Request::SetPlanMode { session_id, active } => {
            chat.set_plan_mode(session_id, active).await;
            Response::Ok
        }
        Request::ResolveApproval { id, allow } => {
            if chat.resolve_approval(id, allow).await {
                Response::Ok
            } else {
                Response::Error { message: format!("approval {id} is no longer pending") }
            }
        }
        Request::AnswerQuestion { id, answer } => {
            if chat.answer_question(id, answer).await {
                Response::Ok
            } else {
                Response::Error { message: format!("question {id} is no longer pending") }
            }
        }
        Request::SteerTurn { session_id, text } => {
            chat.steer_turn(session_id, text).await;
            Response::Ok
        }
        Request::InjectContext { session_id, text } => {
            chat.inject_context(session_id, text).await;
            Response::Ok
        }
        // The queue verbs report a row that is already gone as an error
        // rather than as success: the loop claims rows on its own schedule,
        // so "it was not there" is a normal outcome the user has to see.
        Request::EditQueued { session_id, id, text } => {
            match chat.edit_queued(session_id, id, text).await {
                Ok(()) => Response::Ok,
                Err(message) => Response::Error { message },
            }
        }
        Request::RemoveQueued { session_id, id } => {
            match chat.remove_queued(session_id, id).await {
                Ok(()) => Response::Ok,
                Err(message) => Response::Error { message },
            }
        }
        Request::SteerQueued { session_id, id } => {
            match chat.steer_queued(session_id, id).await {
                Ok(()) => Response::Ok,
                Err(message) => Response::Error { message },
            }
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

