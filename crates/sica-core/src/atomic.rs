//! Crash-safe whole-file writes.
//!
//! Every file this app rewrites in place - the settings document, the
//! workspace registry, a migrated session log - is a document whose
//! *partial* form is worse than its previous form: a half-written
//! `workspaces.json` loses every workspace, not the one being renamed.
//! [`atomic_write`] gives the write the usual three-step shape (temp file in
//! the same directory, `sync_all`, then `rename`), so a reader sees either
//! the old bytes or the new ones and never a prefix of the new ones.
//!
//! The temp file is a sibling, not a `%TEMP%` entry: `rename` is only atomic
//! within a volume, and the destination's own directory is the one place
//! guaranteed to be on the same one.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Write `bytes` to `path`, replacing it atomically.
///
/// Creates the parent directory if it is missing. On success the temp file
/// is gone; on failure it is removed on a best-effort basis and `path` is
/// left exactly as it was.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let tmp = temp_sibling(path);
    let write = || -> io::Result<()> {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        // Durability before visibility: a rename that lands before the data
        // does turns a crash into a zero-length document.
        file.sync_all()?;
        drop(file);
        // `std::fs::rename` replaces an existing destination on both
        // Windows (`MoveFileEx` with `REPLACE_EXISTING`) and Unix.
        fs::rename(&tmp, path)
    };
    match write() {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Serialise `value` as pretty JSON and [`atomic_write`] it.
pub fn atomic_write_json<T: serde::Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    atomic_write(path, text.as_bytes())
}

/// `<name>.<pid>.tmp` beside the target. The pid keeps two processes
/// writing the same document from clobbering each other's temp file; the
/// rename is still last-writer-wins, which is the semantics a single-writer
/// document wants.
fn temp_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!("{name}.{}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sica-atomic-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_creates_parents_and_replaces_in_place() {
        let dir = temp_dir("basic");
        let path = dir.join("nested").join("doc.json");
        atomic_write(&path, b"first").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "first");
        // Replacing an existing file is the case a naive create-new rename
        // gets wrong on Windows, so it is pinned here.
        atomic_write(&path, b"second").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaves_no_temp_file_behind() {
        let dir = temp_dir("clean");
        let path = dir.join("doc.txt");
        atomic_write(&path, b"x").unwrap();
        let strays: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "temp files left: {strays:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_write_leaves_the_old_bytes() {
        let dir = temp_dir("fail");
        let path = dir.join("doc.txt");
        atomic_write(&path, b"good").unwrap();
        // A directory where the temp file wants to be: `File::create` fails,
        // and the destination must be untouched.
        fs::create_dir(temp_sibling(&path)).unwrap();
        assert!(atomic_write(&path, b"bad").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "good");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_round_trips() {
        let dir = temp_dir("json");
        let path = dir.join("v.json");
        atomic_write_json(&path, &serde_json::json!({ "a": 1 })).unwrap();
        let back: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["a"], 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
