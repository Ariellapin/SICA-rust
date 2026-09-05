//! On-disk persistence for chat sessions: one append-only JSONL event log per
//! session under `sica_core::paths::sessions_dir()`.
//!
//! [`SessionLog`] is the in-memory handle. Every mutation is an
//! [`EventKind`] appended through [`SessionLog::append`]; [`flush`] writes
//! only the lines appended since the last flush, so persisting a message is
//! one `O(line)` append rather than a whole-file rewrite. The model-visible
//! history is *derived* from the events ([`sica_core::event::derive_surface`])
//! — nothing is ever spliced out of the log.
//!
//! A fresh session is not written until its first event after creation
//! (`flushed == 0`), so an empty "New session" that is never used leaves no
//! file behind and vanishes on restart, as before.
//!
//! Legacy `sessions/<id>.toml` files (the pre-event-log format) are migrated
//! on load: each message becomes one `LegacyMessage` event, the `.jsonl` is
//! written in full, and the `.toml` is renamed to `.toml.bak` — never deleted.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sica_core::atomic::atomic_write;
use sica_core::event::{
    derive_surface, migrate, EventKind, SessionEvent, SurfaceEntry, SurfaceOp, SESSION_FORMAT,
};
use sica_core::message::{Message, Role};
use sica_core::paths::sessions_dir;
use sica_core::session::Session;
use tracing::{info, warn};

pub struct SessionLog {
    pub id: u64,
    pub events: Vec<SessionEvent>,
    next_seq: u64,
    /// Number of leading events already on disk.
    flushed: usize,
    /// Set when the log was migrated on load (guide S3.8). The file on disk
    /// is still the old generation; the next `flush` rewrites it whole
    /// rather than appending, because appending a v1 row to a v0 file would
    /// leave a log that is neither. Until then nothing is written, so
    /// merely *opening* an old session does not touch the disk.
    rewrite: bool,
}

impl SessionLog {
    /// A brand-new session in the process default working directory.
    /// Records `SessionCreated` in memory only; the file appears on the
    /// first flush.
    pub fn new(id: u64, title: impl Into<String>) -> Self {
        Self::new_in(id, title, None)
    }

    /// A brand-new session working in `cwd`.
    ///
    /// The directory lands in the header, which is what makes it the
    /// session's own for the rest of its life: reopening the session under
    /// a different process default no longer moves it (guide S3.9), and the
    /// workspace registry reads exactly this field to decide membership.
    pub fn new_in(id: u64, title: impl Into<String>, cwd: Option<PathBuf>) -> Self {
        let created_at = chrono::Utc::now().timestamp();
        let mut log =
            Self { id, events: Vec::new(), next_seq: 1, flushed: 0, rewrite: false };
        log.append(EventKind::SessionCreated {
            id,
            title: title.into(),
            created_at,
            format: SESSION_FORMAT,
            cwd,
        });
        log
    }

    /// Fingerprint of the newest [`EventKind::RequestEnvelope`] in the log,
    /// or `None` when none has been written yet.
    ///
    /// The hop that is about to send compares its own fingerprint with this
    /// and appends only on a difference, which is what keeps one copy of the
    /// system prompt per *distinct* prompt rather than one per request.
    pub fn latest_envelope(&self) -> Option<u64> {
        self.events.iter().rev().find_map(|e| match &e.kind {
            EventKind::RequestEnvelope { fingerprint, .. } => Some(*fingerprint),
            _ => None,
        })
    }

    /// A fork: a fresh session carrying `src`'s events up to and including
    /// `cut` (the caller cuts at the last `TurnEnd`, so an in-flight turn
    /// never crosses — the same rule `subagent-fork` follows).
    ///
    /// The copied events keep their original seqs. That is deliberate:
    /// `ToolResult.call_seq` points at the `ToolCall` that produced it, and
    /// renumbering would silently break every one of those joins. Only the
    /// source's own `SessionCreated` / `SessionTitle` lines are dropped, and
    /// neither is ever referenced by seq.
    pub fn fork(id: u64, title: impl Into<String>, src: &[SessionEvent], cut: usize) -> Self {
        let created_at = chrono::Utc::now().timestamp();
        // The fork inherits the source's working directory: a fork of a
        // session in one project that reopened in another would be a
        // silent move, which is the bug S3.9 exists to close.
        let cwd = header_cwd(src);
        let mut events = vec![SessionEvent::now(
            1,
            EventKind::SessionCreated {
                id,
                title: title.into(),
                created_at,
                format: SESSION_FORMAT,
                cwd,
            },
        )];
        events.extend(
            src[..=cut]
                .iter()
                .filter(|e| {
                    !matches!(
                        e.kind,
                        EventKind::SessionCreated { .. }
                            | EventKind::SessionTitle { .. }
                            | EventKind::SessionArchived
                    )
                })
                .cloned(),
        );
        let next_seq = events.iter().map(|e| e.seq).max().unwrap_or(0) + 1;
        Self { id, events, next_seq, flushed: 0, rewrite: false }
    }

    /// A session restored from disk — every event is already persisted.
    fn from_events(id: u64, events: Vec<SessionEvent>, rewrite: bool) -> Self {
        let next_seq = events.iter().map(|e| e.seq).max().unwrap_or(0) + 1;
        let flushed = events.len();
        Self { id, events, next_seq, flushed, rewrite }
    }

    /// Append one event and return its seq.
    pub fn append(&mut self, kind: EventKind) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.events.push(SessionEvent::now(seq, kind));
        seq
    }

    pub fn last_seq(&self) -> u64 {
        self.next_seq - 1
    }

    /// Latest `SessionTitle`, else the title `SessionCreated` carried.
    pub fn title(&self) -> String {
        for ev in self.events.iter().rev() {
            match &ev.kind {
                EventKind::SessionTitle { title } => return title.clone(),
                EventKind::SessionCreated { title, .. } => return title.clone(),
                _ => {}
            }
        }
        format!("Session {}", self.id)
    }

    /// The working directory stamped into the header, if any. `None` for a
    /// log written before per-session directories existed; the caller falls
    /// back to `paths::working_dir()`.
    pub fn cwd(&self) -> Option<PathBuf> {
        header_cwd(&self.events)
    }

    /// The log's on-disk format generation (`0` for a pre-header log).
    /// Read by the format tests; production reads it off the raw rows
    /// during load, before there is a `SessionLog` to ask.
    #[allow(dead_code)]
    pub fn format(&self) -> u16 {
        self.events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::SessionCreated { format, .. } => Some(*format),
                _ => None,
            })
            .unwrap_or(0)
    }

    pub fn created_at(&self) -> i64 {
        self.events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::SessionCreated { created_at, .. } => Some(*created_at),
                _ => None,
            })
            .unwrap_or(0)
    }

    /// Archived sessions stay on disk and stay loadable; they simply leave
    /// the list. There is no un-archive door yet, matching dsh.
    pub fn archived(&self) -> bool {
        self.events
            .iter()
            .any(|e| matches!(e.kind, EventKind::SessionArchived))
    }

    /// Timestamp of the newest event — what the sidebar orders on (§4.2).
    /// In unix **seconds**, like `created_at`; the log's own `ts` is in
    /// milliseconds. Falls back to `created_at` for a log whose events carry
    /// no usable timestamp.
    pub fn updated_at(&self) -> i64 {
        self.events
            .iter()
            .rev()
            .map(|e| e.ts / 1000)
            .find(|ts| *ts > 0)
            .unwrap_or_else(|| self.created_at())
    }

    pub fn derive_surface(&self) -> Vec<SurfaceEntry> {
        derive_surface(&self.events)
    }

    /// The derived history as plain messages. Currently only tests call
    /// this — the live loop works from [`derive_surface`] so it keeps the
    /// seqs — but it is the store's public replay API.
    #[allow(dead_code)]
    pub fn derive_messages(&self) -> Vec<Message> {
        self.derive_surface().into_iter().map(|e| e.message).collect()
    }

    /// Messages the user typed, counting migrated ones.
    pub fn user_message_count(&self) -> usize {
        self.events
            .iter()
            .filter(|e| match &e.kind {
                EventKind::UserMessage { .. } => true,
                EventKind::LegacyMessage { message, .. } => message.role == Role::User,
                _ => false,
            })
            .count()
    }

}

fn jsonl_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("{id}.jsonl"))
}

/// The `cwd` on a log's `SessionCreated` header row.
fn header_cwd(events: &[SessionEvent]) -> Option<PathBuf> {
    events.iter().find_map(|e| match &e.kind {
        EventKind::SessionCreated { cwd, .. } => cwd.clone(),
        _ => None,
    })
}

/// What a listing needs about a session without reading its body
/// (guide S3.8): the header row, plus the two facts that can only be known
/// from later rows - the newest title and whether it was archived.
/// Only `id`, `cwd` and `created_at` have a production reader today (the
/// workspace bootstrap, §3.9); the rest are what a header-only *listing*
/// needs, and are asserted against a full load so the cheap read cannot
/// drift from the expensive one.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SessionHeader {
    pub id: u64,
    pub format: u16,
    pub title: String,
    pub created_at: i64,
    pub cwd: Option<PathBuf>,
    pub updated_at: i64,
    pub archived: bool,
}

/// Append every not-yet-flushed event to the session's file.
pub fn flush(log: &mut SessionLog) -> io::Result<()> {
    flush_in(&sessions_dir(), log)
}

pub fn flush_in(dir: &Path, log: &mut SessionLog) -> io::Result<()> {
    if log.rewrite {
        return rewrite_in(dir, log);
    }
    if log.flushed >= log.events.len() {
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(jsonl_path(dir, log.id))?;
    let mut buf = String::new();
    for ev in &log.events[log.flushed..] {
        let line = serde_json::to_string(ev).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        buf.push_str(&line);
        buf.push('\n');
    }
    file.write_all(buf.as_bytes())?;
    file.flush()?;
    log.flushed = log.events.len();
    Ok(())
}

/// Republish a migrated log whole, atomically (guide S3.8, S14.6). This is
/// the moment the file stops being readable by a build older than the
/// migration, which is why it waits for an append rather than happening at
/// load: a read-only visit to an old session leaves the disk untouched.
fn rewrite_in(dir: &Path, log: &mut SessionLog) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let mut buf = String::new();
    for ev in &log.events {
        let line = serde_json::to_string(ev).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        buf.push_str(&line);
        buf.push('\n');
    }
    atomic_write(&jsonl_path(dir, log.id), buf.as_bytes())?;
    log.flushed = log.events.len();
    log.rewrite = false;
    info!(id = log.id, "session log rewritten in the current format");
    Ok(())
}

/// Load every session under the sessions directory, migrating legacy TOML
/// files first. Sorted by `created_at` ascending.
pub fn load_all() -> Vec<SessionLog> {
    load_all_in(&sessions_dir())
}

pub fn load_all_in(dir: &Path) -> Vec<SessionLog> {
    let mut out = Vec::new();
    let list = |ext: &str| -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(|s| s.to_str()) == Some(ext))
                    .collect()
            })
            .unwrap_or_default();
        paths.sort();
        paths
    };

    for path in list("toml") {
        migrate_toml(dir, &path);
    }
    // Listed after migration so freshly written logs are included.
    for path in list("jsonl") {
        let Some(id) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        match read_log(&path) {
            Ok(Some(loaded)) => {
                loaded.report(&path);
                out.push(SessionLog::from_events(id, loaded.events, loaded.migrated.is_some()));
            }
            Ok(None) => {}
            Err(e) => warn!(path = %path.display(), error = %e, "unreadable session log — skipped"),
        }
    }
    out.sort_by_key(|l| l.created_at());
    out
}

/// One log, read and brought up to the current format.
struct LoadedLog {
    events:   Vec<SessionEvent>,
    /// Generation it was migrated *from*, when it was migrated at all.
    migrated: Option<u16>,
    /// Rows this build has no variant for. They survive as
    /// `EventKind::Unknown` so a downgrade never loses data — but silent
    /// acceptance is how a version skew stays invisible, so the count is
    /// reported once per log.
    unknown:  usize,
}

impl LoadedLog {
    fn report(&self, path: &Path) {
        if let Some(from) = self.migrated {
            info!(
                path = %path.display(),
                from, to = SESSION_FORMAT,
                "session log migrated (rewritten on its next append)",
            );
        }
        if self.unknown > 0 {
            warn!(
                path = %path.display(),
                count = self.unknown,
                "session log holds event kinds this build does not know",
            );
        }
    }
}

/// Parse a JSONL file, tolerating a torn final line (a crash mid-append)
/// and skipping any unparseable line in the middle. Never fatal on
/// content — only on I/O.
///
/// Rows land as `serde_json::Value` first so the format chain (guide §3.8)
/// can reshape them before they are typed. `Ok(None)` means the log was
/// empty or refused: a log written by a *newer* backend is skipped with a
/// warning rather than read through the wrong lens, and is never rewritten.
fn read_log(path: &Path) -> io::Result<Option<LoadedLog>> {
    let text = fs::read_to_string(path)?;
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let lines: Vec<&str> = text.split('\n').collect();
    let last = lines.len().saturating_sub(1);
    for (i, line) in lines.iter().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(row) => rows.push(row),
            Err(e) if i == last => {
                warn!(path = %path.display(), error = %e, "torn tail line dropped");
            }
            Err(e) => {
                warn!(path = %path.display(), line = i + 1, error = %e, "bad event line skipped");
            }
        }
    }
    if rows.is_empty() {
        warn!(path = %path.display(), "empty session log — skipped");
        return Ok(None);
    }
    if let migrate::Plan::Future { format } = migrate::plan(&rows) {
        warn!(
            path = %path.display(), format, known = SESSION_FORMAT,
            "session log was written by a newer backend — skipped, not touched",
        );
        return Ok(None);
    }
    let migrated = migrate::apply(&mut rows);

    let mut events = Vec::with_capacity(rows.len());
    let mut unknown = 0usize;
    for (i, row) in rows.into_iter().enumerate() {
        match serde_json::from_value::<SessionEvent>(row) {
            Ok(ev) => {
                if matches!(ev.kind, EventKind::Unknown) {
                    unknown += 1;
                }
                events.push(ev);
            }
            Err(e) => {
                warn!(path = %path.display(), line = i + 1, error = %e, "bad event row skipped");
            }
        }
    }
    if events.is_empty() {
        warn!(path = %path.display(), "empty session log — skipped");
        return Ok(None);
    }
    Ok(Some(LoadedLog { events, migrated, unknown }))
}

/// List every session by its **header** — line 1 plus the few later rows
/// that can change what a listing shows (guide §3.8). Nothing here derives
/// a surface or builds a `SessionLog`: `LoadSession` is where a body is
/// read. The workspace registry (§3.9) bootstraps from exactly this.
///
/// A future-format log is skipped, the same way [`read_log`] skips it.
/// Sorted by `created_at` ascending, like [`load_all_in`].
pub fn list_headers_in(dir: &Path) -> Vec<SessionHeader> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else { return out };
    for path in rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
    {
        let Some(id) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        let Ok(text) = fs::read_to_string(&path) else {
            warn!(path = %path.display(), "unreadable session log — not listed");
            continue;
        };
        if let Some(header) = header_from(id, &text) {
            out.push(header);
        }
    }
    out.sort_by_key(|h| h.created_at);
    out
}

pub fn list_headers() -> Vec<SessionHeader> {
    list_headers_in(&sessions_dir())
}

/// Build one header from a log's text.
///
/// Only three row shapes are parsed: the header itself, any `session_title`
/// (the newest wins) and any `session_archived`. Every other line is matched
/// as a substring and skipped, so a megabyte of tool results costs a scan
/// rather than thousands of `serde_json` allocations. `updated_at` comes
/// from the last row carrying a `ts`, which in an append-only log is the
/// newest one.
fn header_from(id: u64, text: &str) -> Option<SessionHeader> {
    let mut lines = text
        .split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty());
    let first = lines.next()?;
    let head: serde_json::Value = serde_json::from_str(first).ok()?;
    let format = head.get("format").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
    if format > SESSION_FORMAT {
        return None;
    }
    let mut title = head.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let created_at = head.get("created_at").and_then(|v| v.as_i64()).unwrap_or(0);
    let cwd = head
        .get("cwd")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let mut archived = false;
    let mut updated_at = created_at;
    for line in lines {
        if line.contains(ARCHIVED_ROW) {
            archived = true;
        }
        if line.contains(TITLE_ROW) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(t) = v.get("title").and_then(|t| t.as_str()) {
                    title = t.to_string();
                }
            }
        }
        if let Some(ts) = ts_of(line) {
            updated_at = ts / 1000;
        }
    }
    if title.is_empty() {
        title = format!("Session {id}");
    }
    Some(SessionHeader { id, format, title, created_at, cwd, updated_at, archived })
}

const ARCHIVED_ROW: &str = "\"type\":\"session_archived\"";
const TITLE_ROW: &str = "\"type\":\"session_title\"";

/// `ts` off a raw row without parsing the rest of it. Every row this app
/// writes carries `"ts":<millis>` — a shape the writer controls — so the
/// cheap read is correct here, and it simply declines on anything else.
fn ts_of(line: &str) -> Option<i64> {
    let rest = line.split("\"ts\":").nth(1)?;
    let digits: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    digits.parse().ok().filter(|ts: &i64| *ts > 0)
}

/// Convert `<id>.toml` into `<id>.jsonl` (if that does not already exist)
/// and rename the original to `.toml.bak`. If both exist — an earlier run
/// wrote the log but died before the rename — the JSONL wins and only the
/// rename is retried.
fn migrate_toml(dir: &Path, toml_path: &Path) {
    let Some(id) = toml_path
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse::<u64>().ok())
    else {
        return;
    };
    let jsonl = jsonl_path(dir, id);
    if !jsonl.exists() {
        let Ok(text) = fs::read_to_string(toml_path) else { return };
        let Ok(session) = toml::from_str::<Session>(&text) else {
            warn!(path = %toml_path.display(), "unparseable legacy session — left in place");
            return;
        };
        let mut log = SessionLog {
            id,
            events: Vec::new(),
            next_seq: 1,
            flushed: 0,
            rewrite: false,
        };
        log.append(EventKind::SessionCreated {
            id,
            title: session.title.clone(),
            created_at: session.created_at,
            format: SESSION_FORMAT,
            cwd: None,
        });
        for message in session.messages {
            log.append(EventKind::LegacyMessage { surface: SurfaceOp::Append, message });
        }
        if let Err(e) = flush_in(dir, &mut log) {
            warn!(path = %toml_path.display(), error = %e, "legacy session migration failed");
            return;
        }
        info!(id, events = log.events.len(), "migrated legacy session to event log");
    }
    let bak = dir.join(format!("{id}.toml.bak"));
    if let Err(e) = fs::rename(toml_path, &bak) {
        warn!(path = %toml_path.display(), error = %e, "could not rename migrated session");
    }
}

/// Best-effort delete of every on-disk form of a session. Missing files are
/// not errors — a never-flushed session has none.
pub fn delete(id: u64) {
    delete_in(&sessions_dir(), id);
}

pub fn delete_in(dir: &Path, id: u64) {
    for name in [format!("{id}.jsonl"), format!("{id}.toml"), format!("{id}.toml.bak")] {
        let _ = fs::remove_file(dir.join(name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_latest_envelope_is_the_newest_one_written() {
        let mut log = SessionLog::new(1, "t");
        assert_eq!(log.latest_envelope(), None);
        for fingerprint in [7u64, 9] {
            log.append(EventKind::RequestEnvelope {
                fingerprint,
                system:  "sys".into(),
                tools:   String::new(),
                options: String::new(),
            });
            // Events written after it must not hide it.
            log.append(EventKind::SessionTitle { title: "x".into() });
            assert_eq!(log.latest_envelope(), Some(fingerprint));
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sica-sessions-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn user(text: &str) -> EventKind {
        EventKind::UserMessage { surface: SurfaceOp::Append, content: text.into(), images: Vec::new() }
    }

    /// A fork must keep `ToolResult.call_seq` pointing at its `ToolCall`.
    /// Renumbering the copied events would break that join silently, which
    /// is why `fork` copies seqs verbatim.
    #[test]
    fn fork_copies_completed_turns_and_keeps_call_seq_joins() {
        let mut src = SessionLog::new(3, "Original");
        src.append(EventKind::TurnStart { turn_id: 1, source: Default::default() });
        src.append(user("do it"));
        let call_seq = src.append(EventKind::ToolCall {
            name: "read-file".into(),
            args_preview: "read-file 'a.rs'".into(),
            expectation: String::new(),
            call_id: None,
            args_json: Some(r#"{"path":"a.rs"}"#.into()),
        });
        src.append(EventKind::ToolResult {
            surface: SurfaceOp::Append,
            call_seq,
            skill: "read-file".into(),
            tool_call_id: None,
            ok: true,
            summary: "1\tfn main() {}".into(),
            trusted: false,
            pruned: false,
        });
        src.append(EventKind::TurnEnd {
            turn_id: 1,
            finish_reason: "done".into(),
            hops: 1,
        });
        // Everything after the cut belongs to a turn still in flight.
        src.append(user("and this too"));
        let cut = src
            .events
            .iter()
            .rposition(|e| matches!(e.kind, EventKind::TurnEnd { .. }))
            .unwrap();

        let fork = SessionLog::fork(9, "Original (fork)", &src.events, cut);
        assert_eq!(fork.id, 9);
        assert_eq!(fork.title(), "Original (fork)");
        // Exactly one SessionCreated, and it is the fork's own.
        let created = fork
            .events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::SessionCreated { .. }))
            .count();
        assert_eq!(created, 1);
        // The in-flight message did not cross.
        assert_eq!(fork.user_message_count(), 1);
        // The join still resolves: the derived tool entry names the skill
        // from its `ToolCall`, not the fallback.
        let tool = fork
            .derive_surface()
            .into_iter()
            .find_map(|e| e.tool)
            .expect("tool entry");
        assert_eq!(tool.args_preview, "read-file 'a.rs'");
        assert_eq!(tool.args_json.as_deref(), Some(r#"{"path":"a.rs"}"#));
        // Nothing is on disk yet, so the fork flushes whole.
        assert_eq!(fork.last_seq(), fork.events.iter().map(|e| e.seq).max().unwrap());
    }

    #[test]
    fn archived_is_sticky_and_updated_at_is_seconds() {
        let mut log = SessionLog::new(4, "t");
        assert!(!log.archived());
        // `created_at` is seconds and event `ts` is milliseconds; the two
        // must come back on the same scale or the sidebar's relative time
        // reads "56y ago".
        log.append(user("hi"));
        let updated = log.updated_at();
        assert!(
            (updated - log.created_at()).abs() < 5,
            "updated_at {updated} is not on the same scale as created_at {}",
            log.created_at()
        );
        log.append(EventKind::SessionArchived);
        assert!(log.archived());
    }

    #[test]
    fn new_log_is_not_written_until_flushed_with_content() {
        let dir = temp_dir("lazy");
        let mut log = SessionLog::new(5, "Session 5");
        // Even a flush of the bare `SessionCreated` writes — that is the
        // caller's choice; the hub only flushes after the first user event.
        assert_eq!(log.events.len(), 1);
        assert!(!jsonl_path(&dir, 5).exists());
        log.append(user("hi"));
        flush_in(&dir, &mut log).unwrap();
        assert!(jsonl_path(&dir, 5).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrip_appends_incrementally() {
        let dir = temp_dir("roundtrip");
        let mut log = SessionLog::new(1, "t");
        log.append(user("a"));
        flush_in(&dir, &mut log).unwrap();
        log.append(EventKind::AssistantMessage {
            surface: SurfaceOp::Append,
            content: "b".into(),
            reasoning: None,
            tool_calls: None,
        });
        flush_in(&dir, &mut log).unwrap();
        // Idempotent when nothing new.
        flush_in(&dir, &mut log).unwrap();

        let loaded = load_all_in(&dir);
        assert_eq!(loaded.len(), 1);
        let back = &loaded[0];
        assert_eq!(back.id, 1);
        assert_eq!(back.events.len(), 3);
        assert_eq!(back.last_seq(), 3);
        assert_eq!(back.title(), "t");
        let msgs = back.derive_messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].content, "b");
        assert_eq!(fs::read_to_string(jsonl_path(&dir, 1)).unwrap().lines().count(), 3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn torn_tail_and_garbage_lines_are_skipped() {
        let dir = temp_dir("torn");
        let mut log = SessionLog::new(2, "t");
        log.append(user("a"));
        log.append(user("b"));
        flush_in(&dir, &mut log).unwrap();
        let path = jsonl_path(&dir, 2);
        let mut text = fs::read_to_string(&path).unwrap();
        // Corrupt the middle line, then append a torn partial line.
        let mut lines: Vec<String> = text.lines().map(String::from).collect();
        lines[1] = "{not json".into();
        text = lines.join("\n") + "\n{\"seq\":9,\"ts\":0,\"type\":\"user_mess";
        fs::write(&path, text).unwrap();

        let loaded = load_all_in(&dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].events.len(), 2);
        assert_eq!(loaded[0].last_seq(), 3, "next_seq derives from the max surviving seq");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrates_legacy_toml_once_and_keeps_a_backup() {
        let dir = temp_dir("migrate");
        let legacy = Session {
            id: 44,
            title: "old chat".into(),
            created_at: 1_700_000_000,
            messages: vec![
                Message::user("hi"),
                Message { reasoning: Some("r".into()), ..Message::assistant("yo") },
                Message::system(format!("{} folded", protocol::CONTEXT_SUMMARY_PREFIX)),
            ],
        };
        fs::write(dir.join("44.toml"), toml::to_string_pretty(&legacy).unwrap()).unwrap();

        let loaded = load_all_in(&dir);
        assert_eq!(loaded.len(), 1);
        let log = &loaded[0];
        assert_eq!(log.id, 44);
        assert_eq!(log.title(), "old chat");
        assert_eq!(log.created_at(), 1_700_000_000);
        assert_eq!(log.user_message_count(), 1);
        let msgs = log.derive_messages();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1].reasoning.as_deref(), Some("r"));
        assert!(msgs[2].content.starts_with(protocol::CONTEXT_SUMMARY_PREFIX));
        assert!(dir.join("44.jsonl").exists());
        assert!(dir.join("44.toml.bak").exists());
        assert!(!dir.join("44.toml").exists());

        // Second load: nothing to migrate, same result.
        let again = load_all_in(&dir);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].events.len(), 4);

        delete_in(&dir, 44);
        assert!(!dir.join("44.jsonl").exists());
        assert!(!dir.join("44.toml.bak").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn title_event_overrides_created_title() {
        let mut log = SessionLog::new(3, "Session 3");
        assert_eq!(log.title(), "Session 3");
        log.append(EventKind::SessionTitle { title: "Renamed".into() });
        assert_eq!(log.title(), "Renamed");
    }


    /// A pre-header log (guide §3.8, format 0) loads, is stamped in memory,
    /// and — crucially — the file is left alone until something is appended
    /// to it. Opening an old session read-only must not touch the disk.
    #[test]
    fn a_legacy_log_migrates_in_memory_and_is_rewritten_on_its_next_append() {
        let dir = temp_dir("format-migrate");
        let path = jsonl_path(&dir, 7);
        fs::create_dir_all(&dir).unwrap();
        let legacy = concat!(
            r#"{"seq":1,"ts":1000,"type":"session_created","id":7,"title":"old","created_at":1}"#,
            "\n",
            r#"{"seq":2,"ts":2000,"type":"user_message","surface":{"op":"append"},"content":"hi"}"#,
            "\n",
        );
        fs::write(&path, legacy).unwrap();

        let before = fs::read_to_string(&path).unwrap();
        let mut loaded = load_all_in(&dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].format(), SESSION_FORMAT, "stamped in memory");
        assert_eq!(loaded[0].title(), "old");
        assert_eq!(loaded[0].user_message_count(), 1);
        assert_eq!(fs::read_to_string(&path).unwrap(), before, "a read must not write");

        // The next append republishes the whole file in the new format —
        // once. A second flush is an ordinary append again.
        let log = &mut loaded[0];
        log.append(user("more"));
        flush_in(&dir, log).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(text.lines().next().unwrap().contains(r#""format":1"#));
        log.append(user("and more"));
        flush_in(&dir, log).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 4);

        let again = load_all_in(&dir);
        assert_eq!(again[0].events.len(), 4);
        assert_eq!(again[0].format(), SESSION_FORMAT);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A log from a *newer* backend is refused, not repaired: nothing is
    /// damaged, this build simply cannot know what its rows mean. It is
    /// skipped by both the loader and the listing, and left byte-identical.
    #[test]
    fn a_future_format_log_is_skipped_and_never_rewritten() {
        let dir = temp_dir("format-future");
        fs::create_dir_all(&dir).unwrap();
        let path = jsonl_path(&dir, 8);
        let text = format!(
            concat!(
                r#"{{"seq":1,"ts":1000,"type":"session_created","id":8,"#,
                r#""title":"tomorrow","created_at":1,"format":{}}}"#,
                "\n",
            ),
            SESSION_FORMAT + 1
        );
        fs::write(&path, &text).unwrap();

        assert!(load_all_in(&dir).is_empty());
        assert!(list_headers_in(&dir).is_empty());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The listing answers from headers alone: no surface is derived and no
    /// `SessionLog` is built. It still has to agree with the full load about
    /// the two facts later rows can change — the newest title and the
    /// archive flag — including when they are far past any tail window.
    #[test]
    fn headers_agree_with_a_full_load_without_reading_the_body() {
        let dir = temp_dir("headers");
        let cwd = PathBuf::from("/tmp/proj-a");
        let mut a = SessionLog::new_in(21, "Session 21", Some(cwd.clone()));
        a.append(EventKind::SessionTitle { title: "Renamed early".into() });
        // A long body between the title and the end: a header read that only
        // looked at a bounded tail would lose the rename.
        for i in 0..200 {
            a.append(user(&format!("filler {i}")));
        }
        flush_in(&dir, &mut a).unwrap();

        let mut b = SessionLog::new(22, "Session 22");
        b.append(user("x"));
        b.append(EventKind::SessionArchived);
        flush_in(&dir, &mut b).unwrap();

        let headers = list_headers_in(&dir);
        assert_eq!(headers.len(), 2);
        let h = headers.iter().find(|h| h.id == 21).unwrap();
        assert_eq!(h.title, "Renamed early");
        assert_eq!(h.cwd.as_deref(), Some(cwd.as_path()));
        assert_eq!(h.format, SESSION_FORMAT);
        assert!(!h.archived);
        assert_eq!(h.created_at, a.created_at());
        assert_eq!(h.updated_at, a.updated_at());

        let g = headers.iter().find(|h| h.id == 22).unwrap();
        assert!(g.archived);
        assert!(g.cwd.is_none(), "a session created without one has no cwd");

        // The full load must not disagree with the cheap read.
        for log in load_all_in(&dir) {
            let h = headers.iter().find(|h| h.id == log.id).unwrap();
            assert_eq!(h.title, log.title());
            assert_eq!(h.archived, log.archived());
            assert_eq!(h.cwd, log.cwd());
            assert_eq!(h.updated_at, log.updated_at());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// A fork inherits the source's working directory. Without this a fork
    /// of a session in one project would reopen in whatever folder the
    /// process happens to default to — the silent move §3.9 exists to stop.
    #[test]
    fn a_fork_inherits_the_source_working_directory() {
        let cwd = PathBuf::from("/tmp/proj-b");
        let mut src = SessionLog::new_in(31, "Original", Some(cwd.clone()));
        src.append(EventKind::TurnStart { turn_id: 1, source: Default::default() });
        src.append(user("hi"));
        src.append(EventKind::TurnEnd { turn_id: 1, finish_reason: "done".into(), hops: 1 });
        let cut = src.events.len() - 1;
        let fork = SessionLog::fork(32, "Original (fork)", &src.events, cut);
        assert_eq!(fork.cwd(), Some(cwd));
        assert_eq!(fork.format(), SESSION_FORMAT);
    }
    #[test]
    fn delete_of_unflushed_session_is_a_noop() {
        let dir = temp_dir("delete-noop");
        delete_in(&dir, 12345);
        let _ = fs::remove_dir_all(&dir);
    }
}
