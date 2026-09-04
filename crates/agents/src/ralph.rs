//! `ralph` — fresh-agent rounds against one immutable objective
//! (Wave 4, guide §12.6).
//!
//! Every other delegation in this crate hands a child *context*. Ralph
//! deliberately hands it almost none: each round is a brand-new
//! conversation that sees no parent transcript and no previous child
//! session, only the objective and the previous round's **bounded,
//! validated** report. The workspace itself — files, working tree, tool
//! output — is the long-term memory.
//!
//! That is the whole point, and it is the part worth keeping even if the
//! tool is never called: *only a small validated struct crosses a context
//! boundary*. A round cannot inherit another round's confident narration,
//! so a mistake in round 3 does not quietly become a premise in round 7;
//! round 4 has to re-derive it from the workspace or drop it.
//!
//! The loop stops on `complete`, on `blocked`, at the round limit, or when
//! a round fails to produce a valid report. `status` is cross-field
//! validated in Rust ([`check_report`]) rather than trusted, because the
//! three statuses are exactly the claims a model is most tempted to make
//! without the evidence they require.

use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::registry::SkillRegistry;
use crate::runner::{self, RunSpec};
use crate::skill::{Skill, SkillContext, SkillOutcome};

pub const RALPH_NAME: &str = "ralph";

/// Rounds when the caller does not say. dsh's default.
const DEFAULT_ROUNDS: u8 = 8;
/// Hard ceiling regardless of what the caller asks for. Each round is a
/// full LLM conversation with its own tool budget.
const MAX_ROUNDS: u8 = 64;
/// Tool calls one round may make before it must report.
const MAX_ROUND_HOPS: u8 = 8;
/// Bound on the handoff between rounds. A report that grows without limit
/// turns Ralph back into one long conversation with extra steps.
const MAX_HANDOFF_CHARS: usize = 16 * 1024;

const RALPH_DESCRIPTION: &str = "Run fresh-agent iteration: repeated rounds \
    of a brand-new agent against one fixed objective, each seeing only the \
    workspace and the previous round's short report. Use only when the user \
    explicitly asks for Ralph or fresh-agent iteration — it spends one full \
    agent conversation per round.";

/// The report every round must return through `structured-output`.
pub fn report_schema() -> Value {
    json!({
        "type": "object",
        "required": ["status", "summary"],
        "properties": {
            "status": {
                "type": "string",
                "enum": ["continue", "complete", "blocked"],
            },
            "summary": {"type": "string"},
            "evidence": {
                "type": "array",
                "items": {"type": "string"},
            },
            "next_steps": {
                "type": "array",
                "items": {"type": "string"},
            },
            "blocker": {"type": "string"},
        },
    })
}

/// Cross-field rules the schema cannot express. Each one exists because the
/// corresponding status is a claim, and a claim without its evidence is the
/// failure this tool is built to prevent:
///
/// - `complete` without evidence is "I think I'm done".
/// - `complete` with outstanding steps contradicts itself.
/// - `continue` with no next step gives the following round nothing.
/// - `blocked` without a concrete blocker ends the loop on a shrug.
pub fn check_report(v: &Value) -> Result<Status, String> {
    let status = v.get("status").and_then(Value::as_str).unwrap_or("");
    let list = |k: &str| -> Vec<String> {
        v.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let evidence = list("evidence");
    let next_steps = list("next_steps");
    let blocker = v
        .get("blocker")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    match status {
        "complete" => {
            if evidence.is_empty() {
                return Err(
                    "`complete` needs at least one `evidence` entry naming what you \
                     ran or read that shows the objective is met"
                        .into(),
                );
            }
            if !next_steps.is_empty() {
                return Err(
                    "`complete` cannot list `next_steps` — if work remains, the \
                     status is `continue`"
                        .into(),
                );
            }
            Ok(Status::Complete)
        }
        "continue" => {
            if next_steps.is_empty() {
                return Err(
                    "`continue` needs at least one `next_steps` entry — the next \
                     round starts from nothing else"
                        .into(),
                );
            }
            if blocker.is_some() {
                return Err(
                    "`continue` cannot carry a `blocker` — if you are blocked, the \
                     status is `blocked`"
                        .into(),
                );
            }
            Ok(Status::Continue)
        }
        "blocked" => match blocker {
            Some(_) => Ok(Status::Blocked),
            None => Err(
                "`blocked` needs a concrete `blocker` saying exactly what stopped \
                 you and what would unblock it"
                    .into(),
            ),
        },
        other => Err(format!(
            "`status` must be continue | complete | blocked, got {other:?}"
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Continue,
    Complete,
    Blocked,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Continue => "continue",
            Status::Complete => "complete",
            Status::Blocked  => "blocked",
        }
    }
}

pub struct Ralph {
    registry: OnceLock<Weak<SkillRegistry>>,
}

impl Ralph {
    pub fn new() -> Self {
        Self { registry: OnceLock::new() }
    }

    /// Give rounds access to the live skill catalogue. Must be called after
    /// the registry is wrapped in its final `Arc`; calling it twice is a
    /// no-op.
    pub fn attach_registry(&self, registry: &Arc<SkillRegistry>) {
        let _ = self.registry.set(Arc::downgrade(registry));
    }

    fn round_registry(&self) -> Option<Arc<SkillRegistry>> {
        let live = self.registry.get().and_then(Weak::upgrade)?;
        Some(Arc::new(live.excluding(crate::control::CHILD_EXCLUDED)))
    }
}

impl Default for Ralph {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Skill for Ralph {
    fn name(&self) -> &str {
        RALPH_NAME
    }
    fn description(&self) -> &str {
        RALPH_DESCRIPTION
    }
    fn positional_args(&self) -> Vec<String> {
        vec!["objective".into()]
    }
    fn optional_args(&self) -> Vec<String> {
        vec!["max_rounds".into()]
    }
    /// Up to `MAX_ROUNDS` full agent conversations. The 120 s default would
    /// kill it in round one.
    fn timeout(&self) -> Duration {
        Duration::from_secs(60 * 60)
    }

    async fn run(&self, args: Value, ctx: SkillContext) -> SkillOutcome {
        let objective = match args.get("objective").and_then(Value::as_str) {
            Some(o) if !o.trim().is_empty() => o.trim().to_string(),
            _ => {
                return fail(
                    "missing or empty `objective` arg — state the one fixed goal \
                     every round works toward",
                )
            }
        };
        let max_rounds = parse_rounds(args.get("max_rounds"));
        let Some(client) = ctx.sub.summarizer.clone() else {
            return fail(
                "ralph needs an LLM but none is attached to this call — connect \
                 an LLM and retry",
            );
        };
        let registry = self.round_registry();
        let catalogue = registry
            .as_ref()
            .map(|r| r.catalogue_markdown_excluding(&[]))
            .filter(|c| !c.is_empty());
        let cancel = ctx.sub.cancel.clone();
        let schema = report_schema();

        info!(rounds = max_rounds, "ralph: starting");
        ctx.sub.events.emit(protocol::Event::LogLine {
            level:   "INFO".into(),
            message: format!(
                "ralph: up to {max_rounds} fresh round(s) — {}",
                crate::team::truncate_chars(&objective, 120)
            ),
        });

        let mut handoff: Option<Value> = None;
        let mut history: Vec<String> = Vec::new();
        let mut outcome = Status::Continue;
        let mut round: u8 = 0;

        while round < max_rounds {
            if runner::is_cancelled(&cancel) {
                return fail_with(
                    &objective,
                    round,
                    &history,
                    "interrupted — the workspace holds whatever the finished rounds left",
                );
            }
            round += 1;
            let spec = RunSpec {
                label:    format!("ralph round {round}/{max_rounds}"),
                system:   round_system(catalogue.as_deref()),
                seed:     Vec::new(), // fresh agent: no parent, no prior round
                task:     round_task(&objective, round, max_rounds, handoff.as_ref()),
                max_hops: MAX_ROUND_HOPS,
                schema:   Some(schema.clone()),
            };
            let mut transcript = runner::seed_transcript(&spec);
            let report = runner::run_conversation(
                &client, registry.as_ref(), &ctx.sub, &mut transcript, &spec, &cancel,
            )
            .await;

            let Some(report) = report else {
                warn!(round, "ralph: round produced nothing");
                return fail_with(
                    &objective, round, &history,
                    "a round produced no report (every LLM call failed, or the turn \
                     was interrupted)",
                );
            };
            let Some(value) = report.structured else {
                warn!(round, "ralph: round never reported through structured-output");
                return fail_with(
                    &objective, round, &history,
                    "a round never reported through `structured-output`, so its work \
                     cannot be handed on",
                );
            };
            let status = match check_report(&value) {
                Ok(s) => s,
                Err(problem) => {
                    warn!(round, %problem, "ralph: report failed the cross-field rules");
                    return fail_with(
                        &objective, round, &history,
                        &format!("a round's report was self-contradictory: {problem}"),
                    );
                }
            };

            let summary = value.get("summary").and_then(Value::as_str).unwrap_or("").trim();
            history.push(format!(
                "- round {round} [{}{}]: {}",
                status.label(),
                if report.tool_ok > 0 {
                    String::new()
                } else {
                    " · UNVERIFIED, no successful tool call".into()
                },
                crate::team::truncate_chars(summary, 400)
            ));
            ctx.sub.events.emit(protocol::Event::LogLine {
                level:   "INFO".into(),
                message: format!(
                    "ralph: round {round}/{max_rounds} → {} ({} tool call(s) ok)",
                    status.label(),
                    report.tool_ok
                ),
            });

            outcome = status;
            // Record the round's report before deciding whether to stop:
            // the closing message reads the blocker off it, and on a
            // `blocked` round that blocker is in *this* report, not the
            // previous one.
            handoff = Some(value);
            if status != Status::Continue {
                break;
            }
        }

        let closing = match outcome {
            Status::Complete => "objective reported complete".to_string(),
            Status::Blocked => format!(
                "stopped: blocked — {}",
                handoff
                    .as_ref()
                    .and_then(|v| v.get("blocker"))
                    .and_then(Value::as_str)
                    .unwrap_or("see the last round")
            ),
            Status::Continue => format!(
                "stopped at the round limit ({max_rounds}) with work still outstanding"
            ),
        };
        SkillOutcome {
            ok:      outcome != Status::Blocked,
            summary: render(&objective, round, &history, &closing),
        }
    }
}

/// `max_rounds` from the call, clamped. Accepts a JSON number or its text —
/// the text protocol only ever sends strings.
fn parse_rounds(v: Option<&Value>) -> u8 {
    let n = match v {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.trim().parse::<u64>().ok(),
        _ => None,
    };
    n.unwrap_or(DEFAULT_ROUNDS as u64).clamp(1, MAX_ROUNDS as u64) as u8
}

/// The system prompt every round gets. Identical each time — the round is
/// fresh, so nothing accumulates here either.
fn round_system(catalogue: Option<&str>) -> String {
    let mut charter = String::from(
        "You are one round of a fresh-agent iteration inside the sica-rust \
         desktop app. You have no memory of earlier rounds and no access to \
         their conversations.\n\n\
         The shared workspace — its files and its current working tree — is \
         the long-term memory and the source of truth. Inspect it before \
         acting. If you were handed a previous round's report, treat it \
         strictly as a bounded handoff: a claim in it is a lead to confirm \
         against the workspace, never a fact to build on.\n\n\
         Make concrete progress this round and verify the result before you \
         report it.\n\n",
    );
    if let Some(cat) = catalogue {
        charter.push_str(&format!(
            "To call a tool, reply with a SINGLE line of exactly this form and \
             nothing else:\n\n\
             <skill-name> '<arg1>' '<arg2>' > <what you want to learn>\n\n\
             One tool call per reply, at most {MAX_ROUND_HOPS} in this round.\n\n\
             Available skills:\n{cat}\n\n"
        ));
    }
    charter.push_str(
        "Grounding rule, and it is absolute: state a file's contents, a \
         command's output, or whether a path exists ONLY if a tool result in \
         this conversation shows it. Before reporting `complete`, gather the \
         evidence that shows it — a command you ran, a file you read — and \
         name it. Never report work you did not do.",
    );
    charter
}

/// The round's task: the immutable objective, where it sits in the run, and
/// the previous round's report, bounded.
fn round_task(objective: &str, round: u8, max_rounds: u8, handoff: Option<&Value>) -> String {
    let mut task = format!(
        "Objective (unchanged every round): {objective}\n\nRound {round} of {max_rounds}.\n"
    );
    match handoff {
        None => task.push_str(
            "\nThis is the first round — nothing has been handed to you. Start by \
             inspecting the workspace.\n",
        ),
        Some(prev) => {
            let text = serde_json::to_string_pretty(prev)
                .unwrap_or_else(|_| prev.to_string());
            task.push_str(&format!(
                "\nThe previous round reported this. It is a handoff, not \
                 evidence — confirm anything you rely on:\n\n```json\n{}\n```\n",
                crate::team::truncate_chars(&text, MAX_HANDOFF_CHARS)
            ));
        }
    }
    task.push_str(
        "\nWork the objective forward now, then report with `structured-output`: \
         `continue` (with next_steps) if work remains, `complete` (with evidence, \
         no next_steps) if the objective is met, `blocked` (with a concrete \
         blocker) if you cannot proceed.",
    );
    task
}

fn render(objective: &str, rounds: u8, history: &[String], closing: &str) -> String {
    let mut out = format!("# Ralph — {closing}\n\nObjective: {objective}\n\n");
    if history.is_empty() {
        out.push_str("No round completed.\n");
    } else {
        out.push_str(&format!("Rounds run: {rounds}\n\n"));
        out.push_str(&history.join("\n"));
        out.push('\n');
    }
    out.push_str(
        "\nThe workspace holds the actual work — this is only the round log. \
         Inspect it before reporting the result onward.",
    );
    out
}

fn fail_with(objective: &str, rounds: u8, history: &[String], why: &str) -> SkillOutcome {
    SkillOutcome {
        ok:      false,
        summary: render(objective, rounds, history, why),
    }
}

fn fail(msg: &str) -> SkillOutcome {
    SkillOutcome { ok: false, summary: msg.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::ToolSubAgent;
    use protocol::Event;

    struct Silent;
    impl crate::agent::EventSink for Silent {
        fn emit(&self, _ev: Event) {}
    }

    fn ctx() -> SkillContext {
        SkillContext { sub: ToolSubAgent::root(Arc::new(Silent)) }
    }

    #[test]
    fn a_well_formed_report_of_each_status_passes() {
        assert_eq!(
            check_report(&json!({
                "status": "complete", "summary": "done",
                "evidence": ["cargo test: 281 passed"]
            }))
            .unwrap(),
            Status::Complete
        );
        assert_eq!(
            check_report(&json!({
                "status": "continue", "summary": "part way",
                "next_steps": ["wire the dispatcher"]
            }))
            .unwrap(),
            Status::Continue
        );
        assert_eq!(
            check_report(&json!({
                "status": "blocked", "summary": "stuck",
                "blocker": "the API key is missing"
            }))
            .unwrap(),
            Status::Blocked
        );
    }

    #[test]
    fn complete_needs_evidence_and_cannot_carry_next_steps() {
        let e = check_report(&json!({"status": "complete", "summary": "s"})).unwrap_err();
        assert!(e.contains("evidence"), "{e}");
        let e = check_report(&json!({
            "status": "complete", "summary": "s",
            "evidence": ["x"], "next_steps": ["y"]
        }))
        .unwrap_err();
        assert!(e.contains("next_steps"), "{e}");
    }

    #[test]
    fn continue_needs_a_next_step_and_cannot_carry_a_blocker() {
        let e = check_report(&json!({"status": "continue", "summary": "s"})).unwrap_err();
        assert!(e.contains("next_steps"), "{e}");
        let e = check_report(&json!({
            "status": "continue", "summary": "s",
            "next_steps": ["y"], "blocker": "z"
        }))
        .unwrap_err();
        assert!(e.contains("blocker"), "{e}");
    }

    #[test]
    fn blocked_needs_a_concrete_blocker_not_an_empty_one() {
        let e = check_report(&json!({"status": "blocked", "summary": "s"})).unwrap_err();
        assert!(e.contains("blocker"), "{e}");
        let e = check_report(&json!({
            "status": "blocked", "summary": "s", "blocker": "   "
        }))
        .unwrap_err();
        assert!(e.contains("blocker"), "{e}");
    }

    #[test]
    fn blank_list_entries_do_not_satisfy_a_requirement() {
        let e = check_report(&json!({
            "status": "continue", "summary": "s", "next_steps": ["", "  "]
        }))
        .unwrap_err();
        assert!(e.contains("next_steps"), "{e}");
    }

    #[test]
    fn an_unknown_status_is_rejected() {
        assert!(check_report(&json!({"status": "done", "summary": "s"})).is_err());
        assert!(check_report(&json!({"summary": "s"})).is_err());
    }

    #[test]
    fn the_schema_and_the_cross_field_rules_agree_on_a_good_report() {
        let good = json!({"status": "continue", "summary": "s", "next_steps": ["a"]});
        assert!(runner::validate(&good, &report_schema(), "$").is_empty());
        assert!(check_report(&good).is_ok());
        // The schema alone cannot catch this one — that is why both run.
        let bad = json!({"status": "complete", "summary": "s"});
        assert!(runner::validate(&bad, &report_schema(), "$").is_empty());
        assert!(check_report(&bad).is_err());
    }

    #[test]
    fn rounds_are_clamped_and_accept_the_text_protocol_shape() {
        assert_eq!(parse_rounds(None), DEFAULT_ROUNDS);
        assert_eq!(parse_rounds(Some(&json!("3"))), 3);
        assert_eq!(parse_rounds(Some(&json!(3))), 3);
        assert_eq!(parse_rounds(Some(&json!(0))), 1);
        assert_eq!(parse_rounds(Some(&json!(9999))), MAX_ROUNDS);
        assert_eq!(parse_rounds(Some(&json!("nonsense"))), DEFAULT_ROUNDS);
    }

    #[test]
    fn the_first_round_gets_no_handoff_and_later_rounds_get_a_bounded_one() {
        let first = round_task("ship it", 1, 8, None);
        assert!(first.contains("first round"));
        assert!(first.contains("ship it"));
        let prev = json!({"status": "continue", "summary": "x".repeat(40_000)});
        let later = round_task("ship it", 2, 8, Some(&prev));
        assert!(later.contains("handoff, not"));
        assert!(
            later.len() < MAX_HANDOFF_CHARS + 2_000,
            "the handoff must stay bounded, got {}",
            later.len()
        );
    }

    #[test]
    fn the_round_prompt_names_the_workspace_as_the_source_of_truth() {
        let s = round_system(None);
        assert!(s.contains("source of truth"));
        assert!(s.contains("no memory of earlier rounds"));
        assert!(s.contains("Grounding rule"));
    }

    #[test]
    fn the_closing_line_reports_the_blocking_round_own_blocker() {
        // Regression: the handoff used to be recorded only for `continue`
        // rounds, so a `blocked` finish printed the *previous* round's
        // report (or "see the last round" when it blocked on round one).
        let blocking = json!({
            "status": "blocked", "summary": "s", "blocker": "no network access"
        });
        let closing = format!(
            "stopped: blocked — {}",
            blocking.get("blocker").and_then(Value::as_str).unwrap_or("see the last round")
        );
        let out = render("obj", 1, &["- round 1 [blocked]: s".into()], &closing);
        assert!(out.contains("no network access"), "{out}");
        assert!(!out.contains("see the last round"), "{out}");
    }

    #[tokio::test]
    async fn a_missing_objective_fails_before_needing_an_llm() {
        let out = Ralph::new().run(json!({}), ctx()).await;
        assert!(!out.ok);
        assert!(out.summary.contains("`objective`"), "{}", out.summary);
    }

    #[tokio::test]
    async fn without_an_llm_it_fails_cleanly() {
        let out = Ralph::new().run(json!({"objective": "ship it"}), ctx()).await;
        assert!(!out.ok);
        assert!(out.summary.contains("needs an LLM"), "{}", out.summary);
    }
}
