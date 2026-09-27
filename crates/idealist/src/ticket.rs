//! Improvement tickets: one file per *kind* of failure, not per failure.
//!
//! A ticket is `idealist_workspace/tickets/<id>.md` — TOML front-matter
//! between `+++` fences, then a markdown body. The id is derived from the
//! failure's [`fingerprint`], so the place an error happens can name its
//! ticket (and write it into the session log and the ledger) without
//! waiting for the daemon to write the file.
//!
//! Before this, every trigger wrote a new `Improvement-*.md`, and an agent
//! looping on one failing `read-file` buried the one useful file under
//! forty copies of itself. Now a repeat bumps `occurrences`; a repeat of a
//! ticket someone marked `resolved` reopens it as a regression.
//!
//! The store never edits source and never calls an LLM. The investigator
//! (`backend::investigate`) appends its findings through
//! [`TicketStore::append_investigation`].

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::analyzer::Analysis;
use crate::classifier::TriggerSource;
use crate::trigger_bus::{Trigger, TriggerOrigin};

/// Serialises every read-modify-write on a ticket file. The daemon and the
/// investigator both write, and a lost `occurrences` bump would under-count
/// exactly the tickets that matter most.
static WRITE: Mutex<()> = Mutex::new(());

/// Bytes of normalised message that go into the fingerprint. Long enough to
/// keep two different errors apart, short enough that a stderr tail that
/// differs on line 40 does not split one failure into many tickets.
const FINGERPRINT_INPUT: usize = 400;

/// Hex characters of the digest kept as the ticket id.
const ID_LEN: usize = 12;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketStatus {
    #[default]
    Open,
    Investigating,
    Diagnosed,
    /// The investigator ran and produced nothing usable. Retried at the
    /// next session end, like `Open`.
    InvestigationFailed,
    Resolved,
    Wontfix,
    Noise,
}

impl TicketStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TicketStatus::Open => "open",
            TicketStatus::Investigating => "investigating",
            TicketStatus::Diagnosed => "diagnosed",
            TicketStatus::InvestigationFailed => "investigation_failed",
            TicketStatus::Resolved => "resolved",
            TicketStatus::Wontfix => "wontfix",
            TicketStatus::Noise => "noise",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "open" => TicketStatus::Open,
            "investigating" => TicketStatus::Investigating,
            "diagnosed" => TicketStatus::Diagnosed,
            "investigation_failed" => TicketStatus::InvestigationFailed,
            "resolved" => TicketStatus::Resolved,
            "wontfix" => TicketStatus::Wontfix,
            "noise" => TicketStatus::Noise,
            _ => return None,
        })
    }

    /// Waiting for the investigator.
    pub fn investigable(self) -> bool {
        matches!(self, TicketStatus::Open | TicketStatus::InvestigationFailed)
    }
}

/// What the investigator concluded, kept in the front-matter so a listing
/// does not have to parse markdown.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Diagnosis {
    pub category:   String,
    pub confidence: String,
    pub at:         String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lesson:     Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TicketMeta {
    pub id:           String,
    pub fingerprint:  String,
    pub status:       TicketStatus,
    pub origin:       TriggerOrigin,
    /// `frontend` / `backend` / `tool` / `llm` / `unknown` — the classifier.
    pub source:       String,
    pub kind:         String,
    pub module:       String,
    pub category:     String,
    pub severity:     String,
    pub occurrences:  u32,
    #[serde(default)]
    pub regressions:  u32,
    pub first_seen:   String,
    pub last_seen:    String,
    /// Every session the failure happened in, oldest first.
    #[serde(default)]
    pub sessions:     Vec<u64>,
    /// The newest message, one line, capped — the first one is in the body.
    #[serde(default)]
    pub last_message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnosis:    Option<Diagnosis>,
    /// The session opened to fix this ticket (`StartFixSession`), so a
    /// second request reopens it instead of starting another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_session:  Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ticket {
    pub meta: TicketMeta,
    pub body: String,
}

impl Ticket {
    pub fn render(&self) -> Result<String> {
        let front = toml::to_string(&self.meta).context("serialise ticket front-matter")?;
        Ok(format!("+++\n{front}+++\n\n{}", self.body.trim_start()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let text = text.replace("\r\n", "\n");
        let rest = text
            .strip_prefix("+++\n")
            .ok_or_else(|| anyhow!("ticket has no `+++` front-matter"))?;
        let end = rest
            .find("\n+++")
            .ok_or_else(|| anyhow!("ticket front-matter is not closed"))?;
        let meta: TicketMeta = toml::from_str(&rest[..end]).context("parse ticket front-matter")?;
        let body = rest[end + 4..].trim_start_matches('\n').to_string();
        Ok(Self { meta, body })
    }
}

/// What [`TicketStore::upsert`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upsert {
    pub id:          String,
    pub path:        PathBuf,
    pub created:     bool,
    /// A `resolved` ticket fired again.
    pub reopened:    bool,
    pub occurrences: u32,
}

/// Collapse the parts of a message that change between two occurrences of
/// the same failure: quoted values, paths, hex ids, numbers, whitespace.
/// `timeout after 30s` and `timeout after 31s` are one failure.
pub fn normalize(message: &str) -> String {
    static QUOTED: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"'[^'\n]*'|"[^"\n]*"|`[^`\n]*`"#).unwrap());
    static PATH: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?:[A-Za-z]:)?[\w.~-]*[/\\][\w./\\~-]*").unwrap());
    static HEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(?:0x)?[0-9a-f]{6,}\b").unwrap());
    static NUM: Lazy<Regex> = Lazy::new(|| Regex::new(r"\d+").unwrap());
    static WS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());

    let lower = message.to_lowercase();
    let s = QUOTED.replace_all(&lower, "<q>");
    let s = PATH.replace_all(&s, "<path>");
    let s = HEX.replace_all(&s, "<hex>");
    let s = NUM.replace_all(&s, "<n>");
    let s = WS.replace_all(&s, " ");
    let s = s.trim();
    let mut end = s.len().min(FINGERPRINT_INPUT);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Stable identity of a failure: origin, module and the normalised message.
pub fn fingerprint(t: &Trigger) -> String {
    let mut h = Sha256::new();
    h.update(t.origin.as_str().as_bytes());
    h.update([0]);
    h.update(t.module.as_bytes());
    h.update([0]);
    h.update(normalize(&t.message).as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The ticket id a trigger files under — the fingerprint's head.
pub fn ticket_id(t: &Trigger) -> String {
    fingerprint(t)[..ID_LEN].to_string()
}

/// The skill a `agents::tool::<skill>` module names, if it is one.
pub fn tool_skill(module: &str) -> Option<&str> {
    module.strip_prefix("agents::tool::")
}

pub fn source_name(src: TriggerSource) -> &'static str {
    match src {
        TriggerSource::Frontend => "frontend",
        TriggerSource::Backend => "backend",
        TriggerSource::SubAgentTool => "tool",
        TriggerSource::Llm => "llm",
        TriggerSource::Unknown => "unknown",
    }
}

fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn one_line(s: &str, cap: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.len() <= cap {
        return flat;
    }
    let mut end = cap;
    while !flat.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &flat[..end])
}

/// The markdown body of a new ticket. The heuristic analysis stays — it is
/// what the investigator starts from, and a skill-swap hint is often the
/// whole answer.
fn initial_body(t: &Trigger, a: &Analysis, src: TriggerSource) -> String {
    let title = one_line(&a.summary, 100);
    let mut out = format!(
        "# {title}\n\n\
         **Module:** `{}` · **Origin:** `{}` · **Trigger kind:** `{}`\n",
        t.module,
        t.origin.as_str(),
        t.kind,
    );
    if src == TriggerSource::Frontend {
        out.push_str(
            "\nFE issues are never auto-patched. Review the failing render path \
             before making changes.\n",
        );
    }
    out.push_str(&format!(
        "\n## Message\n\n```\n{}\n```\n\n## Traceback\n\n```\n{}\n```\n\n\
         ## Heuristic analysis\n\n{}\n",
        t.message.trim_end(),
        t.traceback.as_deref().unwrap_or("(none)").trim_end(),
        a.proposed_fix,
    ));
    if let Some(name) = a.suggested_skill.as_deref() {
        out.push_str(&format!(
            "\n**Suggested skill swap:** retry the failing operation with \
             **`{name}`** instead — see `skills/{name}.md`.\n"
        ));
    }
    out
}

/// The ticket directory and the operations on it. Cheap to construct; the
/// directory is the only state.
#[derive(Debug, Clone)]
pub struct TicketStore {
    dir: PathBuf,
}

impl TicketStore {
    /// `<idealist_workspace>/tickets`.
    pub fn open_default() -> Self {
        Self::at(sica_core::paths::idealist_workspace().join("tickets"))
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.md"))
    }

    pub fn load(&self, id: &str) -> Result<Ticket> {
        let path = self.path(id);
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        Ticket::parse(&text).with_context(|| format!("ticket {}", path.display()))
    }

    fn save(&self, t: &Ticket) -> Result<PathBuf> {
        fs::create_dir_all(&self.dir)?;
        let path = self.path(&t.meta.id);
        let tmp = path.with_extension("md.tmp");
        fs::write(&tmp, t.render()?)?;
        fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// Every ticket that parses, newest `last_seen` first. A file that does
    /// not parse is skipped rather than failing the listing — a hand edit
    /// that broke one ticket must not hide the others.
    pub fn list(&self) -> Vec<Ticket> {
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out: Vec<Ticket> = rd
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
            .filter_map(|e| fs::read_to_string(e.path()).ok())
            .filter_map(|s| Ticket::parse(&s).ok())
            .collect();
        out.sort_by(|a, b| b.meta.last_seen.cmp(&a.meta.last_seen));
        out
    }

    /// File a trigger: bump the ticket its fingerprint names, or open one.
    pub fn upsert(&self, t: &Trigger, a: &Analysis, src: TriggerSource) -> Result<Upsert> {
        let _g = WRITE.lock().unwrap_or_else(|p| p.into_inner());
        let fp = fingerprint(t);
        let id = fp[..ID_LEN].to_string();
        let stamp = now();
        let path = self.path(&id);

        if path.exists() {
            let mut ticket = self.load(&id)?;
            let m = &mut ticket.meta;
            m.occurrences = m.occurrences.saturating_add(1);
            m.last_seen = stamp.clone();
            m.last_message = one_line(&t.message, 200);
            if let Some(sid) = t.session_id {
                if !m.sessions.contains(&sid) {
                    m.sessions.push(sid);
                }
            }
            let reopened = m.status == TicketStatus::Resolved;
            if reopened {
                m.status = TicketStatus::Open;
                m.regressions = m.regressions.saturating_add(1);
                let session = t.session_id.map(|s| format!(" (session {s})")).unwrap_or_default();
                ticket.body.push_str(&format!(
                    "\n## Regression — {stamp}{session}\n\nThis ticket was marked \
                     resolved and the same failure happened again:\n\n```\n{}\n```\n",
                    t.message.trim_end()
                ));
            }
            let occurrences = ticket.meta.occurrences;
            let path = self.save(&ticket)?;
            return Ok(Upsert { id, path, created: false, reopened, occurrences });
        }

        let ticket = Ticket {
            meta: TicketMeta {
                id:           id.clone(),
                fingerprint:  fp,
                status:       TicketStatus::Open,
                origin:       t.origin,
                source:       source_name(src).into(),
                kind:         t.kind.clone(),
                module:       t.module.clone(),
                category:     a.category.clone(),
                severity:     a.severity.clone(),
                occurrences:  1,
                regressions:  0,
                first_seen:   stamp.clone(),
                last_seen:    stamp,
                sessions:     t.session_id.into_iter().collect(),
                last_message: one_line(&t.message, 200),
                diagnosis:    None,
                fix_session:  None,
            },
            body: initial_body(t, a, src),
        };
        let path = self.save(&ticket)?;
        Ok(Upsert { id, path, created: true, reopened: false, occurrences: 1 })
    }

    /// Record the session opened to fix a ticket.
    pub fn set_fix_session(&self, id: &str, session_id: u64) -> Result<Ticket> {
        let _g = WRITE.lock().unwrap_or_else(|p| p.into_inner());
        let mut t = self.load(id)?;
        t.meta.fix_session = Some(session_id);
        self.save(&t)?;
        Ok(t)
    }

    /// Set a ticket's status. `Err` when the ticket does not exist.
    pub fn set_status(&self, id: &str, status: TicketStatus) -> Result<Ticket> {
        let _g = WRITE.lock().unwrap_or_else(|p| p.into_inner());
        let mut t = self.load(id)?;
        t.meta.status = status;
        self.save(&t)?;
        Ok(t)
    }

    /// Append an investigation section and record its verdict. `diagnosis`
    /// `None` means the run produced nothing usable: the section still goes
    /// in (a raw reply is evidence too) and the status says so.
    pub fn append_investigation(
        &self,
        id: &str,
        session_id: u64,
        section: &str,
        diagnosis: Option<Diagnosis>,
    ) -> Result<Ticket> {
        let _g = WRITE.lock().unwrap_or_else(|p| p.into_inner());
        let mut t = self.load(id)?;
        // A person may have closed the ticket while the run was going; their
        // call stands, the findings are still worth keeping.
        let closed = matches!(
            t.meta.status,
            TicketStatus::Resolved | TicketStatus::Wontfix | TicketStatus::Noise
        );
        if !closed {
            t.meta.status = if diagnosis.is_some() {
                TicketStatus::Diagnosed
            } else {
                TicketStatus::InvestigationFailed
            };
        }
        if diagnosis.is_some() {
            t.meta.diagnosis = diagnosis;
        }
        t.body.push_str(&format!(
            "\n## Investigation — {} (session {session_id})\n\n{}\n",
            now(),
            section.trim()
        ));
        self.save(&t)?;
        Ok(t)
    }

    /// The `## Investigation` sections of a ticket, oldest first — what the
    /// next investigation of the same fingerprint starts from.
    pub fn investigations(t: &Ticket) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur: Option<String> = None;
        for line in t.body.lines() {
            if line.starts_with("## ") {
                if let Some(s) = cur.take() {
                    out.push(s);
                }
                if line.starts_with("## Investigation") {
                    cur = Some(String::new());
                }
            }
            if let Some(s) = cur.as_mut() {
                s.push_str(line);
                s.push('\n');
            }
        }
        out.extend(cur);
        out
    }

    /// Reset tickets left `investigating` by a backend that stopped
    /// mid-run. Called once at startup; returns how many it reset.
    pub fn reset_stale(&self) -> usize {
        let _g = WRITE.lock().unwrap_or_else(|p| p.into_inner());
        let mut n = 0;
        for mut t in self.list() {
            if t.meta.status == TicketStatus::Investigating {
                t.meta.status = TicketStatus::Open;
                if self.save(&t).is_ok() {
                    n += 1;
                }
            }
        }
        n
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::analyzer::analyze;
    use crate::classifier::classify;

    pub(crate) fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "sica-idealist-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn trig(origin: TriggerOrigin, module: &str, msg: &str, session: Option<u64>) -> Trigger {
        Trigger {
            kind: "tool_failed".into(),
            module: module.into(),
            message: msg.into(),
            origin,
            session_id: session,
            ..Default::default()
        }
    }

    fn file(store: &TicketStore, t: &Trigger) -> Upsert {
        store.upsert(t, &analyze(t), classify(t)).unwrap()
    }

    #[test]
    fn normalize_collapses_what_varies() {
        assert_eq!(normalize("timeout after 30s"), normalize("timeout after 31s"));
        assert_eq!(
            normalize("stat C:\\work\\a.rs: no such file"),
            normalize("stat /home/x/b.rs: no such file"),
        );
        assert_eq!(
            normalize("'rg' is not recognized"),
            normalize("'fd' is not recognized"),
        );
        assert_ne!(normalize("permission denied"), normalize("no such file"));
    }

    #[test]
    fn fingerprint_separates_origin_and_module() {
        let a = trig(TriggerOrigin::ToolCall, "agents::tool::read-file", "boom", None);
        let b = trig(TriggerOrigin::ToolCall, "agents::tool::glob", "boom", None);
        let c = trig(TriggerOrigin::TurnError, "agents::tool::read-file", "boom", None);
        assert_ne!(fingerprint(&a), fingerprint(&b));
        assert_ne!(fingerprint(&a), fingerprint(&c));
        assert_eq!(ticket_id(&a).len(), ID_LEN);
    }

    #[test]
    fn repeats_bump_one_ticket() {
        let store = TicketStore::at(scratch("dedupe"));
        let t1 = trig(TriggerOrigin::ToolCall, "agents::tool::run-cli", "timeout after 30s", Some(7));
        let t2 = trig(TriggerOrigin::ToolCall, "agents::tool::run-cli", "timeout after 32s", Some(8));
        let a = file(&store, &t1);
        let b = file(&store, &t2);
        assert!(a.created && !b.created);
        assert_eq!(a.id, b.id);
        assert_eq!(b.occurrences, 2);
        let t = store.load(&a.id).unwrap();
        assert_eq!(t.meta.sessions, vec![7, 8]);
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn resolved_ticket_reopens_as_regression() {
        let store = TicketStore::at(scratch("regress"));
        let t = trig(TriggerOrigin::TurnError, "backend::turn::llm", "not retryable", Some(1));
        let a = file(&store, &t);
        store.set_status(&a.id, TicketStatus::Resolved).unwrap();
        let b = file(&store, &t);
        assert!(b.reopened);
        let back = store.load(&a.id).unwrap();
        assert_eq!(back.meta.status, TicketStatus::Open);
        assert_eq!(back.meta.regressions, 1);
        assert!(back.body.contains("## Regression"));
    }

    #[test]
    fn noise_stays_closed_on_repeat() {
        let store = TicketStore::at(scratch("noise"));
        let t = trig(TriggerOrigin::ToolCall, "agents::tool::glob", "no match", None);
        let a = file(&store, &t);
        store.set_status(&a.id, TicketStatus::Noise).unwrap();
        let b = file(&store, &t);
        assert!(!b.reopened);
        assert_eq!(store.load(&a.id).unwrap().meta.status, TicketStatus::Noise);
    }

    #[test]
    fn front_matter_round_trips() {
        let store = TicketStore::at(scratch("roundtrip"));
        let t = trig(
            TriggerOrigin::ToolCall,
            "agents::tool::run-cli",
            "exit=1\n'rg' is not recognized as an internal or external command",
            Some(3),
        );
        let mut t = t;
        t.traceback = Some("host_os=windows\n".into());
        let a = file(&store, &t);
        let text = fs::read_to_string(&a.path).unwrap();
        let parsed = Ticket::parse(&text).unwrap();
        assert_eq!(parsed.render().unwrap(), text);
        assert!(parsed.body.contains("run-pwsh"), "skill swap kept: {}", parsed.body);
        // CRLF, as a Windows editor would save it, still parses.
        assert!(Ticket::parse(&text.replace('\n', "\r\n")).is_ok());
    }

    #[test]
    fn investigation_sets_status_and_is_recoverable() {
        let store = TicketStore::at(scratch("investigate"));
        let t = trig(TriggerOrigin::TurnError, "backend::turn::llm", "gave up", Some(2));
        let a = file(&store, &t);
        let diag = Diagnosis {
            category: "environment".into(),
            confidence: "high".into(),
            at: now(),
            lesson: Some("The server restarts nightly.".into()),
        };
        let back = store.append_investigation(&a.id, 2, "root cause: x", Some(diag)).unwrap();
        assert_eq!(back.meta.status, TicketStatus::Diagnosed);
        assert_eq!(TicketStore::investigations(&back).len(), 1);

        let failed = store.append_investigation(&a.id, 3, "raw reply", None).unwrap();
        assert_eq!(failed.meta.status, TicketStatus::InvestigationFailed);
        assert!(failed.meta.diagnosis.is_some(), "an earlier diagnosis is kept");
        assert_eq!(TicketStore::investigations(&failed).len(), 2);
    }

    #[test]
    fn stale_investigating_resets_to_open() {
        let store = TicketStore::at(scratch("stale"));
        let t = trig(TriggerOrigin::Panic, "backend::main", "panicked", None);
        let a = file(&store, &t);
        store.set_status(&a.id, TicketStatus::Investigating).unwrap();
        assert_eq!(store.reset_stale(), 1);
        assert_eq!(store.load(&a.id).unwrap().meta.status, TicketStatus::Open);
    }
}
