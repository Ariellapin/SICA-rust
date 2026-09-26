//! Spill-to-file for oversized tool outputs.
//!
//! A tool result is fed straight back into the model's context, so one
//! `run-cli` that dumps a build log — or a `read-file` on a generated
//! artifact — can burn most of the prompt budget in a single hop. When a
//! successful outcome exceeds [`SPILL_THRESHOLD`] the full text is written to
//! `spill/<label>/<skill>-<ts>-<id>.txt` and the model receives a digest:
//! the head, an omission marker naming the file and exact byte count, and
//! the tail. The raw text is never lost — it is on disk, and the marker tells
//! the model to `read-file` it if the digest is not enough.
//!
//! Deliberately narrow: only successful, text outcomes qualify; `read-file`
//! itself is exempt (a read → spill → read loop would never converge); a
//! write failure keeps the raw output rather than turning a success into an
//! error.
//!
//! Nothing here is ever read back by the harness, so nothing here has to
//! last: [`sweep`] (long-session-plan E2) removes files older than an age
//! and, per session, the oldest files past a byte cap. The backend runs it
//! at start and once an hour under the two `harness.toml` spill keys.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use sica_core::retain::{utf8_head, utf8_tail};
use sica_core::retain::{head_tail, notice};

/// Outputs longer than this (in bytes) are spilled.
pub const SPILL_THRESHOLD: usize = 48 * 1024;
/// Bytes of the original kept at the front of the digest.
pub const HEAD_BYTES: usize = 4096;
/// Bytes of the original kept at the end of the digest.
pub const TAIL_BYTES: usize = 1024;

/// Write `text` under `base/<label>/` and return the file path. `label` is
/// the session id in production; tests pass a temp dir as `base`.
pub fn write(base: &Path, label: &str, skill: &str, id: u64, text: &str) -> std::io::Result<PathBuf> {
    let dir = base.join(sanitize(label));
    fs::create_dir_all(&dir)?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("{}-{ts}-{id}.txt", sanitize(skill)));
    fs::write(&path, text)?;
    Ok(path)
}

/// The model-facing replacement for a spilled output: head + marker + tail.
/// The marker states the exact number of bytes omitted and the file path so
/// the model can decide whether the digest suffices.
pub fn digest(full: &str, path: &Path) -> String {
    let window = head_tail(full, HEAD_BYTES, TAIL_BYTES);
    let recovery = format!(
        "full output ({} bytes) saved to {}; use read-file '{}' to inspect it",
        full.len(),
        path.display(),
        path.display(),
    );
    window.render(&notice(window.omitted, &recovery))
}

/// One-line pointer appended after the expectation summariser rewrites a
/// digest, so the path survives the paraphrase.
pub fn pointer(path: &Path) -> String {
    format!("[full raw output saved to {}]", path.display())
}

/// What one [`sweep`] did. `errors` are per-file complaints (a file that
/// would not delete, a directory that would not list), each one line.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Sweep {
    pub removed:     usize,
    pub freed_bytes: u64,
    pub errors:      Vec<String>,
}

impl Sweep {
    /// One line for the operator's log.
    pub fn summary(&self) -> String {
        format!(
            "spill sweep: {} file(s) removed, {:.1} MiB freed{}",
            self.removed,
            self.freed_bytes as f64 / (1024.0 * 1024.0),
            if self.errors.is_empty() { String::new() } else { format!(", {} error(s)", self.errors.len()) }
        )
    }
}

/// Remove spilled files under `base/<label>/` that are older than
/// `max_age` (by modification time) and then, per label, the oldest files
/// until the label's total is within `max_bytes`. `None` switches a rule
/// off; both off is a no-op. Empty label directories are left in place —
/// a running session may be about to write into one.
pub fn sweep(base: &Path, max_age: Option<Duration>, max_bytes: Option<u64>) -> Sweep {
    sweep_at(base, SystemTime::now(), max_age, max_bytes)
}

/// [`sweep`] with the clock supplied, for tests.
pub fn sweep_at(base: &Path, now: SystemTime, max_age: Option<Duration>, max_bytes: Option<u64>) -> Sweep {
    let mut out = Sweep::default();
    if max_age.is_none() && max_bytes.is_none() {
        return out;
    }
    let labels = match fs::read_dir(base) {
        Ok(d) => d,
        // No spill directory is the normal case on a fresh install.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return out,
        Err(e) => {
            out.errors.push(format!("{}: {e}", base.display()));
            return out;
        }
    };
    for label in labels.flatten() {
        let dir = label.path();
        if !dir.is_dir() {
            continue;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(d) => d,
            Err(e) => {
                out.errors.push(format!("{}: {e}", dir.display()));
                continue;
            }
        };
        // (modified, len, path) for every regular file, oldest first.
        let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                if !meta.is_file() {
                    return None;
                }
                Some((meta.modified().unwrap_or(UNIX_EPOCH), meta.len(), e.path()))
            })
            .collect();
        files.sort_by_key(|(modified, _, _)| *modified);

        let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
        for (modified, len, path) in files {
            let too_old = max_age
                .is_some_and(|age| now.duration_since(modified).map_or(false, |d| d > age));
            let too_big = max_bytes.is_some_and(|cap| total > cap);
            if !too_old && !too_big {
                // Files are oldest first: once one is young and the label
                // fits, so does everything after it.
                break;
            }
            match fs::remove_file(&path) {
                Ok(()) => {
                    out.removed += 1;
                    out.freed_bytes += len;
                    total -= len;
                }
                Err(e) => out.errors.push(format!("{}: {e}", path.display())),
            }
        }
    }
    out
}

fn sanitize(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if out.is_empty() { "_".into() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_keeps_head_tail_and_names_path() {
        let full: String = (0..SPILL_THRESHOLD + 10).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let path = PathBuf::from("spill/7/run-cli-1-2.txt");
        let d = digest(&full, &path);
        assert!(d.starts_with(&full[..HEAD_BYTES]));
        assert!(d.ends_with(&full[full.len() - TAIL_BYTES..]));
        assert!(d.contains("run-cli-1-2.txt"));
        let omitted = full.len() - HEAD_BYTES - TAIL_BYTES;
        assert!(d.contains(&format!("{omitted} bytes omitted")));
        assert!(d.len() < full.len());
    }

    #[test]
    fn write_roundtrip_in_temp_dir() {
        let base = std::env::temp_dir().join(format!("sica-spill-test-{}", std::process::id()));
        let text = "x".repeat(100);
        let path = write(&base, "42", "run-cli", 9, &text).unwrap();
        assert!(path.starts_with(base.join("42")));
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        let _ = fs::remove_dir_all(&base);
    }

    fn sweep_dir(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("sica-sweep-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("7")).unwrap();
        base
    }

    fn aged(base: &Path, label: &str, name: &str, bytes: usize, age: Duration, now: SystemTime) -> PathBuf {
        let path = base.join(label).join(name);
        fs::write(&path, "x".repeat(bytes)).unwrap();
        fs::File::options().write(true).open(&path).unwrap().set_modified(now - age).unwrap();
        path
    }

    /// The age rule: older than the limit goes, younger stays, and the
    /// label directory is left for the session that owns it.
    #[test]
    fn sweep_removes_only_files_past_the_age() {
        let base = sweep_dir("age");
        let now = SystemTime::now();
        let old = aged(&base, "7", "run-cli-1-1.txt", 10, Duration::from_secs(8 * 86_400), now);
        let young = aged(&base, "7", "run-cli-2-2.txt", 10, Duration::from_secs(86_400), now);
        let s = sweep_at(&base, now, Some(Duration::from_secs(7 * 86_400)), None);
        assert_eq!(s, Sweep { removed: 1, freed_bytes: 10, errors: Vec::new() });
        assert!(!old.exists() && young.exists());
        assert!(base.join("7").is_dir(), "the label directory stays");
        assert!(s.summary().starts_with("spill sweep: 1 file(s) removed"), "{}", s.summary());
        let _ = fs::remove_dir_all(&base);
    }

    /// The size rule: per label, oldest first, until the total fits; a
    /// second label with room is untouched.
    #[test]
    fn sweep_trims_each_label_to_the_cap_oldest_first() {
        let base = sweep_dir("size");
        fs::create_dir_all(base.join("8")).unwrap();
        let now = SystemTime::now();
        let d = Duration::from_secs;
        let oldest = aged(&base, "7", "a.txt", 100, d(300), now);
        let middle = aged(&base, "7", "b.txt", 100, d(200), now);
        let newest = aged(&base, "7", "c.txt", 100, d(100), now);
        let other = aged(&base, "8", "job-1.txt", 100, d(1000), now);
        let s = sweep_at(&base, now, None, Some(150));
        assert_eq!(s.removed, 2, "{s:?}");
        assert_eq!(s.freed_bytes, 200);
        assert!(!oldest.exists() && !middle.exists() && newest.exists());
        assert!(other.exists(), "a label under the cap is not touched");
        let _ = fs::remove_dir_all(&base);
    }

    /// Both rules off, or no directory at all, is a no-op and not an error.
    #[test]
    fn sweep_is_a_no_op_when_off_or_absent() {
        let base = sweep_dir("off");
        let now = SystemTime::now();
        let f = aged(&base, "7", "a.txt", 10, Duration::from_secs(30 * 86_400), now);
        assert_eq!(sweep_at(&base, now, None, None), Sweep::default());
        assert!(f.exists());
        let _ = fs::remove_dir_all(&base);
        assert_eq!(sweep_at(&base, now, Some(Duration::from_secs(1)), Some(1)), Sweep::default());
    }

    #[test]
    fn sanitize_strips_path_chars() {
        assert_eq!(sanitize("../x/y"), "___x_y");
        assert_eq!(sanitize(""), "_");
    }
}
