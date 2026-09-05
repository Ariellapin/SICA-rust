//! The workspace registry (guide §3.9).
//!
//! A **workspace** is the durable record of a directory the user works in:
//! `{ id, path, title, created_at, updated_at, sessions }`. It exists so the
//! sidebar can group sessions by project and so a new session knows which
//! folder to start in — nothing more. The subsystem is deliberately
//! **invisible to models**: no tool, no prompt text, no session event.
//!
//! Two rules carried over from dsh, and they are the whole design.
//!
//! **The session header is the proof.** Membership needs *both* an id on the
//! workspace's account and a session whose own `SessionCreated.cwd` (§3.8)
//! equals the workspace's path. A session therefore belongs to at most one
//! workspace, and a stale account entry — a session moved, deleted or
//! created under a folder that has since been re-registered — is filtered
//! out on read and pruned on the next write. The account is an *ordering*,
//! not the truth.
//!
//! **A registration is not the directory.** [`Registry::delete`] removes the
//! row and nothing else: the folder, its files, the sessions and their logs
//! all survive, and those sessions become Ungrouped. A folder that is not
//! there right now reports `missing` — answered live, never stored, because
//! an unmounted drive is a status and not a deletion.
//!
//! One JSON document under `sica-settings/workspaces.json`, republished
//! through [`sica_core::atomic::atomic_write`] on every mutation. dsh needs
//! a pending-mutation marker because create and delete are two writes there;
//! here it is one document and one atomic rename, so that hazard does not
//! exist and neither does the marker.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use protocol::WorkspaceDump;
use serde::{Deserialize, Serialize};
use sica_core::atomic::atomic_write_json;
use tracing::{info, warn};

/// Generation of the registry document. A newer one is refused rather than
/// half-understood — the same fail-closed rule the session format takes
/// (§3.8), and the same reason: nothing is damaged, this build just cannot
/// know what the rows mean.
pub const REGISTRY_VERSION: u16 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Row {
    pub path: PathBuf,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// Manual order. A new session is prepended; activity never reorders.
    #[serde(default)]
    pub sessions: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub version: u16,
    /// Display order of the workspaces themselves.
    #[serde(default)]
    pub order: Vec<u64>,
    #[serde(default)]
    pub rows: BTreeMap<u64, Row>,
}

impl Default for Document {
    fn default() -> Self {
        Self { version: REGISTRY_VERSION, order: Vec::new(), rows: BTreeMap::new() }
    }
}

/// What the registry needs to know about one session to place it. Supplied
/// by the caller from the live session map, so a session that has not been
/// flushed yet still groups correctly.
#[derive(Debug, Clone)]
pub struct SessionFacts {
    pub id: u64,
    pub cwd: Option<PathBuf>,
    pub archived: bool,
}

pub struct Registry {
    path: PathBuf,
    doc: Mutex<Document>,
    /// The document on disk is from a newer backend. Everything still
    /// works — the registry is simply empty for this run — but nothing may
    /// be written over it.
    refused: bool,
}

impl Registry {
    /// Open the document. A missing one is the normal first-run case and
    /// yields an empty registry — [`Registry::bootstrap_if_empty`] is what
    /// fills it.
    ///
    /// A document from a *newer* backend is refused and left on disk
    /// untouched; a corrupt one is derived, rebuildable data, so it is moved
    /// aside rather than made fatal.
    pub fn load(path: PathBuf) -> Self {
        let doc = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Document>(&text) {
                Ok(doc) if doc.version > REGISTRY_VERSION => {
                    warn!(
                        path = %path.display(), version = doc.version,
                        known = REGISTRY_VERSION,
                        "workspace registry was written by a newer backend — \
                         starting empty and leaving the file alone",
                    );
                    // `refused` keeps `bootstrap_if_empty` from overwriting
                    // a document it could not read.
                    return Self { path, doc: Mutex::new(Document::default()), refused: true };
                }
                Ok(doc) => doc,
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "unreadable workspace registry — \
                          moved aside; it will be rebuilt from session headers");
                    let _ = std::fs::rename(&path, path.with_extension("json.bad"));
                    Document::default()
                }
            },
            Err(_) => Document::default(),
        };
        Self { path, doc: Mutex::new(doc), refused: false }
    }

    /// Fill an empty registry from **session headers only** (§3.8) — never a
    /// body — grouping sessions that have a directory, newest first, one
    /// workspace per distinct path. Sessions without one stay Ungrouped,
    /// which is what every pre-§3.9 session is.
    ///
    /// Does nothing once the registry holds anything, so it is safe to call
    /// on every start: the ids a bootstrap mints are stable from the moment
    /// the document is written, and the write happens last, so an
    /// interrupted bootstrap simply runs again next time.
    pub fn bootstrap_if_empty(&self, headers: &[BootstrapSession], next_id: &dyn Fn() -> u64) {
        if self.refused {
            return;
        }
        {
            let doc = self.doc.lock().expect("registry");
            if !doc.rows.is_empty() {
                return;
            }
        }
        let mut doc = Document::default();
        // Newest first: the session that opens a workspace's list should be
        // the one the user touched last, and prepending as we walk oldest to
        // newest is the same order dsh's `sessionIds` ends up in.
        let mut sorted: Vec<&BootstrapSession> = headers.iter().collect();
        sorted.sort_by_key(|h| h.created_at);
        for h in sorted {
            let Some(cwd) = h.cwd.as_ref().and_then(|p| canonical(p).ok()) else { continue };
            let existing = doc.rows.iter().find(|(_, r)| r.path == cwd).map(|(id, _)| *id);
            let id = match existing {
                Some(id) => id,
                None => {
                    let id = next_id();
                    doc.rows.insert(id, Row {
                        title: title_for(&cwd),
                        path: cwd.clone(),
                        created_at: h.created_at,
                        updated_at: h.created_at,
                        sessions: Vec::new(),
                    });
                    doc.order.push(id);
                    id
                }
            };
            if let Some(row) = doc.rows.get_mut(&id) {
                row.sessions.insert(0, h.id);
                row.updated_at = row.updated_at.max(h.created_at);
            }
        }
        if doc.rows.is_empty() {
            // Nothing to record — and nothing to write, so a fresh install
            // does not leave an empty document behind.
            return;
        }
        info!(workspaces = doc.rows.len(), "workspace registry bootstrapped from session headers");
        *self.doc.lock().expect("registry") = doc;
        self.save();
    }

    fn save(&self) {
        if self.refused {
            return;
        }
        let doc = self.doc.lock().expect("registry").clone();
        if let Err(e) = atomic_write_json(&self.path, &doc) {
            warn!(path = %self.path.display(), error = %e, "could not write workspace registry");
        }
    }

    /// The directory a workspace points at, for resolving a new session's
    /// `cwd`. `None` for an id that is not registered.
    pub fn path_of(&self, id: u64) -> Option<PathBuf> {
        self.doc.lock().expect("registry").rows.get(&id).map(|r| r.path.clone())
    }

    /// Register `path`, or return the existing id when it is already known.
    ///
    /// Idempotent by **canonical** path, so `C:\proj`, `C:\proj\` and
    /// `C:\proj\sub\..` are one workspace — and so is a symlink pointing at
    /// an already-registered directory, which is the collision dsh wants
    /// rather than two rows for one folder.
    pub fn create(&self, id: u64, path: &Path, title: Option<String>) -> Result<u64, String> {
        if path.as_os_str().is_empty() {
            return Err("workspace path is empty".into());
        }
        if !path.is_absolute() {
            return Err(format!("{} is not an absolute path", path.display()));
        }
        let canon = canonical(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !canon.is_dir() {
            return Err(format!("{} is not a directory", canon.display()));
        }
        let mut doc = self.doc.lock().expect("registry");
        if let Some((existing, _)) = doc.rows.iter().find(|(_, r)| r.path == canon) {
            return Ok(*existing);
        }
        let now = chrono::Utc::now().timestamp();
        let title = title.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| title_for(&canon));
        doc.rows.insert(id, Row {
            path: canon,
            title,
            created_at: now,
            updated_at: now,
            sessions: Vec::new(),
        });
        doc.order.push(id);
        drop(doc);
        self.save();
        Ok(id)
    }

    pub fn rename(&self, id: u64, title: &str) -> bool {
        let mut doc = self.doc.lock().expect("registry");
        let Some(row) = doc.rows.get_mut(&id) else { return false };
        if title.trim().is_empty() {
            return false;
        }
        row.title = title.trim().to_string();
        row.updated_at = chrono::Utc::now().timestamp();
        drop(doc);
        self.save();
        true
    }

    /// Remove the registration. Nothing on disk is touched — the directory,
    /// its files and every session log survive, and those sessions become
    /// Ungrouped on the next projection.
    pub fn delete(&self, id: u64) -> bool {
        let mut doc = self.doc.lock().expect("registry");
        if doc.rows.remove(&id).is_none() {
            return false;
        }
        doc.order.retain(|x| *x != id);
        drop(doc);
        self.save();
        true
    }

    /// Place `id` immediately before `before`, or last when it is `None`.
    pub fn move_workspace(&self, id: u64, before: Option<u64>) -> bool {
        let mut doc = self.doc.lock().expect("registry");
        if !doc.rows.contains_key(&id) {
            return false;
        }
        doc.order.retain(|x| *x != id);
        match before.and_then(|b| doc.order.iter().position(|x| *x == b)) {
            Some(at) => doc.order.insert(at, id),
            None => doc.order.push(id),
        }
        drop(doc);
        self.save();
        true
    }

    /// Reorder one session inside its workspace's manual order.
    pub fn move_session(&self, workspace_id: u64, session_id: u64, before: Option<u64>) -> bool {
        let mut doc = self.doc.lock().expect("registry");
        let Some(row) = doc.rows.get_mut(&workspace_id) else { return false };
        if !row.sessions.contains(&session_id) {
            return false;
        }
        row.sessions.retain(|x| *x != session_id);
        match before.and_then(|b| row.sessions.iter().position(|x| *x == b)) {
            Some(at) => row.sessions.insert(at, session_id),
            None => row.sessions.push(session_id),
        }
        drop(doc);
        self.save();
        true
    }

    /// Put a freshly created session at the head of a workspace's order.
    /// Called *after* the session's header is stamped, so the account only
    /// ever records what the header already proves.
    pub fn attach(&self, workspace_id: u64, session_id: u64) -> bool {
        let mut doc = self.doc.lock().expect("registry");
        let Some(row) = doc.rows.get_mut(&workspace_id) else { return false };
        row.sessions.retain(|x| *x != session_id);
        row.sessions.insert(0, session_id);
        row.updated_at = chrono::Utc::now().timestamp();
        drop(doc);
        self.save();
        true
    }

    /// The projection the wire carries: one row per workspace plus the
    /// sessions that belong to none.
    ///
    /// This is where the header rule bites. A session appears under a
    /// workspace only when its own directory matches; an account entry that
    /// disagrees is dropped here and pruned from the document by
    /// [`Registry::prune`]. Archived sessions are hidden from every
    /// grouping surface, exactly as they are hidden from the list.
    pub fn project(&self, sessions: &[SessionFacts]) -> (Vec<WorkspaceDump>, Vec<u64>) {
        let doc = self.doc.lock().expect("registry");
        let live: BTreeMap<u64, Option<PathBuf>> = sessions
            .iter()
            .filter(|s| !s.archived)
            .map(|s| (s.id, s.cwd.as_ref().and_then(|p| canonical(p).ok())))
            .collect();

        let mut rows = Vec::new();
        let mut grouped: Vec<u64> = Vec::new();
        for id in &doc.order {
            let Some(row) = doc.rows.get(id) else { continue };
            let mut members: Vec<u64> = row
                .sessions
                .iter()
                .copied()
                .filter(|sid| live.get(sid).map(|cwd| cwd.as_deref() == Some(row.path.as_path())).unwrap_or(false))
                .collect();
            // A session whose header names this workspace but which the
            // account has never heard of — created by an older build, or
            // attached while the document could not be written — still
            // belongs here. Newest first, after the manually ordered ones.
            let mut extra: Vec<u64> = live
                .iter()
                .filter(|(sid, cwd)| {
                    cwd.as_deref() == Some(row.path.as_path()) && !members.contains(sid)
                })
                .map(|(sid, _)| *sid)
                .collect();
            extra.sort_unstable_by(|a, b| b.cmp(a));
            members.append(&mut extra);
            grouped.extend(members.iter().copied());
            rows.push(WorkspaceDump {
                id: *id,
                path: row.path.clone(),
                title: row.title.clone(),
                created_at: row.created_at,
                updated_at: row.updated_at,
                sessions: members,
                missing: !row.path.is_dir(),
            });
        }
        let ungrouped: Vec<u64> = sessions
            .iter()
            .filter(|s| !s.archived && !grouped.contains(&s.id))
            .map(|s| s.id)
            .collect();
        (rows, ungrouped)
    }

    /// Drop account entries for sessions that no longer exist or whose
    /// header disagrees. Called after a mutation, so the document converges
    /// without a read ever depending on it.
    pub fn prune(&self, sessions: &[SessionFacts]) {
        let known: BTreeMap<u64, Option<PathBuf>> = sessions
            .iter()
            .map(|s| (s.id, s.cwd.as_ref().and_then(|p| canonical(p).ok())))
            .collect();
        let mut doc = self.doc.lock().expect("registry");
        let mut changed = false;
        for row in doc.rows.values_mut() {
            let before = row.sessions.len();
            let path = row.path.clone();
            row.sessions
                .retain(|sid| known.get(sid).map(|c| c.as_deref() == Some(path.as_path())).unwrap_or(false));
            changed |= row.sessions.len() != before;
        }
        drop(doc);
        if changed {
            self.save();
        }
    }
}

/// The three header facts a bootstrap needs. Kept separate from
/// `sessions_store::SessionHeader` so the registry does not depend on the
/// store's shape — dsh reads id, cwd and createdAt and nothing else.
#[derive(Debug, Clone)]
pub struct BootstrapSession {
    pub id: u64,
    pub cwd: Option<PathBuf>,
    pub created_at: i64,
}

/// `fs::canonicalize` with Windows' verbatim prefix stripped.
///
/// The prefix is correct but unprintable — `\\?\C:\proj` in a sidebar is
/// noise — and, worse, it makes two spellings of one directory compare
/// unequal, which is exactly what canonicalising is for.
pub fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    let canon = std::fs::canonicalize(path)?;
    let text = canon.to_string_lossy();
    Ok(match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => canon,
    })
}

/// Default title: the directory's own name, which is what the user calls
/// the project. Falls back to the whole path for a drive root, which has no
/// final segment.
fn title_for(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sica-ws-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        canonical(&dir).unwrap()
    }

    fn counter() -> impl Fn() -> u64 {
        let n = std::sync::atomic::AtomicU64::new(1);
        move || n.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn boot(reg: Registry, headers: &[BootstrapSession]) -> Registry {
        reg.bootstrap_if_empty(headers, &counter());
        reg
    }

    fn facts(id: u64, cwd: Option<&Path>) -> SessionFacts {
        SessionFacts { id, cwd: cwd.map(Path::to_path_buf), archived: false }
    }

    #[test]
    fn create_is_idempotent_per_canonical_path() {
        let root = temp_dir("idem");
        let proj = root.join("proj");
        std::fs::create_dir_all(proj.join("sub")).unwrap();
        let reg = Registry::load(root.join("workspaces.json"));

        let a = reg.create(10, &proj, None).unwrap();
        // Same directory, three spellings: one workspace.
        let b = reg.create(11, &proj.join("sub").join(".."), None).unwrap();
        let c = reg.create(12, &PathBuf::from(format!("{}\\", proj.display())), None).unwrap();
        assert_eq!((a, b), (10, 10));
        assert_eq!(c, 10);
        let (rows, _) = reg.project(&[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "proj", "the title defaults to the folder name");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_relative_or_missing_path_is_refused() {
        let root = temp_dir("refuse");
        let reg = Registry::load(root.join("workspaces.json"));
        assert!(reg.create(1, Path::new("relative/dir"), None).is_err());
        assert!(reg.create(2, &root.join("nope"), None).is_err());
        // A file is not a directory.
        let file = root.join("f.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(reg.create(3, &file, None).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn membership_needs_the_session_header_to_agree() {
        let root = temp_dir("member");
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let reg = Registry::load(root.join("workspaces.json"));
        reg.create(100, &a, None).unwrap();
        reg.attach(100, 1);
        reg.attach(100, 2);

        // Session 2's header says it works in `b`: the account entry is a
        // lie and the projection must not repeat it.
        let sessions = vec![facts(1, Some(&a)), facts(2, Some(&b)), facts(3, None)];
        let (rows, ungrouped) = reg.project(&sessions);
        assert_eq!(rows[0].sessions, vec![1]);
        assert_eq!(ungrouped, vec![2, 3], "a disagreeing header lands Ungrouped");

        // ...and the next write drops it.
        reg.prune(&sessions);
        let reg = Registry::load(root.join("workspaces.json"));
        let (rows, _) = reg.project(&sessions);
        assert_eq!(rows[0].sessions, vec![1]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_session_the_account_never_heard_of_still_groups_by_its_header() {
        let root = temp_dir("orphan");
        let a = root.join("a");
        std::fs::create_dir_all(&a).unwrap();
        let reg = Registry::load(root.join("workspaces.json"));
        reg.create(100, &a, None).unwrap();
        let (rows, ungrouped) = reg.project(&[facts(7, Some(&a))]);
        assert_eq!(rows[0].sessions, vec![7]);
        assert!(ungrouped.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_removes_the_registration_only() {
        let root = temp_dir("delete");
        let a = root.join("a");
        std::fs::create_dir_all(&a).unwrap();
        let marker = a.join("keep.txt");
        std::fs::write(&marker, "still here").unwrap();
        let reg = Registry::load(root.join("workspaces.json"));
        reg.create(100, &a, None).unwrap();
        reg.attach(100, 1);

        assert!(reg.delete(100));
        assert!(!reg.delete(100), "deleting twice is not a second removal");
        let (rows, ungrouped) = reg.project(&[facts(1, Some(&a))]);
        assert!(rows.is_empty());
        assert_eq!(ungrouped, vec![1], "its sessions become Ungrouped");
        assert!(marker.exists(), "the directory and its files are untouched");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_directory_is_a_status_not_a_deletion() {
        let root = temp_dir("missing");
        let gone = root.join("gone");
        std::fs::create_dir_all(&gone).unwrap();
        let reg = Registry::load(root.join("workspaces.json"));
        reg.create(100, &gone, None).unwrap();
        std::fs::remove_dir_all(&gone).unwrap();

        let (rows, _) = reg.project(&[]);
        assert_eq!(rows.len(), 1, "the registration survives its directory");
        assert!(rows[0].missing);
        // And the recorded path is never rewritten.
        assert_eq!(rows[0].path, canonical(&root).unwrap().join("gone"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bootstrap_groups_by_directory_newest_first_and_survives_a_restart() {
        let root = temp_dir("bootstrap");
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let headers = vec![
            BootstrapSession { id: 1, cwd: Some(a.clone()), created_at: 100 },
            BootstrapSession { id: 2, cwd: Some(b.clone()), created_at: 200 },
            BootstrapSession { id: 3, cwd: Some(a.clone()), created_at: 300 },
            // A pre-§3.9 session: no directory, so it stays Ungrouped.
            BootstrapSession { id: 4, cwd: None, created_at: 400 },
        ];
        let doc_path = root.join("workspaces.json");
        let reg = boot(Registry::load(doc_path.clone()), &headers);
        let sessions: Vec<SessionFacts> = vec![
            facts(1, Some(&a)), facts(2, Some(&b)), facts(3, Some(&a)), facts(4, None),
        ];
        let (rows, ungrouped) = reg.project(&sessions);
        assert_eq!(rows.len(), 2);
        let wa = rows.iter().find(|r| r.path == canonical(&a).unwrap()).unwrap();
        assert_eq!(wa.sessions, vec![3, 1], "newest first");
        assert_eq!(ungrouped, vec![4]);

        // The document was written, so the next start does not bootstrap.
        assert!(doc_path.exists());
        let again = Registry::load(doc_path);
        let (rows2, _) = again.project(&sessions);
        assert_eq!(rows2.len(), 2);
        assert_eq!(
            rows2.iter().find(|r| r.path == canonical(&a).unwrap()).unwrap().sessions,
            vec![3, 1],
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ordering_is_manual_for_workspaces_and_for_sessions() {
        let root = temp_dir("order");
        let (a, b, c) = (root.join("a"), root.join("b"), root.join("c"));
        for d in [&a, &b, &c] {
            std::fs::create_dir_all(d).unwrap();
        }
        let reg = Registry::load(root.join("workspaces.json"));
        reg.create(1, &a, None).unwrap();
        reg.create(2, &b, None).unwrap();
        reg.create(3, &c, None).unwrap();
        assert!(reg.move_workspace(3, Some(1)));
        let (rows, _) = reg.project(&[]);
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![3, 1, 2]);

        reg.attach(1, 10);
        reg.attach(1, 11);
        let sessions = vec![facts(10, Some(&a)), facts(11, Some(&a))];
        let (rows, _) = reg.project(&sessions);
        assert_eq!(rows[1].sessions, vec![11, 10], "a new session is prepended");
        assert!(reg.move_session(1, 11, None));
        let (rows, _) = reg.project(&sessions);
        assert_eq!(rows[1].sessions, vec![10, 11]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_future_document_is_refused_and_left_alone() {
        let root = temp_dir("future");
        let path = root.join("workspaces.json");
        let text = format!(r#"{{"version":{},"order":[],"rows":{{}}}}"#, REGISTRY_VERSION + 1);
        std::fs::write(&path, &text).unwrap();
        let reg = Registry::load(path.clone());
        assert!(reg.project(&[]).0.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_corrupt_document_is_moved_aside_and_rebuilt() {
        let root = temp_dir("corrupt");
        let a = root.join("a");
        std::fs::create_dir_all(&a).unwrap();
        let path = root.join("workspaces.json");
        std::fs::write(&path, "{not json").unwrap();
        let headers = vec![BootstrapSession { id: 1, cwd: Some(a.clone()), created_at: 1 }];
        let reg = boot(Registry::load(path.clone()), &headers);
        assert_eq!(reg.project(&[facts(1, Some(&a))]).0.len(), 1);
        assert!(path.with_extension("json.bad").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn archived_sessions_are_hidden_from_every_grouping_surface() {
        let root = temp_dir("archived");
        let a = root.join("a");
        std::fs::create_dir_all(&a).unwrap();
        let reg = Registry::load(root.join("workspaces.json"));
        reg.create(1, &a, None).unwrap();
        reg.attach(1, 5);
        let sessions = vec![SessionFacts { id: 5, cwd: Some(a.clone()), archived: true }];
        let (rows, ungrouped) = reg.project(&sessions);
        assert!(rows[0].sessions.is_empty());
        assert!(ungrouped.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
