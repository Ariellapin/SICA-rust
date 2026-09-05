//! Wire protocol shared by backend and frontend.
//!
//! A single duplex stream carries every message. Each `Frame` carries a
//! correlation `id` (0 for unsolicited events) and a tagged `Payload`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROTOCOL_VERSION: u32 = 25;

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

/// How the model is offered its tools (guide §2.3, §7).
///
/// - `Text` — the text protocol: the catalogue lives in the system prompt
///   and a call is a fenced block the backend parses out of the reply.
/// - `Native` — the OpenAI `tools` / `tool_calls` API. Requires a server +
///   template with tool support (vLLM `--enable-auto-tool-choice`, etc.).
/// - `Ptc` — programmatic tool calling: the model is offered `run-code`
///   (plus the harness controls, whose bodies live in the dispatcher and so
///   cannot run inside a program) and calls every other tool from inside a
///   script. Rides the native wire, so it implies [`ToolMode::native`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolMode {
    #[default]
    Text,
    Native,
    Ptc,
}

impl ToolMode {
    /// Whether requests use the OpenAI-native `tools` array. `Ptc` does —
    /// it is a narrower catalogue on the same wire, not a third transport.
    pub fn native(self) -> bool {
        matches!(self, ToolMode::Native | ToolMode::Ptc)
    }

    /// Whether the model reaches its tools through `run-code`.
    pub fn ptc(self) -> bool {
        matches!(self, ToolMode::Ptc)
    }

    pub fn label(self) -> &'static str {
        match self {
            ToolMode::Text => "text",
            ToolMode::Native => "native",
            ToolMode::Ptc => "ptc",
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
    /// How the model is offered its tools: the text protocol, the
    /// OpenAI-native `tools` array, or programmatic tool calling.
    #[serde(default)]
    pub tool_mode: ToolMode,
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
            tool_mode: ToolMode::Text,
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
    /// Rewrite an earlier prompt and re-run the conversation from it. `seq`
    /// is the [`MessageDump::seq`] of the user message being edited (also
    /// pushed live by [`Event::UserMessageStored`]); everything from it
    /// onwards leaves the model's view — durably, by a shadowing record, so
    /// the superseded turns stay in the log — and the edited text runs as a
    /// fresh turn carrying the original message's images.
    ///
    /// Refused while a turn is running: the rewind would race the loop's own
    /// appends. Answers `Ok`, or `Error` with the reason.
    EditUserMessage { session_id: u64, seq: u64, text: String },
    InterruptTurn   { session_id: u64 },
    NewSession,
    ListSessions,
    LoadSession   { session_id: u64 },
    DeleteSession { session_id: u64 },
    /// Give a session a title of the user's own. The auto-titler only fires
    /// while the title is still the one it wrote, so this pins it.
    RenameSession { session_id: u64, title: String },
    /// Copy a session's *completed* turns into a fresh session — the same cut
    /// `subagent-fork` uses, so an in-flight turn never crosses. Answers
    /// `SessionCreated` with the new id.
    ForkSession   { session_id: u64 },
    /// Hide a session from the list. The log stays on disk; unlike
    /// `DeleteSession` nothing is lost.
    ArchiveSession { session_id: u64 },
    /// Substring search over every session's stored messages. Answers
    /// `SessionSearch`.
    SearchSessions { query: String },
    /// Page through a session's **raw event log** — the ledger the Trajectory
    /// view draws (UI guide §10). Unlike `LoadSession`, which answers with the
    /// *derived* surface the model sees, this returns every line the log
    /// holds, shadowed ones included, so the view can show what the fold threw
    /// away. Rows come back in seq order from `from_seq` (inclusive), capped
    /// at `limit` (0, or more than the backend's own cap, takes the cap).
    /// Answers `SessionEvents`.
    LoadSessionEvents { session_id: u64, from_seq: u64, limit: u32 },
    ConnectLlm    { base_url: String, model: String, api_key: Option<String>, options: LlmOptions },
    /// Ask a provider what models it serves (`GET /v1/models`). Answered
    /// `Ok` immediately and reported by `ModelsListed` — the guide sketches a
    /// `Response::Models`, but a provider that is slow to answer would then
    /// stall the whole dispatcher loop, which the codebase forbids.
    ///
    /// The guide names the field `provider`; provider configs live on the
    /// frontend side here, so the frontend sends what the backend actually
    /// needs to make the call.
    ListModels    { base_url: String, api_key: Option<String> },
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
    /// Select the session's agent preset (`agents/<name>.md`, guide §5.2):
    /// its body becomes the persona section of the system prompt and its
    /// `skills:` list restricts the registry the session dispatches
    /// against. `None` clears the selection. Refused once the session has
    /// produced a model message — a prompt prefix that changes mid-session
    /// discards the provider's cache and leaves half the transcript
    /// answering to rules no longer in force.
    SetSessionAgent { session_id: u64, name: Option<String> },
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

    /// Rewrite a message still waiting in the queue. Addressed by the
    /// [`QueuedDump::id`] the last `QueueChanged` carried; a row already
    /// claimed by the loop is gone and the edit is answered with an error,
    /// because silently editing nothing looks identical to success.
    EditQueued { session_id: u64, id: u64, text: String },
    /// Drop a message from the queue before it ever runs.
    RemoveQueued { session_id: u64, id: u64 },
    /// Promote a queued message into a steer: it leaves the queue and joins
    /// the *running* turn at its next hop instead of waiting for one of its
    /// own. Meaningless with nothing running, and answered with an error
    /// then — the FE disables the action rather than sending it.
    SteerQueued { session_id: u64, id: u64 },

    /// Fold a session's log into the finished projections a client reads
    /// (guide §3.3): the counters under the title and the turn outline the
    /// sidebar jumps with. Pure over the log, so it is safe to ask for at
    /// any time — including while a turn is running, when it answers with
    /// what is durable so far.
    SessionStats { session_id: u64 },

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
    /// Content-search hits, newest first, capped by the backend.
    SessionSearch  { hits: Vec<SessionHit> },
    SessionCreated { id: u64 },
    SessionLoaded  { session: SessionDump },
    Catalog        { entries: Vec<CatalogEntry> },
    /// Outcome text of a `RunCommand` (shown in the log panel; never model
    /// history).
    CommandResult  { text: String },
    /// One page of a session's raw event log (`LoadSessionEvents`).
    SessionEvents  {
        session_id: u64,
        events:     Vec<EventDump>,
        /// Every request envelope the rows in this page point at, sent once
        /// each rather than copied onto every row — a system prompt is
        /// kilobytes and a page is hundreds of rows. Includes the envelope
        /// in force when the page opens, whose own row may be pages back.
        envelopes:  Vec<EnvelopeDump>,
        /// How many events the log holds, so the view can say "200 of 1204"
        /// without loading the rest.
        total:      u32,
        /// Seq to ask for next, or `None` when this page reached the end.
        next_seq:   Option<u64>,
    },
    /// Session projections (`SessionStats`). `through_seq` says how much of
    /// the log the numbers cover, so a client can tell a stale answer from
    /// a current one instead of guessing.
    SessionStats {
        session_id:  u64,
        stats:       StatsDump,
        /// One row per turn, oldest first.
        outline:     Vec<TurnRowDump>,
        through_seq: u64,
    },
}

/// Which family an [`EventDump`] belongs to — the ledger's tinted kind tag
/// (UI guide §10). Deliberately coarser than `sica_core::event::EventKind`:
/// the view groups rows by what they *mean* to a reader, and a kind written
/// by a newer backend lands in `Other` rather than breaking the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventTag {
    /// Session lifecycle: created, retitled, archived.
    System,
    /// `TurnStart` / `TurnEnd` — the ledger's turn headers.
    Turn,
    User,
    Assistant,
    /// Harness-injected context (`/name` bodies, instructions, notices).
    Context,
    /// A dispatched tool call.
    Tool,
    /// Its outcome.
    ToolResult,
    /// A compaction summary or a rewind: the two shadowing mechanisms.
    Compacted,
    /// A failed LLM attempt that was retried — a *failed* request boundary.
    Retry,
    /// Prompt size after a completed hop — the request boundary.
    Usage,
    /// The system prompt / tools / options a request went out with.
    Prompt,
    /// A harness command that never made a model message.
    Command,
    Approval,
    /// A user hook ran (guide §13.1).
    Hook,
    Goal,
    Job,
    Other,
}

impl EventTag {
    /// Short uppercase label for the ledger's tag column.
    pub fn label(self) -> &'static str {
        match self {
            EventTag::System => "SYSTEM",
            EventTag::Turn => "TURN",
            EventTag::User => "USER",
            EventTag::Assistant => "ASSISTANT",
            EventTag::Context => "CONTEXT",
            EventTag::Tool => "TOOL",
            EventTag::ToolResult => "RESULT",
            EventTag::Compacted => "COMPACTED",
            EventTag::Retry => "RETRY",
            EventTag::Usage => "USAGE",
            EventTag::Prompt => "PROMPT",
            EventTag::Command => "COMMAND",
            EventTag::Approval => "APPROVAL",
            EventTag::Hook => "HOOK",
            EventTag::Goal => "GOAL",
            EventTag::Job => "JOB",
            EventTag::Other => "OTHER",
        }
    }
}

/// One line of a session's event log, flattened for the wire.
///
/// A protocol-safe mirror of `sica_core::event::SessionEvent`: the `protocol`
/// crate stays leaf-level (no dep on `sica-core`), and the fold's own types
/// never have to become bincode-safe. Every field is something a ledger row
/// or the inspector reads — nothing here is re-parsed from prose on the
/// frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventDump {
    /// The durable seq — the ledger's `#` column, and the handle the Inspect
    /// pill on a tool row jumps to.
    pub seq:        u64,
    /// Unix time in **milliseconds** (the log's own resolution).
    pub ts:         i64,
    pub tag:        EventTag,
    /// One-line ledger text.
    pub text:       String,
    /// The request side, for the inspector: a tool call's resolved arguments,
    /// a message's full body, a command's input.
    pub payload:    String,
    /// The response side: a tool result's summary, a compaction's summary, an
    /// assistant message's reasoning.
    pub result:     String,
    /// The provider's own `usage`, on the events that carry it (`TokenUsage`).
    pub tokens_in:  u32,
    pub tokens_out: u32,
    /// Outcome, where the event has one (tool results, commands, turn ends).
    pub ok:         Option<bool>,
    /// `true` when a later `Replace` or `Rewind` shadows this event: it is
    /// still in the log but the model no longer sees it. Showing these is the
    /// whole point of the view.
    pub shadowed:   bool,
    /// The span a shadowing event covers (`Replace` / `Rewind`), so the row
    /// can say what it erased.
    pub shadows:    Option<(u64, u64)>,
    /// The `ToolCall` seq this row joins to: its own seq on a call, the call
    /// it answers on a result.
    pub call_seq:   Option<u64>,
    /// Turn this row belongs to, from the enclosing `TurnStart`.
    pub turn_id:    Option<u64>,
    /// The event as the log stores it — the inspector's Raw tab.
    pub raw:        String,
    /// Seq of the [`EnvelopeDump`] in force at this row: the newest request
    /// envelope at or before it. `None` for rows written before the first
    /// request of the session — and for every row of a log written by a
    /// backend that did not record envelopes.
    pub envelope:   Option<u64>,
}

/// One request envelope, as the inspector's System Prompt / Tools / Options
/// / Schema tabs read it. Sent alongside a page of [`EventDump`]s and joined
/// by [`EventDump::envelope`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvelopeDump {
    /// Seq of the envelope's own log event — the join key.
    pub seq:         u64,
    pub ts:          i64,
    /// Hash of the three bodies together; two rows carrying the same
    /// fingerprint went out with the same prompt.
    pub fingerprint: u64,
    /// The composed system prompt, verbatim.
    pub system:      String,
    /// The `tools` array, pretty-printed. Empty in text-protocol mode, where
    /// the catalogue lives inside `system`.
    pub tools:       String,
    /// Sampling and mode options, as JSON.
    pub options:     String,
}

/// Counters folded from a session's log (`sica_core::project::SessionStats`).
///
/// Counts *events*, not the derived surface: a turn compaction later shadowed
/// still happened, and a stats line that shrank when the context was
/// compacted would be lying about the session's history.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatsDump {
    pub user_msgs:      u32,
    pub assistant_msgs: u32,
    pub tool_calls:     u32,
    /// Tool results that came back `ok: false`. Policy denials count — a
    /// refused call is a call that did not do its work.
    pub tool_failures:  u32,
    /// Failed LLM attempts that were re-run.
    pub retries:        u32,
    pub turns:          u32,
    /// First event to last, in milliseconds.
    pub wall_ms:        i64,
}

/// One turn of the outline (`sica_core::project::TurnOutline`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnRowDump {
    pub turn_id:         u64,
    /// First non-empty line of the opening user message, capped for a row.
    /// Empty for a turn no user message opened (a goal round).
    pub first_user_line: String,
    /// `human` / `goal round` / `followup`.
    pub source:          String,
    /// Hops the turn took. `0` while it is still running.
    pub hops:            u8,
    /// Empty while the turn is still running.
    pub finish_reason:   String,
    /// Seq of the `TurnStart` — what a "jump to turn" click carries into the
    /// Trajectory view.
    pub start_seq:       u64,
    pub ts_start:        i64,
    pub ts_end:          i64,
    pub tool_calls:      u32,
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
    /// Timestamp of the session's newest event (falls back to `created_at`
    /// for a session that has none yet). The sidebar orders on this and
    /// renders it as a relative bucket (§4.2).
    #[serde(default)]
    pub updated_at: i64,
}

/// One content-search hit: the session that matched plus the line it matched
/// on, so the result row can show why it is there (§4.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHit {
    pub id: u64,
    pub title: String,
    pub snippet: String,
    pub updated_at: i64,
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
    /// Selected agent preset (`agents/<name>.md`), when the session runs
    /// one — drives the FE composer chip without a second round-trip.
    #[serde(default)]
    pub agent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDump {
    /// Seq of the log event that produced this entry — the durable handle a
    /// message keeps across restarts. [`Request::EditUserMessage`] addresses
    /// a prompt by it. `0` in dumps written before the field existed.
    #[serde(default)]
    pub seq: u64,
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
    /// Identity of the call within its session, so a reloaded transcript can
    /// rebuild the same rows a live `ToolCallStarted` would. This is the seq
    /// of the durable `ToolCall` event, not the process-local tool id — the
    /// latter does not survive a restart.
    #[serde(default)]
    pub tool_call_id: Option<u64>,
    /// Parent call, when the log recorded one. Only top-level calls reach the
    /// session log today (nested `SkillContext::sub` calls are live events
    /// only), so this is `None` on reload and the rows render flat.
    #[serde(default)]
    pub tool_parent_id: Option<u64>,
    #[serde(default)]
    pub tool_depth: u8,
    /// Resolved arguments as JSON text — the expanded row's body (§3.4).
    #[serde(default)]
    pub tool_args_json: Option<String>,
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
    /// A user message reached the session log, with the seq it landed at.
    /// Emitted once per turn-opening message (never for a steer), so the
    /// frontend can offer [`Request::EditUserMessage`] on a prompt it has
    /// only ever seen live — without it, editing would work solely on a
    /// transcript reloaded from disk.
    UserMessageStored { session_id: u64, seq: u64 },
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

    /// Answer to [`Request::ListModels`]. `models` is empty when the fetch
    /// failed; `error` then says why.
    ModelsListed {
        base_url: String,
        models:   Vec<String>,
        error:    Option<String>,
    },

    /// A step-level LLM retry is under way (`chat.rs` classified the failure
    /// as retryable and is sleeping out the backoff). The durable `LlmRetry`
    /// log event is the record; this is the live push that lets the FE draw
    /// the retry chain on the turn instead of leaving it to the log panel.
    LlmRetry {
        session_id: u64,
        attempt:    u32,
        max:        u32,
        delay_ms:   u64,
        reason:     String,
    },

    /// One completed turn's accounting, emitted once at `TurnEnd`. Unlike
    /// [`Event::TokenUsage`] — which is the live per-session meter — these are
    /// the totals for this turn alone, summed over its hops, and they feed the
    /// turn tail's usage and time pills.
    TurnUsage {
        session_id:  u64,
        turn_id:     u64,
        /// Prompt tokens, summed over the turn's hops (provider `usage` when
        /// the provider sent one; 0 when it never did).
        prompt:      u32,
        /// Completion tokens, summed the same way.
        completion:  u32,
        /// Characters of reasoning the turn produced — a proxy the FE renders
        /// as a "(+reasoning)" note; providers do not break it out.
        reasoning:   u32,
        /// Wall-clock time of the whole turn, hops and tool calls included.
        duration_ms: u64,
        /// Time to the first streamed token of the turn's first hop.
        ttft_ms:     u64,
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
        /// The resolved arguments as JSON text — what the expanded row needs
        /// to render a real body (the command for a terminal block, the
        /// `old`/`new` pair for a diff, the path for a read). `args_preview`
        /// is a one-line rendering and is truncated; this is not.
        #[serde(default)]
        args_json: String,
        /// Seq of the durable `EventKind::ToolCall` this dispatch logged —
        /// the handle the Inspect pill (UI guide §3.4) jumps to in the
        /// Trajectory view, and the same identity a reloaded transcript
        /// rebuilds its rows from (`MessageDump::tool_call_id`). Without it
        /// a live row and a reloaded one would carry different ids for the
        /// same call. `0` when the call was never logged — a nested
        /// `SkillContext::sub` call, or a standalone sub-agent outside a
        /// session (teammates, evals).
        #[serde(default)]
        call_seq: u64,
    },
    ToolCallFinished {
        id: u64,
        ok: bool,
        /// The model-visible outcome: the expectation summariser's paraphrase
        /// when one ran, else the same text as `output`.
        summary: String,
        /// The tool's own output as the model received it — spill-aware (the
        /// head/tail digest when the raw text went to disk) but *before* the
        /// expectation summariser paraphrased it. This is what the expanded
        /// row shows.
        #[serde(default)]
        output: String,
        /// Wall-clock time of the whole dispatch, pipeline included.
        #[serde(default)]
        duration_ms: u64,
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
        /// The longer body under the headline: what the asker wants the
        /// person to read before choosing. Kept apart from `question` so the
        /// takeover can render the two at their own weights instead of the
        /// asker packing both into one string.
        detail: Option<String>,
        options: Vec<String>,
        /// Options are checkboxes rather than one-of: several may be picked,
        /// and the answer is the picked labels joined. Meaningless with no
        /// options.
        multi: bool,
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
    /// The session's agent preset changed (via `/agent` or
    /// `SetSessionAgent`), or was pushed on load. `None` is no preset —
    /// the default persona-less prompt.
    SessionAgentChanged {
        session_id: u64,
        name: Option<String>,
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
    /// The queued user messages themselves, in the order they will run.
    /// `InboxChanged` says *how many* are waiting; this says *what* they
    /// are, which is what the composer's queue dock renders. Carries the
    /// whole list so the frontend never reconciles deltas, and is pushed on
    /// session load as well as on every change.
    QueueChanged {
        session_id: u64,
        rows: Vec<QueuedDump>,
    },
    /// A session's background jobs changed — one started, finished or was
    /// killed. Carries the whole list so the FE never has to reconcile
    /// deltas.
    JobsChanged {
        session_id: u64,
        jobs: Vec<JobDump>,
    },
    /// A session's durable goal changed — created, edited, a round started,
    /// paused, completed or blocked. `None` means the session has no goal.
    GoalChanged {
        session_id: u64,
        goal: Option<GoalDump>,
    },
}

/// Where a session's durable objective stands (guide §12.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GoalPhase {
    /// Rounds may run.
    Active,
    /// Held by a human; rounds do not run until resumed.
    Paused,
    /// Reached. Terminal.
    Completed,
    /// Stopped on something the agent cannot resolve. Terminal until a
    /// human edits the goal.
    Blocked,
}

impl GoalPhase {
    pub fn label(&self) -> &'static str {
        match self {
            GoalPhase::Active => "active",
            GoalPhase::Paused => "paused",
            GoalPhase::Completed => "completed",
            GoalPhase::Blocked => "blocked",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "active" => Some(GoalPhase::Active),
            "paused" => Some(GoalPhase::Paused),
            "completed" => Some(GoalPhase::Completed),
            "blocked" => Some(GoalPhase::Blocked),
            _ => None,
        }
    }

    /// Terminal phases never run another round, whatever the round budget
    /// says.
    pub fn is_terminal(&self) -> bool {
        matches!(self, GoalPhase::Completed | GoalPhase::Blocked)
    }
}

/// A session's goal as the FE sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalDump {
    pub id:             u64,
    pub revision:       u32,
    pub objective:      String,
    pub phase:          GoalPhase,
    pub rounds_started: u32,
    pub max_rounds:     u32,
    pub blocker:        Option<String>,
    /// Whether the round driver will actually start rounds. Process-local
    /// and never persisted: after a backend restart an active goal comes
    /// back disarmed, so a reboot can never resume an autonomous loop the
    /// user has not asked for again.
    pub armed:          bool,
}

/// One user message waiting in a session's inbox (`Event::QueueChanged`).
///
/// `id` is stable for as long as the row waits, which is what makes
/// [`Request::EditQueued`] and its siblings addressable; it is *not* a log
/// seq, because a queued message has not been logged yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedDump {
    pub id:     u64,
    pub text:   String,
    /// How many images ride with it. The bytes stay in the backend — the
    /// dock only needs to say the row carries some, and a row that does
    /// cannot be steered (a steer is text).
    pub images: u32,
}

/// One background job as the FE sees it (`agents::jobs::JobSummary` over
/// the wire; `status` is the rendered label, since the FE only displays it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobDump {
    pub id:      String,
    pub kind:    String,
    pub command: String,
    pub status:  String,
    pub running: bool,
    pub unread:  u64,
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
            args_json: r#"{"command":"echo hi"}"#.into(),
            call_seq: 7,
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
                detail: Some("because of x".into()),
                options: vec!["a".into()], multi: true,
            },
            Event::TodosChanged {
                session_id: 2,
                items: vec![TodoItem { content: "x".into(), status: TodoStatus::InProgress }],
            },
            Event::PlanModeChanged { session_id: 2, active: true },
            Event::PermissionModeChanged { session_id: 2, mode: PermissionMode::ReadOnly },
            Event::SessionAgentChanged { session_id: 2, name: Some("reviewer".into()) },
            Event::SessionAgentChanged { session_id: 2, name: None },
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
