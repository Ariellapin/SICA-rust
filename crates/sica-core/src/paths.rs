use std::path::PathBuf;

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

/// Workspace root used by every helper above. Public so callers (e.g. the
/// `read-file`/`write-file` skills) can resolve relative paths against the
/// same directory the BE considers its working tree.
pub fn workspace_root() -> PathBuf {
    // Prefer a sibling of the running BE executable. In dev, that's
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
