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

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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

    #[test]
    fn sanitize_strips_path_chars() {
        assert_eq!(sanitize("../x/y"), "___x_y");
        assert_eq!(sanitize(""), "_");
    }
}
