//! `ToolSubAgent`: wraps one tool invocation and emits start/finish events.
//! Each sub-agent carries its own depth + parent id so nested calls produce a
//! traceable chain — fixing the original Python limitation where sub-agents
//! could not invoke further sub-agents.
//!
//! When an `LlmClient` summarizer is attached, the sub-agent post-processes
//! the raw skill output through the LLM, focused on the main agent's
//! `expectation` string (the text after `>` in the natural-language tool
//! call). This keeps the main agent's context window tight: instead of
//! re-ingesting the full file or the full shell output, it only sees a
//! focused answer.
//!
//! Failures from a tool call (CLI exit non-zero, missing file, denied write,
//! etc.) are also broadcast to an optional `ToolFailureSink`. The backend wires
//! that sink to the idealist `TriggerBus` so each exception becomes an
//! improvement ticket, and — when relevant — gets routed toward an
//! environment-appropriate skill (e.g. `run-pwsh` instead of `run-cli` on
//! Windows when `cmd /C` cannot resolve a command).

use std::sync::atomic::{AtomicU64, Ordering};
use std::path::PathBuf;
use std::sync::Arc;

use once_cell::sync::Lazy;
use protocol::Event;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use llm::client::{ChatMessage, LlmClient};

use crate::agent::EventSink;
use crate::broker::BrokerSet;
use crate::parse_tool_call;
use crate::pipeline::{CallView, PostDecision, PreDecision};
use crate::skill::{Skill, SkillContext, SkillOutcome};

static TOOL_ID: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(1));

/// Mint a tool-call id from the shared atomic. `ChatHub` uses the same
/// space for harness-control calls it dispatches without a sub-agent, so
/// chips never collide.
pub fn next_tool_id() -> u64 {
    TOOL_ID.fetch_add(1, Ordering::Relaxed)
}

/// Describes one failed sub-agent tool invocation. Includes the host OS so
/// downstream classifiers can detect environment mismatches (e.g. a command
/// that fails under `cmd.exe` but would work under PowerShell).
#[derive(Debug, Clone)]
pub struct ToolFailureReport {
    pub skill:        String,
    pub args_preview: String,
    pub summary:      String,
    pub depth:        u8,
    pub host_os:      &'static str,
    pub host_family:  &'static str,
}

/// Receiver for sub-agent tool failures. The backend forwards into the
/// idealist `TriggerBus`.
pub trait ToolFailureSink: Send + Sync {
    fn report(&self, report: ToolFailureReport);
}

/// One invocation request: the resolved skill + args + raw positional values
/// (for the UI's `args_preview`) + the main agent's expectation text.
pub struct ToolInvocation<'a> {
    pub skill:       &'a dyn Skill,
    pub args:        Value,
    pub raw_args:    Vec<String>,
    pub expectation: String,
}

/// Audit record for one approval round-trip. `ChatHub` persists it as an
/// `Approval` event; the model saw only the tool outcome.
pub struct ApprovalRecord {
    pub skill:        String,
    pub args_preview: String,
    /// `allowed-once` or `denied`.
    pub decision:     &'static str,
}

/// Everything one pipeline run produces: the model-facing outcome, advisory
/// context to inject after the result, and the approval audit (if asked).
pub struct RunReport {
    pub outcome:  SkillOutcome,
    pub notices:  Vec<String>,
    pub approval: Option<ApprovalRecord>,
}

/// One edge of an orchestrated run: the run starting, a member starting, a
/// member ending, or the run ending (UI guide §6.11).
#[derive(Debug, Clone)]
pub struct RunEdge {
    pub run_id:    u64,
    /// Seq of the `ToolCall` that started the run.
    pub call_seq:  u64,
    pub phase:     Option<String>,
    pub member:    Option<String>,
    pub member_id: Option<u64>,
    pub state:     sica_core::event::RunState,
}

/// Who turns a [`RunEdge`] into a durable row. Implemented by the backend,
/// which owns the session log — `agents` knows how to run a workflow and
/// nothing about where its history lives, the same split `JobNotifier`
/// makes for background jobs.
pub trait RunNotifier: Send + Sync {
    fn edge(&self, session_id: u64, edge: RunEdge);
}

/// Run ids are per process and monotonic, and **shared by every kind of
/// orchestrator**: a `workflow` and an `agent-team` in one session must not
/// be able to mint the same id, or their rows would fold into one tree.
static NEXT_RUN_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn next_run_id() -> u64 {
    NEXT_RUN_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[derive(Clone)]
pub struct ToolSubAgent {
    pub depth:         u8,
    pub parent_id:     Option<u64>,
    pub max_depth:     u8,
    pub events:        Arc<dyn EventSink>,
    pub failure_sink:  Option<Arc<dyn ToolFailureSink>>,
    pub summarizer:    Option<LlmClient>,
    /// Fired by `InterruptTurn`. A tool call is the long pole of a turn — the
    /// skill itself, plus a summarizer round-trip that is a whole second LLM
    /// request — so without this the model keeps working long after the user
    /// pressed Stop. Both stages race the token and abandon their work.
    pub cancel:        Option<CancellationToken>,
    /// Sub-directory under `spill_dir` for this call chain's oversized
    /// outputs — the session id in production. `None` disables spilling
    /// (raw output passes through), which is what test doubles want.
    pub spill_label:   Option<String>,
    /// Guarded-execution policies (Wave 3, `agents::pipeline`). Evaluated
    /// pre → guards → post around the skill body. Empty in tests and in
    /// standalone uses (team transcripts, evals) that predate the pipeline.
    pub policies:      Arc<[Arc<dyn crate::pipeline::ToolPolicy>]>,
    /// Human-in-the-loop rendezvous for `Ask` decisions and the `ask-user`
    /// skill. `None` outside a live session (tests, teammates, evals) — an
    /// `Ask` then degrades to a denial.
    pub brokers:       Option<Arc<BrokerSet>>,
    /// Session this call belongs to. Carried into broker events so the FE
    /// can route the prompt; `None` means no human is reachable.
    pub session_id:    Option<u64>,
    /// Whether the owning session is in plan mode. Read by control skills;
    /// inherited by children.
    pub plan_active:   bool,
    /// The parent session's *completed* turns, as wire messages — what
    /// `subagent-fork` seeds its child with (Wave 4, §12.1). Built once per
    /// dispatch by `ChatHub`; `None` outside a live session. The in-flight
    /// turn is excluded by construction, so a fork never inherits a
    /// half-written exchange.
    pub fork_seed:     Option<Arc<Vec<ChatMessage>>>,
    /// Where the durable rows of an orchestrated run go (UI guide §6.11).
    /// `None` outside a live session, which is also where there is no log to
    /// write them to. Inherited by `child()` so a nested orchestrator would
    /// report through the same sink.
    pub runs:          Option<Arc<dyn RunNotifier>>,
    /// Directory the owning session works in (guide §3.9). `None` means
    /// "whatever the process defaults to" — a session created before
    /// sessions had their own directory, or a sub-agent running outside one
    /// (tests, evals, teammates). Inherited by `child()`, so a nested call
    /// can never drift into another project's folder.
    pub cwd:           Option<PathBuf>,
    /// Seq of the durable `ToolCall` the caller logged for *this* dispatch,
    /// carried onto `Event::ToolCallStarted` so a live tool row and the
    /// ledger row for the same call share one identity. `None` for a nested
    /// call and for any sub-agent running outside a session — neither is
    /// written to a session log, so neither has a seq to give.
    pub log_seq:       Option<u64>,
}

impl ToolSubAgent {
    pub fn root(events: Arc<dyn EventSink>) -> Self {
        Self {
            depth:        0,
            parent_id:    None,
            max_depth:    4,
            events,
            failure_sink: None,
            summarizer:   None,
            cancel:       None,
            spill_label:  None,
            policies:     Arc::new([]),
            brokers:      None,
            session_id:   None,
            plan_active:  false,
            fork_seed:    None,
            runs:         None,
            cwd:          None,
            log_seq:      None,
        }
    }

    /// Report one edge of an orchestrated run (§6.11): the run starting, a
    /// member starting, a member ending, or the run ending.
    ///
    /// Silently does nothing without a notifier or a session — a run
    /// outside a session (a test, an eval) has no log to be durable in, and
    /// that is not a failure. `member` is `(id, label)`; the id is what lets
    /// an end find its own start when two members share a label.
    pub fn run_edge(
        &self,
        run_id: u64,
        phase: Option<&str>,
        member: Option<(u64, &str)>,
        state: sica_core::event::RunState,
    ) {
        let (Some(runs), Some(session_id)) = (self.runs.as_ref(), self.session_id) else {
            return;
        };
        runs.edge(session_id, RunEdge {
            run_id,
            call_seq: self.log_seq.unwrap_or(0),
            phase: phase.filter(|p| !p.is_empty()).map(str::to_string),
            member: member.map(|(_, l)| l.to_string()),
            member_id: member.map(|(id, _)| id),
            state,
        });
    }

    /// Attach the sink that makes an orchestrated run durable (§6.11).
    pub fn with_runs(mut self, runs: Arc<dyn RunNotifier>) -> Self {
        self.runs = Some(runs);
        self
    }

    /// Bind this call (and its children) to the session's working
    /// directory. Without it every file skill falls back to the process
    /// default, which is the pre-§3.9 behaviour.
    pub fn with_cwd(mut self, cwd: Option<PathBuf>) -> Self {
        self.cwd = cwd;
        self
    }

    /// Name the durable `ToolCall` seq this dispatch was logged under.
    pub fn with_log_seq(mut self, seq: u64) -> Self {
        self.log_seq = (seq > 0).then_some(seq);
        self
    }

    /// Enable spill-to-file for outputs over `spill::SPILL_THRESHOLD`,
    /// filed under `spill/<label>/`.
    pub fn with_spill_label(mut self, label: impl Into<String>) -> Self {
        self.spill_label = Some(label.into());
        self
    }

    /// Attach a failure sink so any failed tool invocation (or one of its
    /// descendants) gets reported. Returns `self` for chaining.
    pub fn with_failure_sink(mut self, sink: Arc<dyn ToolFailureSink>) -> Self {
        self.failure_sink = Some(sink);
        self
    }

    /// Attach an LLM client used to post-summarize successful skill outcomes
    /// against the main agent's expectation. Without one, the raw skill
    /// `summary` is returned unchanged.
    pub fn with_summarizer(mut self, client: LlmClient) -> Self {
        self.summarizer = Some(client);
        self
    }

    /// Attach the turn's cancellation token so an interrupt unwinds this call
    /// (and everything it spawns) instead of running to completion.
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Attach pipeline policies. Inherited by `child()`.
    pub fn with_policies(mut self, policies: Vec<Arc<dyn crate::pipeline::ToolPolicy>>) -> Self {
        self.policies = policies.into();
        self
    }

    /// Attach the human-in-the-loop brokers. Inherited by `child()`.
    pub fn with_brokers(mut self, brokers: Arc<BrokerSet>) -> Self {
        self.brokers = Some(brokers);
        self
    }

    /// Bind this call (and its children) to a session for broker events.
    pub fn with_session(mut self, session_id: u64) -> Self {
        self.session_id = Some(session_id);
        self
    }

    /// Mark calls from a plan-mode session. Read by control skills.
    pub fn with_plan_active(mut self, active: bool) -> Self {
        self.plan_active = active;
        self
    }

    /// Attach the parent session's completed turns for `subagent-fork`.
    /// Inherited by `child()`.
    pub fn with_fork_seed(mut self, seed: Arc<Vec<ChatMessage>>) -> Self {
        self.fork_seed = Some(seed);
        self
    }

    /// Build a child sub-agent rooted at the call id `parent_id`. Used by
    /// `SkillContext` so a skill can spawn further sub-agents. Inherits the
    /// failure sink, summarizer and cancellation token so nested calls share
    /// configuration and stop together.
    pub fn child(&self, parent_id: u64) -> Self {
        Self {
            depth:        self.depth.saturating_add(1),
            parent_id:    Some(parent_id),
            max_depth:    self.max_depth,
            events:       self.events.clone(),
            failure_sink: self.failure_sink.clone(),
            summarizer:   self.summarizer.clone(),
            cancel:       self.cancel.clone(),
            spill_label:  self.spill_label.clone(),
            policies:     self.policies.clone(),
            brokers:      self.brokers.clone(),
            session_id:   self.session_id,
            plan_active:  self.plan_active,
            fork_seed:    self.fork_seed.clone(),
            runs:         self.runs.clone(),
            cwd:          self.cwd.clone(),
            // A nested call is a live event only — it never reaches the
            // session log, so it inherits no seq.
            log_seq:      None,
        }
    }

    /// `true` once the turn this call belongs to has been interrupted.
    fn cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(|t| t.is_cancelled())
    }

    /// Run one skill invocation. Emits the start/finish events with the
    /// natural-language args preview and the main agent's expectation. On
    /// success, if a summarizer is configured and the expectation is non-
    /// empty, the raw `outcome.summary` is replaced by the LLM's focused
    /// answer.
    ///
    /// Thin wrapper over [`run_report`](Self::run_report) for callers that
    /// only need the model-facing outcome (teammates, evals, tests).
    pub async fn run(&self, inv: ToolInvocation<'_>) -> SkillOutcome {
        self.run_report(inv).await.outcome
    }

    /// Full pipeline run: pre-execute → guards → body → post-execute (see
    /// `agents::pipeline`). A `Deny`/`Block` becomes a failed outcome the
    /// model reads — never a defect, so never sunk — while `post_execute`
    /// still runs for denied calls so the repeat reminder counts them.
    pub async fn run_report(&self, inv: ToolInvocation<'_>) -> RunReport {
        let ToolInvocation { skill, args, raw_args, expectation } = inv;

        if self.depth >= self.max_depth {
            warn!(
                depth = self.depth,
                max_depth = self.max_depth,
                skill = skill.name(),
                parent_id = self.parent_id,
                "sub-agent depth limit reached — aborting tool call"
            );
            self.events.emit(Event::LogLine {
                level: "WARN".into(),
                message: format!(
                    "sub-agent[depth={}] depth limit ({}) reached for skill `{}` — aborting",
                    self.depth,
                    self.max_depth,
                    skill.name()
                ),
            });
            return RunReport {
                outcome: SkillOutcome {
                    ok: false,
                    summary: format!("sub-agent depth limit ({}) reached", self.max_depth),
                },
                notices: Vec::new(),
                approval: None,
            };
        }

        // Interrupted before we even started: say nothing to the UI. Emitting
        // a start event here would leave a chip spinning for a call that never
        // ran.
        if self.cancelled() {
            return RunReport {
                outcome: SkillOutcome {
                    ok: false,
                    summary: "interrupted before the tool call started".into(),
                },
                notices: Vec::new(),
                approval: None,
            };
        }

        let id = next_tool_id();
        let started = std::time::Instant::now();
        // The UI payloads are bounded before they ever reach the pipe. A
        // `write-file` argument or a `read-file` result can be megabytes, and
        // neither the transcript row nor the frontend's chip needs more than
        // a screenful — but a truncated payload must announce itself rather
        // than look like the whole thing.
        let args_json = ui_args_json(&args);
        let args_preview = parse_tool_call::render(skill.name(), &raw_args);
        info!(
            tool_id = id,
            parent_id = self.parent_id,
            depth = self.depth,
            skill = skill.name(),
            args_preview = %args_preview,
            expectation = %expectation,
            "sub-agent: tool call started"
        );
        self.events.emit(Event::LogLine {
            level: "INFO".into(),
            message: format!(
                "sub-agent[depth={}, id={}{}] → {}{}",
                self.depth,
                id,
                self.parent_id
                    .map(|p| format!(", parent={p}"))
                    .unwrap_or_default(),
                args_preview,
                if expectation.trim().is_empty() {
                    String::new()
                } else {
                    format!(" > {expectation}")
                },
            ),
        });
        self.events.emit(Event::ToolCallStarted {
            id,
            parent_id:    self.parent_id,
            depth:        self.depth,
            name:         skill.name().to_string(),
            args_preview: args_preview.clone(),
            expectation:  expectation.clone(),
            args_json,
            call_seq:     self.log_seq.unwrap_or(0),
        });
        let mut notices: Vec<String> = Vec::new();
        let mut veto = false;
        let approval: Option<ApprovalRecord>;

        // Pipeline pre-stage. A denial skips the body but still flows
        // through post-execute (repeat counting) and the finish events so
        // the chip never spins forever and the transcript shows the veto.
        // `view_args` is a clone: the body consumes `args` while post still
        // needs them.
        let view_args = args.clone();
        let view = CallView {
            skill:        skill.name(),
            args:         &view_args,
            args_preview: &args_preview,
            depth:        self.depth,
            session_id:   self.session_id,
        };
        let (denial, record) = self.pre_decision(&view).await;
        approval = record;
        if let Some(denied) = denial {
            let outcome = SkillOutcome { ok: false, summary: denied };
            let (summary, extra, blocked) = self.post_chain(&view, &outcome).await;
            notices.extend(extra);
            veto = true;
            let outcome = SkillOutcome { ok: false, summary: if blocked { summary } else { outcome.summary } };
            let output = ui_output(&outcome.summary);
            self.finish_call(id, skill.name(), args_preview, &outcome, veto, output, started);
            return RunReport { outcome, notices, approval };
        }

        let ctx = SkillContext { sub: self.child(id) };
        // Race the skill against the interrupt and its own wall-clock budget.
        // Dropping the skill future stops it at its next await point; a child
        // process it owns dies with it only if the skill spawned it with
        // `kill_on_drop`.
        let limit = skill.timeout();
        let timed = tokio::time::timeout(limit, skill.run(args, ctx));
        let timed_out = || SkillOutcome {
            ok: false,
            summary: format!(
                "`{}` timed out after {}s (pipeline limit) — the call was abandoned; \
                 narrow the work or split it into smaller calls",
                skill.name(),
                limit.as_secs()
            ),
        };
        let mut outcome = match &self.cancel {
            Some(token) => tokio::select! {
                biased;
                _ = token.cancelled() => SkillOutcome {
                    ok: false,
                    summary: format!("`{}` interrupted", skill.name()),
                },
                res = timed => res.unwrap_or_else(|_| timed_out()),
            },
            None => timed.await.unwrap_or_else(|_| timed_out()),
        };

        // Oversized successful output goes to disk first; the model gets a
        // head/tail digest naming the file. Runs before the summariser so a
        // paraphrase is made from the digest, not from 200 KB of raw text.
        let spilled = self.spill(skill.name(), id, &mut outcome);

        // Pipeline post-stage: accept (possibly with extra context) or
        // block (the feedback replaces the outcome). A block is a policy
        // veto, not a tool defect — sunk never, summarised never.
        let (summary, extra, blocked) = self.post_chain(&view, &outcome).await;
        notices.extend(extra);
        if blocked {
            veto = true;
            outcome = SkillOutcome { ok: false, summary };
        } else {
            outcome.summary = summary;
        }

        // Short outputs are passed through verbatim: the raw text is ground
        // truth, and every LLM rewrite is a chance to misquote it. Only
        // outputs too big to re-ingest are worth the lossy focused summary.
        // Skipped outright on interrupt — the summary feeds the next hop, and
        // there is no next hop.
        // What the model actually received from the tool, before the
        // expectation summariser had a chance to paraphrase it. The expanded
        // row shows this; `summary` is what went into the context.
        let output = ui_output(&outcome.summary);

        const SUMMARIZE_THRESHOLD: usize = 2000;
        if outcome.ok
            && !self.cancelled()
            && !expectation.trim().is_empty()
            && outcome.summary.len() > SUMMARIZE_THRESHOLD
        {
            if let Some(client) = &self.summarizer {
                let focused = match &self.cancel {
                    Some(token) => tokio::select! {
                        biased;
                        _ = token.cancelled() => None,
                        s = summarize(client, skill.name(), &expectation, &outcome.summary) => s,
                    },
                    None => {
                        summarize(client, skill.name(), &expectation, &outcome.summary).await
                    }
                };
                match focused {
                    Some(mut focused) => {
                        debug!(
                            tool_id = id,
                            skill = skill.name(),
                            "sub-agent: summarizer produced focused answer"
                        );
                        // The summariser is free to drop the omission marker;
                        // the path must survive so the model can still reach
                        // the raw output.
                        if let Some(path) = &spilled {
                            focused.push('\n');
                            focused.push_str(&crate::spill::pointer(path));
                        }
                        outcome.summary = focused;
                    }
                    None => {
                        warn!(
                            tool_id = id,
                            skill = skill.name(),
                            "sub-agent: summarizer returned no answer — keeping raw summary"
                        );
                    }
                }
            }
        }

        self.finish_call(id, skill.name(), args_preview, &outcome, veto, output, started);

        RunReport { outcome, notices, approval }
    }

    /// First non-`Allow` pre-execute decision wins; monotonic guards run
    /// after and can only deny. Returns the denial text (if any) plus the
    /// approval audit whenever the verdict went through the broker —
    /// approved or denied, every request is audited.
    async fn pre_decision(&self, view: &CallView<'_>) -> (Option<String>, Option<ApprovalRecord>) {
        let mut verdict: Option<PreDecision> = None;
        for policy in self.policies.iter() {
            match policy.pre_execute(view).await {
                PreDecision::Allow => {}
                other => {
                    verdict = Some(other);
                    break;
                }
            }
        }
        // Guards run even past a denial verdict — deny-only by contract —
        // but only an Allow/Ask verdict can still change.
        let mut verdict = verdict.unwrap_or(PreDecision::Allow);
        if matches!(verdict, PreDecision::Allow | PreDecision::Ask { .. }) {
            for policy in self.policies.iter() {
                if let Some(reason) = policy.guard(view) {
                    verdict = PreDecision::Deny { reason };
                    break;
                }
            }
        }
        match verdict {
            PreDecision::Allow => (None, None),
            PreDecision::Deny { reason } => (Some(reason), None),
            PreDecision::Ask { reason } => {
                let allowed = match (&self.brokers, self.session_id) {
                    (Some(brokers), Some(session_id)) => {
                        brokers
                            .ask_approval(
                                &self.events,
                                session_id,
                                view.skill,
                                view.args_preview,
                                &reason,
                                self.cancel.clone(),
                            )
                            .await
                    }
                    _ => false,
                };
                let record = ApprovalRecord {
                    skill: view.skill.to_string(),
                    args_preview: view.args_preview.to_string(),
                    decision: if allowed { "allowed-once" } else { "denied" },
                };
                if allowed {
                    self.events.emit(Event::LogLine {
                        level: "INFO".into(),
                        message: format!("approval: `{}` allowed once", view.skill),
                    });
                    (None, Some(record))
                } else {
                    self.events.emit(Event::LogLine {
                        level: "WARN".into(),
                        message: format!(
                            "approval: `{}` denied ({})",
                            view.skill,
                            if self.brokers.is_some() && self.session_id.is_some() {
                                "no allow arrived"
                            } else {
                                "no approval path for this call"
                            }
                        ),
                    });
                    (Some(format!(
                        "approval denied ({reason}) — change approach, use a \
                         read-only alternative, or ask the user"
                    )), Some(record))
                }
            }
        }
    }

    /// Run every `post_execute`; the first `Block` wins the summary while
    /// all `extra_context` is collected. Returns (summary, context, blocked).
    async fn post_chain(&self, view: &CallView<'_>, outcome: &SkillOutcome) -> (String, Vec<String>, bool) {
        let mut summary = outcome.summary.clone();
        let mut extra = Vec::new();
        let mut blocked = false;
        for policy in self.policies.iter() {
            match policy.post_execute(view, outcome).await {
                PostDecision::Accept { summary: s, extra_context } => {
                    if !blocked {
                        summary = s;
                    }
                    extra.extend(extra_context);
                }
                PostDecision::Block { feedback, extra_context } => {
                    if !blocked {
                        summary = feedback;
                        blocked = true;
                    }
                    extra.extend(extra_context);
                }
            }
        }
        (summary, extra, blocked)
    }

    /// Shared tail for the body and deny paths: finish logging, the
    /// `ToolCallFinished` chip event, and the failure-sink report (skipped
    /// for interrupts and policy vetoes — neither is a defect).
    #[allow(clippy::too_many_arguments)]
    fn finish_call(
        &self,
        id: u64,
        skill_name: &str,
        args_preview: String,
        outcome: &SkillOutcome,
        veto: bool,
        output: String,
        started: std::time::Instant,
    ) {
        if outcome.ok {
            info!(
                tool_id = id,
                depth = self.depth,
                skill = skill_name,
                "sub-agent: tool call finished ok"
            );
        } else {
            warn!(
                tool_id = id,
                depth = self.depth,
                skill = skill_name,
                summary = %short(&outcome.summary),
                "sub-agent: tool call failed"
            );
        }
        self.events.emit(Event::LogLine {
            level: if outcome.ok { "INFO".into() } else { "WARN".into() },
            message: format!(
                "sub-agent[depth={}, id={}] {} `{}` — {}",
                self.depth,
                id,
                if outcome.ok { "ok" } else { "err" },
                skill_name,
                short(&outcome.summary),
            ),
        });
        self.events.emit(Event::ToolCallFinished {
            id,
            ok: outcome.ok,
            summary: outcome.summary.clone(),
            output,
            duration_ms: started.elapsed().as_millis() as u64,
        });
        if !outcome.ok && !veto && !self.cancelled() {
            if let Some(sink) = &self.failure_sink {
                info!(
                    tool_id = id,
                    skill = skill_name,
                    host_os = std::env::consts::OS,
                    "sub-agent: forwarding failure to idealist sink"
                );
                sink.report(ToolFailureReport {
                    skill:        skill_name.to_string(),
                    args_preview,
                    summary:      outcome.summary.clone(),
                    depth:        self.depth,
                    host_os:      std::env::consts::OS,
                    host_family:  std::env::consts::FAMILY,
                });
            }
        }
    }

    /// Spill an oversized successful outcome to disk and replace its summary
    /// with the digest. Returns the file path when it happened. `read-file`
    /// is exempt so the model's follow-up read of a spill file cannot spill
    /// again. A write failure keeps the raw summary — spilling is an
    /// optimisation, never a reason to fail a call that succeeded.
    fn spill(&self, skill_name: &str, id: u64, outcome: &mut SkillOutcome) -> Option<std::path::PathBuf> {
        let label = self.spill_label.as_deref()?;
        if !outcome.ok
            || self.cancelled()
            || skill_name == crate::builtins::READ_FILE_NAME
            || outcome.summary.len() <= crate::spill::SPILL_THRESHOLD
        {
            return None;
        }
        let base = sica_core::paths::spill_dir();
        match crate::spill::write(&base, label, skill_name, id, &outcome.summary) {
            Ok(path) => {
                info!(
                    tool_id = id,
                    skill = skill_name,
                    bytes = outcome.summary.len(),
                    path = %path.display(),
                    "sub-agent: output spilled to disk"
                );
                self.events.emit(Event::LogLine {
                    level: "INFO".into(),
                    message: format!(
                        "sub-agent[depth={}, id={}] `{}` output ({} bytes) spilled to {}",
                        self.depth,
                        id,
                        skill_name,
                        outcome.summary.len(),
                        path.display()
                    ),
                });
                outcome.summary = crate::spill::digest(&outcome.summary, &path);
                Some(path)
            }
            Err(e) => {
                warn!(
                    tool_id = id,
                    skill = skill_name,
                    error = %e,
                    "sub-agent: spill write failed — keeping raw output"
                );
                None
            }
        }
    }
}

/// Truncate a string to a single short log-friendly line. Used for log
/// payloads where multi-line summaries would drown the panel.
fn short(s: &str) -> String {
    const CAP: usize = 160;
    let one_line: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if one_line.chars().count() <= CAP {
        return one_line;
    }
    let mut out: String = one_line.chars().take(CAP).collect();
    out.push('…');
    out
}

/// Best-effort LLM summary. Returns `None` on any transport / network error
/// — the caller falls back to the raw skill summary so a flaky LLM never
/// breaks the tool-call chain.
async fn summarize(
    client:      &LlmClient,
    skill_name:  &str,
    expectation: &str,
    raw:         &str,
) -> Option<String> {
    let system = format!(
        "You summarize the raw output of skill `{skill_name}` for the main agent. \
         Reply with a concise focused answer (at most ~10 lines) addressing the \
         expectation below. Quote exact values — numbers, versions, paths, error \
         messages — verbatim from the raw output; never infer, round, or guess. \
         If the raw output does not contain the answer, say exactly that — do not \
         substitute a plausible answer. Do not include any preamble or fenced \
         blocks; output only the answer."
    );
    let user = format!("Expectation: {expectation}\n\nRaw output:\n{raw}");
    let messages = vec![
        ChatMessage::text("system", system),
        ChatMessage::text("user", user),
    ];
    match client.chat_once(messages).await {
        Ok(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

/// Largest argument blob worth putting on the wire for the UI. A
/// `write-file` body can be megabytes; the expanded row shows a few hundred
/// pixels of it. Over the cap the field goes out empty and the frontend falls
/// back to `args_preview` — a truncated JSON string would not parse, and a
/// row that silently showed half an argument would be worse than one that
/// shows the summary line.
const UI_ARGS_JSON_MAX: usize = 64 * 1024;

/// Largest tool output worth putting on the wire. Shell results are already
/// capped at 32 KiB and oversized successes spill to disk, but `read-file` is
/// exempt from spilling (so a follow-up read cannot spill again) and can
/// return up to 1 MiB.
const UI_OUTPUT_MAX: usize = 256 * 1024;

fn ui_args_json(args: &serde_json::Value) -> String {
    let text = args.to_string();
    if text.len() > UI_ARGS_JSON_MAX { String::new() } else { text }
}

/// The output as the model received it, bounded for the UI. When it is cut,
/// the shared omission sentence says so — the row must never imply it is
/// showing everything.
fn ui_output(summary: &str) -> String {
    if summary.len() <= UI_OUTPUT_MAX {
        return summary.to_string();
    }
    let window = sica_core::retain::head_tail(summary, UI_OUTPUT_MAX * 3 / 4, UI_OUTPUT_MAX / 4);
    format!(
        "{}\n{}\n{}",
        window.head,
        sica_core::retain::notice(window.omitted, ""),
        window.tail
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct Capture(Mutex<Vec<Event>>);

    impl EventSink for Capture {
        fn emit(&self, ev: Event) {
            self.0.lock().unwrap().push(ev);
        }
    }

    struct Echo;
    #[async_trait]
    impl Skill for Echo {
        fn name(&self) -> &str { "echo" }
        async fn run(&self, _args: Value, _ctx: SkillContext) -> SkillOutcome {
            SkillOutcome { ok: true, summary: "echoed".into() }
        }
    }

    fn inv(skill: &dyn Skill) -> ToolInvocation<'_> {
        ToolInvocation {
            skill,
            args: Value::Null,
            raw_args: Vec::new(),
            expectation: String::new(),
        }
    }

    /// Filter the captured event stream to only `ToolCallStarted` /
    /// `ToolCallFinished` so tests stay focused on the call lifecycle rather
    /// than the surrounding `LogLine` instrumentation.
    fn lifecycle(events: &[Event]) -> Vec<Event> {
        events
            .iter()
            .filter(|e| matches!(e, Event::ToolCallStarted { .. } | Event::ToolCallFinished { .. }))
            .cloned()
            .collect()
    }

    #[tokio::test]
    async fn child_increments_depth_and_parent() {
        let cap = Arc::new(Capture(Mutex::new(Vec::new())));
        let root = ToolSubAgent::root(cap.clone());
        let outcome = root.run(inv(&Echo)).await;
        assert!(outcome.ok);
        let events = lifecycle(&cap.0.lock().unwrap());
        assert_eq!(events.len(), 2);
        if let Event::ToolCallStarted { depth, parent_id, .. } = &events[0] {
            assert_eq!(*depth, 0);
            assert!(parent_id.is_none());
        } else { panic!("expected ToolCallStarted"); }
    }

    #[tokio::test]
    async fn depth_limit_aborts() {
        let cap = Arc::new(Capture(Mutex::new(Vec::new())));
        let mut sub = ToolSubAgent::root(cap.clone());
        sub.depth = sub.max_depth;
        let outcome = sub.run(inv(&Echo)).await;
        assert!(!outcome.ok);
        assert!(outcome.summary.contains("depth limit"));
    }

    struct Fail;
    #[async_trait]
    impl Skill for Fail {
        fn name(&self) -> &str { "fail" }
        async fn run(&self, _args: Value, _ctx: SkillContext) -> SkillOutcome {
            SkillOutcome { ok: false, summary: "boom".into() }
        }
    }

    struct CaptureFailures(Mutex<Vec<ToolFailureReport>>);
    impl ToolFailureSink for CaptureFailures {
        fn report(&self, r: ToolFailureReport) {
            self.0.lock().unwrap().push(r);
        }
    }

    #[tokio::test]
    async fn failure_is_reported_to_sink() {
        let cap = Arc::new(Capture(Mutex::new(Vec::new())));
        let failures = Arc::new(CaptureFailures(Mutex::new(Vec::new())));
        let root = ToolSubAgent::root(cap.clone())
            .with_failure_sink(failures.clone());
        let invocation = ToolInvocation {
            skill: &Fail,
            args: serde_json::json!({"command": "no-such-cmd"}),
            raw_args: vec!["no-such-cmd".into()],
            expectation: String::new(),
        };
        let out = root.run(invocation).await;
        assert!(!out.ok);
        let reports = failures.0.lock().unwrap().clone();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].skill, "fail");
        assert_eq!(reports[0].summary, "boom");
        assert!(reports[0].args_preview.contains("no-such-cmd"));
    }

    #[tokio::test]
    async fn success_does_not_report_failure() {
        let cap = Arc::new(Capture(Mutex::new(Vec::new())));
        let failures = Arc::new(CaptureFailures(Mutex::new(Vec::new())));
        let root = ToolSubAgent::root(cap.clone())
            .with_failure_sink(failures.clone());
        let out = root.run(inv(&Echo)).await;
        assert!(out.ok);
        assert!(failures.0.lock().unwrap().is_empty());
    }

    /// Sleeps far longer than its own declared timeout.
    struct Slow;
    #[async_trait]
    impl Skill for Slow {
        fn name(&self) -> &str { "slow" }
        fn timeout(&self) -> std::time::Duration { std::time::Duration::from_millis(10) }
        async fn run(&self, _args: Value, _ctx: SkillContext) -> SkillOutcome {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            SkillOutcome { ok: true, summary: "finished".into() }
        }
    }

    #[tokio::test]
    async fn timeout_fails_the_call_and_reports_to_sink() {
        let cap = Arc::new(Capture(Mutex::new(Vec::new())));
        let failures = Arc::new(CaptureFailures(Mutex::new(Vec::new())));
        let root = ToolSubAgent::root(cap.clone()).with_failure_sink(failures.clone());
        let out = root.run(inv(&Slow)).await;
        assert!(!out.ok);
        assert!(out.summary.contains("timed out"), "{}", out.summary);
        let reports = failures.0.lock().unwrap();
        assert_eq!(reports.len(), 1, "a timeout is a defect worth a ticket");
        let events = lifecycle(&cap.0.lock().unwrap());
        assert!(matches!(events[1], Event::ToolCallFinished { ok: false, .. }));
    }

    #[tokio::test]
    async fn cancel_beats_timeout_and_is_not_reported() {
        let cap = Arc::new(Capture(Mutex::new(Vec::new())));
        let failures = Arc::new(CaptureFailures(Mutex::new(Vec::new())));
        let token = CancellationToken::new();
        token.cancel();
        let root = ToolSubAgent::root(cap.clone())
            .with_failure_sink(failures.clone())
            .with_cancel(token);
        let out = root.run(inv(&Slow)).await;
        assert!(!out.ok);
        assert!(out.summary.contains("interrupted"), "{}", out.summary);
        assert!(failures.0.lock().unwrap().is_empty());
    }

    /// Returns a payload well over the spill threshold.
    struct Firehose;
    #[async_trait]
    impl Skill for Firehose {
        fn name(&self) -> &str { "firehose" }
        async fn run(&self, _args: Value, _ctx: SkillContext) -> SkillOutcome {
            SkillOutcome { ok: true, summary: "z".repeat(crate::spill::SPILL_THRESHOLD + 1) }
        }
    }

    #[tokio::test]
    async fn oversized_output_is_spilled_when_labelled() {
        let cap = Arc::new(Capture(Mutex::new(Vec::new())));
        let label = format!("test-{}-{}", std::process::id(), next_tool_id());
        let root = ToolSubAgent::root(cap.clone()).with_spill_label(label.clone());
        let out = root.run(inv(&Firehose)).await;
        assert!(out.ok);
        assert!(out.summary.len() < crate::spill::SPILL_THRESHOLD);
        assert!(out.summary.contains("bytes omitted"), "{}", out.summary);
        let dir = sica_core::paths::spill_dir().join(&label);
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(files.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn oversized_output_passes_through_without_label() {
        let cap = Arc::new(Capture(Mutex::new(Vec::new())));
        let root = ToolSubAgent::root(cap.clone());
        let out = root.run(inv(&Firehose)).await;
        assert_eq!(out.summary.len(), crate::spill::SPILL_THRESHOLD + 1);
    }

    struct DenyAll;
    #[async_trait]
    impl crate::pipeline::ToolPolicy for DenyAll {
        async fn pre_execute(
            &self,
            _call: &crate::pipeline::CallView<'_>,
        ) -> crate::pipeline::PreDecision {
            crate::pipeline::PreDecision::Deny { reason: "nope".into() }
        }
    }

    #[tokio::test]
    async fn deny_skips_body_and_sink_but_still_finishes() {
        let cap = Arc::new(Capture(Mutex::new(Vec::new())));
        let failures = Arc::new(CaptureFailures(Mutex::new(Vec::new())));
        let root = ToolSubAgent::root(cap.clone())
            .with_failure_sink(failures.clone())
            .with_policies(vec![Arc::new(DenyAll)]);
        let report = root.run_report(inv(&Echo)).await;
        assert!(!report.outcome.ok);
        assert!(report.outcome.summary.contains("nope"));
        assert!(report.approval.is_none());
        // A denial is the harness working, not a defect: no ticket.
        assert!(failures.0.lock().unwrap().is_empty());
        let events = lifecycle(&cap.0.lock().unwrap());
        assert_eq!(events.len(), 2, "denied calls still open and close the chip");
        assert!(matches!(events[1], Event::ToolCallFinished { ok: false, .. }));
    }

    struct AskAll;
    #[async_trait]
    impl crate::pipeline::ToolPolicy for AskAll {
        async fn pre_execute(
            &self,
            _call: &crate::pipeline::CallView<'_>,
        ) -> crate::pipeline::PreDecision {
            crate::pipeline::PreDecision::Ask { reason: "sure?".into() }
        }
    }

    #[tokio::test]
    async fn ask_without_broker_denies() {
        let cap = Arc::new(Capture(Mutex::new(Vec::new())));
        let root =
            ToolSubAgent::root(cap.clone()).with_policies(vec![Arc::new(AskAll)]);
        let report = root.run_report(inv(&Echo)).await;
        assert!(!report.outcome.ok);
        assert!(report.outcome.summary.contains("approval denied"));
        let record = report.approval.expect("denials through Ask are audited");
        assert_eq!(record.decision, "denied");
    }

    #[tokio::test]
    async fn ask_with_broker_runs_body_on_allow() {
        use crate::broker::BrokerSet;
        let cap = Arc::new(Capture(Mutex::new(Vec::new())));
        let brokers = Arc::new(BrokerSet::new());
        let root = ToolSubAgent::root(cap.clone())
            .with_session(9)
            .with_brokers(brokers.clone())
            .with_policies(vec![Arc::new(AskAll)]);
        let (report, _) = tokio::join!(root.run_report(inv(&Echo)), async {
            for _ in 0..100 {
                // Id 1: the first broker request of this fresh set.
                if brokers.resolve_approval(1, true).await {
                    break;
                }
                tokio::task::yield_now().await;
            }
        });
        assert!(report.outcome.ok, "{}", report.outcome.summary);
        let record = report.approval.expect("allowed asks are audited too");
        assert_eq!(record.decision, "allowed-once");
        assert_eq!(record.skill, "echo");
    }

    #[tokio::test]
    async fn repeat_notice_arrives_as_context_not_block() {
        let cap = Arc::new(Capture(Mutex::new(Vec::new())));
        let reminder = Arc::new(crate::pipeline::RepeatReminder::new());
        let root = ToolSubAgent::root(cap)
            .with_policies(vec![reminder.clone() as Arc<dyn crate::pipeline::ToolPolicy>]);
        let args = serde_json::json!({});
        for _ in 0..2 {
            let r = root
                .run_report(ToolInvocation {
                    skill: &Echo,
                    args: args.clone(),
                    raw_args: Vec::new(),
                    expectation: String::new(),
                })
                .await;
            assert!(r.outcome.ok);
            assert!(r.notices.is_empty());
        }
        let r = root
            .run_report(ToolInvocation {
                skill: &Echo,
                args,
                raw_args: Vec::new(),
                expectation: String::new(),
            })
            .await;
        assert!(r.outcome.ok, "the reminder advises, never blocks");
        assert_eq!(r.notices.len(), 1);
        assert!(r.notices[0].contains("3 times"));
    }

    /// A payload too big for a row must not silently become "half the
    /// arguments" or "half the output".
    #[test]
    fn ui_payloads_are_bounded_and_say_when_they_were_cut() {
        let small = serde_json::json!({"path": "a.rs"});
        assert_eq!(ui_args_json(&small), small.to_string());
        let huge = serde_json::json!({"content": "x".repeat(UI_ARGS_JSON_MAX)});
        assert!(
            ui_args_json(&huge).is_empty(),
            "an oversized argument blob is dropped, not truncated into unparsable JSON"
        );

        assert_eq!(ui_output("hello"), "hello");
        let long = "y".repeat(UI_OUTPUT_MAX + 5_000);
        let cut = ui_output(&long);
        assert!(cut.len() < long.len());
        assert!(cut.contains("omitted"), "the cut must announce itself: {}", &cut[..200]);
    }

    #[tokio::test]
    async fn started_event_carries_args_preview_and_expectation() {
        let cap = Arc::new(Capture(Mutex::new(Vec::new())));
        let root = ToolSubAgent::root(cap.clone());
        let invocation = ToolInvocation {
            skill: &Echo,
            args: Value::Null,
            raw_args: vec!["hello there".into()],
            expectation: "is the echo working".into(),
        };
        let _ = root.run(invocation).await;
        let events = lifecycle(&cap.0.lock().unwrap());
        if let Event::ToolCallStarted { args_preview, expectation, .. } = &events[0] {
            assert_eq!(args_preview, "echo 'hello there'");
            assert_eq!(expectation, "is the echo working");
        } else {
            panic!("expected ToolCallStarted as first event");
        }
    }
}
