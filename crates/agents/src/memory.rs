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
markdown file under `skills/` with the full contract — open it to see the
positional arguments it accepts.

## Skills

- **run-cli** — execute a shell command on the host. See [skills/run-cli.md](skills/run-cli.md).
- **run-pwsh** — execute a PowerShell command (preferred on Windows). See [skills/run-pwsh.md](skills/run-pwsh.md).
- **read-file** — read a UTF-8 file from disk. See [skills/read-file.md](skills/read-file.md).
- **write-file** — write UTF-8 content to a file. See [skills/write-file.md](skills/write-file.md).
- **skill-creator** — author a new markdown skill at runtime. See [skills/skill-creator.md](skills/skill-creator.md).

User-authored skills (any other `*.md` files in `skills/`) are loaded at
startup and are equally available.

## Invocation — natural language, one line

Emit a single line:

    <skill-name> '<arg-1>' ['<arg-2>' ...] > <what you expect back>

- The first token is the skill name (always dashed, lowercase).
- Then come the positional args in the order declared by the skill's
  `skills/<name>.md`. Quote every arg with single or double quotes (escape
  newlines as `\n`, single-quotes as `\'`, etc.).
- The `>` token (whitespace on both sides) separates the call from your
  **expectation** — a short phrase saying what you want to know from the
  result. The sub-agent will run the skill and reply with a focused summary
  addressing exactly that expectation, not the full raw output.

Examples:

    read-file 'skills/run-cli.md' > what positional args does run-cli accept
    run-cli 'cargo --version' > confirm cargo is installed and report the version
    write-file 'notes/x.md' 'hello, world\n' > confirm bytes written

Do **not** use JSON, fenced `tool_call` blocks, or any other shape — the
parser only recognises the single-line natural-language form above. After
your final answer, stop emitting tool-call lines: the loop ends when the
assistant turn contains no more invocations.
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
