use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::{CatalogEntry, LlmState, Request, SessionDump, SessionMeta, Severity, UserImage};
use sica_core::theme::{tokens, Theme};
use tokio::sync::mpsc::UnboundedSender;

use crate::settings_store::{self, Settings};
use crate::supervisor::{self, UiCommand, UiEvent};
use crate::ui;

const LOG_CAPACITY: usize = 2000;

/// How many previously chosen working directories the picker remembers.
const RECENT_WORKING_DIRS: usize = 5;

/// Settings sections (§7.2). Settings is a modal, not a view — the sidebar
/// has no view switch at all since UI-1.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    General,
    Models,
    Skills,
    Agents,
    Integrations,
    Diagnostics,
}

/// Appearance preference — the three cubes in Settings › General.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemeMode {
    Light,
    Dark,
    System,
}

impl ThemeMode {
    pub fn parse(s: &str) -> Self {
        match s {
            "light" => ThemeMode::Light,
            "system" => ThemeMode::System,
            _ => ThemeMode::Dark,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            ThemeMode::Light => "light",
            ThemeMode::Dark => "dark",
            ThemeMode::System => "system",
        }
    }
    /// Resolve to a concrete theme. `System` follows the OS preference
    /// eframe reported at startup (`IntegrationInfo::system_theme`), which is
    /// the only place the platform tells us; unknown means dark.
    pub fn is_dark(self, system_dark: bool) -> bool {
        match self {
            ThemeMode::Light => false,
            ThemeMode::Dark => true,
            ThemeMode::System => system_dark,
        }
    }
}

/// What plain Enter does while a turn is running (Settings › General).
/// Ctrl+Enter always does the other one — dsh's "accelerated" submit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BusyEnter {
    Queue,
    Steer,
}

impl BusyEnter {
    pub fn parse(s: &str) -> Self {
        if s == "steer" { BusyEnter::Steer } else { BusyEnter::Queue }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            BusyEnter::Queue => "queue",
            BusyEnter::Steer => "steer",
        }
    }
}

/// Shell column state (§2.2). Transient by design: dsh does not persist
/// sidebar width or collapse, and neither do we — every launch opens at 280
/// expanded with the details column closed.
pub struct LayoutState {
    pub sidebar_w: f32,
    pub sidebar_collapsed: bool,
    /// `Some` while the window is under the auto-collapse threshold, holding
    /// the collapse state to restore when it widens again.
    pub narrow_override: Option<bool>,
    pub details_w: f32,
    /// User override of the conversation content width, persisted.
    pub content_w: Option<f32>,
}

impl Default for LayoutState {
    fn default() -> Self {
        Self {
            sidebar_w: tokens::SIDEBAR_DEFAULT,
            sidebar_collapsed: false,
            narrow_override: None,
            details_w: 0.0,
            content_w: None,
        }
    }
}

/// Which view the conversation column shows. dsh registers views and draws
/// the tab strip only when more than one exists; there are exactly two here,
/// so the strip is always drawn on a session that has content.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChatView {
    Chat,
    Trajectory,
}

/// The Trajectory view's own state (§10): one page of the session's raw
/// event log plus the toolbar, selection and paging around it.
///
/// Deliberately *not* derived from `chat.turns`. The transcript shows the
/// derived surface — what the model sees — and this shows the log, including
/// the events the fold shadowed. Building one from the other would lose
/// exactly the rows the view exists for.
#[derive(Default)]
pub struct TrajectoryState {
    /// Session the rows belong to. A page for another session is dropped,
    /// the same rule `SessionLoaded` follows.
    pub session_id: u64,
    pub rows:       Vec<protocol::EventDump>,
    /// Events the log holds, so the footer can say "200 of 1204".
    pub total:      u32,
    /// Seq to ask for next; `None` when the last page reached the end.
    pub next_seq:   Option<u64>,
    /// A page request is in flight — the "Load more" button says so and does
    /// not fire twice.
    pub loading:    bool,
    /// Live search. Non-matching rows dim rather than disappear, so the
    /// numbering and the turn structure stay readable (dsh's rule).
    pub search:     String,
    /// Seq of the row the inspector is showing.
    pub selected:   Option<u64>,
    /// Turn ids folded away by the "Turns" collapse-all.
    pub collapsed:  std::collections::HashSet<u64>,
    /// Timeline segments sized by real duration rather than equally.
    pub actual_duration: bool,
    /// Set when a row should be scrolled into view on the next frame — the
    /// Inspect pill's jump, and the "Load more" landing.
    pub scroll_to:  Option<u64>,
    /// Request envelopes by their own seq, accumulated across pages. A row's
    /// `envelope` field is the key; the bodies arrive once per envelope
    /// rather than once per row, so this map is what the inspector's System
    /// Prompt / Tools / Options / Schema tabs read.
    pub envelopes:  std::collections::HashMap<u64, protocol::EnvelopeDump>,
}

/// The session projections the backend folded for us (guide §3.3).
///
/// Asked for, never pushed: a projection is pure over the log, so there is
/// nothing to subscribe to — the FE re-asks when the log grew (a session
/// load, a finished turn) and shows what came back until then.
#[derive(Default)]
pub struct StatsState {
    /// Session the numbers belong to. An answer for another session is
    /// dropped, the same rule `SessionLoaded` and the ledger follow.
    pub session_id: u64,
    pub stats:      Option<protocol::StatsDump>,
    /// One row per turn, oldest first — the sidebar's jump list.
    pub outline:    Vec<protocol::TurnRowDump>,
    /// Seq the fold covered. The answer is never wrong, only ever behind.
    pub through_seq: u64,
    /// A request is in flight; a second one would answer the same numbers.
    pub loading:    bool,
    /// The jump list is folded away by default — the sidebar's job is
    /// sessions, and the outline is a drill-down into the open one.
    pub expanded:   bool,
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

    /// Sidebar workspaces (§4.3).
    pub workspaces: WorkspacesUi,
    /// The hero's agent-preset picker (§7.2): anchored to its chip.
    pub preset_menu: Option<egui::Rect>,
    /// Image open in the lightbox (§5.3): `(turn, index)`.
    pub lightbox: Option<(usize, usize)>,
    /// First-run key dialog (§7.3): open now, and answered once ever.
    pub onboarding_open: bool,
    pub onboarded: bool,
    /// Preset a *new* session starts with (§7.2). Applied through
    /// `SetSessionAgent` right after `SessionCreated`; `None` means the
    /// persona-less default prompt.
    pub default_agent: Option<String>,

    pub request_draft: RequestDraft,
    pub release_profile: bool,
    pub autoscroll: bool,

    // Settings modal (§7): open flag + active section.
    pub settings_open: bool,
    pub settings_tab: SettingsTab,

    // Settings — General.
    pub theme_mode:            ThemeMode,
    pub theme_dark:            bool,
    pub content_px:            u8,
    pub transcript_compact:    bool,
    pub busy_enter:            BusyEnter,
    pub reduce_motion:         bool,
    pub log_raw_llm:           bool,
    pub idealist_auto_apply_be: bool,

    // Settings — LLM tab. One TOML-backed panel per provider.
    pub providers: Vec<crate::llm_providers::ProviderConfig>,
    /// `id` of the provider whose Connect button most recently fired —
    /// determines which panel reflects `llm_state` and which "Disconnect"
    /// button is enabled. `None` means no panel is active.
    pub active_provider_id: Option<String>,
    /// Models a provider reported, keyed by its base URL — the "Fetch
    /// available models" list (7). An `Err` is the fetch's own message,
    /// shown in place of the list rather than swallowed.
    pub provider_models: std::collections::HashMap<String, Result<Vec<String>, String>>,
    /// Base URLs with a fetch in flight, so the button can say so.
    pub models_pending: std::collections::HashSet<String>,

    // Auto-bootstrap flags.
    pub auto_start_be:    bool,
    pub auto_connect_llm: bool,
    /// Set once we've fired the auto-start (so we don't loop).
    #[allow(dead_code)]
    pub did_auto_start_be: bool,
    /// Set once we've fired ConnectLlm for the current IPC connection.
    pub did_auto_connect_llm: bool,

    // Chat state.
    pub chat: ChatState,

    // Live token meter — atomics so backend can update from any thread.
    pub tokens: Arc<TokenMeter>,

    /// Generation speed derived from successive `TokenUsage.used` deltas.
    /// UI-thread only (updated while draining `UiEvent`s, read while drawing
    /// the footer) — no atomics needed, unlike `tokens`.
    pub gen_speed: GenSpeed,

    // Active design tokens (derived from `theme_mode` + `content_px`).
    pub theme: Theme,

    /// Shell columns — sidebar width / collapse, details width, content axis.
    pub layout: LayoutState,

    /// The one live toast (dsh shows one at a time; a new one replaces it).
    pub toast: Option<crate::ui::kit::Toast>,
    toast_seq: u64,

    /// Set when the IPC link drops, cleared 2 s after it comes back — drives
    /// the connection indicator's "Connected" confirmation.
    pub had_outage: bool,
    pub recovered_at: Option<Instant>,

    /// Log-panel level filter (Diagnostics).
    pub log_filter: LogKind2,

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

    // Wave 3 control-plane UI state.
    /// One-shot approval currently awaiting the human (Allow once / Deny).
    pub pending_approval: Option<PendingApproval>,
    /// `ask-user` / plan-review question currently awaiting an answer.
    pub pending_question: Option<PendingQuestion>,
    /// Durable todo list of the active session (checklist above the
    /// composer). Clears on the next turn start.
    pub todos: Vec<protocol::TodoItem>,
    /// Background jobs of the active session (strip above the composer).
    /// Replaced wholesale on every `JobsChanged`.
    pub jobs: Vec<protocol::JobDump>,
    /// Durable objective of the active session, when it has one.
    pub goal: Option<protocol::GoalDump>,
    /// The goal bar's inline objective editor, and its draft. `Some` only
    /// while the field is open; committing sends `/goal edit <text>`, which
    /// is a compare-and-set on the backend like every other goal mutation.
    pub goal_edit: Option<String>,
    /// Permission mode of the active session (status-bar pill).
    pub permission_mode: protocol::PermissionMode,
    /// Plan mode of the active session (composer toggle).
    pub plan_active: bool,
    /// Agent preset of the active session (`agents/*.md`), when it runs
    /// one — the composer's agent chip.
    pub session_agent: Option<String>,
    /// Session the last composer `/command` targeted — its `CommandResult`
    /// reloads that session, since compaction rewrites history.
    pub last_command_session: Option<u64>,
    /// Session whose prompt edit is awaiting an answer. The transcript is
    /// truncated optimistically when the edit goes out, so a refusal has to
    /// pull the real history back — this says which session to reload.
    pub pending_edit_session: Option<u64>,
    /// Permission mode for freshly minted sessions (Settings JSON).
    pub default_permission_mode: String,
    /// Last prompt composition the BE reported — the context ring's panel is
    /// the first surface to read it (`TokenBreakdown` has been on the wire
    /// since v12 and was never drawn).
    pub token_breakdown: Option<protocol::TokenBreakdown>,
    /// Open state of the composer's toolbar menus.
    pub menu_open: MenuOpen,
    /// Folder the agent works in, when the user pointed it somewhere other
    /// than the app's own root. `None` = `paths::workspace_root()`.
    pub working_dir: Option<PathBuf>,
    /// Working directories the user has picked before, most recent first.
    /// The Settings picker lists them so switching projects is one click.
    pub recent_working_dirs: Vec<PathBuf>,
    /// Tool call shown in the details column, when it is open.
    pub details_call: Option<u64>,
    /// Which view the conversation column is showing (§10).
    pub view: ChatView,
    /// The Trajectory view's ledger and toolbar state.
    pub trajectory: TrajectoryState,
    /// Folded session projections: the header's stats line and the
    /// sidebar's turn outline (§3.3).
    pub stats: StatsState,
    /// The OS light/dark preference eframe reported at startup; what
    /// `ThemeMode::System` resolves to.
    pub system_dark: bool,
}

/// Which composer / header menu is open. Only one at a time — egui has no
/// z-index war because dsh's three overlay disciplines collapse to that rule.
#[derive(Default)]
pub struct MenuOpen {
    pub permission: bool,
    /// Settings › General working-directory picker.
    pub working_dir: bool,
    pub model: bool,
    pub jobs: bool,
    pub goal: bool,
    pub context: bool,
}

/// One pipeline `Ask` waiting on the strip above the composer.
pub struct PendingApproval {
    pub id: u64,
    pub session_id: u64,
    pub skill: String,
    pub args_preview: String,
    pub reason: String,
}

/// One human question waiting on the composer takeover (§6.2).
pub struct PendingQuestion {
    pub id: u64,
    pub session_id: u64,
    pub question: String,
    /// The body under the headline, when the asker sent one.
    pub detail: Option<String>,
    pub options: Vec<String>,
    /// The options are checkboxes rather than one-of. The answer is then the
    /// picked labels joined, so it still crosses back as one string.
    pub multi: bool,
    /// Which options are ticked, in `options` order. Only used when `multi`;
    /// a single-select option answers on the click itself.
    pub picked: Vec<bool>,
    pub draft: String,
    /// The BE frames a plan review as a question whose text opens with the
    /// plan; the takeover then reads as "Plan review" with Approve / Refuse
    /// instead of a generic answer field.
    pub plan_review: bool,
}

pub struct TokenMeter {
    pub used:  AtomicU32,
    /// The model's full context window.
    pub limit: AtomicU32,
    /// Slice of `limit` available to the prompt (window minus the reply
    /// reserve). Denominator of the status-bar percentage, and the number the
    /// backend's auto-compaction triggers on. Zero until the first turn
    /// reports it.
    pub budget: AtomicU32,
}

impl TokenMeter {
    /// Prompt-budget occupancy in percent, clamped to 100. `None` while no
    /// turn has reported a budget yet, so the status bar can render a dash
    /// instead of a misleading 0%.
    pub fn pct(&self) -> Option<u32> {
        let budget = self.budget.load(Ordering::Relaxed);
        if budget == 0 {
            return None;
        }
        let used = self.used.load(Ordering::Relaxed);
        Some(((u64::from(used) * 100 / u64::from(budget)) as u32).min(100))
    }
}

/// Live generation speed (tokens/sec) for the footer.
///
/// Derived entirely in the FE from successive `TokenUsage.used` deltas, so
/// no protocol change is needed: `used` is prompt + generated-so-far and the
/// backend emits it every ~100 ms mid-stream. The first reading of each turn
/// is the prompt baseline (the jump from the previous turn's total is prompt,
/// not generation) and everything after it counts as generated tokens. An
/// exponential moving average over per-window rates gives a live feel; when
/// the turn ends the footer freezes on the turn's overall average.
#[derive(Default)]
pub struct GenSpeed {
    /// `true` between `TurnStarted` and `TurnFinished`.
    pub streaming: bool,
    /// Live EMA while streaming, frozen turn average after.
    pub tps: f32,
    /// Generated tokens so far (this turn, or the last one once finished).
    pub completed: u32,
    /// Wall time since the turn's baseline reading, in seconds.
    pub elapsed_secs: f32,
    turn_start: Option<Instant>,
    start_used: u32,
    last_used: u32,
    last_update: Option<Instant>,
    baseline_set: bool,
}

impl GenSpeed {
    pub fn on_turn_started(&mut self) {
        self.streaming = true;
        self.tps = 0.0;
        self.completed = 0;
        self.elapsed_secs = 0.0;
        self.turn_start = Some(Instant::now());
        self.last_update = None;
        self.baseline_set = false;
    }

    pub fn on_token_usage(&mut self, used: u32) {
        if !self.streaming {
            return;
        }
        let now = Instant::now();
        if !self.baseline_set {
            self.start_used = used;
            self.last_used = used;
            self.turn_start = Some(now);
            self.last_update = Some(now);
            self.baseline_set = true;
            return;
        }
        self.completed = used.saturating_sub(self.start_used);
        if let Some(t0) = self.turn_start {
            self.elapsed_secs = now.duration_since(t0).as_secs_f32();
        }
        if let Some(last) = self.last_update {
            let dt = now.duration_since(last).as_secs_f32();
            if dt >= 0.05 {
                let delta = used.saturating_sub(self.last_used) as f32;
                if delta > 0.0 {
                    let inst = delta / dt;
                    self.tps = if self.tps <= 0.0 {
                        inst
                    } else {
                        0.35 * inst + 0.65 * self.tps
                    };
                }
                self.last_used = used;
                self.last_update = Some(now);
            }
        } else {
            self.last_used = used;
            self.last_update = Some(now);
        }
    }

    pub fn on_turn_finished(&mut self) {
        self.streaming = false;
        if self.baseline_set && self.elapsed_secs > 0.0 && self.completed > 0 {
            self.tps = self.completed as f32 / self.elapsed_secs;
        }
    }

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
    Warn,
    Error,
    Debug,
}

impl LogKind {
    pub fn tag(self) -> &'static str {
        match self {
            LogKind::Info => "INF",
            LogKind::Build => "BLD",
            LogKind::Be => "BE ",
            LogKind::Ipc => "IPC",
            LogKind::Event => "EVT",
            LogKind::Warn => "WRN",
            LogKind::Error => "ERR",
            LogKind::Debug => "DBG",
        }
    }
    /// Severity rank for the Diagnostics filter: 0 debug … 3 error.
    pub fn rank(self) -> u8 {
        match self {
            LogKind::Debug => 0,
            LogKind::Warn => 2,
            LogKind::Error => 3,
            _ => 1,
        }
    }
    /// Map a backend `LogLine.level` (tracing's level names) onto a kind.
    /// The level used to be dropped on the wire-to-UI hop, which made every
    /// BE line read as INF — a rejected tool call included.
    pub fn from_level(level: &str) -> Self {
        match level.to_ascii_uppercase().as_str() {
            "ERROR" => LogKind::Error,
            "WARN" => LogKind::Warn,
            "DEBUG" | "TRACE" => LogKind::Debug,
            _ => LogKind::Info,
        }
    }
}

/// Minimum level shown in the log panel.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LogKind2 {
    All,
    InfoUp,
    WarnUp,
    ErrorOnly,
}

impl LogKind2 {
    pub fn min_rank(self) -> u8 {
        match self {
            LogKind2::All => 0,
            LogKind2::InfoUp => 1,
            LogKind2::WarnUp => 2,
            LogKind2::ErrorOnly => 3,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            LogKind2::All => "All",
            LogKind2::InfoUp => "Info",
            LogKind2::WarnUp => "Warn",
            LogKind2::ErrorOnly => "Error",
        }
    }
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

/// Sidebar workspace state (§4.3): the backend's projection plus the
/// fold/menu bookkeeping that is nobody's business but this app's.
///
/// Membership, order and titles all belong to the backend — it owns the
/// registry — so nothing here is ever edited locally and re-sent. What lives
/// here is what the user is *looking at*: which groups are open, which menu
/// is up, which row is armed for delete.
#[derive(Default)]
pub struct WorkspacesUi {
    pub rows:      Vec<protocol::WorkspaceDump>,
    pub ungrouped: Vec<u64>,
    /// Group by workspace, or one flat list. Persisted (`sidebar_group`).
    pub grouped:   bool,
    /// `true` orders the sessions inside a group by their newest event
    /// instead of the backend's manual order (`sidebar_order`).
    pub by_updated: bool,
    /// Groups the user has folded shut. Absent = open, so a workspace that
    /// appears while the app runs opens rather than hides.
    pub collapsed: std::collections::HashSet<u64>,
    /// Groups showing every session instead of the first five.
    pub show_all:  std::collections::HashSet<u64>,
    pub menu:      Option<(u64, egui::Rect)>,
    pub view_menu: Option<egui::Rect>,
    /// The hero chip's picker (§4.3): anchored to the chip, opened by it.
    pub hero_menu: Option<egui::Rect>,
    pub renaming:  Option<u64>,
    pub rename_draft: String,
    /// Armed for delete — the same two-step the session rows use.
    pub pending_delete: Option<u64>,
    /// Folder handed to `CreateWorkspace`, held until the answer arrives so
    /// a refusal can name the folder the user actually picked.
    pub creating:  Option<String>,
    /// A refused `CreateWorkspace`, shown as "Couldn't open folder".
    pub error:     Option<String>,
}

impl App {
    /// Name and folder of the workspace the **active session** belongs to.
    ///
    /// A session that belongs to none — every session written before §3.9,
    /// and any whose directory is not registered — answers with the app-wide
    /// folder, which is where it actually runs. This is what the header
    /// crumb and the hero chip name: the session's own place, not the
    /// process default they used to show.
    pub fn session_workspace(&self) -> (String, std::path::PathBuf) {
        let id = self.chat.session_id;
        if let Some(w) = self
            .workspaces
            .of_session(id)
            .and_then(|wid| self.workspaces.rows.iter().find(|w| w.id == wid))
        {
            return (w.title.clone(), w.path.clone());
        }
        (self.workspace_name.clone(), sica_core::paths::working_dir())
    }
}

impl WorkspacesUi {
    /// The workspace a session belongs to, if any. Read from the projection
    /// rather than from the session, because the projection is what already
    /// applied the header rule.
    pub fn of_session(&self, session_id: u64) -> Option<u64> {
        self.rows
            .iter()
            .find(|w| w.sessions.contains(&session_id))
            .map(|w| w.id)
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
    /// Session id awaiting delete confirmation. The first `×` click arms a
    /// row (one row at a time); the inline "Delete? / Keep" affordance then
    /// commits or cancels. Keeps an accidental click from dropping a session.
    pub pending_delete: Option<u64>,
    /// `true` once the user has opted out of follow-the-stream autoscroll
    /// (by clicking into the transcript or scrolling up). While paused, new
    /// content no longer snaps the viewport to the bottom; a floating
    /// "resume" pill in the messages view flips this back to `false`.
    pub autoscroll_paused: bool,
    /// Turn index whose assistant output is "selected" via Ctrl+A. Rendered
    /// as a wash over that message; Ctrl+C copies its markdown source.
    /// Cleared on click / Esc / session switch.
    pub selected_turn: Option<usize>,
    /// Anchor of middle-click scroll mode (Windows-style pan): set by a
    /// middle click over the transcript, cleared by any other click or Esc.
    /// While set, vertical pointer displacement from the anchor scrolls.
    pub middle_scroll_origin: Option<egui::Pos2>,
    /// `true` between sending `InterruptTurn` and the BE confirming the turn
    /// is over. Interrupting mid-tool-call is not instant — the skill and any
    /// summarizer round-trip still have to unwind — so the UI reports
    /// "Stopping…" rather than pretending the work already ended.
    pub interrupt_requested: bool,
    /// `true` while the backend is summarising older history to free context.
    /// Surfaced as a "COMPRESSING" marker in the status bar; the finished
    /// result lands in the transcript as a [`Notice`].
    pub compacting: bool,
    /// The "/" palette that opens when the draft starts with a slash.
    pub slash: SlashState,
    /// The "@" file picker, which opens on an `@` token anywhere in the draft.
    pub at: AtState,
    /// Sessions with a turn in flight. `TurnStarted`/`TurnFinished` carry a
    /// session id, so a background session's dot is live too (§4.1).
    pub running_sessions: std::collections::HashSet<u64>,
    /// Sessions blocked on a human (approval / question / plan review).
    pub waiting_sessions: std::collections::HashSet<u64>,
    /// Sessions that completed a turn while not on screen; cleared on open.
    pub unseen_sessions: std::collections::HashSet<u64>,
    /// Queued-but-not-started messages, in the order they will run — the
    /// composer's queue dock. Authoritative once `QueueChanged` lands; until
    /// then it holds this frontend's own local echoes.
    pub queued: Vec<QueuedRow>,
    /// Queue row open for inline editing, and the draft. Keyed by row id
    /// rather than by position: the loop claims rows while the field is
    /// open, and an index would then name the wrong message.
    pub queue_edit: Option<u64>,
    pub queue_edit_draft: String,
    /// Session id whose row menu is open, and the row's screen rect.
    pub row_menu: Option<(u64, egui::Rect)>,
    /// Sidebar search (§4.2): the header icon expands into a field. Title
    /// matches filter the list immediately; the backend's content search is
    /// debounced behind them and its hits are merged in.
    pub search_open: bool,
    pub search_query: String,
    pub search_hits: Vec<protocol::SessionHit>,
    /// The query the last `SearchSessions` went out for, and when the field
    /// last changed — dsh debounces the host search by 250 ms.
    pub search_sent: String,
    pub search_changed_at: Option<std::time::Instant>,
    /// Session being renamed inline, and the draft. One row at a time, the
    /// same discipline as the armed delete.
    pub renaming: Option<u64>,
    pub rename_draft: String,
    /// Turn whose prompt is open for editing, and the draft text. One at a
    /// time, like the inline rename. Cleared on send, on Escape, on a turn
    /// starting and on a session switch — the transcript underneath it can
    /// change, and an editor anchored to a stale index would rewrite the
    /// wrong message.
    pub editing_turn: Option<usize>,
    pub edit_draft: String,
    /// Screen rect the composer card occupied last frame. The `/` and `@`
    /// menus float 4 px above it (§6.3), and they are drawn *before* it —
    /// they have to claim the navigation keys before the text field sees
    /// them — so last frame's rect is the anchor they get. `None` only
    /// before the first card has ever laid out.
    pub composer_rect: Option<egui::Rect>,
}

/// One message waiting in the backend's inbox, as the queue dock draws it.
///
/// `id` is the handle `EditQueued` / `RemoveQueued` / `SteerQueued` address.
/// `None` marks a **local echo**: the send has gone out but no
/// `QueueChanged` has come back yet, so there is nothing to address and the
/// row draws inert — dsh shows those rows too rather than making the queue
/// flicker in a frame late.
#[derive(Clone)]
pub struct QueuedRow {
    pub id:     Option<u64>,
    pub text:   String,
    pub images: u32,
}

/// State of the "/" palette in the composer. The catalogue is pulled once per
/// IPC connection (`Request::ListCatalog`); everything else here is per-keystroke
/// picker state.
#[derive(Default)]
pub struct SlashState {
    /// Skills, markdown agents and markdown commands the BE reported. The
    /// frontend's own app commands (`/new`, `/stop`, …) are appended by the
    /// palette at draw time and never travel over the wire.
    pub entries: Vec<CatalogEntry>,
    /// Index of the highlighted row within the *filtered* list.
    pub selected: usize,
    /// Query the highlight belongs to. When the query changes the highlight
    /// snaps back to the first row instead of pointing at an unrelated entry.
    pub last_query: String,
    /// Set by Esc: hides the list without discarding what was typed. Cleared
    /// as soon as the query changes, so typing brings the list back.
    pub dismissed: bool,
}

/// State of the "@" file picker (§6.3). Unlike the "/" palette, whose
/// catalogue arrives over the wire, the file list is walked by the frontend
/// itself: it is the *frontend's* workspace that `@` names, the walk honours
/// `.gitignore` through the `ignore` crate, and a keystroke must never wait
/// on the dispatcher loop.
#[derive(Default)]
pub struct AtState {
    /// Workspace paths, relative and `/`-separated, directories included and
    /// marked with a trailing slash. Empty until the scan lands.
    pub entries: Vec<FileEntry>,
    /// The scan running on its own thread. Taken as soon as it delivers.
    pub scan: Option<std::sync::mpsc::Receiver<Vec<FileEntry>>>,
    /// When the last scan landed. A workspace changes under the app, so the
    /// index is re-walked when the picker opens on one older than
    /// [`crate::ui::chat::at_menu::INDEX_TTL`]; the stale list keeps serving
    /// until the new one arrives.
    pub scanned_at: Option<std::time::Instant>,
    /// Index of the highlighted row within the *filtered* list.
    pub selected: usize,
    /// Query the highlight belongs to (see [`SlashState::last_query`]).
    pub last_query: String,
    /// Set by Esc; cleared as soon as the query changes.
    pub dismissed: bool,
}

/// One entry of the `@` index.
pub struct FileEntry {
    /// Relative, `/`-separated; a directory ends in `/`.
    pub path:   String,
    pub is_dir: bool,
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
    /// When `Some`, this entry is not a message at all but an out-of-band
    /// transcript marker (today: the auto-compaction record). It renders as a
    /// centred caps line and every other field stays empty.
    pub notice:             Option<Notice>,
    /// The backend queued this message behind a turn that was already
    /// running instead of starting it (Wave 4 inbox). It renders as
    /// "queued" rather than as a stalled stream, and clears when the
    /// backend reports it running.
    pub queued:             bool,
    /// Step-level LLM retries the backend performed inside this turn, in
    /// arrival order. Each is a row on the turn (3.5); an empty vec is the
    /// normal case and draws nothing.
    pub retries:            Vec<RetryRow>,
    /// This turn's own accounting, once `TurnUsage` lands. Drives the tail's
    /// usage and time pills.
    pub usage:              Option<TurnUsage>,
    /// Seq of the log event holding this turn's user message — the handle
    /// `Request::EditUserMessage` addresses. Filled from the session dump on
    /// reload and from `UserMessageStored` on a live send; `None` on a turn
    /// with no prompt of its own (an assistant-only hop) and on a send the
    /// backend has not acknowledged yet, which is exactly when editing must
    /// not be offered.
    pub user_seq:           Option<u64>,
}

/// One step-level retry inside a turn: the backend classified an LLM failure
/// as retryable, slept out the backoff, and rebuilt the identical request.
#[derive(Clone)]
pub struct RetryRow {
    pub attempt:  u32,
    pub max:      u32,
    pub delay_ms: u64,
    pub reason:   String,
    /// When the row arrived — the anchor for the live countdown while the
    /// backoff is still running.
    pub at:       std::time::Instant,
}

/// A completed turn's own token and time accounting (`Event::TurnUsage`).
#[derive(Clone, Copy)]
pub struct TurnUsage {
    pub prompt:      u32,
    pub completion:  u32,
    /// Characters of reasoning - providers do not break the tokens out.
    pub reasoning:   u32,
    pub duration_ms: u64,
    pub ttft_ms:     u64,
}

impl Turn {
    /// An empty turn. Every construction site starts here and overrides what
    /// it knows, so a new field is one edit rather than six.
    pub fn new(session_id: u64, turn_id: u64) -> Self {
        Self {
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
            notice: None,
            queued: false,
            retries: Vec::new(),
            usage: None,
            user_seq: None,
        }
    }

    /// A marker entry in the transcript. `turn_id` is 0 — notices are not
    /// turns the backend knows about, and nothing correlates against them.
    pub fn marker(session_id: u64, notice: Notice) -> Self {
        Self {
            finished: true,
            reasoning_collapsed: true,
            notice: Some(notice),
            ..Self::new(session_id, 0)
        }
    }
}

/// What kind of out-of-band row a [`Notice`] renders as (§3.5). Each maps to
/// a disclosure row with its own icon and title; `detail` is the body.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    /// "Context compacted" — the summary that shadowed the folded span.
    Compaction,
    /// "Context injection" — a `/name` load, a tool notice, a job notice.
    Injection,
    /// A steer the user aimed at the running turn.
    Steer,
}

/// Out-of-band transcript marker. `detail` is the expanded body — for a
/// compaction that is the summary the model wrote, so the user can read
/// exactly what replaced their history.
#[derive(Clone)]
pub struct Notice {
    pub kind:   NoticeKind,
    pub label:  String,
    pub detail: String,
    /// `false` tints the marker with the danger colour (the operation failed).
    pub ok:     bool,
    /// Body open/closed. Rows start collapsed, like dsh's.
    pub open:   bool,
}

impl Notice {
    pub fn new(kind: NoticeKind, label: impl Into<String>, detail: impl Into<String>, ok: bool) -> Self {
        Self { kind, label: label.into(), detail: detail.into(), ok, open: false }
    }
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
    /// Seq of the durable `ToolCall` this row is, when it has one — the
    /// handle the Inspect pill hands the Trajectory view. `0` for a nested
    /// call, which is a live event only and never reaches the log, and for a
    /// pre-v13 reloaded row that carried no `tool_call_id`.
    pub log_seq:      u64,
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
    /// The tool's own output as the model received it, before the expectation
    /// summariser paraphrased it — what the expanded body renders. Equal to
    /// `summary` when no summariser ran.
    pub output:       String,
    /// Resolved arguments as JSON text: the command for a terminal block, the
    /// `old`/`new` pair for a diff, the path for a read.
    pub args_json:    String,
    /// Wall-clock time of the dispatch, pipeline included.
    pub duration_ms:  u64,
    /// Progress lines an orchestrating skill printed while it ran — the
    /// `phase()` and `log()` calls of a `workflow` script, and `agent-team`'s
    /// equivalents (UI guide §6.11). They are the operator's view of a run
    /// that otherwise shows only its final output, and they live on the chip
    /// rather than in the log panel so a long run stays legible where it
    /// happened. Live only: a reload rebuilds the row without them, which is
    /// what the durable `WorkflowRun` events are for.
    pub notes:        Vec<String>,
    /// Body open/closed (§3.4). Rows start collapsed.
    pub expanded:     bool,
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
        let theme_mode = ThemeMode::parse(&settings.theme_mode);
        let content_px = settings
            .content_px
            .clamp(tokens::CONTENT_MIN_PX, tokens::CONTENT_MAX_PX);
        let system_dark = !matches!(
            cc.integration_info.system_theme,
            Some(eframe::Theme::Light)
        );
        let dark = theme_mode.is_dark(system_dark);
        let theme = Theme { content_px, ..Theme::of(dark) };
        crate::ui::fonts::install(&cc.egui_ctx);
        Self::apply_visuals(&cc.egui_ctx, &theme);

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

        // The working directory has to be live *before* the BE child is
        // spawned: `child::spawn` passes it down in the environment, and the
        // BE resolves its file skills against it at startup.
        let working_dir = settings
            .working_dir
            .as_deref()
            .map(PathBuf::from)
            .filter(|p| p.is_dir());
        let recent_working_dirs: Vec<PathBuf> =
            settings.recent_working_dirs.iter().map(PathBuf::from).collect();
        sica_core::paths::set_working_dir(working_dir.as_deref());

        let auto_start_be = settings.auto_start_be;
        if auto_start_be {
            let _ = cmd_tx.send(UiCommand::StartBe);
        }

        Self {
            rt,
            cmd_tx,
            ui_rx,
            provider_models: std::collections::HashMap::new(),
            models_pending: std::collections::HashSet::new(),
            log: VecDeque::with_capacity(LOG_CAPACITY),
            be_state: BeState::default(),
            ipc_state: IpcState::default(),
            llm_state: LlmUiState::default(),
            build_state: BuildState::default(),
            auto_watch: settings.auto_watch,
            workspaces: WorkspacesUi {
                grouped: settings.sidebar_group != "flat",
                by_updated: settings.sidebar_order != "manual",
                ..Default::default()
            },

            preset_menu: None,
            lightbox: None,
            onboarding_open: crate::ui::onboarding_wanted(
                &providers,
                settings.last_active_provider.as_deref(),
                settings.onboarded,
            ),
            onboarded: settings.onboarded,
            default_agent: settings.default_agent.clone(),

            request_draft: RequestDraft::default(),
            release_profile: settings.release_profile,
            autoscroll: settings.autoscroll,
            settings_open: false,
            settings_tab: SettingsTab::General,
            theme_mode,
            theme_dark: dark,
            content_px,
            transcript_compact: settings.transcript_compact,
            busy_enter: BusyEnter::parse(&settings.busy_enter),
            reduce_motion: settings.reduce_motion,
            log_raw_llm: settings.log_raw_llm,
            idealist_auto_apply_be: settings.idealist_auto_apply_be,
            providers,
            active_provider_id,
            auto_start_be,
            auto_connect_llm: settings.auto_connect_llm,
            did_auto_start_be: auto_start_be,
            did_auto_connect_llm: false,
            chat: ChatState {
                session_id: 1,
                next_session: AtomicU64::new(2),
                ..ChatState::default()
            },
            tokens: Arc::new(TokenMeter {
                used:   AtomicU32::new(0),
                limit:  AtomicU32::new(24_000),
                budget: AtomicU32::new(0),
            }),
            gen_speed: GenSpeed::default(),
            theme,
            layout: LayoutState {
                content_w: settings.chat_content_width,
                ..LayoutState::default()
            },
            toast: None,
            toast_seq: 0,
            had_outage: false,
            recovered_at: None,
            log_filter: LogKind2::All,
            workspace_name: sica_core::paths::working_dir()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "—".into()),
            md_cache: egui_commonmark::CommonMarkCache::default(),
            pending_approval: None,
            pending_question: None,
            todos: Vec::new(),
            jobs: Vec::new(),
            goal: None,
            goal_edit: None,
            permission_mode: protocol::PermissionMode::default(),
            plan_active: false,
            session_agent: None,
            last_command_session: None,
            pending_edit_session: None,
            default_permission_mode: settings.default_permission_mode.clone(),
            token_breakdown: None,
            menu_open: MenuOpen::default(),
            working_dir,
            recent_working_dirs,
            details_call: None,
            view: ChatView::Chat,
            trajectory: TrajectoryState::default(),
            stats: StatsState::default(),
            system_dark,
        }
    }

    /// Rebuild the token set from the live preferences and push it into the
    /// egui style. General settings apply live (dsh has no Apply button
    /// there), so this runs on every change rather than on a bar click.
    pub fn refresh_theme(&mut self, ctx: &egui::Context) {
        self.theme_dark = self.theme_mode.is_dark(self.system_dark);
        self.content_px = self
            .content_px
            .clamp(tokens::CONTENT_MIN_PX, tokens::CONTENT_MAX_PX);
        self.theme = Theme {
            content_px: self.content_px,
            ..Theme::of(self.theme_dark)
        };
        Self::apply_visuals(ctx, &self.theme);
    }

    /// Persist General-tab state without the status toast — those rows apply
    /// live, so every change writes straight through.
    pub fn save_general(&mut self, ctx: &egui::Context) {
        self.refresh_theme(ctx);
        let _ = settings_store::save(&self.settings_snapshot());
    }

    /// Point the agent at `dir` — `None` means the app's own root. The
    /// backend resolves the folder once, at startup (its file skills capture
    /// it), so a change restarts the BE; sessions live on disk and are
    /// re-listed once it is back.
    pub fn set_working_dir(&mut self, dir: Option<PathBuf>, ctx: &egui::Context) {
        let dir = dir.filter(|p| p != &sica_core::paths::workspace_root());
        if dir == self.working_dir {
            return;
        }
        if let Some(p) = &dir {
            self.recent_working_dirs.retain(|r| r != p);
            self.recent_working_dirs.insert(0, p.clone());
            self.recent_working_dirs.truncate(RECENT_WORKING_DIRS);
        }
        self.working_dir = dir.clone();
        sica_core::paths::set_working_dir(dir.as_deref());
        self.workspace_name = sica_core::paths::working_dir()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "—".into());
        self.save_general(ctx);

        let path = sica_core::paths::working_dir().display().to_string();
        self.push_log(LogKind::Info, format!("working directory → {path}"));
        // Restart rather than rebuild: the binary is unchanged, only the
        // environment it starts in.
        if self.be_state.running {
            self.send(UiCommand::StopBe);
            self.send(UiCommand::StartBe);
        }
        self.show_toast(crate::ui::icons::Icon::Folder, format!("Working directory: {path}"), 2600);
    }

    /// Show a toast, replacing whatever is on screen (dsh shows one at a
    /// time; bumping the sequence restarts the fade).
    pub fn show_toast(&mut self, icon: crate::ui::icons::Icon, text: impl Into<String>, hold_ms: u64) {
        self.toast_seq += 1;
        self.toast = Some(crate::ui::kit::Toast::new(self.toast_seq, icon, text, hold_ms));
    }

    /// Route an orchestrator's progress line onto the chip of the call that
    /// is producing it (§6.11).
    ///
    /// The prefix is the association: `workflow` and `agent-team` print
    /// `"<skill>: …"`, and only one such call runs at a time in a session —
    /// both cap their children and drive them from a single dispatch — so
    /// the newest unfinished chip of that name is the right one. A line that
    /// arrives with no such chip running stays in the log panel alone, which
    /// is where it went before.
    fn note_on_running_chip(&mut self, message: &str) {
        const ORCHESTRATORS: [&str; 2] = ["workflow", "agent-team"];
        let Some(skill) = ORCHESTRATORS
            .iter()
            .find(|s| message.starts_with(&format!("{s}: ")))
        else {
            return;
        };
        let body = message[skill.len() + 2..].to_string();
        for turn in self.chat.turns.iter_mut().rev() {
            if let Some(chip) = turn
                .tool_chips
                .iter_mut()
                .rev()
                .find(|c| !c.finished && c.name == **skill)
            {
                // Bounded: a script that logs in a loop must not be able to
                // grow the transcript without limit.
                const MAX_NOTES: usize = 200;
                if chip.notes.len() < MAX_NOTES {
                    chip.notes.push(body);
                }
                return;
            }
        }
    }

    /// A `file://` link the transcript emitted (§3.8) — an inline-code path
    /// the model named, which the renderer turned into a link.
    ///
    /// Taken back out of egui's output before the platform sees it, because
    /// the platform would hand a local file to the *browser*. A plain click
    /// opens it the way the OS would; a modified click (egui reports it as
    /// "new tab") puts `@path` in the composer instead, which is how the
    /// user brings the file into the next message rather than reading it.
    fn take_file_link(&mut self, ctx: &egui::Context) {
        let Some(open) = ctx.output_mut(|o| o.open_url.take()) else { return };
        let Some(raw) = open.url.strip_prefix("file://") else {
            // Not ours: put it back for the platform to open.
            ctx.output_mut(|o| o.open_url = Some(open));
            return;
        };
        let path = std::path::PathBuf::from(raw);
        if open.new_tab {
            let cwd = self.session_workspace().1;
            let rel = path.strip_prefix(&cwd).unwrap_or(&path);
            let at = format!("@{}", rel.display().to_string().replace('\\', "/"));
            if !self.chat.draft.is_empty() && !self.chat.draft.ends_with(' ') {
                self.chat.draft.push(' ');
            }
            self.chat.draft.push_str(&at);
            self.chat.draft.push(' ');
            return;
        }
        if let Err(e) = crate::ui::open_path_public(&path) {
            self.push_log(LogKind::Error, format!("open {}: {e}", path.display()));
        }
    }

    fn settings_snapshot(&self) -> Settings {
        Settings {
            theme_dark:             self.theme_dark,
            theme_mode:             self.theme_mode.as_str().to_string(),
            content_px:             self.content_px,
            transcript_compact:     self.transcript_compact,
            busy_enter:             self.busy_enter.as_str().to_string(),
            reduce_motion:          self.reduce_motion,
            chat_content_width:     self.layout.content_w,
            log_raw_llm:            self.log_raw_llm,
            idealist_auto_apply_be: self.idealist_auto_apply_be,
            auto_start_be:          self.auto_start_be,
            auto_connect_llm:       self.auto_connect_llm,
            autoscroll:             self.autoscroll,
            release_profile:        self.release_profile,
            auto_watch:             self.auto_watch,
            last_active_provider:   self.active_provider_id.clone(),
            default_permission_mode: self.default_permission_mode.clone(),
            working_dir:            self
                .working_dir
                .as_ref()
                .map(|p| p.display().to_string()),
            recent_working_dirs:    self
                .recent_working_dirs
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            default_agent:          self.default_agent.clone(),
            onboarded:              self.onboarded,
            sidebar_group:          if self.workspaces.grouped {
                "workspace".into()
            } else {
                "flat".into()
            },
            sidebar_order:          if self.workspaces.by_updated {
                "updated".into()
            } else {
                "manual".into()
            },
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
        // A provider file may name an environment variable instead of
        // holding the key (guide §14.6): `api_key = "${DEEPSEEK_API_KEY}"`.
        // Resolved here, on the way to the wire, so the stored settings
        // keep the reference and never the secret.
        let api_key = sica_core::creds::resolve(&cfg.api_key);
        let api_key = if api_key.is_empty() { None } else { Some(api_key) };
        let options = cfg.llm_options();
        self.send(UiCommand::SendRequest(Request::ConnectLlm {
            base_url: cfg.base_url,
            model:    cfg.model,
            api_key,
            options,
        }));
    }

    /// Pour the token set into `egui::Style`. Every colour below is an
    /// alias — no literals, no theme branches (§1.3). The theme is also
    /// stashed in `Context` memory so `ui::kit` can read it without a
    /// palette threaded through every signature.
    fn apply_visuals(ctx: &egui::Context, theme: &Theme) {
        use egui::{FontFamily, FontId, Rounding, Stroke, TextStyle};
        use sica_core::theme::tokens::{
            FAMILY_MEDIUM, FAMILY_MONO, HAIRLINE, RADIUS_INPUT, RADIUS_MENU, RADIUS_MODAL,
        };

        crate::ui::kit::set_theme(ctx, *theme);

        let a = &theme.alias;
        let col = crate::ui::kit::col;
        let cola = crate::ui::kit::cola;

        let mut style = (*ctx.style()).clone();
        style.visuals = if theme.dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };

        // Type. Chrome sizes are fixed; `content` follows the user's ladder.
        let ui_font = FontFamily::Proportional;
        let medium = FontFamily::Name(FAMILY_MEDIUM.into());
        let mono = FontFamily::Name(FAMILY_MONO.into());
        let content = theme.content_px as f32;
        style.text_styles = [
            (TextStyle::Heading, FontId::new(16.0, medium.clone())),
            (TextStyle::Body, FontId::new(content, ui_font.clone())),
            (TextStyle::Monospace, FontId::new(12.0, mono.clone())),
            (TextStyle::Button, FontId::new(14.0, medium.clone())),
            (TextStyle::Small, FontId::new(12.0, ui_font.clone())),
            (
                TextStyle::Name("row-title".into()),
                FontId::new(theme.content_secondary_px(), medium.clone()),
            ),
            (TextStyle::Name("content".into()), FontId::new(content, ui_font.clone())),
            (TextStyle::Name("code-block".into()), FontId::new(11.0, mono.clone())),
            (TextStyle::Name("h1".into()), FontId::new(21.0 + theme.delta(), medium.clone())),
            (TextStyle::Name("h2".into()), FontId::new(19.0 + theme.delta(), medium.clone())),
            (TextStyle::Name("h3".into()), FontId::new(18.0 + theme.delta(), medium.clone())),
        ]
        .into();

        // Rhythm: dsh has no spacing token set — the scale is 2 4 6 8 …
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(10.0, 6.0);
        style.spacing.menu_margin = egui::Margin::same(4.0);
        style.spacing.scroll.bar_width = 8.0;
        style.spacing.scroll.floating = true;

        // Surfaces.
        style.visuals.panel_fill = col(a.bg_base);
        style.visuals.window_fill = col(a.bg_layer[1]);
        style.visuals.extreme_bg_color = col(a.code_block);
        style.visuals.faint_bg_color = col(a.tip);
        style.visuals.override_text_color = Some(col(a.label[0]));
        style.visuals.hyperlink_color = col(a.business);
        style.visuals.window_stroke = Stroke::new(HAIRLINE, cola(a.border[0]));
        style.visuals.window_rounding = Rounding::same(RADIUS_MODAL);
        style.visuals.menu_rounding = Rounding::same(RADIUS_MENU);
        style.visuals.popup_shadow = egui::epaint::Shadow {
            offset: egui::vec2(0.0, 3.0),
            blur: 12.0,
            spread: 0.0,
            color: egui::Color32::from_black_alpha(if theme.dark { 40 } else { 24 }),
        };
        style.visuals.window_shadow = style.visuals.popup_shadow;

        // Selection + caret are `business` — focus never shows as a fill.
        style.visuals.selection.bg_fill = col(a.business).linear_multiply(0.18);
        style.visuals.selection.stroke = Stroke::new(1.0, col(a.business));
        style.visuals.text_cursor.stroke = Stroke::new(1.5, col(a.business));

        let r: Rounding = Rounding::same(RADIUS_INPUT);
        let widgets = &mut style.visuals.widgets;
        widgets.noninteractive.rounding = r;
        widgets.noninteractive.bg_fill = col(a.bg_base);
        widgets.noninteractive.weak_bg_fill = col(a.bg_base);
        widgets.noninteractive.bg_stroke = Stroke::new(HAIRLINE, cola(a.border[0]));
        widgets.noninteractive.fg_stroke = Stroke::new(1.0, col(a.label[0]));

        widgets.inactive.rounding = r;
        widgets.inactive.bg_fill = egui::Color32::TRANSPARENT;
        widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
        widgets.inactive.bg_stroke = Stroke::new(HAIRLINE, cola(a.border[2]));
        widgets.inactive.fg_stroke = Stroke::new(1.0, col(a.label[0]));
        widgets.inactive.expansion = 0.0;

        widgets.hovered.rounding = r;
        widgets.hovered.bg_fill = cola(a.hover);
        widgets.hovered.weak_bg_fill = cola(a.hover);
        widgets.hovered.bg_stroke = Stroke::new(HAIRLINE, cola(a.border[2]));
        widgets.hovered.fg_stroke = Stroke::new(1.0, col(a.label[0]));
        widgets.hovered.expansion = 0.0;

        widgets.active.rounding = r;
        widgets.active.bg_fill = cola(a.active);
        widgets.active.weak_bg_fill = cola(a.active);
        widgets.active.bg_stroke = Stroke::new(HAIRLINE, cola(a.border[3]));
        widgets.active.fg_stroke = Stroke::new(1.0, col(a.label[0]));
        widgets.active.expansion = 0.0;

        widgets.open.rounding = r;
        widgets.open.bg_fill = cola(a.hover);
        widgets.open.weak_bg_fill = cola(a.hover);
        widgets.open.bg_stroke = Stroke::new(HAIRLINE, cola(a.border[2]));
        widgets.open.fg_stroke = Stroke::new(1.0, col(a.label[0]));

        style.visuals.warn_fg_color = col(a.warn);
        style.visuals.error_fg_color = col(a.error);

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
        self.chat.pending_delete = None;
        if self.chat.session_id == id {
            return;
        }
        self.chat.session_id = id;
        self.chat.unseen_sessions.remove(&id);
        self.chat.row_menu = None;
        self.chat.queued.clear();
        self.chat.queue_edit = None;
        self.chat.queue_edit_draft.clear();
        self.chat.turns.clear();
        self.chat.selected_turn = None;
        self.chat.autoscroll_paused = false;
        self.chat.middle_scroll_origin = None;
        self.chat.interrupt_requested = false;
        self.chat.compacting = false;
        self.chat.editing_turn = None;
        self.chat.edit_draft.clear();
        // The ledger belongs to the session that was open; drop it rather
        // than let a page for the old one land on the new one's view.
        self.trajectory = crate::app::TrajectoryState::default();
        // Same rule for the projections: a fold for the old session
        // must not be read as the new one's.
        let expanded = self.stats.expanded;
        self.stats = StatsState { expanded, ..Default::default() };
        self.details_call = None;
        self.layout.details_w = 0.0;
        self.send(UiCommand::SendRequest(Request::LoadSession { session_id: id }));
    }

    /// Ask the backend to re-fold the active session's projections (§3.3).
    ///
    /// Cheap enough to call on every session load and every finished turn:
    /// the fold is a pass over the log, and asking is how the numbers stay
    /// honest — there is no push channel because there is no state to
    /// subscribe to, only a log that grew.
    pub fn load_session_stats(&mut self) {
        if self.stats.loading {
            return;
        }
        let session_id = self.chat.session_id;
        if session_id == 0 {
            return;
        }
        self.stats.loading = true;
        self.send(UiCommand::SendRequest(Request::SessionStats { session_id }));
    }

    /// Ask for the next page of the active session's event log. Called when
    /// the Trajectory view opens, when the user reloads it, and when the
    /// ledger's "Load more" is pressed.
    ///
    /// `reset` starts from the top and throws away what is on screen — a
    /// turn that ran while the tab was open appended events the ledger has
    /// not seen, and continuing from `next_seq` would show them under a
    /// stale total.
    pub fn load_trajectory(&mut self, reset: bool) {
        if self.trajectory.loading {
            return;
        }
        let session_id = self.chat.session_id;
        let from_seq = if reset || self.trajectory.session_id != session_id {
            self.trajectory.rows.clear();
            // The envelope map is keyed by seq, and seqs are per session —
            // keeping it across a switch would answer the new session's rows
            // with the old session's prompt.
            self.trajectory.envelopes.clear();
            self.trajectory.session_id = session_id;
            self.trajectory.next_seq = None;
            0
        } else {
            match self.trajectory.next_seq {
                Some(seq) => seq,
                // Nothing more to fetch; a bare click on Reload is a reset.
                None if !self.trajectory.rows.is_empty() => return,
                None => 0,
            }
        };
        self.trajectory.loading = true;
        self.send(UiCommand::SendRequest(Request::LoadSessionEvents {
            session_id,
            from_seq,
            limit: 0,
        }));
    }

    /// Open the Trajectory view focused on one durable event — the Inspect
    /// pill on a tool row (§3.4). The page may not be loaded yet, so the
    /// selection is recorded first and the ledger scrolls to it when the row
    /// arrives.
    pub fn inspect_event(&mut self, seq: u64) {
        self.view = ChatView::Trajectory;
        self.trajectory.selected = Some(seq);
        self.trajectory.scroll_to = Some(seq);
        self.details_call = None;
        self.layout.details_w = sica_core::theme::tokens::DETAILS_DEFAULT;
        if self.trajectory.session_id != self.chat.session_id || self.trajectory.rows.is_empty() {
            self.load_trajectory(true);
        }
    }

    /// Rewrite the prompt of turn `idx` and re-run from it. Everything after
    /// it leaves the conversation, so the transcript is truncated here and a
    /// fresh turn pushed in its place — the same optimistic shape a send
    /// takes, and it matches the rewind the backend records.
    pub fn edit_user_message(&mut self, idx: usize, text: String) {
        self.chat.editing_turn = None;
        self.chat.edit_draft.clear();
        let text = text.trim().to_string();
        let session_id = self.chat.session_id;
        let Some(turn) = self.chat.turns.get(idx) else { return };
        let Some(seq) = turn.user_seq else { return };
        if text.is_empty() || text == turn.user {
            return;
        }
        // The attachments carry over — this edits what the user said, not
        // what they attached — but their textures are re-uploaded for the
        // new turn rather than shared with the one it replaces.
        let images: Vec<Attachment> = turn
            .images
            .iter()
            .map(|a| Attachment {
                mime: a.mime.clone(),
                data_base64: a.data_base64.clone(),
                texture: None,
            })
            .collect();
        self.chat.turns.truncate(idx);
        self.chat.turns.push(Turn {
            user: text.clone(),
            images,
            user_seq: None,
            ..Turn::new(session_id, 0)
        });
        self.chat.selected_turn = None;
        self.chat.scroll_to_bottom = true;
        self.chat.interrupt_requested = false;
        self.pending_edit_session = Some(session_id);
        self.send(UiCommand::SendRequest(Request::EditUserMessage {
            session_id,
            seq,
            text,
        }));
    }

    /// Ask the BE to drop `id` and remove it from the local list. If the
    /// deleted session was active, fall back to the first remaining session.
    pub fn delete_session(&mut self, id: u64) {
        self.chat.pending_delete = None;
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

    /// Ask the BE to abandon the in-flight turn for the active session, and
    /// flag the UI as stopping until the BE confirms with `TurnFinished`.
    /// Idempotent — pressing Stop twice sends twice, which the BE tolerates.
    pub fn interrupt_turn(&mut self) {
        let session_id = self.chat.session_id;
        self.chat.interrupt_requested = true;
        self.send(UiCommand::SendRequest(Request::InterruptTurn { session_id }));
    }

    /// Find the active turn and return a mutable ref. Marker entries (context
    /// notices) are skipped — a notice pushed between turns must never absorb
    /// the next stream's deltas.
    fn active_turn_mut(&mut self) -> Option<&mut Turn> {
        self.chat.turns.iter_mut().rev().find(|t| t.notice.is_none())
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.ui_rx.try_recv() {
            self.handle_event(ev);
        }
    }

    fn handle_event(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::Log(s) => self.push_log(LogKind::Info, s),
            // The BE's own level survives the hop now (§9): a WARN from the
            // tool-call parser has to be visible without opening Settings,
            // so it also raises a toast.
            UiEvent::LogLine { level, message } => {
                self.note_on_running_chip(&message);
                let kind = LogKind::from_level(&level);
                match kind {
                    LogKind::Error => {
                        self.show_toast(crate::ui::icons::Icon::Warning, message.clone(), 6000)
                    }
                    LogKind::Warn => {
                        self.show_toast(crate::ui::icons::Icon::Warning, message.clone(), 3000)
                    }
                    _ => {}
                }
                self.push_log(kind, message);
            }
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
                if self.had_outage {
                    self.recovered_at = Some(Instant::now());
                }
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
                // The workspace projection (§4.3). Pulled once per connect;
                // every later change arrives as `WorkspacesChanged`.
                self.send(UiCommand::SendRequest(Request::ListWorkspaces));
                // Refresh the "/" palette: skills and markdown files are read
                // by the BE at startup, so a reconnect is exactly when the
                // catalogue can have changed.
                self.send(UiCommand::SendRequest(Request::ListCatalog));
            }
            UiEvent::IpcDisconnected { error } => {
                self.had_outage = true;
                self.recovered_at = None;
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
                self.gen_speed.on_turn_started();
                // The backend opens a turn per *hop*; only the first one of a
                // request meets the bubble the composer pushed on send.
                let first_hop = !self.chat.running_sessions.contains(&session_id);
                self.chat.running_sessions.insert(session_id);
                self.chat.unseen_sessions.remove(&session_id);
                // A new turn retires the checklist (the projection clears
                // on turn start; the log keeps the audit).
                if session_id == self.chat.session_id {
                    self.todos.clear();
                }
                // An editor left open while the conversation moves under it
                // is anchored to an index that no longer means what it did.
                if session_id == self.chat.session_id {
                    self.chat.editing_turn = None;
                }
                // That bubble is a real, unfinished turn already — adopting it
                // instead of opening a second row is what keeps it from
                // sitting there unfinished (and shimmering "Working…") long
                // after the request ended.
                let adopt = first_hop
                    && self.chat.turns.last().is_some_and(|t| {
                        t.notice.is_none()
                            && !t.finished
                            && t.session_id == session_id
                            && t.assistant.is_empty()
                            && t.reasoning.is_empty()
                            && t.tool_chips.is_empty()
                    });
                match self.chat.turns.last_mut() {
                    Some(t) if adopt => {
                        t.turn_id = turn_id;
                        t.queued = false;
                    }
                    _ => self.chat.turns.push(Turn::new(session_id, turn_id)),
                }
                self.chat.scroll_to_bottom = true;
            }
            // The prompt that just landed is the *oldest* one this session
            // has on screen without a seq: the composer pushes its bubble
            // optimistically on send, and a queued followup keeps that
            // bubble until the loop claims it, so unaddressed bubbles retire
            // in the order they were sent.
            UiEvent::UserMessageStored { session_id, seq } => {
                self.pending_edit_session = None;
                if session_id != self.chat.session_id {
                    return;
                }
                // Messages are stored in the order they were sent, so the
                // oldest prompt still missing its handle is this one.
                if let Some(t) = self.chat.turns.iter_mut().find(|t| {
                    t.notice.is_none()
                        && t.user_seq.is_none()
                        && !(t.user.is_empty() && t.images.is_empty())
                }) {
                    t.user_seq = Some(seq);
                }
            }
            UiEvent::WorkspacesChanged { rows, ungrouped } => {
                // A folder the user just added: open a session in it, which
                // is the whole reason they added it. Matched by path — the
                // id is the backend's to mint, and registering a directory
                // that was already known answers with the row it already had.
                let created = self.workspaces.creating.take().and_then(|path| {
                    rows.iter()
                        .find(|w| w.path.display().to_string() == path)
                        .map(|w| w.id)
                });
                self.workspaces.rows = rows;
                self.workspaces.ungrouped = ungrouped;
                if let Some(id) = created {
                    self.workspaces.collapsed.remove(&id);
                    self.send(UiCommand::SendRequest(Request::NewSession {
                        workspace_id: Some(id),
                    }));
                }
            }
            UiEvent::RequestFailed { message } => {
                // A refused `CreateWorkspace` is the one failure with its own
                // surface: the folder cannot be adopted, and the user has to
                // pick another one.
                if self.workspaces.creating.take().is_some() {
                    self.workspaces.error = Some(message.clone());
                }
                self.show_toast(crate::ui::icons::Icon::Warning, message.clone(), 6000);
                self.push_log(LogKind::Error, message);
                // An edit truncated the transcript before the backend had
                // agreed to it. Pull the real history back.
                if let Some(id) = self.pending_edit_session.take() {
                    if id == self.chat.session_id {
                        self.send(UiCommand::SendRequest(Request::LoadSession {
                            session_id: id,
                        }));
                    }
                }
            }
            UiEvent::AssistantDelta { content, reasoning, .. } => {
                if let Some(t) = self.active_turn_mut() {
                    t.assistant.push_str(&content);
                    t.reasoning.push_str(&reasoning);
                }
                self.chat.scroll_to_bottom = true;
            }
            UiEvent::TurnFinished { session_id, finish_reason, .. } => {
                self.gen_speed.on_turn_finished();
                // `running_sessions` is *not* cleared here: this is the end of
                // one hop, and the request goes on through the tool it just
                // asked for. `TurnUsage` closes the request.
                if session_id != self.chat.session_id {
                    self.chat.unseen_sessions.insert(session_id);
                }
                if let Some(t) = self.active_turn_mut() {
                    t.finished = true;
                    t.finish_reason = Some(finish_reason);
                    t.reasoning_collapsed = true;
                }
                self.chat.interrupt_requested = false;
                self.chat.scroll_to_bottom = true;
                // The turn appended rows; the counters and the outline
                // are one turn behind until we re-fold.
                if session_id == self.chat.session_id {
                    self.load_session_stats();
                }
            }
            UiEvent::TokenUsage { used, limit, budget, breakdown, .. } => {
                if breakdown.is_some() {
                    self.token_breakdown = breakdown;
                }
                self.tokens.used.store(used, Ordering::Relaxed);
                self.tokens.limit.store(limit, Ordering::Relaxed);
                self.tokens.budget.store(budget, Ordering::Relaxed);
                self.gen_speed.on_token_usage(used);
            }
            UiEvent::ContextCompacting { session_id } => {
                if session_id == self.chat.session_id {
                    self.chat.compacting = true;
                }
                self.push_log(
                    LogKind::Event,
                    "context: compressing older history to free window space…".into(),
                );
            }
            UiEvent::ContextCompacted {
                session_id, ok, folded, before_tokens, after_tokens, summary, pruned,
            } => {
                if session_id == self.chat.session_id {
                    self.chat.compacting = false;
                }
                let label = if ok && folded == 0 && pruned > 0 {
                    format!(
                        "Context pruned · {pruned} oversized older tool result(s) \
                         trimmed to head/tail windows · no summary needed"
                    )
                } else if ok {
                    let saved = before_tokens.saturating_sub(after_tokens);
                    let mut l = format!(
                        "Context compressed · {folded} messages → summary · \
                         {before_tokens} → {after_tokens} tokens (−{saved})"
                    );
                    if pruned > 0 {
                        l.push_str(&format!(" · {pruned} result(s) pruned"));
                    }
                    l
                } else {
                    "Context compression failed · oldest messages will be trimmed instead"
                        .to_string()
                };
                self.push_log(
                    if ok { LogKind::Event } else { LogKind::Error },
                    format!("context: {label}"),
                );
                // Only the active session's transcript is on screen; a notice
                // for a background session would land in the wrong scrollback.
                if session_id == self.chat.session_id {
                    self.chat.turns.push(Turn::marker(
                        session_id,
                        Notice::new(NoticeKind::Compaction, label, summary, ok),
                    ));
                    self.chat.scroll_to_bottom = true;
                }
            }
            UiEvent::ToolCallStarted {
                id, parent_id, depth, name, args_preview, expectation, args_json, call_seq,
            } => {
                if let Some(t) = self.active_turn_mut() {
                    t.tool_chips.push(ToolChip {
                        id, log_seq: call_seq, parent_id, depth, name,
                        args_preview, expectation, args_json,
                        finished: false, ok: true, summary: String::new(),
                        output: String::new(), duration_ms: 0,
                        expanded: false,
                        notes: Vec::new(),
                    });
                }
            }
            UiEvent::ToolCallFinished { id, ok, summary, output, duration_ms } => {
                if let Some(t) = self.active_turn_mut() {
                    if let Some(chip) = t.tool_chips.iter_mut().find(|c| c.id == id) {
                        chip.finished = true;
                        chip.ok = ok;
                        chip.summary = summary;
                        chip.output = output;
                        chip.duration_ms = duration_ms;
                    }
                }
            }
            UiEvent::LlmRetry { session_id, attempt, max, delay_ms, reason } => {
                if session_id == self.chat.session_id {
                    if let Some(t) = self.active_turn_mut() {
                        t.retries.push(RetryRow {
                            attempt,
                            max,
                            delay_ms,
                            reason,
                            at: std::time::Instant::now(),
                        });
                    }
                    self.chat.scroll_to_bottom = true;
                }
            }
            UiEvent::TurnUsage {
                session_id, prompt, completion, reasoning, duration_ms, ttft_ms, ..
            } => {
                // One `TurnUsage` per *request* — so this, not the last hop's
                // `TurnFinished`, is where the session stops being busy. Stop
                // pressed between two hops ends the request without a further
                // `TurnFinished`, so the stopping flag is cleared here too.
                self.chat.running_sessions.remove(&session_id);
                if session_id == self.chat.session_id {
                    self.chat.interrupt_requested = false;
                }
                // The backend counts one turn where the transcript shows one
                // row per hop, so the accounting lands on the last row of that
                // session — the one carrying the final answer, which is where
                // the tail pills belong.
                if let Some(t) = self
                    .chat
                    .turns
                    .iter_mut()
                    .rev()
                    .find(|t| t.notice.is_none() && t.session_id == session_id)
                {
                    t.usage = Some(TurnUsage {
                        prompt, completion, reasoning, duration_ms, ttft_ms,
                    });
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
                // Newest on top — the list reads most-recent-first.
                // Last-updated order (§4.2). A session with no events yet
                // falls back to its creation time rather than sorting last.
                sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at.max(s.created_at)));
                if sessions.is_empty() {
                    // First-run case: ask the BE to mint a session so the user
                    // has something to type into.
                    self.send(UiCommand::SendRequest(Request::NewSession { workspace_id: None }));
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
            UiEvent::ModelsListed { base_url, models, error } => {
                self.models_pending.remove(&base_url);
                let entry = match error {
                    Some(e) => {
                        self.push_log(LogKind::Error, format!("models: {base_url} — {e}"));
                        Err(e)
                    }
                    None => Ok(models),
                };
                self.provider_models.insert(base_url, entry);
            }
            UiEvent::SessionSearch { hits } => {
                self.chat.search_hits = hits;
            }
            UiEvent::SessionCreated { id } => {
                if !self.chat.sessions.iter().any(|s| s.id == id) {
                    // Insert at the front — the list is most-recent-first.
                    self.chat.sessions.insert(0, SessionMeta {
                        id,
                        title: format!("Session {id}"),
                        created_at: 0,
                        updated_at: 0,
                        cwd: None,
                    });
                }
                self.switch_session(id);
                // Fresh sessions start in the configured default mode.
                let mode = protocol::PermissionMode::parse(&self.default_permission_mode)
                    .unwrap_or_default();
                self.send(UiCommand::SendRequest(Request::SetPermissionMode {
                    session_id: id,
                    mode,
                }));
                // …and in the configured default preset (§7.2). Sent right
                // after creation, which is the only moment it can be set:
                // a session's preset is fixed once it has produced anything.
                if let Some(name) = self.default_agent.clone() {
                    self.send(UiCommand::SendRequest(Request::SetSessionAgent {
                        session_id: id,
                        name: Some(name),
                    }));
                }
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
                self.load_session_stats();
                self.permission_mode = session.permission_mode;
                self.plan_active = session.plan_active;
                self.session_agent = session.agent;
                self.todos = session.todos;
                // The backend pushes this session's `JobsChanged` alongside
                // the dump; clearing here keeps the previous session's jobs
                // off screen in the frame before it lands.
                self.jobs.clear();
                self.goal = None;
                // An objective half-edited in the session being left must not
                // be committed against the one being opened.
                self.goal_edit = None;
            }
            UiEvent::SessionStats { session_id, stats, outline, through_seq } => {
                self.stats.loading = false;
                // A fold for a session the user has already left is
                // not stale, it is about something else.
                if session_id != self.chat.session_id {
                    return;
                }
                self.stats.session_id = session_id;
                self.stats.stats = Some(stats);
                self.stats.outline = outline;
                self.stats.through_seq = through_seq;
            }
            UiEvent::SessionEvents { session_id, events, envelopes, total, next_seq } => {
                self.trajectory.loading = false;
                // A page for a session the user has already left is dropped,
                // the same rule `SessionLoaded` follows.
                if session_id != self.chat.session_id {
                    return;
                }
                self.trajectory.session_id = session_id;
                self.trajectory.total = total;
                self.trajectory.next_seq = next_seq;
                if let Some(first) = events.first().map(|e| e.seq) {
                    // Re-fetching a page that is already on screen (a reload
                    // after new events landed) replaces from that seq on
                    // rather than duplicating the rows.
                    self.trajectory.rows.retain(|r| r.seq < first);
                }
                self.trajectory.rows.extend(events);
                for env in envelopes {
                    self.trajectory.envelopes.insert(env.seq, env);
                }
            }
            UiEvent::Catalog { entries } => {
                self.push_log(
                    LogKind::Event,
                    format!("catalog: {} entries available to the / palette", entries.len()),
                );
                self.chat.slash.entries = entries;
            }
            UiEvent::SessionTitleChanged { session_id, title } => {
                if let Some(s) = self.chat.sessions.iter_mut().find(|s| s.id == session_id) {
                    s.title = title;
                }
            }
            UiEvent::CommandResult { text } => {
                self.push_log(LogKind::Event, format!("command: {text}"));
                // Compaction rewrites history — pull the fresh transcript
                // for the session the command targeted.
                if let Some(id) = self.last_command_session {
                    if id == self.chat.session_id {
                        self.send(UiCommand::SendRequest(Request::LoadSession { session_id: id }));
                    }
                    self.last_command_session = None;
                }
            }
            UiEvent::ApprovalRequested { id, session_id, skill, args_preview, reason } => {
                self.push_log(
                    LogKind::Event,
                    format!("approval requested: {skill} — {reason}"),
                );
                self.chat.waiting_sessions.insert(session_id);
                self.pending_approval = Some(PendingApproval {
                    id, session_id, skill, args_preview, reason,
                });
            }
            UiEvent::QuestionAsked { id, session_id, question, detail, options, multi } => {
                self.push_log(LogKind::Event, "question asked — answer in the composer".into());
                // `exit-plan-mode` asks its review as an Approve/Refuse
                // question; the takeover renders it as a plan review.
                let lowered = question.to_lowercase();
                let plan_review = options.iter().any(|o| o.eq_ignore_ascii_case("approve"))
                    && (lowered.contains("plan") || options.len() <= 3);
                self.chat.waiting_sessions.insert(session_id);
                self.pending_question = Some(PendingQuestion {
                    id, session_id, question, detail,
                    picked: vec![false; options.len()],
                    options, multi,
                    draft: String::new(),
                    plan_review,
                });
            }
            UiEvent::TodosChanged { session_id, items } => {
                if session_id == self.chat.session_id {
                    self.todos = items;
                    self.chat.scroll_to_bottom = true;
                }
            }
            UiEvent::PlanModeChanged { session_id, active } => {
                if session_id == self.chat.session_id {
                    self.plan_active = active;
                }
                self.push_log(
                    LogKind::Event,
                    format!("plan mode {}", if active { "on" } else { "off" }),
                );
            }
            UiEvent::PermissionModeChanged { session_id, mode } => {
                if session_id == self.chat.session_id {
                    self.permission_mode = mode;
                }
            }
            UiEvent::SessionAgentChanged { session_id, name } => {
                if session_id == self.chat.session_id {
                    self.session_agent = name;
                }
            }
            UiEvent::JobsChanged { session_id, jobs } => {
                if session_id == self.chat.session_id {
                    self.jobs = jobs;
                }
            }
            UiEvent::GoalChanged { session_id, goal } => {
                if session_id == self.chat.session_id {
                    // The bar's editor is closed by the change it asked for
                    // — and by any other change, since the text it holds was
                    // a rewording of an objective that no longer stands.
                    self.goal_edit = None;
                    self.goal = goal;
                }
            }
            UiEvent::InboxChanged { session_id, queued, accepted } => {
                if session_id == self.chat.session_id {
                    match accepted.as_str() {
                        // The optimistic turn this send pushed is waiting
                        // behind the running one — say so instead of
                        // letting it read as a stalled stream.
                        "queued" => {
                            if let Some(t) = self
                                .chat
                                .turns
                                .iter_mut()
                                .rev()
                                .find(|t| t.notice.is_none() && !t.finished)
                            {
                                t.queued = true;
                            }
                        }
                        // The oldest queued message just became the running
                        // turn; `TurnStarted` fills the rest in.
                        "running" => {
                            if let Some(t) = self
                                .chat
                                .turns
                                .iter_mut()
                                .find(|t| t.queued)
                            {
                                t.queued = false;
                            }
                        }
                        _ => {}
                    }
                }
                self.push_log(
                    LogKind::Event,
                    format!("message {accepted} ({queued} queued)"),
                );
            }
            UiEvent::QueueChanged { session_id, rows } => {
                if session_id != self.chat.session_id {
                    return;
                }
                // The backend's list supersedes every local echo: a row it
                // does not list either ran or was dropped, and keeping the
                // echo would offer actions against a message that is gone.
                self.chat.queued = rows
                    .into_iter()
                    .map(|r| QueuedRow { id: Some(r.id), text: r.text, images: r.images })
                    .collect();
                // An open editor whose row left the queue has nothing to
                // save to.
                if let Some(id) = self.chat.queue_edit {
                    if !self.chat.queued.iter().any(|r| r.id == Some(id)) {
                        self.chat.queue_edit = None;
                        self.chat.queue_edit_draft.clear();
                    }
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

/// Rebuild a `Vec<Turn>` from a session's persisted message list. Walks the
/// messages, pairing each user message with the assistant message that
/// follows it (if any). System messages are skipped — except the
/// auto-compaction summary, which becomes a marker so a reloaded session
/// shows where its history was compressed instead of just starting
/// abruptly. Tool-role messages whose dump carries `tool_name` (recovered
/// from the backend's event log) become finished chips on the current turn;
/// tool messages migrated from the pre-event-log format have no such
/// metadata and are skipped. `context`-role messages (harness-injected) are
/// markers when they are a `/name` load, otherwise omitted.
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
                    user: m.content.clone(),
                    finished: true,
                    reasoning_collapsed: true,
                    images: m.images.iter().map(Attachment::from_user_image).collect(),
                    // A dump from a backend older than the field carries 0,
                    // which is not a seq — such a turn simply offers no edit.
                    user_seq: (m.seq > 0).then_some(m.seq),
                    ..Turn::new(session.id, turns.len() as u64 + 1)
                });
            }
            "assistant" => {
                let slot = current.get_or_insert_with(|| Turn {
                    finished: true,
                    reasoning_collapsed: true,
                    ..Turn::new(session.id, turns.len() as u64 + 1)
                });
                slot.assistant = m.content.clone();
                if let Some(r) = &m.reasoning {
                    slot.reasoning = r.clone();
                }
            }
            "tool" => {
                let Some(name) = m.tool_name.clone() else { continue };
                let Some(slot) = current.as_mut() else { continue };
                // Synthetic id: no live `ToolCallFinished` will ever look it
                // up, and chips only need ids to be distinct within a turn.
                // The log's `ToolCall` seq is the identity that survives a
                // restart; fall back to a synthetic id for logs written
                // before the field existed. Either way ids only need to be
                // distinct within the turn.
                let id = m
                    .tool_call_id
                    .unwrap_or(u64::MAX - slot.tool_chips.len() as u64);
                slot.tool_chips.push(ToolChip {
                    id,
                    log_seq: m.tool_call_id.unwrap_or(0),
                    parent_id: m.tool_parent_id,
                    depth: m.tool_depth,
                    name: name.clone(),
                    args_preview: m.tool_args_preview.clone().unwrap_or(name),
                    expectation: m.tool_expectation.clone().unwrap_or_default(),
                    finished: true,
                    ok: m.tool_ok.unwrap_or(true),
                    // On reload the stored outcome is all there is: the
                    // pre-summariser output was never durable.
                    summary: m.content.clone(),
                    output: m.content.clone(),
                    args_json: m.tool_args_json.clone().unwrap_or_default(),
                    duration_ms: 0,
                    expanded: false,
                    notes: Vec::new(),
                });
            }
            "system" if m.content.starts_with(protocol::CONTEXT_SUMMARY_PREFIX) => {
                if let Some(t) = current.take() {
                    turns.push(t);
                }
                turns.push(Turn::marker(
                    session.id,
                    Notice::new(
                        NoticeKind::Compaction,
                        "Context compacted",
                        m.content.clone(),
                        true,
                    ),
                ));
            }
            // Harness-injected context. A `/name` load precedes the user
            // message it belongs to, so it becomes a marker between turns
            // with the loaded body on hover. Anything else (a loop-guard
            // notice mid-turn) is left out of the transcript rather than
            // split a turn in two — the log panel already showed it live.
            "context" if m.content.starts_with("<skill_content") => {
                if let Some(t) = current.take() {
                    turns.push(t);
                }
                let name = m
                    .content
                    .split_once("name=\"")
                    .and_then(|(_, rest)| rest.split_once('"'))
                    .map(|(n, _)| n.to_string())
                    .unwrap_or_default();
                turns.push(Turn::marker(
                    session.id,
                    Notice::new(
                        NoticeKind::Injection,
                        format!("/{name}"),
                        m.content.clone(),
                        true,
                    ),
                ));
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
        self.take_file_link(ctx);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gen_speed_ignores_prompt_baseline() {
        let mut g = GenSpeed::default();
        g.on_turn_started();
        assert!(g.streaming);
        // First reading of a turn is the prompt baseline — not generation.
        g.on_token_usage(5000);
        assert_eq!(g.completed, 0);
        assert_eq!(g.tps, 0.0);
        // Growth past the baseline counts as generated tokens.
        g.on_token_usage(5010);
        assert_eq!(g.completed, 10);
    }

    #[test]
    fn gen_speed_freezes_turn_average_on_finish() {
        let mut g = GenSpeed::default();
        g.on_turn_started();
        g.on_token_usage(1000);
        g.completed = 50;
        g.elapsed_secs = 2.0;
        g.on_turn_finished();
        assert!(!g.streaming);
        assert!((g.tps - 25.0).abs() < f32::EPSILON);
    }

    #[test]
    fn gen_speed_ignores_usage_outside_a_turn() {
        let mut g = GenSpeed::default();
        g.on_token_usage(1234);
        assert_eq!(g.tps, 0.0);
        assert_eq!(g.completed, 0);
    }
}
