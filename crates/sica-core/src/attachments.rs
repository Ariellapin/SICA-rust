//! Content-addressed attachments (guide §9.6).
//!
//! An image the user pastes is bytes, and bytes are not history. The log
//! records *that* a message carried an image and what it hashes to; the
//! bytes live once under `sessions/<id>/attachments/<sha>.<ext>` and are
//! read back only by whoever needs to look at them — the request builder,
//! or the transcript.
//!
//! Three things follow, and they are the whole reason for the store.
//! **The log stops carrying megabytes.** A session with twenty screenshots
//! was twenty base64 blobs re-read on every load, re-derived on every turn,
//! and re-sent across the pipe on every reload. **Identical bytes are
//! stored once** — the same screenshot pasted into three messages is one
//! file, because the name *is* the content. And **the store is
//! append-only**: a file is written if it is not already there and never
//! rewritten, so a reference in an old message cannot be invalidated by a
//! later one.
//!
//! Nothing here deletes. `sessions_store::delete` takes the session's
//! directory with it, which is the only moment an attachment stops being
//! reachable.

use std::path::{Path, PathBuf};

use base64::Engine;
use protocol::UserImage;
use sha2::{Digest, Sha256};

/// dsh's admission limits (§9.6), as constants because they are the same
/// numbers the frontend normalises against.
pub mod limits {
    /// Images one message may carry.
    pub const IMAGES_PER_MESSAGE: usize = 20;
    /// One image's stored bytes. Also what keeps a request under the
    /// provider's body limit — base64 is a third larger again.
    pub const BYTES_PER_IMAGE: usize = 4 * 1024 * 1024;
    /// Longest edge after the frontend downscales on intake.
    pub const LONG_EDGE_PX: u32 = 2048;
}

/// SHA-256 of `bytes`, lowercase hex — the file's name and the image's
/// identity.
pub fn sha_of(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Store one image's bytes and return the reference that replaces them.
///
/// Idempotent by content: writing the same image twice is one file and one
/// `Ok`. An image that is already a reference passes through — it has been
/// stored, and re-storing it would need bytes nobody is carrying any more.
pub fn store(dir: &Path, image: &UserImage) -> std::io::Result<UserImage> {
    if image.is_reference() {
        return Ok(image.clone());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(image.data_base64.as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let sha = sha_of(&bytes);
    let path = dir.join(format!("{sha}.{}", image.extension()));
    if !path.exists() {
        std::fs::create_dir_all(dir)?;
        // Atomic, so a crash mid-write cannot leave a file whose name
        // promises content it does not have.
        crate::atomic::atomic_write(&path, &bytes)?;
    }
    Ok(UserImage {
        mime: image.mime.clone(),
        data_base64: String::new(),
        sha,
        bytes: bytes.len() as u64,
    })
}

/// Where a stored image lives.
pub fn path_of(dir: &Path, image: &UserImage) -> PathBuf {
    dir.join(format!("{}.{}", image.sha, image.extension()))
}

/// Read an image back as base64 — what the wire and the model both want.
///
/// An image that still carries its own bytes (a message logged before the
/// store existed) is returned as it is: the point is to resolve a
/// reference, not to insist every image be one.
pub fn resolve(dir: &Path, image: &UserImage) -> std::io::Result<UserImage> {
    if !image.is_reference() {
        return Ok(image.clone());
    }
    let bytes = std::fs::read(path_of(dir, image))?;
    Ok(UserImage {
        mime: image.mime.clone(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        sha: image.sha.clone(),
        bytes: bytes.len() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sica-att-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn image(bytes: &[u8]) -> UserImage {
        UserImage {
            mime: "image/png".into(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            sha: String::new(),
            bytes: 0,
        }
    }

    #[test]
    fn storing_replaces_the_bytes_with_a_reference_that_reads_back() {
        let dir = temp_dir("round");
        let original = image(b"the bytes of a screenshot");
        let stored = store(&dir, &original).unwrap();

        assert!(stored.data_base64.is_empty(), "the log must not carry bytes");
        assert!(stored.is_reference());
        assert_eq!(stored.bytes, 25);
        assert!(path_of(&dir, &stored).is_file());

        let back = resolve(&dir, &stored).unwrap();
        assert_eq!(back.data_base64, original.data_base64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The name is the content, so the same image in three messages is one
    /// file — and a second store is not a second write.
    #[test]
    fn identical_bytes_are_one_file() {
        let dir = temp_dir("dedup");
        let a = store(&dir, &image(b"same")).unwrap();
        let b = store(&dir, &image(b"same")).unwrap();
        assert_eq!(a.sha, b.sha);
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);

        // Different bytes are a different file.
        let c = store(&dir, &image(b"other")).unwrap();
        assert_ne!(a.sha, c.sha);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A message logged before the store existed carries its own bytes.
    /// Both paths have to keep working, in both directions.
    #[test]
    fn an_inline_image_passes_through_untouched() {
        let dir = temp_dir("legacy");
        let inline = image(b"old");
        assert!(!inline.is_reference());
        assert_eq!(resolve(&dir, &inline).unwrap().data_base64, inline.data_base64);
        // Storing it *is* meaningful — that is the migration — but resolving
        // a reference that was never stored is an error, not silence.
        let missing = UserImage {
            mime: "image/png".into(),
            data_base64: String::new(),
            sha: "0".repeat(64),
            bytes: 0,
        };
        assert!(resolve(&dir, &missing).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_extension_follows_the_declared_type() {
        for (mime, ext) in [
            ("image/png", "png"),
            ("image/jpeg", "jpg"),
            ("image/webp", "webp"),
            ("application/octet-stream", "bin"),
        ] {
            let img = UserImage { mime: mime.into(), ..image(b"x") };
            assert_eq!(img.extension(), ext);
        }
    }
}
