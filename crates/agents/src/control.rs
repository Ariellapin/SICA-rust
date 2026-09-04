//! Harness-control skills (Wave 3, guide §8.4, §11).
//!
//! `todo-write` and `exit-plan-mode` are registered like any other skill —
//! the model discovers them through the catalogue and calls them through
//! the native `tools` array — but their bodies run in `backend::chat`, not
//! in a `Skill::run`: both mutate the session log (`TodoWrite` / `PlanMode`
//! events) and `exit-plan-mode` concludes the turn, none of which a skill
//! can reach through `SkillContext`. The `run()` methods below are
//! unreachable fallbacks (teammates never see these names — see
//! [`TEAMMATE_EXCLUDED`]); the real handlers live next to the dispatcher.
//!
//! `ask-user` is different — it needs no session mutation, only the broker
//! on its sub-agent — so it is a normal skill in [`crate::builtins`].

use std::path::Path;

use async_trait::async_trait;
use serde_json::Value;

use crate::skill::{Skill, SkillContext, SkillOutcome};

pub const TODO_WRITE_NAME: &str = "todo-write";
pub const EXIT_PLAN_MODE_NAME: &str = "exit-plan-mode";
pub const ASK_USER_NAME: &str = "ask-user";

/// Skills a runtime-owned child (an `agent-team` teammate, a `subagent`,
/// a Ralph round) must never see: the harness controls, and every form of
/// further delegation.
///
/// A child cannot ask the user, rewrite the harness todo list or exit the
/// parent's plan mode — anything it needs from those must arrive in its
/// final report instead. It cannot delegate onward either: nested
/// delegation only unwinds at `ToolSubAgent::max_depth`, having spent a
/// whole LLM conversation at every level on the way down.
pub const CHILD_EXCLUDED: &[&str] = &[
    crate::team::AGENT_TEAM_NAME,
    crate::delegate::SUBAGENT_NAME,
    crate::delegate::SUBAGENT_FORK_NAME,
    crate::ralph::RALPH_NAME,
    ASK_USER_NAME,
    TODO_WRITE_NAME,
    EXIT_PLAN_MODE_NAME,
    crate::goal::CREATE_GOAL_NAME,
    crate::goal::GET_GOAL_NAME,
    crate::goal::UPDATE_GOAL_NAME,
];

/// User-editable plan-mode policy, seeded once into `skills/` (never
/// overwritten) and composed into the system prompt as the `PLAN_POLICY`
/// section while plan mode is active. Kept out of the skill scan by name —
/// it is configuration, not a callable skill.
pub const PLAN_MODE_DOC: &str = "plan-mode.md";

pub const PLAN_MODE_SEED: &str = r#"# Plan mode policy

You are in plan mode: explore and design, do not change anything yet.

- Stay in plan mode until `exit-plan-mode` succeeds. Conversational
  agreement ("looks good", "go ahead") approves nothing — only that tool
  call exits plan mode.
- Explore with non-mutating reads (`read-file`, `glob`, `grep`, read-only
  shell commands). These plan-mode rules override any later tool
  description that suggests otherwise.
- Resolve discoverable facts by inspection — read the code instead of
  asking the user or guessing.
- Do not use `todo-write` for the plan itself; the plan is the document
  you are writing.
- Make the plan decision-complete: exact files, commands, and acceptance
  criteria, so it can be executed without further questions.
- When the plan is ready, `exit-plan-mode` with the full plan markdown is
  the only and final tool call.
"#;

/// Seed `skills/plan-mode.md` when absent. Never overwrites — the file is
/// the user's once on disk, like `memory.md`.
pub fn seed_plan_mode(dir: &Path) -> std::io::Result<()> {
    let path = dir.join(PLAN_MODE_DOC);
    if !path.exists() {
        std::fs::write(&path, PLAN_MODE_SEED)?;
    }
    Ok(())
}

/// Is this workspace file the plan-mode policy doc (not a skill)?
pub fn is_policy_doc(path: &Path) -> bool {
    path.file_name().and_then(|s| s.to_str()) == Some(PLAN_MODE_DOC)
}

/// Harness-control skills run in `backend::chat`, not in a `Skill::run`:
/// they mutate the session log and (for `exit-plan-mode`) conclude the
/// turn. The dispatcher intercepts them before any sub-agent spins up.
pub fn is_control_skill(name: &str) -> bool {
    name == TODO_WRITE_NAME || name == EXIT_PLAN_MODE_NAME || crate::goal::is_goal_skill(name)
}

/// Coerce an argument that should be a JSON array into one. The tool
/// schema declares every parameter as `string`, so the text protocol and a
/// literal-minded native model send the array *encoded as a string* — but
/// most native models send the real array anyway. Accept both rather than
/// failing the call over the wrapper.
/// A boolean argument, which arrives as a real JSON bool from a native call
/// and as a string from the text protocol (`'multi=true'`). Anything else,
/// absent included, is `false`: a flag the model did not clearly set is off.
pub fn flag_arg(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => {
            matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on")
        }
        _ => false,
    }
}

pub fn array_arg(v: &Value) -> Option<Vec<Value>> {
    match v {
        Value::Array(a) => Some(a.clone()),
        Value::String(s) => serde_json::from_str::<Vec<Value>>(s).ok(),
        _ => None,
    }
}

/// Validate a `todo-write` items argument: a JSON array of
/// `{content, status}` with `status ∈ pending | in_progress | completed`,
/// sent either as a real array or as its JSON text (see [`array_arg`]).
/// The list replaces the previous one wholesale — the model sends the whole
/// list every time. Rules: trimmed non-empty content, no duplicates, at most
/// one `in_progress` (parallel tracks are future work).
pub fn parse_todo_items(raw: &Value) -> Result<Vec<protocol::TodoItem>, String> {
    let arr = array_arg(raw)
        .ok_or_else(|| "`items` must be a JSON array of {content, status}".to_string())?;
    let arr = &arr;
    if arr.is_empty() {
        return Err("`items` must not be empty — send the whole list each time".into());
    }
    if arr.len() > 50 {
        return Err(format!("too many items ({}) — keep the list under 50", arr.len()));
    }
    let mut out = Vec::with_capacity(arr.len());
    let mut in_progress = 0;
    for (i, item) in arr.iter().enumerate() {
        let content = item
            .get("content")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("item {i}: `content` must be non-empty text"))?;
        let status = item
            .get("status")
            .and_then(|v| v.as_str())
            .and_then(protocol::TodoStatus::parse)
            .ok_or_else(|| {
                format!("item {i}: `status` must be pending | in_progress | completed")
            })?;
        if out.iter().any(|t: &protocol::TodoItem| t.content == content) {
            return Err(format!("item {i}: duplicate of an earlier item"));
        }
        if status == protocol::TodoStatus::InProgress {
            in_progress += 1;
        }
        out.push(protocol::TodoItem { content: content.to_string(), status });
    }
    if in_progress > 1 {
        return Err("at most one item may be `in_progress` — finish or park the other first".into());
    }
    Ok(out)
}

fn unreachable(name: &str) -> SkillOutcome {
    SkillOutcome {
        ok: false,
        summary: format!(
            "`{name}` is handled by the harness loop, not by a skill body — \
             this call should never have been dispatched"
        ),
    }
}

/// Full-replacement todo list. Validated and persisted by the harness (see
/// module docs); this stub only carries the catalogue entry + guidance.
pub struct TodoWrite;

#[async_trait]
impl Skill for TodoWrite {
    fn name(&self) -> &str {
        TODO_WRITE_NAME
    }
    fn description(&self) -> &str {
        "Replace the session todo list with a JSON array of {content, status}. Shows as a checklist in the UI."
    }
    fn positional_args(&self) -> Vec<String> {
        vec!["items".into()]
    }
    fn prompt_guidance(&self) -> Option<&'static str> {
        Some("Use todo-write for multi-step tasks; send the whole list each time.")
    }
    async fn run(&self, _args: Value, _ctx: SkillContext) -> SkillOutcome {
        unreachable(TODO_WRITE_NAME)
    }
}

/// Present a finished plan for approval. Handled by the harness: asks the
/// user (Approve / Keep planning) and, on approval, leaves plan mode and
/// ends the turn. Always registered — outside plan mode it rejects, so the
/// tool catalog stays byte-stable across modes.
pub struct ExitPlanMode;

#[async_trait]
impl Skill for ExitPlanMode {
    fn name(&self) -> &str {
        EXIT_PLAN_MODE_NAME
    }
    fn description(&self) -> &str {
        "Present the finished plan (markdown) for user approval. Only call in plan mode; ends the turn on approval."
    }
    fn positional_args(&self) -> Vec<String> {
        vec!["plan".into()]
    }
    fn prompt_guidance(&self) -> Option<&'static str> {
        Some("In plan mode, exit-plan-mode with the full plan markdown is the only and final call.")
    }
    async fn run(&self, _args: Value, _ctx: SkillContext) -> SkillOutcome {
        unreachable(EXIT_PLAN_MODE_NAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_items_validate_happy_path() {
        let items = parse_todo_items(&Value::String(
            r#"[{"content": "a", "status": "pending"},
                {"content": "b", "status": "in_progress"}]"#
                .into(),
        ))
        .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].status, protocol::TodoStatus::InProgress);
    }

    #[test]
    fn todo_items_reject_bad_lists() {
        let s = |t: &str| parse_todo_items(&Value::String(t.into()));
        assert!(s("not json").is_err());
        assert!(s(r#"{"content": "a"}"#).is_err());
        assert!(s("[]").is_err());
        assert!(s(r#"[{"content": "", "status": "pending"}]"#).is_err());
        assert!(s(r#"[{"content": "a", "status": "later"}]"#).is_err());
        assert!(
            s(r#"[{"content": "a", "status": "pending"},{"content": "a", "status": "done"}]"#)
                .is_err(),
            "duplicates"
        );
        assert!(
            s(r#"[{"content": "a", "status": "in_progress"},{"content": "b", "status": "in_progress"}]"#)
                .is_err(),
            "two in_progress"
        );
    }

    #[test]
    fn todo_items_accept_a_native_array_as_well_as_its_json_text() {
        // Every tool parameter is declared `string`, but native models
        // routinely send the real array. Both shapes must land.
        let native = serde_json::json!([{"content": "a", "status": "pending"}]);
        let text = Value::String(r#"[{"content": "a", "status": "pending"}]"#.into());
        assert_eq!(parse_todo_items(&native).unwrap(), parse_todo_items(&text).unwrap());
        assert!(parse_todo_items(&Value::Null).is_err());
    }

    #[test]
    fn policy_doc_detection_is_by_filename() {
        assert!(is_policy_doc(Path::new("skills/plan-mode.md")));
        assert!(!is_policy_doc(Path::new("skills/other.md")));
        assert!(!is_policy_doc(Path::new("plan-mode.md.off")));
    }
}
