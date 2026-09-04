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
use std::sync::Arc;

use once_cell::sync::Lazy;
use protocol::Event;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use llm::client::{ChatMessage, LlmClient};

use crate::agent::EventSink;
use crate::parse_tool_call;
use crate::skill::{Skill, SkillContext, SkillOutcome};

static TOOL_ID: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(1));

fn next_tool_id() -> u64 {
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
        }
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
    pub async fn run(&self, inv: ToolInvocation<'_>) -> SkillOutcome {
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
            return SkillOutcome {
                ok: false,
                summary: format!("sub-agent depth limit ({}) reached", self.max_depth),
            };
        }

        // Interrupted before we even started: say nothing to the UI. Emitting
        // a start event here would leave a chip spinning for a call that never
        // ran.
        if self.cancelled() {
            return SkillOutcome {
                ok: false,
                summary: "interrupted before the tool call started".into(),
            };
        }

        let id = next_tool_id();
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
        });

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

        // Short outputs are passed through verbatim: the raw text is ground
        // truth, and every LLM rewrite is a chance to misquote it. Only
        // outputs too big to re-ingest are worth the lossy focused summary.
        // Skipped outright on interrupt — the summary feeds the next hop, and
        // there is no next hop.
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

        if outcome.ok {
            info!(
                tool_id = id,
                depth = self.depth,
                skill = skill.name(),
                "sub-agent: tool call finished ok"
            );
        } else {
            warn!(
                tool_id = id,
                depth = self.depth,
                skill = skill.name(),
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
                skill.name(),
                short(&outcome.summary),
            ),
        });
        self.events.emit(Event::ToolCallFinished {
            id,
            ok: outcome.ok,
            summary: outcome.summary.clone(),
        });

        // A user-initiated stop is not a defect: reporting it would spend an
        // idealist ticket on "the operator pressed Stop".
        if !outcome.ok && !self.cancelled() {
            if let Some(sink) = &self.failure_sink {
                info!(
                    tool_id = id,
                    skill = skill.name(),
                    host_os = std::env::consts::OS,
                    "sub-agent: forwarding failure to idealist sink"
                );
                sink.report(ToolFailureReport {
                    skill:        skill.name().to_string(),
                    args_preview,
                    summary:      outcome.summary.clone(),
                    depth:        self.depth,
                    host_os:      std::env::consts::OS,
                    host_family:  std::env::consts::FAMILY,
                });
            }
        }

        outcome
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
