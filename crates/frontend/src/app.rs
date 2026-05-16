use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use protocol::Request;
use tokio::sync::mpsc::UnboundedSender;

use crate::supervisor::{self, UiCommand, UiEvent};
use crate::ui;

const LOG_CAPACITY: usize = 2000;

pub struct App {
    // Held to keep the runtime alive for the supervisor task; not accessed directly.
    #[allow(dead_code)]
    pub rt: Arc<tokio::runtime::Runtime>,
    pub cmd_tx: UnboundedSender<UiCommand>,
    pub ui_rx: std::sync::mpsc::Receiver<UiEvent>,

    pub log: VecDeque<LogEntry>,
    pub be_state: BeState,
    pub ipc_state: IpcState,
    pub build_state: BuildState,
    pub auto_watch: bool,

    pub request_draft: RequestDraft,
    pub release_profile: bool,
    pub autoscroll: bool,
}

#[derive(Clone)]
pub struct LogEntry {
    #[allow(dead_code)]
    pub ts: Instant,
    pub kind: LogKind,
    pub text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum LogKind {
    Info,
    Build,
    Be,
    Ipc,
    Event,
    Error,
}

#[derive(Default)]
pub struct BeState {
    pub running: bool,
    pub pid: Option<u32>,
    pub last_exit_code: Option<i32>,
}

#[derive(Default)]
pub struct IpcState {
    pub connected: bool,
}

#[derive(Default)]
pub struct BuildState {
    pub in_flight: bool,
    pub last_ok: Option<bool>,
    pub last_duration_ms: Option<u128>,
}

pub struct RequestDraft {
    pub kind: RequestKind,
    pub inc_by: i64,
    pub fib_n: u32,
    pub echo_text: String,
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum RequestKind {
    GetCounter,
    Increment,
    Reset,
    Fib,
    Echo,
}

impl Default for RequestDraft {
    fn default() -> Self {
        Self {
            kind: RequestKind::Increment,
            inc_by: 1,
            fib_n: 30,
            echo_text: String::from("hello"),
        }
    }
}

impl RequestDraft {
    pub fn to_request(&self) -> Request {
        match self.kind {
            RequestKind::GetCounter => Request::GetCounter,
            RequestKind::Increment => Request::IncrementCounter { by: self.inc_by },
            RequestKind::Reset => Request::ResetCounter,
            RequestKind::Fib => Request::ComputeFib { n: self.fib_n },
            RequestKind::Echo => Request::EchoText { text: self.echo_text.clone() },
        }
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime"),
        );

        let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();
        let cmd_tx = supervisor::spawn(&rt, cc.egui_ctx.clone(), ui_tx);

        // Lightly customize style so it looks intentional, not default.
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 4.0);
        style.visuals.window_rounding = 6.0.into();
        style.visuals.widgets.noninteractive.rounding = 4.0.into();
        style.visuals.widgets.inactive.rounding = 4.0.into();
        style.visuals.widgets.hovered.rounding = 4.0.into();
        style.visuals.widgets.active.rounding = 4.0.into();
        cc.egui_ctx.set_style(style);

        Self {
            rt,
            cmd_tx,
            ui_rx,
            log: VecDeque::with_capacity(LOG_CAPACITY),
            be_state: BeState::default(),
            ipc_state: IpcState::default(),
            build_state: BuildState::default(),
            auto_watch: false,
            request_draft: RequestDraft::default(),
            release_profile: false,
            autoscroll: true,
        }
    }

    pub fn push_log(&mut self, kind: LogKind, text: String) {
        if self.log.len() >= LOG_CAPACITY {
            self.log.pop_front();
        }
        self.log.push_back(LogEntry { ts: Instant::now(), kind, text });
    }

    pub fn send(&self, cmd: UiCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.ui_rx.try_recv() {
            match ev {
                UiEvent::Log(s) => self.push_log(LogKind::Info, s),
                UiEvent::BeStarted { pid } => {
                    self.be_state.running = true;
                    self.be_state.pid = Some(pid);
                    self.push_log(LogKind::Be, format!("BE started pid={pid}"));
                }
                UiEvent::BeStopped { code } => {
                    self.be_state.running = false;
                    self.be_state.pid = None;
                    self.be_state.last_exit_code = code;
                    self.push_log(LogKind::Be, format!("BE stopped code={code:?}"));
                }
                UiEvent::BuildStarted => {
                    self.build_state.in_flight = true;
                    self.push_log(LogKind::Build, "build: started".into());
                }
                UiEvent::BuildLine(line) => self.push_log(LogKind::Build, line),
                UiEvent::BuildFinished { ok, duration_ms } => {
                    self.build_state.in_flight = false;
                    self.build_state.last_ok = Some(ok);
                    self.build_state.last_duration_ms = Some(duration_ms);
                    self.push_log(
                        LogKind::Build,
                        format!(
                            "build: {} in {:.2}s",
                            if ok { "ok" } else { "FAILED" },
                            duration_ms as f64 / 1000.0
                        ),
                    );
                }
                UiEvent::IpcConnected => {
                    self.ipc_state.connected = true;
                    self.push_log(LogKind::Ipc, "IPC connected".into());
                }
                UiEvent::IpcDisconnected => {
                    self.ipc_state.connected = false;
                    self.push_log(LogKind::Ipc, "IPC disconnected".into());
                }
                UiEvent::IpcFrame(_frame) => {
                    // Already logged a summary string from the IPC client; nothing else needed.
                }
                UiEvent::FsEvent(paths) => {
                    let count = paths.len();
                    let sample = paths
                        .iter()
                        .take(3)
                        .map(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join(", ");
                    self.push_log(LogKind::Event, format!("fs: {count} change(s): {sample}"));
                }
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        ui::draw(self, ctx);
        if self.build_state.in_flight {
            ctx.request_repaint_after(std::time::Duration::from_millis(80));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.cmd_tx.send(UiCommand::Quit);
        // Give the supervisor a moment to kill BE before the runtime is dropped.
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
}
