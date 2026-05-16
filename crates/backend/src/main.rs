use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{info, warn};

use protocol::{Event, Frame, Payload, Request};

mod be_core;
mod dispatcher;
mod ipc;
mod parent_watch;

use be_core::BeState;

#[derive(Debug, Clone)]
struct Args {
    ipc: String,
    parent_pid: Option<u32>,
}

fn parse_args() -> Args {
    let mut ipc = None;
    let mut parent_pid = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--ipc" => ipc = it.next(),
            "--parent-pid" => parent_pid = it.next().and_then(|s| s.parse().ok()),
            "--log-level" => {
                if let Some(lvl) = it.next() {
                    std::env::set_var("RUST_LOG", lvl);
                }
            }
            other => eprintln!("backend: unknown arg {other:?}"),
        }
    }
    Args {
        ipc: ipc.expect("--ipc <pipe-name> is required"),
        parent_pid,
    }
}

fn main() -> Result<()> {
    let args = parse_args();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(run(args))
}

async fn run(args: Args) -> Result<()> {
    info!(pipe = %args.ipc, "backend starting");

    if let Some(ppid) = args.parent_pid {
        parent_watch::spawn(ppid);
    }

    let (read_half, write_half) = ipc::accept(&args.ipc).await?;

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Frame>();
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Frame>();

    // Reader task: socket -> in_tx
    let read_task = tokio::spawn(async move {
        let mut framed = ipc::framed_read(read_half);
        while let Some(item) = framed.next().await {
            match item {
                Ok(bytes) => match Frame::decode(&bytes) {
                    Ok(f) => {
                        if in_tx.send(f).is_err() {
                            break;
                        }
                    }
                    Err(e) => warn!(error = %e, "decode error"),
                },
                Err(e) => {
                    warn!(error = %e, "read error");
                    break;
                }
            }
        }
        info!("reader task ended");
    });

    // Writer task: out_rx -> socket
    let write_task = tokio::spawn(async move {
        let mut framed = ipc::framed_write(write_half);
        while let Some(frame) = out_rx.recv().await {
            match frame.encode() {
                Ok(bytes) => {
                    if let Err(e) = framed.send(bytes.into()).await {
                        warn!(error = %e, "write error");
                        break;
                    }
                }
                Err(e) => warn!(error = %e, "encode error"),
            }
        }
        info!("writer task ended");
    });

    let state = Arc::new(BeState::new());
    let started = Instant::now();

    // Send ServerHello immediately
    let _ = out_tx.send(Frame {
        id: 0,
        payload: Payload::ServerHello {
            protocol_version: protocol::PROTOCOL_VERSION,
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    });

    // Heartbeat task
    let hb_state = state.clone();
    let hb_tx = out_tx.clone();
    let heartbeat_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.tick().await; // skip immediate
        loop {
            tick.tick().await;
            let uptime = started.elapsed().as_secs();
            let counter = hb_state.counter.get();
            if hb_tx
                .send(Frame::event(Event::Heartbeat { uptime_secs: uptime, counter }))
                .is_err()
            {
                break;
            }
        }
    });

    // Dispatcher loop (current task)
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    loop {
        tokio::select! {
            maybe_frame = in_rx.recv() => {
                let Some(frame) = maybe_frame else { break };
                match frame.payload {
                    Payload::Request(req) => {
                        let is_shutdown = matches!(req, Request::Shutdown);
                        let resp = dispatcher::handle(req, &state, &shutdown_tx).await;
                        let _ = out_tx.send(Frame::response(frame.id, resp));
                        if is_shutdown { break; }
                    }
                    Payload::Ping => {
                        let _ = out_tx.send(Frame { id: frame.id, payload: Payload::Pong });
                    }
                    Payload::ClientHello { protocol_version } => {
                        info!(client_version = protocol_version, "client hello");
                    }
                    other => warn!(?other, "unexpected payload from client"),
                }
            }
            _ = shutdown_rx.recv() => {
                info!("shutdown requested");
                break;
            }
        }
    }

    // After the dispatcher breaks, close the writer channel and tear down the
    // I/O tasks. Don't wait on read_task — on Windows named pipes the FE's
    // shutdown doesn't always propagate as a clean EOF, so we abort instead.
    drop(out_tx);
    heartbeat_task.abort();
    read_task.abort();
    let _ = tokio::time::timeout(std::time::Duration::from_millis(300), write_task).await;
    info!("backend exiting cleanly");
    Ok(())
}
