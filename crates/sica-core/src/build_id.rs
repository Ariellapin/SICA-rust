//! Build-identity hash derived from the latest source mtime.
//!
//! Both the backend (at startup) and the frontend (on file-watcher events)
//! call [`source_version`] to compute the same value. The BE reports its
//! result in `ServerHello.version`; the FE compares that against its own
//! freshly-computed value and surfaces a "restart" button when they diverge,
//! which means the on-disk source has been edited since the running BE was
//! built.

use std::time::UNIX_EPOCH;

use crate::paths::workspace_root;

/// Walk `crates/*/src/` and `crates/*/Cargo.toml` under the workspace root
/// and return a short identifier of the latest modification time. Files we
/// cannot stat are skipped silently — partial info is better than a panic on
/// a corner-case permission error.
pub fn source_version() -> String {
    let root = workspace_root();
    let crates = root.join("crates");
    let mut latest: u64 = 0;

    let Ok(entries) = std::fs::read_dir(&crates) else {
        return "unknown".to_string();
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Cargo.toml at crate root counts (dependency edits matter).
        bump_mtime(&path.join("Cargo.toml"), &mut latest);
        // The src/ tree.
        let src = path.join("src");
        if src.is_dir() {
            walk(&src, &mut latest);
        }
    }
    if latest == 0 {
        "unknown".to_string()
    } else {
        format!("src-{latest}")
    }
}

fn walk(dir: &std::path::Path, latest: &mut u64) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, latest);
        } else {
            bump_mtime(&path, latest);
        }
    }
}

fn bump_mtime(path: &std::path::Path, latest: &mut u64) {
    let Ok(meta) = std::fs::metadata(path) else { return };
    let Ok(modified) = meta.modified() else { return };
    let Ok(epoch) = modified.duration_since(UNIX_EPOCH) else { return };
    let secs = epoch.as_secs();
    if secs > *latest {
        *latest = secs;
    }
}
