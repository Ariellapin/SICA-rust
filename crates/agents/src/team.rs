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

use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use futures::future::join_all;
use protocol::Event;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use llm::client::{ChatMessage, LlmClient};

use crate::parse_tool_call;
use crate::registry::SkillRegistry;
use crate::skill::{Skill, SkillContext, SkillOutcome};
use crate::subagent::{ToolInvocation, ToolSubAgent};

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
- `rounds` (1–3, default 1): after each round every teammate sees the shared
  *team board* (everyone's report) and coordinates/refines in the next round.
- `shared` (optional): briefing text prepended to every teammate's charter.
- A team-lead pass merges all reports into one deliverable; the individual
  reports are appended after it.
- Interrupting the turn stops the whole team immediately.

Grounding: a teammate that made no successful tool call is reported as
**UNVERIFIED** — its prose is model reasoning, not something checked against
the machine, and the lead is told not to restate it as fact. If no teammate
verified anything the whole result carries a warning banner.

**This file is the on/off switch.** The backend registers `agent-team` only
when `skills/agent-team.md` exists; rename it to `agent-team.md.off` (only
`*.md` is scanned) or delete it, restart the backend, and the skill vanishes
from the catalogue. It is not seeded automatically.
"#;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Teammate {
    pub role: String,
    pub task: String,
}

/// What one teammate produced in a round, plus how much of it is grounded.
///
/// The counts exist because a teammate's prose is indistinguishable from
/// fact once it reaches the lead: a model that never called `read-file` will
/// still happily report what a file "contains". `tool_ok == 0` means nothing
/// in `report` was checked against the machine, and every consumer — the
/// board, the lead, the final summary — says so out loud.
#[derive(Debug, Clone, Default)]
pub(crate) struct TeammateOutcome {
    pub report:  String,
    pub tool_ok: u32,
    pub tool_err: u32,
}

impl TeammateOutcome {
    fn verified(&self) -> bool {
        self.tool_ok > 0
    }

    /// Suffix appended to this teammate's heading wherever the report is
    /// shown. Empty when at least one tool call succeeded; otherwise it says
    /// *why* nothing is grounded, since "never tried" and "tried and every
    /// call errored" call for different scepticism from the reader.
    fn provenance(&self) -> String {
        if self.verified() {
            String::new()
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
            registry.map(|r| Arc::new(r.excluding(crate::control::TEAMMATE_EXCLUDED)));
        let catalogue = registry
            .as_ref()
            .map(|r| r.catalogue_markdown_excluding(crate::control::TEAMMATE_EXCLUDED))
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

        for round in 1..=spec.rounds {
            if is_cancelled(&cancel) {
                break;
            }
            if round > 1 {
                let board = render_board(round - 1, &spec.teammates, &reports);
                for t in transcripts.iter_mut() {
                    t.push(ChatMessage::text(
                        "user",
                        format!(
                            "{board}\n\nAbove is what every teammate produced \
                             last round. Coordinate: avoid duplicating their \
                             work, resolve any conflicts with your own \
                             findings, and reply with your improved report. \
                             You may make tool calls first. A heading marked \
                             UNVERIFIED means that teammate ran no successful \
                             tool call — treat its claims as guesses and check \
                             anything you intend to build on."
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
                .map(|(transcript, mate)| {
                    run_teammate(&client, registry.as_ref(), &ctx.sub, mate, transcript, &cancel)
                })
                .collect();
            let results = join_all(round_futs).await;
            for (slot, res) in reports.iter_mut().zip(results) {
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

        if is_cancelled(&cancel) {
            return fail("agent-team interrupted".into());
        }
        if reports.iter().all(Option::is_none) {
            return fail(
                "agent-team: no teammate produced a report (every LLM call \
                 failed or returned nothing)"
                    .into(),
            );
        }

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
                "**Warning: no teammate produced a successful tool call — nothing \
                 below was checked against the machine. Treat every claim about \
                 files, commands or output as unverified.**\n\n",
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

/// One teammate's turn within a round: chat, dispatch at most
/// `MAX_TEAMMATE_HOPS` tool calls, and return the final report together with
/// its tool-call tally. `None` means every LLM call failed (or the turn was
/// interrupted) — the caller keeps whatever report the previous round
/// produced.
///
/// A reply with no parsable tool call used to be accepted as the final
/// report unconditionally. That silently promoted *botched* calls —
/// `read-file 'README.md'` with no ` > ` clause, an unreadable
/// ```tool_call fence — into confident prose about a file the teammate
/// never opened. Such a reply now costs the teammate one corrective nudge
/// before it is accepted, and the acceptance is recorded as unverified.
async fn run_teammate(
    client:     &LlmClient,
    registry:   Option<&Arc<SkillRegistry>>,
    sub:        &ToolSubAgent,
    mate:       &Teammate,
    transcript: &mut Vec<ChatMessage>,
    cancel:     &Option<CancellationToken>,
) -> Option<TeammateOutcome> {
    let mut hops: u8 = 0;
    let mut tool_ok:  u32 = 0;
    let mut tool_err: u32 = 0;
    let mut nudged = false;
    loop {
        let reply = llm_call(client, cancel, transcript.clone()).await?;
        transcript.push(ChatMessage::text("assistant", reply.clone()));

        let call = registry.and_then(|reg| {
            parse_tool_call::extract_known(&reply, |n| reg.by_name.contains_key(n))
        });
        let Some(call) = call else {
            let rejected = registry.and_then(|reg| {
                parse_tool_call::rejected_attempt(&reply, |n| reg.by_name.contains_key(n))
            });
            if let Some(reason) = rejected {
                warn!(
                    role = %mate.role,
                    reason = %reason,
                    nudged,
                    "agent-team: teammate emitted an unparsable tool call"
                );
                sub.events.emit(Event::LogLine {
                    level:   "WARN".into(),
                    message: format!(
                        "agent-team: `{}` emitted {reason} — {}",
                        mate.role,
                        if nudged || hops >= MAX_TEAMMATE_HOPS {
                            "accepting its reply as an UNVERIFIED report"
                        } else {
                            "asking it to retry with the correct syntax"
                        }
                    ),
                });
                if !nudged && hops < MAX_TEAMMATE_HOPS {
                    nudged = true;
                    transcript.push(ChatMessage::text("user", SYNTAX_CORRECTION.to_string()));
                    continue;
                }
            }
            return Some(TeammateOutcome { report: reply, tool_ok, tool_err });
        };

        if hops >= MAX_TEAMMATE_HOPS {
            transcript.push(ChatMessage::text(
                "user",
                format!(
                    "Tool budget ({MAX_TEAMMATE_HOPS}) exhausted for this round \
                     — reply with your final report now, using what you \
                     already have."
                ),
            ));
            let final_reply = llm_call(client, cancel, transcript.clone()).await?;
            transcript.push(ChatMessage::text("assistant", final_reply.clone()));
            return Some(TeammateOutcome { report: final_reply, tool_ok, tool_err });
        }
        hops += 1;

        // `registry` is Some here — `call` only exists when it was.
        let reg = registry.expect("tool call parsed without a registry");
        let outcome = match reg.resolve(&call) {
            Some((skill, args)) => {
                sub.run(ToolInvocation {
                    skill:       &*skill,
                    args,
                    raw_args:    call.raw_args.clone(),
                    expectation: call.expectation.clone(),
                })
                .await
            }
            None => SkillOutcome {
                ok:      false,
                summary: format!("unknown skill `{}`", call.skill),
            },
        };
        if outcome.ok { tool_ok += 1 } else { tool_err += 1 }
        transcript.push(ChatMessage::text(
            "user",
            format!(
                "Tool result for `{}` ({}):\n{}",
                call.skill,
                if outcome.ok { "ok" } else { "error" },
                outcome.summary
            ),
        ));
        info!(
            role = %mate.role,
            skill = %call.skill,
            ok = outcome.ok,
            hop = hops,
            "agent-team: teammate tool call"
        );
    }
}

/// Sent to a teammate that emitted something tool-call-shaped the parser
/// could not read. Restates the contract and — the part that matters —
/// forbids the fallback the model would otherwise take: writing up the
/// output it *expected* the tool to produce.
const SYNTAX_CORRECTION: &str = "\
That was not a valid tool call, so NOTHING ran and you received no output. \
To call a tool, reply with exactly one line and nothing else:\n\n\
    <skill-name> '<arg1>' '<arg2>' > <what you want to learn>\n\n\
Every argument must be quoted and the ` > <expectation>` part is required. \
Retry the call now if you still need it. If you do not, reply with your \
report — but do NOT describe file contents, command output, or whether a \
path exists unless a tool result above actually shows it; say plainly that \
you could not verify it instead.";

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
        body.push_str(&format!("Team briefing: {}\n\n", spec.shared.trim()));
    }
    for (mate, report) in spec.teammates.iter().zip(reports) {
        body.push_str(&format!(
            "## Report from `{}`{} (task: {})\n{}\n\n",
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
                  teammates' reports into ONE coherent deliverable: keep every \
                  concrete fact (numbers, paths, versions, errors) verbatim, \
                  drop duplication, and flag any point where two reports \
                  contradict each other instead of silently picking one. \
                  A report whose heading says UNVERIFIED is not backed by any \
                  tool output — it is that teammate's guess. Never restate its \
                  claims as established fact: either attribute them (\"`role` \
                  believes …, unverified\") or leave them out. \
                  Output only the merged result — no preamble.";
    let messages = vec![
        ChatMessage::text("system", system),
        ChatMessage::text("user", body),
    ];
    llm_call(client, cancel, messages).await
}

/// One non-streaming chat round-trip raced against the turn's cancellation
/// token. `None` on cancel, transport error, or an empty reply.
async fn llm_call(
    client:   &LlmClient,
    cancel:   &Option<CancellationToken>,
    messages: Vec<ChatMessage>,
) -> Option<String> {
    let res = match cancel {
        Some(token) => tokio::select! {
            biased;
            _ = token.cancelled() => return None,
            r = client.chat_once(messages) => r,
        },
        None => client.chat_once(messages).await,
    };
    match res {
        Ok(s) if !s.trim().is_empty() => Some(s),
        Ok(_) => None,
        Err(e) => {
            warn!(error = %e, "agent-team: LLM call failed");
            None
        }
    }
}

fn is_cancelled(cancel: &Option<CancellationToken>) -> bool {
    cancel.as_ref().is_some_and(|t| t.is_cancelled())
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
             When you have what you need, reply with your final report as \
             plain text containing no tool-call line.\n\n\
             Available skills:\n{cat}"
        ));
    }
    charter.push_str(
        "\nKeep your final report concise and factual. Quote exact values \
         (numbers, paths, errors) verbatim from tool output. Start directly \
         with content — no preamble.\n\n\
         Grounding rule, and it is absolute: state a file's contents, a \
         command's output, or whether a path exists ONLY if a tool result in \
         this conversation shows it. You cannot see the disk otherwise. If \
         you have not run the tool, write `unverified:` in front of the claim \
         and name the call you would need — never invent output, and never \
         report a file as existing because the name sounds plausible.",
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
fn truncate_chars(s: &str, cap: usize) -> String {
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
        // Without a catalogue the tool section must be absent.
        let sys = teammate_system(&spec.teammates[1], &spec, None);
        assert!(!sys.contains("tool call"));
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
            assert!(sys.contains("unverified:"), "grounding rule missing");
            assert!(sys.contains("never invent output"), "grounding rule missing");
        }
    }

    fn outcome(report: &str, tool_ok: u32) -> TeammateOutcome {
        TeammateOutcome { report: report.into(), tool_ok, tool_err: 0 }
    }

    #[test]
    fn board_includes_every_role_and_placeholder() {
        let mates = vec![
            Teammate { role: "a".into(), task: "t".into() },
            Teammate { role: "b".into(), task: "t".into() },
        ];
        let reports = vec![Some(outcome("report A", 1)), None];
        let board = render_board(1, &mates, &reports);
        assert!(board.contains("### a"));
        assert!(board.contains("report A"));
        assert!(board.contains("### b"));
        assert!(board.contains("(no report yet)"));
    }

    #[test]
    fn board_marks_reports_with_no_successful_tool_call() {
        let mates = vec![
            Teammate { role: "grounded".into(), task: "t".into() },
            Teammate { role: "guessing".into(), task: "t".into() },
        ];
        let reports = vec![
            Some(outcome("read it", 1)),
            Some(outcome("README.md exists and lists the crates", 0)),
        ];
        let board = render_board(1, &mates, &reports);
        assert!(board.contains("### grounded\n"), "verified role must not be marked");
        assert!(
            board.contains("### guessing — UNVERIFIED (no tool call made)"),
            "board: {board}"
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
        };
        assert!(!only_errors.verified());
        assert!(only_errors.provenance().contains("UNVERIFIED"));
        assert_eq!(outcome("x", 1).provenance(), "");
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
