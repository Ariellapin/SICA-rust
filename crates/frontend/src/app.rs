use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::{LlmState, Request, Severity};
use tokio::sync::mpsc::UnboundedSender;

use crate::supervisor::{self, UiCommand, UiEvent};
use crate::ui;

const LOG_CAPACITY: usize = 2000;

/// Top-level view in the main window.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AppView {
    Chat,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    General,
    Llm,
    Communication,
}

pub struct App {
    #[allow(dead_code)]
    pub rt: Arc<tokio::runtime::Runtime>,
    pub cmd_tx: UnboundedSender<UiCommand>,
    pub ui_rx: std::sync::mpsc::Receiver<UiEvent>,

    pub log: VecDeque<LogEntry>,
    pub be_state:    BeState,
    pub ipc_state:   IpcState,
    pub llm_state:   LlmUiState,
    pub build_state: BuildState,
    pub auto_watch:  bool,

    pub request_draft: RequestDraft,
    pub release_profile: bool,
    pub autoscroll: bool,

    // View routing.
    pub view: AppView,
    pub settings_tab: SettingsTab,

    // Settings — General tab.
    pub theme_dark:            bool,
    pub log_raw_llm:           bool,
    pub idealist_auto_apply_be: bool,

    // Settings — LLM tab.
    pub llm_base_url: String,
    pub llm_model:    String,

    // Chat state.
    pub chat: ChatState,

    // Live token meter — atomics so backend can update from any thread.
    pub tokens: Arc<TokenMeter>,
}

pub struct TokenMeter {
    pub used:  AtomicU32,
    pub limit: AtomicU32,
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
    pub running:        bool,
    pub pid:            Option<u32>,
    pub last_exit_code: Option<i32>,
    pub last_error:     Option<String>,
}

pub struct IpcState {
    pub connected:      bool,
    pub last_heartbeat: Option<Instant>,
    pub last_error:     Option<String>,
    pub heartbeat_timeout: bool,
}

impl Default for IpcState {
    fn default() -> Self {
        Self {
            connected: false,
            last_heartbeat: None,
            last_error: None,
            heartbeat_timeout: false,
        }
    }
}

pub struct LlmUiState {
    pub state:      LlmState,
    pub last_error: Option<String>,
}

impl Default for LlmUiState {
    fn default() -> Self {
        Self {
            state: LlmState::Disconnected,
            last_error: None,
        }
    }
}

impl LlmUiState {
    pub fn is_ready(&self) -> bool {
        matches!(self.state, LlmState::Ready { .. })
    }
    pub fn label(&self) -> String {
        match &self.state {
            LlmState::Disconnected => "disconnected".into(),
            LlmState::Connecting => "connecting…".into(),
            LlmState::Ready { model, .. } => format!("ready: {model}"),
            LlmState::Error { message } => format!("error: {message}"),
        }
    }
}

#[derive(Default)]
pub struct BuildState {
    pub in_flight:        bool,
    pub last_ok:          Option<bool>,
    pub last_duration_ms: Option<u128>,
}

pub struct RequestDraft {
    pub kind:      RequestKind,
    pub inc_by:    i64,
    pub fib_n:     u32,
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

#[derive(Default)]
pub struct ChatState {
    pub session_id:    u64,
    #[allow(dead_code)]
    pub next_session:  AtomicU64,
    pub turns:         Vec<Turn>,
    pub draft:         String,
    pub idealist:      IdealistUiState,
}

#[derive(Default)]
pub struct IdealistUiState {
    pub activity:    String,
    pub last_ticket: Option<String>,
    pub severity:    Option<Severity>,
}

#[allow(dead_code)]
pub struct Turn {
    pub session_id:    u64,
    pub turn_id:       u64,
    pub user:          String,
    pub assistant:     String,
    pub reasoning:     String,
    pub finished:      bool,
    pub finish_reason: Option<String>,
    pub tool_chips:    Vec<ToolChip>,
}

#[allow(dead_code)]
pub struct ToolChip {
    pub id:        u64,
    pub parent_id: Option<u64>,
    pub depth:     u8,
    pub name:      String,
    pub finished:  bool,
    pub ok:        bool,
    pub summary:   String,
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

        Self::apply_theme(&cc.egui_ctx, true);

        Self {
            rt,
            cmd_tx,
            ui_rx,
            log: VecDeque::with_capacity(LOG_CAPACITY),
            be_state: BeState::default(),
            ipc_state: IpcState::default(),
            llm_state: LlmUiState::default(),
            build_state: BuildState::default(),
            auto_watch: false,
            request_draft: RequestDraft::default(),
            release_profile: false,
            autoscroll: true,
            view: AppView::Chat,
            settings_tab: SettingsTab::General,
            theme_dark: true,
            log_raw_llm: false,
            idealist_auto_apply_be: false,
            llm_base_url: "http://localhost:8080".into(),
            llm_model: "local".into(),
            chat: ChatState {
                session_id: 1,
                next_session: AtomicU64::new(2),
                ..ChatState::default()
            },
            tokens: Arc::new(TokenMeter {
                used:  AtomicU32::new(0),
                limit: AtomicU32::new(24_000),
            }),
        }
    }

    fn apply_theme(ctx: &egui::Context, dark: bool) {
        use sica_core::theme as t;
        let mut style = (*ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 4.0);
        style.visuals.window_rounding = 6.0.into();
        style.visuals.widgets.noninteractive.rounding = 4.0.into();
        style.visuals.widgets.inactive.rounding = 4.0.into();
        style.visuals.widgets.hovered.rounding = 4.0.into();
        style.visuals.widgets.active.rounding = 4.0.into();
        if dark {
            style.visuals = egui::Visuals::dark();
            style.visuals.panel_fill = rgb(t::PAGE_BG);
            style.visuals.window_fill = rgb(t::SIDEBAR_BG);
            style.visuals.extreme_bg_color = rgb(t::STATUS_BAR_BG);
            style.visuals.selection.bg_fill = rgb(t::SIDEBAR_ACTIVE_BG);
            style.visuals.override_text_color = Some(rgb(t::TEXT_PRIMARY));
            style.visuals.hyperlink_color = rgb(t::ACCENT);
        } else {
            style.visuals = egui::Visuals::light();
        }
        ctx.set_style(style);
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

    /// Find the active turn (last unfinished, else last) and return a mutable ref.
    fn active_turn_mut(&mut self) -> Option<&mut Turn> {
        self.chat.turns.last_mut()
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.ui_rx.try_recv() {
            self.handle_event(ev);
        }
    }

    fn handle_event(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::Log(s) => self.push_log(LogKind::Info, s),
            UiEvent::BeStarted { pid } => {
                self.be_state.running = true;
                self.be_state.pid = Some(pid);
                self.be_state.last_error = None;
                self.push_log(LogKind::Be, format!("BE started pid={pid}"));
            }
            UiEvent::BeStopped { code } => {
                self.be_state.running = false;
                self.be_state.pid = None;
                self.be_state.last_exit_code = code;
                if let Some(c) = code {
                    if c != 0 {
                        self.be_state.last_error = Some(format!("exit code {c}"));
                    }
                }
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
                self.ipc_state.last_error = None;
                self.ipc_state.last_heartbeat = Some(Instant::now());
                self.ipc_state.heartbeat_timeout = false;
                self.push_log(LogKind::Ipc, "IPC connected".into());
            }
            UiEvent::IpcDisconnected { error } => {
                self.ipc_state.connected = false;
                self.ipc_state.last_error = error.clone();
                self.push_log(
                    LogKind::Ipc,
                    format!("IPC disconnected{}", error.map(|e| format!(": {e}")).unwrap_or_default()),
                );
            }
            UiEvent::IpcFrame(_) => {
                // Already handled by the typed event forwarders.
            }
            UiEvent::Heartbeat => {
                self.ipc_state.last_heartbeat = Some(Instant::now());
                self.ipc_state.heartbeat_timeout = false;
                // Intentionally not logged — IPC dot color is the only surface.
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
            UiEvent::LlmStateChanged(state) => {
                let err = if let LlmState::Error { message } = &state {
                    Some(message.clone())
                } else {
                    None
                };
                self.llm_state = LlmUiState { state, last_error: err };
            }
            UiEvent::TurnStarted { session_id, turn_id } => {
                self.chat.turns.push(Turn {
                    session_id,
                    turn_id,
                    user: String::new(),
                    assistant: String::new(),
                    reasoning: String::new(),
                    finished: false,
                    finish_reason: None,
                    tool_chips: Vec::new(),
                });
            }
            UiEvent::AssistantDelta { content, reasoning, .. } => {
                if let Some(t) = self.active_turn_mut() {
                    t.assistant.push_str(&content);
                    t.reasoning.push_str(&reasoning);
                }
            }
            UiEvent::TurnFinished { finish_reason, .. } => {
                if let Some(t) = self.active_turn_mut() {
                    t.finished = true;
                    t.finish_reason = Some(finish_reason);
                }
            }
            UiEvent::TokenUsage { used, limit, .. } => {
                self.tokens.used.store(used, Ordering::Relaxed);
                self.tokens.limit.store(limit, Ordering::Relaxed);
            }
            UiEvent::ToolCallStarted { id, parent_id, depth, name } => {
                if let Some(t) = self.active_turn_mut() {
                    t.tool_chips.push(ToolChip {
                        id, parent_id, depth, name,
                        finished: false, ok: true, summary: String::new(),
                    });
                }
            }
            UiEvent::ToolCallFinished { id, ok, summary } => {
                if let Some(t) = self.active_turn_mut() {
                    if let Some(chip) = t.tool_chips.iter_mut().find(|c| c.id == id) {
                        chip.finished = true;
                        chip.ok = ok;
                        chip.summary = summary;
                    }
                }
            }
            UiEvent::IdealistStatus { activity, severity, last_ticket } => {
                self.chat.idealist.activity = activity;
                self.chat.idealist.severity = Some(severity);
                if last_ticket.is_some() {
                    self.chat.idealist.last_ticket = last_ticket;
                }
            }
            UiEvent::IdealistTicketWritten { path, kind } => {
                self.push_log(
                    LogKind::Event,
                    format!("idealist ticket written ({kind:?}): {path}"),
                );
                self.chat.idealist.last_ticket = Some(path);
            }
        }
    }

    /// Re-evaluates the IPC heartbeat watchdog. Called once per frame.
    fn tick_heartbeat_watchdog(&mut self) {
        if !self.ipc_state.connected {
            return;
        }
        let stale = self
            .ipc_state
            .last_heartbeat
            .map(|t| t.elapsed() > Duration::from_secs(5))
            .unwrap_or(false);
        if stale && !self.ipc_state.heartbeat_timeout {
            self.ipc_state.heartbeat_timeout = true;
            self.ipc_state.last_error = Some("heartbeat timeout".into());
        }
    }
}

pub fn rgb(c: sica_core::theme::Rgb) -> egui::Color32 {
    egui::Color32::from_rgb(c.0, c.1, c.2)
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.tick_heartbeat_watchdog();
        ui::draw(self, ctx);
        if self.build_state.in_flight {
            ctx.request_repaint_after(std::time::Duration::from_millis(80));
        }
        if self.ipc_state.connected {
            // Keep refreshing so the watchdog can fire even without other events.
            ctx.request_repaint_after(std::time::Duration::from_millis(1000));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.cmd_tx.send(UiCommand::Quit);
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
}
