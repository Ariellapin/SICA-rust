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
        }
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

    /// Build a child sub-agent rooted at the call id `parent_id`. Used by
    /// `SkillContext` so a skill can spawn further sub-agents. Inherits the
    /// failure sink and summarizer so nested calls share configuration.
    pub fn child(&self, parent_id: u64) -> Self {
        Self {
            depth:        self.depth.saturating_add(1),
            parent_id:    Some(parent_id),
            max_depth:    self.max_depth,
            events:       self.events.clone(),
            failure_sink: self.failure_sink.clone(),
            summarizer:   self.summarizer.clone(),
        }
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
        let mut outcome = skill.run(args, ctx).await;

        if outcome.ok && !expectation.trim().is_empty() {
            if let Some(client) = &self.summarizer {
                match summarize(client, skill.name(), &expectation, &outcome.summary).await {
                    Some(focused) => {
                        debug!(
                            tool_id = id,
                            skill = skill.name(),
                            "sub-agent: summarizer produced focused answer"
                        );
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

        if !outcome.ok {
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
         Reply with a concise focused answer (at most ~6 lines) addressing the \
         expectation below. If the raw output does not contain the answer, say so \
         plainly. Do not include any preamble or fenced blocks; output only the answer."
    );
    let user = format!("Expectation: {expectation}\n\nRaw output:\n{raw}");
    let messages = vec![
        ChatMessage { role: "system".into(), content: system },
        ChatMessage { role: "user".into(),   content: user   },
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
