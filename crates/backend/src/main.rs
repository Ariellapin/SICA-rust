use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{info, warn};

use protocol::{Event, Frame, Payload, Request};
use sica_core::paths::{memory_file, skills_dir, workspace_root};

mod be_core;
mod chat;
mod dispatcher;
mod ipc;
mod parent_watch;
mod sessions_store;
mod title_gen;

use be_core::BeState;
use chat::ChatHub;

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

    // Skill registry — seed the built-in skills' markdown contracts and
    // the workspace `memory.md` index, then scan the folder so any user-
    // authored `*.md` is loaded alongside them.
    let skills_path = skills_dir();
    let root = workspace_root();
    if let Err(e) = agents::skill_creator::seed_default(&skills_path) {
        warn!(error = %e, dir = %skills_path.display(), "seed skill-creator.md failed");
    }
    if let Err(e) = agents::builtins::seed_defaults(&skills_path) {
        warn!(error = %e, dir = %skills_path.display(), "seed builtin skill docs failed");
    }
    let memory_path = memory_file();
    if let Err(e) = agents::memory::seed_default(&memory_path) {
        warn!(error = %e, path = %memory_path.display(), "seed memory.md failed");
    }
    let mut skill_registry = agents::SkillRegistry::new();
    skill_registry.register(Arc::new(agents::SkillCreator::new(skills_path.clone())));
    skill_registry.register(Arc::new(agents::RunCli));
    skill_registry.register(Arc::new(agents::ReadFile::new(root.clone())));
    skill_registry.register(Arc::new(agents::WriteFile::new(root.clone())));
    let parse_errors = agents::md_skill::register_all(&mut skill_registry, &skills_path);
    let skill_count = skill_registry.by_name.len();
    let skill_registry = Arc::new(skill_registry);
    info!(count = skill_count, dir = %skills_path.display(), "skills loaded");
    let _ = out_tx.send(Frame::event(Event::LogLine {
        level: "INFO".into(),
        message: format!(
            "skills: {skill_count} loaded from {}",
            skills_path.display()
        ),
    }));
    for (path, err) in parse_errors {
        warn!(file = %path.display(), error = %err, "skill parse error");
        let _ = out_tx.send(Frame::event(Event::LogLine {
            level: "WARN".into(),
            message: format!("skill parse error in {}: {err}", path.display()),
        }));
    }

    // Chat hub + idealist daemon (use the dispatcher channel as their sink so
    // every event flows out through the same write task).
    let chat = ChatHub::new_loaded(out_tx.clone(), skill_registry.clone());
    let idealist_sink: Arc<dyn idealist::IdealistEventSink> = Arc::new(OutSink { tx: out_tx.clone() });
    let idealist = Arc::new(idealist::Idealist::new(idealist_sink));
    let idealist_bus = idealist.bus.clone();
    Arc::clone(&idealist).spawn();

    // Initial broadcasts: ServerHello + initial LLM state so the FE can sync.
    let _ = out_tx.send(Frame {
        id: 0,
        payload: Payload::ServerHello {
            protocol_version: protocol::PROTOCOL_VERSION,
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    });
    let _ = out_tx.send(Frame::event(Event::LlmStateChanged {
        state: protocol::LlmState::Disconnected,
    }));

    // Heartbeat task. FE silently consumes these to keep the IPC dot green;
    // it no longer logs them to the user-visible log panel.
    let hb_state = state.clone();
    let hb_tx = out_tx.clone();
    let heartbeat_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.tick().await;
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

    // Dispatcher loop.
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    loop {
        tokio::select! {
            maybe_frame = in_rx.recv() => {
                let Some(frame) = maybe_frame else { break };
                match frame.payload {
                    Payload::Request(req) => {
                        let is_shutdown = matches!(req, Request::Shutdown);
                        let resp = dispatcher::handle(req, &state, &chat, &idealist_bus, &shutdown_tx).await;
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

    drop(out_tx);
    heartbeat_task.abort();
    read_task.abort();
    let _ = tokio::time::timeout(std::time::Duration::from_millis(300), write_task).await;
    info!("backend exiting cleanly");
    Ok(())
}

struct OutSink {
    tx: mpsc::UnboundedSender<Frame>,
}

impl idealist::IdealistEventSink for OutSink {
    fn emit(&self, ev: Event) {
        let _ = self.tx.send(Frame::event(ev));
    }
}
