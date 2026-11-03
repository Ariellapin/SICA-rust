//! The frontend's half of failure reporting: what goes wrong in the GUI
//! process itself becomes an idealist ticket too.
//!
//! The backend files everything it sees (`backend::incident`), but before
//! this `Request::ReportFrontendError` had no caller, so a GUI panic or a
//! backend that would not start was a log-panel line and nothing more.
//!
//! A report cannot always be sent when it happens: a panic usually takes
//! the window down, and a backend that failed to start is not there to
//! hear about it. So reports go to a small queue file first
//! (`idealist_workspace/fe-pending.jsonl`) and are sent the next time the
//! pipe connects — in this run or the next one.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One queued `ReportFrontendError`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub module:    String,
    pub message:   String,
    pub traceback: Option<String>,
}

/// Past this the queue stops growing: a failure that repeats every frame
/// must not fill the disk, and one copy of it is all the idealist needs.
const QUEUE_CAP_BYTES: u64 = 256 * 1024;

fn queue_path(dir: &Path) -> PathBuf {
    dir.join("fe-pending.jsonl")
}

/// Queue a report for the next connect. Never fails: reporting a problem
/// must not become a second one.
pub fn queue(module: &str, message: impl Into<String>, traceback: Option<String>) {
    queue_at(&sica_core::paths::idealist_workspace(), module, message.into(), traceback);
}

fn queue_at(dir: &Path, module: &str, message: String, traceback: Option<String>) {
    let path = queue_path(dir);
    if std::fs::metadata(&path).map(|m| m.len() >= QUEUE_CAP_BYTES).unwrap_or(false) {
        return;
    }
    let report = Report { module: module.into(), message, traceback };
    let Ok(line) = serde_json::to_string(&report) else { return };
    let _ = std::fs::create_dir_all(dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}

/// Take every queued report, oldest first, and empty the queue. Identical
/// reports collapse to one: the ticket counts occurrences on its own, and
/// a repeat adds nothing a person reads.
pub fn drain() -> Vec<Report> {
    drain_at(&sica_core::paths::idealist_workspace())
}

fn drain_at(dir: &Path) -> Vec<Report> {
    let path = queue_path(dir);
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    let _ = std::fs::remove_file(&path);
    let mut out: Vec<Report> = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        // A torn last line (the process died mid-write) is skipped, not fatal.
        if let Ok(r) = serde_json::from_str::<Report>(line) {
            if !out.contains(&r) {
                out.push(r);
            }
        }
    }
    out
}

/// Chain a panic hook that writes a crash file and queues the report.
/// Synchronous on purpose: after a panic on the UI thread the window and
/// the runtime are going away, and the file is what survives.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "panic with a non-string payload".to_string()
        };
        let (file, line) = info
            .location()
            .map(|l| (l.file().replace('\\', "/"), l.line()))
            .unwrap_or_else(|| ("unknown".into(), 0));
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        // A panic inside a dependency (an egui assertion) is located in the
        // cargo registry — machine-specific, and not where the fix goes.
        // The first frame in this workspace's own crates is.
        let own = own_frame(&backtrace);
        let mut traceback = format!("at {file}:{line} (thread {thread})\n");
        if let Some(own) = &own {
            traceback.push_str(&format!("first frame in sica-rust: {own}\n"));
        }
        traceback.push_str(&format!("\n{backtrace}"));
        let file = own
            .as_deref()
            .map(|f| f.split(':').next().unwrap_or(f).to_string())
            .unwrap_or(file);

        let dir = sica_core::paths::idealist_workspace();
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            dir.join(format!("fe-crash-{stamp}.md")),
            format!("# Frontend panic\n\n```\n{message}\n```\n\n```\n{traceback}\n```\n"),
        );
        // The module carries the file, not the line, like the backend's
        // hook: the same panic after an edit above it is the same ticket.
        queue_at(&dir, &format!("frontend::panic::{file}"), message, Some(traceback));
        previous(info);
    }));
}

/// The first backtrace frame in this workspace's `crates/`, as
/// `crates/<crate>/src/<file>:<line>` — skipping this module's own hook
/// frame and anything under the cargo registry.
fn own_frame(backtrace: &str) -> Option<String> {
    backtrace.lines().find_map(|l| {
        let path = l.trim().strip_prefix("at ")?.replace('\\', "/");
        if path.contains("/registry/") || path.contains("/rustc/") {
            return None;
        }
        let at = if path.starts_with("crates/") {
            0
        } else if path.starts_with("./crates/") {
            2
        } else {
            path.find("/crates/")? + 1
        };
        let rel = &path[at..];
        if rel.starts_with("crates/frontend/src/fe_incident.rs") {
            return None;
        }
        // `file:line:col` → `file:line`.
        let mut parts = rel.rsplitn(3, ':').collect::<Vec<_>>();
        parts.reverse();
        Some(match parts.as_slice() {
            [file, line, _col] => format!("{file}:{line}"),
            _ => rel.to_string(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_frame_skips_the_hook_and_the_registry() {
        let bt = "   2: frontend::fe_incident::install_panic_hook::{{closure}}\n\
                  \x20            at .\\crates\\frontend\\src\\fe_incident.rs:95:25\n\
                  \x20 29: ecolor::color32::Color32::linear_multiply\n\
                  \x20            at C:\\Users\\U\\.cargo\\registry\\src\\index.crates.io-1949\\ecolor-0.28.1\\src\\color32.rs:220:9\n\
                  \x20 31: frontend::ui::issues::sidebar_chip\n\
                  \x20            at .\\crates\\frontend\\src\\ui\\issues.rs:260:53\n";
        assert_eq!(own_frame(bt).as_deref(), Some("crates/frontend/src/ui/issues.rs:260"));
        let absolute = "  at C:\\work\\sica-rust\\crates\\backend\\src\\chat.rs:10:5\n";
        assert_eq!(own_frame(absolute).as_deref(), Some("crates/backend/src/chat.rs:10"));
        assert_eq!(own_frame("  at /rustc/abc/library/std/src/panicking.rs:1:1\n"), None);
    }

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "sica-fe-incident-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn queue_then_drain_round_trips_and_empties() {
        let dir = scratch("roundtrip");
        queue_at(&dir, "frontend::supervisor::start_be", "start BE failed: x".into(), None);
        queue_at(&dir, "frontend::panic::src/app.rs", "boom".into(), Some("at src/app.rs:1".into()));
        let got = drain_at(&dir);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].module, "frontend::supervisor::start_be");
        assert_eq!(got[1].traceback.as_deref(), Some("at src/app.rs:1"));
        assert!(drain_at(&dir).is_empty(), "a drained queue is empty");
    }

    #[test]
    fn drain_collapses_repeats_and_skips_torn_lines() {
        let dir = scratch("repeats");
        for _ in 0..3 {
            queue_at(&dir, "frontend::supervisor::respawn", "respawn failed".into(), None);
        }
        let mut f = std::fs::OpenOptions::new().append(true).open(queue_path(&dir)).unwrap();
        write!(f, "{{\"module\":\"torn").unwrap();
        drop(f);
        let got = drain_at(&dir);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn queue_stops_at_the_cap() {
        let dir = scratch("cap");
        let big = "x".repeat(QUEUE_CAP_BYTES as usize);
        queue_at(&dir, "frontend::m", big, None);
        let len = std::fs::metadata(queue_path(&dir)).unwrap().len();
        queue_at(&dir, "frontend::m", "more".into(), None);
        assert_eq!(std::fs::metadata(queue_path(&dir)).unwrap().len(), len);
    }

    #[test]
    fn nothing_queued_drains_to_nothing() {
        assert!(drain_at(&scratch("empty")).is_empty());
    }
}
