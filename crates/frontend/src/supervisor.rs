//! The supervisor task. Owns the BE child, the IPC client, the file watcher,
//! and the cargo-build subprocess. Receives `UiCommand`s and emits `UiEvent`s.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::{Frame, Request};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::builder;
use crate::child::{self, BeChild};
use crate::ipc_client;
use crate::watcher;

#[derive(Debug, Clone)]
pub enum UiCommand {
    StartBe,
    StopBe,
    Rebuild { release: bool },
    RebuildAndRestart { release: bool },
    SendRequest(Request),
    SetAutoWatch(bool),
    Quit,
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    Log(String),
    BeStarted { pid: u32 },
    BeStopped { code: Option<i32> },
    BuildStarted,
    BuildLine(String),
    BuildFinished { ok: bool, duration_ms: u128 },
    IpcConnected,
    IpcDisconnected,
    IpcFrame(Frame),
    FsEvent(Vec<PathBuf>),
}

pub struct UiBridge {
    tx: std::sync::mpsc::Sender<UiEvent>,
    repaint: egui::Context,
}

impl UiBridge {
    pub fn send(&self, ev: UiEvent) {
        let _ = self.tx.send(ev);
        self.repaint.request_repaint();
    }
}

/// Spawn the supervisor on the runtime. Returns the command sender.
pub fn spawn(
    rt: &tokio::runtime::Runtime,
    egui_ctx: egui::Context,
    ui_tx: std::sync::mpsc::Sender<UiEvent>,
) -> mpsc::UnboundedSender<UiCommand> {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
    let bridge = Arc::new(UiBridge { tx: ui_tx, repaint: egui_ctx });
    rt.spawn(run(cmd_rx, bridge));
    cmd_tx
}

/// Whether the BE child is currently running (and its IPC handle if so).
struct Running {
    child: BeChild,
    ipc_kill: tokio::sync::oneshot::Sender<()>,
    request_tx: mpsc::UnboundedSender<Frame>,
    next_req_id: u64,
    #[allow(dead_code)]
    started_at: Instant,
    #[allow(dead_code)]
    consecutive_crashes: u32,
}

async fn run(mut cmd_rx: mpsc::UnboundedReceiver<UiCommand>, bridge: Arc<UiBridge>) {
    let mut running: Option<Running> = None;
    let mut auto_watch = false;

    // File watcher: lives for the whole supervisor lifetime. The `auto_watch`
    // flag below decides whether to act on its events.
    let (fs_tx, mut fs_rx) = mpsc::unbounded_channel::<Vec<PathBuf>>();
    let _watcher_handle = match watcher::start(fs_tx) {
        Ok(h) => Some(h),
        Err(e) => {
            bridge.send(UiEvent::Log(format!("watcher init failed: {e}")));
            None
        }
    };

    // `tokio::select!` is sequential: only one branch body runs at a time.
    // While a `do_build().await` is in flight, no other branch can fire — events
    // queue in their channels and are delivered on subsequent loop iterations.
    // This naturally coalesces rapid FS events through the debouncer plus the
    // sequencing here.
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    UiCommand::StartBe => {
                        if running.is_some() {
                            bridge.send(UiEvent::Log("BE already running".into()));
                        } else {
                            match start_be(&bridge).await {
                                Ok(r) => running = Some(r),
                                Err(e) => bridge.send(UiEvent::Log(format!("start BE failed: {e}"))),
                            }
                        }
                    }
                    UiCommand::StopBe => {
                        if let Some(r) = running.take() {
                            stop_be(r, &bridge).await;
                        }
                    }
                    UiCommand::Rebuild { release } => {
                        if let Some(r) = running.take() {
                            stop_be(r, &bridge).await;
                        }
                        if !do_build(release, &bridge).await {
                            bridge.send(UiEvent::Log("build failed".into()));
                        }
                    }
                    UiCommand::RebuildAndRestart { release } => {
                        if let Some(r) = running.take() {
                            stop_be(r, &bridge).await;
                        }
                        if do_build(release, &bridge).await {
                            match start_be_with_profile(&bridge, release).await {
                                Ok(r) => running = Some(r),
                                Err(e) => bridge.send(UiEvent::Log(format!("respawn failed: {e}"))),
                            }
                        }
                        // Drain any FS events queued during the build so we don't
                        // immediately rebuild again.
                        while fs_rx.try_recv().is_ok() {}
                    }
                    UiCommand::SendRequest(req) => {
                        if let Some(r) = running.as_mut() {
                            r.next_req_id += 1;
                            let id = r.next_req_id;
                            bridge.send(UiEvent::Log(format!("REQ#{id} {req:?}")));
                            let _ = r.request_tx.send(Frame::request(id, req));
                        } else {
                            bridge.send(UiEvent::Log("no BE connection".into()));
                        }
                    }
                    UiCommand::SetAutoWatch(on) => {
                        auto_watch = on;
                        bridge.send(UiEvent::Log(format!("auto-watch = {on}")));
                    }
                    UiCommand::Quit => {
                        if let Some(r) = running.take() {
                            stop_be(r, &bridge).await;
                        }
                        break;
                    }
                }
            }
            Some(paths) = fs_rx.recv() => {
                bridge.send(UiEvent::FsEvent(paths.clone()));
                if auto_watch {
                    bridge.send(UiEvent::Log(format!("fs change: {} path(s), auto rebuild+restart", paths.len())));
                    if let Some(r) = running.take() {
                        stop_be(r, &bridge).await;
                    }
                    if do_build(false, &bridge).await {
                        match start_be_with_profile(&bridge, false).await {
                            Ok(r) => running = Some(r),
                            Err(e) => bridge.send(UiEvent::Log(format!("respawn failed: {e}"))),
                        }
                    }
                    while fs_rx.try_recv().is_ok() {}
                }
            }
        }
    }

    info!("supervisor exiting");
}

async fn start_be(bridge: &Arc<UiBridge>) -> anyhow::Result<Running> {
    start_be_with_profile(bridge, false).await
}

async fn start_be_with_profile(bridge: &Arc<UiBridge>, release: bool) -> anyhow::Result<Running> {
    let exe = child::backend_exe_path(release)?;
    if !exe.exists() {
        bridge.send(UiEvent::Log(format!(
            "backend binary not found at {} — run a build first",
            exe.display()
        )));
        anyhow::bail!("backend binary missing: {}", exe.display());
    }
    let pipe_name = format!(r"\\.\pipe\sica-rust-{}", std::process::id());

    bridge.send(UiEvent::Log(format!("starting BE: {}", exe.display())));
    let child = child::spawn(&exe, &pipe_name, std::process::id(), bridge.clone()).await?;
    let pid = child.pid;
    bridge.send(UiEvent::BeStarted { pid });

    // Connect IPC with retry/backoff.
    let (request_tx, request_rx) = mpsc::unbounded_channel::<Frame>();
    let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
    ipc_client::spawn(pipe_name.clone(), bridge.clone(), request_rx, kill_rx);

    Ok(Running {
        child,
        ipc_kill: kill_tx,
        request_tx,
        next_req_id: 0,
        started_at: Instant::now(),
        consecutive_crashes: 0,
    })
}

async fn stop_be(running: Running, bridge: &Arc<UiBridge>) {
    let Running { mut child, ipc_kill, request_tx, .. } = running;

    // 1) cooperative shutdown over IPC
    let _ = request_tx.send(Frame::request(u64::MAX, Request::Shutdown));
    // Brief grace period
    let waited = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
    match waited {
        Ok(Ok(status)) => {
            bridge.send(UiEvent::BeStopped { code: status.code() });
            let _ = ipc_kill.send(());
            return;
        }
        Ok(Err(e)) => warn!(error = %e, "child.wait failed"),
        Err(_) => {}
    }

    // 2) forceful kill
    let _ = child.kill().await;
    match child.wait().await {
        Ok(s) => bridge.send(UiEvent::BeStopped { code: s.code() }),
        Err(e) => bridge.send(UiEvent::Log(format!("kill+reap failed: {e}"))),
    }
    let _ = ipc_kill.send(());
}

async fn do_build(release: bool, bridge: &Arc<UiBridge>) -> bool {
    bridge.send(UiEvent::BuildStarted);
    let start = Instant::now();
    let result = builder::run_cargo_build(release, bridge.clone()).await;
    let ok = result.unwrap_or_else(|e| {
        bridge.send(UiEvent::Log(format!("build error: {e}")));
        false
    });
    bridge.send(UiEvent::BuildFinished {
        ok,
        duration_ms: start.elapsed().as_millis(),
    });
    ok
}

