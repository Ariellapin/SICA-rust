use std::sync::Arc;

use tokio::sync::mpsc;

use protocol::{Request, Response};

use crate::be_core::{fib, BeState};

pub async fn handle(req: Request, state: &Arc<BeState>, shutdown_tx: &mpsc::Sender<()>) -> Response {
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
            // Heavy compute -> push to blocking pool so we don't stall the runtime.
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
    }
}
