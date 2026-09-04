//! `subagent` and `subagent-fork` (Wave 4, guide §12.1).
//!
//! `ToolSubAgent` delegates one *tool call*; `agent-team` delegates a whole
//! roster. Between them sits the common case: hand one bounded task to one
//! fresh model conversation and get back a single report, so the main
//! agent's context never fills with the child's exploration.
//!
//! Two shapes, one implementation, differing only in what the child starts
//! from — and the description says which, because it decides how the caller
//! must write the task (dsh's `providerWording`):
//!
//! - **`subagent`** — an empty conversation. The child knows nothing the
//!   caller does not spell out, so the task must be self-contained.
//! - **`subagent-fork`** — seeded with the parent session's *completed*
//!   turns. The in-flight turn is deliberately excluded: forking mid-turn
//!   would hand the child a half-written exchange (an assistant message
//!   whose tool results have not landed yet), which reads as a truncated
//!   conversation and, in native mode, is an invalid request.
//!
//! Children run on a restricted registry ([`crate::control::CHILD_EXCLUDED`]):
//! no harness controls, and no further delegation — recursion would
//! otherwise only unwind at `ToolSubAgent::max_depth`, after spending a
//! full conversation at every level.

use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tracing::info;

use crate::registry::SkillRegistry;
use crate::runner::{self, RunSpec};
use crate::skill::{Skill, SkillContext, SkillOutcome};

pub const SUBAGENT_NAME: &str = "subagent";
pub const SUBAGENT_FORK_NAME: &str = "subagent-fork";

/// Tool calls one delegated child may make before it must answer with what
/// it has. Higher than a teammate's budget (a child works alone, so it
/// cannot lean on anyone else's findings) and well under the main agent's.
const MAX_CHILD_HOPS: u8 = 8;

/// Cap on the report crossing back. A child exists to *shrink* what reaches
/// the parent's context; an unbounded report defeats the point.
const MAX_REPORT_CHARS: usize = 6000;

const SUBAGENT_DESCRIPTION: &str = "Delegate one bounded task to a fresh \
    agent with an empty conversation, and get back a single report. It sees \
    nothing of this conversation, so write the task standalone: state the \
    goal, the paths, and what to report back.";

const SUBAGENT_FORK_DESCRIPTION: &str = "Delegate one bounded task to a child \
    agent that inherits this conversation's completed turns, and get back a \
    single report. It has read what you have read, so write the task as a \
    follow-up instruction rather than repeating context.";

/// One delegated-conversation tool. `fork` selects which of the two names,
/// descriptions and seeds this instance provides.
pub struct Subagent {
    /// Set once by the backend after the registry is built. `Weak` because
    /// the registry also holds this skill.
    registry: OnceLock<Weak<SkillRegistry>>,
    fork:     bool,
}

impl Subagent {
    /// A child starting from an empty conversation.
    pub fn fresh() -> Self {
        Self { registry: OnceLock::new(), fork: false }
    }

    /// A child seeded with the parent's completed turns.
    pub fn forking() -> Self {
        Self { registry: OnceLock::new(), fork: true }
    }

    /// Give children access to the live skill catalogue. Must be called
    /// after the registry is wrapped in its final `Arc`; calling it twice
    /// is a no-op.
    pub fn attach_registry(&self, registry: &Arc<SkillRegistry>) {
        let _ = self.registry.set(Arc::downgrade(registry));
    }

    /// The registry a child sees: the live one minus every skill a
    /// runtime-owned child must not reach.
    fn child_registry(&self) -> Option<Arc<SkillRegistry>> {
        let live = self.registry.get().and_then(Weak::upgrade)?;
        Some(Arc::new(live.excluding(crate::control::CHILD_EXCLUDED)))
    }
}

#[async_trait]
impl Skill for Subagent {
    fn name(&self) -> &str {
        if self.fork { SUBAGENT_FORK_NAME } else { SUBAGENT_NAME }
    }

    fn description(&self) -> &str {
        if self.fork { SUBAGENT_FORK_DESCRIPTION } else { SUBAGENT_DESCRIPTION }
    }

    fn positional_args(&self) -> Vec<String> {
        vec!["task".into()]
    }

    fn prompt_guidance(&self) -> Option<&'static str> {
        Some(
            "Delegate a self-contained investigation with subagent (or \
             subagent-fork to hand over this conversation) when the work \
             would otherwise flood your context; you get back one report.",
        )
    }

    /// A full child conversation with up to `MAX_CHILD_HOPS` tool calls.
    /// The 120 s default would kill it mid-exploration.
    fn timeout(&self) -> Duration {
        Duration::from_secs(15 * 60)
    }

    async fn run(&self, args: Value, ctx: SkillContext) -> SkillOutcome {
        let task = match args.get("task").and_then(|v| v.as_str()) {
            Some(t) if !t.trim().is_empty() => t.trim().to_string(),
            _ => return fail("missing or empty `task` arg — say what the child should do"),
        };
        let Some(client) = ctx.sub.summarizer.clone() else {
            return fail(
                "delegation needs an LLM but none is attached to this call — \
                 connect an LLM and retry",
            );
        };
        let registry = self.child_registry();
        let catalogue = registry
            .as_ref()
            .map(|r| r.catalogue_markdown_excluding(&[]))
            .filter(|c| !c.is_empty());

        // A fork with nothing to inherit is a `subagent` with a misleading
        // name: say so rather than silently running the other shape.
        let seed = if self.fork {
            match &ctx.sub.fork_seed {
                Some(s) if !s.is_empty() => s.as_ref().clone(),
                _ => {
                    return fail(
                        "nothing to fork — this conversation has no completed turn \
                         yet; use `subagent` with a self-contained task instead",
                    )
                }
            }
        } else {
            Vec::new()
        };

        let spec = RunSpec {
            label:    self.name().to_string(),
            system:   child_system(self.fork, catalogue.as_deref()),
            seed,
            task:     task.clone(),
            max_hops: MAX_CHILD_HOPS,
            schema:   None,
                    // Fresh conversation each time, so ids start at call-1.
            call_seq_start: 0,
};
        info!(
            skill = self.name(),
            seeded = spec.seed.len(),
            "delegate: starting child conversation"
        );
        ctx.sub.events.emit(protocol::Event::LogLine {
            level:   "INFO".into(),
            message: format!(
                "{}: delegating — {}",
                self.name(),
                crate::team::truncate_chars(&task, 120)
            ),
        });

        let mut transcript = runner::seed_transcript(&spec);
        let cancel = ctx.sub.cancel.clone();
        let Some(report) =
            runner::run_conversation(&client, registry.as_ref(), &ctx.sub, &mut transcript, &spec, &cancel)
                .await
        else {
            return fail(
                "the delegated child produced nothing (every LLM call failed, \
                 or the turn was interrupted)",
            );
        };

        // The caller sees only this string, so the grounding caveat has to
        // travel with it: an all-prose child run reads exactly like a
        // researched one otherwise.
        let mut out = String::new();
        if !report.verified(false) {
            out.push_str(
                "**UNVERIFIED — the child made no successful tool call, so nothing \
                 below was checked against the machine. Treat every claim about \
                 files, commands or output as its guess.**\n\n",
            );
        }
        out.push_str(crate::team::truncate_chars(report.text.trim(), MAX_REPORT_CHARS).trim_end());
        info!(
            skill = self.name(),
            tool_ok = report.tool_ok,
            tool_err = report.tool_err,
            hops = report.hops,
            "delegate: child finished"
        );
        SkillOutcome { ok: true, summary: out }
    }
}

/// The child's system prompt. Same grounding contract as a teammate's —
/// the failure mode is identical — plus the line that tells it which shape
/// it is, since that decides whether the seeded conversation is its own
/// work or someone else's.
fn child_system(fork: bool, catalogue: Option<&str>) -> String {
    let mut charter = String::from(
        "You are a delegated agent inside the sica-rust desktop app, working \
         on ONE task handed to you by the main agent. You cannot ask \
         questions and nobody reads anything but your final report.\n\n",
    );
    if fork {
        charter.push_str(
            "The conversation above is the parent session's completed turns, \
             handed to you as context. Anything it shows a tool having done \
             really happened; anything still pending did not reach you.\n\n",
        );
    }
    if let Some(cat) = catalogue {
        charter.push_str(&format!(
            "You may use tools. To call one, reply with a SINGLE line of \
             exactly this form and nothing else:\n\n\
             <skill-name> '<arg1>' '<arg2>' > <what you want to learn>\n\n\
             One tool call per reply, at most {MAX_CHILD_HOPS} in total. When \
             you have what you need, reply with your final report as plain \
             text containing no tool-call line.\n\n\
             Available skills:\n{cat}\n\n"
        ));
    }
    charter.push_str(
        "Keep your final report concise and factual. Quote exact values \
         (numbers, paths, errors) verbatim from tool output, and name the \
         files you actually opened. Start directly with content — no \
         preamble.\n\n\
         Grounding rule, and it is absolute: state a file's contents, a \
         command's output, or whether a path exists ONLY if a tool result in \
         this conversation shows it. You cannot see the disk otherwise. If \
         you have not run the tool, write `unverified:` in front of the claim \
         and name the call you would need — never invent output, and never \
         report a file as existing because the name sounds plausible.",
    );
    charter
}

fn fail(msg: &str) -> SkillOutcome {
    SkillOutcome { ok: false, summary: msg.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::ToolSubAgent;
    use protocol::Event;
    use std::sync::Mutex;

    struct Silent;
    impl crate::agent::EventSink for Silent {
        fn emit(&self, _ev: Event) {}
    }

    struct Capture(Mutex<Vec<Event>>);
    impl crate::agent::EventSink for Capture {
        fn emit(&self, ev: Event) {
            self.0.lock().unwrap().push(ev);
        }
    }

    fn ctx() -> SkillContext {
        SkillContext { sub: ToolSubAgent::root(Arc::new(Silent)) }
    }

    #[test]
    fn the_two_shapes_differ_in_name_and_in_what_the_task_must_say() {
        let fresh = Subagent::fresh();
        let fork = Subagent::forking();
        assert_eq!(fresh.name(), SUBAGENT_NAME);
        assert_eq!(fork.name(), SUBAGENT_FORK_NAME);
        // The description is what tells the caller how to write the task.
        assert!(fresh.description().contains("standalone"));
        assert!(fork.description().contains("inherits"));
        assert_ne!(fresh.description(), fork.description());
    }

    #[tokio::test]
    async fn missing_task_fails_before_needing_an_llm() {
        let out = Subagent::fresh().run(serde_json::json!({}), ctx()).await;
        assert!(!out.ok);
        assert!(out.summary.contains("`task`"), "{}", out.summary);
        let out = Subagent::fresh()
            .run(serde_json::json!({"task": "   "}), ctx())
            .await;
        assert!(!out.ok);
    }

    #[tokio::test]
    async fn without_an_llm_it_fails_cleanly_rather_than_hanging() {
        let out = Subagent::fresh()
            .run(serde_json::json!({"task": "look at README"}), ctx())
            .await;
        assert!(!out.ok);
        assert!(out.summary.contains("needs an LLM"), "{}", out.summary);
    }

    #[tokio::test]
    async fn forking_with_no_completed_turn_says_so_instead_of_running_fresh() {
        // `fork_seed` is unset: silently behaving like `subagent` would hide
        // that the child never saw the conversation the caller assumed.
        let cap = Arc::new(Capture(Mutex::new(Vec::new())));
        let sub = ToolSubAgent::root(cap).with_summarizer(llm::client::LlmClient::new(
            "http://127.0.0.1:1",
            "m",
            None,
        ));
        let out = Subagent::forking()
            .run(serde_json::json!({"task": "continue"}), SkillContext { sub })
            .await;
        assert!(!out.ok);
        assert!(out.summary.contains("nothing to fork"), "{}", out.summary);
    }

    #[test]
    fn child_system_states_the_grounding_rule_and_the_seed_provenance() {
        let fresh = child_system(false, Some("- **read-file** ('path') — read"));
        assert!(fresh.contains("Grounding rule"));
        assert!(fresh.contains("read-file"));
        assert!(!fresh.contains("parent session's completed turns"));
        let forked = child_system(true, None);
        assert!(forked.contains("parent session's completed turns"));
        assert!(forked.contains("Grounding rule"));
    }

    #[test]
    fn children_cannot_delegate_further_or_reach_harness_controls() {
        for name in [SUBAGENT_NAME, SUBAGENT_FORK_NAME, crate::team::AGENT_TEAM_NAME] {
            assert!(
                crate::control::CHILD_EXCLUDED.contains(&name),
                "{name} must be hidden from a delegated child"
            );
        }
        assert!(crate::control::CHILD_EXCLUDED.contains(&crate::control::ASK_USER_NAME));
    }
}
