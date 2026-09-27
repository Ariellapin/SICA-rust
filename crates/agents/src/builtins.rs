//! Built-in skills implemented in Rust: `run-cli`, `run-pwsh`, `read-file`,
//! `write-file`, `edit-file`, `glob`, `grep`.
//!
//! Each skill has a companion `skills/<name>.md` describing the contract for
//! the LLM (seeded on first BE start, see `seed_defaults`). The Rust impl
//! below is what actually runs when the skill is invoked through a
//! `ToolSubAgent`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::timeout;

use crate::skill::{Concurrency, Skill, SkillContext, SkillOutcome};

const CLI_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT: usize = 32 * 1024;
const MAX_FILE: u64 = 1024 * 1024;
/// `glob` returns at most this many paths (newest first).
const MAX_GLOB: usize = 100;
/// `grep` reports at most this many matches inline.
const MAX_GREP_MATCHES: usize = 250;

pub const RUN_CLI_NAME:    &str = "run-cli";
pub const RUN_PWSH_NAME:   &str = "run-pwsh";
pub const READ_FILE_NAME:  &str = "read-file";
pub const WRITE_FILE_NAME: &str = "write-file";
pub const EDIT_FILE_NAME:  &str = "edit-file";
pub const GLOB_NAME:       &str = "glob";
pub const GREP_NAME:       &str = "grep";

pub const RUN_CLI_DESCRIPTION: &str =
    "Execute a shell command. Positional args: <command>. Optional named \
     args: cwd, background (`true` starts it as a background job and \
     returns a job id instead of waiting). Returns stdout/stderr/exit_code; \
     capped to 32 KiB output, 30 s timeout in the foreground.";

pub const RUN_PWSH_DESCRIPTION: &str =
    "Execute a PowerShell command (preferred on Windows). \
     Positional args: <command>. Optional named args: cwd, background \
     (`true` starts it as a background job and returns a job id instead of \
     waiting). Returns stdout/stderr/exit_code; capped to 32 KiB output, \
     30 s timeout in the foreground.";

pub const READ_FILE_DESCRIPTION: &str =
    "Read a UTF-8 file with line numbers. Positional args: <path>. \
     Optional named args: start, end (1-based line range). \
     Relative paths resolve against the working directory; up to 1 MiB.";

pub const WRITE_FILE_DESCRIPTION: &str =
    "Write UTF-8 content to a file. Positional args: <path> <content>. \
     Optional named args: append (`true` adds to the end of the file — \
     write a large file in parts). Creates parent dirs; refuses `..` \
     traversal in relative paths.";

pub const EDIT_FILE_DESCRIPTION: &str =
    "Replace exact text in a file. Positional args: <path> <old> <new>. \
     <old> must occur exactly once; include enough surrounding lines to make \
     it unique. Returns the changed region with line numbers.";

pub const GLOB_DESCRIPTION: &str =
    "List files matching a glob pattern (gitignore-aware). \
     Positional args: <pattern> (e.g. 'src/**/*.rs'). \
     Returns up to 100 paths, most recently modified first.";

pub const GREP_DESCRIPTION: &str =
    "Regex search across files. Positional args: <regex> <path> — path is a \
     file or a directory (searched recursively, gitignore-aware). \
     Returns `path:line: text` rows, capped at 250 matches.";

/// One-sentence shell guidance composed into the system prompt (the
/// `SKILL_GUIDANCE` slot) — the "base every claim on a tool result" rule
/// lives with the tools it concerns, not in a central persona blob.
pub const SHELL_PROMPT_GUIDANCE: &str =
    "Check the `exit=N` marker at the top of every run-cli / run-pwsh \
     result — a non-zero exit means the command failed even when its output \
     looks plausible. Base every claim about the host system (installed \
     tools, file contents, command output) on an actual tool result from \
     this conversation, never on assumption.";

pub const RUN_CLI_SEED_MD: &str = r#"---
name: run-cli
description: Execute a shell command on the host (cmd.exe on Windows, /bin/sh elsewhere).
---
Run a shell command on the host. stdout and stderr are captured and returned
to the agent.

Invocation (single line):

    run-cli '<command>' > <what you want to know from the output>

Example:

    run-cli 'cargo --version' > confirm cargo is installed and report the version

Behaviour:
- Windows: invokes `cmd /C <command>`. Other OSes: `/bin/sh -c <command>`.
- Stdout and stderr are each capped to **32 KiB** before being returned.
- A timeout of **30 seconds** kills the child and reports an error outcome.
- The outcome `ok` mirrors the child exit code (0 = ok), reported as the
  `exit=N` marker at the top of the result.
- Optional named arg `cwd` (JSON-fenced / native calls only): the directory
  to run in, relative to the working directory unless absolute. Without it
  the command runs in the working directory.

Use this for build tools, git, package managers, or one-shot scripts.

Background jobs — for anything longer than the 30 s foreground cap
(a build, a test suite, a watcher). Pass `background=true` and the call
returns a job id immediately instead of waiting:

    run-cli 'cargo build --workspace' 'background=true' > start the build

    started job `cli-3` in the background: cargo build --workspace

From then on three tools cover it, and they work for any job whatever
started it:

    job-list                       > what is running
    job-output 'cli-3'             > what the build printed so far
    job-kill 'cli-3'               > stop it

`job-output` returns everything printed **since your last read** and ends
with a `[status: …]` line, so polling it twice does not repeat output. You
do not have to poll: when a job ends, a notice naming it and its exit status
is put in front of you at your next step automatically.

Limits: 10 running jobs per session, 256 KiB of retained output per job
(a job that out-runs that says how much was dropped). Jobs belong to the
session that started them and die with the backend.

**Windows note:** if a command fails with `is not recognized as an internal
or external command` or `is not recognized as the name of a cmdlet`, the
shell can't find the executable. Retry the same command with `run-pwsh`,
which uses PowerShell and resolves PATH and aliases differently from
`cmd.exe`. The idealist daemon will also raise an improvement ticket
suggesting that swap automatically.
"#;

pub const RUN_PWSH_SEED_MD: &str = r#"---
name: run-pwsh
description: Execute a PowerShell command on the host. Preferred on Windows.
---
Run a command through PowerShell. Use this instead of `run-cli` when the host
is Windows and you need PowerShell-specific cmdlets, aliases, or PATH lookup
behaviour (notably: anything that fails under `cmd.exe` with
`is not recognized as an internal or external command`).

Invocation (single line):

    run-pwsh '<command>' > <what you want to know>

Example:

    run-pwsh 'Get-ChildItem | Measure-Object' > how many items in the cwd

Behaviour:
- Windows: invokes `powershell -NoLogo -NoProfile -NonInteractive -Command <command>`.
  Falls back to `pwsh` (PowerShell Core) if `powershell.exe` is missing.
- Non-Windows: invokes `pwsh -NoLogo -NoProfile -NonInteractive -Command <command>`.
- Stdout and stderr are each capped to **32 KiB** before being returned.
- A timeout of **30 seconds** kills the child and reports an error outcome.
- The outcome `ok` mirrors the child exit code (0 = ok).
- Optional named arg `cwd` (JSON-fenced / native calls only): the directory
  to run in, relative to the working directory unless absolute. Without it
  the command runs in the working directory.
- Optional named arg `background`: `true` starts the command as a
  background job and returns a job id instead of waiting. See `run-cli` for
  the full description; `job-list` / `job-output` / `job-kill` control it.
"#;

pub const READ_FILE_SEED_MD: &str = r#"---
name: read-file
description: Read a UTF-8 file from disk with line numbers, optionally a line range.
---
Read a file from disk. The output is line-numbered (`  <n>\t<line>`) so you
can quote exact lines to `edit-file`.

Invocation (single line):

    read-file '<path>' > <what you want to know from the file>

Examples:

    read-file 'skills/run-cli.md' > what positional args does run-cli accept
    read-file 'src/main.rs' 'start=1' 'end=80' > the first 80 lines

Behaviour:
- Relative paths resolve against the working directory.
- A `skills/<name>.md` the working directory does not have is read from the
  app's own `skills/` folder, so a skill's contract opens from any project.
- Relative paths may not escape the workspace via `..`.
- Files larger than **1 MiB** are rejected.
- Optional named args `start` / `end` (1-based, inclusive) select a line
  range, as `'start=N'` / `'end=M'` after the path.
- One call returns at most **48 KB** of numbered lines. A longer read stops
  at the last whole line that fits and ends with a
  `[read-file: output capped …]` line naming the `start` / `end` to read on.

- The raw contents are summarised by the sub-agent against the expectation
  text after `>` before being returned to the main agent.
"#;

pub const WRITE_FILE_SEED_MD: &str = r#"---
name: write-file
description: Write UTF-8 content to a file. Creates parent dirs.
---
Write text to a file.

Invocation (single line):

    write-file '<path>' '<content>' > <what you want confirmed>

Example:

    write-file 'notes/scratch.md' 'hello, world\n' > confirm bytes written

Behaviour:
- Parent directories are created automatically.
- Relative paths may not escape the workspace via `..`.
- Use `\n`, `\t`, `\\`, `\'`, `\"` escapes inside the quoted content to
  embed newlines or quote characters.
- Optional named arg `append`: `'append=true'` adds the content to the end
  of the file instead of replacing it. Write a large file in parts — one
  reply can only hold so much, and a call cut off mid-body never runs:

      write-file 'out.txt' '<first 20 lines>' > confirm the first part
      write-file 'out.txt' '<next 20 lines>' 'append=true' > confirm the second part

- Returns the number of bytes written in the outcome summary.
"#;

pub const EDIT_FILE_SEED_MD: &str = r#"---
name: edit-file
description: Replace an exact block of text in a file. The block must match exactly once.
---
Replace one exact stretch of text in an existing file.

Invocation (JSON-fenced form preferred — the arguments contain newlines):

    ```tool_call
    { "skill": "edit-file", "args": { "path": "src/main.rs", "old": "let x = 1;", "new": "let x = 2;" }, "expectation": "confirm the replacement" }
    ```

Behaviour:
- `old` must match the file **exactly once**, byte for byte (indentation and
  newlines included). `read-file` first — its line-numbered output tells you
  the exact text.
- 0 matches or more than 1 match is an error; include more surrounding lines
  in `old` to make it unique.
- The result shows the changed region with line numbers so you can verify it.
"#;

pub const GLOB_SEED_MD: &str = r#"---
name: glob
description: List files matching a glob pattern, most recently modified first.
---
Find files by glob pattern. Honours `.gitignore`.

Invocation (single line):

    glob '<pattern>' > <what you want to find>

Examples:

    glob 'src/**/*.rs' > every Rust source file
    glob '**/*.toml' > all TOML files

Behaviour:
- `**` matches any number of directories; `*` matches within one path
  segment. Patterns are relative to the working directory.
- A `skills/…` pattern that matches nothing there lists the app's own skill
  docs instead, as absolute paths.
- Returns at most **100** paths, most recently modified first, one per line.
- Use `grep` to search file *contents*.
"#;

pub const GREP_SEED_MD: &str = r#"---
name: grep
description: Regex search across files; returns path:line: text rows.
---
Search file contents with a regular expression. Honours `.gitignore`.

Invocation (single line):

    grep '<regex>' '<path>' > <what you want to find>

Examples:

    grep 'fn main' '.' > where is main defined
    grep 'TODO|FIXME' 'src' > outstanding work markers

Behaviour:
- `path` is a file or a directory (searched recursively).
- Output rows are `path:line: text`; at most **250** matches are returned.
- Non-UTF-8 (binary) files are skipped silently.
- Uses Rust `regex` syntax — no backreferences, no look-around.
"#;

/// `run-cli`. The optional job registry is what makes
/// `'background=true'` possible; `None` (tests, the catalogue probe) simply
/// means background is unavailable.
pub struct RunCli(pub Option<Arc<crate::jobs::JobRegistry>>);

#[async_trait]
impl Skill for RunCli {
    fn name(&self) -> &str { RUN_CLI_NAME }
    fn description(&self) -> &str { RUN_CLI_DESCRIPTION }
    fn positional_args(&self) -> Vec<String> { vec!["command".into()] }
    fn optional_args(&self) -> Vec<String> {
        vec!["cwd".into(), "background".into(), "timeout_secs".into()]
    }
    fn prompt_guidance(&self) -> Option<&'static str> { Some(SHELL_PROMPT_GUIDANCE) }
    fn concurrency(&self, args: &Value) -> Concurrency {
        let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        if crate::pipeline::is_read_only_command(cmd) {
            Concurrency::Parallel
        } else {
            Concurrency::Exclusive
        }
    }

    async fn run(&self, args: Value, ctx: SkillContext) -> SkillOutcome {
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) if !c.is_empty() => c.to_string(),
            _ => return err("missing or empty `command` arg"),
        };
        let cwd = shell_cwd(&args, &ctx);

        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", &command]);
            c
        } else {
            let mut c = Command::new("/bin/sh");
            c.args(["-c", &command]);
            c
        };
        cmd.current_dir(&cwd);
        if wants_background(&args) {
            return start_background(&self.0, "cli", &command, cmd, &ctx);
        }
        run_shell(cmd, "cmd", &ctx, shell_timeout(&args, self.timeout())).await
    }
}

/// Directory a shell call runs in: the `cwd` arg when it has one (a relative
/// path resolves against the working directory), otherwise the working
/// directory itself. Without this the command would inherit the backend
/// process's own cwd — wherever the frontend happened to be launched from —
/// rather than the folder the user pointed the agent at.
fn shell_cwd(args: &Value, ctx: &SkillContext) -> PathBuf {
    let root = call_root(None, ctx);
    match args
        .get("cwd")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        Some(c) if Path::new(c).is_absolute() => PathBuf::from(c),
        Some(c) => root.join(c),
        None => root,
    }
}

/// `background` arrives as a JSON bool from a native call and as a string
/// from the text protocol (`'background=true'`). Accept both, and treat
/// anything unrecognised as "no" — running in the foreground is the safe
/// misreading, since the caller still gets its output.
fn wants_background(args: &Value) -> bool {
    crate::control::flag_arg(args.get("background"))
}

/// Start `cmd` under the session's job registry instead of waiting for it.
///
/// Refuses rather than falling back to a foreground run: a caller that asked
/// for background wants a command longer than the 30 s foreground cap, so
/// running it in the foreground would just time out and lose the work.
fn start_background(
    jobs: &Option<Arc<crate::jobs::JobRegistry>>,
    kind: &str,
    command: &str,
    cmd: Command,
    ctx: &SkillContext,
) -> SkillOutcome {
    let Some(jobs) = jobs else {
        return err("background jobs are not available in this context");
    };
    let Some(session_id) = ctx.sub.session_id else {
        return err("background jobs are per session and this call has none");
    };
    match jobs.start_shell(session_id, kind, command, cmd) {
        Ok(id) => SkillOutcome {
            ok:      true,
            summary: format!(
                "started job `{id}` in the background: {command}\n\
                 It is still running. Read what it prints with \
                 `{} '{id}' > what it printed`, list jobs with `{}`, stop it \
                 with `{} '{id}'`. You will be told when it finishes.",
                crate::jobs::JOB_OUTPUT_NAME,
                crate::jobs::JOB_LIST_NAME,
                crate::jobs::JOB_KILL_NAME,
            ),
        },
        Err(e) => err(&e),
    }
}

/// The foreground budget for one shell call: the default cap, or the
/// call's own `timeout_secs` (guide §6.3), clamped to the skill's pipeline
/// timeout so a call can never outlive the wrapper that would kill it.
fn shell_timeout(args: &Value, skill_timeout: Duration) -> Duration {
    let asked = match args.get("timeout_secs") {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.trim().parse::<u64>().ok(),
        _ => None,
    };
    match asked {
        Some(secs) if secs > 0 => Duration::from_secs(secs).min(skill_timeout),
        _ => CLI_TIMEOUT,
    }
}

/// Spawn a prepared shell command, cap its streams, and report
/// `exit=N` + stdout + stderr. Shared by `run-cli` and `run-pwsh`.
///
/// Process management: the child is `kill_on_drop` (a timeout or an
/// `InterruptTurn` drops the future) *and*, on Windows, placed in a
/// kill-on-close Job Object so the processes *it* started die with it —
/// without that a timed-out `cmd /C npm install` leaves `node` running.
/// `SICA_SESSION_ID` is set for scripts that want to know their caller.
async fn run_shell(
    mut cmd: Command,
    exe: &str,
    ctx: &SkillContext,
    limit: Duration,
) -> SkillOutcome {
    if let Some(session) = &ctx.sub.spill_label {
        cmd.env("SICA_SESSION_ID", session);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::null());
    cmd.kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return err(&format!("spawn {exe}: {e}")),
    };
    // Held until the child has finished: dropping it closes the job.
    let _job = crate::proc::JobGuard::attach(&child);

    let output = match timeout(limit, child.wait_with_output()).await {
        Ok(Ok(o))  => o,
        Ok(Err(e)) => return err(&format!("wait {exe}: {e}")),
        Err(_)     => return err(&format!("timeout after {}s", limit.as_secs())),
    };

    let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();
    // Over the cap the stream is kept whole on disk (guide §6.9) and the
    // model gets the head plus a pointer — the same spill seam every other
    // oversized output goes through, instead of the tail being discarded.
    cap_stream(&mut stdout, "stdout", exe, ctx);
    cap_stream(&mut stderr, "stderr", exe, ctx);
    let code = output.status.code().unwrap_or(-1);
    SkillOutcome {
        ok: output.status.success(),
        summary: format!(
            "exit={code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        ),
    }
}

/// `run-pwsh`. See [`RunCli`] for the job registry.
pub struct RunPwsh(pub Option<Arc<crate::jobs::JobRegistry>>);

#[async_trait]
impl Skill for RunPwsh {
    fn name(&self) -> &str { RUN_PWSH_NAME }
    fn description(&self) -> &str { RUN_PWSH_DESCRIPTION }
    fn positional_args(&self) -> Vec<String> { vec!["command".into()] }
    fn optional_args(&self) -> Vec<String> {
        vec!["cwd".into(), "background".into(), "timeout_secs".into()]
    }
    fn prompt_guidance(&self) -> Option<&'static str> { Some(SHELL_PROMPT_GUIDANCE) }
    fn concurrency(&self, args: &Value) -> Concurrency {
        let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        if crate::pipeline::is_read_only_command(cmd) {
            Concurrency::Parallel
        } else {
            Concurrency::Exclusive
        }
    }

    async fn run(&self, args: Value, ctx: SkillContext) -> SkillOutcome {
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) if !c.is_empty() => c.to_string(),
            _ => return err("missing or empty `command` arg"),
        };
        let cwd = shell_cwd(&args, &ctx);

        // On Windows prefer the system `powershell.exe`; everywhere else
        // (including the rare case where `powershell.exe` is missing on
        // Windows) reach for `pwsh` (PowerShell Core).
        let exe = if cfg!(windows) {
            if which_in_path("powershell.exe").is_some() {
                "powershell"
            } else {
                "pwsh"
            }
        } else {
            "pwsh"
        };

        let mut cmd = Command::new(exe);
        cmd.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &command,
        ]);
        cmd.current_dir(&cwd);
        if wants_background(&args) {
            return start_background(&self.0, "pwsh", &command, cmd, &ctx);
        }
        run_shell(cmd, exe, &ctx, shell_timeout(&args, self.timeout())).await
    }
}

/// Minimal PATH lookup used only by `RunPwsh` so we can fall back to `pwsh`
/// when `powershell.exe` is absent. We deliberately avoid pulling in an
/// extra crate for this — the agents crate stays leaf-level.
fn which_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub struct ReadFile {
    pub root: PathBuf,
    /// The app's own skill folder, where a `skills/…` path the working
    /// directory has no file for is read from ([`resolve_read`]).
    pub skills: PathBuf,
}

impl ReadFile {
    pub fn new(root: PathBuf) -> Self { Self { root, skills: sica_core::paths::skills_dir() } }

    /// Look up `skills/…` fallbacks in `dir` instead of the app's folder.
    pub fn with_skills_dir(mut self, dir: PathBuf) -> Self {
        self.skills = dir;
        self
    }
}

#[async_trait]
impl Skill for ReadFile {
    fn name(&self) -> &str { READ_FILE_NAME }
    fn description(&self) -> &str { READ_FILE_DESCRIPTION }
    fn positional_args(&self) -> Vec<String> { vec!["path".into()] }
    fn optional_args(&self) -> Vec<String> { vec!["start".into(), "end".into()] }
    fn concurrency(&self, _args: &Value) -> Concurrency {
        Concurrency::Parallel
    }

    async fn run(&self, args: Value, ctx: SkillContext) -> SkillOutcome {
        let root = call_root(Some(&self.root), &ctx);
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p,
            _ => return err("missing or empty `path` arg"),
        };
        let (resolved, app_doc) = match resolve_read(&root, &self.skills, path) {
            Ok(p)  => p,
            Err(e) => return err(&e),
        };
        let meta = match fs::metadata(&resolved) {
            Ok(m)  => m,
            // "Workspace root" is what this used to say, and it is the app's
            // folder, not the one relative paths resolve against.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return err(&format!(
                    "no such file: {} — relative paths resolve against the working \
                     directory; find the file with `{GLOB_NAME}`, or give its absolute path",
                    resolved.display()
                ))
            }
            Err(e) => return err(&format!("stat {}: {e}", resolved.display())),
        };
        if meta.len() > MAX_FILE {
            return err(&format!(
                "{} too large ({} bytes, max {MAX_FILE})",
                resolved.display(),
                meta.len()
            ));
        }
        let text = match fs::read_to_string(&resolved) {
            Ok(t)  => t,
            Err(e) => return err(&format!("read {}: {e}", resolved.display())),
        };

        // Optional 1-based inclusive line range. Accepted as numbers or
        // numeric strings (the text protocol only carries strings).
        let start = parse_line_arg(args.get("start"));
        let end = parse_line_arg(args.get("end"));
        let page = numbered_page(&text, start, end, path, READ_FILE_MAX_BYTES);
        SkillOutcome {
            ok:      true,
            // The model asked for a file in its working directory and is
            // reading one from somewhere else: say where.
            summary: if app_doc {
                format!("[read from the app's skill folder: {}]\n{page}", resolved.display())
            } else {
                page
            },
        }
    }
}

/// Most numbered text one `read-file` returns, in bytes. `read-file` is
/// exempt from spilling (a spill file is read back with it), so without a
/// cap a 325 KB wildcard file reached the model whole — 80 K tokens in a
/// 64 K window — the summariser could not fit it either, and the turn died
/// on the overflow (`sessions/95`). Over the cap the result stops at the
/// last whole line that fits and says how to read on.
pub const READ_FILE_MAX_BYTES: usize = 48 * 1024;

/// How the line that ends a capped read begins. The sub-agent keeps that
/// line when it summarises the result: the way to the rest of the file has
/// to survive the paraphrase.
pub const READ_FILE_CAP_MARK: &str = "[read-file: output capped";

/// [`numbered_range`], cut to `cap` bytes at a line boundary. A cut read
/// opens with the range it shows and ends with a [`READ_FILE_CAP_MARK`]
/// line naming the next call to make; a single line longer than the cap is
/// shown up to the cap and says so.
fn numbered_page(text: &str, start: Option<usize>, end: Option<usize>, path: &str, cap: usize) -> String {
    let full = numbered_range(text, start, end);
    if full.len() <= cap {
        return full;
    }
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    // `numbered_range` returns early (and short) past the end, so the
    // range here is never empty.
    let from = start.unwrap_or(1).max(1);
    let to = end.unwrap_or(total).min(total);
    let quoted = path.replace('\'', "\\'");
    let mut body = String::new();
    let mut last = from - 1;
    for n in from..=to {
        let row = format!("{n:>5}\t{}\n", lines[n - 1]);
        if body.len() + row.len() > cap {
            break;
        }
        body.push_str(&row);
        last = n;
    }
    if last < from {
        // One line alone is over the cap: its head is all there is room for.
        let row = format!("{from:>5}\t{}", lines[from - 1]);
        let head = sica_core::retain::utf8_head(&row, cap);
        let mut out = format!("[line {from} of {total}]\n{head}\n");
        out.push_str(&format!(
            "{READ_FILE_CAP_MARK} at {} KB — line {from} is {} bytes long and only its \
             first {} are shown; search it with grep or run-pwsh instead",
            cap / 1024,
            lines[from - 1].len(),
            head.len(),
        ));
        if from < to {
            out.push_str(&format!(
                "; the next lines start with read-file '{quoted}' 'start={}'",
                from + 1
            ));
        }
        out.push(']');
        return out;
    }
    let next = last + 1;
    let next_end = (next + (last - from)).min(to);
    format!(
        "[lines {from}-{last} of {total}]\n{body}{READ_FILE_CAP_MARK} at {} KB — lines \
         {next}-{to} are not shown; read on with read-file '{quoted}' 'start={next}' \
         'end={next_end}']",
        cap / 1024,
    )
}

fn parse_line_arg(v: Option<&Value>) -> Option<usize> {
    v.and_then(|v| {
        v.as_u64()
            .map(|n| n as usize)
            .or_else(|| v.as_str().and_then(|s| s.trim().parse::<usize>().ok()))
    })
    .filter(|&n| n >= 1)
}

/// Line-numbered output (`{n:>5}\t{line}`) over an optional 1-based
/// inclusive range — the shape that makes `edit-file` targeting reliable.
/// Out-of-range bounds clamp; an inverted or empty range says so instead of
/// returning nothing silently.
fn numbered_range(text: &str, start: Option<usize>, end: Option<usize>) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let (from, to) = match (start, end) {
        (None, None) => (1, total),
        (s, e) => {
            let from = s.unwrap_or(1).max(1);
            let to = e.unwrap_or(total).min(total);
            (from, to)
        }
    };
    let mut out = String::new();
    if start.is_some() || end.is_some() {
        out.push_str(&format!("[lines {from}-{to} of {total}]\n"));
    }
    if from > total {
        out.push_str(&format!("[line {from} is past the end of the file ({total} lines)]"));
        return out;
    }
    if from > to {
        out.push_str(&format!("[the range is empty: start {from} is after end {to}]"));
        return out;
    }
    for (i, line) in lines[(from - 1)..to].iter().enumerate() {
        out.push_str(&format!("{:>5}\t{}\n", from + i, line));
    }
    out.pop(); // trailing newline
    out
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
    fn description(&self) -> &str { WRITE_FILE_DESCRIPTION }
    fn positional_args(&self) -> Vec<String> { vec!["path".into(), "content".into()] }
    fn optional_args(&self) -> Vec<String> { vec!["append".into()] }

    async fn run(&self, args: Value, ctx: SkillContext) -> SkillOutcome {
        let root = call_root(Some(&self.root), &ctx);
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p,
            _ => return err("missing or empty `path` arg"),
        };
        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None    => return err("missing `content` arg"),
        };
        // A bool from a JSON call, `'append=true'` from the text protocol.
        // Appending is how a file too big for one reply gets written: a
        // single 300 KB `write-file` runs past the completion cap, and the
        // truncated call never runs at all (`sessions/92`, `sessions/93`).
        let append = crate::control::flag_arg(args.get("append"));
        let resolved = match resolve(&root, path) {
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
                summary: format!(
                    "{} {} bytes to {}",
                    if append { "appended" } else { "wrote" },
                    content.len(),
                    resolved.display()
                ),
            },
            Err(e) => err(&format!("write {}: {e}", resolved.display())),
        }
    }
}

pub struct EditFile {
    pub root: PathBuf,
}

impl EditFile {
    pub fn new(root: PathBuf) -> Self { Self { root } }
}

#[async_trait]
impl Skill for EditFile {
    fn name(&self) -> &str { EDIT_FILE_NAME }
    fn description(&self) -> &str { EDIT_FILE_DESCRIPTION }
    fn positional_args(&self) -> Vec<String> {
        vec!["path".into(), "old".into(), "new".into()]
    }

    async fn run(&self, args: Value, ctx: SkillContext) -> SkillOutcome {
        let root = call_root(Some(&self.root), &ctx);
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p,
            _ => return err("missing or empty `path` arg"),
        };
        let old = match args.get("old").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => return err("missing or empty `old` arg"),
        };
        let new = match args.get("new").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None    => return err("missing `new` arg"),
        };
        let resolved = match resolve(&root, path) {
            Ok(p)  => p,
            Err(e) => return err(&e),
        };
        let text = match fs::read_to_string(&resolved) {
            Ok(t)  => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return err(&format!(
                    "no such file: {} — edit-file only edits existing files; use write-file to create one",
                    resolved.display()
                ))
            }
            Err(e) => {
                return err(&format!(
                    "read {}: {e} — edit-file only edits existing files; use write-file to create one",
                    resolved.display()
                ))
            }
        };

        // Literal, non-overlapping match count. Exactly one, or the model
        // gets a specific reason: 0 means the file moved under it, >1 means
        // the anchor is ambiguous.
        let count = text.match_indices(&old).count();
        match count {
            0 => {
                return err(
                    "old text not found — the file may have changed since you read it; \
                     read it again and copy the exact text (whitespace included)",
                )
            }
            n if n > 1 => {
                return err(&format!(
                    "old text matches {n} times — include more surrounding lines to make it unique"
                ))
            }
            _ => {}
        }

        let at = text.find(&old).expect("counted one match");
        let updated = format!("{}{}{}", &text[..at], new, &text[at + old.len()..]);
        if let Err(e) = fs::write(&resolved, updated.as_bytes()) {
            return err(&format!("write {}: {e}", resolved.display()));
        }

        // Show the changed region with line numbers (three lines of context
        // either side) so the model can verify without another read. The
        // replacement starts on the same line number in the updated file.
        let before_lines = text[..at].matches('\n').count() + 1;
        let old_lines = old.matches('\n').count() + 1;
        let new_lines = new.matches('\n').count() + 1;
        let end_line = before_lines + old_lines.max(new_lines) - 1;
        let from = before_lines.saturating_sub(3).max(1);
        let to = end_line + 3;
        SkillOutcome {
            ok: true,
            summary: format!(
                "replaced {} bytes with {} bytes in {}\n{}",
                old.len(),
                new.len(),
                resolved.display(),
                numbered_range(&updated, Some(from), Some(to)),
            ),
        }
    }
}

pub struct Glob {
    pub root: PathBuf,
    /// The app's own skill folder, searched when a `skills/…` pattern
    /// matches nothing in the working directory.
    pub skills: PathBuf,
}

impl Glob {
    pub fn new(root: PathBuf) -> Self { Self { root, skills: sica_core::paths::skills_dir() } }

    /// Look up `skills/…` fallbacks in `dir` instead of the app's folder.
    pub fn with_skills_dir(mut self, dir: PathBuf) -> Self {
        self.skills = dir;
        self
    }
}

/// Files under `from` matching `pattern`, which is anchored at `base` the
/// way a `.gitignore` line is.
fn glob_walk(base: &Path, from: &Path, pattern: &str) -> Result<Vec<(std::time::SystemTime, PathBuf)>, String> {
    // gitignore-style overrides give us `**` semantics without a second
    // glob dialect — and the walker already honours .gitignore.
    let overrides = ignore::overrides::OverrideBuilder::new(base)
        .add(pattern)
        .and_then(|b| b.build())
        .map_err(|e| format!("bad glob pattern {pattern:?}: {e}"))?;

    let mut hits: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in ignore::WalkBuilder::new(from).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        // Override semantics are include-flavoured: a path matching the
        // pattern reports `Whitelist`, everything else `Ignore`.
        match overrides.matched(entry.path(), false) {
            ignore::Match::Whitelist(_) => {
                hits.push((
                    entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(std::time::UNIX_EPOCH),
                    entry.path().to_path_buf(),
                ));
            }
            ignore::Match::Ignore(_) | ignore::Match::None => {}
        }
    }
    Ok(hits)
}

#[async_trait]
impl Skill for Glob {
    fn name(&self) -> &str { GLOB_NAME }
    fn description(&self) -> &str { GLOB_DESCRIPTION }
    fn positional_args(&self) -> Vec<String> { vec!["pattern".into()] }

    async fn run(&self, args: Value, ctx: SkillContext) -> SkillOutcome {
        let root = call_root(Some(&self.root), &ctx);
        let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p,
            _ => return err("missing or empty `pattern` arg"),
        };
        let mut hits = match glob_walk(&root, &root, pattern) {
            Ok(h)  => h,
            Err(e) => return err(&e),
        };
        // A `skills/…` pattern with nothing to show for it here is after the
        // app's skill docs, which live in the app's folder — the same
        // fallback `read-file` makes (`resolve_read`). The hits print as
        // absolute paths, since they are not under the working directory.
        let mut from_app = false;
        if hits.is_empty() && names_skill_docs(pattern) {
            if let Some(app) = self.skills.parent() {
                if let Ok(found) = glob_walk(app, &self.skills, pattern) {
                    from_app = !found.is_empty();
                    hits = found;
                }
            }
        }
        hits.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
        let total = hits.len();
        hits.truncate(MAX_GLOB);
        if hits.is_empty() {
            return SkillOutcome {
                ok: true,
                summary: format!("no files match {pattern:?}"),
            };
        }
        let mut out = String::new();
        if from_app {
            out.push_str(&format!(
                "[nothing in the working directory matches — these are the app's own skill docs in {}]\n",
                self.skills.display()
            ));
        }
        if total > hits.len() {
            out.push_str(&format!(
                "[showing the {} most recently modified of {total} matches]\n",
                hits.len()
            ));
        }
        for (_, p) in &hits {
            out.push_str(&display_relative(&root, p));
            out.push('\n');
        }
        out.pop();
        SkillOutcome { ok: true, summary: out }
    }
}

pub struct Grep {
    pub root: PathBuf,
    /// The app's own skill folder, searched for a `skills/…` path the
    /// working directory does not have ([`resolve_read`]).
    pub skills: PathBuf,
}

impl Grep {
    pub fn new(root: PathBuf) -> Self { Self { root, skills: sica_core::paths::skills_dir() } }

    /// Look up `skills/…` fallbacks in `dir` instead of the app's folder.
    pub fn with_skills_dir(mut self, dir: PathBuf) -> Self {
        self.skills = dir;
        self
    }
}

#[async_trait]
impl Skill for Grep {
    fn name(&self) -> &str { GREP_NAME }
    fn description(&self) -> &str { GREP_DESCRIPTION }
    fn positional_args(&self) -> Vec<String> { vec!["pattern".into(), "path".into()] }

    async fn run(&self, args: Value, ctx: SkillContext) -> SkillOutcome {
        let root = call_root(Some(&self.root), &ctx);
        let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p,
            _ => return err("missing or empty `pattern` arg"),
        };
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(".");
        let re = match regex::Regex::new(pattern) {
            Ok(r)  => r,
            Err(e) => return err(&format!("bad regex {pattern:?}: {e}")),
        };
        let (start, _) = match resolve_read(&root, &self.skills, path) {
            Ok(p)  => p,
            Err(e) => return err(&e),
        };
        if !start.exists() {
            return err(&format!("no such path: {}", start.display()));
        }

        let mut rows: Vec<String> = Vec::new();
        // Matches past the cap, kept for the spill file (never for the
        // model). Bounded too: a pattern matching every line of a tree
        // would otherwise hold the tree in memory.
        let mut overflow: Vec<String> = Vec::new();
        const OVERFLOW_CAP: usize = 20_000;
        let mut total = 0usize;
        let mut capped = false;

        let mut handle_file = |file: &Path| {
            let Ok(text) = fs::read_to_string(file) else { return }; // binary → skip
            for (i, line) in text.lines().enumerate() {
                if !re.is_match(line) {
                    continue;
                }
                total += 1;
                let row = format!("{}:{}: {}", display_relative(&root, file), i + 1, line);
                if rows.len() < MAX_GREP_MATCHES {
                    rows.push(row);
                } else {
                    capped = true;
                    if overflow.len() < OVERFLOW_CAP {
                        overflow.push(row);
                    }
                }
            }
        };

        if start.is_file() {
            handle_file(&start);
        } else {
            for entry in ignore::WalkBuilder::new(&start).build().flatten() {
                if entry.file_type().is_some_and(|t| t.is_file()) {
                    handle_file(entry.path());
                }
            }
        }

        if rows.is_empty() {
            return SkillOutcome {
                ok: true,
                summary: format!("no matches for {pattern:?} under {}", display_relative(&root, &start)),
            };
        }
        let mut out = rows.join("\n");
        if capped {
            // The rest is not lost: every match goes to the spill seam
            // (guide §6.9) and the marker names the file.
            let spilled = ctx.sub.spill_label.as_deref().and_then(|label| {
                crate::spill::write(
                    &sica_core::paths::spill_dir(),
                    label,
                    GREP_NAME,
                    ctx.sub.parent_id.unwrap_or(0),
                    &overflow.join("\n"),
                )
                .ok()
            });
            match spilled {
                Some(path) => out.push_str(&format!(
                    "\n[{total} matches total — showing the first {MAX_GREP_MATCHES}; \
                     all of them are in {} — read-file it, or narrow the pattern]",
                    path.display()
                )),
                None => out.push_str(&format!(
                    "\n[{total} matches total — showing the first {MAX_GREP_MATCHES}; \
                     narrow the pattern or the path]"
                )),
            }
        }
        SkillOutcome { ok: true, summary: out }
    }
}

/// Ask the human a question and wait for the answer (Wave 3, guide
/// §10.1). The model asks; the tool blocks until the FE answers, and the
/// answer returns as an ordinary tool result so no loop mechanics change.
/// A runtime-owned child (teammate) never sees this skill — it must put
/// the unresolved question in its final report instead.
///
/// `options` is an optional JSON array of suggested answers shown as
/// buttons; the human may always answer in free text. `detail` is the body
/// under the headline — the context the person needs before choosing — and
/// `multi=true` makes the options checkboxes, so several can be picked at
/// once. Whatever the shape, the answer comes back as one string.
pub struct AskUser;

#[async_trait]
impl Skill for AskUser {
    fn name(&self) -> &str {
        crate::control::ASK_USER_NAME
    }
    fn description(&self) -> &str {
        "Ask the user a question and wait for their answer. Use when blocked on a human decision — never guess. Optional named args: detail (context shown under the question), options (a JSON array of suggested answers), multi (`true` lets the user pick several options)."
    }
    fn positional_args(&self) -> Vec<String> {
        vec!["question".into()]
    }
    fn optional_args(&self) -> Vec<String> {
        vec!["detail".into(), "options".into(), "multi".into()]
    }
    fn prompt_guidance(&self) -> Option<&'static str> {
        Some("When blocked on a decision only the user can make, ask-user with a precise question instead of guessing.")
    }
    fn timeout(&self) -> Duration {
        // Covers the broker's question wait with room to spare.
        Duration::from_secs(15 * 60)
    }
    async fn run(&self, args: Value, ctx: SkillContext) -> SkillOutcome {
        let question = match args.get("question").and_then(|v| v.as_str()) {
            Some(q) if !q.trim().is_empty() => q.trim().to_string(),
            _ => return err("missing or empty `question` arg"),
        };
        // The schema declares `options` a string, but native models often
        // send the real array — `array_arg` takes either shape.
        let options: Vec<String> = args
            .get("options")
            .and_then(crate::control::array_arg)
            .map(|vs| {
                vs.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .take(8)
                    .collect()
            })
            .unwrap_or_default();
        let detail = args
            .get("detail")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string);
        // Checkboxes over one option are just a radio with extra steps, so
        // the flag only means anything once there are options to combine.
        let multi = crate::control::flag_arg(args.get("multi")) && options.len() > 1;
        let (Some(brokers), Some(session_id)) = (ctx.sub.brokers.clone(), ctx.sub.session_id)
        else {
            return err("cannot ask the user from this context (no broker) — \
                         include the unresolved question in your final report instead");
        };
        match brokers
            .ask_question(
                &ctx.sub.events,
                session_id,
                &question,
                detail.as_deref(),
                &options,
                multi,
                ctx.sub.cancel.clone(),
            )
            .await
        {
            Some(answer) if !answer.trim().is_empty() => SkillOutcome {
                ok: true,
                summary: format!("User answered: {}", answer.trim()),
            },
            _ => err("no answer — the question timed out or the turn was interrupted"),
        }
    }
}

/// Display a path relative to the working directory when it is under it —
/// relative paths are what the model should hand back to other skills.
fn display_relative(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .map(|r| r.display().to_string())
        .unwrap_or_else(|_| p.display().to_string())
}

/// Drop `skills/<name>.md` for each built-in skill if absent — the loader
/// reads them on startup so the LLM sees the contract alongside any user-
/// authored skills. Never clobbers a file the user already edited.
///
/// `agent-team` is deliberately **not** seeded here: its doc file doubles as
/// the on/off switch for the feature (see `team::AGENT_TEAM_SEED_MD` and the
/// registration in `backend::main`), so seeding it would turn the feature on
/// for everyone on first run.
pub fn seed_defaults(skills_dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(skills_dir)?;
    for (name, body) in [
        (RUN_CLI_NAME,    RUN_CLI_SEED_MD),
        (RUN_PWSH_NAME,   RUN_PWSH_SEED_MD),
        (READ_FILE_NAME,  READ_FILE_SEED_MD),
        (WRITE_FILE_NAME, WRITE_FILE_SEED_MD),
        (EDIT_FILE_NAME,  EDIT_FILE_SEED_MD),
        (GLOB_NAME,       GLOB_SEED_MD),
        (GREP_NAME,       GREP_SEED_MD),
        (crate::web::WEB_FETCH_NAME,  crate::web::WEB_FETCH_SEED_MD),
        (crate::web::WEB_SEARCH_NAME, crate::web::WEB_SEARCH_SEED_MD),
    ] {
        let path = skills_dir.join(format!("{name}.md"));
        if !path.exists() {
            fs::write(&path, body)?;
        }
    }
    Ok(())
}

/// Directory a call resolves relative paths against.
///
/// The session's own working directory wins (guide §3.9); `fallback` — the
/// root the skill was registered with — is what a call outside any session
/// gets, and `None` falls back to the process default. The registry is built
/// once at startup and shared by every session, so a skill that trusted only
/// its construction-time root would send every session to the same folder.
pub(crate) fn call_root(fallback: Option<&Path>, ctx: &SkillContext) -> PathBuf {
    ctx.cwd().unwrap_or_else(|| {
        fallback.map(Path::to_path_buf).unwrap_or_else(sica_core::paths::working_dir)
    })
}

pub(crate) fn resolve(root: &Path, path: &str) -> Result<PathBuf, String> {
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

/// First segment of the path every prompt gives for a skill's contract —
/// `read-file 'skills/<name>.md'` in `memory.md`, the `read-file` doc and
/// the idealist's tickets.
const SKILL_DOCS_DIR: &str = "skills";

/// Where a *read* of `path` lands, and whether that is the app's own skill
/// doc rather than a file in the working directory.
///
/// The working directory first, as for any call. A relative `skills/…`
/// path with nothing there is then looked up in the app's `skills/` folder:
/// the prompt says a skill's contract is at `skills/<name>.md`, which is
/// only true of the app's folder, and since sessions got folders of their
/// own (guide §3.9) the working directory is usually a project elsewhere —
/// a session opened on one could not open a single contract (`sessions/96`).
/// Reads only: a write to `skills/…` still lands in the working directory,
/// where the permission policy checks it.
pub(crate) fn resolve_read(root: &Path, skills: &Path, path: &str) -> Result<(PathBuf, bool), String> {
    let resolved = resolve(root, path)?;
    if !resolved.exists() {
        if let Some(doc) = skill_doc_path(skills, path) {
            return Ok((doc, true));
        }
    }
    Ok((resolved, false))
}

/// `path` inside the app's `skills` folder, when it is a relative
/// `skills/…` path naming something there. Every segment after the first
/// has to be a plain name, so `skills/../sica-settings/.env` is not a way
/// out of the folder.
fn skill_doc_path(skills: &Path, path: &str) -> Option<PathBuf> {
    use std::path::Component;
    let mut parts = Path::new(path).components().filter(|c| !matches!(c, Component::CurDir));
    match parts.next() {
        Some(Component::Normal(first))
            if first.to_str().is_some_and(|f| f.eq_ignore_ascii_case(SKILL_DOCS_DIR)) => {}
        _ => return None,
    }
    let mut doc = skills.to_path_buf();
    for part in parts {
        match part {
            Component::Normal(name) => doc.push(name),
            _ => return None,
        }
    }
    doc.exists().then_some(doc)
}

/// Whether a glob pattern is after the skill docs: its first segment is
/// `skills`.
fn names_skill_docs(pattern: &str) -> bool {
    let p = pattern.trim_start_matches("./").trim_start_matches('/');
    p.split(['/', '\\']).next().is_some_and(|s| s.eq_ignore_ascii_case(SKILL_DOCS_DIR))
}

/// Cap one output stream at `limit` bytes, keeping the head. The omission
/// notice uses the shared `retain` wording so the model reads the same
/// sentence here as on a spill digest or a pruned result.
/// Cap one shell stream at [`MAX_OUTPUT`]. In a session the full stream is
/// spilled first, so the marker names a file to `read-file`; outside one
/// (tests, the catalogue probe) it is plain truncation.
fn cap_stream(s: &mut String, stream: &str, exe: &str, ctx: &SkillContext) {
    if s.len() <= MAX_OUTPUT {
        return;
    }
    if let Some(label) = ctx.sub.spill_label.as_deref() {
        let skill = format!("{exe}-{stream}");
        let id = ctx.sub.parent_id.unwrap_or(0);
        if let Ok(path) = crate::spill::write(&sica_core::paths::spill_dir(), label, &skill, id, s) {
            let window = sica_core::retain::head_only(s, MAX_OUTPUT);
            let marker = sica_core::retain::notice(
                window.omitted,
                &format!(
                    "full {stream} ({} bytes) saved to {}; use read-file '{}' to inspect it",
                    s.len(),
                    path.display(),
                    path.display()
                ),
            );
            *s = window.render(&marker);
            return;
        }
    }
    truncate(s, MAX_OUTPUT);
}

fn truncate(s: &mut String, limit: usize) {
    let window = sica_core::retain::head_only(s, limit);
    if window.omitted != sica_core::retain::Omitted::None {
        let marker = sica_core::retain::notice(
            window.omitted,
            "the stream was capped; narrow the command or redirect to a file",
        );
        *s = window.render(&marker);
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
        assert_eq!(out.summary, "    1\thi");
    }

    #[tokio::test]
    async fn read_numbers_lines_and_slices_ranges() {
        let dir = tempdir();
        std::fs::write(dir.join("f.txt"), "a\nb\nc\nd\ne").unwrap();
        let r = ReadFile::new(dir.clone());
        let out = r.run(json!({ "path": "f.txt", "start": "2", "end": "4" }), ctx()).await;
        assert!(out.ok, "{}", out.summary);
        assert_eq!(out.summary, "[lines 2-4 of 5]\n    2\tb\n    3\tc\n    4\td");
        // Clamps rather than erroring.
        let out = r.run(json!({ "path": "f.txt", "start": 4, "end": 99 }), ctx()).await;
        assert!(out.summary.ends_with("    5\te"), "{}", out.summary);
        // Past-the-end is reported, not silent.
        let out = r.run(json!({ "path": "f.txt", "start": 9, "end": 12 }), ctx()).await;
        assert!(out.summary.contains("past the end"), "{}", out.summary);
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

    /// `'append=true'` from the text protocol arrives as a string.
    #[tokio::test]
    async fn write_appends_on_a_text_protocol_flag() {
        let dir = tempdir();
        let w = WriteFile::new(dir.clone());
        w.run(json!({ "path": "log.txt", "content": "a" }), ctx()).await;
        let out = w.run(json!({ "path": "log.txt", "content": "b", "append": "true" }), ctx()).await;
        assert!(out.summary.starts_with("appended 1 bytes"), "{}", out.summary);
        assert_eq!(std::fs::read_to_string(dir.join("log.txt")).unwrap(), "ab");
        assert_eq!(WriteFile::new(dir).optional_args(), vec!["append".to_string()]);
    }

    /// `sessions/95`: a 325 KB file came back whole and overflowed the
    /// window. A read over the cap stops at a line boundary and names the
    /// call that reads on.
    #[tokio::test]
    async fn a_long_read_stops_at_the_cap_and_says_how_to_read_on() {
        let dir = tempdir();
        let line = "x".repeat(2_600);
        let body: String = (0..124).map(|_| format!("{line}\n")).collect();
        std::fs::write(dir.join("wk.txt"), &body).unwrap();
        let r = ReadFile::new(dir.clone());
        let out = r.run(json!({ "path": "wk.txt" }), ctx()).await;
        assert!(out.ok);
        assert!(out.summary.len() <= READ_FILE_MAX_BYTES + 512, "{}", out.summary.len());
        // Each numbered row is 2 607 bytes: 18 fit in 48 KB.
        assert!(out.summary.starts_with("[lines 1-18 of 124]\n    1\t"), "{}", &out.summary[..40]);
        let last = out.summary.lines().last().unwrap();
        assert!(last.starts_with(READ_FILE_CAP_MARK), "{last}");
        assert!(last.contains("read-file 'wk.txt' 'start=19' 'end=36'"), "{last}");
        // Reading on picks up exactly where it stopped.
        let out = r.run(json!({ "path": "wk.txt", "start": "19", "end": "36" }), ctx()).await;
        assert!(out.summary.starts_with("[lines 19-36 of 124]"), "{}", &out.summary[..40]);
        assert!(out.summary.lines().last().unwrap().starts_with("   36\t"));
        // One line longer than the cap is shown up to the cap, and says so.
        std::fs::write(dir.join("one.json"), "y".repeat(100_000)).unwrap();
        let out = r.run(json!({ "path": "one.json" }), ctx()).await;
        assert!(out.summary.len() <= READ_FILE_MAX_BYTES + 512, "{}", out.summary.len());
        assert!(out.summary.contains("line 1 is 100000 bytes long"), "{}", out.summary.lines().last().unwrap());
    }

    /// An inverted range used to slice `lines[start..end]` backwards and
    /// panic the turn.
    #[tokio::test]
    async fn an_inverted_range_is_reported_not_a_panic() {
        let dir = tempdir();
        std::fs::write(dir.join("f.txt"), "a\nb\nc\nd\ne").unwrap();
        let r = ReadFile::new(dir);
        let out = r.run(json!({ "path": "f.txt", "start": 5, "end": 2 }), ctx()).await;
        assert!(out.summary.contains("range is empty"), "{}", out.summary);
    }

    #[tokio::test]
    async fn read_missing_file_fails_cleanly() {
        let dir = tempdir();
        let r = ReadFile::new(dir);
        let out = r.run(json!({ "path": "nope.txt" }), ctx()).await;
        assert!(!out.ok);
        assert!(out.summary.starts_with("no such file: "), "{}", out.summary);
        assert!(out.summary.contains("glob"), "{}", out.summary);
    }

    /// An app folder holding one skill doc, and an empty project folder
    /// beside it: a session opened on a project elsewhere (§3.9).
    fn app_and_project() -> (PathBuf, PathBuf) {
        let base = tempdir();
        let skills = base.join("app").join("skills");
        let work = base.join("project");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(skills.join("agent-team.md"), "---\nname: agent-team\n---\nTEAM CONTRACT\n").unwrap();
        std::fs::write(base.join("app").join("secret.txt"), "SECRET").unwrap();
        (skills, work)
    }

    /// `sessions/96`: the prompt says a skill's contract is at
    /// `skills/<name>.md`, and a session working in another folder got
    /// `no such file` for every one of them.
    #[tokio::test]
    async fn a_skill_doc_opens_from_any_working_directory() {
        let (skills, work) = app_and_project();
        let r = ReadFile::new(work.clone()).with_skills_dir(skills.clone());
        let out = r.run(json!({ "path": "skills/agent-team.md" }), ctx()).await;
        assert!(out.ok, "{}", out.summary);
        assert!(out.summary.contains("TEAM CONTRACT"), "{}", out.summary);
        // It says where the file really is.
        let first = out.summary.lines().next().unwrap();
        assert!(first.starts_with("[read from the app's skill folder: "), "{first}");
        assert!(first.contains(&skills.join("agent-team.md").display().to_string()), "{first}");

        // A project's own `skills/` file of that name is the one it means.
        std::fs::create_dir_all(work.join("skills")).unwrap();
        std::fs::write(work.join("skills").join("agent-team.md"), "PROJECT COPY").unwrap();
        let out = r.run(json!({ "path": "skills/agent-team.md" }), ctx()).await;
        assert_eq!(out.summary, "    1\tPROJECT COPY");

        // Neither has it: the error names the working directory — not the
        // "workspace root" it used to, which is the app's folder.
        let out = r.run(json!({ "path": "skills/nope.md" }), ctx()).await;
        assert!(!out.ok);
        assert!(out.summary.starts_with("no such file: "), "{}", out.summary);
        assert!(out.summary.contains("working directory"), "{}", out.summary);
    }

    /// Only a plain `skills/<name>` reaches the fallback: a `..` must not
    /// make it a way into the rest of the app's folder.
    #[test]
    fn the_skill_doc_fallback_stays_inside_the_folder() {
        let (skills, _work) = app_and_project();
        let doc = skills.join("agent-team.md");
        assert_eq!(skill_doc_path(&skills, "skills/agent-team.md"), Some(doc.clone()));
        assert_eq!(skill_doc_path(&skills, "./skills/agent-team.md"), Some(doc.clone()));
        assert_eq!(skill_doc_path(&skills, "Skills/agent-team.md"), Some(doc.clone()));
        assert_eq!(skill_doc_path(&skills, "skills/../secret.txt"), None);
        assert_eq!(skill_doc_path(&skills, "skills/missing.md"), None);
        assert_eq!(skill_doc_path(&skills, "docs/agent-team.md"), None);
        assert_eq!(skill_doc_path(&skills, &doc.display().to_string()), None);
    }

    /// `glob 'skills/*.md'` was the model's next move in `sessions/96`, and
    /// it matched nothing for the same reason; `grep` follows `read-file`.
    #[tokio::test]
    async fn a_skills_glob_or_grep_falls_back_to_the_app_folder() {
        let (skills, work) = app_and_project();
        let g = Glob::new(work.clone()).with_skills_dir(skills.clone());
        let out = g.run(json!({ "pattern": "skills/*.md" }), ctx()).await;
        assert!(out.ok, "{}", out.summary);
        assert!(
            out.summary.starts_with("[nothing in the working directory matches"),
            "{}",
            out.summary
        );
        assert!(out.summary.contains(&skills.join("agent-team.md").display().to_string()), "{}", out.summary);
        // Any other pattern is the working directory's business alone.
        let out = g.run(json!({ "pattern": "**/*.md" }), ctx()).await;
        assert!(out.summary.contains("no files match"), "{}", out.summary);

        let gr = Grep::new(work).with_skills_dir(skills);
        let out = gr.run(json!({ "pattern": "TEAM", "path": "skills" }), ctx()).await;
        assert!(out.summary.contains("TEAM CONTRACT"), "{}", out.summary);
    }

    #[tokio::test]
    async fn edit_missing_file_points_at_write_file() {
        let dir = tempdir();
        let e = EditFile::new(dir);
        let out = e.run(json!({ "path": "nope.txt", "old": "a", "new": "b" }), ctx()).await;
        assert!(!out.ok);
        assert!(out.summary.starts_with("no such file: "), "{}", out.summary);
        assert!(out.summary.contains("write-file"), "{}", out.summary);
    }

    #[tokio::test]
    async fn edit_replaces_exactly_once() {
        let dir = tempdir();
        std::fs::write(dir.join("f.txt"), "one\ntwo\nthree\n").unwrap();
        let e = EditFile::new(dir.clone());
        let out = e.run(
            json!({ "path": "f.txt", "old": "two", "new": "TWO" }),
            ctx(),
        ).await;
        assert!(out.ok, "{}", out.summary);
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "one\nTWO\nthree\n");
        assert!(out.summary.contains("TWO"), "{}", out.summary);
    }

    #[tokio::test]
    async fn edit_rejects_ambiguous_and_missing_anchors() {
        let dir = tempdir();
        std::fs::write(dir.join("f.txt"), "x\ny\nx\n").unwrap();
        let e = EditFile::new(dir.clone());
        let dup = e.run(json!({ "path": "f.txt", "old": "x", "new": "z" }), ctx()).await;
        assert!(!dup.ok);
        assert!(dup.summary.contains("2 times"), "{}", dup.summary);
        let miss = e.run(json!({ "path": "f.txt", "old": "nope", "new": "z" }), ctx()).await;
        assert!(!miss.ok);
        assert!(miss.summary.contains("not found"), "{}", miss.summary);
        let absent = e.run(json!({ "path": "ghost.txt", "old": "a", "new": "b" }), ctx()).await;
        assert!(!absent.ok);
        assert!(absent.summary.contains("write-file"), "{}", absent.summary);
    }

    #[tokio::test]
    async fn glob_lists_newest_first_and_caps() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn a() {}").unwrap();
        std::fs::write(dir.join("src/b.rs"), "fn b() {}").unwrap();
        std::fs::write(dir.join("README.md"), "# hi").unwrap();
        let g = Glob::new(dir.clone());
        let out = g.run(json!({ "pattern": "src/**/*.rs" }), ctx()).await;
        assert!(out.ok, "{}", out.summary);
        assert!(out.summary.contains("a.rs") && out.summary.contains("b.rs"), "{}", out.summary);
        assert!(!out.summary.contains("README"), "{}", out.summary);
        let none = g.run(json!({ "pattern": "**/*.zzz" }), ctx()).await;
        assert!(none.summary.contains("no files match"), "{}", none.summary);
        let bad = g.run(json!({ "pattern": "[" }), ctx()).await;
        assert!(!bad.ok, "an invalid glob must be an error");
    }

    #[tokio::test]
    async fn grep_finds_matches_with_locations() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn main() {}\nfn helper() {}\n").unwrap();
        std::fs::write(dir.join("src/b.rs"), "nothing here\n").unwrap();
        let g = Grep::new(dir.clone());
        let out = g.run(json!({ "pattern": "fn \\w+", "path": "src" }), ctx()).await;
        assert!(out.ok, "{}", out.summary);
        assert!(out.summary.contains("a.rs:1: fn main"), "{}", out.summary);
        assert!(out.summary.contains("a.rs:2: fn helper"), "{}", out.summary);
        assert!(!out.summary.contains("b.rs"), "{}", out.summary);
        let none = g.run(json!({ "pattern": "zebra", "path": "." }), ctx()).await;
        assert!(none.summary.contains("no matches"), "{}", none.summary);
        let bad = g.run(json!({ "pattern": "(", "path": "." }), ctx()).await;
        assert!(!bad.ok);
    }

    #[tokio::test]
    async fn cli_echoes() {
        let out = RunCli(None).run(
            json!({ "command": if cfg!(windows) { "echo hi" } else { "echo hi" } }),
            ctx(),
        ).await;
        assert!(out.ok, "cli failed: {}", out.summary);
        assert!(out.summary.contains("hi"));
        assert!(out.summary.starts_with("exit=0"));
    }

    #[tokio::test]
    async fn cli_sees_session_id_when_labelled() {
        let sink: std::sync::Arc<dyn crate::agent::EventSink> = std::sync::Arc::new(NullSink);
        let ctx = SkillContext { sub: crate::ToolSubAgent::root(sink).with_spill_label("77") };
        let command = if cfg!(windows) { "echo %SICA_SESSION_ID%" } else { "echo $SICA_SESSION_ID" };
        let out = RunCli(None).run(json!({ "command": command }), ctx).await;
        assert!(out.ok, "{}", out.summary);
        assert!(out.summary.contains("77"), "{}", out.summary);
    }

    #[test]
    fn truncate_caps_with_shared_notice() {
        let mut s = "x".repeat(100);
        truncate(&mut s, 100);
        assert_eq!(s.len(), 100, "at the limit nothing changes");
        let mut s = "x".repeat(150);
        truncate(&mut s, 100);
        assert!(s.starts_with(&"x".repeat(100)));
        assert!(s.contains("[… 50 bytes omitted; the stream was capped"), "{s}");
    }

    #[test]
    fn seed_defaults_writes_files_once() {
        let dir = tempdir();
        seed_defaults(&dir).unwrap();
        for name in [
            RUN_CLI_NAME, RUN_PWSH_NAME, READ_FILE_NAME, WRITE_FILE_NAME,
            EDIT_FILE_NAME, GLOB_NAME, GREP_NAME,
            crate::web::WEB_FETCH_NAME, crate::web::WEB_SEARCH_NAME,
        ] {
            let p = dir.join(format!("{name}.md"));
            assert!(p.exists(), "expected {}", p.display());
        }
        // `agent-team` is opt-in — seeding its doc would silently enable it.
        assert!(
            !dir.join(format!("{}.md", crate::team::AGENT_TEAM_NAME)).exists(),
            "agent-team.md must not be seeded"
        );
        // Tamper, re-seed: file must not be clobbered.
        let path = dir.join(format!("{RUN_CLI_NAME}.md"));
        std::fs::write(&path, "edited").unwrap();
        seed_defaults(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited");
    }

    #[test]
    fn background_is_recognised_from_both_call_shapes() {
        // Native calls send a JSON bool; the text protocol sends a string.
        assert!(wants_background(&json!({ "background": true })));
        assert!(wants_background(&json!({ "background": "true" })));
        assert!(wants_background(&json!({ "background": "YES" })));
        // Anything else runs in the foreground — the safe misreading, since
        // the caller still gets its output.
        assert!(!wants_background(&json!({ "background": "later" })));
        assert!(!wants_background(&json!({ "background": false })));
        assert!(!wants_background(&json!({})));
    }

    #[tokio::test]
    async fn background_without_a_registry_fails_instead_of_running_in_the_foreground() {
        // Falling back would run a command the caller expected to outlive
        // the 30 s foreground cap, and time it out.
        let out = RunCli(None)
            .run(json!({ "command": "echo hi", "background": true }), ctx())
            .await;
        assert!(!out.ok);
        assert!(out.summary.contains("not available"), "{}", out.summary);
    }

    #[tokio::test]
    async fn a_background_shell_call_returns_a_job_id_and_keeps_running() {
        let jobs = std::sync::Arc::new(crate::jobs::JobRegistry::new());
        let mut c = ctx();
        c.sub.session_id = Some(7);
        let out = RunCli(Some(jobs.clone()))
            .run(json!({ "command": "echo hi", "background": "true" }), c)
            .await;
        assert!(out.ok, "{}", out.summary);
        assert!(out.summary.contains("cli-1"), "{}", out.summary);
        assert!(out.summary.contains(crate::jobs::JOB_OUTPUT_NAME));
        assert_eq!(jobs.list(7).len(), 1);
        jobs.clear(7);
    }
}
