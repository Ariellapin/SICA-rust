//! `agent-team`: coordinate a small team of LLM teammates on one goal.
//!
//! Where `ToolSubAgent` wraps exactly one tool invocation, this skill spawns
//! several *LLM-backed* teammates, each with its own role charter and its own
//! conversation transcript. Teammates run concurrently within a round, may
//! call any registered skill through the normal `ToolSubAgent` machinery
//! (so their tool calls show up in the FE as children of the `agent-team`
//! chip and inherit depth/cancellation), and — when `rounds > 1` — see a
//! shared *team board* of everyone's previous report before refining their
//! own. A final team-lead pass merges the reports into one deliverable.
//!
//! The LLM client is taken from `SkillContext` (the `ToolSubAgent`'s
//! summarizer slot, which the backend fills with the connected client). The
//! `SkillRegistry` is attached after registry construction via
//! [`AgentTeam::attach_registry`] — a `Weak` reference, because the registry
//! also owns this skill and an `Arc` cycle would never free either.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use futures::future::join_all;
use protocol::Event;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::info;

use llm::client::{ChatMessage, LlmClient};

use crate::registry::SkillRegistry;
use crate::runner;
use crate::skill::{Skill, SkillContext, SkillOutcome};
use crate::subagent::ToolSubAgent;
use sica_core::event::RunState;

pub const AGENT_TEAM_NAME: &str = "agent-team";

pub const AGENT_TEAM_DESCRIPTION: &str =
    "Run a team of role-based LLM teammates concurrently on one goal. \
     Positional args: <plan> — either `role: task || role: task` or a JSON \
     object {\"team\":[{\"role\",\"task\"}],\"shared\",\"rounds\"}. Teammates \
     can call skills; multi-round runs share a team board; a lead merges the \
     reports.";

/// Hard cap on team size: each teammate is a full LLM conversation, so the
/// cost grows linearly and a runaway plan should not fan out unbounded.
const MAX_TEAMMATES: usize = 6;
/// Rounds beyond the third add latency faster than quality on small models.
const MAX_ROUNDS: u8 = 3;
/// Tool calls a single teammate may make within one round.
const MAX_TEAMMATE_HOPS: u8 = 4;
/// Per-report cap when assembling the final outcome (the outer summarizer
/// re-condenses anything over 2 KB against the caller's expectation anyway).
const MAX_REPORT_CHARS: usize = 6000;
/// Per-report cap on the inter-round team board — the board is prompt input
/// for every teammate, so it must stay far smaller than the reports.
const BOARD_REPORT_CHARS: usize = 1500;

pub const AGENT_TEAM_SEED_MD: &str = r#"---
name: agent-team
description: Coordinate a team of role-based LLM teammates working concurrently on one goal.
---
Spawn a small team of LLM teammates. Each teammate gets its own role, its own
task, and its own conversation; teammates run **concurrently** and may call
any loaded skill. Use this when a goal splits into independent sub-tasks
(research + implementation + review, or several files to inspect at once).

Invocation (single line):

    agent-team '<plan>' > <what you want from the team>

The plan is either compact text — teammates separated by `||`, each `role: task`:

    agent-team 'researcher: list the crates in this workspace and what each does || critic: read README.md and report gaps' > one merged report

or a JSON object for full control:

    agent-team '{"team":[{"role":"researcher","task":"..."},{"role":"coder","task":"..."}],"shared":"context every teammate sees","rounds":2}' > merged answer

Behaviour:
- Up to **6** teammates run concurrently, each as its own LLM conversation.
- Teammates may call any loaded skill (same one-line syntax), up to **4**
  tool calls each per round; their calls appear nested under the team call.
- Each teammate reports through the `structured-output` tool; that call is
  the report, not a tool call, and does not use up the budget.
- `rounds` (1–3, default 1): after each round every teammate sees the shared
  *team board* (everyone's report) and coordinates/refines in the next round.
- `shared` (optional): briefing text prepended to every teammate's charter.
- A team-lead pass merges all reports into one deliverable; the individual
  reports are appended after it.
- Interrupting the turn stops the whole team immediately.

Grounding: a teammate does not write prose — it reports a list of **claims**,
each citing the ids (`call-1`, `call-2`) of the tool results that back it.
A claim citing nothing, or citing an id that names no successful call, is
rendered as `unverified:` everywhere it appears, and the lead is told never
to restate it as fact. A teammate whose claims are all uncited is headed
**UNVERIFIED**; if no teammate cited anything the whole result carries a
warning banner.
**This file is the on/off switch.** The backend registers `agent-team` only
when `skills/agent-team.md` exists; rename it to `agent-team.md.off` (only
`*.md` is scanned) or delete it, restart the backend, and the skill vanishes
from the catalogue. It is not seeded automatically.
"#;

/// The shape every teammate must report in (guide §12.2).
///
/// Prose is the wrong type for a teammate report: "src/lib.rs defines
/// `Registry`" and "I never opened src/lib.rs" are the same string shape,
/// so the lead cannot tell them apart and launders both into the
/// deliverable. Splitting the report into claims that each cite the tool
/// call backing them makes the difference *checkable* — and because the
/// cited ids come from [`runner::CallRecord`], a citation is verified
/// against calls that really ran rather than taken on the model's word.
pub(crate) fn teammate_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "required": ["claims"],
        "properties": {
            "claims": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "required": ["text", "evidence"],
                    "properties": {
                        "text": {"type": "string"},
                        "evidence": {"type": "array", "items": {"type": "string"}}
                    }
                }
            },
            "open_questions": {"type": "array", "items": {"type": "string"}}
        }
    })
}

/// Render a validated teammate report as the markdown the board, the lead
/// and the final summary all read, and count how many claims are actually
/// backed. A claim citing an id that names no *successful* call in this
/// run is marked as loudly as one citing nothing at all — a fabricated
/// citation is worse than a missing one.
fn render_claims(structured: &Value, ok_ids: &HashSet<&str>) -> (String, usize, usize) {
    let claims = structured.get("claims").and_then(Value::as_array);
    let mut out = String::new();
    let mut total = 0usize;
    let mut cited = 0usize;
    for claim in claims.into_iter().flatten() {
        total += 1;
        let text = claim.get("text").and_then(Value::as_str).unwrap_or("").trim();
        let evidence: Vec<&str> = claim
            .get("evidence")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let good: Vec<&str> = evidence
            .iter()
            .copied()
            .filter(|e| ok_ids.contains(citation_id(e).as_str()))
            .collect();
        if good.is_empty() {
            cited += 0;
            out.push_str(&format!(
                "- unverified: {text}  ({})\n",
                if evidence.is_empty() {
                    "no evidence cited".to_string()
                } else {
                    format!("cites {}, which named no successful tool call", evidence.join(", "))
                }
            ));
        } else {
            cited += 1;
            out.push_str(&format!("- {text}  [{}]\n", good.join(", ")));
        }
    }
    let questions: Vec<&str> = structured
        .get("open_questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|q| !q.trim().is_empty())
        .collect();
    if !questions.is_empty() {
        out.push_str("\nOpen questions:\n");
        for q in questions {
            out.push_str(&format!("- {}\n", q.trim()));
        }
    }
    (out, total, cited)
}

/// Models cite a call as `call-2`, as `` `call-2` ``, or as
/// `call-2 (read-file)`. Take the id and ignore the decoration rather than
/// failing an otherwise honest citation.
fn citation_id(raw: &str) -> String {
    raw.split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .to_string()
}

/// A member's id within the run: unique across rounds, so a round-2 row
/// cannot close a round-1 member, and derived rather than counted so the
/// same teammate keeps a recognisable identity.
fn member_id(round: u8, index: usize) -> u64 {
    round as u64 * 1_000 + index as u64
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Teammate {
    pub role: String,
    pub task: String,
}

/// What one teammate produced in a round, plus how much of it is grounded.
///
/// The counts exist because a teammate's prose is indistinguishable from
/// fact once it reaches the lead: a model that never called `read-file` will
/// still happily report what a file "contains". Since Wave 4 the report is
/// *typed* (`teammate_schema`) so grounding is measured per claim rather
/// than per teammate — `cited` counts the claims backed by a tool call that
/// really succeeded — and every consumer (the board, the lead, the final
/// summary) says so out loud. `structured == false` means the teammate
/// never delivered a valid report and we fell back to its prose, which is
/// the weakest outcome there is.
#[derive(Debug, Clone, Default)]
pub(crate) struct TeammateOutcome {
    pub report:     String,
    pub tool_ok:    u32,
    pub tool_err:   u32,
    /// Claims in the typed report, and how many cite a successful call.
    pub claims:     usize,
    pub cited:      usize,
    pub structured: bool,
}

impl TeammateOutcome {
    /// Grounded means *something in the report is backed*: at least one
    /// claim citing a call that ran and succeeded. A prose fallback can
    /// only fall back to the old, coarser test.
    fn verified(&self) -> bool {
        if self.structured {
            self.cited > 0
        } else {
            self.tool_ok > 0
        }
    }

    /// Suffix appended to this teammate's heading wherever the report is
    /// shown. Empty only when every claim is cited; otherwise it says *why*
    /// the reader should be sceptical, since "never tried", "tried and every
    /// call errored", "answered in prose" and "three of five claims cite
    /// nothing" call for different amounts of it.
    fn provenance(&self) -> String {
        if self.structured {
            return if self.claims == 0 {
                " — UNVERIFIED (reported no claims)".to_string()
            } else if self.cited == self.claims {
                String::new()
            } else if self.cited > 0 {
                format!(
                    " — PARTLY VERIFIED ({}/{} claims cite a successful tool call)",
                    self.cited, self.claims
                )
            } else {
                " — UNVERIFIED (no claim cites a successful tool call)".to_string()
            };
        }
        if self.tool_ok > 0 {
            format!(
                " — UNSTRUCTURED ({} tool call(s) succeeded, but claims are not cited)",
                self.tool_ok
            )
        } else if self.tool_err > 0 {
            format!(
                " — UNVERIFIED (all {} tool call(s) failed)",
                self.tool_err
            )
        } else {
            " — UNVERIFIED (no tool call made)".to_string()
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TeamSpec {
    pub teammates: Vec<Teammate>,
    pub shared:    String,
    pub rounds:    u8,
}

pub struct AgentTeam {
    /// Set once by the backend after the registry is built. `Weak` because
    /// the registry also holds this skill.
    registry: OnceLock<Weak<SkillRegistry>>,
}

impl AgentTeam {
    pub fn new() -> Self {
        Self { registry: OnceLock::new() }
    }

    /// Give teammates access to the live skill catalogue. Must be called
    /// after the registry is wrapped in its final `Arc`; calling it twice is
    /// a no-op.
    pub fn attach_registry(&self, registry: &Arc<SkillRegistry>) {
        let _ = self.registry.set(Arc::downgrade(registry));
    }

    fn registry(&self) -> Option<Arc<SkillRegistry>> {
        self.registry.get().and_then(Weak::upgrade)
    }
}

impl Default for AgentTeam {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Skill for AgentTeam {
    fn name(&self) -> &str { AGENT_TEAM_NAME }
    fn description(&self) -> &str { AGENT_TEAM_DESCRIPTION }
    fn positional_args(&self) -> Vec<String> { vec!["plan".into()] }
    /// Up to 6 teammates × 3 rounds × 4 hops of LLM traffic plus a lead pass.
    fn timeout(&self) -> std::time::Duration { std::time::Duration::from_secs(1800) }

    async fn run(&self, args: Value, ctx: SkillContext) -> SkillOutcome {
        let spec = match parse_spec(&args) {
            Ok(s)  => s,
            Err(e) => return fail(e),
        };
        let Some(client) = ctx.sub.summarizer.clone() else {
            return fail(
                "agent-team needs an LLM but none is attached to this call — \
                 connect an LLM and retry"
                    .into(),
            );
        };
        let registry = self.registry();
        // Teammates run with a restricted view: no harness controls and
        // no nested teams. A runtime-owned child cannot ask the user,
        // rewrite the todo list, exit the parent's plan, or spawn its own
        // team — those needs arrive via its final report instead.
        let registry =
            registry.map(|r| Arc::new(r.excluding(crate::control::CHILD_EXCLUDED)));
        let catalogue = registry
            .as_ref()
            .map(|r| r.catalogue_markdown_excluding(crate::control::CHILD_EXCLUDED))
            .filter(|c| !c.is_empty());
        let cancel = ctx.sub.cancel.clone();

        let roles: Vec<&str> = spec.teammates.iter().map(|t| t.role.as_str()).collect();
        info!(
            teammates = spec.teammates.len(),
            rounds = spec.rounds,
            roles = ?roles,
            "agent-team: starting"
        );
        ctx.sub.events.emit(Event::LogLine {
            level: "INFO".into(),
            message: format!(
                "agent-team: {} teammate(s) [{}], {} round(s)",
                spec.teammates.len(),
                roles.join(", "),
                spec.rounds
            ),
        });

        // One transcript per teammate, kept across rounds so a teammate
        // remembers its own tool results and earlier drafts.
        let mut transcripts: Vec<Vec<ChatMessage>> = spec
            .teammates
            .iter()
            .map(|m| {
                vec![
                    ChatMessage::text(
                        "system",
                        teammate_system(m, &spec, catalogue.as_deref()),
                    ),
                    ChatMessage::text("user", format!("Your task: {}", m.task)),
                ]
            })
            .collect();
        let mut reports: Vec<Option<TeammateOutcome>> = vec![None; spec.teammates.len()];
        // Evidence trail per teammate, accumulated across rounds so call ids
        // stay unique and stay resolvable.
        let mut trails: Vec<Vec<runner::CallRecord>> =
            vec![Vec::new(); spec.teammates.len()];

        // The run opens before the first round (§6.11). A team's rounds are
        // its phases — they are stages of one run, which is what a phase is
        // — and each teammate is a member of the round it ran in.
        let run_id = crate::subagent::next_run_id();
        ctx.sub.run_edge(run_id, None, None, RunState::Started);

        for round in 1..=spec.rounds {
            if runner::is_cancelled(&cancel) {
                break;
            }
            let phase = format!("round {round}");
            if round > 1 {
                let board = render_board(round - 1, &spec.teammates, &reports);
                for t in transcripts.iter_mut() {
                    t.push(ChatMessage::text(
                        "user",
                        format!(
                            "{board}\n\nAbove is what every teammate produced \
                             last round. Coordinate: avoid duplicating their \
                             work, resolve any conflicts with your own \
                             findings, and report again. You may make tool \
                             calls first. A line starting `unverified:` was \
                             not backed by any tool result — treat it as a \
                             guess and check anything you intend to build on. \
                             The `[call-N]` ids in another teammate's report \
                             belong to its conversation, not yours: cite only \
                             ids from your own tool results."
                        ),
                    ));
                }
            }

            // Concurrent within the round: each future owns a disjoint
            // `&mut` transcript; tool calls funnel through the shared
            // `ctx.sub` (ids are atomic, events are an Arc).
            let round_futs: Vec<_> = transcripts
                .iter_mut()
                .zip(spec.teammates.iter())
                .zip(trails.iter_mut())
                .map(|((transcript, mate), trail)| {
                    run_teammate(
                        &client,
                        registry.as_ref(),
                        &ctx.sub,
                        mate,
                        transcript,
                        trail,
                        &cancel,
                    )
                })
                .collect();
            // Every member of this round opens before any of them runs:
            // they are concurrent, and a reader watching the tree should see
            // the whole round light up at once rather than in completion
            // order.
            for (i, mate) in spec.teammates.iter().enumerate() {
                ctx.sub.run_edge(
                    run_id,
                    Some(&phase),
                    Some((member_id(round, i), &mate.role)),
                    RunState::Started,
                );
            }
            let results = join_all(round_futs).await;
            for (i, (slot, res)) in reports.iter_mut().zip(results).enumerate() {
                let role = spec.teammates[i].role.as_str();
                // A round that produced nothing is a failed member; the
                // previous round's report stands, which is why the *run* can
                // still succeed with a failed member in it.
                ctx.sub.run_edge(
                    run_id,
                    Some(&phase),
                    Some((member_id(round, i), role)),
                    if res.is_some() { RunState::Done } else { RunState::Failed },
                );
                // A failed refinement round keeps the previous round's report.
                if let Some(r) = res {
                    *slot = Some(r);
                }
            }
            ctx.sub.events.emit(Event::LogLine {
                level: "INFO".into(),
                message: format!(
                    "agent-team: round {round}/{} done — {}/{} report(s) in, {} backed by tool output",
                    spec.rounds,
                    reports.iter().filter(|r| r.is_some()).count(),
                    reports.len(),
                    reports.iter().flatten().filter(|r| r.verified()).count(),
                ),
            });
        }

        if runner::is_cancelled(&cancel) {
            // The run is left **open**: an interrupted run is visible
            // because its terminal row is missing, which is the whole
            // reason there are four rows rather than one summary (§6.11).
            return fail("agent-team interrupted".into());
        }
        if reports.iter().all(Option::is_none) {
            ctx.sub.run_edge(run_id, None, None, RunState::Failed);
            return fail(
                "agent-team: no teammate produced a report (every LLM call \
                 failed or returned nothing)"
                    .into(),
            );
        }
        ctx.sub.run_edge(run_id, None, None, RunState::Done);

        // Lead synthesis merges the reports; pointless for a team of one.
        let synthesis = if spec.teammates.len() > 1 {
            synthesize(&client, &cancel, &spec, &reports).await
        } else {
            None
        };

        let mut out = String::new();
        // The caller (main agent, or the sub-agent summarizer) sees only this
        // string, so the grounding caveat has to travel with it. Without the
        // banner an all-prose team run reads exactly like a researched one.
        if reports.iter().flatten().all(|r| !r.verified()) {
            out.push_str(
                "**Warning: not one claim below cites a tool result that ran \
                 successfully — nothing here was checked against the machine. \
                 Treat every statement about files, commands or output as \
                 unverified.**\n\n",
            );
        }
        match &synthesis {
            Some(s) => {
                out.push_str("## Team result\n\n");
                out.push_str(s.trim());
                out.push_str("\n\n## Teammate reports\n");
            }
            None if spec.teammates.len() > 1 => {
                out.push_str(
                    "(lead synthesis unavailable — individual reports below)\n\n\
                     ## Teammate reports\n",
                );
            }
            None => {}
        }
        for (mate, report) in spec.teammates.iter().zip(&reports) {
            if spec.teammates.len() > 1 || synthesis.is_some() {
                out.push_str(&format!(
                    "\n### {}{}\n\n",
                    mate.role,
                    report.as_ref().map(TeammateOutcome::provenance).unwrap_or_default()
                ));
            }
            match report {
                Some(r) => {
                    out.push_str(truncate_chars(r.report.trim(), MAX_REPORT_CHARS).trim_end())
                }
                None => out.push_str("(no report — LLM calls failed)"),
            }
            out.push('\n');
        }

        SkillOutcome { ok: true, summary: out.trim().to_string() }
    }
}

/// One teammate's turn within a round, delegated to the shared conversation
/// runner (`agents::runner`): chat, dispatch at most `MAX_TEAMMATE_HOPS`
/// tool calls, and return the final report with its tool-call tally.
/// `None` means every LLM call failed (or the turn was interrupted) — the
/// caller keeps whatever report the previous round produced.
///
/// The runner owns the grounding rules a teammate needs (one corrective
/// nudge for a botched tool call before its reply is accepted, and the
/// unverified tally), because `subagent` and `ralph` need exactly the same
/// ones. Everything team-specific — the roster, the board, the lead pass —
/// stays here.
async fn run_teammate(
    client:     &LlmClient,
    registry:   Option<&Arc<SkillRegistry>>,
    sub:        &ToolSubAgent,
    mate:       &Teammate,
    transcript: &mut Vec<ChatMessage>,
    calls:      &mut Vec<runner::CallRecord>,
    cancel:     &Option<CancellationToken>,
) -> Option<TeammateOutcome> {
    // The transcript is already seeded (and carried across rounds), so the
    // spec only supplies the label, the hop budget and the schema. `calls`
    // is the teammate's whole evidence trail so far: ids continue across
    // rounds, and a round-2 claim may legitimately cite a round-1 result
    // that is still in this transcript.
    let spec = runner::RunSpec {
        label:          format!("agent-team `{}`", mate.role),
        system:         String::new(),
        seed:           Vec::new(),
        task:           String::new(),
        max_hops:       MAX_TEAMMATE_HOPS,
        schema:         Some(teammate_schema()),
        call_seq_start: calls.len(),
    };
    let report =
        runner::run_conversation(client, registry, sub, transcript, &spec, cancel).await?;
    calls.extend(report.calls.iter().cloned());

    let tool_ok  = calls.iter().filter(|c| c.ok).count() as u32;
    let tool_err = calls.len() as u32 - tool_ok;
    let ok_ids: HashSet<&str> =
        calls.iter().filter(|c| c.ok).map(|c| c.id.as_str()).collect();

    Some(match &report.structured {
        Some(value) => {
            let (rendered, claims, cited) = render_claims(value, &ok_ids);
            TeammateOutcome {
                report: rendered,
                tool_ok,
                tool_err,
                claims,
                cited,
                structured: true,
            }
        }
        // The runner already nudged once; prose here means the teammate
        // could not produce a typed report at all. Keep the text — it may
        // still be useful — but never let it pass as a cited report.
        None => TeammateOutcome {
            report: report.text,
            tool_ok,
            tool_err,
            claims: 0,
            cited: 0,
            structured: false,
        },
    })
}

/// Team-lead pass: merge every report into one deliverable. Best-effort —
/// `None` on any LLM failure, and the caller falls back to raw reports.
async fn synthesize(
    client:  &LlmClient,
    cancel:  &Option<CancellationToken>,
    spec:    &TeamSpec,
    reports: &[Option<TeammateOutcome>],
) -> Option<String> {
    let mut body = String::new();
    if !spec.shared.trim().is_empty() {
        body.push_str(&format!("Team briefing: {}

", spec.shared.trim()));
    }
    for (mate, report) in spec.teammates.iter().zip(reports) {
        body.push_str(&format!(
            "## Report from `{}`{} (task: {})
{}

",
            mate.role,
            report.as_ref().map(TeammateOutcome::provenance).unwrap_or_default(),
            truncate_chars(&mate.task, 200),
            match report {
                Some(r) => truncate_chars(r.report.trim(), MAX_REPORT_CHARS),
                None    => "(no report)".into(),
            }
        ));
    }
    let system = "You are the lead of a small agent team. Merge your \
                  teammates' reports into ONE coherent deliverable. Each \
                  report is a list of claims: a claim ending in a `[call-N]` \
                  citation was checked against a tool result that really \
                  ran, and a line starting `unverified:` was not. Keep every \
                  cited fact (numbers, paths, versions, errors) verbatim, \
                  drop duplication, and flag any point where two reports \
                  contradict each other instead of silently picking one. \
                  Never restate an unverified claim as established fact: \
                  either attribute it (\"`role` believes …, unverified\") or \
                  leave it out. Carry open questions through as open \
                  questions. Output only the merged result — no preamble.";
    let messages = vec![
        ChatMessage::text("system", system),
        ChatMessage::text("user", body),
    ];
    runner::llm_call(client, cancel, messages).await
}


/// Role charter fed to one teammate as its system message. Rendered through
/// the shared prompt assembly (persona section at the `MEMORY` slot) so all
/// three builders in the codebase produce the same skeleton.
fn teammate_system(mate: &Teammate, spec: &TeamSpec, catalogue: Option<&str>) -> String {
    let mut charter = format!(
        "You are `{}`, one teammate on a small agent team inside the \
         sica-rust desktop app. Work ONLY on your own task; trust your \
         teammates to handle theirs.\n\nTeam roster:\n",
        mate.role
    );
    for m in &spec.teammates {
        charter.push_str(&format!(
            "- {}: {}{}\n",
            m.role,
            truncate_chars(&m.task, 120),
            if m.role == mate.role { "  (you)" } else { "" }
        ));
    }
    if !spec.shared.trim().is_empty() {
        charter.push_str(&format!("\nShared briefing: {}\n", spec.shared.trim()));
    }
    if let Some(cat) = catalogue {
        charter.push_str(&format!(
            "\nYou may use tools. To call one, reply with a SINGLE line of \
             exactly this form and nothing else:\n\n\
             <skill-name> '<arg1>' '<arg2>' > <what you want to learn>\n\n\
             One tool call per reply, at most {MAX_TEAMMATE_HOPS} per round. \
             Every tool result you receive is labelled with an id like \
             `[id: call-2]` — quote those ids as the evidence for your \
             claims.\n\n\
             Available skills:\n{cat}"
        ));
    }
    charter.push_str(
        "\nGrounding rule, and it is absolute: state a file's contents, a \
         command's output, or whether a path exists ONLY if a tool result in \
         this conversation shows it. You cannot see the disk otherwise. A \
         claim with no evidence id is reported to the team lead as \
         unverified, and an evidence id that names no successful tool result \
         is reported the same way — inventing an id is worse than admitting \
         you did not check. Quote exact values (numbers, paths, errors) \
         verbatim from tool output.",
    );
    charter.push_str(&crate::runner::structured_directive(&teammate_schema()));
    charter.push_str(
        "\n\nOne claim per finding, each with the ids of the tool results \
         that back it (`\"evidence\": [\"call-1\", \"call-3\"]`). Put anything \
         you could not check into `open_questions` instead of asserting it.",
    );
    let mut a = crate::prompt::Assembly::new();
    a.section(crate::prompt::Section::new(
        "persona",
        crate::prompt::order::MEMORY,
        charter.clone(),
    ));
    a.render().map(|r| r.system).unwrap_or(charter)
}

/// The shared board every teammate sees at the start of round `round + 1`.
fn render_board(round: u8, teammates: &[Teammate], reports: &[Option<TeammateOutcome>]) -> String {
    let mut out = format!("## Team board — end of round {round}\n");
    for (mate, report) in teammates.iter().zip(reports) {
        out.push_str(&format!(
            "\n### {}{}\n{}\n",
            mate.role,
            report.as_ref().map(TeammateOutcome::provenance).unwrap_or_default(),
            match report {
                Some(r) => truncate_chars(r.report.trim(), BOARD_REPORT_CHARS),
                None    => "(no report yet)".into(),
            }
        ));
    }
    out
}

/// Parse the `args` object into a validated `TeamSpec`.
///
/// Accepted shapes, in order:
/// 1. `{"team":[...], ...}` directly in the args object (JSON-fence calls).
/// 2. `plan` as a JSON string — object with `team`, or a bare array.
/// 3. `plan` as compact text: teammates split on `||`, each `role: task`.
fn parse_spec(args: &Value) -> Result<TeamSpec, String> {
    if args.get("team").is_some() {
        return spec_from_json(args);
    }
    let plan = args
        .get("plan")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "missing `plan` arg — pass `role: task || role: task` or a JSON \
             object with a `team` array"
                .to_string()
        })?;

    if plan.starts_with('{') || plan.starts_with('[') {
        let value: Value = serde_json::from_str(plan)
            .map_err(|e| format!("plan looks like JSON but does not parse: {e}"))?;
        return match value {
            Value::Array(_) => spec_from_json(&serde_json::json!({ "team": value })),
            _ => spec_from_json(&value),
        };
    }

    let mut teammates = Vec::new();
    for (i, part) in plan.split("||").enumerate() {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (role, task) = match part.split_once(':') {
            Some((r, t)) if !r.trim().is_empty() && !t.trim().is_empty() => {
                (r.trim().to_string(), t.trim().to_string())
            }
            _ => (format!("teammate-{}", i + 1), part.to_string()),
        };
        teammates.push(Teammate { role, task });
    }
    finish_spec(teammates, String::new(), 1)
}

fn spec_from_json(obj: &Value) -> Result<TeamSpec, String> {
    let team = obj
        .get("team")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "`team` must be an array of {role, task} objects".to_string())?;
    let mut teammates = Vec::new();
    for (i, entry) in team.iter().enumerate() {
        let task = entry
            .get("task")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("team[{i}] is missing a non-empty `task`"))?;
        let role = entry
            .get("role")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| format!("teammate-{}", i + 1));
        teammates.push(Teammate { role, task: task.to_string() });
    }
    let shared = obj
        .get("shared")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let rounds = obj.get("rounds").and_then(|v| v.as_u64()).unwrap_or(1);
    finish_spec(teammates, shared, rounds.min(u64::from(u8::MAX)) as u8)
}

fn finish_spec(mut teammates: Vec<Teammate>, shared: String, rounds: u8) -> Result<TeamSpec, String> {
    if teammates.is_empty() {
        return Err("the team plan names no teammates".into());
    }
    teammates.truncate(MAX_TEAMMATES);
    Ok(TeamSpec {
        teammates,
        shared,
        rounds: rounds.clamp(1, MAX_ROUNDS),
    })
}

/// Char-boundary-safe truncation with an ellipsis marker.
pub(crate) fn truncate_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let mut out: String = s.chars().take(cap).collect();
    out.push('…');
    out
}

fn fail(msg: String) -> SkillOutcome {
    SkillOutcome { ok: false, summary: msg }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A round-2 row must not be able to close a round-1 member: the ids
    /// are what an end matches its start by, and a team runs the same
    /// roles again every round (§6.11).
    #[test]
    fn member_ids_are_unique_across_rounds() {
        let mut seen = std::collections::HashSet::new();
        for round in 1..=4u8 {
            for index in 0..8usize {
                assert!(seen.insert(member_id(round, index)), "collision at {round}/{index}");
            }
        }
        // Same teammate, different round: different member.
        assert_ne!(member_id(1, 0), member_id(2, 0));
        // Same round, different teammate: different member.
        assert_ne!(member_id(1, 0), member_id(1, 1));
    }

    fn ctx() -> SkillContext {
        struct NullSink;
        impl crate::agent::EventSink for NullSink {
            fn emit(&self, _ev: Event) {}
        }
        let sink: Arc<dyn crate::agent::EventSink> = Arc::new(NullSink);
        SkillContext { sub: ToolSubAgent::root(sink) }
    }

    #[test]
    fn parses_compact_text_plan() {
        let spec = parse_spec(&json!({
            "plan": "researcher: list the crates || critic: review README"
        }))
        .unwrap();
        assert_eq!(spec.teammates.len(), 2);
        assert_eq!(spec.teammates[0].role, "researcher");
        assert_eq!(spec.teammates[0].task, "list the crates");
        assert_eq!(spec.teammates[1].role, "critic");
        assert_eq!(spec.rounds, 1);
    }

    #[test]
    fn text_plan_without_role_gets_default_name() {
        let spec = parse_spec(&json!({ "plan": "just do the thing" })).unwrap();
        assert_eq!(spec.teammates.len(), 1);
        assert_eq!(spec.teammates[0].role, "teammate-1");
        assert_eq!(spec.teammates[0].task, "just do the thing");
    }

    #[test]
    fn parses_json_object_plan_with_shared_and_rounds() {
        let plan = r#"{"team":[{"role":"a","task":"t1"},{"role":"b","task":"t2"}],"shared":"ctx","rounds":2}"#;
        let spec = parse_spec(&json!({ "plan": plan })).unwrap();
        assert_eq!(spec.teammates.len(), 2);
        assert_eq!(spec.shared, "ctx");
        assert_eq!(spec.rounds, 2);
    }

    #[test]
    fn parses_json_array_plan() {
        let plan = r#"[{"role":"a","task":"t1"},{"task":"t2"}]"#;
        let spec = parse_spec(&json!({ "plan": plan })).unwrap();
        assert_eq!(spec.teammates.len(), 2);
        assert_eq!(spec.teammates[1].role, "teammate-2");
    }

    #[test]
    fn accepts_team_key_directly_from_json_fence_args() {
        let spec = parse_spec(&json!({
            "team": [{"role": "x", "task": "y"}],
            "rounds": 9
        }))
        .unwrap();
        assert_eq!(spec.teammates.len(), 1);
        assert_eq!(spec.rounds, MAX_ROUNDS, "rounds must clamp to the cap");
    }

    #[test]
    fn rounds_zero_clamps_to_one() {
        let spec = parse_spec(&json!({
            "team": [{"role": "x", "task": "y"}],
            "rounds": 0
        }))
        .unwrap();
        assert_eq!(spec.rounds, 1);
    }

    #[test]
    fn team_size_is_capped() {
        let team: Vec<Value> = (0..10)
            .map(|i| json!({"role": format!("r{i}"), "task": "t"}))
            .collect();
        let spec = parse_spec(&json!({ "team": team })).unwrap();
        assert_eq!(spec.teammates.len(), MAX_TEAMMATES);
    }

    #[test]
    fn missing_plan_is_an_error() {
        assert!(parse_spec(&json!({})).is_err());
        assert!(parse_spec(&json!({ "plan": "  " })).is_err());
    }

    #[test]
    fn empty_task_in_json_is_an_error() {
        assert!(parse_spec(&json!({ "team": [{"role": "a", "task": ""}] })).is_err());
    }

    #[test]
    fn malformed_json_plan_is_an_error() {
        assert!(parse_spec(&json!({ "plan": "{not json" })).is_err());
    }

    #[test]
    fn charter_names_roster_and_tools() {
        let spec = TeamSpec {
            teammates: vec![
                Teammate { role: "researcher".into(), task: "find crates".into() },
                Teammate { role: "critic".into(), task: "review docs".into() },
            ],
            shared: "workspace is sica-rust".into(),
            rounds: 1,
        };
        let sys = teammate_system(&spec.teammates[0], &spec, Some("- **run-cli**"));
        assert!(sys.contains("`researcher`"));
        assert!(sys.contains("(you)"));
        assert!(sys.contains("critic"));
        assert!(sys.contains("workspace is sica-rust"));
        assert!(sys.contains("run-cli"));
        // Without a catalogue the skills section must be absent — the
        // reporting contract stays, since `structured-output` is scoped to
        // the run rather than drawn from the registry.
        let sys = teammate_system(&spec.teammates[1], &spec, None);
        assert!(!sys.contains("Available skills"));
        assert!(sys.contains(runner::STRUCTURED_OUTPUT_NAME));
    }

    #[test]
    fn charter_states_the_grounding_rule() {
        let spec = TeamSpec {
            teammates: vec![Teammate { role: "r".into(), task: "t".into() }],
            shared:    String::new(),
            rounds:    1,
        };
        // Both with and without tools: a teammate with no tools at all is the
        // one most likely to invent output.
        for cat in [Some("- **read-file**"), None] {
            let sys = teammate_system(&spec.teammates[0], &spec, cat);
            assert!(sys.contains("unverified"), "grounding rule missing");
            assert!(
                sys.contains("inventing an id is worse than admitting"),
                "grounding rule missing"
            );
        }
    }

    /// A structured teammate outcome with `cited` of `claims` claims backed.
    fn outcome(report: &str, cited: usize, claims: usize) -> TeammateOutcome {
        TeammateOutcome {
            report: report.into(),
            tool_ok: cited as u32,
            tool_err: 0,
            claims,
            cited,
            structured: true,
        }
    }

    #[test]
    fn board_includes_every_role_and_placeholder() {
        let mates = vec![
            Teammate { role: "a".into(), task: "t".into() },
            Teammate { role: "b".into(), task: "t".into() },
        ];
        let reports = vec![Some(outcome("report A", 1, 1)), None];
        let board = render_board(1, &mates, &reports);
        assert!(board.contains("### a"));
        assert!(board.contains("report A"));
        assert!(board.contains("### b"));
        assert!(board.contains("(no report yet)"));
    }

    #[test]
    fn board_marks_reports_with_no_cited_claim() {
        let mates = vec![
            Teammate { role: "grounded".into(), task: "t".into() },
            Teammate { role: "guessing".into(), task: "t".into() },
        ];
        let reports = vec![
            Some(outcome("read it", 1, 1)),
            Some(outcome("README.md lists the crates", 0, 2)),
        ];
        let board = render_board(1, &mates, &reports);
        assert!(board.contains("### grounded\n"), "fully cited role must not be marked");
        assert!(
            board.contains("### guessing — UNVERIFIED (no claim cites a successful tool call)"),
            "board: {board}"
        );
    }

    #[test]
    fn a_partly_cited_report_says_so_rather_than_passing_as_clean() {
        let partly = outcome("mixed", 1, 3);
        assert!(partly.verified(), "one cited claim is still grounded");
        assert!(
            partly.provenance().contains("PARTLY VERIFIED (1/3"),
            "{}",
            partly.provenance()
        );
    }

    #[test]
    fn failed_tool_calls_do_not_count_as_verification() {
        // `read-file` on a missing path returns ok=false. A teammate that only
        // ever got errors has verified nothing — this is exactly the run that
        // used to come back claiming the file was there.
        let only_errors = TeammateOutcome {
            report:   "the file is present".into(),
            tool_ok:  0,
            tool_err: 2,
            claims:   0,
            cited:    0,
            structured: false,
        };
        assert!(!only_errors.verified());
        assert!(only_errors.provenance().contains("UNVERIFIED"));
        assert_eq!(outcome("x", 1, 1).provenance(), "");
    }

    #[test]
    fn a_prose_fallback_is_never_reported_as_cited() {
        // The runner already nudged once; prose that still arrives is the
        // weakest outcome and must not look like a clean report.
        let prose = TeammateOutcome {
            report:     "I read the file and it defines Registry".into(),
            tool_ok:    2,
            tool_err:   0,
            claims:     0,
            cited:      0,
            structured: false,
        };
        assert!(prose.verified(), "it did call tools successfully");
        assert!(prose.provenance().contains("UNSTRUCTURED"), "{}", prose.provenance());
    }

    #[test]
    fn claims_render_with_their_citation_and_count_as_cited() {
        let ok_ids: HashSet<&str> = ["call-1", "call-2"].into_iter().collect();
        let report = json!({
            "claims": [
                {"text": "the workspace has 7 crates", "evidence": ["call-1"]},
                {"text": "README documents them", "evidence": ["`call-2` (read-file)"]}
            ],
            "open_questions": ["is agent-team enabled?"]
        });
        let (md, claims, cited) = render_claims(&report, &ok_ids);
        assert_eq!((claims, cited), (2, 2));
        assert!(md.contains("- the workspace has 7 crates  [call-1]"), "{md}");
        assert!(md.contains("[`call-2` (read-file)]"), "decoration is kept in the citation: {md}");
        assert!(md.contains("Open questions:"));
        assert!(md.contains("- is agent-team enabled?"));
    }

    #[test]
    fn a_claim_citing_nothing_is_marked_unverified() {
        let ok_ids: HashSet<&str> = ["call-1"].into_iter().collect();
        let report = json!({"claims": [{"text": "src/lib.rs is empty", "evidence": []}]});
        let (md, claims, cited) = render_claims(&report, &ok_ids);
        assert_eq!((claims, cited), (1, 0));
        assert!(md.contains("- unverified: src/lib.rs is empty  (no evidence cited)"), "{md}");
    }

    #[test]
    fn a_claim_citing_an_id_that_never_succeeded_is_marked_too() {
        // The whole point of checkable citations: a fabricated `call-9`, or
        // one naming a call that errored, must not read as evidence.
        let ok_ids: HashSet<&str> = ["call-1"].into_iter().collect();
        let report = json!({
            "claims": [{"text": "the build passes", "evidence": ["call-9"]}]
        });
        let (md, _, cited) = render_claims(&report, &ok_ids);
        assert_eq!(cited, 0);
        assert!(md.contains("cites call-9, which named no successful tool call"), "{md}");
    }

    #[test]
    fn citation_ids_survive_the_decoration_models_add() {
        for raw in ["call-2", "`call-2`", "call-2 (read-file)", " \"call-2\", "] {
            assert_eq!(citation_id(raw), "call-2", "raw: {raw}");
        }
    }

    #[test]
    fn the_teammate_schema_accepts_a_good_report_and_names_a_bad_one() {
        let schema = teammate_schema();
        let good = json!({"claims": [{"text": "t", "evidence": ["call-1"]}]});
        assert!(runner::validate(&good, &schema, "$").is_empty());
        let bad = json!({"claims": [{"text": "t"}]});
        let problems = runner::validate(&bad, &schema, "$");
        assert!(
            problems.iter().any(|p| p.contains("missing required field `evidence`")),
            "{problems:?}"
        );
        assert!(!runner::validate(&json!({"claims": []}), &schema, "$").is_empty());
    }

    #[test]
    fn charter_states_the_typed_reporting_contract() {
        let spec = TeamSpec {
            teammates: vec![Teammate { role: "r".into(), task: "t".into() }],
            shared:    String::new(),
            rounds:    1,
        };
        let sys = teammate_system(&spec.teammates[0], &spec, Some("- **read-file**"));
        assert!(sys.contains(runner::STRUCTURED_OUTPUT_NAME), "{sys}");
        assert!(sys.contains("open_questions"));
        assert!(sys.contains("[id: call-2]"), "the id convention must be explained");
    }

    #[tokio::test]
    async fn run_without_llm_fails_cleanly() {
        let team = AgentTeam::new();
        let out = team
            .run(json!({ "plan": "a: do x" }), ctx())
            .await;
        assert!(!out.ok);
        assert!(out.summary.contains("LLM"));
    }

    #[tokio::test]
    async fn run_with_bad_plan_fails_before_needing_llm() {
        let team = AgentTeam::new();
        let out = team.run(json!({}), ctx()).await;
        assert!(!out.ok);
        assert!(out.summary.contains("plan"));
    }
}
