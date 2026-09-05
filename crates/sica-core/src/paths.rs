use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// Directory where the idealist writes BE/FE improvement tickets.
/// Mirrors the Python project's `idealist_workspace/` convention.
pub fn idealist_workspace() -> PathBuf {
    workspace_root().join("idealist_workspace")
}

/// Where chat session JSONL files live.
pub fn sessions_dir() -> PathBuf {
    workspace_root().join("sessions")
}

/// Where raw-LLM logs are written when `log_raw_llm` is on (Python parity).
pub fn raw_llm_log_dir() -> PathBuf {
    workspace_root().join("logs").join("model")
}

/// JSON file the frontend reads at startup and writes on Apply.
pub fn settings_file() -> PathBuf {
    workspace_root().join("sica-settings.json")
}

/// Directory holding one TOML file per LLM provider panel. Folder is
/// `.gitignore`d because individual files may contain API keys.
pub fn llm_providers_dir() -> PathBuf {
    workspace_root().join("sica-settings").join("llm-providers")
}

/// Directory the agent runtime scans at startup to load user-authored
/// skills. Each `*.md` file becomes one skill, addressable by the `name:`
/// in its YAML frontmatter. See `agents::md_skill` for the format.
pub fn skills_dir() -> PathBuf {
    workspace_root().join("skills")
}

/// Directory holding markdown-defined *agents* — personas the user can pick
/// from the chat "/" palette. Same file format as `skills/` (see
/// `agents::md_skill`); the difference is intent, not syntax: an agent file
/// describes who should answer, a skill file describes a callable tool.
pub fn agents_dir() -> PathBuf {
    workspace_root().join("agents")
}

/// Directory holding markdown-defined *commands* — canned prompts the user can
/// pick from the chat "/" palette. Same file format as `skills/`.
pub fn commands_dir() -> PathBuf {
    workspace_root().join("commands")
}

/// Directory holding `model-eval` prompt suites — one `.toml` per suite.
/// Seeded with `default.toml` at backend start; user-owned thereafter.
pub fn evals_dir() -> PathBuf {
    workspace_root().join("evals")
}

/// Where `model-eval` writes its reports: a Markdown report plus the JSON
/// baseline the next run of the same suite diffs against.
pub fn eval_reports_dir() -> PathBuf {
    evals_dir().join("reports")
}

/// Where the tool sub-agent parks tool outputs too large to feed back into
/// the model's context. One sub-directory per session id; the model receives
/// a head/tail digest plus the file path so it can `read-file` the rest.
/// `.gitignore`d — regenerated churn, never source.
pub fn spill_dir() -> PathBuf {
    workspace_root().join("spill")
}

/// `memory.md` at the workspace root. The backend seeds a default index of
/// available skills here and prepends its contents as a system message on
/// every chat turn. See `agents::memory`.
pub fn memory_file() -> PathBuf {
    workspace_root().join("memory.md")
}

/// Environment variable overriding [`workspace_root`]. Exists for the replay
/// driver, which needs a run's own state to land in a scratch tree.
pub const WORKSPACE_ROOT_ENV: &str = "SICA_WORKSPACE_ROOT";

/// Workspace root used by every helper above — the *application* home: where
/// `sica-settings.json`, `sessions/`, `skills/` and the rest of the app's own
/// state live. The directory the **agent** works in is [`working_dir`], which
/// defaults to this one but can be pointed at any project folder.
pub fn workspace_root() -> PathBuf {
    // An explicit override wins. Set by the replay driver (guide §14.1) so a
    // scenario writes its sessions, spill and skills into a scratch tree
    // rather than into the checkout it is being run from — an eval that
    // leaves state behind is an eval whose second run tests something else.
    if let Some(v) = std::env::var_os(WORKSPACE_ROOT_ENV) {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    // Otherwise prefer a sibling of the running BE executable. In dev, that's
    // target/debug/, so walk up to the workspace root.
    if let Ok(exe) = std::env::current_exe() {
        let mut p = exe.clone();
        for _ in 0..6 {
            if p.join("Cargo.toml").exists() {
                return p;
            }
            if !p.pop() {
                break;
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}


// ---------------------------------------------------------------------------
// Working directory — the folder the agent acts on.
// ---------------------------------------------------------------------------

/// Environment variable carrying the working-directory override. The frontend
/// sets it on the backend child it spawns, so both processes agree on the
/// folder without adding it to every request.
pub const WORKING_DIR_ENV: &str = "SICA_WORKING_DIR";

/// Process-local override, set from the user's setting before the BE child is
/// spawned. Takes precedence over the environment so a change applies to the
/// running frontend without touching the process environment.
static OVERRIDE: RwLock<Option<PathBuf>> = RwLock::new(None);

/// The directory the agent reads, writes and runs commands in: `read-file`
/// and friends resolve relative paths against it, `workspace-write` confines
/// writes to it, the AGENTS.md/CLAUDE.md chain is discovered under it and
/// `{{cwd}}` renders it.
///
/// Falls back to [`workspace_root`] when nothing overrides it, which is the
/// historical behaviour: the app acts on its own checkout.
pub fn working_dir() -> PathBuf {
    if let Some(p) = OVERRIDE.read().ok().and_then(|g| g.clone()) {
        return p;
    }
    match std::env::var_os(WORKING_DIR_ENV) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => workspace_root(),
    }
}

/// Point [`working_dir`] at `dir`, or back at [`workspace_root`] with `None`.
pub fn set_working_dir(dir: Option<&Path>) {
    if let Ok(mut g) = OVERRIDE.write() {
        *g = dir.map(Path::to_path_buf);
    }
}

/// `true` when the working directory is somewhere other than the app's own
/// root — what the UI shows a "reset" affordance for.
pub fn working_dir_overridden() -> bool {
    working_dir() != workspace_root()
}
