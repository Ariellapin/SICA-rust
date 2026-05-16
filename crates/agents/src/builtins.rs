//! Built-in skills implemented in Rust: `run-cli`, `read-file`, `write-file`.
//!
//! Each skill has a companion `skills/<name>.md` describing the JSON
//! contract for the LLM (seeded on first BE start, see `seed_defaults`).
//! The Rust impl below is what actually runs when the skill is invoked
//! through a `ToolSubAgent`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::timeout;

use crate::skill::{Skill, SkillContext, SkillOutcome};

const CLI_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT: usize = 32 * 1024;
const MAX_FILE: u64 = 1024 * 1024;

pub const RUN_CLI_NAME:    &str = "run-cli";
pub const READ_FILE_NAME:  &str = "read-file";
pub const WRITE_FILE_NAME: &str = "write-file";

pub const RUN_CLI_DESCRIPTION: &str =
    "Execute a shell command. Args: { command, cwd? }. \
     Returns stdout/stderr/exit_code; capped to 32 KiB output, 30 s timeout.";

pub const READ_FILE_DESCRIPTION: &str =
    "Read a UTF-8 file. Args: { path }. \
     Relative paths resolve against the workspace root; up to 1 MiB.";

pub const WRITE_FILE_DESCRIPTION: &str =
    "Write UTF-8 content to a file. Args: { path, content, append? }. \
     Creates parent dirs; refuses `..` traversal in relative paths.";

pub const RUN_CLI_SEED_MD: &str = r#"---
name: run-cli
description: Execute a shell command on the host (cmd.exe on Windows, /bin/sh elsewhere).
---
Run a shell command on the host. stdout and stderr are captured and returned
to the agent in the outcome `summary`.

Args (JSON):

```
{
  "command": "git status",   // required, the shell command line
  "cwd":     "."             // optional, working dir (relative to workspace root or absolute)
}
```

Behaviour:
- Windows: invokes `cmd /C <command>`. Other OSes: `/bin/sh -c <command>`.
- Stdout and stderr are each capped to **32 KiB** before being returned.
- A timeout of **30 seconds** kills the child and reports an error outcome.
- The outcome `ok` mirrors the child exit code (0 = ok).

Use this for build tools, git, package managers, or one-shot scripts.
"#;

pub const READ_FILE_SEED_MD: &str = r#"---
name: read-file
description: Read a UTF-8 file from disk and return its contents to the agent.
---
Read a file from disk.

Args (JSON):

```
{ "path": "crates/backend/src/main.rs" }
```

Behaviour:
- Relative paths resolve against the workspace root.
- Relative paths may not escape the workspace via `..`.
- Files larger than **1 MiB** are rejected.
- File contents are returned as the skill outcome `summary`.
"#;

pub const WRITE_FILE_SEED_MD: &str = r#"---
name: write-file
description: Write UTF-8 content to a file. Creates parent dirs; supports append.
---
Write text to a file.

Args (JSON):

```
{
  "path":    "notes/scratch.md",   // required, relative to workspace root or absolute
  "content": "hello, world\n",     // required
  "append":  false                  // optional, default false (overwrites)
}
```

Behaviour:
- Parent directories are created automatically.
- Relative paths may not escape the workspace via `..`.
- Returns the number of bytes written in the outcome summary.
"#;

pub struct RunCli;

#[async_trait]
impl Skill for RunCli {
    fn name(&self) -> &str { RUN_CLI_NAME }

    async fn run(&self, args: Value, _ctx: SkillContext) -> SkillOutcome {
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) if !c.is_empty() => c.to_string(),
            _ => return err("missing or empty `command` arg"),
        };
        let cwd = args.get("cwd").and_then(|v| v.as_str()).map(String::from);

        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", &command]);
            c
        } else {
            let mut c = Command::new("/bin/sh");
            c.args(["-c", &command]);
            c
        };
        if let Some(cwd) = &cwd {
            cmd.current_dir(cwd);
        }

        let output = match timeout(CLI_TIMEOUT, cmd.output()).await {
            Ok(Ok(o))  => o,
            Ok(Err(e)) => return err(&format!("spawn: {e}")),
            Err(_)     => return err(&format!("timeout after {}s", CLI_TIMEOUT.as_secs())),
        };

        let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();
        truncate(&mut stdout, MAX_OUTPUT);
        truncate(&mut stderr, MAX_OUTPUT);
        let code = output.status.code().unwrap_or(-1);
        SkillOutcome {
            ok: output.status.success(),
            summary: format!(
                "exit={code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
            ),
        }
    }
}

pub struct ReadFile {
    pub root: PathBuf,
}

impl ReadFile {
    pub fn new(root: PathBuf) -> Self { Self { root } }
}

#[async_trait]
impl Skill for ReadFile {
    fn name(&self) -> &str { READ_FILE_NAME }

    async fn run(&self, args: Value, _ctx: SkillContext) -> SkillOutcome {
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p,
            _ => return err("missing or empty `path` arg"),
        };
        let resolved = match resolve(&self.root, path) {
            Ok(p)  => p,
            Err(e) => return err(&e),
        };
        let meta = match fs::metadata(&resolved) {
            Ok(m)  => m,
            Err(e) => return err(&format!("stat {}: {e}", resolved.display())),
        };
        if meta.len() > MAX_FILE {
            return err(&format!(
                "{} too large ({} bytes, max {MAX_FILE})",
                resolved.display(),
                meta.len()
            ));
        }
        match fs::read_to_string(&resolved) {
            Ok(text) => SkillOutcome { ok: true, summary: text },
            Err(e)   => err(&format!("read {}: {e}", resolved.display())),
        }
    }
}

pub struct WriteFile {
    pub root: PathBuf,
}

impl WriteFile {
    pub fn new(root: PathBuf) -> Self { Self { root } }
}

#[async_trait]
impl Skill for WriteFile {
    fn name(&self) -> &str { WRITE_FILE_NAME }

    async fn run(&self, args: Value, _ctx: SkillContext) -> SkillOutcome {
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p,
            _ => return err("missing or empty `path` arg"),
        };
        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None    => return err("missing `content` arg"),
        };
        let append = args.get("append").and_then(|v| v.as_bool()).unwrap_or(false);
        let resolved = match resolve(&self.root, path) {
            Ok(p)  => p,
            Err(e) => return err(&e),
        };
        if let Some(parent) = resolved.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                return err(&format!("create dir {}: {e}", parent.display()));
            }
        }
        let result = if append {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&resolved)
                .and_then(|mut f| f.write_all(content.as_bytes()))
        } else {
            fs::write(&resolved, content.as_bytes())
        };
        match result {
            Ok(()) => SkillOutcome {
                ok: true,
                summary: format!("wrote {} bytes to {}", content.len(), resolved.display()),
            },
            Err(e) => err(&format!("write {}: {e}", resolved.display())),
        }
    }
}

/// Drop `skills/<name>.md` for each built-in skill if absent — the loader
/// reads them on startup so the LLM sees the contract alongside any user-
/// authored skills. Never clobbers a file the user already edited.
pub fn seed_defaults(skills_dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(skills_dir)?;
    for (name, body) in [
        (RUN_CLI_NAME,    RUN_CLI_SEED_MD),
        (READ_FILE_NAME,  READ_FILE_SEED_MD),
        (WRITE_FILE_NAME, WRITE_FILE_SEED_MD),
    ] {
        let path = skills_dir.join(format!("{name}.md"));
        if !path.exists() {
            fs::write(&path, body)?;
        }
    }
    Ok(())
}

fn resolve(root: &Path, path: &str) -> Result<PathBuf, String> {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return Ok(candidate.to_path_buf());
    }
    // Walk components to ensure `..` doesn't pop above the root.
    let mut depth: i32 = 0;
    for c in candidate.components() {
        use std::path::Component::*;
        match c {
            Normal(_) => depth += 1,
            ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(format!("path {path:?} escapes workspace via `..`"));
                }
            }
            _ => {}
        }
    }
    Ok(root.join(candidate))
}

fn truncate(s: &mut String, limit: usize) {
    if s.len() > limit {
        let mut end = limit;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push_str("\n…[truncated]");
    }
}

fn err(msg: &str) -> SkillOutcome {
    SkillOutcome { ok: false, summary: msg.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "sica-builtins-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn ctx() -> SkillContext {
        let sink: std::sync::Arc<dyn crate::agent::EventSink> =
            std::sync::Arc::new(NullSink);
        SkillContext { sub: crate::ToolSubAgent::root(sink) }
    }

    struct NullSink;
    impl crate::agent::EventSink for NullSink {
        fn emit(&self, _ev: protocol::Event) {}
    }

    #[test]
    fn resolve_blocks_parent_traversal() {
        let root = PathBuf::from("/work");
        assert!(resolve(&root, "../../etc/passwd").is_err());
        assert!(resolve(&root, "a/../b").is_ok());
        assert!(resolve(&root, "a/../../b").is_err());
    }

    #[tokio::test]
    async fn read_write_roundtrip() {
        let dir = tempdir();
        let w = WriteFile::new(dir.clone());
        let r = ReadFile::new(dir.clone());

        let out = w.run(json!({ "path": "hello.txt", "content": "hi" }), ctx()).await;
        assert!(out.ok, "write failed: {}", out.summary);

        let out = r.run(json!({ "path": "hello.txt" }), ctx()).await;
        assert!(out.ok);
        assert_eq!(out.summary, "hi");
    }

    #[tokio::test]
    async fn write_appends() {
        let dir = tempdir();
        let w = WriteFile::new(dir.clone());
        w.run(json!({ "path": "log.txt", "content": "a" }), ctx()).await;
        w.run(json!({ "path": "log.txt", "content": "b", "append": true }), ctx()).await;
        let text = std::fs::read_to_string(dir.join("log.txt")).unwrap();
        assert_eq!(text, "ab");
    }

    #[tokio::test]
    async fn read_missing_file_fails_cleanly() {
        let dir = tempdir();
        let r = ReadFile::new(dir);
        let out = r.run(json!({ "path": "nope.txt" }), ctx()).await;
        assert!(!out.ok);
        assert!(out.summary.contains("stat"));
    }

    #[tokio::test]
    async fn cli_echoes() {
        let out = RunCli.run(
            json!({ "command": if cfg!(windows) { "echo hi" } else { "echo hi" } }),
            ctx(),
        ).await;
        assert!(out.ok, "cli failed: {}", out.summary);
        assert!(out.summary.contains("hi"));
    }

    #[test]
    fn seed_defaults_writes_files_once() {
        let dir = tempdir();
        seed_defaults(&dir).unwrap();
        for name in [RUN_CLI_NAME, READ_FILE_NAME, WRITE_FILE_NAME] {
            let p = dir.join(format!("{name}.md"));
            assert!(p.exists(), "expected {}", p.display());
        }
        // Tamper, re-seed: file must not be clobbered.
        let path = dir.join(format!("{RUN_CLI_NAME}.md"));
        std::fs::write(&path, "edited").unwrap();
        seed_defaults(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited");
    }
}
