//! User hooks (guide §13.1): shell commands the operator runs at fixed
//! points of the loop, configured exactly like Claude Code's so an existing
//! `hooks.json` can be copied across.
//!
//! `.sica/hooks.json` in the working directory:
//!
//! ```json
//! {
//!   "hooks": {
//!     "PreToolUse":  [{ "matcher": "run-cli|run-pwsh",
//!                       "hooks": [{ "type": "command",
//!                                   "command": "python audit.py",
//!                                   "timeout": 30 }] }],
//!     "PostToolUse": [{ "hooks": [{ "type": "command", "command": "..." }] }]
//!   }
//! }
//! ```
//!
//! Each hook gets the event as JSON on stdin and answers on stdout. Two
//! output shapes are accepted, because Claude Code grew a second one:
//! `{"decision": "approve"|"block", "reason": "…"}` and
//! `{"hookSpecificOutput": {"permissionDecision": "allow"|"deny"|"ask",
//! "permissionDecisionReason": "…", "additionalContext": "…"}}`. Exit code
//! 2 is a block with stderr as the reason — the shape a hook written as a
//! plain script uses. `"continue": false` is a deny that also says the
//! whole turn should stop; we treat it as the strongest deny there is.
//!
//! **Merge: strictest wins.** `deny > ask > allow`, reasons kept per rank,
//! every hook's `additionalContext` collected in the order the hooks ran.
//! A hook that fails to spawn, times out, or writes garbage is *not* a
//! denial: the operator's script being broken must not silently turn into a
//! permission decision. It is logged as `error` and abstains — with one
//! exception, exit code 2, which is a deliberate block.
//!
//! Wired at four points. `PreToolUse` / `PostToolUse` ride the §6.1
//! pipeline as [`HooksPolicy`]; `UserPromptSubmit` and `SessionStart` are
//! called directly from `chat.rs`, where a deny refuses the prompt and
//! `additionalContext` becomes a `ContextInjected` the model reads.
//! `Stop` is defined by the config schema but not yet dispatched — a hook
//! configured for it is reported at load rather than silently ignored.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use sica_core::event::EventKind;

/// Default per-hook wall clock. Overridable per hook (`"timeout"`, seconds).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// Cap on a hook's stdout. A hook that dumps a build log is a
/// misconfiguration, not a reason to blow up the prompt.
const MAX_OUTPUT: usize = 16 * 1024;

/// The points a hook can run at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
}

impl HookEvent {
    /// The name used in the config file and in the JSON payload — Claude
    /// Code's spelling, so a copied hooks file works unchanged.
    pub fn name(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "SessionStart",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::Stop => "Stop",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "SessionStart" => HookEvent::SessionStart,
            "UserPromptSubmit" => HookEvent::UserPromptSubmit,
            "PreToolUse" => HookEvent::PreToolUse,
            "PostToolUse" => HookEvent::PostToolUse,
            "Stop" => HookEvent::Stop,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct RawFile {
    #[serde(default)]
    hooks: HashMap<String, Vec<RawGroup>>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawGroup {
    /// Regex over the tool name. Absent, empty or `"*"` matches everything.
    #[serde(default)]
    matcher: Option<String>,
    #[serde(default)]
    hooks:   Vec<RawHook>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawHook {
    /// Only `"command"` runs. Claude Code has never shipped another kind,
    /// but the field exists, so an unknown one is skipped with a warning
    /// rather than misread as a command.
    #[serde(rename = "type", default)]
    kind:    Option<String>,
    #[serde(default)]
    command: String,
    /// Seconds.
    #[serde(default)]
    timeout: Option<u64>,
}

/// One configured hook, resolved.
#[derive(Debug, Clone)]
pub struct Hook {
    pub command: String,
    pub timeout: Duration,
    /// Compiled matcher; `None` matches every tool.
    matcher: Option<regex::Regex>,
}

impl Hook {
    fn matches(&self, tool: &str) -> bool {
        match &self.matcher {
            None => true,
            Some(re) => re.is_match(tool),
        }
    }
}

/// Everything `.sica/hooks.json` configured, by event.
#[derive(Debug, Default, Clone)]
pub struct HookConfig {
    by_event: HashMap<&'static str, Vec<Hook>>,
    /// Non-fatal complaints from the load: an unknown event name, a hook
    /// type we do not run, a matcher that will not compile. Surfaced as
    /// `LogLine`s — a hook that silently never runs is worse than one that
    /// says why.
    pub warnings: Vec<String>,
}

impl HookConfig {
    pub fn is_empty(&self) -> bool {
        self.by_event.values().all(|v| v.is_empty())
    }

    pub fn for_event(&self, event: HookEvent) -> &[Hook] {
        self.by_event.get(event.name()).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Hooks for `event` whose matcher accepts `tool`.
    pub fn matching(&self, event: HookEvent, tool: &str) -> Vec<&Hook> {
        self.for_event(event).iter().filter(|h| h.matches(tool)).collect()
    }

    /// Total configured hooks — what the startup line reports.
    pub fn count(&self) -> usize {
        self.by_event.values().map(|v| v.len()).sum()
    }
}

/// Where the file lives: under the *working* directory, so pointing the
/// agent at another project picks up that project's hooks rather than the
/// app's own.
pub fn config_path() -> PathBuf {
    sica_core::paths::working_dir().join(".sica").join("hooks.json")
}

/// Load and compile. A missing file is the normal case and yields an empty
/// config; a malformed one is a warning, never a failure — the agent has to
/// keep running when the operator's JSON has a trailing comma.
pub fn load() -> HookConfig {
    load_from(&config_path())
}

pub fn load_from(path: &Path) -> HookConfig {
    let mut cfg = HookConfig::default();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return cfg,
        Err(e) => {
            cfg.warnings.push(format!("hooks: {} unreadable: {e}", path.display()));
            return cfg;
        }
    };
    let raw: RawFile = match serde_json::from_str(&text) {
        Ok(r) => r,
        Err(e) => {
            cfg.warnings
                .push(format!("hooks: {} is not valid JSON: {e} — no hooks loaded", path.display()));
            return cfg;
        }
    };
    for (event_name, groups) in raw.hooks {
        let Some(event) = HookEvent::parse(&event_name) else {
            cfg.warnings.push(format!("hooks: unknown event {event_name:?} — ignored"));
            continue;
        };
        for group in groups {
            let matcher = match group.matcher.as_deref() {
                None | Some("") | Some("*") => None,
                Some(pat) => match regex::Regex::new(pat) {
                    Ok(re) => Some(re),
                    Err(e) => {
                        cfg.warnings.push(format!(
                            "hooks: {event_name} matcher {pat:?} will not compile: {e} — \
                             that group is skipped"
                        ));
                        continue;
                    }
                },
            };
            for h in group.hooks {
                if h.kind.as_deref().unwrap_or("command") != "command" {
                    cfg.warnings.push(format!(
                        "hooks: {event_name} hook of type {:?} is not runnable — only \
                         `command` hooks run",
                        h.kind.unwrap_or_default()
                    ));
                    continue;
                }
                if h.command.trim().is_empty() {
                    cfg.warnings
                        .push(format!("hooks: {event_name} hook has an empty command — ignored"));
                    continue;
                }
                cfg.by_event.entry(event.name()).or_default().push(Hook {
                    command: h.command,
                    timeout: h.timeout.map(Duration::from_secs).unwrap_or(DEFAULT_TIMEOUT),
                    matcher: matcher.clone(),
                });
            }
        }
    }
    if !cfg.for_event(HookEvent::Stop).is_empty() {
        cfg.warnings.push(
            "hooks: Stop hooks are configured but this build does not dispatch \
             them yet — they will not run"
                .into(),
        );
    }
    cfg
}

// ---------------------------------------------------------------------------
// Decisions
// ---------------------------------------------------------------------------

/// Merge rank. Ordered so `max` is "strictest wins".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Rank {
    /// Nothing to say — the outcome when no hook is configured, and when a
    /// hook fails in a way that is not a decision.
    #[default]
    Allow,
    /// Route to the approval broker.
    Ask,
    /// Refuse the call.
    Deny,
}

impl Rank {
    pub fn label(self) -> &'static str {
        match self {
            Rank::Allow => "allow",
            Rank::Ask => "ask",
            Rank::Deny => "deny",
        }
    }
}

/// What one hook answered.
#[derive(Debug, Clone, Default)]
pub struct HookAnswer {
    pub rank:       Rank,
    pub reason:     Option<String>,
    pub context:    Option<String>,
    /// The hook asked for the whole turn to stop. Carried through the merge
    /// so a caller that can honour it may; the tool pipeline treats it as
    /// the deny it also is.
    pub stop:       bool,
    /// The hook wants the tool called with different arguments. We record
    /// it so the operator is told it did nothing rather than believing a
    /// rewrite happened.
    pub updated_input: bool,
    /// `None` when the command could not be spawned or timed out.
    pub exit_code:  Option<i32>,
    /// What the durable `Hook` event records — the merge rank, or `error`
    /// when the hook itself failed.
    pub outcome:    String,
}

/// The merge of every hook that ran for one event.
#[derive(Debug, Clone, Default)]
pub struct Merged {
    pub rank:    Rank,
    /// Some hook asked for different tool arguments. Carried so the caller
    /// can say it did nothing rather than let the author believe otherwise.
    pub rewrote_input: bool,
    /// Reasons at the winning rank, in the order the hooks ran.
    pub reasons: Vec<String>,
    /// Every hook's `additionalContext`, in order.
    pub context: Vec<String>,
    pub stop:    bool,
    /// One row per hook that ran, for the durable log.
    pub ran:     Vec<(String, String, Option<i32>)>,
}

impl Merged {
    /// The reasons as one line for a tool-result / refusal message.
    pub fn reason_text(&self) -> String {
        if self.reasons.is_empty() {
            format!("a hook returned {}", self.rank.label())
        } else {
            self.reasons.join("; ")
        }
    }
}

/// Strictest wins: the highest rank any hook returned, its reasons in
/// order, and *every* hook's context regardless of rank — a hook that
/// allowed the call may still have something the model should know.
pub fn merge(answers: Vec<(String, HookAnswer)>) -> Merged {
    let mut out = Merged::default();
    for (_, a) in &answers {
        out.rank = out.rank.max(a.rank);
        out.stop |= a.stop;
        out.rewrote_input |= a.updated_input;
    }
    for (command, a) in answers {
        if let Some(c) = a.context.clone() {
            if !c.trim().is_empty() {
                out.context.push(c);
            }
        }
        if a.rank == out.rank && out.rank != Rank::Allow {
            if let Some(r) = a.reason.clone() {
                out.reasons.push(r);
            }
        }
        out.ran.push((command, a.outcome, a.exit_code));
    }
    out
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

/// The JSON one hook reads on stdin. `tool_name` / `tool_input` are absent
/// for the non-tool events, which is what Claude Code does too.
pub fn payload(
    event: HookEvent,
    session_id: u64,
    tool: Option<&str>,
    tool_input: Option<&Value>,
    tool_response: Option<&Value>,
    prompt: Option<&str>,
) -> Value {
    let mut v = json!({
        "hook_event_name": event.name(),
        "session_id": session_id.to_string(),
        "cwd": sica_core::paths::working_dir().display().to_string(),
    });
    let map = v.as_object_mut().expect("payload is an object");
    if let Some(t) = tool {
        map.insert("tool_name".into(), json!(t));
    }
    if let Some(i) = tool_input {
        map.insert("tool_input".into(), i.clone());
    }
    if let Some(r) = tool_response {
        map.insert("tool_response".into(), r.clone());
    }
    if let Some(p) = prompt {
        map.insert("prompt".into(), json!(p));
    }
    v
}

/// Run every hook in `hooks` with `payload` on stdin and merge the answers.
pub async fn run_all(hooks: &[&Hook], payload: &Value) -> Merged {
    let body = payload.to_string();
    let mut answers = Vec::new();
    for h in hooks {
        answers.push((h.command.clone(), run_one(h, &body).await));
    }
    merge(answers)
}

async fn run_one(hook: &Hook, stdin_body: &str) -> HookAnswer {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", &hook.command]);
        c
    } else {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", &hook.command]);
        c
    };
    cmd.current_dir(sica_core::paths::working_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return HookAnswer {
                outcome: "error".into(),
                reason: Some(format!("hook {:?} would not start: {e}", hook.command)),
                ..Default::default()
            };
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        // A hook that ignores stdin closes the pipe; that is a broken pipe
        // on our side, not a failure of the hook.
        let _ = stdin.write_all(stdin_body.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }

    let out = match tokio::time::timeout(hook.timeout, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return HookAnswer {
                outcome: "error".into(),
                reason: Some(format!("hook {:?} failed: {e}", hook.command)),
                ..Default::default()
            };
        }
        Err(_) => {
            return HookAnswer {
                outcome: "error".into(),
                reason: Some(format!(
                    "hook {:?} timed out after {}s",
                    hook.command,
                    hook.timeout.as_secs()
                )),
                ..Default::default()
            };
        }
    };

    let code = out.status.code();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stdout = sica_core::retain::utf8_head(&stdout, MAX_OUTPUT);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stderr = sica_core::retain::utf8_head(&stderr, MAX_OUTPUT);
    parse_answer(code, &stdout, &stderr)
}

/// The output codec. Kept a free function so the tests can drive every
/// shape without spawning anything.
pub fn parse_answer(code: Option<i32>, stdout: &str, stderr: &str) -> HookAnswer {
    // Exit code 2 is the scripted block: stderr is the reason, and no JSON
    // is expected. This is the shape a three-line shell hook uses.
    if code == Some(2) {
        let reason = if stderr.trim().is_empty() {
            "a hook blocked this (exit 2)".to_string()
        } else {
            stderr.trim().to_string()
        };
        return HookAnswer {
            rank: Rank::Deny,
            reason: Some(reason),
            exit_code: code,
            outcome: Rank::Deny.label().into(),
            ..Default::default()
        };
    }

    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        // Silence means "no opinion" — the overwhelmingly common case for a
        // hook that exists to log or to lint. A *non-zero* silence is a
        // broken hook: still no opinion (the operator's script failing must
        // not become a permission decision), but recorded as an error so
        // the operator sees it rather than wondering why nothing happens.
        let failed = !matches!(code, Some(0));
        return HookAnswer {
            exit_code: code,
            outcome: if failed { "error".into() } else { Rank::Allow.label().into() },
            reason: failed.then(|| match stderr.trim() {
                "" => format!("hook exited {}", code.map(|c| c.to_string()).unwrap_or_else(|| "on a signal".into())),
                e => e.to_string(),
            }),
            ..Default::default()
        };
    }
    let Ok(json) = serde_json::from_str::<Value>(trimmed) else {
        // Not JSON: a non-zero exit still means the hook is unhappy, but a
        // hook that prints prose is not making a permission decision.
        return HookAnswer {
            exit_code: code,
            outcome: "error".into(),
            reason: Some(format!("hook wrote non-JSON output: {}", first_line(trimmed))),
            ..Default::default()
        };
    };

    let specific = json.get("hookSpecificOutput");
    let mut rank = Rank::Allow;
    let mut reason = None;

    // Shape 1: the flat `decision`.
    match json.get("decision").and_then(|v| v.as_str()) {
        Some("block") => {
            rank = Rank::Deny;
            reason = json.get("reason").and_then(|v| v.as_str()).map(str::to_string);
        }
        Some("approve") => {}
        _ => {}
    }
    // Shape 2: `hookSpecificOutput.permissionDecision`, which wins when it
    // is present — it is the newer, more precise one.
    if let Some(spec) = specific {
        if let Some(decision) = spec.get("permissionDecision").and_then(|v| v.as_str()) {
            let spec_reason = spec
                .get("permissionDecisionReason")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            match decision {
                // It *replaces* rather than raising: a hook that sets both
                // fields means the newer one, and reading `block` through
                // an explicit `allow` would deny a call its author allowed.
                "deny" => {
                    rank = Rank::Deny;
                    reason = spec_reason.or(reason);
                }
                "ask" => {
                    rank = Rank::Ask;
                    reason = spec_reason.or(reason);
                }
                "allow" => {
                    rank = Rank::Allow;
                    reason = None;
                }
                _ => {}
            }
        }
    }

    // `continue: false` stops the turn. It is at least as strict as a deny,
    // so it raises the rank as well as setting the flag.
    let stop = json.get("continue").and_then(|v| v.as_bool()) == Some(false);
    if stop {
        rank = Rank::Deny;
        reason = json
            .get("stopReason")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or(reason);
    }

    // `additionalContext` sits under `hookSpecificOutput` in the new shape
    // and at the top level in hooks written against the old docs.
    let context = specific
        .and_then(|s| s.get("additionalContext"))
        .or_else(|| json.get("additionalContext"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let updated_input = json.get("updatedInput").is_some()
        || specific.and_then(|s| s.get("updatedInput")).is_some();

    HookAnswer {
        outcome: rank.label().into(),
        rank,
        reason,
        context,
        stop,
        updated_input,
        exit_code: code,
    }
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() <= 120 {
        line.to_string()
    } else {
        line.chars().take(120).collect::<String>() + "…"
    }
}

// ---------------------------------------------------------------------------
// The pipeline policy
// ---------------------------------------------------------------------------

/// `PreToolUse` / `PostToolUse` as a §6.1 [`ToolPolicy`].
///
/// Holds the session map so each run can append its durable
/// `EventKind::Hook` row — a hook is an authority over the turn, and an
/// authority whose decisions are not in the log cannot be audited later.
pub struct HooksPolicy {
    pub config:   Arc<HookConfig>,
    pub sessions: crate::chat::Sessions,
    pub events:   Arc<dyn agents::EventSink>,
}

impl HooksPolicy {
    /// Record what ran, and tell the operator about anything that needs
    /// their attention (a broken hook, an `updatedInput` we cannot apply).
    async fn record(&self, event: HookEvent, session_id: Option<u64>, merged: &Merged) {
        if merged.rewrote_input {
            // Parsed, deliberately not applied: the §6.1 pipeline judges a
            // call, it does not rewrite one. Saying so is the difference
            // between a hook that does nothing and a hook whose author
            // believes it rewrote the arguments.
            self.events.emit(protocol::Event::LogLine {
                level:   "WARN".into(),
                message: format!(
                    "a {} hook returned `updatedInput`, which this build does not \
                     apply — the tool ran with the arguments the model sent",
                    event.name()
                ),
            });
        }
        for (command, decision, exit_code) in &merged.ran {
            if decision == "error" {
                self.events.emit(protocol::Event::LogLine {
                    level:   "WARN".into(),
                    message: format!(
                        "{} hook failed and was ignored: {command}",
                        event.name()
                    ),
                });
            }
            if let Some(id) = session_id {
                crate::chat::append_event(&self.sessions, id, EventKind::Hook {
                    event:     event.name().to_string(),
                    command:   command.clone(),
                    decision:  decision.clone(),
                    exit_code: *exit_code,
                })
                .await;
            }
        }
        if merged.rank != Rank::Allow {
            self.events.emit(protocol::Event::LogLine {
                level:   "INFO".into(),
                message: format!(
                    "{} hook returned {}: {}",
                    event.name(),
                    merged.rank.label(),
                    merged.reason_text()
                ),
            });
        }
    }
}

#[async_trait::async_trait]
impl agents::pipeline::ToolPolicy for HooksPolicy {
    async fn pre_execute(
        &self,
        call: &agents::pipeline::CallView<'_>,
    ) -> agents::pipeline::PreDecision {
        use agents::pipeline::PreDecision;
        let hooks = self.config.matching(HookEvent::PreToolUse, call.skill);
        if hooks.is_empty() {
            return PreDecision::Allow;
        }
        let payload = payload(
            HookEvent::PreToolUse,
            call.session_id.unwrap_or(0),
            Some(call.skill),
            Some(call.args),
            None,
            None,
        );
        let merged = run_all(&hooks, &payload).await;
        self.record(HookEvent::PreToolUse, call.session_id, &merged).await;
        match merged.rank {
            Rank::Allow => PreDecision::Allow,
            // An `ask` from a hook is the same rendezvous a destructive
            // shell command takes: the human answers once, for this call.
            Rank::Ask => PreDecision::Ask {
                reason: format!("a hook asks: {}", merged.reason_text()),
            },
            Rank::Deny => PreDecision::Deny {
                reason: format!("[hook denied this call: {}]", merged.reason_text()),
            },
        }
    }

    async fn post_execute(
        &self,
        call: &agents::pipeline::CallView<'_>,
        outcome: &agents::SkillOutcome,
    ) -> agents::pipeline::PostDecision {
        use agents::pipeline::PostDecision;
        let hooks = self.config.matching(HookEvent::PostToolUse, call.skill);
        if hooks.is_empty() {
            return PostDecision::Accept {
                summary:       outcome.summary.clone(),
                extra_context: Vec::new(),
            };
        }
        let response = serde_json::json!({
            "ok":      outcome.ok,
            "summary": sica_core::retain::utf8_head(&outcome.summary, 8 * 1024),
        });
        let payload = payload(
            HookEvent::PostToolUse,
            call.session_id.unwrap_or(0),
            Some(call.skill),
            Some(call.args),
            Some(&response),
            None,
        );
        let merged = run_all(&hooks, &payload).await;
        self.record(HookEvent::PostToolUse, call.session_id, &merged).await;
        // A post hook cannot un-run the call, so `ask` has nothing to ask
        // about: only a deny is actionable, and it becomes feedback the
        // model reads instead of the result.
        if merged.rank == Rank::Deny {
            PostDecision::Block {
                feedback:      format!(
                    "[a hook rejected this result: {}]",
                    merged.reason_text()
                ),
                extra_context: merged.context.clone(),
            }
        } else {
            PostDecision::Accept {
                summary:       outcome.summary.clone(),
                extra_context: merged.context.clone(),
            }
        }
    }
}

/// Run the hooks for an event that is not a tool call, and hand back what
/// the caller has to act on. Used by `chat.rs` for `SessionStart` and
/// `UserPromptSubmit`, neither of which passes through the tool pipeline.
///
/// `session_id` may name a session whose log does not exist yet
/// (`SessionStart` fires as it is created); `append_event` is a no-op then,
/// which is the right outcome — there is nothing to append to.
pub async fn run_event(
    config: &HookConfig,
    sessions: &crate::chat::Sessions,
    events: &Arc<dyn agents::EventSink>,
    event: HookEvent,
    session_id: u64,
    prompt: Option<&str>,
) -> Merged {
    let hooks: Vec<&Hook> = config.for_event(event).iter().collect();
    if hooks.is_empty() {
        return Merged::default();
    }
    let payload = payload(event, session_id, None, None, None, prompt);
    let merged = run_all(&hooks, &payload).await;
    for (command, decision, exit_code) in &merged.ran {
        if decision == "error" {
            events.emit(protocol::Event::LogLine {
                level:   "WARN".into(),
                message: format!("{} hook failed and was ignored: {command}", event.name()),
            });
        }
        crate::chat::append_event(sessions, session_id, EventKind::Hook {
            event:     event.name().to_string(),
            command:   command.clone(),
            decision:  decision.clone(),
            exit_code: *exit_code,
        })
        .await;
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_from(json: &str) -> HookConfig {
        let dir = std::env::temp_dir().join(format!(
            "sica-hooks-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hooks.json");
        std::fs::write(&path, json).unwrap();
        let cfg = load_from(&path);
        let _ = std::fs::remove_dir_all(&dir);
        cfg
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let cfg = load_from(Path::new("does/not/exist/hooks.json"));
        assert!(cfg.is_empty());
        assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
    }

    #[test]
    fn malformed_json_warns_and_loads_nothing() {
        // The agent has to keep running when the operator's JSON has a
        // trailing comma.
        let cfg = cfg_from(r#"{ "hooks": { "PreToolUse": [ , ] } }"#);
        assert!(cfg.is_empty());
        assert_eq!(cfg.warnings.len(), 1);
        assert!(cfg.warnings[0].contains("not valid JSON"), "{:?}", cfg.warnings);
    }

    #[test]
    fn a_claude_code_hooks_file_loads_as_written() {
        let cfg = cfg_from(
            r#"{
              "hooks": {
                "PreToolUse": [
                  { "matcher": "run-cli|run-pwsh",
                    "hooks": [{ "type": "command", "command": "audit.py", "timeout": 5 }] },
                  { "hooks": [{ "type": "command", "command": "log-all.sh" }] }
                ],
                "PostToolUse": [
                  { "matcher": "write-file",
                    "hooks": [{ "type": "command", "command": "fmt.sh" }] }
                ]
              }
            }"#,
        );
        assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
        assert_eq!(cfg.count(), 3);
        // The matcher decides which hooks a given tool sees.
        let for_cli = cfg.matching(HookEvent::PreToolUse, "run-cli");
        assert_eq!(for_cli.len(), 2, "the matcherless group applies to everything");
        assert_eq!(for_cli[0].timeout, Duration::from_secs(5));
        let for_read = cfg.matching(HookEvent::PreToolUse, "read-file");
        assert_eq!(for_read.len(), 1);
        assert_eq!(for_read[0].command, "log-all.sh");
        assert_eq!(cfg.matching(HookEvent::PostToolUse, "read-file").len(), 0);
    }

    #[test]
    fn unrunnable_entries_say_why_instead_of_vanishing() {
        let cfg = cfg_from(
            r#"{ "hooks": {
                   "Nonsense": [],
                   "PreToolUse": [
                     { "hooks": [{ "type": "webhook", "command": "x" }] },
                     { "matcher": "(unclosed", "hooks": [{ "command": "y" }] },
                     { "hooks": [{ "command": "   " }] }
                   ]
                 } }"#,
        );
        assert_eq!(cfg.count(), 0);
        assert_eq!(cfg.warnings.len(), 4, "{:?}", cfg.warnings);
        assert!(cfg.warnings.iter().any(|w| w.contains("unknown event")));
        assert!(cfg.warnings.iter().any(|w| w.contains("only `command` hooks run")));
        assert!(cfg.warnings.iter().any(|w| w.contains("will not compile")));
        assert!(cfg.warnings.iter().any(|w| w.contains("empty command")));
    }

    #[test]
    fn a_stop_hook_is_reported_as_not_dispatched() {
        let cfg = cfg_from(
            r#"{ "hooks": { "Stop": [{ "hooks": [{ "command": "x" }] }] } }"#,
        );
        assert_eq!(cfg.count(), 1);
        assert!(cfg.warnings.iter().any(|w| w.contains("does not dispatch")), "{:?}", cfg.warnings);
    }

    #[test]
    fn a_hook_that_says_nothing_allows() {
        let a = parse_answer(Some(0), "", "");
        assert_eq!(a.rank, Rank::Allow);
        assert!(a.reason.is_none());
    }

    #[test]
    fn exit_two_blocks_with_stderr_as_the_reason() {
        let a = parse_answer(Some(2), "", "no writes under vendor/");
        assert_eq!(a.rank, Rank::Deny);
        assert_eq!(a.reason.as_deref(), Some("no writes under vendor/"));
    }

    #[test]
    fn both_output_shapes_are_understood() {
        let old = parse_answer(Some(0), r#"{"decision":"block","reason":"nope"}"#, "");
        assert_eq!(old.rank, Rank::Deny);
        assert_eq!(old.reason.as_deref(), Some("nope"));

        let new = parse_answer(
            Some(0),
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse",
                "permissionDecision":"ask","permissionDecisionReason":"sure?"}}"#,
            "",
        );
        assert_eq!(new.rank, Rank::Ask);
        assert_eq!(new.reason.as_deref(), Some("sure?"));

        let allow = parse_answer(Some(0), r#"{"decision":"approve"}"#, "");
        assert_eq!(allow.rank, Rank::Allow);
    }

    #[test]
    fn the_precise_shape_wins_over_the_flat_one() {
        // A hook that sets both means the newer field; reading `block` here
        // would deny a call its author allowed.
        let a = parse_answer(
            Some(0),
            r#"{"decision":"block","reason":"old",
                "hookSpecificOutput":{"permissionDecision":"allow"}}"#,
            "",
        );
        assert_eq!(a.rank, Rank::Allow);
    }

    #[test]
    fn continue_false_is_the_strongest_deny() {
        let a = parse_answer(Some(0), r#"{"continue":false,"stopReason":"budget spent"}"#, "");
        assert_eq!(a.rank, Rank::Deny);
        assert!(a.stop);
        assert_eq!(a.reason.as_deref(), Some("budget spent"));
    }

    #[test]
    fn additional_context_is_read_from_either_place() {
        let nested = parse_answer(
            Some(0),
            r#"{"hookSpecificOutput":{"additionalContext":"the repo is frozen"}}"#,
            "",
        );
        assert_eq!(nested.context.as_deref(), Some("the repo is frozen"));
        let flat = parse_answer(Some(0), r#"{"additionalContext":"same"}"#, "");
        assert_eq!(flat.context.as_deref(), Some("same"));
    }

    #[test]
    fn a_broken_hook_abstains_rather_than_denying() {
        // The operator's script being wrong must not become a permission
        // decision — that would make every tool call hostage to a typo.
        let a = parse_answer(Some(1), "this is not json", "");
        assert_eq!(a.rank, Rank::Allow);
        assert_eq!(a.outcome, "error");
        assert!(a.reason.unwrap().contains("non-JSON"));
    }

    #[test]
    fn updated_input_is_noticed_so_it_can_be_reported() {
        let a = parse_answer(Some(0), r#"{"updatedInput":{"path":"other.txt"}}"#, "");
        assert!(a.updated_input);
    }

    #[test]
    fn merge_takes_the_strictest_rank_and_keeps_every_context() {
        let merged = merge(vec![
            ("a".into(), HookAnswer {
                rank: Rank::Allow,
                context: Some("ctx-a".into()),
                outcome: "allow".into(),
                ..Default::default()
            }),
            ("b".into(), HookAnswer {
                rank: Rank::Ask,
                reason: Some("ask-b".into()),
                outcome: "ask".into(),
                ..Default::default()
            }),
            ("c".into(), HookAnswer {
                rank: Rank::Deny,
                reason: Some("deny-c".into()),
                context: Some("ctx-c".into()),
                outcome: "deny".into(),
                ..Default::default()
            }),
        ]);
        assert_eq!(merged.rank, Rank::Deny);
        // Only the winning rank's reasons: an "ask" reason under a deny
        // would read as a second, contradictory decision.
        assert_eq!(merged.reasons, vec!["deny-c".to_string()]);
        // Context from every hook, in order, whatever each decided.
        assert_eq!(merged.context, vec!["ctx-a".to_string(), "ctx-c".to_string()]);
        assert_eq!(merged.ran.len(), 3);
    }

    #[test]
    fn merge_of_nothing_allows() {
        let merged = merge(Vec::new());
        assert_eq!(merged.rank, Rank::Allow);
        assert!(merged.ran.is_empty());
    }

    #[test]
    fn the_payload_carries_only_the_fields_its_event_has() {
        let pre = payload(
            HookEvent::PreToolUse,
            7,
            Some("write-file"),
            Some(&json!({ "path": "a.txt" })),
            None,
            None,
        );
        assert_eq!(pre["hook_event_name"], "PreToolUse");
        assert_eq!(pre["session_id"], "7");
        assert_eq!(pre["tool_name"], "write-file");
        assert!(pre.get("tool_response").is_none());
        assert!(pre.get("prompt").is_none());

        let prompt = payload(HookEvent::UserPromptSubmit, 7, None, None, None, Some("hi"));
        assert_eq!(prompt["prompt"], "hi");
        assert!(prompt.get("tool_name").is_none());
    }

    #[tokio::test]
    async fn a_real_hook_runs_and_its_stdout_decides() {
        // End to end through the shell, which is the part the unit tests
        // above cannot cover: stdin delivery, exit code, stdout parsing.
        //
        // The hook is a *script*, not an inline command: `std::process`
        // escapes arguments MSVC-style, which `cmd.exe` does not parse that
        // way, so an inline `echo {"a":1}` arrives with backslashes in it.
        // A real hook is a script too, so this is also the honest shape.
        let dir = std::env::temp_dir().join(format!("sica-hook-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let body = r#"{"decision":"block","reason":"denied by test"}"#;
        let script = if cfg!(windows) {
            let p = dir.join("hook.bat");
            std::fs::write(&p, format!("@echo off\r\necho {body}\r\n")).unwrap();
            p
        } else {
            let p = dir.join("hook.sh");
            std::fs::write(&p, format!("#!/bin/sh\necho '{body}'\n")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            p
        };
        let hook = Hook {
            command: script.display().to_string(),
            timeout: Duration::from_secs(20),
            matcher: None,
        };
        let merged = run_all(&[&hook], &json!({ "hook_event_name": "PreToolUse" })).await;
        assert_eq!(merged.rank, Rank::Deny);
        assert_eq!(merged.reason_text(), "denied by test");
        assert_eq!(merged.ran.len(), 1);
        assert_eq!(merged.ran[0].2, Some(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_hook_that_cannot_start_does_not_deny() {
        let hook = Hook {
            command: "this-command-does-not-exist-anywhere".into(),
            timeout: Duration::from_secs(20),
            matcher: None,
        };
        let merged = run_all(&[&hook], &json!({})).await;
        assert_eq!(merged.rank, Rank::Allow);
        assert_eq!(merged.ran[0].1, "error");
    }
}
