//! Wire protocol shared by backend and frontend.
//!
//! A single duplex stream carries every message. Each `Frame` carries a
//! correlation `id` (0 for unsolicited events) and a tagged `Payload`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROTOCOL_VERSION: u32 = 14;

/// Default prompt-budget occupancy (percent) at which the backend folds older
/// history into an LLM-written summary instead of letting the trimmer amputate
/// it. The actual trigger is per-connection [`CompactPolicy::threshold_pct`];
/// this constant is the default and what the frontend's status-bar meter tints
/// against when it has not been told otherwise.
pub const COMPACT_TRIGGER_PCT: u32 = 80;

/// Compaction policy knobs sent with `ConnectLlm`. Mirrors dsh's per-routed-
/// model compaction config: trigger early enough to leave room for the reply,
/// keep a verbatim tail, cap the summary, and retry once on a bad summary.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactPolicy {
    /// Fold when the prompt reaches this percent of the budget.
    pub threshold_pct: u32,
    /// Share of the budget (percent) kept verbatim as the tail.
    pub retain_pct: u32,
    /// Completion cap for the summarisation call.
    pub max_tokens: u32,
    /// Extra attempts when the summariser returns nothing usable or is cut
    /// off by `max_tokens` (a truncated summary is discarded, never kept).
    pub retries: u32,
}

impl Default for CompactPolicy {
    fn default() -> Self {
        Self { threshold_pct: COMPACT_TRIGGER_PCT, retain_pct: 16, max_tokens: 8192, retries: 1 }
    }
}

/// Prefix the backend stamps on the system message that replaces compacted
/// history. Shared so the frontend can recognise it when rebuilding a
/// transcript from disk and render it as a marker rather than dropping it.
pub const CONTEXT_SUMMARY_PREFIX: &str = "[context summary";

/// Harness permission modes (Wave 3, guide §10.3). Policy level first:
/// the mode decides which tools the pipeline allows, denies, or asks
/// about. OS-level enforcement (restricted tokens, ACLs) is future work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PermissionMode {
    /// No mutations: `write-file`, `edit-file`, `skill-creator` and every
    /// non-read-only shell command are denied.
    ReadOnly,
    /// Writes stay inside the workspace; destructive-looking shell
    /// commands ask first.
    #[default]
    WorkspaceWrite,
    /// Everything allowed, approval policy `never`.
    DangerFullAccess,
}

impl PermissionMode {
    pub fn label(&self) -> &'static str {
        match self {
            PermissionMode::ReadOnly => "read-only",
            PermissionMode::WorkspaceWrite => "workspace-write",
            PermissionMode::DangerFullAccess => "danger-full-access",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            PermissionMode::ReadOnly =>
                "No mutations. Writes and non-read-only shell commands are denied.",
            PermissionMode::WorkspaceWrite =>
                "Writes stay inside the workspace. Destructive shell commands ask first.",
            PermissionMode::DangerFullAccess =>
                "Everything allowed, nothing asks. Only for sandboxes you can throw away.",
        }
    }

    /// Parse the `/permission` command input or a settings string.
    /// Accepts the canonical labels plus short aliases.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "read-only" | "readonly" | "read" | "ro" => Some(PermissionMode::ReadOnly),
            "workspace-write" | "workspace" | "write" | "ww" => {
                Some(PermissionMode::WorkspaceWrite)
            }
            "danger-full-access" | "danger" | "full" | "full-access" => {
                Some(PermissionMode::DangerFullAccess)
            }
            _ => None,
        }
    }

    /// One-line policy summary for the runtime-context snapshot, so the
    /// model always knows the policy it runs under.
    pub fn context_line(&self) -> &'static str {
        match self {
            PermissionMode::ReadOnly =>
                "read-only (no writes; only read-only shell commands run)",
            PermissionMode::WorkspaceWrite =>
                "workspace-write (writes outside the workspace are denied; \
                 destructive shell commands ask first)",
            PermissionMode::DangerFullAccess =>
                "danger-full-access (everything allowed; nothing asks)",
        }
    }
}

/// One row of the durable `todo-write` list (Wave 3, guide §11.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status:  TodoStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    pub fn label(&self) -> &'static str {
        match self {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Completed => "completed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().replace('-', "_").as_str() {
            "pending" => Some(TodoStatus::Pending),
            "in_progress" | "inprogress" | "active" => Some(TodoStatus::InProgress),
            "completed" | "complete" | "done" => Some(TodoStatus::Completed),
            _ => None,
        }
    }
}

/// Tunables the frontend passes along with `ConnectLlm`. Kept as a struct so
/// adding a knob later is one field, not a new request variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmOptions {
    /// Sampling temperature for every chat request.
    pub temperature: f32,
    /// Per-response completion cap. `None` = let the server decide.
    pub max_tokens: Option<u32>,
    /// Prompt-window budget used for history trimming and the token meter.
    /// `None` = auto-detect from the server (llama.cpp `/props` `n_ctx`,
    /// i.e. the launched `--ctx-size`, then vLLM `max_model_len` /
    /// llama.cpp `n_ctx_train`), falling back to 24k.
    pub context_window: Option<u32>,
    /// Use the OpenAI-native `tools` / `tool_calls` API instead of the
    /// text-protocol tool calling. Requires a server + template with tool
    /// support (e.g. vLLM with `--enable-auto-tool-choice`).
    pub native_tools: bool,
    /// Let the model emit reasoning (`<think>` blocks). When off, requests
    /// carry `chat_template_kwargs: {"enable_thinking": false}` so servers
    /// that template the toggle (llama.cpp, vLLM/Qwen) skip reasoning
    /// entirely — faster replies at some quality cost.
    pub thinking: bool,
    /// How and when history is folded into an LLM-written summary. Defaults
    /// to dsh's 80/16 policy; the frontend can override per provider.
    #[serde(default)]
    pub compact: CompactPolicy,
}

impl Default for LlmOptions {
    fn default() -> Self {
        Self {
            temperature: 0.2,
            max_tokens: None,
            context_window: None,
            native_tools: false,
            thinking: true,
            compact: CompactPolicy::default(),
        }
    }
}

/// One image attached to a user message. `data_base64` is the raw image bytes
/// base64-encoded (no `data:` URL prefix). `mime` is the MIME type, e.g.
/// `image/png`, `image/jpeg`. Used both on the wire (`SendUserMessage`) and
/// in persisted session storage (via `sica_core::message::Message`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserImage {
    pub mime: String,
    pub data_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub id: u64,
    pub payload: Payload,
}

// Note: externally-tagged enums are used so bincode (v1) can deserialize.
// bincode doesn't support `#[serde(tag, content)]`-style internal tags because
// it doesn't preserve field names on the wire.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Payload {
    ClientHello { protocol_version: u32 },
    ServerHello { protocol_version: u32, pid: u32, version: String },
    Request(Request),
    Response(Response),
    Event(Event),
    Ping,
    Pong,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    // Legacy demo requests (still used in the Communication tab).
    GetCounter,
    IncrementCounter { by: i64 },
    ResetCounter,
    ComputeFib { n: u32 },
    EchoText { text: String },
    Shutdown,

    // Chat / LLM control.
    SendUserMessage { session_id: u64, text: String, images: Vec<UserImage> },
    InterruptTurn   { session_id: u64 },
    NewSession,
    ListSessions,
    LoadSession   { session_id: u64 },
    DeleteSession { session_id: u64 },
    ConnectLlm    { base_url: String, model: String, api_key: Option<String>, options: LlmOptions },
    DisconnectLlm,

    /// Everything the workspace can offer the "/" palette: the live skill
    /// registry plus the markdown-defined agents and commands on disk.
    ListCatalog,

    /// Run a harness command that never creates a model message (`compact`,
    /// `plan`, `permission`). Logged as `Command`, answered with
    /// `CommandResult`.
    RunCommand { session_id: u64, name: String, input: String },
    /// Switch a session's permission mode (`read-only | workspace-write |
    /// danger-full-access`). Durable; the model is told via runtime context.
    SetPermissionMode { session_id: u64, mode: PermissionMode },
    /// Enter (`active: true`) or leave plan mode. Leaving is normally done
    /// through the `exit-plan-mode` tool so the plan gets reviewed first.
    SetPlanMode { session_id: u64, active: bool },
    /// Answer a pipeline approval request (`ApprovalRequested`).
    ResolveApproval { id: u64, allow: bool },
    /// Answer an `ask-user` / plan-review question (`QuestionAsked`).
    AnswerQuestion { id: u64, answer: String },

    /// Splice `text` into the *running* turn as a user message at its next
    /// hop, instead of queueing it as a separate turn. This is the user
    /// redirecting an agent mid-flight; with no turn running it is an
    /// ordinary send.
    SteerTurn { session_id: u64, text: String },
    /// Put non-user context into the session at the next hop (or at the
    /// start of the next turn when idle). Injected content is model-visible
    /// but never attributed to the user.
    InjectContext { session_id: u64, text: String },

    // Frontend telemetry — feeds the idealist's classifier.
    ReportFrontendError { module: String, message: String, traceback: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    CounterValue { value: i64 },
    FibResult    { n: u32, value: u128 },
    Echoed       { text: String },
    Ok,
    Error        { message: String },
    SessionList    { sessions: Vec<SessionMeta> },
    SessionCreated { id: u64 },
    SessionLoaded  { session: SessionDump },
    Catalog        { entries: Vec<CatalogEntry> },
    /// Outcome text of a `RunCommand` (shown in the log panel; never model
    /// history).
    CommandResult  { text: String },
}

/// Which family a [`CatalogEntry`] belongs to. Drives the group headings in
/// the frontend's "/" palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CatalogKind {
    /// A callable skill from the live `SkillRegistry` — the Rust built-ins
    /// plus every `skills/*.md`.
    Skill,
    /// A markdown persona from `agents/*.md`.
    Agent,
    /// A markdown prompt from `commands/*.md`. The frontend's own app actions
    /// (`/new`, `/stop`, …) share this kind but never travel over the wire.
    Command,
}

/// One selectable row in the "/" palette.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub kind:        CatalogKind,
    pub name:        String,
    pub description: String,
    /// Ordered positional argument names, as declared by the skill or by the
    /// file's `positional:` frontmatter. Empty when the entry takes none.
    pub args:        Vec<String>,
    /// Display path of the file backing this entry, when there is one. Shown
    /// as the row's hover text so the user can find the file to edit.
    pub source:      Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: u64,
    pub title: String,
    pub created_at: i64,
}

/// Wire-format dump of a full session's history. Kept separate from
/// `sica_core::session::Session` so the `protocol` crate stays leaf-level
/// (no dep on `sica-core`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDump {
    pub id: u64,
    pub title: String,
    pub created_at: i64,
    pub messages: Vec<MessageDump>,
    /// Current permission mode — drives the FE status-bar pill without a
    /// second round-trip.
    #[serde(default)]
    pub permission_mode: PermissionMode,
    /// Whether plan mode is active — drives the FE composer toggle.
    #[serde(default)]
    pub plan_active: bool,
    /// Latest durable todo list — drives the FE checklist on reload.
    #[serde(default)]
    pub todos: Vec<TodoItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDump {
    pub role: String,
    pub content: String,
    pub reasoning: Option<String>,
    #[serde(default)]
    pub images: Vec<UserImage>,
    /// On `tool`-role messages: the skill that ran, recovered from the
    /// session's event log so a reloaded transcript can rebuild its tool
    /// chips. `None` for messages migrated from the pre-event-log format.
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_ok: Option<bool>,
    #[serde(default)]
    pub tool_args_preview: Option<String>,
    #[serde(default)]
    pub tool_expectation: Option<String>,
    /// On `context`-role messages: why the harness injected them — the
    /// `ContextSource` label (`/name`, `instructions`, `runtime context`,
    /// `tool notice`…). Lets the FE present each kind without re-parsing
    /// prose.
    #[serde(default)]
    pub context_source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LlmState {
    Disconnected,
    Connecting,
    Ready { model: String, context_window: u32 },
    Error { message: String },
}

/// What the prompt is made of, per [`Event::TokenUsage`]. All counts are
/// approximate (heuristic or usage-anchored, never both mixed within one
/// field).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenBreakdown {
    /// The system prompt (composed sections).
    pub system: u32,
    /// The native `tools` array (0 in text-protocol mode).
    pub tools: u32,
    /// The derived conversation history.
    pub history: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TicketKind {
    BeFix,
    FeBug,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Heartbeat { uptime_secs: u64, counter: i64 },
    Progress  { request_id: u64, percent: u8 },
    LogLine   { level: String, message: String },

    // LLM connection state.
    LlmStateChanged { state: LlmState },

    // Streaming turn lifecycle.
    TurnStarted   { session_id: u64, turn_id: u64 },
    AssistantDelta {
        session_id: u64,
        turn_id: u64,
        content: String,
        reasoning: String,
    },
    TurnFinished {
        session_id: u64,
        turn_id: u64,
        finish_reason: String,
    },

    /// Emitted after the auto-title agent renames a session. Lets the FE
    /// sidebar update without polling `ListSessions`.
    SessionTitleChanged { session_id: u64, title: String },

    // Live token meter (fixes the stale-meter bug from Python).
    //
    // `used` is prompt + generated-so-far, `limit` is the model's full context
    // window, and `budget` is the slice of that window available to the prompt
    // (window minus the reply reserve) — the denominator auto-compaction
    // measures against, and the one the status-bar percentage uses.
    // `breakdown` says what the prompt is made of (system / tools / history)
    // when the backend could compute it.
    TokenUsage {
        session_id: u64,
        used: u32,
        limit: u32,
        budget: u32,
        breakdown: Option<TokenBreakdown>,
    },

    /// Auto-compaction started: the assembled prompt crossed
    /// [`COMPACT_TRIGGER_PCT`] of the prompt budget and the older half of the
    /// history is being summarised.
    ContextCompacting { session_id: u64 },

    /// Auto-compaction finished. On success `folded` messages were replaced by
    /// a single summary message and the history shrank from `before_tokens` to
    /// `after_tokens` (approximate counts, history only — the system preamble
    /// is not included). On failure (`ok == false`) history was left untouched
    /// and the trimmer takes over as the backstop.
    ContextCompacted {
        session_id:    u64,
        ok:            bool,
        folded:        u32,
        before_tokens: u32,
        after_tokens:  u32,
        /// The summary the compactor wrote; empty when `ok == false`.
        summary:       String,
        /// Oversized older tool results replaced by head/tail windows during
        /// this pass (the pruner). `folded == 0` with `pruned > 0` means the
        /// pruner alone cleared the pressure and no summary was written.
        pruned:        u32,
    },

    // Tool-call / sub-agent UI events. Nested calls inherit parent_id.
    //
    // `args_preview` carries the natural-language rendering of the call as
    // emitted by the model (e.g. `read-file 'skills/run-cli.md'`), and
    // `expectation` carries the text after the `>` separator — what the main
    // agent wants the sub-agent to focus its summary on.
    ToolCallStarted {
        id: u64,
        parent_id: Option<u64>,
        depth: u8,
        name: String,
        args_preview: String,
        expectation: String,
    },
    ToolCallFinished {
        id: u64,
        ok: bool,
        summary: String,
    },

    // Idealist daemon signals.
    IdealistStatus {
        activity: String,
        severity: Severity,
        last_ticket: Option<String>,
    },
    IdealistTicketWritten {
        path: String,
        kind: TicketKind,
    },

    // Wave 3 control plane (guide §10–§11).
    /// A pipeline policy returned `Ask`: the FE shows an Allow-once / Deny
    /// strip and answers with `ResolveApproval`. Times out to deny.
    ApprovalRequested {
        id: u64,
        session_id: u64,
        skill: String,
        args_preview: String,
        reason: String,
    },
    /// The `ask-user` skill (or plan review) needs a human answer. The FE
    /// shows a modal and answers with `AnswerQuestion`. Times out to none.
    QuestionAsked {
        id: u64,
        session_id: u64,
        question: String,
        options: Vec<String>,
    },
    /// The durable `todo-write` list changed. The FE renders a checklist;
    /// it clears on the next `TurnStarted`.
    TodosChanged {
        session_id: u64,
        items: Vec<TodoItem>,
    },
    /// Plan mode flipped (via `/plan`, `SetPlanMode`, or `exit-plan-mode`).
    PlanModeChanged {
        session_id: u64,
        active: bool,
    },
    /// Permission mode flipped (via `/permission` or `SetPermissionMode`).
    PermissionModeChanged {
        session_id: u64,
        mode: PermissionMode,
    },
    /// The session's inbox changed: `queued` user messages are waiting to
    /// run as their own turns once the current one ends. `accepted` says
    /// what the arriving item became — `"queued"`, `"steered"` or
    /// `"injected"` — so the FE can label the message it just optimistically
    /// rendered instead of leaving it looking stalled.
    InboxChanged {
        session_id: u64,
        queued: u32,
        accepted: String,
    },
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("encode: {0}")]
    Encode(#[from] bincode::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl Frame {
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        Ok(bincode::serialize(self)?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        Ok(bincode::deserialize(bytes)?)
    }

    pub fn event(event: Event) -> Self {
        Self { id: 0, payload: Payload::Event(event) }
    }

    pub fn response(id: u64, response: Response) -> Self {
        Self { id, payload: Payload::Response(response) }
    }

    pub fn request(id: u64, request: Request) -> Self {
        Self { id, payload: Payload::Request(request) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_request() {
        let f = Frame::request(7, Request::IncrementCounter { by: 3 });
        let bytes = f.encode().unwrap();
        let back = Frame::decode(&bytes).unwrap();
        assert_eq!(back.id, 7);
        matches!(back.payload, Payload::Request(Request::IncrementCounter { by: 3 }));
    }

    #[test]
    fn roundtrip_event() {
        let f = Frame::event(Event::Heartbeat { uptime_secs: 12, counter: 3 });
        let bytes = f.encode().unwrap();
        let back = Frame::decode(&bytes).unwrap();
        assert_eq!(back.id, 0);
        matches!(back.payload, Payload::Event(Event::Heartbeat { .. }));
    }

    #[test]
    fn roundtrip_token_usage() {
        let f = Frame::event(Event::TokenUsage {
            session_id: 1,
            used: 1234,
            limit: 24000,
            budget: 19392,
            breakdown: Some(TokenBreakdown { system: 100, tools: 50, history: 1084 }),
        });
        let bytes = f.encode().unwrap();
        let back = Frame::decode(&bytes).unwrap();
        matches!(back.payload, Payload::Event(Event::TokenUsage { .. }));
    }

    #[test]
    fn compact_policy_defaults_match_dsh() {
        let p = CompactPolicy::default();
        assert_eq!(p.threshold_pct, 80);
        assert_eq!(p.retain_pct, 16);
        assert_eq!(p.max_tokens, 8192);
        assert_eq!(p.retries, 1);
        // Old provider TOMLs / FE builds without the field must still work.
        let opts: LlmOptions = serde_json::from_str(
            r#"{"temperature":0.2,"max_tokens":null,"context_window":null,"native_tools":false,"thinking":true}"#,
        ).unwrap();
        assert_eq!(opts.compact, CompactPolicy::default());
    }

    #[test]
    fn roundtrip_tool_call() {
        let f = Frame::event(Event::ToolCallStarted {
            id: 1,
            parent_id: None,
            depth: 0,
            name: "cmd".into(),
            args_preview: "cmd 'echo hi'".into(),
            expectation: "confirm it ran".into(),
        });
        let bytes = f.encode().unwrap();
        let back = Frame::decode(&bytes).unwrap();
        matches!(back.payload, Payload::Event(Event::ToolCallStarted { .. }));
    }

    #[test]
    fn roundtrip_catalog() {
        let f = Frame::response(
            11,
            Response::Catalog {
                entries: vec![CatalogEntry {
                    kind:        CatalogKind::Skill,
                    name:        "read-file".into(),
                    description: "read a UTF-8 file".into(),
                    args:        vec!["path".into()],
                    source:      Some("skills/read-file.md".into()),
                }],
            },
        );
        let bytes = f.encode().unwrap();
        let back = Frame::decode(&bytes).unwrap();
        let Payload::Response(Response::Catalog { entries }) = back.payload else {
            panic!("expected Catalog response");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, CatalogKind::Skill);
        assert_eq!(entries[0].args, vec!["path".to_string()]);
    }

    #[test]
    fn permission_mode_labels_parse_back() {
        for m in [
            PermissionMode::ReadOnly,
            PermissionMode::WorkspaceWrite,
            PermissionMode::DangerFullAccess,
        ] {
            assert_eq!(PermissionMode::parse(m.label()), Some(m), "{}", m.label());
        }
        assert_eq!(PermissionMode::parse("ro"), Some(PermissionMode::ReadOnly));
        assert_eq!(PermissionMode::parse("DANGER"), Some(PermissionMode::DangerFullAccess));
        assert_eq!(PermissionMode::default(), PermissionMode::WorkspaceWrite);
        assert_eq!(PermissionMode::parse("nuke"), None);
    }

    #[test]
    fn todo_status_parses_aliases() {
        assert_eq!(TodoStatus::parse("pending"), Some(TodoStatus::Pending));
        assert_eq!(TodoStatus::parse("in-progress"), Some(TodoStatus::InProgress));
        assert_eq!(TodoStatus::parse("done"), Some(TodoStatus::Completed));
        assert_eq!(TodoStatus::parse("later"), None);
    }

    #[test]
    fn roundtrip_wave3_events() {
        let events = vec![
            Event::ApprovalRequested {
                id: 1, session_id: 2, skill: "run-cli".into(),
                args_preview: "run-cli 'rm -rf x'".into(), reason: "destructive".into(),
            },
            Event::QuestionAsked {
                id: 3, session_id: 2, question: "which?".into(),
                options: vec!["a".into()],
            },
            Event::TodosChanged {
                session_id: 2,
                items: vec![TodoItem { content: "x".into(), status: TodoStatus::InProgress }],
            },
            Event::PlanModeChanged { session_id: 2, active: true },
            Event::PermissionModeChanged { session_id: 2, mode: PermissionMode::ReadOnly },
        ];
        for ev in events {
            let back = Frame::decode(&Frame::event(ev).encode().unwrap()).unwrap();
            assert_eq!(back.id, 0);
            assert!(matches!(back.payload, Payload::Event(_)));
        }
        let req = Request::RunCommand { session_id: 1, name: "compact".into(), input: String::new() };
        let back = Frame::decode(&Frame::request(9, req).encode().unwrap()).unwrap();
        assert!(matches!(back.payload, Payload::Request(Request::RunCommand { .. })));
    }

    #[test]
    fn roundtrip_llm_state() {
        let f = Frame::event(Event::LlmStateChanged {
            state: LlmState::Ready { model: "qwen".into(), context_window: 24000 },
        });
        let bytes = f.encode().unwrap();
        let back = Frame::decode(&bytes).unwrap();
        matches!(back.payload, Payload::Event(Event::LlmStateChanged { .. }));
    }
}
