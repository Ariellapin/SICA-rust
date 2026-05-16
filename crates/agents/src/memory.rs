//! `memory.md` — a small system brief the backend prepends to the chat
//! history so the model knows which built-in skills are available and how
//! to invoke them. Seeded at workspace root on first BE start and reloaded
//! from disk on every turn so user edits take effect without restarting.

use std::fs;
use std::path::Path;

/// Default content for `memory.md`. Index-shaped — one line per skill with
/// a relative link to the full contract in `skills/<name>.md`.
pub const SEED: &str = r#"# Sica memory

You are running inside the **sica-rust** desktop app. The backend exposes a
small set of built-in skills you can invoke. Each skill has its own
markdown file under `skills/` with the full contract — open it for the
JSON shape and behaviour.

## Skills

- **run-cli** — execute a shell command on the host. See [skills/run-cli.md](skills/run-cli.md).
- **read-file** — read a UTF-8 file from disk. See [skills/read-file.md](skills/read-file.md).
- **write-file** — write UTF-8 content to a file (overwrite or append). See [skills/write-file.md](skills/write-file.md).
- **skill-creator** — author a new markdown skill at runtime. See [skills/skill-creator.md](skills/skill-creator.md).

User-authored skills (any other `*.md` files in `skills/`) are loaded at
startup and are equally available.

## Invocation

To invoke a skill, emit a fenced ```tool_call``` block containing JSON with
a `skill` name and an `args` object:

    ```tool_call
    { "skill": "run-cli", "args": { "command": "cargo --version" } }
    ```

Refer to the linked `skills/*.md` file for the exact arg shape each skill
expects.
"#;

/// Write `memory.md` if it does not already exist. Never overwrites a file
/// the user has edited — they own this surface once it's on disk.
pub fn seed_default(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        fs::write(path, SEED)?;
    }
    Ok(())
}

/// Read `memory.md` from disk. Returns `None` if the file is absent so the
/// caller can skip the system-prompt prepend without surfacing an error.
pub fn load(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "sica-memory-{}-{}",
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
    fn seeds_once_and_preserves_edits() {
        let dir = tempdir();
        let p = dir.join("memory.md");
        seed_default(&p).unwrap();
        assert!(p.exists());
        fs::write(&p, "edited").unwrap();
        seed_default(&p).unwrap();
        assert_eq!(load(&p).unwrap(), "edited");
    }

    #[test]
    fn load_missing_is_none() {
        let dir = tempdir();
        assert!(load(&dir.join("nope.md")).is_none());
    }
}
