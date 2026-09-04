//! One durable objective per session, and the rules around it (§12.3).
//!
//! A goal is what lets a session keep working after the reply that would
//! normally end it: while one is active and *armed*, the round driver in
//! `backend::chat` opens a fresh turn against the same objective each time
//! the agent goes idle, up to a hard round cap.
//!
//! Everything dangerous about that idea is answered by one of the rules
//! below, so they are worth stating plainly:
//!
//! - **Every mutation is a compare-and-set on `revision`.** A round that
//!   has been superseded — the human edited the goal while it was
//!   thinking — is refused instead of silently overwriting the edit.
//! - **Authority is checked at execution, not at parse time.** Creating,
//!   editing, pausing and resuming need a direct human turn on a top-level
//!   agent ([`TurnSource::is_human`], `depth == 0`). Completing and
//!   blocking also accept the current automatic round, because a round
//!   that has finished the work is exactly who should say so.
//! - **A round cannot cry "blocked" too early.** The first
//!   [`BLOCKED_AFTER_CONSECUTIVE_ROUNDS`] rounds may not declare the goal
//!   blocked; a model that gets stuck on its first attempt has usually not
//!   tried the second way yet. A human can block it at any time.
//! - **Arming is process-local and never persisted.** After a restart, a
//!   fork, or a session reload, an active goal comes back disarmed and
//!   waits for a human to say continue. A reboot must never resume an
//!   autonomous loop on its own.

use async_trait::async_trait;
use protocol::GoalPhase;
use serde_json::Value;
use sica_core::event::TurnSource;

use crate::skill::{Skill, SkillContext, SkillOutcome};

pub const CREATE_GOAL_NAME: &str = "create-goal";
pub const GET_GOAL_NAME: &str = "get-goal";
pub const UPDATE_GOAL_NAME: &str = "update-goal";

/// Hard ceiling on rounds for one goal, whatever the caller asks for.
pub const MAX_GOAL_ROUNDS: u32 = 256;
/// Rounds a goal gets when the caller does not say.
pub const DEFAULT_GOAL_ROUNDS: u32 = 16;
/// An automatic round may only declare the goal blocked once this many
/// rounds have started.
pub const BLOCKED_AFTER_CONSECUTIVE_ROUNDS: u32 = 3;

/// A session's objective. One per session; `revision` increments on every
/// change and is the token every mutation must present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Goal {
    pub id:             u64,
    pub revision:       u32,
    pub objective:      String,
    pub phase:          GoalPhase,
    pub rounds_started: u32,
    pub max_rounds:     u32,
    pub blocker:        Option<String>,
}

impl Goal {
    pub fn new(id: u64, objective: String, max_rounds: u32) -> Self {
        Self {
            id,
            revision: 1,
            objective,
            phase: GoalPhase::Active,
            rounds_started: 0,
            max_rounds: max_rounds.clamp(1, MAX_GOAL_ROUNDS),
            blocker: None,
        }
    }

    /// Whether the driver may open another round. Armed-ness is *not*
    /// checked here — that lives in the hub, because it is process state
    /// rather than goal state.
    pub fn rounds_left(&self) -> bool {
        self.phase == GoalPhase::Active && self.rounds_started < self.max_rounds
    }

    /// One-line rendering for `get-goal` and for log lines.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "goal #{} rev {} [{}] round {}/{}: {}",
            self.id,
            self.revision,
            self.phase.label(),
            self.rounds_started,
            self.max_rounds,
            self.objective
        );
        if let Some(b) = &self.blocker {
            s.push_str(&format!("\nblocker: {b}"));
        }
        s
    }
}

/// What a caller wants to do to an existing goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalAction {
    Pause,
    Resume,
    Complete,
    Block,
}

impl GoalAction {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pause" => Ok(GoalAction::Pause),
            "resume" | "continue" => Ok(GoalAction::Resume),
            "complete" | "done" => Ok(GoalAction::Complete),
            "block" | "blocked" => Ok(GoalAction::Block),
            other => Err(format!(
                "unknown action `{other}` — want pause | resume | complete | block"
            )),
        }
    }

    /// The phase this action moves the goal to.
    pub fn phase(&self) -> GoalPhase {
        match self {
            GoalAction::Pause => GoalPhase::Paused,
            GoalAction::Resume => GoalPhase::Active,
            GoalAction::Complete => GoalPhase::Completed,
            GoalAction::Block => GoalPhase::Blocked,
        }
    }
}

/// Why an attempted goal mutation was refused. Returned rather than
/// panicking so the model reads the reason and can act on it.
pub type GoalRefusal = String;

/// Check that this caller may take this action now.
///
/// `human` is whether the current turn carries a person's authority
/// ([`TurnSource::is_human`]); `depth` is the sub-agent depth, so a
/// delegated child can never touch the goal that spawned it.
pub fn authorize(
    action: GoalAction,
    goal: &Goal,
    human: bool,
    depth: u8,
) -> Result<(), GoalRefusal> {
    if depth > 0 {
        return Err(
            "only the top-level agent may change the goal — report what you \
             found and let it decide"
                .into(),
        );
    }
    match action {
        // A person parks or resumes their own objective. An autonomous
        // round resuming itself would make pausing meaningless.
        GoalAction::Pause | GoalAction::Resume if !human => Err(format!(
            "`{}` needs a direct instruction from the user — an automatic \
             round cannot pause or resume its own goal",
            action_word(action)
        )),
        // A round that has done the work is exactly who should say so.
        GoalAction::Block
            if !human && goal.rounds_started < BLOCKED_AFTER_CONSECUTIVE_ROUNDS =>
        {
            Err(format!(
                "too early to call this blocked — {} of {} rounds have run. \
                 Try another approach and report `block` only if it is still \
                 impossible",
                goal.rounds_started, BLOCKED_AFTER_CONSECUTIVE_ROUNDS
            ))
        }
        _ => Ok(()),
    }
}

fn action_word(a: GoalAction) -> &'static str {
    match a {
        GoalAction::Pause => "pause",
        GoalAction::Resume => "resume",
        GoalAction::Complete => "complete",
        GoalAction::Block => "block",
    }
}

/// Apply an action under compare-and-set. `revision` is what the caller
/// believes the current revision to be; a mismatch is refused so a
/// superseded round cannot clobber a human's edit.
pub fn apply(
    goal: &Goal,
    revision: u32,
    action: GoalAction,
    note: Option<&str>,
) -> Result<Goal, GoalRefusal> {
    if revision != goal.revision {
        return Err(format!(
            "revision mismatch: you sent {revision}, the goal is at {}. \
             Read it with `{GET_GOAL_NAME}` and retry with the current \
             revision — someone changed it while you were working",
            goal.revision
        ));
    }
    if goal.phase.is_terminal() && action != GoalAction::Resume {
        return Err(format!(
            "the goal is already {} — nothing more to do",
            goal.phase.label()
        ));
    }
    if action == GoalAction::Block && note.map(str::trim).unwrap_or("").is_empty() {
        return Err(
            "blocking needs a concrete blocker in `note` — say what is in the \
             way, not that it is hard"
                .into(),
        );
    }
    let mut next = goal.clone();
    next.revision += 1;
    next.phase = action.phase();
    next.blocker = match action {
        GoalAction::Block => note.map(|n| n.trim().to_string()),
        // Resuming clears a stale blocker: it described a state someone has
        // now decided is past.
        GoalAction::Resume => None,
        _ => goal.blocker.clone(),
    };
    Ok(next)
}

/// Record that the driver opened a round. Bumps `revision` like every
/// other mutation, so a round in flight and a human edit still cannot both
/// win.
pub fn start_round(goal: &Goal) -> Goal {
    let mut next = goal.clone();
    next.revision += 1;
    next.rounds_started += 1;
    next
}

/// The prompt an automatic round opens with.
///
/// It says three things the model would otherwise get wrong: that this is
/// the same session and the same objective, that the *workspace* is the
/// source of truth rather than its own earlier narration (the failure mode
/// of every long autonomous run), and that finishing requires evidence.
pub fn round_prompt(goal: &Goal) -> String {
    format!(
        "<goal_round>\n\
         Objective: {}\n\
         Round {} of {}.\n\n\
         Continue working toward the objective in this same session. Treat \
         the current workspace, your tool results and the durable session \
         state as authoritative: inspect them instead of assuming earlier \
         narration is still true. Make concrete progress and verify the \
         result with a tool before you describe it.\n\n\
         When the objective is genuinely met, call `{UPDATE_GOAL_NAME}` with \
         revision {}, action `complete`, and a note naming the evidence. If \
         something outside your reach makes it impossible, use action \
         `block` with the specific blocker. Otherwise just keep working — \
         another round will follow.\n\
         </goal_round>",
        goal.objective,
        goal.rounds_started,
        goal.max_rounds,
        goal.revision,
    )
}

/// Parse the `max_rounds` argument of `create-goal`, which arrives as a
/// string from the text protocol and as a number from a native call.
pub fn parse_max_rounds(v: Option<&Value>) -> u32 {
    let raw = match v {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0) as u32,
        Some(Value::String(s)) => s.trim().parse::<u32>().unwrap_or(0),
        _ => 0,
    };
    if raw == 0 {
        DEFAULT_GOAL_ROUNDS
    } else {
        raw.clamp(1, MAX_GOAL_ROUNDS)
    }
}

/// Whether a turn source carries human authority for the goal skills.
pub fn human_turn(source: TurnSource) -> bool {
    source.is_human()
}

// ---------------------------------------------------------------------------
// Catalogue stubs — the bodies run in `backend::chat` (see `control`).
// ---------------------------------------------------------------------------

fn unreachable(name: &str) -> SkillOutcome {
    SkillOutcome {
        ok: false,
        summary: format!(
            "`{name}` is handled by the harness loop, not by a skill body — \
             this call should never have been dispatched"
        ),
    }
}

pub struct CreateGoal;

#[async_trait]
impl Skill for CreateGoal {
    fn name(&self) -> &str { CREATE_GOAL_NAME }
    fn description(&self) -> &str {
        "Set this session's durable objective and keep working toward it \
         across turns. Positional args: <objective> <max_rounds>. Only the \
         user can create or change a goal; each round is a full turn, so ask \
         before setting one."
    }
    fn positional_args(&self) -> Vec<String> {
        vec!["objective".into(), "max_rounds".into()]
    }
    async fn run(&self, _args: Value, _ctx: SkillContext) -> SkillOutcome {
        unreachable(CREATE_GOAL_NAME)
    }
}

pub struct GetGoal;

#[async_trait]
impl Skill for GetGoal {
    fn name(&self) -> &str { GET_GOAL_NAME }
    fn description(&self) -> &str {
        "Read this session's goal: objective, phase, round count and the \
         current revision (which `update-goal` requires). No arguments."
    }
    async fn run(&self, _args: Value, _ctx: SkillContext) -> SkillOutcome {
        unreachable(GET_GOAL_NAME)
    }
}

pub struct UpdateGoal;

#[async_trait]
impl Skill for UpdateGoal {
    fn name(&self) -> &str { UPDATE_GOAL_NAME }
    fn description(&self) -> &str {
        "Change the goal's phase. Positional args: <revision> <action> \
         <note>, where action is pause | resume | complete | block. The \
         revision must match the goal's current one — read it with get-goal \
         first."
    }
    fn positional_args(&self) -> Vec<String> {
        vec!["revision".into(), "action".into(), "note".into()]
    }
    fn prompt_guidance(&self) -> Option<&'static str> {
        Some(
            "With a goal active, finish a round by calling update-goal only \
             when the objective is met (action `complete`, with the evidence \
             in the note) or genuinely impossible (`block`, with the specific \
             blocker); otherwise just keep working and another round follows.",
        )
    }
    async fn run(&self, _args: Value, _ctx: SkillContext) -> SkillOutcome {
        unreachable(UPDATE_GOAL_NAME)
    }
}

/// The goal skills, like the other harness controls, run in the hub.
pub fn is_goal_skill(name: &str) -> bool {
    name == CREATE_GOAL_NAME || name == GET_GOAL_NAME || name == UPDATE_GOAL_NAME
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal() -> Goal {
        Goal::new(1, "make the tests pass".into(), 8)
    }

    #[test]
    fn a_new_goal_is_active_at_revision_one_with_no_rounds_run() {
        let g = goal();
        assert_eq!((g.revision, g.rounds_started), (1, 0));
        assert_eq!(g.phase, GoalPhase::Active);
        assert!(g.rounds_left());
    }

    #[test]
    fn max_rounds_is_clamped_at_both_ends() {
        assert_eq!(Goal::new(1, "x".into(), 0).max_rounds, 1);
        assert_eq!(Goal::new(1, "x".into(), 10_000).max_rounds, MAX_GOAL_ROUNDS);
        assert_eq!(parse_max_rounds(None), DEFAULT_GOAL_ROUNDS);
        assert_eq!(parse_max_rounds(Some(&Value::String("0".into()))), DEFAULT_GOAL_ROUNDS);
        assert_eq!(parse_max_rounds(Some(&Value::String("4".into()))), 4);
        assert_eq!(parse_max_rounds(Some(&serde_json::json!(4))), 4);
        assert_eq!(parse_max_rounds(Some(&Value::String("nope".into()))), DEFAULT_GOAL_ROUNDS);
    }

    #[test]
    fn a_stale_revision_is_refused_rather_than_overwriting() {
        // The human edited the goal while a round was thinking. The round
        // must lose, and must be told why.
        let g = goal();
        let err = apply(&g, 0, GoalAction::Complete, Some("done")).unwrap_err();
        assert!(err.contains("revision mismatch"), "{err}");
        assert!(err.contains(GET_GOAL_NAME), "{err}");
        assert!(apply(&g, 1, GoalAction::Complete, Some("done")).is_ok());
    }

    #[test]
    fn every_mutation_bumps_the_revision() {
        let g = goal();
        assert_eq!(apply(&g, 1, GoalAction::Pause, None).unwrap().revision, 2);
        assert_eq!(start_round(&g).revision, 2);
        assert_eq!(start_round(&g).rounds_started, 1);
    }

    #[test]
    fn blocking_demands_a_concrete_blocker() {
        let g = goal();
        assert!(apply(&g, 1, GoalAction::Block, None).is_err());
        assert!(apply(&g, 1, GoalAction::Block, Some("   ")).is_err());
        let blocked = apply(&g, 1, GoalAction::Block, Some(" no network access ")).unwrap();
        assert_eq!(blocked.phase, GoalPhase::Blocked);
        assert_eq!(blocked.blocker.as_deref(), Some("no network access"));
    }

    #[test]
    fn resuming_clears_a_stale_blocker_and_reopens_a_terminal_goal() {
        let blocked = apply(&goal(), 1, GoalAction::Block, Some("no network")).unwrap();
        // Terminal phases refuse everything except a resume.
        assert!(apply(&blocked, 2, GoalAction::Complete, Some("x")).is_err());
        let resumed = apply(&blocked, 2, GoalAction::Resume, None).unwrap();
        assert_eq!(resumed.phase, GoalPhase::Active);
        assert!(resumed.blocker.is_none(), "the blocker described a past state");
    }

    #[test]
    fn a_delegated_child_may_never_touch_the_goal() {
        for action in [GoalAction::Pause, GoalAction::Complete] {
            let err = authorize(action, &goal(), true, 1).unwrap_err();
            assert!(err.contains("top-level"), "{err}");
        }
    }

    #[test]
    fn an_automatic_round_cannot_pause_or_resume_its_own_goal() {
        let g = goal();
        assert!(authorize(GoalAction::Pause, &g, false, 0).is_err());
        assert!(authorize(GoalAction::Resume, &g, false, 0).is_err());
        assert!(authorize(GoalAction::Pause, &g, true, 0).is_ok());
    }

    #[test]
    fn an_automatic_round_may_complete_but_not_block_too_early() {
        let mut g = goal();
        assert!(authorize(GoalAction::Complete, &g, false, 0).is_ok(), "a round that finished says so");
        let err = authorize(GoalAction::Block, &g, false, 0).unwrap_err();
        assert!(err.contains("too early"), "{err}");
        // A human may block at any time.
        assert!(authorize(GoalAction::Block, &g, true, 0).is_ok());
        // And so may a round, once it has actually tried.
        g.rounds_started = BLOCKED_AFTER_CONSECUTIVE_ROUNDS;
        assert!(authorize(GoalAction::Block, &g, false, 0).is_ok());
    }

    #[test]
    fn rounds_stop_at_the_cap_and_in_every_non_active_phase() {
        let mut g = goal();
        g.rounds_started = g.max_rounds;
        assert!(!g.rounds_left());
        for phase in [GoalPhase::Paused, GoalPhase::Completed, GoalPhase::Blocked] {
            let mut g = goal();
            g.phase = phase;
            assert!(!g.rounds_left(), "{}", phase.label());
        }
    }

    #[test]
    fn the_round_prompt_carries_the_objective_the_count_and_the_revision() {
        let mut g = goal();
        g.rounds_started = 3;
        g.revision = 5;
        let p = round_prompt(&g);
        assert!(p.contains("make the tests pass"));
        assert!(p.contains("Round 3 of 8"));
        assert!(p.contains("revision 5"));
        assert!(p.contains("authoritative"), "the workspace, not its own narration");
        assert!(p.contains(UPDATE_GOAL_NAME));
    }

    #[test]
    fn actions_parse_from_the_words_a_model_actually_uses() {
        assert_eq!(GoalAction::parse("Pause").unwrap(), GoalAction::Pause);
        assert_eq!(GoalAction::parse("continue").unwrap(), GoalAction::Resume);
        assert_eq!(GoalAction::parse("done").unwrap(), GoalAction::Complete);
        assert_eq!(GoalAction::parse(" blocked ").unwrap(), GoalAction::Block);
        assert!(GoalAction::parse("abandon").is_err());
    }

    #[test]
    fn a_followup_carries_human_authority_but_a_round_does_not() {
        assert!(human_turn(TurnSource::Human));
        assert!(human_turn(TurnSource::Followup), "it is still the user's own message");
        assert!(!human_turn(TurnSource::GoalRound));
    }
}
