use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::{LlmState, Request, SessionDump, SessionMeta, Severity, UserImage};
use sica_core::theme::Palette;
use tokio::sync::mpsc::UnboundedSender;

use crate::settings_store::{self, Settings};
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

    // Settings — LLM tab. One TOML-backed panel per provider.
    pub providers: Vec<crate::llm_providers::ProviderConfig>,
    /// `id` of the provider whose Connect button most recently fired —
    /// determines which panel reflects `llm_state` and which "Disconnect"
    /// button is enabled. `None` means no panel is active.
    pub active_provider_id: Option<String>,

    // Auto-bootstrap flags.
    pub auto_start_be:    bool,
    pub auto_connect_llm: bool,
    /// Set once we've fired the auto-start (so we don't loop).
    #[allow(dead_code)]
    pub did_auto_start_be: bool,
    /// Set once we've fired ConnectLlm for the current IPC connection.
    pub did_auto_connect_llm: bool,

    // Toast feedback for the Apply button.
    pub last_settings_status: Option<(Instant, Result<(), String>)>,

    // Chat state.
    pub chat: ChatState,

    // Live token meter — atomics so backend can update from any thread.
    pub tokens: Arc<TokenMeter>,

    // Active color palette (derived from `theme_dark`).
    pub palette: Palette,

    /// Last path component of the workspace root, surfaced in the status
    /// bar so the user can see at a glance which project the BE is acting
    /// on. Cached at construction time — `paths::workspace_root()` walks
    /// the parent chain and shouldn't be called per frame.
    pub workspace_name: String,

    /// Shared markdown render cache. One instance is reused across every
    /// assistant / reasoning body so egui_commonmark can amortise its
    /// per-document work between frames (streaming deltas re-render the
    /// same buffer many times per second).
    pub md_cache: egui_commonmark::CommonMarkCache,
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
    /// Set when the BE announces a protocol version different from
    /// `protocol::PROTOCOL_VERSION`. Drives a banner that prompts a rebuild.
    pub protocol_mismatch: Option<(u32, u32)>,
    /// Version reported by the running BE in its most recent `ServerHello`.
    /// `None` until the first hello arrives.
    pub running_version: Option<String>,
    /// Version computed from the on-disk source tree. Refreshed on file-watcher
    /// events and after a successful rebuild. When it differs from
    /// `running_version`, the footer surfaces a pulsing "RESTART" button.
    pub source_version: Option<String>,
}

impl BeState {
    /// `true` when the on-disk source no longer matches what the running BE
    /// was built from. Drives the footer restart button.
    pub fn restart_pending(&self) -> bool {
        match (&self.running_version, &self.source_version) {
            (Some(a), Some(b)) => a != b,
            _ => false,
        }
    }
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
    pub sessions:      Vec<SessionMeta>,
    /// Set whenever new turn content arrives. The messages view consumes
    /// it on the next frame to force-scroll to the bottom, complementing
    /// the egui `stick_to_bottom` heuristic with a hard snap on any new
    /// assistant delta or turn boundary.
    pub scroll_to_bottom: bool,
    /// Images the user has attached to the next outgoing message. Drained
    /// into `Request::SendUserMessage` on send; rendered as a thumbnail
    /// strip above the input bar in the meantime.
    pub pending_images: Vec<PendingAttachment>,
}

/// One image the user has attached, ready to send. `texture` is materialised
/// the first frame a thumbnail is rendered and reused after. `filename` is
/// best-effort metadata for the chip label.
#[allow(dead_code)]
pub struct PendingAttachment {
    pub mime:        String,
    pub data_base64: String,
    pub filename:    String,
    /// Decoded byte size — surfaced via tooltip / oversize errors. Stays even
    /// if the chip doesn't currently render it.
    pub size_bytes:  usize,
    pub texture:     Option<egui::TextureHandle>,
}

impl PendingAttachment {
    pub fn to_user_image(&self) -> UserImage {
        UserImage {
            mime: self.mime.clone(),
            data_base64: self.data_base64.clone(),
        }
    }
}

#[derive(Default)]
pub struct IdealistUiState {
    pub activity:    String,
    pub last_ticket: Option<String>,
    pub severity:    Option<Severity>,
}

#[allow(dead_code)]
pub struct Turn {
    pub session_id:         u64,
    pub turn_id:            u64,
    pub user:               String,
    pub assistant:          String,
    pub reasoning:          String,
    pub finished:           bool,
    pub finish_reason:      Option<String>,
    pub tool_chips:         Vec<ToolChip>,
    /// `true` once the reasoning bubble should render as a single-line chip
    /// (brain icon + `>`). Flipped to `true` when `TurnFinished` arrives, and
    /// historical turns from `SessionLoaded` start collapsed.
    pub reasoning_collapsed: bool,
    /// Images attached to the user message that opened this turn (empty for
    /// assistant-only or tool-only turns). Each `Attachment` lazily uploads
    /// its bytes as an egui texture the first time it's rendered.
    pub images:             Vec<Attachment>,
}

/// In-history attachment, owned by a `Turn`. Mirrors `PendingAttachment` but
/// is rendered indefinitely as part of past chat scrollback, so the texture
/// caches per turn rather than being drained.
pub struct Attachment {
    pub mime:        String,
    pub data_base64: String,
    pub texture:     Option<egui::TextureHandle>,
}

impl Attachment {
    pub fn from_user_image(img: &UserImage) -> Self {
        Self {
            mime: img.mime.clone(),
            data_base64: img.data_base64.clone(),
            texture: None,
        }
    }
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct ToolChip {
    pub id:           u64,
    pub parent_id:    Option<u64>,
    pub depth:        u8,
    pub name:         String,
    /// Rendered `skill 'arg1' 'arg2'` form for the chip label.
    pub args_preview: String,
    /// Text after the `>` separator: what the main agent wanted out of the call.
    pub expectation:  String,
    pub finished:     bool,
    pub ok:           bool,
    pub summary:      String,
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

        let settings = settings_store::load();
        let palette = if settings.theme_dark { Palette::iron() } else { Palette::paper() };
        crate::ui::fonts::install(&cc.egui_ctx);
        Self::apply_visuals(&cc.egui_ctx, &palette, settings.theme_dark);

        // Make sure the providers folder has at least the seed files so the
        // LLM tab is non-empty on first launch.
        if let Err(e) = crate::llm_providers::seed_defaults_if_empty() {
            tracing::warn!(error = %e, "seeding default LLM providers failed");
        }
        let providers = crate::llm_providers::load_all();
        let active_provider_id = settings
            .last_active_provider
            .clone()
            .filter(|id| providers.iter().any(|p| &p.id == id));

        let auto_start_be = settings.auto_start_be;
        if auto_start_be {
            let _ = cmd_tx.send(UiCommand::StartBe);
        }

        Self {
            rt,
            cmd_tx,
            ui_rx,
            log: VecDeque::with_capacity(LOG_CAPACITY),
            be_state: BeState::default(),
            ipc_state: IpcState::default(),
            llm_state: LlmUiState::default(),
            build_state: BuildState::default(),
            auto_watch: settings.auto_watch,
            request_draft: RequestDraft::default(),
            release_profile: settings.release_profile,
            autoscroll: settings.autoscroll,
            view: AppView::Chat,
            settings_tab: SettingsTab::General,
            theme_dark: settings.theme_dark,
            log_raw_llm: settings.log_raw_llm,
            idealist_auto_apply_be: settings.idealist_auto_apply_be,
            providers,
            active_provider_id,
            auto_start_be,
            auto_connect_llm: settings.auto_connect_llm,
            did_auto_start_be: auto_start_be,
            did_auto_connect_llm: false,
            last_settings_status: None,
            chat: ChatState {
                session_id: 1,
                next_session: AtomicU64::new(2),
                ..ChatState::default()
            },
            tokens: Arc::new(TokenMeter {
                used:  AtomicU32::new(0),
                limit: AtomicU32::new(24_000),
            }),
            palette,
            workspace_name: sica_core::paths::workspace_root()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "—".into()),
            md_cache: egui_commonmark::CommonMarkCache::default(),
        }
    }

    /// Snapshot the live fields, persist them to disk, and re-apply runtime
    /// state (theme, auto-watch, LLM endpoint). Surfaced to the user as a
    /// small status line in the Settings panel.
    pub fn apply_and_save_settings(&mut self, ctx: &egui::Context) {
        let result = settings_store::save(&self.settings_snapshot()).map_err(|e| e.to_string());
        self.palette = if self.theme_dark { Palette::iron() } else { Palette::paper() };
        Self::apply_visuals(ctx, &self.palette, self.theme_dark);

        self.send(UiCommand::SetAutoWatch(self.auto_watch));

        self.last_settings_status = Some((Instant::now(), result));
    }

    fn settings_snapshot(&self) -> Settings {
        Settings {
            theme_dark:             self.theme_dark,
            log_raw_llm:            self.log_raw_llm,
            idealist_auto_apply_be: self.idealist_auto_apply_be,
            auto_start_be:          self.auto_start_be,
            auto_connect_llm:       self.auto_connect_llm,
            autoscroll:             self.autoscroll,
            release_profile:        self.release_profile,
            auto_watch:             self.auto_watch,
            last_active_provider:   self.active_provider_id.clone(),
        }
    }

    /// Quietly persist the live settings to disk without re-applying visuals
    /// or surfacing a toast. Used after a per-panel Connect / Disconnect so
    /// the next startup auto-reconnects to whichever provider was last in
    /// use, even if the user never opens the Settings → Apply bar.
    pub fn persist_settings(&self) {
        let _ = settings_store::save(&self.settings_snapshot());
    }

    /// Persist a single provider panel's edits to its TOML file and return
    /// the on-screen save status. Used by the per-panel Connect handler so
    /// edits stick even if the user never clicks the global Apply button.
    pub fn save_provider(&self, id: &str) -> Result<(), String> {
        let Some(cfg) = self.providers.iter().find(|p| p.id == id) else {
            return Err(format!("provider {id} not found"));
        };
        crate::llm_providers::save(cfg).map_err(|e| e.to_string())
    }

    /// Switch the active LLM connection to the provider identified by `id`.
    /// Saves its edits, disconnects any prior connection, then issues the
    /// new ConnectLlm with that provider's URL/model/key.
    pub fn connect_provider(&mut self, id: &str) {
        let Some(cfg) = self.providers.iter().find(|p| p.id == id).cloned() else {
            return;
        };
        let _ = self.save_provider(id);
        if self.active_provider_id.as_deref() != Some(id)
            && matches!(self.llm_state.state, protocol::LlmState::Ready { .. } | protocol::LlmState::Connecting)
        {
            self.send(UiCommand::SendRequest(Request::DisconnectLlm));
        }
        self.active_provider_id = Some(cfg.id.clone());
        // Record this provider as the last-active one so the next launch
        // auto-reconnects here. Persisting on every Connect (rather than
        // waiting for the Apply bar) is what makes "auto-connect on start"
        // survive across restarts.
        self.persist_settings();
        let api_key = if cfg.api_key.is_empty() { None } else { Some(cfg.api_key.clone()) };
        self.send(UiCommand::SendRequest(Request::ConnectLlm {
            base_url: cfg.base_url,
            model:    cfg.model,
            api_key,
        }));
    }

    fn apply_visuals(ctx: &egui::Context, palette: &Palette, dark: bool) {
        use egui::{
            epaint::Shadow,
            FontFamily, FontId, Rounding, Stroke, TextStyle,
        };
        use sica_core::theme::tokens::{
            FAMILY_ITALIC, HAIRLINE, RADIUS_0, RADIUS_2, SPACE_2, SPACE_3,
        };

        let mut style = (*ctx.style()).clone();
        style.visuals = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };

        // Typography — IBM Plex Mono for body / button / monospace; Newsreader
        // italic for the two display roles. Custom `Caps` and `Display` styles
        // are addressed by `widgets::caps_label` / `widgets::display_text`.
        let mono     = FontFamily::Monospace;
        let italic   = FontFamily::Name(FAMILY_ITALIC.into());
        style.text_styles = [
            (TextStyle::Heading,                FontId::new(22.0, italic.clone())),
            (TextStyle::Body,                   FontId::new(14.0, mono.clone())),
            (TextStyle::Monospace,              FontId::new(13.0, mono.clone())),
            (TextStyle::Button,                 FontId::new(13.0, mono.clone())),
            (TextStyle::Small,                  FontId::new(12.0, mono.clone())),
            (TextStyle::Name("Caps".into()),    FontId::new(11.0, mono.clone())),
            (TextStyle::Name("Display".into()), FontId::new(28.0, italic.clone())),
        ]
        .into();

        // Spacing — 4px grid, generous vertical air, square button padding.
        style.spacing.item_spacing   = egui::vec2(SPACE_2, SPACE_2);
        style.spacing.button_padding = egui::vec2(SPACE_3, 6.0);
        style.spacing.menu_margin    = egui::Margin::same(SPACE_2);

        // Shape language — precise corners, no shadows.
        let r2: Rounding = RADIUS_2.into();
        style.visuals.window_rounding = RADIUS_0.into();
        style.visuals.menu_rounding   = r2;
        style.visuals.popup_shadow    = Shadow::NONE;
        style.visuals.window_shadow   = Shadow::NONE;

        // Surface palette.
        let page     = rgb(palette.page_bg);
        let surface  = rgb(palette.surface);
        let sunk     = rgb(palette.surface_sunk);
        let ink      = rgb(palette.ink);
        let hairline = rgb(palette.hairline);
        let accent   = rgb(palette.accent);
        let subtle   = rgb(palette.accent_subtle);
        let on_accent = if dark { page } else { rgb(palette.page_bg) };

        style.visuals.panel_fill          = page;
        style.visuals.window_fill         = surface;
        style.visuals.extreme_bg_color    = sunk;
        style.visuals.override_text_color = Some(ink);
        style.visuals.hyperlink_color     = accent;
        style.visuals.faint_bg_color      = sunk;
        style.visuals.window_stroke       = Stroke::new(HAIRLINE, hairline);
        style.visuals.menu_rounding       = r2;

        // Selection — accent wash + hairline stroke. Replaces egui's default
        // saturated blue on focused TextEdits.
        style.visuals.selection.bg_fill = subtle;
        style.visuals.selection.stroke  = Stroke::new(HAIRLINE, accent);

        // Widget states. Buttons are ghost-by-default (hairline border on
        // hover) and flip to a solid accent fill when active / pressed.
        let widgets = &mut style.visuals.widgets;
        widgets.noninteractive.rounding   = r2;
        widgets.noninteractive.bg_stroke  = Stroke::new(HAIRLINE, hairline);
        widgets.noninteractive.fg_stroke  = Stroke::new(HAIRLINE, ink);
        widgets.noninteractive.bg_fill    = page;
        widgets.noninteractive.weak_bg_fill = page;

        widgets.inactive.rounding       = r2;
        widgets.inactive.bg_fill        = egui::Color32::TRANSPARENT;
        widgets.inactive.weak_bg_fill   = egui::Color32::TRANSPARENT;
        widgets.inactive.bg_stroke      = Stroke::new(HAIRLINE, hairline);
        widgets.inactive.fg_stroke      = Stroke::new(HAIRLINE, ink);
        widgets.inactive.expansion      = 0.0;

        widgets.hovered.rounding        = r2;
        widgets.hovered.bg_fill         = subtle;
        widgets.hovered.weak_bg_fill    = subtle;
        widgets.hovered.bg_stroke       = Stroke::new(HAIRLINE, accent);
        widgets.hovered.fg_stroke       = Stroke::new(HAIRLINE, accent);
        widgets.hovered.expansion       = 0.0;

        widgets.active.rounding         = r2;
        widgets.active.bg_fill          = accent;
        widgets.active.weak_bg_fill     = subtle;
        widgets.active.bg_stroke        = Stroke::new(HAIRLINE, accent);
        widgets.active.fg_stroke        = Stroke::new(HAIRLINE, on_accent);
        widgets.active.expansion        = 0.0;

        widgets.open.rounding           = r2;
        widgets.open.bg_fill            = subtle;
        widgets.open.weak_bg_fill       = subtle;
        widgets.open.bg_stroke          = Stroke::new(HAIRLINE, accent);
        widgets.open.fg_stroke          = Stroke::new(HAIRLINE, accent);

        style.visuals.warn_fg_color  = rgb(palette.warn);
        style.visuals.error_fg_color = rgb(palette.danger);

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

    /// Switch the active chat to `id`. Clears the on-screen turn buffer and
    /// asks the BE for the session's history; the response arrives as
    /// `UiEvent::SessionLoaded` and is rebuilt into `Vec<Turn>`.
    pub fn switch_session(&mut self, id: u64) {
        if self.chat.session_id == id {
            return;
        }
        self.chat.session_id = id;
        self.chat.turns.clear();
        self.send(UiCommand::SendRequest(Request::LoadSession { session_id: id }));
    }

    /// Ask the BE to drop `id` and remove it from the local list. If the
    /// deleted session was active, fall back to the first remaining session.
    pub fn delete_session(&mut self, id: u64) {
        if self.chat.sessions.len() <= 1 {
            return;
        }
        self.send(UiCommand::SendRequest(Request::DeleteSession { session_id: id }));
        self.chat.sessions.retain(|s| s.id != id);
        if self.chat.session_id == id {
            if let Some(first) = self.chat.sessions.first() {
                let next = first.id;
                self.switch_session(next);
            }
        }
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
                if self.auto_connect_llm && !self.did_auto_connect_llm {
                    self.did_auto_connect_llm = true;
                    if let Some(id) = self.active_provider_id.clone() {
                        self.connect_provider(&id);
                    }
                }
                // Pull the session list so the sidebar can populate. If the
                // BE has none yet, the SessionList handler will create one.
                self.send(UiCommand::SendRequest(Request::ListSessions));
            }
            UiEvent::IpcDisconnected { error } => {
                self.ipc_state.connected = false;
                self.ipc_state.last_error = error.clone();
                // Reset the LLM auto-connect guard so the next IPC reconnect
                // retries the LLM connection automatically.
                self.did_auto_connect_llm = false;
                self.push_log(
                    LogKind::Ipc,
                    format!("IPC disconnected{}", error.map(|e| format!(": {e}")).unwrap_or_default()),
                );
            }
            UiEvent::IpcFrame(_) => {
                // Already handled by the typed event forwarders.
            }
            UiEvent::ServerHello { protocol_version, version, .. } => {
                let fe_version = protocol::PROTOCOL_VERSION;
                if protocol_version != fe_version {
                    self.be_state.protocol_mismatch = Some((protocol_version, fe_version));
                    self.push_log(
                        LogKind::Error,
                        format!(
                            "PROTOCOL MISMATCH: BE reports v{protocol_version}, FE is v{fe_version} \
                             — rebuild the BE binary (Settings → Communication → Rebuild & Restart)."
                        ),
                    );
                } else {
                    self.be_state.protocol_mismatch = None;
                }
                self.be_state.running_version = Some(version);
                // Recompute the on-disk source version so the restart-pending
                // comparison is fresh for the just-attached BE.
                self.be_state.source_version = Some(sica_core::build_id::source_version());
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
                self.be_state.source_version = Some(sica_core::build_id::source_version());
            }
            UiEvent::LlmStateChanged(state) => {
                let err = if let LlmState::Error { message } = &state {
                    Some(message.clone())
                } else {
                    None
                };
                let line = match &state {
                    LlmState::Disconnected => "LLM: disconnected".to_string(),
                    LlmState::Connecting   => "LLM: connecting…".to_string(),
                    LlmState::Ready { model, .. } => format!("LLM: ready ({model})"),
                    LlmState::Error { message } => format!("LLM: error — {message}"),
                };
                self.push_log(LogKind::Event, line);
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
                    reasoning_collapsed: false,
                    images: Vec::new(),
                });
                self.chat.scroll_to_bottom = true;
            }
            UiEvent::AssistantDelta { content, reasoning, .. } => {
                if let Some(t) = self.active_turn_mut() {
                    t.assistant.push_str(&content);
                    t.reasoning.push_str(&reasoning);
                }
                self.chat.scroll_to_bottom = true;
            }
            UiEvent::TurnFinished { finish_reason, .. } => {
                if let Some(t) = self.active_turn_mut() {
                    t.finished = true;
                    t.finish_reason = Some(finish_reason);
                    t.reasoning_collapsed = true;
                }
                self.chat.scroll_to_bottom = true;
            }
            UiEvent::TokenUsage { used, limit, .. } => {
                self.tokens.used.store(used, Ordering::Relaxed);
                self.tokens.limit.store(limit, Ordering::Relaxed);
            }
            UiEvent::ToolCallStarted { id, parent_id, depth, name, args_preview, expectation } => {
                if let Some(t) = self.active_turn_mut() {
                    t.tool_chips.push(ToolChip {
                        id, parent_id, depth, name,
                        args_preview, expectation,
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
            UiEvent::SessionList { mut sessions } => {
                sessions.sort_by_key(|s| s.created_at);
                if sessions.is_empty() {
                    // First-run case: ask the BE to mint a session so the user
                    // has something to type into.
                    self.send(UiCommand::SendRequest(Request::NewSession));
                    self.chat.sessions.clear();
                } else {
                    let has_active = sessions.iter().any(|s| s.id == self.chat.session_id);
                    if !has_active {
                        let next = sessions[0].id;
                        self.switch_session(next);
                    } else if self.chat.turns.is_empty() {
                        // Initial sync after IPC reconnect: pull history so the
                        // chat panel shows past turns instead of an empty pane.
                        let id = self.chat.session_id;
                        self.send(UiCommand::SendRequest(Request::LoadSession { session_id: id }));
                    }
                    self.chat.sessions = sessions;
                }
            }
            UiEvent::SessionCreated { id } => {
                if !self.chat.sessions.iter().any(|s| s.id == id) {
                    self.chat.sessions.push(SessionMeta {
                        id,
                        title: format!("Session {id}"),
                        created_at: 0,
                    });
                }
                self.switch_session(id);
                // Re-list so the title/timestamp come from the BE rather than
                // the placeholder we just inserted.
                self.send(UiCommand::SendRequest(Request::ListSessions));
            }
            UiEvent::SessionLoaded { session } => {
                // Only apply if it matches the currently active session. The
                // user may have clicked through to another session before the
                // response arrived; in that case we drop this snapshot.
                if session.id != self.chat.session_id {
                    return;
                }
                self.chat.turns = rebuild_turns(&session);
                self.chat.scroll_to_bottom = true;
            }
            UiEvent::SessionTitleChanged { session_id, title } => {
                if let Some(s) = self.chat.sessions.iter_mut().find(|s| s.id == session_id) {
                    s.title = title;
                }
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

/// Rebuild a `Vec<Turn>` from a session's persisted message list. Walks the
/// messages, pairing each user message with the assistant message that
/// follows it (if any). System/tool roles are skipped — they aren't shown
/// in the chat panel today. Tool chips are not reconstructed: `Message`
/// has no tool-call field, so this is a known v1 limitation.
fn rebuild_turns(session: &SessionDump) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    let mut current: Option<Turn> = None;
    for m in &session.messages {
        match m.role.as_str() {
            "user" => {
                if let Some(t) = current.take() {
                    turns.push(t);
                }
                current = Some(Turn {
                    session_id: session.id,
                    turn_id: turns.len() as u64 + 1,
                    user: m.content.clone(),
                    assistant: String::new(),
                    reasoning: String::new(),
                    finished: true,
                    finish_reason: None,
                    tool_chips: Vec::new(),
                    reasoning_collapsed: true,
                    images: m.images.iter().map(Attachment::from_user_image).collect(),
                });
            }
            "assistant" => {
                let slot = current.get_or_insert_with(|| Turn {
                    session_id: session.id,
                    turn_id: turns.len() as u64 + 1,
                    user: String::new(),
                    assistant: String::new(),
                    reasoning: String::new(),
                    finished: true,
                    finish_reason: None,
                    tool_chips: Vec::new(),
                    reasoning_collapsed: true,
                    images: Vec::new(),
                });
                slot.assistant = m.content.clone();
                if let Some(r) = &m.reasoning {
                    slot.reasoning = r.clone();
                }
            }
            _ => {}
        }
    }
    if let Some(t) = current {
        turns.push(t);
    }
    turns
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
