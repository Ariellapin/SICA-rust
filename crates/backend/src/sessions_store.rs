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

use sica_core::event::{derive_surface, EventKind, SessionEvent, SurfaceEntry, SurfaceOp};
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
}

impl SessionLog {
    /// A brand-new session. Records `SessionCreated` in memory only; the
    /// file appears on the first flush.
    pub fn new(id: u64, title: impl Into<String>) -> Self {
        let created_at = chrono::Utc::now().timestamp();
        let mut log = Self { id, events: Vec::new(), next_seq: 1, flushed: 0 };
        log.append(EventKind::SessionCreated { id, title: title.into(), created_at });
        log
    }

    /// A session restored from disk — every event is already persisted.
    fn from_events(id: u64, events: Vec<SessionEvent>) -> Self {
        let next_seq = events.iter().map(|e| e.seq).max().unwrap_or(0) + 1;
        let flushed = events.len();
        Self { id, events, next_seq, flushed }
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

    pub fn created_at(&self) -> i64 {
        self.events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::SessionCreated { created_at, .. } => Some(*created_at),
                _ => None,
            })
            .unwrap_or(0)
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

/// Append every not-yet-flushed event to the session's file.
pub fn flush(log: &mut SessionLog) -> io::Result<()> {
    flush_in(&sessions_dir(), log)
}

pub fn flush_in(dir: &Path, log: &mut SessionLog) -> io::Result<()> {
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
        match read_events(&path) {
            Ok(events) if !events.is_empty() => out.push(SessionLog::from_events(id, events)),
            Ok(_) => warn!(path = %path.display(), "empty session log — skipped"),
            Err(e) => warn!(path = %path.display(), error = %e, "unreadable session log — skipped"),
        }
    }
    out.sort_by_key(|l| l.created_at());
    out
}

/// Parse a JSONL file, tolerating a torn final line (a crash mid-append)
/// and skipping any unparseable line in the middle. Never fatal on
/// content — only on I/O.
fn read_events(path: &Path) -> io::Result<Vec<SessionEvent>> {
    let text = fs::read_to_string(path)?;
    let mut out = Vec::new();
    let lines: Vec<&str> = text.split('\n').collect();
    let last = lines.len().saturating_sub(1);
    for (i, line) in lines.iter().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<SessionEvent>(line) {
            Ok(ev) => out.push(ev),
            Err(e) if i == last => {
                warn!(path = %path.display(), error = %e, "torn tail line dropped");
            }
            Err(e) => {
                warn!(path = %path.display(), line = i + 1, error = %e, "bad event line skipped");
            }
        }
    }
    Ok(out)
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
        };
        log.append(EventKind::SessionCreated {
            id,
            title: session.title.clone(),
            created_at: session.created_at,
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

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sica-sessions-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn user(text: &str) -> EventKind {
        EventKind::UserMessage { surface: SurfaceOp::Append, content: text.into(), images: Vec::new() }
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

    #[test]
    fn delete_of_unflushed_session_is_a_noop() {
        let dir = temp_dir("delete-noop");
        delete_in(&dir, 12345);
        let _ = fs::remove_dir_all(&dir);
    }
}
