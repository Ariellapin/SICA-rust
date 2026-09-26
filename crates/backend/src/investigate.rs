//! The end-of-session investigator.
//!
//! While a session works, [`crate::incident`] files every failure as an
//! idealist ticket and notes it in the session's ledger. When the session
//! *ends*, this reads each open ticket together with the part of the log
//! where it happened and the source it names, and writes back a root cause,
//! a proposed fix and — when the model is what went wrong — a one-line
//! lesson.
//!
//! There is no "end of session" in the protocol, so this defines one
//! ([`EndReason`]): the session has been idle for `idle_minutes` after its
//! last turn, it was archived, someone asked (`InvestigateSession`), or the
//! backend restarted with sessions nobody investigated. Shutdown never
//! waits for an investigation; the startup sweep picks those up.
//!
//! Four rules shape it:
//!
//! - **Never compete with the person.** A local LLM serves one request at a
//!   time. The worker starts only when no turn is running anywhere, and any
//!   turn starting cancels the run in flight; its tickets go back to `open`
//!   and the session is re-queued.
//! - **Read-only.** The run gets `read-file`, `glob` and `grep` and nothing
//!   else, whatever `agents/investigator.md` lists — an investigator that
//!   could edit would be an auto-patcher nobody approved.
//! - **No recursion.** Its sub-agent has no failure sink, so its own failed
//!   calls open no tickets.
//! - **Never fails anything.** A timeout, a lost connection or a reply that
//!   is not a finding writes what there was to the ticket, marks it
//!   `investigation_failed` (retried at the next session end) and says so in
//!   a WARN line — as `verdict` does for the completion check.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use agents::runner::{self, RunSpec};
use agents::{EventSink, ToolSubAgent};
use idealist::{
    lessons, Diagnosis, IdealistConfig, Ledger, LedgerEntry, Ticket, TicketStatus, TicketStore,
    TriggerOrigin,
};
use llm::client::LlmClient;
use protocol::Event;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::chat::ChatHub;

/// The only skills an investigation may call.
pub const READ_ONLY: [&str; 3] = [
    agents::builtins::READ_FILE_NAME,
    agents::builtins::GLOB_NAME,
    agents::builtins::GREP_NAME,
];

/// A recovered failure is investigated anyway once its ticket has fired
/// this often: the model keeps making the same mistake, which is worth a
/// lesson.
const RECOVERED_PROMOTE_AT: u32 = 3;
/// Log rows shown before and after the failure.
const WINDOW_BEFORE: usize = 30;
const WINDOW_AFTER: usize = 10;
/// Characters per row, and for the whole window / ticket / prior findings.
const ROW_CAP: usize = 300;
const WINDOW_CAP: usize = 8_000;
const TICKET_CAP: usize = 5_000;
const PRIOR_CAP: usize = 3_000;
/// How long the worker waits between checks for a quiet moment.
const QUIET_POLL: Duration = Duration::from_secs(10);
/// Delay before the startup sweep, so a restart is not greeted by an
/// investigation racing the person's first message.
const SWEEP_DELAY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    Idle,
    Archived,
    Manual,
    StartupSweep,
}

impl EndReason {
    /// The `reason` a `SessionEnd` hook reads.
    fn hook_reason(self) -> &'static str {
        match self {
            EndReason::Idle => "idle",
            EndReason::Archived => "archived",
            EndReason::Manual => "requested",
            EndReason::StartupSweep => "startup",
        }
    }

    fn label(self) -> &'static str {
        match self {
            EndReason::Idle => "idle",
            EndReason::Archived => "archived",
            EndReason::Manual => "requested",
            EndReason::StartupSweep => "startup sweep",
        }
    }
}

pub struct Investigator {
    cfg:     IdealistConfig,
    /// The hub, for the LLM, the running turns, the logs, the skills, the
    /// event sink, the hooks, and creating fix sessions.
    hub:     ChatHub,
    store:   TicketStore,
    ledger:  Ledger,
    tx:      mpsc::UnboundedSender<(u64, EndReason)>,
    /// One pending idle timer per session.
    idle:    std::sync::Mutex<HashMap<u64, CancellationToken>>,
    /// Sessions whose end has been announced (`SessionEnd` hooks ran) since
    /// their last turn, so idle-then-archive announces once.
    ended:   std::sync::Mutex<HashSet<u64>>,
    /// The run in flight, so a starting turn can stop it.
    current: std::sync::Mutex<Option<CancellationToken>>,
}

static INVESTIGATOR: OnceLock<Arc<Investigator>> = OnceLock::new();

/// Start session-end tracking, and the worker. Called once from `main`,
/// inside the runtime. Tracking runs whatever `investigate` says, because
/// `SessionEnd` hooks depend on it too; `investigate = false` only stops
/// investigations from being queued. Tests and replay runs never call
/// this, and every hook below is then a no-op.
pub fn install(cfg: IdealistConfig, hub: ChatHub) {
    let store = TicketStore::open_default();
    if cfg.investigate {
        let reset = store.reset_stale();
        if reset > 0 {
            info!(reset, "investigator: tickets left `investigating` by a stopped backend reset to open");
        }
    } else {
        info!("investigator: investigations disabled by idealist.toml");
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let inv = Arc::new(Investigator {
        cfg,
        hub,
        store,
        ledger: Ledger::open_default(),
        tx,
        idle: std::sync::Mutex::new(HashMap::new()),
        ended: std::sync::Mutex::new(HashSet::new()),
        current: std::sync::Mutex::new(None),
    });
    if INVESTIGATOR.set(inv.clone()).is_err() {
        return;
    }
    tokio::spawn(worker(inv.clone(), rx));
    if inv.cfg.investigate {
        tokio::spawn(async move {
            tokio::time::sleep(SWEEP_DELAY).await;
            for id in inv.ledger.pending_sessions() {
                let _ = inv.tx.send((id, EndReason::StartupSweep));
            }
        });
    }
}

/// A turn is starting in `session_id`: it is not ended, and nothing may
/// hold the LLM while it runs.
pub fn session_active(session_id: u64) {
    let Some(inv) = INVESTIGATOR.get() else { return };
    if let Some(t) = inv.idle.lock().unwrap_or_else(|p| p.into_inner()).remove(&session_id) {
        t.cancel();
    }
    inv.ended.lock().unwrap_or_else(|p| p.into_inner()).remove(&session_id);
    if let Some(t) = inv.current.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        t.cancel();
    }
}

/// `session_id` went idle. Arms its end-of-session timer when anything
/// would happen at the end: tickets to investigate, or `SessionEnd` hooks.
pub fn session_idle(session_id: u64) {
    let Some(inv) = INVESTIGATOR.get() else { return };
    let investigate = inv.cfg.investigate && inv.ledger.load(session_id).has_pending();
    if !investigate && !inv.has_end_hooks() {
        return;
    }
    let token = CancellationToken::new();
    if let Some(old) = inv
        .idle
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(session_id, token.clone())
    {
        old.cancel();
    }
    let wait = Duration::from_secs(inv.cfg.idle_minutes * 60);
    let inv = inv.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = token.cancelled() => {}
            _ = tokio::time::sleep(wait) => {
                inv.idle.lock().unwrap_or_else(|p| p.into_inner()).remove(&session_id);
                inv.end(session_id, EndReason::Idle).await;
            }
        }
    });
}

/// The session was archived: it has ended.
pub fn session_archived(session_id: u64) {
    let Some(inv) = INVESTIGATOR.get() else { return };
    if let Some(t) = inv.idle.lock().unwrap_or_else(|p| p.into_inner()).remove(&session_id) {
        t.cancel();
    }
    let inv = inv.clone();
    tokio::spawn(async move { inv.end(session_id, EndReason::Archived).await });
}

/// `Request::InvestigateSession`. `Err` says why nothing will happen.
pub fn request(session_id: u64) -> Result<(), String> {
    let inv = INVESTIGATOR.get().filter(|i| i.cfg.investigate).ok_or_else(|| {
        format!(
            "the investigator is off — set `investigate = true` in {}",
            idealist::config::path().display()
        )
    })?;
    if !inv.ledger.load(session_id).has_pending() {
        return Err(format!("session {session_id} has no uninvestigated tickets"));
    }
    inv.tx
        .send((session_id, EndReason::Manual))
        .map_err(|_| "the investigator has stopped".to_string())
}

/// `idealist.toml` as loaded at startup. Separate from the worker: the
/// lessons switch applies even with investigation turned off.
static CONFIG: OnceLock<IdealistConfig> = OnceLock::new();

pub fn configure(cfg: IdealistConfig) {
    let _ = CONFIG.set(cfg);
}

/// The prompt section of lessons, when `lessons_in_prompt` is on and there
/// are any. Read per turn, so a hand edit of `lessons.md` takes effect on
/// the next message.
pub fn lessons_section() -> Option<String> {
    if !CONFIG.get().is_some_and(|c| c.lessons_in_prompt) {
        return None;
    }
    lessons::prompt_section(&lessons::path())
}

async fn worker(inv: Arc<Investigator>, mut rx: mpsc::UnboundedReceiver<(u64, EndReason)>) {
    while let Some((session_id, reason)) = rx.recv().await {
        // The same session queued twice (idle, then archived) runs once:
        // the second pull finds nothing pending.
        if !inv.ledger.load(session_id).has_pending() {
            continue;
        }
        let client = loop {
            if let Some(c) = inv.quiet_client().await {
                break c;
            }
            tokio::time::sleep(QUIET_POLL).await;
        };
        inv.run_session(&client, session_id, reason).await;
    }
}

/// Why one ticket was or was not picked.
fn pick(entries: Vec<&LedgerEntry>, store: &TicketStore, cap: usize) -> Vec<(LedgerEntry, Ticket)> {
    let mut seen = HashSet::new();
    let mut picked: Vec<(LedgerEntry, Ticket)> = entries
        .into_iter()
        .filter(|e| seen.insert(e.ticket_id.clone()))
        // Config tickets are for the record: a malformed hooks file is not
        // something reading the log will explain.
        .filter(|e| !matches!(e.origin, TriggerOrigin::Config | TriggerOrigin::Investigator))
        .filter_map(|e| store.load(&e.ticket_id).ok().map(|t| (e.clone(), t)))
        .filter(|(_, t)| t.meta.status.investigable())
        .filter(|(e, t)| !e.recovered() || t.meta.occurrences >= RECOVERED_PROMOTE_AT)
        .collect();
    let rank = |t: &Ticket| match t.meta.severity.as_str() {
        "Error" => 0,
        "Warning" => 1,
        _ => 2,
    };
    picked.sort_by(|(_, a), (_, b)| {
        rank(a).cmp(&rank(b)).then(b.meta.occurrences.cmp(&a.meta.occurrences))
    });
    picked.truncate(cap);
    picked
}

impl Investigator {
    fn has_end_hooks(&self) -> bool {
        !self.hub.hooks.for_event(crate::hooks::HookEvent::SessionEnd).is_empty()
    }

    /// `session_id` has ended: announce it to `SessionEnd` hooks (once per
    /// quiet period), then queue its investigation.
    async fn end(&self, session_id: u64, reason: EndReason) {
        let first = self.ended.lock().unwrap_or_else(|p| p.into_inner()).insert(session_id);
        if first && self.has_end_hooks() {
            crate::hooks::run_session_end(
                &self.hub.hooks,
                &self.hub.sessions,
                &self.hub.event_sink,
                session_id,
                reason.hook_reason(),
            )
            .await;
        }
        if self.cfg.investigate {
            let _ = self.tx.send((session_id, reason));
        }
    }

    /// The LLM client, when one is connected and no turn is running.
    async fn quiet_client(&self) -> Option<LlmClient> {
        if !self.hub.active_turns.lock().await.is_empty() {
            return None;
        }
        self.hub.llm.lock().await.clone()
    }

    fn emit(&self, level: &str, message: String) {
        self.hub.event_sink.emit(Event::LogLine { level: level.into(), message });
    }

    async fn run_session(&self, client: &LlmClient, session_id: u64, reason: EndReason) {
        let ledger = self.ledger.load(session_id);
        let picked = pick(ledger.pending().collect(), &self.store, self.cfg.max_per_session);
        if picked.is_empty() {
            // Everything pending was recovered, closed or config: nothing
            // to spend the model on, and nothing to come back for.
            let _ = self.ledger.mark_investigated(session_id);
            return;
        }
        self.emit(
            "INFO",
            format!(
                "idealist: session {session_id} ended ({}) — investigating {} ticket(s)",
                reason.label(),
                picked.len()
            ),
        );

        let cancel = CancellationToken::new();
        *self.current.lock().unwrap_or_else(|p| p.into_inner()) = Some(cancel.clone());
        // Publish the token, *then* look again: a turn that started after
        // the worker's quiet check but before the token existed had nothing
        // to cancel, and would otherwise share the LLM with this run.
        if !self.hub.active_turns.lock().await.is_empty() {
            cancel.cancel();
        }
        let mut interrupted = false;
        for (entry, ticket) in picked {
            if cancel.is_cancelled() {
                interrupted = true;
                break;
            }
            let id = ticket.meta.id.clone();
            let _ = self.store.set_status(&id, TicketStatus::Investigating);
            self.hub.event_sink.emit(Event::IdealistStatus {
                activity:    format!("investigating {id}"),
                severity:    protocol::Severity::Info,
                last_ticket: None,
            });
            let outcome = self.investigate(client, session_id, &entry, &ticket, &cancel).await;
            if cancel.is_cancelled() {
                // A turn started. Give the ticket back untouched; the
                // session goes to the back of the queue.
                let _ = self.store.set_status(&id, TicketStatus::Open);
                interrupted = true;
                break;
            }
            self.record(session_id, &id, outcome).await;
        }
        *self.current.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.hub.event_sink.emit(Event::IdealistStatus {
            activity:    "idle".into(),
            severity:    protocol::Severity::Info,
            last_ticket: None,
        });

        if interrupted {
            self.emit(
                "INFO",
                format!("idealist: investigation of session {session_id} paused — a turn started"),
            );
            let _ = self.tx.send((session_id, reason));
            return;
        }
        if let Err(e) = self.ledger.mark_investigated(session_id) {
            warn!(session_id, error = %e, "investigator: ledger update failed");
        }
    }

    /// Write one outcome to its ticket, the lessons file and the FE.
    async fn record(&self, session_id: u64, id: &str, outcome: Outcome) {
        let (section, diagnosis, summary) = match &outcome {
            Outcome::Found { finding, section, verified } => {
                let confidence = if *verified { finding.confidence.clone() } else { "low".into() };
                let d = Diagnosis {
                    category:   finding.category.clone(),
                    confidence,
                    at:         chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                    lesson:     finding.lesson.clone().filter(|l| !l.trim().is_empty()),
                };
                (section.clone(), Some(d), one_line(&finding.root_cause, 160))
            }
            Outcome::Failed { why, raw } => {
                let mut s = format!("**No finding** — {why}.\n");
                if !raw.trim().is_empty() {
                    s.push_str(&format!("\nThe run's last reply:\n\n```\n{}\n```\n", clip(raw, 2_000)));
                }
                (s, None, why.clone())
            }
        };
        let ok = diagnosis.is_some();
        let (category, confidence) = diagnosis
            .as_ref()
            .map(|d| (Some(d.category.clone()), Some(d.confidence.clone())))
            .unwrap_or((None, None));
        if let Some(d) = &diagnosis {
            if let Some(lesson) = d.lesson.as_deref() {
                if lessons::category_teaches(&d.category) {
                    if let Err(e) = lessons::record(&lessons::path(), id, lesson) {
                        warn!(error = %e, "investigator: lessons write failed");
                    }
                }
            }
        }
        if let Err(e) = self.store.append_investigation(id, session_id, &section, diagnosis) {
            warn!(ticket = id, error = %e, "investigator: ticket write failed");
            self.emit("WARN", format!("idealist: could not write the investigation of {id} — {e}"));
            return;
        }
        let line = match (&category, &confidence) {
            (Some(c), Some(conf)) => format!("idealist: ticket {id} diagnosed ({c}, {conf}) — {summary}"),
            _ => format!("idealist: ticket {id} not diagnosed — {summary}"),
        };
        self.emit(if ok { "INFO" } else { "WARN" }, line);
        // A sure harness bug gets a fix session when the operator opted in.
        // `confidence` is already `low` for an unverified run, so `high`
        // here means the investigator read code that backs it.
        let sure_bug = category.as_deref() == Some("harness_bug") && confidence.as_deref() == Some("high");
        self.hub.event_sink.emit(Event::IdealistInvestigated {
            ticket_id: id.to_string(),
            session_id,
            ok,
            category,
            confidence,
            summary,
        });
        if sure_bug && self.cfg.auto_fix_session {
            match start_fix_session(&self.hub, id).await {
                Ok((fix_id, draft)) => {
                    self.emit(
                        "INFO",
                        format!(
                            "idealist: opened fix session {fix_id} for ticket {id} — the fix \
                             prompt is waiting in its composer, nothing has been sent"
                        ),
                    );
                    self.hub.event_sink.emit(Event::FixSessionReady {
                        session_id: fix_id,
                        ticket_id:  id.to_string(),
                        draft,
                    });
                }
                Err(e) => self.emit("WARN", format!("idealist: no fix session for {id} — {e}")),
            }
        }
    }

    async fn investigate(
        &self,
        client: &LlmClient,
        session_id: u64,
        entry: &LedgerEntry,
        ticket: &Ticket,
        cancel: &CancellationToken,
    ) -> Outcome {
        let (persona, names) = persona_and_skills();
        let registry = Arc::new(self.hub.skills.restricted_to(&names));
        if registry.by_name.is_empty() {
            return Outcome::Failed {
                why: "none of read-file / glob / grep is registered".into(),
                raw: String::new(),
            };
        }
        let window = self.window(session_id, entry.seq.or(entry.last_seq)).await;
        let prior: Vec<String> = TicketStore::investigations(ticket);
        let task = task_text(session_id, ticket, &window, &prior);
        let spec = RunSpec {
            label:          format!("investigator {}", ticket.meta.id),
            system:         persona,
            seed:           Vec::new(),
            task,
            max_hops:       self.cfg.max_hops,
            schema:         Some(finding_schema()),
            call_seq_start: 0,
        };
        let root = sica_core::paths::workspace_root();
        let sub = ToolSubAgent::root(Arc::new(Quiet))
            .with_cancel(cancel.clone())
            .with_cwd(Some(root))
            .with_spill_label(format!("idealist-{}", ticket.meta.id));
        let mut transcript = runner::seed_transcript(&spec);
        let token = Some(cancel.clone());
        let run = runner::run_conversation(client, Some(&registry), &sub, &mut transcript, &spec, &token);
        let report = match tokio::time::timeout(Duration::from_secs(self.cfg.timeout_secs), run).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Outcome::Failed {
                    why: "every LLM call failed or the run was stopped".into(),
                    raw: String::new(),
                }
            }
            Err(_) => {
                return Outcome::Failed {
                    why: format!("timed out after {} s", self.cfg.timeout_secs),
                    raw: String::new(),
                }
            }
        };
        let verified = report.verified(true);
        let Some(value) = report.structured.clone() else {
            return Outcome::Failed {
                why: "the run never reported a structured finding".into(),
                raw: report.text,
            };
        };
        match serde_json::from_value::<Finding>(value) {
            Ok(finding) => {
                let section = render_finding(&finding, verified, &report.call_lines());
                Outcome::Found { finding, section, verified }
            }
            Err(e) => Outcome::Failed { why: format!("the finding did not parse ({e})"), raw: report.text },
        }
    }

    /// The log rows around `seq`, rendered as the Trajectory view reads.
    async fn window(&self, session_id: u64, seq: Option<u64>) -> String {
        let g = self.hub.sessions.lock().await;
        let Some(log) = g.get(&session_id) else {
            return "(the session's log is not loaded — it may have been deleted)".into();
        };
        let events = &log.events;
        if events.is_empty() {
            return "(empty log)".into();
        }
        let at = seq
            .and_then(|s| events.iter().rposition(|e| e.seq <= s))
            .unwrap_or(events.len() - 1);
        let start = at.saturating_sub(WINDOW_BEFORE);
        let end = (at + WINDOW_AFTER + 1).min(events.len());
        let mut lines: Vec<String> =
            events[start..end].iter().map(|e| crate::trajectory::line(e, ROW_CAP)).collect();
        // Over budget: drop from the front — the rows nearest the failure
        // are the ones that explain it.
        while lines.iter().map(|l| l.len() + 1).sum::<usize>() > WINDOW_CAP && lines.len() > 1 {
            lines.remove(0);
        }
        lines.join("\n")
    }
}

enum Outcome {
    Found { finding: Finding, section: String, verified: bool },
    Failed { why: String, raw: String },
}

#[derive(Debug, Clone, Deserialize)]
struct Finding {
    category:       String,
    root_cause:     String,
    confidence:     String,
    #[serde(default)]
    evidence:       Vec<String>,
    proposed_fix:   String,
    #[serde(default)]
    files_to_touch: Vec<String>,
    #[serde(default)]
    lesson:         Option<String>,
}

const CATEGORIES: [&str; 5] = ["harness_bug", "model_mistake", "environment", "config", "external"];

fn finding_schema() -> Value {
    json!({
        "type": "object",
        "required": ["category", "root_cause", "confidence", "evidence", "proposed_fix"],
        "properties": {
            "category":       { "type": "string", "enum": CATEGORIES },
            "root_cause":     { "type": "string" },
            "confidence":     { "type": "string", "enum": ["high", "medium", "low"] },
            "evidence":       { "type": "array", "items": { "type": "string" }, "minItems": 1 },
            "proposed_fix":   { "type": "string" },
            "files_to_touch": { "type": "array", "items": { "type": "string" } },
            "lesson":         {}
        }
    })
}

/// Appended to whatever `agents/investigator.md` says, so a hand edit of
/// the persona cannot lose the definitions the schema's enum relies on.
const CONTRACT: &str = "\n\n## Categories\n\
- `harness_bug` — the sica-rust code itself is wrong; name the file and line.\n\
- `model_mistake` — the model called a tool wrongly (bad path, wrong skill, \
  wrong arguments) and the harness behaved correctly.\n\
- `environment` — the machine: a missing binary, a shell difference, \
  permissions, a server that was down.\n\
- `config` — a settings file, preset, hook or provider TOML is wrong.\n\
- `external` — outside this machine: the LLM server or a remote service.\n\n\
Every evidence item names a file:line you read or a log `#seq` shown to you. \
Write `lesson` only for `model_mistake` or `environment`: one sentence the \
model should know next time (\"On this machine use run-pwsh, not run-cli, \
for rg\"). Otherwise leave it null.";

/// The persona from `agents/investigator.md` (seeded on first run), and
/// the skills it may use — never more than [`READ_ONLY`].
fn persona_and_skills() -> (String, Vec<&'static str>) {
    let root = sica_core::paths::workspace_root();
    let cwd = root.display().to_string();
    match agents::preset::load(&sica_core::paths::agents_dir(), "investigator") {
        Ok(p) => {
            let names: Vec<&'static str> = READ_ONLY
                .iter()
                .copied()
                .filter(|n| p.skills.is_empty() || p.skills.iter().any(|s| s == n))
                .collect();
            (format!("{}{CONTRACT}", p.persona.replace("{{cwd}}", &cwd)), names)
        }
        Err(e) => {
            warn!(error = %e, "investigator: preset missing, using the built-in persona");
            let persona = agents::preset::INVESTIGATOR_PERSONA.replace("{{cwd}}", &cwd);
            (format!("{persona}{CONTRACT}"), READ_ONLY.to_vec())
        }
    }
}

/// Where the source behind a module path probably is.
fn source_hint(module: &str) -> String {
    if let Some(skill) = idealist::ticket::tool_skill(module) {
        return format!(
            "`skills/{skill}.md` (the tool's contract) and `crates/agents/src/builtins.rs` \
             (built-in tool bodies); `crates/agents/src/subagent.rs` runs every tool call"
        );
    }
    if let Some(file) = module.strip_prefix("backend::panic::") {
        return format!("`{file}` (where it panicked)");
    }
    if module.starts_with("backend::turn::") {
        return "`crates/backend/src/chat.rs` (the turn loop) and `docs/agent-loop.md`".into();
    }
    if module.starts_with("backend::invariants::") {
        return "`crates/backend/src/invariants.rs`".into();
    }
    if module == "agents::preset" {
        return "`crates/agents/src/preset.rs` and the files under `agents/`".into();
    }
    let mut parts = module.split("::");
    match (parts.next(), parts.next()) {
        (Some(krate), Some(m)) => format!("`crates/{}/src/{m}.rs`", krate.replace('_', "-")),
        (Some(krate), None) => format!("`crates/{}/src/`", krate.replace('_', "-")),
        _ => "the workspace".into(),
    }
}

fn task_text(session_id: u64, ticket: &Ticket, window: &str, prior: &[String]) -> String {
    let rendered = ticket.render().unwrap_or_else(|_| ticket.body.clone());
    let mut prior_text = prior.join("\n");
    if prior_text.trim().is_empty() {
        prior_text = "None — this is the first investigation of this failure.".into();
    }
    format!(
        "Investigate improvement ticket `{id}`, raised in session {session_id}. It has fired \
         {occ} time(s) across {n} session(s).\n\n\
         ## The ticket\n\n{ticket}\n\n\
         ## Session log around the failure\n\n\
         Rows are `#seq TAG text`; `[FAILED]` marks a failed row.\n\n```\n{window}\n```\n\n\
         ## Earlier investigations of this failure\n\n{prior}\n\n\
         ## Where to start\n\nThe failing module is `{module}`; start with {hint}.\n\n\
         Read the code before concluding. When you have a root cause you can back \
         with evidence, report it.",
        id = ticket.meta.id,
        occ = ticket.meta.occurrences,
        n = ticket.meta.sessions.len().max(1),
        ticket = clip(&rendered, TICKET_CAP),
        prior = clip(&prior_text, PRIOR_CAP),
        module = ticket.meta.module,
        hint = source_hint(&ticket.meta.module),
    )
}

/// Open the session that fixes ticket `ticket_id`, or find the one already
/// opened for it, and build the prompt a person reviews before sending.
///
/// The session works in the sica-rust checkout (`workspace_root`), not the
/// user's working directory: a harness bug is in this code, whatever folder
/// the user's own sessions are pointed at. Nothing is sent from here — the
/// FE puts the draft in the composer.
pub async fn start_fix_session(hub: &ChatHub, ticket_id: &str) -> Result<(u64, String), String> {
    let store = TicketStore::open_default();
    if !store.path(ticket_id).is_file() {
        return Err(format!("no ticket `{ticket_id}` in {}", store.dir().display()));
    }
    let ticket = store.load(ticket_id).map_err(|e| format!("ticket {ticket_id}: {e}"))?;
    let draft = fix_prompt(&ticket, &store.path(ticket_id));
    if let Some(existing) = ticket.meta.fix_session {
        if hub.sessions.lock().await.contains_key(&existing) {
            return Ok((existing, draft));
        }
    }
    let root = sica_core::paths::workspace_root();
    let id = hub.create_session_in(root, None).await;
    let title = format!("Fix {ticket_id}: {}", ticket.meta.module);
    hub.rename_session(id, &title).await;
    store
        .set_fix_session(ticket_id, id)
        .map_err(|e| format!("session {id} opened but the ticket could not record it: {e}"))?;
    Ok((id, draft))
}

/// The fix session's opening message. Built from the ticket so it stands on
/// its own; the investigator's latest section rides along when there is one.
fn fix_prompt(ticket: &Ticket, path: &std::path::Path) -> String {
    let m = &ticket.meta;
    let diagnosed = match &m.diagnosis {
        Some(d) => format!(
            "The end-of-session investigator diagnosed it as `{}` with `{}` confidence.",
            d.category, d.confidence
        ),
        None => "It has not been diagnosed yet — start by finding the root cause.".into(),
    };
    let latest = TicketStore::investigations(ticket)
        .pop()
        .map(|s| format!("\n\nThe latest investigation:\n\n{}", clip(s.trim(), 3_000)))
        .unwrap_or_default();
    let krate = crate_of(&m.module);
    format!(
        "Fix idealist ticket `{id}` (`{module}`, {occ} occurrence(s)). {diagnosed} \
         The ticket is at {path}.{latest}\n\n\
         1. Read the ticket and check the diagnosis against the code before changing anything.\n\
         2. Make the smallest change that removes the root cause, and add or adjust a unit test \
         that fails without it.\n\
         3. Run the tests: `.\\run.ps1 test -p {krate}`.\n\
         4. Summarise what you changed and why. The ticket stays open until a person marks it \
         resolved.",
        id = m.id,
        module = m.module,
        occ = m.occurrences,
        path = path.display(),
    )
}

/// The crate a module path lives in, for the test command.
fn crate_of(module: &str) -> String {
    if module.starts_with("agents::tool::") {
        return "agents".into();
    }
    if let Some(file) = module.strip_prefix("backend::panic::") {
        if let Some(rest) = file.strip_prefix("crates/") {
            if let Some(k) = rest.split('/').next() {
                return k.to_string();
            }
        }
        return "backend".into();
    }
    module.split("::").next().filter(|k| !k.is_empty()).unwrap_or("backend").replace('_', "-")
}

fn render_finding(f: &Finding, verified: bool, calls: &[String]) -> String {
    let mut out = String::new();
    if !verified {
        out.push_str(
            "**UNVERIFIED — the run made no successful tool call, so nothing below was \
             checked against the code. Confidence lowered to `low`.**\n\n",
        );
    }
    out.push_str(&format!(
        "**Category:** `{}` · **Confidence:** `{}`\n\n### Root cause\n\n{}\n\n### Evidence\n\n",
        f.category,
        if verified { f.confidence.as_str() } else { "low" },
        f.root_cause.trim()
    ));
    for e in &f.evidence {
        out.push_str(&format!("- {}\n", e.trim()));
    }
    out.push_str(&format!("\n### Proposed fix\n\n{}\n", f.proposed_fix.trim()));
    if !f.files_to_touch.is_empty() {
        out.push_str("\n### Files to touch\n\n");
        for p in &f.files_to_touch {
            out.push_str(&format!("- `{}`\n", p.trim()));
        }
    }
    if let Some(l) = f.lesson.as_deref().filter(|l| !l.trim().is_empty()) {
        out.push_str(&format!("\n### Lesson\n\n{}\n", l.trim()));
    }
    if !calls.is_empty() {
        out.push_str(&format!("\n_Evidence trail: {}_\n", calls.join(", ")));
    }
    out
}

fn clip(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…[{} more bytes]", &s[..end], s.len() - end)
}

fn one_line(s: &str, cap: usize) -> String {
    clip(&s.split_whitespace().collect::<Vec<_>>().join(" "), cap).replace('\n', " ")
}

/// The investigation's tool calls are its own business: they must not show
/// up as chips in whichever chat the FE has open.
struct Quiet;
impl EventSink for Quiet {
    fn emit(&self, _ev: Event) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use idealist::ticket::TicketMeta;

    fn ticket(id: &str, severity: &str, occurrences: u32, status: TicketStatus) -> Ticket {
        Ticket {
            meta: TicketMeta {
                id: id.into(),
                fingerprint: id.into(),
                status,
                origin: TriggerOrigin::TurnError,
                source: "backend".into(),
                kind: "turn_error".into(),
                module: "backend::turn::llm_request".into(),
                category: "Logic".into(),
                severity: severity.into(),
                occurrences,
                regressions: 0,
                first_seen: "2026-09-26T00:00:00Z".into(),
                last_seen: "2026-09-26T00:00:00Z".into(),
                sessions: vec![1],
                last_message: "x".into(),
                diagnosis: None,
                fix_session: None,
            },
            body: "# x\n".into(),
        }
    }

    #[test]
    fn schema_accepts_a_good_finding_and_rejects_a_bad_category() {
        let good = json!({
            "category": "environment", "root_cause": "rg missing", "confidence": "high",
            "evidence": ["#12 TOOL_RESULT"], "proposed_fix": "use run-pwsh", "lesson": null
        });
        assert!(runner::validate(&good, &finding_schema(), "$").is_empty());
        let f: Finding = serde_json::from_value(good).unwrap();
        assert!(f.lesson.is_none());
        let bad = json!({
            "category": "cosmic_rays", "root_cause": "?", "confidence": "high",
            "evidence": [], "proposed_fix": "?"
        });
        assert_eq!(runner::validate(&bad, &finding_schema(), "$").len(), 2);
    }

    #[test]
    fn source_hints_point_at_the_right_files() {
        assert!(source_hint("agents::tool::run-cli").contains("skills/run-cli.md"));
        assert!(source_hint("backend::turn::prompt").contains("chat.rs"));
        assert!(source_hint("backend::panic::crates/agents/src/turn.rs").contains("crates/agents/src/turn.rs"));
        assert_eq!(source_hint("sica_core::event"), "`crates/sica-core/src/event.rs`");
    }

    #[test]
    fn fix_prompt_is_self_contained() {
        let mut t = ticket("abc123", "Error", 3, TicketStatus::Diagnosed);
        t.meta.module = "agents::tool::run-cli".into();
        t.meta.diagnosis = Some(Diagnosis {
            category: "harness_bug".into(),
            confidence: "high".into(),
            at: "2026-09-26T00:00:00Z".into(),
            lesson: None,
        });
        t.body.push_str("\n## Investigation — x (session 1)\n\nroot cause: quoting\n");
        let p = fix_prompt(&t, std::path::Path::new("idealist_workspace/tickets/abc123.md"));
        assert!(p.contains("`abc123`") && p.contains("harness_bug") && p.contains("root cause: quoting"));
        assert!(p.contains("run.ps1 test -p agents"), "{p}");
        assert!(p.contains("tickets/abc123.md"));
    }

    #[test]
    fn crate_of_maps_modules_to_crates() {
        assert_eq!(crate_of("agents::tool::glob"), "agents");
        assert_eq!(crate_of("backend::turn::prompt"), "backend");
        assert_eq!(crate_of("sica_core::event"), "sica-core");
        assert_eq!(crate_of("backend::panic::crates/llm/src/client.rs"), "llm");
        assert_eq!(crate_of("backend::panic::src/x.rs"), "backend");
    }

    #[test]
    fn render_marks_unverified_runs() {
        let f = Finding {
            category: "harness_bug".into(),
            root_cause: "off by one".into(),
            confidence: "high".into(),
            evidence: vec!["crates/a.rs:1".into()],
            proposed_fix: "fix it".into(),
            files_to_touch: vec!["crates/a.rs".into()],
            lesson: None,
        };
        let v = render_finding(&f, true, &["call-1 read-file (ok)".into()]);
        assert!(v.contains("`high`") && !v.contains("UNVERIFIED"));
        let u = render_finding(&f, false, &[]);
        assert!(u.contains("UNVERIFIED") && u.contains("`low`"));
    }

    #[test]
    fn pick_skips_closed_config_and_recovered_and_ranks() {
        let dir = std::env::temp_dir().join(format!(
            "sica-investigate-pick-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let store = TicketStore::at(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let save = |t: Ticket| std::fs::write(store.path(&t.meta.id), t.render().unwrap()).unwrap();
        save(ticket("warn1", "Warning", 9, TicketStatus::Open));
        save(ticket("err1", "Error", 1, TicketStatus::Open));
        save(ticket("err5", "Error", 5, TicketStatus::InvestigationFailed));
        save(ticket("done", "Error", 5, TicketStatus::Diagnosed));
        save(ticket("rec", "Error", 1, TicketStatus::Open));
        save(ticket("cfg", "Error", 1, TicketStatus::Open));
        let entry = |id: &str, origin: TriggerOrigin, recovered: bool| LedgerEntry {
            ticket_id: id.into(),
            origin,
            skill: recovered.then(|| "read-file".to_string()),
            seq: Some(1),
            last_seq: Some(1),
            count: 1,
            recovered_turns: recovered as u32,
            unrecovered_turns: 0,
            investigated: false,
            turns: Vec::new(),
        };
        let entries = vec![
            entry("warn1", TriggerOrigin::TurnError, false),
            entry("err1", TriggerOrigin::TurnError, false),
            entry("err5", TriggerOrigin::TurnError, false),
            entry("done", TriggerOrigin::TurnError, false),
            entry("rec", TriggerOrigin::ToolCall, true),
            entry("cfg", TriggerOrigin::Config, false),
        ];
        let got: Vec<String> =
            pick(entries.iter().collect(), &store, 5).into_iter().map(|(_, t)| t.meta.id).collect();
        assert_eq!(got, vec!["err5", "err1", "warn1"]);
        let capped = pick(entries.iter().collect(), &store, 1);
        assert_eq!(capped.len(), 1);
    }
}
