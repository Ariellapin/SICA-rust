//! Guarded tool-execution pipeline (Wave 3, guide §6.1).
//!
//! Every tool call passes three stages around the skill body:
//!
//! 1. `pre_execute` — each policy answers `allow | deny | ask`. The first
//!    non-`Allow` wins; `Ask` routes to the approval broker (§10.2), or
//!    becomes a denial when nobody can answer.
//! 2. Monotonic `guard`s — deny-only. They run after all `pre_execute`
//!    listeners so ordering can never turn a denial back into permission.
//! 3. `post_execute` — `accept | block` over the outcome, plus
//!    `extra_context`: the generic "attach a nudge to the next request"
//!    channel (repeat-tool reminders ride it).
//!
//! A `Deny`/`Block` becomes `SkillOutcome { ok: false }` carrying the
//! reason — the model reads it and self-corrects. It is *not* a defect, so
//! it is never forwarded to the failure sink. `post_execute` runs even for
//! denied calls: a model hammering a denied call is exactly the loop worth
//! breaking, so the repeat reminder counts those too.
//!
//! Policies shipped here: [`PermissionPolicy`] (§10.3), [`PlanModePolicy`]
//! (§11.1), [`ReadBeforeEdit`] (§8.5) and [`RepeatReminder`] (§6.4).

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use protocol::PermissionMode;
use serde_json::Value;

use crate::guard::RepeatTracker;
use crate::skill::SkillOutcome;

/// Decision of one `pre_execute` listener.
pub enum PreDecision {
    Allow,
    Deny { reason: String },
    /// Ask the human once. Needs a broker + session on the sub-agent;
    /// without either it degrades to `Deny`.
    Ask { reason: String },
}

/// Decision of one `post_execute` listener.
pub enum PostDecision {
    Accept { summary: String, extra_context: Vec<String> },
    Block { feedback: String, extra_context: Vec<String> },
}

/// Read-only view of the call a policy judges. `args` is the resolved JSON
/// object (positionals already zipped onto their names).
pub struct CallView<'a> {
    pub skill: &'a str,
    pub args: &'a Value,
    pub args_preview: &'a str,
    pub depth: u8,
    pub session_id: Option<u64>,
}

/// One pipeline participant. All methods have blanket allow-all defaults so
/// a policy implements only the stage it cares about.
#[async_trait]
pub trait ToolPolicy: Send + Sync {
    async fn pre_execute(&self, _call: &CallView<'_>) -> PreDecision {
        PreDecision::Allow
    }
    /// Deny-only: `Some(reason)` denies, `None` abstains. Runs after every
    /// `pre_execute`, so a guard can only deny, never re-allow.
    fn guard(&self, _call: &CallView<'_>) -> Option<String> {
        None
    }
    async fn post_execute(
        &self,
        _call: &CallView<'_>,
        outcome: &SkillOutcome,
    ) -> PostDecision {
        PostDecision::Accept {
            summary: outcome.summary.clone(),
            extra_context: Vec::new(),
        }
    }
}

/// Permission modes as policy (§10.3, policy level — no OS enforcement).
/// Constructed per dispatch with the session's current mode; the mode
/// itself lives on `ChatHub` and rides the runtime-context line so the
/// model always knows the policy.
pub struct PermissionPolicy {
    pub mode: PermissionMode,
    pub workspace_root: PathBuf,
}

impl PermissionPolicy {
    /// Skills that mutate the workspace by writing files.
    fn is_write_skill(skill: &str) -> bool {
        matches!(
            skill,
            crate::builtins::WRITE_FILE_NAME
                | crate::builtins::EDIT_FILE_NAME
                | "skill-creator"
        )
    }

    fn is_shell_skill(skill: &str) -> bool {
        matches!(
            skill,
            crate::builtins::RUN_CLI_NAME | crate::builtins::RUN_PWSH_NAME
        )
    }
}

#[async_trait]
impl ToolPolicy for PermissionPolicy {
    async fn pre_execute(&self, call: &CallView<'_>) -> PreDecision {
        match self.mode {
            PermissionMode::DangerFullAccess => PreDecision::Allow,
            PermissionMode::ReadOnly => {
                if Self::is_write_skill(call.skill) {
                    return PreDecision::Deny {
                        reason: "[permission: write denied under read-only mode — \
                                 ask the user to switch modes]"
                            .into(),
                    };
                }
                if Self::is_shell_skill(call.skill) {
                    let cmd = call.args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                    if !is_read_only_command(cmd) {
                        return PreDecision::Deny {
                            reason: "[permission: shell denied under read-only mode — \
                                     only read-only commands (dir, git status, rg, …) run; \
                                     ask the user to switch modes]"
                                .into(),
                        };
                    }
                }
                PreDecision::Allow
            }
            PermissionMode::WorkspaceWrite => {
                if Self::is_write_skill(call.skill) {
                    // Writes must stay inside the workspace. A missing path
                    // is left to the skill's own error (it names the arg).
                    if let Some(path) = call.args.get("path").and_then(|v| v.as_str()) {
                        match crate::builtins::resolve(&self.workspace_root, path) {
                            Ok(resolved) if !resolved.starts_with(&self.workspace_root) => {
                                return PreDecision::Deny {
                                    reason: format!(
                                        "[permission: write denied — {path:?} is outside \
                                         the workspace; ask the user to switch modes]"
                                    ),
                                };
                            }
                            Err(e) => {
                                return PreDecision::Deny {
                                    reason: format!("[permission: bad path — {e}]"),
                                };
                            }
                            _ => {}
                        }
                    }
                    return PreDecision::Allow;
                }
                if Self::is_shell_skill(call.skill) {
                    let cmd = call.args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                    if looks_destructive(cmd) {
                        return PreDecision::Ask {
                            reason: format!(
                                "shell command looks destructive: `{cmd}` — \
                                 approve once to run it"
                            ),
                        };
                    }
                }
                PreDecision::Allow
            }
        }
    }
}

/// Plan mode as policy (§11.1). While active, only non-mutating tools run;
/// the plan is finished with `exit-plan-mode`, which stays registered (and
/// rejecting) when plan mode is off so the tool catalog never shifts.
pub struct PlanModePolicy {
    pub active: bool,
}

#[async_trait]
impl ToolPolicy for PlanModePolicy {
    async fn pre_execute(&self, call: &CallView<'_>) -> PreDecision {
        if !self.active {
            return PreDecision::Allow;
        }
        // The way out is always open.
        if call.skill == crate::control::EXIT_PLAN_MODE_NAME {
            return PreDecision::Allow;
        }
        if PermissionPolicy::is_write_skill(call.skill) {
            return PreDecision::Deny {
                reason: "plan mode: only non-mutating tools are available; \
                         finish with exit-plan-mode"
                    .into(),
            };
        }
        if PermissionPolicy::is_shell_skill(call.skill) {
            let cmd = call.args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if !is_read_only_command(cmd) {
                return PreDecision::Deny {
                    reason: "plan mode: only non-mutating tools are available; \
                             finish with exit-plan-mode"
                        .into(),
                };
            }
        }
        PreDecision::Allow
    }
}

/// Read-before-edit (§8.5). Enforced purely through observed versions: a
/// file the session has never read may only be *created*; a read file may
/// only be written at the version last seen. Per-session state, shared
/// with the sub-agents of one session.
pub struct ReadBeforeEdit {
    seen: Mutex<HashMap<PathBuf, u64>>,
    pub root: PathBuf,
}

impl ReadBeforeEdit {
    pub fn new(root: PathBuf) -> Self {
        Self { seen: Mutex::new(HashMap::new()), root }
    }

    fn digest(bytes: &[u8]) -> u64 {
        let mut h = DefaultHasher::new();
        bytes.hash(&mut h);
        h.finish()
    }

    fn resolved(&self, args: &Value) -> Option<PathBuf> {
        let path = args.get("path").and_then(|v| v.as_str())?;
        crate::builtins::resolve(&self.root, path).ok()
    }
}

#[async_trait]
impl ToolPolicy for ReadBeforeEdit {
    async fn pre_execute(&self, call: &CallView<'_>) -> PreDecision {
        if call.skill != crate::builtins::WRITE_FILE_NAME
            && call.skill != crate::builtins::EDIT_FILE_NAME
        {
            return PreDecision::Allow;
        }
        let Some(resolved) = self.resolved(call.args) else {
            return PreDecision::Allow; // missing path: the skill's own error names it
        };
        let current = std::fs::read(&resolved).ok().map(|b| Self::digest(&b));
        let Some(current) = current else {
            return PreDecision::Allow; // unseen path may only be created
        };
        match self.seen.lock().unwrap().get(&resolved) {
            None => PreDecision::Deny {
                reason: format!(
                    "read the file first — {} exists and you have not observed it",
                    resolved.display()
                ),
            },
            Some(seen) if *seen != current => PreDecision::Deny {
                reason: format!(
                    "file changed on disk since you read {} — read it again",
                    resolved.display()
                ),
            },
            _ => PreDecision::Allow,
        }
    }

    async fn post_execute(
        &self,
        call: &CallView<'_>,
        outcome: &SkillOutcome,
    ) -> PostDecision {
        // A successful read *or* write observes the on-disk version, so a
        // write never trips the check on its own result.
        if outcome.ok
            && (call.skill == crate::builtins::READ_FILE_NAME
                || call.skill == crate::builtins::WRITE_FILE_NAME
                || call.skill == crate::builtins::EDIT_FILE_NAME)
        {
            if let Some(resolved) = self.resolved(call.args) {
                if let Ok(bytes) = std::fs::read(&resolved) {
                    self.seen
                        .lock()
                        .unwrap()
                        .insert(resolved, Self::digest(&bytes));
                }
            }
        }
        PostDecision::Accept {
            summary: outcome.summary.clone(),
            extra_context: Vec::new(),
        }
    }
}

/// The repeat-tool reminder as a policy (§6.4). Counts in `post_execute`
/// *including denied calls* and returns the escalating advisory notice as
/// `extra_context` — advice, never a block. One instance per session.
pub struct RepeatReminder {
    tracker: Mutex<RepeatTracker>,
}

impl RepeatReminder {
    pub fn new() -> Self {
        Self { tracker: Mutex::new(RepeatTracker::default()) }
    }

    /// Record one dispatched call outside the pipeline (hop-limit and
    /// unknown-skill outcomes never reach a sub-agent). Same notice the
    /// policy path returns.
    pub fn observe(&self, skill: &str, args: &Value) -> Option<String> {
        self.tracker.lock().unwrap().observe(skill, args)
    }

    /// A new user message starts a fresh chain.
    pub fn reset(&self) {
        self.tracker.lock().unwrap().reset();
    }
}

impl Default for RepeatReminder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolPolicy for RepeatReminder {
    async fn post_execute(
        &self,
        call: &CallView<'_>,
        outcome: &SkillOutcome,
    ) -> PostDecision {
        let extra = self.observe(call.skill, call.args).into_iter().collect();
        PostDecision::Accept {
            summary: outcome.summary.clone(),
            extra_context: extra,
        }
    }
}

/// Shell commands that observably change nothing: listings, diffs, text
/// search, version probes. Anything chained (`&&`, `||`, `;`, `|`) is *not*
/// read-only — the tail could do anything. Used by the read-only permission
/// mode, plan mode, and the parallel-call classifier.
pub fn is_read_only_command(cmd: &str) -> bool {
    let c = cmd.trim().to_lowercase();
    if c.is_empty() {
        return false;
    }
    // Chained or redirected commands can do anything past the first verb.
    if c.contains("&&") || c.contains("||") || c.contains(';') || c.contains('|') || c.contains('>') {
        return false;
    }
    const PREFIXES: &[&str] = &[
        "dir", "echo", "type", "git status", "git diff", "git log", "git show", "git branch",
        "git remote", "rg", "find", "findstr", "where", "whoami", "hostname", "set",
        "ls", "cat", "head", "tail", "wc", "pwd", "node --version", "npm --version",
        "python --version", "cargo --version", "rustc --version",
    ];
    PREFIXES.iter().any(|p| c == *p || c.starts_with(&format!("{p} ")))
}

/// Shell commands that look destructive under `workspace-write`: they ask
/// first. Token-based (not substring) so `firmware` never matches `rm`.
pub fn looks_destructive(cmd: &str) -> bool {
    let c = cmd.trim().to_lowercase();
    if c.is_empty() {
        return false;
    }
    let tokens: Vec<&str> = c
        .split(|ch: char| !ch.is_alphanumeric() && ch != '-' && ch != '_' && ch != '.')
        .filter(|t| !t.is_empty())
        .collect();
    const WORDS: &[&str] = &[
        "rm", "del", "erase", "rd", "rmdir", "deltree", "format", "mkfs", "dd",
        "remove-item", "ri", "clear-recyclebin",
    ];
    if tokens.iter().any(|t| WORDS.contains(t)) {
        return true;
    }
    c.contains("--force") || c.contains("reset --hard") || c.contains("clean -fd")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view<'a>(skill: &'a str, args: &'a Value, preview: &'a str) -> CallView<'a> {
        CallView { skill, args, args_preview: preview, depth: 0, session_id: Some(1) }
    }

    fn ok() -> SkillOutcome {
        SkillOutcome { ok: true, summary: "done".into() }
    }

    #[tokio::test]
    async fn read_only_mode_denies_writes_and_shell() {
        let p = PermissionPolicy {
            mode: PermissionMode::ReadOnly,
            workspace_root: PathBuf::from("/work"),
        };
        let args = json!({"path": "a.txt", "content": "x"});
        let d = p.pre_execute(&view("write-file", &args, "")).await;
        assert!(matches!(d, PreDecision::Deny { .. }));
        let d = p.pre_execute(&view("skill-creator", &json!({}), "")).await;
        assert!(matches!(d, PreDecision::Deny { .. }));
        let d = p
            .pre_execute(&view("run-cli", &json!({"command": "cargo build"}), ""))
            .await;
        assert!(matches!(d, PreDecision::Deny { .. }));
        // Read-only shell passes.
        let d = p
            .pre_execute(&view("run-cli", &json!({"command": "git status"}), ""))
            .await;
        assert!(matches!(d, PreDecision::Allow));
        let d = p
            .pre_execute(&view("read-file", &json!({"path": "a.txt"}), ""))
            .await;
        assert!(matches!(d, PreDecision::Allow));
    }

    #[tokio::test]
    async fn workspace_write_asks_on_destructive_shell() {
        let p = PermissionPolicy {
            mode: PermissionMode::WorkspaceWrite,
            workspace_root: PathBuf::from("/work"),
        };
        let d = p
            .pre_execute(&view("run-cli", &json!({"command": "rm -rf target"}), ""))
            .await;
        assert!(matches!(d, PreDecision::Ask { .. }));
        let d = p
            .pre_execute(&view("run-cli", &json!({"command": "git push --force"}), ""))
            .await;
        assert!(matches!(d, PreDecision::Ask { .. }));
        let d = p
            .pre_execute(&view("run-cli", &json!({"command": "cargo build"}), ""))
            .await;
        assert!(matches!(d, PreDecision::Allow));
    }

    #[tokio::test]
    async fn danger_allows_everything() {
        let p = PermissionPolicy {
            mode: PermissionMode::DangerFullAccess,
            workspace_root: PathBuf::from("/work"),
        };
        let d = p
            .pre_execute(&view("run-cli", &json!({"command": "rm -rf /"}), ""))
            .await;
        assert!(matches!(d, PreDecision::Allow));
    }

    #[test]
    fn destructive_matching_is_token_based() {
        assert!(looks_destructive("rm -rf x"));
        assert!(looks_destructive("del /f a.txt"));
        assert!(looks_destructive("Remove-Item -Recurse build"));
        assert!(looks_destructive("git push --force"));
        assert!(!looks_destructive("echo firmware"));
        assert!(!looks_destructive("cargo build"));
        assert!(!looks_destructive(""));
    }

    #[test]
    fn read_only_list_allows_probes_rejects_chains() {
        assert!(is_read_only_command("dir"));
        assert!(is_read_only_command("git status --short"));
        assert!(is_read_only_command("rg 'foo' src"));
        assert!(!is_read_only_command("dir && del a"));
        assert!(!is_read_only_command("git status > out.txt"));
        assert!(!is_read_only_command("cargo build"));
        assert!(!is_read_only_command(""));
    }

    #[tokio::test]
    async fn plan_mode_denies_mutation_but_leaves_the_exit_open() {
        let p = PlanModePolicy { active: true };
        let d = p
            .pre_execute(&view("write-file", &json!({"path": "a"}), ""))
            .await;
        assert!(matches!(d, PreDecision::Deny { .. }));
        let d = p
            .pre_execute(&view("run-cli", &json!({"command": "cargo test"}), ""))
            .await;
        assert!(matches!(d, PreDecision::Deny { .. }));
        let d = p
            .pre_execute(&view("read-file", &json!({"path": "a"}), ""))
            .await;
        assert!(matches!(d, PreDecision::Allow));
        let d = p
            .pre_execute(&view("exit-plan-mode", &json!({"plan": "x"}), ""))
            .await;
        assert!(matches!(d, PreDecision::Allow));
        let off = PlanModePolicy { active: false };
        let d = off
            .pre_execute(&view("write-file", &json!({"path": "a"}), ""))
            .await;
        assert!(matches!(d, PreDecision::Allow));
    }

    #[tokio::test]
    async fn read_before_edit_tracks_versions() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir()
            .join(format!("rbe-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("a.txt");
        std::fs::write(&file, "v1").unwrap();
        let rel = "a.txt";

        let p = ReadBeforeEdit::new(root.clone());
        let w = json!({"path": rel, "content": "v2"});
        let d = p.pre_execute(&view("write-file", &w, "")).await;
        assert!(matches!(d, PreDecision::Deny { .. }), "unseen existing file may only be created");

        // A successful read observes the version.
        let r = json!({"path": rel});
        p.post_execute(&view("read-file", &r, ""), &ok()).await;
        let d = p.pre_execute(&view("write-file", &w, "")).await;
        assert!(matches!(d, PreDecision::Allow));

        // External change invalidates the observation.
        std::fs::write(&file, "v1-external").unwrap();
        let d = p.pre_execute(&view("write-file", &w, "")).await;
        assert!(matches!(d, PreDecision::Deny { reason } if reason.contains("changed on disk")));

        // A successful write re-observes; a missing path stays creatable.
        std::fs::write(&file, "v2").unwrap();
        p.post_execute(&view("write-file", &w, ""), &ok()).await;
        let d = p.pre_execute(&view("write-file", &w, "")).await;
        assert!(matches!(d, PreDecision::Allow));
        let n = json!({"path": "new.txt", "content": "x"});
        let d = p.pre_execute(&view("write-file", &n, "")).await;
        assert!(matches!(d, PreDecision::Allow));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn repeat_reminder_fires_through_post_execute() {
        let r = RepeatReminder::new();
        let args = json!({"path": "a"});
        let v = view("read-file", &args, "");
        for _ in 0..2 {
            let d = r.post_execute(&v, &ok()).await;
            assert!(matches!(d, PostDecision::Accept { extra_context, .. } if extra_context.is_empty()));
        }
        let PostDecision::Accept { summary, extra_context } = r.post_execute(&v, &ok()).await
        else {
            panic!("notice must arrive as Accept + context, never a block");
        };
        assert_eq!(summary, "done");
        assert_eq!(extra_context.len(), 1);
        assert!(extra_context[0].contains("3 times"));
        r.reset();
        let d = r.post_execute(&v, &ok()).await;
        assert!(matches!(d, PostDecision::Accept { extra_context, .. } if extra_context.is_empty()));
    }
}
