//! Chat session bookkeeping + LLM connection wiring used by the dispatcher.
//!
//! Sessions are append-only event logs ([`SessionLog`]); the history sent
//! to the model is derived from them on every hop. Nothing in this module
//! mutates a message list — every persistence site is an [`append_event`].

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use protocol::{Event, Frame, LlmOptions, LlmState, MessageDump, PermissionMode, SessionDump, SessionMeta, UserImage};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use agents::goal::Goal;
use agents::meter::TokenMeter;
use agents::pipeline::{PermissionPolicy, PlanModePolicy, ReadBeforeEdit, RepeatReminder, ToolPolicy};
use agents::{
    BrokerSet, EventSink, SkillRegistry, ToolFailureSink, ToolSubAgent,
};
use llm::client::{ChatContent, ChatMessage, ContentPart, ImageUrl, LlmClient};
use sica_core::event::{ContextSource, EventKind, SurfaceEntry, SurfaceOp, TurnSource};
use sica_core::message::{Message, Role};

use crate::hooks;
use crate::inbox::{Inbound, Inbox};
use crate::sessions_store::{self, SessionLog};
use crate::title_gen;

/// Hard cap on tool hops within one user message. Stops a model from
/// ping-ponging skill calls forever when it cannot decide a final answer.
/// Generous because the documented workflow spends hops on `read-file`ing
/// `skills/*.md` contracts before the real calls.
const MAX_TOOL_HOPS: u8 = 12;

/// Largest resolved-argument blob recorded on a durable `ToolCall`. Past it
/// the field is omitted and a rebuilt row falls back to `args_preview`; the
/// whole log is parsed at every backend start, so a megabyte of `write-file`
/// body would be paid for on every one.
const LOGGED_ARGS_JSON_MAX: usize = 64 * 1024;

/// Title given to a freshly minted session. Used both at creation time and
/// as the trigger for the auto-title agent — if the title still matches
/// this format after the first response, we replace it with a summary.
pub fn default_title(id: u64) -> String {
    format!("Session {id}")
}

pub type Sessions = Arc<Mutex<HashMap<u64, SessionLog>>>;

#[derive(Clone)]
pub struct ChatHub {
    pub sessions:      Sessions,
    pub next_id:       Arc<AtomicU64>,
    pub next_turn:     Arc<AtomicU64>,
    pub llm:           Arc<Mutex<Option<LlmClient>>>,
    pub llm_state:     Arc<Mutex<LlmState>>,
    pub out_tx:        mpsc::UnboundedSender<Frame>,
    pub event_sink:    Arc<dyn EventSink>,
    /// Skill catalogue used to dispatch `tool_call` blocks parsed from the
    /// assistant's reply. Shared (immutable post-startup) so cloning a hub
    /// does not copy the map.
    pub skills:        Arc<SkillRegistry>,
    /// Forwards each failed sub-agent tool call into the idealist daemon.
    /// `None` only in test contexts where the daemon isn't running.
    pub failure_sink:  Option<Arc<dyn ToolFailureSink>>,
    /// One cancellation token per session for the currently-running user
    /// turn. `InterruptTurn` looks the session's token up and fires it,
    /// which propagates into `run_turn` / `chat_stream`. The `u64` is a
    /// monotonically-increasing marker so a finishing turn can avoid
    /// removing a *later* turn's token from the slot.
    pub active_turns:  Arc<Mutex<HashMap<u64, (u64, CancellationToken)>>>,
    pub next_marker:   Arc<AtomicU64>,
    /// Options the FE sent with the last successful `ConnectLlm`.
    pub llm_opts:      Arc<Mutex<LlmOptions>>,
    /// Effective prompt window (configured or auto-detected at connect).
    pub context_window: Arc<AtomicU32>,
    /// Repeat-tool-reminder policy per session (`agents::pipeline`). A new
    /// user message drops the entry; every sub-agent of the session shares
    /// the instance, so denied calls count exactly like executed ones.
    pub repeat:        Arc<Mutex<HashMap<u64, Arc<RepeatReminder>>>>,
    /// Observed file versions per session for read-before-edit.
    pub read_seen:     Arc<Mutex<HashMap<u64, Arc<ReadBeforeEdit>>>>,
    /// Permission mode per session. Absent = default (`WorkspaceWrite`);
    /// restored from the log's latest `PermissionMode` event on load.
    pub permissions:   Arc<Mutex<HashMap<u64, PermissionMode>>>,
    /// Plan-mode flag per session; restored from the log on load.
    pub plans:         Arc<Mutex<HashMap<u64, bool>>>,
    /// Selected agent preset per session (guide §5.2), restored from the
    /// log's latest `AgentPreset` event. Absent = no preset: the default
    /// persona-less prompt against the full registry.
    pub presets:       Arc<Mutex<HashMap<u64, String>>>,
    /// Human-in-the-loop rendezvous for pipeline approvals and questions.
    pub brokers:       Arc<BrokerSet>,
    /// Usage-anchored token meter per session (`agents::meter`). Anchored on
    /// the provider's own `usage` after each successful request; cleared on
    /// reconnect.
    pub meters:        Arc<Mutex<HashMap<u64, TokenMeter>>>,
    /// What is waiting to enter each session's loop (`crate::inbox`): user
    /// messages sent while a turn was running, mid-turn steers, and context
    /// the runtime wants the model to see at its next step.
    pub inbox:         Arc<Inbox>,
    /// Background jobs (`agents::jobs`). `main.rs` builds this *before* the
    /// skill registry — the shell skills need it to start a job — and
    /// hands it over with [`Self::with_jobs`], so the hub and the skills
    /// share one registry.
    pub jobs:          Arc<agents::JobRegistry>,
    /// Durable objective per session (`agents::goal`), restored from the
    /// log's latest `GoalChange` on load.
    pub goals:         Arc<Mutex<HashMap<u64, Goal>>>,
    /// Sessions whose goal is *armed* — the round driver only opens rounds
    /// for these. Deliberately not persisted and empty at startup: a
    /// backend restart must never resume an autonomous loop on its own, so
    /// a restored active goal waits for `/goal continue`.
    pub goal_armed:    Arc<Mutex<HashSet<u64>>>,
    pub next_goal_id:  Arc<AtomicU64>,
    /// User hooks from `.sica/hooks.json` (guide §13.1). Read once at
    /// startup: a hooks file that could change under a running turn
    /// would make two calls in one turn answer to different rules.
    pub hooks:         Arc<hooks::HookConfig>,
}

/// Wave-3 per-session control plane, shared with the turn task: the pieces
/// a dispatch needs beyond the skill map (policies, brokers, mode maps).
/// Cloned into the spawned turn so the loop sees mode flips live.
#[derive(Clone)]
struct ControlState {
    events:       Arc<dyn EventSink>,
    failure_sink: Option<Arc<dyn ToolFailureSink>>,
    permissions:  Arc<Mutex<HashMap<u64, PermissionMode>>>,
    plans:        Arc<Mutex<HashMap<u64, bool>>>,
    repeat:       Arc<Mutex<HashMap<u64, Arc<RepeatReminder>>>>,
    read_seen:    Arc<Mutex<HashMap<u64, Arc<ReadBeforeEdit>>>>,
    brokers:      Arc<BrokerSet>,
    goals:        Arc<Mutex<HashMap<u64, Goal>>>,
    /// Who opened the turn this state belongs to. The goal skills' authority
    /// check reads it: an automatic round may not set or pause its own
    /// objective. Carried here rather than passed down four call layers.
    turn_source:  TurnSource,
    /// Sessions whose goal is *armed*. Process-local by design (§12.3) —
    /// see `ControlState::armed`.
    arm_set:      Arc<Mutex<HashSet<u64>>>,
    next_goal:    Arc<AtomicU64>,
    /// User hooks, so a dispatch can put `HooksPolicy` in the pipeline.
    hooks:        Arc<hooks::HookConfig>,
}

impl ControlState {
    async fn mode(&self, session_id: u64) -> PermissionMode {
        self.permissions
            .lock()
            .await
            .get(&session_id)
            .copied()
            .unwrap_or_default()
    }

    async fn plan_active(&self, session_id: u64) -> bool {
        self.plans.lock().await.get(&session_id).copied().unwrap_or(false)
    }

    async fn reminder(&self, session_id: u64) -> Arc<RepeatReminder> {
        self.repeat
            .lock()
            .await
            .entry(session_id)
            .or_insert_with(|| Arc::new(RepeatReminder::new()))
            .clone()
    }

    async fn reader(&self, session_id: u64) -> Arc<ReadBeforeEdit> {
        self.read_seen
            .lock()
            .await
            .entry(session_id)
            .or_insert_with(|| {
                Arc::new(ReadBeforeEdit::new(sica_core::paths::working_dir()))
            })
            .clone()
    }

    /// One fully-wired sub-agent for a main-agent dispatch: policies from
    /// the session's live mode/plan, shared per-session policy state, the
    /// broker rendezvous, and the usual summarizer / cancel / spill / sink.
    async fn sub_agent(
        &self,
        sessions: &Sessions,
        session_id: u64,
        client: &LlmClient,
        cancel: CancellationToken,
    ) -> ToolSubAgent {
        let mode = self.mode(session_id).await;
        let plan = self.plan_active(session_id).await;
        let reminder = self.reminder(session_id).await;
        let reader = self.reader(session_id).await;
        let root = sica_core::paths::working_dir();
        // The hook policy goes in only when hooks are configured: an
        // always-present policy that answers `Allow` still costs a lock and
        // a payload build on every call, and the common case is no hooks.
        let mut policies: Vec<Arc<dyn ToolPolicy>> = vec![
            Arc::new(PermissionPolicy { mode, workspace_root: root }) as Arc<dyn ToolPolicy>,
            Arc::new(PlanModePolicy { active: plan }) as Arc<dyn ToolPolicy>,
            reader as Arc<dyn ToolPolicy>,
            reminder as Arc<dyn ToolPolicy>,
        ];
        if !self.hooks.is_empty() {
            policies.push(Arc::new(crate::hooks::HooksPolicy {
                config:   self.hooks.clone(),
                sessions: sessions.clone(),
                events:   self.events.clone(),
            }) as Arc<dyn ToolPolicy>);
        }
        let mut sub = ToolSubAgent::root(self.events.clone())
            .with_summarizer(client.clone())
            .with_cancel(cancel)
            .with_spill_label(session_id.to_string())
            .with_brokers(self.brokers.clone())
            .with_session(session_id)
            .with_plan_active(plan)
            .with_policies(policies);
        if let Some(fs) = self.failure_sink.clone() {
            sub = sub.with_failure_sink(fs);
        }
        // `subagent-fork` seeds its child from this; absent on a session
        // whose first turn has not finished yet, and the skill says so
        // rather than silently running a fresh child.
        if let Some(seed) = fork_seed(sessions, session_id).await {
            sub = sub.with_fork_seed(seed);
        }
        sub
    }

    /// Feed one pipeline notice to the model as `ToolNotice` context plus a
    /// WARN log line. The reminder advises — it never blocks the call.
    async fn inject_notices(
        &self,
        sessions: &Sessions,
        session_id: u64,
        notices: Vec<String>,
    ) {
        for notice in notices {
            warn!(session_id, "repeat-tool reminder issued");
            self.events.emit(Event::LogLine {
                level: "WARN".into(),
                message: "loop guard: called repeatedly with identical arguments — \
                          reminder injected"
                    .into(),
            });
            append_event(sessions, session_id, EventKind::ContextInjected {
                surface: SurfaceOp::Append,
                source: ContextSource::ToolNotice,
                content: notice,
            })
            .await;
        }
    }

    /// Manual repeat counting for outcomes that bypassed the pipeline
    /// (hop-limit, unknown skill): failed and unknown calls count too — a
    /// model hammering a failing call is exactly the loop worth breaking.
    async fn observe(
        &self,
        sessions: &Sessions,
        session_id: u64,
        skill: &str,
        args: &serde_json::Value,
    ) {
        let notice = self.reminder(session_id).await.observe(skill, args);
        if let Some(notice) = notice {
            self.inject_notices(sessions, session_id, vec![notice]).await;
        }
    }

    /// Run a harness-control skill (`todo-write`, `exit-plan-mode`) without
    /// a sub-agent: both mutate the session log, and `exit-plan-mode`
    /// concludes the turn. Chips are emitted here so the transcript shows
    /// the call like any other. Returns the outcome and whether the turn
    /// ends now.
    #[allow(clippy::too_many_arguments)]
    /// This session's goal, if it has one.
    async fn goal(&self, session_id: u64) -> Option<Goal> {
        self.goals.lock().await.get(&session_id).cloned()
    }

    /// Whether the round driver may open rounds for this session. Process
    /// state, never persisted: an active goal comes back disarmed after a
    /// restart and waits for a human to say continue.
    async fn armed(&self, session_id: u64) -> bool {
        self.arm_set.lock().await.contains(&session_id)
    }

    async fn set_armed(&self, session_id: u64, on: bool) {
        let mut g = self.arm_set.lock().await;
        if on {
            g.insert(session_id);
        } else {
            g.remove(&session_id);
        }
    }

    /// Persist a goal change, remember it, and push it to the FE. Every
    /// mutation goes through here so the three copies — log, memory,
    /// frontend — cannot drift.
    async fn put_goal(&self, sessions: &Sessions, session_id: u64, goal: Goal) {
        append_event(sessions, session_id, EventKind::GoalChange {
            goal_id:        goal.id,
            revision:       goal.revision,
            objective:      goal.objective.clone(),
            phase:          goal.phase,
            rounds_started: goal.rounds_started,
            max_rounds:     goal.max_rounds,
            blocker:        goal.blocker.clone(),
        })
        .await;
        // A goal that can no longer run rounds is disarmed as a matter of
        // course: leaving it armed would restart the loop the moment
        // someone resumed it.
        if !goal.rounds_left() {
            self.set_armed(session_id, false).await;
        }
        let armed = self.armed(session_id).await;
        self.events.emit(Event::GoalChanged {
            session_id,
            goal: Some(goal_dump(&goal, armed)),
        });
        self.goals.lock().await.insert(session_id, goal);
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_control(
        &self,
        sessions: &Sessions,
        name: &str,
        args: &serde_json::Value,
        args_preview: &str,
        expectation: &str,
        session_id: u64,
        call_seq: u64,
        cancel: &CancellationToken,
    ) -> (agents::SkillOutcome, bool) {
        let id = agents::subagent::next_tool_id();
        let started = std::time::Instant::now();
        self.events.emit(Event::ToolCallStarted {
            id,
            parent_id: None,
            depth: 0,
            name: name.to_string(),
            args_preview: args_preview.to_string(),
            expectation: expectation.to_string(),
            args_json: args.to_string(),
            call_seq,
        });
        let (outcome, conclude) = self
            .handle_control_body(sessions, name, args, session_id, cancel)
            .await;
        self.events.emit(Event::ToolCallFinished {
            id,
            ok: outcome.ok,
            summary: outcome.summary.clone(),
            // Harness controls have no summariser between them and the model.
            output: outcome.summary.clone(),
            duration_ms: started.elapsed().as_millis() as u64,
        });
        (outcome, conclude)
    }

    async fn handle_control_body(
        &self,
        sessions: &Sessions,
        name: &str,
        args: &serde_json::Value,
        session_id: u64,
        cancel: &CancellationToken,
    ) -> (agents::SkillOutcome, bool) {
        let source = self.turn_source;
        let fail = |summary: &str| agents::SkillOutcome { ok: false, summary: summary.into() };
        if name == agents::control::TODO_WRITE_NAME {
            // Accepts the real array as well as its JSON text — the tool
            // schema says `string`, native models often send the array.
            let raw = args.get("items").unwrap_or(&serde_json::Value::Null).clone();
            match agents::control::parse_todo_items(&raw) {
                Err(e) => (fail(&e), false),
                Ok(items) => {
                    let total = items.len();
                    let active = items
                        .iter()
                        .filter(|t| t.status == protocol::TodoStatus::InProgress)
                        .count();
                    append_event(sessions, session_id, EventKind::TodoWrite {
                        items: items.clone(),
                    })
                    .await;
                    self.events.emit(Event::TodosChanged { session_id, items });
                    (
                        agents::SkillOutcome {
                            ok: true,
                            summary: format!(
                                "todo list updated: {total} items, {active} in progress"
                            ),
                        },
                        false,
                    )
                }
            }
        } else if name == agents::control::EXIT_PLAN_MODE_NAME {
            let plan = args
                .get("plan")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if plan.is_empty() {
                return (
                    fail("missing or empty `plan` arg — pass the full plan markdown"),
                    false,
                );
            }
            if !self.plan_active(session_id).await {
                return (
                    fail("`exit-plan-mode` outside plan mode — nothing to exit"),
                    false,
                );
            }
            let question = format!(
                "The agent proposes this plan for approval:\n\n{plan}\n\n\
                 Approve it (leaves plan mode), or keep planning?"
            );
            let options = vec!["Approve".to_string(), "Keep planning".to_string()];
            match self
                .brokers
                .ask_question(
                    &self.events,
                    session_id,
                    &question,
                    None,
                    &options,
                    false,
                    Some(cancel.clone()),
                )
                .await
            {
                None => (
                    fail("plan review got no answer (timeout or interrupt) — staying in plan mode"),
                    false,
                ),
                Some(answer) if answer.trim().eq_ignore_ascii_case("approve") => {
                    self.plans.lock().await.insert(session_id, false);
                    append_event(sessions, session_id, EventKind::PlanMode { active: false }).await;
                    self.events.emit(Event::PlanModeChanged { session_id, active: false });
                    self.events.emit(Event::LogLine {
                        level: "INFO".into(),
                        message: "plan approved — plan mode off".into(),
                    });
                    (
                        agents::SkillOutcome {
                            ok: true,
                            summary: "plan approved by the user — plan mode is now off; \
                                      execute or summarise the plan directly"
                                .into(),
                        },
                        true,
                    )
                }
                Some(feedback) => (
                    fail(&format!("keeping plan mode: {feedback}")),
                    false,
                ),
            }
        } else if name == agents::goal::CREATE_GOAL_NAME {
            let objective = args
                .get("objective")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let Some(objective) = objective else {
                return (
                    fail("missing `objective` — say what the goal is, concretely"),
                    false,
                );
            };
            if !agents::goal::human_turn(source) {
                return (
                    fail(
                        "only the user can set a goal — an automatic round cannot \
                         give itself a new objective",
                    ),
                    false,
                );
            }
            if let Some(existing) = self.goal(session_id).await {
                if !existing.phase.is_terminal() {
                    return (
                        fail(&format!(
                            "this session already has a goal — finish or block it \
                             first:\n{}",
                            existing.summary()
                        )),
                        false,
                    );
                }
            }
            let max_rounds = agents::goal::parse_max_rounds(args.get("max_rounds"));
            let id = self.next_goal.fetch_add(1, Ordering::Relaxed);
            let goal = Goal::new(id, objective.to_string(), max_rounds);
            // A goal created in a human turn is armed by that same
            // instruction; nothing else ever arms one implicitly.
            self.set_armed(session_id, true).await;
            let summary = goal.summary();
            self.put_goal(sessions, session_id, goal).await;
            self.events.emit(Event::LogLine {
                level:   "INFO".into(),
                message: format!("goal set ({max_rounds} rounds max): {objective}"),
            });
            (
                agents::SkillOutcome {
                    ok: true,
                    summary: format!(
                        "{summary}\n\nA fresh round opens against this objective each \
                         time you go idle, until it is complete, blocked, paused by \
                         the user, or the round budget runs out."
                    ),
                },
                false,
            )
        } else if name == agents::goal::GET_GOAL_NAME {
            match self.goal(session_id).await {
                Some(goal) => {
                    let armed = self.armed(session_id).await;
                    (
                        agents::SkillOutcome {
                            ok: true,
                            summary: format!(
                                "{}\nrounds armed: {armed}",
                                goal.summary()
                            ),
                        },
                        false,
                    )
                }
                None => (fail("this session has no goal"), false),
            }
        } else if name == agents::goal::UPDATE_GOAL_NAME {
            let Some(goal) = self.goal(session_id).await else {
                return (fail("this session has no goal to update"), false);
            };
            let revision = args
                .get("revision")
                .and_then(|v| match v {
                    serde_json::Value::Number(n) => n.as_u64().map(|n| n as u32),
                    serde_json::Value::String(s) => s.trim().parse::<u32>().ok(),
                    _ => None,
                });
            let Some(revision) = revision else {
                return (
                    fail(&format!(
                        "missing or unreadable `revision` — the goal is at revision {}",
                        goal.revision
                    )),
                    false,
                );
            };
            let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
            let action = match agents::goal::GoalAction::parse(action) {
                Ok(a) => a,
                Err(e) => return (fail(&e), false),
            };
            let note = args.get("note").and_then(|v| v.as_str());
            // Authority first, then compare-and-set: a caller who may not
            // take this action should be told that, not handed a revision
            // error it would go on to "fix".
            if let Err(e) =
                agents::goal::authorize(action, &goal, agents::goal::human_turn(source), 0)
            {
                return (fail(&e), false);
            }
            match agents::goal::apply(&goal, revision, action, note) {
                Err(e) => (fail(&e), false),
                Ok(next) => {
                    if next.phase != protocol::GoalPhase::Active {
                        self.set_armed(session_id, false).await;
                    }
                    let summary = next.summary();
                    self.put_goal(sessions, session_id, next).await;
                    self.events.emit(Event::LogLine {
                        level:   "INFO".into(),
                        message: format!("goal {}", summary.lines().next().unwrap_or("")),
                    });
                    (agents::SkillOutcome { ok: true, summary }, false)
                }
            }
        } else {
            (fail(&format!("unknown control skill `{name}`")), false)
        }
    }

    /// Dispatch one native call sequentially: unknown skills, control
    /// skills, exclusive skills, and the over-limit synthesised errors.
    /// Returns whether the turn concludes now (`exit-plan-mode`).
    #[allow(clippy::too_many_arguments)]
    async fn run_native_one(
        &self,
        sessions: &Sessions,
        session_id: u64,
        skills: &SkillRegistry,
        call: &agents::turn::NativeToolCall,
        over_limit: bool,
        ptc: bool,
        client: &LlmClient,
        cancel: &CancellationToken,
    ) -> bool {
        let call_seq = append_event(sessions, session_id, EventKind::ToolCall {
            name: call.name.clone(),
            args_preview: format!("{} {}", call.name, call.arguments),
            expectation: String::new(),
            call_id: Some(call.id.clone()),
            args_json: Some(call.arguments.clone()),
        })
        .await
        .unwrap_or(0);
        if over_limit {
            let msg = format!("tool-hop limit ({MAX_TOOL_HOPS}) reached — call not executed");
            append_tool_result(sessions, session_id, call_seq, &call.name, Some(&call.id), false, &msg, true)
                .await;
            let args = serde_json::from_str(&call.arguments)
                .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone()));
            self.observe(sessions, session_id, &call.name, &args).await;
            return false;
        }
        // Guide §7: under PTC the model announced one data tool, so a call
        // naming any other is refused here — before the policy pipeline, and
        // resolved from the mode the request was actually built with, so a
        // preset can never announce one surface and execute another.
        if ptc && !agents::ptc::direct_callable(&call.name) {
            let msg = agents::ptc::direct_call_refused(&call.name);
            append_tool_result(sessions, session_id, call_seq, &call.name, Some(&call.id), false, &msg, true)
                .await;
            return false;
        }
        if agents::control::is_control_skill(&call.name) {
            let args: serde_json::Value = match serde_json::from_str(&call.arguments) {
                Ok(v) => v,
                Err(e) => {
                    let msg = format!("invalid JSON in tool-call arguments ({e}); raw: {}", call.arguments);
                    append_tool_result(sessions, session_id, call_seq, &call.name, Some(&call.id), false, &msg, true)
                        .await;
                    return false;
                }
            };
            let preview = format!("{} {}", call.name, call.arguments);
            let (outcome, conclude) = self
                .handle_control(sessions, &call.name, &args, &preview, "", session_id, call_seq, cancel)
                .await;
            append_tool_result(sessions, session_id, call_seq, &call.name, Some(&call.id), outcome.ok, &outcome.summary, true)
                .await;
            return conclude;
        }
        let Some(skill) = skills.get(&call.name) else {
            let msg = format!("unknown skill `{}`", call.name);
            append_tool_result(sessions, session_id, call_seq, &call.name, Some(&call.id), false, &msg, true)
                .await;
            let args = serde_json::Value::String(call.arguments.clone());
            self.observe(sessions, session_id, &call.name, &args).await;
            return false;
        };
        let args: serde_json::Value = match serde_json::from_str(&call.arguments) {
            Ok(v) => v,
            Err(e) => {
                let msg = format!(
                    "invalid JSON in tool-call arguments ({e}); raw: {}",
                    call.arguments
                );
                append_tool_result(sessions, session_id, call_seq, &call.name, Some(&call.id), false, &msg, true)
                    .await;
                return false;
            }
        };
        let raw_args: Vec<String> = args
            .as_object()
            .map(|m| {
                m.values()
                    .map(|v| match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let sub = self
            .sub_agent(sessions, session_id, client, cancel.clone())
            .await
            .with_log_seq(call_seq);
        let report = sub
            .run_report(agents::ToolInvocation {
                skill: &*skill,
                args,
                raw_args,
                expectation: String::new(),
            })
            .await;
        self.after_report(sessions, session_id, call_seq, &call.name, Some(&call.id), skill.trusted(), report)
            .await;
        false
    }

    /// Shared tail for a sub-agent dispatch: approval audit, result,
    /// instruction reconciliation, repeat notices (counted in the pipeline).
    #[allow(clippy::too_many_arguments)]
    async fn after_report(
        &self,
        sessions: &Sessions,
        session_id: u64,
        call_seq: u64,
        skill_name: &str,
        tool_call_id: Option<&str>,
        trusted: bool,
        report: agents::subagent::RunReport,
    ) {
        if let Some(rec) = report.approval {
            append_event(sessions, session_id, EventKind::Approval {
                skill: rec.skill,
                args_preview: rec.args_preview,
                decision: rec.decision.to_string(),
            })
            .await;
        }
        append_tool_result(
            sessions,
            session_id,
            call_seq,
            skill_name,
            tool_call_id,
            report.outcome.ok,
            &report.outcome.summary,
            trusted,
        )
        .await;
        if report.outcome.ok && is_fs_skill(skill_name) {
            reconcile_instructions_after_fs(sessions, &self.events, session_id).await;
        }
        self.inject_notices(sessions, session_id, report.notices).await;
    }

    /// Answer native calls the batch never ran. An assistant message that
    /// carries `tool_calls` needs exactly one `tool` result per id — a
    /// dangling call poisons the next request's template — so a batch cut
    /// short (`exit-plan-mode` concluded the turn) still records a failed
    /// result for every remaining id.
    async fn answer_unrun(
        &self,
        sessions: &Sessions,
        session_id: u64,
        calls: &[agents::turn::NativeToolCall],
        reason: &str,
    ) {
        for call in calls {
            let seq = append_event(sessions, session_id, EventKind::ToolCall {
                name: call.name.clone(),
                args_preview: format!("{} {}", call.name, call.arguments),
                expectation: String::new(),
                call_id: Some(call.id.clone()),
                args_json: Some(call.arguments.clone()),
            })
            .await
            .unwrap_or(0);
            append_tool_result(
                sessions, session_id, seq, &call.name, Some(&call.id), false, reason, true,
            )
            .await;
        }
    }

    /// Dispatch a native batch (§6.2): consecutive `Parallel` calls overlap
    /// in a bounded pool (cap 4); everything else runs sequentially in
    /// model order. Returns whether the turn concludes now.
    async fn run_native_batch(
        &self,
        sessions: &Sessions,
        session_id: u64,
        skills: &SkillRegistry,
        calls: &[agents::turn::NativeToolCall],
        over_limit: bool,
        ptc: bool,
        client: &LlmClient,
        cancel: &CancellationToken,
    ) -> bool {
        let mut i = 0;
        while i < calls.len() {
            if cancel.is_cancelled() {
                break;
            }
            // Open a parallel group: consecutive parallel-eligible calls.
            let mut group: Vec<usize> = Vec::new();
            // Under PTC a batch is one `run-code` call and, at most, some
            // harness controls — nothing that overlaps — and the refusal
            // below has to see every call, so the parallel path is off.
            if !over_limit && !ptc {
                let mut j = i;
                while j < calls.len() && parallel_call(skills, &calls[j]).is_some() {
                    group.push(j);
                    j += 1;
                }
            }
            if group.len() < 2 {
                if self
                    .run_native_one(
                        sessions, session_id, skills, &calls[i], over_limit, ptc, client, cancel,
                    )
                    .await
                {
                    // The turn ends here, but the remaining ids in this
                    // batch still need results or the next request's
                    // template carries dangling `tool_calls`.
                    self.answer_unrun(
                        sessions,
                        session_id,
                        &calls[i + 1..],
                        "not executed — the turn ended when the plan was approved",
                    )
                    .await;
                    return true;
                }
                i += 1;
                continue;
            }
            // Parallel chunk: log every call first (in order), run the
            // bodies concurrently, then land the results in model order.
            for chunk in group.chunks(4) {
                if cancel.is_cancelled() {
                    break;
                }
                let mut seqs = Vec::with_capacity(chunk.len());
                for &k in chunk {
                    let call = &calls[k];
                    let seq = append_event(sessions, session_id, EventKind::ToolCall {
                        name: call.name.clone(),
                        args_preview: format!("{} {}", call.name, call.arguments),
                        expectation: String::new(),
                        call_id: Some(call.id.clone()),
                        args_json: Some(call.arguments.clone()),
                    })
                    .await
                    .unwrap_or(0);
                    seqs.push(seq);
                }
                let mut prepared = Vec::with_capacity(chunk.len());
                for &k in chunk {
                    let (skill, args) =
                        parallel_call(skills, &calls[k]).expect("group checked");
                    let raw_args: Vec<String> = args
                        .as_object()
                        .map(|m| {
                            m.values()
                                .map(|v| match v {
                                    serde_json::Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    prepared.push((skill, args, raw_args));
                }
                // One sub-agent per call. They share the session's policy
                // instances (lock-guarded) and broker, so asks serialize
                // while read-only bodies overlap.
                let mut subs = Vec::with_capacity(chunk.len());
                for &seq in &seqs {
                    subs.push(
                        self.sub_agent(sessions, session_id, client, cancel.clone())
                            .await
                            .with_log_seq(seq),
                    );
                }
                let futs: Vec<_> = prepared
                    .into_iter()
                    .zip(subs)
                    .map(|((skill, args, raw_args), sub)| async move {
                        let report = sub
                            .run_report(agents::ToolInvocation {
                                skill: &*skill,
                                args,
                                raw_args,
                                expectation: String::new(),
                            })
                            .await;
                        (skill, report)
                    })
                    .collect();
                let reports = futures::future::join_all(futs).await;
                for (((skill, report), &k), &seq) in
                    reports.into_iter().zip(chunk.iter()).zip(seqs.iter())
                {
                    let call = &calls[k];
                    self.after_report(
                        sessions,
                        session_id,
                        seq,
                        &call.name,
                        Some(&call.id),
                        skill.trusted(),
                        report,
                    )
                    .await;
                }
            }
            i += group.len();
        }
        false
    }
}

/// Fallback prompt window when neither the user nor the server reports one.
pub const DEFAULT_CONTEXT_WINDOW: u32 = 24_000;

impl ChatHub {
    pub fn new(
        out_tx: mpsc::UnboundedSender<Frame>,
        skills: Arc<SkillRegistry>,
        failure_sink: Option<Arc<dyn ToolFailureSink>>,
    ) -> Self {
        let sink: Arc<dyn EventSink> = Arc::new(OutSink { tx: out_tx.clone() });
        Self {
            sessions:     Arc::new(Mutex::new(HashMap::new())),
            next_id:      Arc::new(AtomicU64::new(1)),
            next_turn:    Arc::new(AtomicU64::new(1)),
            llm:          Arc::new(Mutex::new(None)),
            llm_state:    Arc::new(Mutex::new(LlmState::Disconnected)),
            out_tx,
            event_sink:   sink,
            skills,
            failure_sink,
            active_turns: Arc::new(Mutex::new(HashMap::new())),
            next_marker:  Arc::new(AtomicU64::new(1)),
            llm_opts:     Arc::new(Mutex::new(LlmOptions::default())),
            context_window: Arc::new(AtomicU32::new(DEFAULT_CONTEXT_WINDOW)),
            repeat:       Arc::new(Mutex::new(HashMap::new())),
            read_seen:    Arc::new(Mutex::new(HashMap::new())),
            permissions:  Arc::new(Mutex::new(HashMap::new())),
            plans:        Arc::new(Mutex::new(HashMap::new())),
            presets:      Arc::new(Mutex::new(HashMap::new())),
            brokers:      Arc::new(BrokerSet::new()),
            meters:       Arc::new(Mutex::new(HashMap::new())),
            inbox:        Arc::new(Inbox::new()),
            jobs:         Arc::new(agents::JobRegistry::new()),
            goals:        Arc::new(Mutex::new(HashMap::new())),
            goal_armed:   Arc::new(Mutex::new(HashSet::new())),
            next_goal_id: Arc::new(AtomicU64::new(1)),
            hooks:        Arc::new(hooks::HookConfig::default()),
        }
    }

    /// Build a hub pre-populated with every session it can find on disk
    /// (migrating legacy TOML files on the way). `next_id` is advanced past
    /// the largest existing id so newly minted sessions never collide with
    /// restored ones. Wave-3 control state (permission mode, plan flag) is
    /// restored from each log's latest durable event.
    pub fn new_loaded(
        out_tx: mpsc::UnboundedSender<Frame>,
        skills: Arc<SkillRegistry>,
        failure_sink: Option<Arc<dyn ToolFailureSink>>,
    ) -> Self {
        let hub = Self::new(out_tx, skills, failure_sink);
        let loaded = sessions_store::load_all();
        let max_id = loaded.iter().map(|s| s.id).max().unwrap_or(0);
        {
            let map = hub.sessions.clone();
            let mut g = map.try_lock().expect("fresh ChatHub, no contention");
            let mut perms = hub.permissions.try_lock().expect("fresh ChatHub");
            let mut plans = hub.plans.try_lock().expect("fresh ChatHub");
            let mut presets = hub.presets.try_lock().expect("fresh ChatHub");
            let mut goals = hub.goals.try_lock().expect("fresh ChatHub");
            let mut max_goal = 0;
            for s in loaded {
                let (mode, plan, preset, goal) = control_state(&s);
                if mode != PermissionMode::default() {
                    perms.insert(s.id, mode);
                }
                if plan {
                    plans.insert(s.id, true);
                }
                if let Some(name) = preset {
                    presets.insert(s.id, name);
                }
                // Restored *disarmed*: `goal_armed` stays empty at startup,
                // so an active goal resumes only when a human says so.
                if let Some(goal) = goal {
                    max_goal = max_goal.max(goal.id);
                    goals.insert(s.id, goal);
                }
                g.insert(s.id, s);
            }
            hub.next_goal_id.store(max_goal + 1, Ordering::Relaxed);
        }
        hub.next_id.store(max_id + 1, Ordering::Relaxed);
        hub
    }

    /// The per-session control plane the turn task and the harness
    /// commands share. One constructor so a new field cannot be wired into
    /// one path and forgotten in the other.
    fn control(&self) -> ControlState {
        ControlState {
            events:       self.event_sink.clone(),
            failure_sink: self.failure_sink.clone(),
            permissions:  self.permissions.clone(),
            plans:        self.plans.clone(),
            repeat:       self.repeat.clone(),
            read_seen:    self.read_seen.clone(),
            brokers:      self.brokers.clone(),
            goals:        self.goals.clone(),
            arm_set:      self.goal_armed.clone(),
            next_goal:    self.next_goal_id.clone(),
            hooks:        self.hooks.clone(),
            // Harness commands are the user acting directly; a turn task
            // overrides this with its own source.
            turn_source:  TurnSource::Human,
        }
    }

    /// Adopt the job registry the skills were built with. Without this the
    /// hub would hold an empty registry of its own and never see the jobs
    /// the shell skills actually start.
    /// Adopt the hook configuration `main.rs` loaded and reported.
    pub fn with_hooks(mut self, hooks: Arc<hooks::HookConfig>) -> Self {
        self.hooks = hooks;
        self
    }

    pub fn with_jobs(mut self, jobs: Arc<agents::JobRegistry>) -> Self {
        self.jobs = jobs;
        self
    }

    pub async fn list_sessions(&self) -> Vec<SessionMeta> {
        let g = self.sessions.lock().await;
        let mut out: Vec<SessionMeta> = g
            .values()
            .filter(|s| !s.archived())
            .map(|s| SessionMeta {
                id: s.id,
                title: s.title(),
                created_at: s.created_at(),
                updated_at: s.updated_at(),
            })
            .collect();
        out.sort_by_key(|s| s.created_at);
        out
    }

    /// The wire dump the FE rebuilds a transcript from. Tool-role entries
    /// carry the skill name / outcome recovered from their `ToolCall`
    /// event so chips survive a reload, and their `content` is the raw
    /// outcome text (what the live chip showed), not the fenced block the
    /// model reads. Injected context goes out under the `context` role so
    /// the FE never mistakes it for something the user typed.
    /// One page of a session's **raw** event log — the Trajectory view's
    /// ledger (UI guide §10). Deliberately not part of `dump_session`: the
    /// transcript wants the derived surface, and a long log would otherwise
    /// ride along on every session switch.
    pub async fn dump_events(
        &self,
        id: u64,
        from_seq: u64,
        limit: u32,
    ) -> Option<(
        Vec<protocol::EventDump>,
        Vec<protocol::EnvelopeDump>,
        u32,
        Option<u64>,
    )> {
        let g = self.sessions.lock().await;
        let log = g.get(&id)?;
        Some(crate::trajectory::page(log, from_seq, limit))
    }

    /// Fold this session's log into the projections a client reads (guide
    /// §3.3). Pure over the log — no cache, because at our session sizes
    /// the fold is cheaper than the staleness question a cache would raise.
    pub async fn session_stats(
        &self,
        id: u64,
    ) -> Option<(protocol::StatsDump, Vec<protocol::TurnRowDump>, u64)> {
        use sica_core::project::{Projection, SessionStats, TurnOutline};

        let g = self.sessions.lock().await;
        let log = g.get(&id)?;
        let events = &log.events;
        let s = SessionStats::fold(events);
        let outline = TurnOutline::fold(events);
        let rows = outline
            .turns
            .into_iter()
            .map(|t| protocol::TurnRowDump {
                turn_id:         t.turn_id,
                first_user_line: t.first_user_line,
                source:          t.source,
                hops:            t.hops,
                finish_reason:   t.finish_reason,
                start_seq:       t.start_seq,
                ts_start:        t.ts_start,
                ts_end:          t.ts_end,
                tool_calls:      t.tool_calls,
            })
            .collect();
        Some((
            protocol::StatsDump {
                user_msgs:      s.user_msgs,
                assistant_msgs: s.assistant_msgs,
                tool_calls:     s.tool_calls,
                tool_failures:  s.tool_failures,
                retries:        s.retries,
                turns:          s.turns,
                wall_ms:        s.wall_ms,
            },
            rows,
            s.through_seq,
        ))
    }

    pub async fn dump_session(&self, id: u64) -> Option<SessionDump> {
        // Loading a session is the FE switching to it, so push its goal, the
        // jobs it owns and its queue: these are otherwise only emitted on a
        // change, and a session with a build already running — or a message
        // still waiting behind a turn — would show an empty strip.
        // `QueueChanged` alone here: nothing was *accepted*, so pairing it
        // with an `InboxChanged` would put "message loaded" in the log every
        // time the user clicks a session.
        self.event_sink.emit(Event::QueueChanged {
            session_id: id,
            rows:       self.inbox.rows(id).await,
        });
        self.event_sink.emit(Event::JobsChanged {
            session_id: id,
            jobs: self
                .jobs
                .list(id)
                .into_iter()
                .map(|j| protocol::JobDump {
                    id:      j.id,
                    kind:    j.kind,
                    command: j.command,
                    status:  j.status.label(),
                    running: j.status.is_running(),
                    unread:  j.unread,
                })
                .collect(),
        });
        {
            let goal = self.goals.lock().await.get(&id).cloned();
            let armed = self.goal_armed.lock().await.contains(&id);
            self.event_sink.emit(Event::GoalChanged {
                session_id: id,
                goal: goal.as_ref().map(|g| goal_dump(g, armed)),
            });
        }
        let g = self.sessions.lock().await;
        let log = g.get(&id)?;
        let messages = log
            .derive_surface()
            .into_iter()
            .map(|e| {
                let role = if e.context.is_some() {
                    "context"
                } else {
                    role_to_str(e.message.role)
                };
                let tool = e.tool;
                let content = match &tool {
                    Some(t) => t.summary.clone(),
                    None => e.message.content,
                };
                MessageDump {
                    seq: e.seq,
                    role: role.into(),
                    content,
                    reasoning: e.message.reasoning,
                    images: e.message.images,
                    tool_name: tool.as_ref().map(|t| t.name.clone()),
                    tool_ok: tool.as_ref().map(|t| t.ok),
                    tool_args_preview: tool.as_ref().map(|t| t.args_preview.clone()),
                    tool_expectation: tool.as_ref().map(|t| t.expectation.clone()),
                    // The `ToolCall` seq is the only identity that survives a
                    // restart; nested calls never reach the log, so the
                    // rebuilt rows are flat by construction.
                    tool_call_id: tool.as_ref().map(|t| t.call_seq),
                    tool_parent_id: None,
                    tool_depth: 0,
                    tool_args_json: tool.as_ref().and_then(|t| t.args_json.clone()),
                    context_source: e.context.as_ref().map(|c| c.label()),
                }
            })
            .collect();
        let permission_mode =
            self.permissions.lock().await.get(&id).copied().unwrap_or_default();
        let plan_active = self.plans.lock().await.get(&id).copied().unwrap_or(false);
        let agent = self.presets.lock().await.get(&id).cloned();
        let todos = log
            .events
            .iter()
            .filter_map(|ev| match &ev.kind {
                EventKind::TodoWrite { items } => Some(items.clone()),
                _ => None,
            })
            .last()
            .unwrap_or_default();
        Some(SessionDump {
            id: log.id,
            title: log.title(),
            created_at: log.created_at(),
            messages,
            permission_mode,
            plan_active,
            todos,
            agent,
        })
    }

    /// Mint a session in memory only. It reaches disk with its first user
    /// message, so an unused "new session" leaves no file behind.
    pub async fn create_session(&self) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let s = SessionLog::new(id, default_title(id));
        self.sessions.lock().await.insert(id, s);
        self.run_session_start_hooks(id).await;
        id
    }

    /// `SessionStart` hooks (guide §13.1). Only their `additionalContext`
    /// matters here — there is no call to deny yet, so a hook that returns
    /// a decision at session creation has nothing to decide about, and the
    /// merged rank is deliberately ignored.
    async fn run_session_start_hooks(&self, id: u64) {
        if self.hooks.for_event(hooks::HookEvent::SessionStart).is_empty() {
            return;
        }
        let merged = hooks::run_event(
            &self.hooks,
            &self.sessions,
            &self.event_sink,
            hooks::HookEvent::SessionStart,
            id,
            None,
        )
        .await;
        self.inject_hook_context(id, merged.context).await;
    }

    /// A hook's `additionalContext` becomes a model-visible message. Source
    /// `Injected`: it is the operator's own script handing the model a
    /// fact, which is exactly what that source means.
    async fn inject_hook_context(&self, session_id: u64, context: Vec<String>) {
        for text in context {
            if text.trim().is_empty() {
                continue;
            }
            append_event(&self.sessions, session_id, EventKind::ContextInjected {
                surface: SurfaceOp::Append,
                source:  ContextSource::Injected,
                content: text,
            })
            .await;
        }
    }

    pub async fn delete_session(&self, id: u64) -> bool {
        let removed = self.sessions.lock().await.remove(&id).is_some();
        if removed {
            sessions_store::delete(id);
            self.inbox.clear(id).await;
            // Its background jobs go with it, process trees included.
            self.jobs.clear(id);
            self.goals.lock().await.remove(&id);
            self.goal_armed.lock().await.remove(&id);
        }
        removed
    }

    /// Give a session a title of the user's own (§4.2). The auto-titler
    /// only writes over the title it wrote itself, so this pins it without a
    /// separate flag.
    pub async fn rename_session(&self, id: u64, title: &str) -> bool {
        let title = title.trim();
        if title.is_empty() {
            return false;
        }
        let mut g = self.sessions.lock().await;
        let Some(log) = g.get_mut(&id) else { return false };
        log.append(EventKind::SessionTitle { title: title.to_string() });
        if let Err(e) = sessions_store::flush(log) {
            warn!(error = %e, session_id = id, "flush session (after rename) failed");
        }
        drop(g);
        self.event_sink.emit(Event::SessionTitleChanged {
            session_id: id,
            title: title.to_string(),
        });
        true
    }

    /// Copy a session's completed turns into a fresh one. Cut at the last
    /// `TurnEnd`, so a turn still in flight never crosses — its tool results
    /// have not landed, and half a turn is not a conversation to resume.
    pub async fn fork_session(&self, id: u64) -> Option<u64> {
        let new_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut g = self.sessions.lock().await;
        let src = g.get(&id)?;
        let cut = src
            .events
            .iter()
            .rposition(|e| matches!(e.kind, EventKind::TurnEnd { .. }))?;
        let title = format!("{} (fork)", src.title());
        let mut forked = SessionLog::fork(new_id, title, &src.events, cut);
        if let Err(e) = sessions_store::flush(&mut forked) {
            warn!(error = %e, session_id = new_id, "flush session (fork) failed");
        }
        g.insert(new_id, forked);
        Some(new_id)
    }

    /// Hide a session without losing it. Unlike `delete_session` the log
    /// stays on disk and the session stays loadable by id.
    pub async fn archive_session(&self, id: u64) -> bool {
        let mut g = self.sessions.lock().await;
        let Some(log) = g.get_mut(&id) else { return false };
        if log.archived() {
            return true;
        }
        log.append(EventKind::SessionArchived);
        if let Err(e) = sessions_store::flush(log) {
            warn!(error = %e, session_id = id, "flush session (archive) failed");
        }
        true
    }

    /// Case-insensitive substring search over what each session actually
    /// said — the derived surface, so injected snapshots and fenced tool
    /// blocks do not flood the results. Newest first, capped.
    pub async fn search_sessions(&self, query: &str) -> Vec<protocol::SessionHit> {
        const MAX_HITS: usize = 20;
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        let g = self.sessions.lock().await;
        let mut hits: Vec<protocol::SessionHit> = g
            .values()
            .filter(|log| !log.archived())
            .filter_map(|log| {
                let title = log.title();
                let snippet = if title.to_lowercase().contains(&needle) {
                    String::new()
                } else {
                    log.derive_surface()
                        .iter()
                        .filter(|e| e.tool.is_none() && e.context.is_none())
                        .flat_map(|e| {
                            e.message
                                .content
                                .lines()
                                .map(str::to_string)
                                .collect::<Vec<_>>()
                        })
                        .find(|line| line.to_lowercase().contains(&needle))?
                };
                Some(protocol::SessionHit {
                    id: log.id,
                    title,
                    snippet: sica_core::retain::utf8_head(snippet.trim(), 160).to_string(),
                    updated_at: log.updated_at(),
                })
            })
            .collect();
        hits.sort_by_key(|h| std::cmp::Reverse(h.updated_at));
        hits.truncate(MAX_HITS);
        hits
    }

    /// Fetch a provider's model list off the dispatcher loop and report it
    /// as an event. A provider that is slow to answer must not stall every
    /// other request behind it.
    pub fn spawn_list_models(&self, base_url: String, api_key: Option<String>) {
        let sink = self.event_sink.clone();
        tokio::spawn(async move {
            let client = LlmClient::new(base_url.clone(), String::new(), api_key);
            let (models, error) = match client.list_models().await {
                Ok(models) => (models, None),
                Err(e) => (Vec::new(), Some(e.to_string())),
            };
            sink.emit(Event::ModelsListed { base_url, models, error });
        });
    }

    pub async fn set_llm_state(&self, st: LlmState) {
        *self.llm_state.lock().await = st.clone();
        let _ = self
            .out_tx
            .send(Frame::event(Event::LlmStateChanged { state: st }));
    }

    pub async fn connect_llm(
        &self,
        base_url: String,
        model: String,
        api_key: Option<String>,
        options: LlmOptions,
    ) {
        self.set_llm_state(LlmState::Connecting).await;
        // Push a visible log line so the FE log panel reflects what's happening
        // — the dot transition can be subtle on first run.
        self.event_sink.emit(Event::LogLine {
            level: "INFO".into(),
            message: format!("LLM: connecting to {base_url} (model={model})"),
        });
        let mut client = LlmClient::new(base_url.clone(), model.clone(), api_key);
        client.temperature = options.temperature;
        client.max_tokens = options.max_tokens;
        client.thinking = options.thinking;
        match client.health().await {
            Ok(()) => {
                // Prompt window: explicit setting wins; otherwise ask the
                // server (llama.cpp `/props` `n_ctx` — the launched
                // `--ctx-size` — then vLLM `max_model_len` / `n_ctx_train`).
                let window = match options.context_window {
                    Some(w) if w > 0 => w,
                    _ => client
                        .detect_context_window()
                        .await
                        .unwrap_or(DEFAULT_CONTEXT_WINDOW),
                };
                self.context_window.store(window, Ordering::Relaxed);
                *self.llm_opts.lock().await = options.clone();
                *self.llm.lock().await = Some(client);
                // A new provider invalidates every usage anchor.
                self.meters.lock().await.clear();
                self.set_llm_state(LlmState::Ready {
                    model: model.clone(),
                    context_window: window,
                })
                .await;
                self.event_sink.emit(Event::LogLine {
                    level: "INFO".into(),
                    message: format!(
                        "LLM: ready ({base_url}, model={model}, ctx={window}, \
                         temp={}, max_tokens={}, tools={}, thinking={})",
                        options.temperature,
                        options
                            .max_tokens
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "server-default".into()),
                        options.tool_mode.label(),
                        options.thinking,
                    ),
                });
            }
            Err(e) => {
                let msg = format!("{e}");
                self.set_llm_state(LlmState::Error { message: msg.clone() }).await;
                warn!(error = %e, "LLM connect failed");
                self.event_sink.emit(Event::LogLine {
                    level: "ERROR".into(),
                    message: format!("LLM: connect failed — {msg}"),
                });
            }
        }
    }

    /// Spawn `connect_llm` on the runtime so the dispatcher returns to the
    /// caller immediately instead of stalling for the full HTTP round-trip.
    pub fn spawn_connect_llm(
        &self,
        base_url: String,
        model: String,
        api_key: Option<String>,
        options: LlmOptions,
    ) {
        let this = self.clone();
        tokio::spawn(async move {
            this.connect_llm(base_url, model, api_key, options).await;
        });
    }

    /// Install a keyless replay client (guide §14.1) and report Ready.
    ///
    /// Skips the health check and the context-window probe: there is no
    /// server to ask. The window is passed in so a scenario can force a
    /// small one and make compaction fire without a giant recording.
    pub async fn connect_replay(
        &self,
        script: Arc<llm::replay::ReplayScript>,
        window: u32,
        options: LlmOptions,
    ) {
        let model = "replay".to_string();
        let mut client = LlmClient::new("replay://recorded", &model, None);
        client.temperature = options.temperature;
        client.max_tokens = options.max_tokens;
        client.thinking = options.thinking;
        let calls = script.total();
        let client = client.with_replay(script);
        self.context_window.store(window, Ordering::Relaxed);
        *self.llm_opts.lock().await = options;
        *self.llm.lock().await = Some(client);
        self.meters.lock().await.clear();
        self.set_llm_state(LlmState::Ready {
            model: model.clone(),
            context_window: window,
        })
        .await;
        self.event_sink.emit(Event::LogLine {
            level:   "INFO".into(),
            message: format!("LLM: replay mode — {calls} recorded call(s), ctx={window}"),
        });
    }

    pub async fn disconnect_llm(&self) {
        *self.llm.lock().await = None;
        self.meters.lock().await.clear();
        self.set_llm_state(LlmState::Disconnected).await;
    }

    /// Cancel the in-flight turn (if any) for `session_id`. Idempotent.
    pub async fn interrupt_session(&self, session_id: u64) {
        if let Some((_, tok)) = self.active_turns.lock().await.get(&session_id) {
            tok.cancel();
        }
        // Stop also stops the goal loop. Anything else makes the button a
        // lie: the round the user just killed would be followed by the next
        // one a moment later.
        self.goal_armed.lock().await.remove(&session_id);
        // Steers and injects aimed at the turn being killed die with it —
        // applying them to some later, unrelated turn would be worse than
        // dropping them. Queued user messages survive: pressing Stop right
        // after sending one is how a user says "do this instead".
        self.inbox.drain_mid_turn(session_id).await;
    }

    async fn session_exists(&self, session_id: u64) -> bool {
        self.sessions.lock().await.contains_key(&session_id)
    }

    /// Switch a session's permission mode. Durable (`PermissionMode` event)
    /// and pushed (`PermissionModeChanged`); the model learns it from the
    /// runtime-context line on the next hop. No-op when unchanged.
    pub async fn set_permission_mode(&self, session_id: u64, mode: PermissionMode) -> bool {
        if !self.session_exists(session_id).await {
            return false;
        }
        let changed = {
            let mut g = self.permissions.lock().await;
            if g.get(&session_id).copied().unwrap_or_default() == mode {
                false
            } else {
                g.insert(session_id, mode);
                true
            }
        };
        if !changed {
            return false;
        }
        append_event(&self.sessions, session_id, EventKind::PermissionMode { mode }).await;
        self.event_sink.emit(Event::PermissionModeChanged { session_id, mode });
        self.event_sink.emit(Event::LogLine {
            level: "INFO".into(),
            message: format!("permission mode → {} — {}", mode.label(), mode.description()),
        });
        true
    }

    /// Enter or leave plan mode. Durable (`PlanMode` event) and pushed
    /// (`PlanModeChanged`); the prompt gains/loses the `PLAN_POLICY`
    /// section on the next hop. Leaving here is the user's direct action —
    /// no review; the model leaves through `exit-plan-mode` instead.
    pub async fn set_plan_mode(&self, session_id: u64, active: bool) -> bool {
        if !self.session_exists(session_id).await {
            return false;
        }
        let changed = {
            let mut g = self.plans.lock().await;
            if g.get(&session_id).copied().unwrap_or(false) == active {
                false
            } else {
                g.insert(session_id, active);
                true
            }
        };
        if !changed {
            return false;
        }
        append_event(&self.sessions, session_id, EventKind::PlanMode { active }).await;
        self.event_sink.emit(Event::PlanModeChanged { session_id, active });
        self.event_sink.emit(Event::LogLine {
            level: "INFO".into(),
            message: if active {
                "plan mode on — only non-mutating tools run; finish with exit-plan-mode".into()
            } else {
                "plan mode off".into()
            },
        });
        true
    }

    /// Select (`Some`) or clear (`None`) the session's agent preset —
    /// `agents/<name>.md`, guide §5.2. Durable (`AgentPreset` event) and
    /// pushed (`SessionAgentChanged`); the persona section and the
    /// restricted registry view take effect on the next turn.
    ///
    /// Refused once the session has produced a model message, per dsh: the
    /// persona sits in the system-prompt prefix, so swapping it mid-session
    /// both discards the provider's cache and leaves the earlier half of
    /// the transcript answering to rules that are no longer in force.
    /// `Ok(text)` describes what happened; `Err(text)` says why not.
    pub async fn set_session_agent(
        &self,
        session_id: u64,
        name: Option<String>,
    ) -> Result<String, String> {
        self.set_session_agent_in(&sica_core::paths::agents_dir(), session_id, name).await
    }

    /// [`set_session_agent`](Self::set_session_agent) against an explicit
    /// preset directory. The directory is a parameter for the same reason
    /// `catalog::build` takes its three: it makes the rule testable without
    /// a process-global workspace override.
    async fn set_session_agent_in(
        &self,
        dir: &std::path::Path,
        session_id: u64,
        name: Option<String>,
    ) -> Result<String, String> {
        if !self.session_exists(session_id).await {
            return Err(format!("unknown session {session_id}"));
        }
        // Validate before touching any state: an unreadable preset must
        // leave the session exactly as it was.
        let preset = match &name {
            Some(n) => Some(agents::preset::load(dir, n)?),
            None => None,
        };
        let current = self.presets.lock().await.get(&session_id).cloned();
        if current == name {
            return Ok(match &name {
                Some(n) => format!("already running agent `{n}`"),
                None => "no agent selected".into(),
            });
        }
        if self.session_has_run(session_id).await {
            return Err(
                "the agent is fixed once a session has produced a reply — start a new                  session to run a different one"
                    .into(),
            );
        }
        {
            let mut g = self.presets.lock().await;
            match &name {
                Some(n) => g.insert(session_id, n.clone()),
                None => g.remove(&session_id),
            };
        }
        append_event(&self.sessions, session_id, EventKind::AgentPreset { name: name.clone() })
            .await;
        self.event_sink.emit(Event::SessionAgentChanged { session_id, name: name.clone() });
        let message = match (&name, &preset) {
            (Some(n), Some(p)) => {
                // Unknown skill names are the user's typo to fix, not a
                // reason to refuse the preset — say so once, here, rather
                // than silently narrowing the registry on every turn.
                let unknown = agents::preset::unknown_skills(&self.skills, p);
                if !unknown.is_empty() {
                    self.event_sink.emit(Event::LogLine {
                        level:   "WARN".into(),
                        message: format!(
                            "agent `{n}` lists unknown skill(s): {}",
                            unknown.join(", ")
                        ),
                    });
                }
                if p.skills.is_empty() {
                    format!("agent → {n} — every skill stays available")
                } else {
                    format!("agent → {n} — skills: {}", p.skills.join(", "))
                }
            }
            _ => "agent cleared".to_string(),
        };
        self.event_sink.emit(Event::LogLine { level: "INFO".into(), message: message.clone() });
        Ok(message)
    }

    /// Whether the session has produced a model message yet. What fixes the
    /// agent selection: a session that has only been *created*, or that
    /// holds an unsent draft, is still free to choose one.
    async fn session_has_run(&self, session_id: u64) -> bool {
        let g = self.sessions.lock().await;
        let Some(log) = g.get(&session_id) else { return false };
        log.events
            .iter()
            .any(|ev| matches!(ev.kind, EventKind::AssistantMessage { .. }))
    }

    /// The skill registry and persona a turn on `session_id` runs with.
    /// Resolved once per turn: both halves come from the same preset, so
    /// the prompt can never advertise a skill the dispatcher would refuse.
    ///
    /// A preset that has since been deleted or broken degrades to the
    /// unrestricted default with a loud `LogLine` — failing every turn of
    /// an existing session because a file was renamed would be worse.
    async fn effective_agent(&self, session_id: u64) -> (Arc<SkillRegistry>, Option<String>) {
        self.effective_agent_in(&sica_core::paths::agents_dir(), session_id).await
    }

    /// [`effective_agent`](Self::effective_agent) against an explicit preset
    /// directory — see [`set_session_agent_in`](Self::set_session_agent_in).
    async fn effective_agent_in(
        &self,
        dir: &std::path::Path,
        session_id: u64,
    ) -> (Arc<SkillRegistry>, Option<String>) {
        let Some(name) = self.presets.lock().await.get(&session_id).cloned() else {
            return (self.skills.clone(), None);
        };
        match agents::preset::load(dir, &name) {
            Ok(p) => {
                let view = agents::preset::view(&self.skills, &p);
                (Arc::new(view), Some(p.persona))
            }
            Err(e) => {
                self.event_sink.emit(Event::LogLine {
                    level:   "ERROR".into(),
                    message: format!("agent `{name}` could not be loaded ({e}) — running without it"),
                });
                (self.skills.clone(), None)
            }
        }
    }

    /// Deliver a pipeline approval verdict. Returns whether the id was
    /// still pending — a late answer reports `false`.
    pub async fn resolve_approval(&self, id: u64, allow: bool) -> bool {
        self.brokers.resolve_approval(id, allow).await
    }

    /// Deliver a human answer to an `ask-user` / plan-review question.
    pub async fn answer_question(&self, id: u64, answer: String) -> bool {
        self.brokers.answer_question(id, answer).await
    }

    /// Run a harness command that never creates a model message. Always
    /// logged as a `Command` event (durable audit, never surfaced) and
    /// answered with text for the log panel.
    pub async fn run_command(&self, session_id: u64, name: &str, input: &str) -> String {
        let (ok, text) = if !self.session_exists(session_id).await {
            (false, format!("unknown session {session_id}"))
        } else {
            match name {
                "compact" => self.command_compact(session_id).await,
                "plan" => self.command_plan(session_id, input).await,
                "permission" => self.command_permission(session_id, input).await,
                "job-kill" => self.command_job_kill(session_id, input).await,
                "goal" => self.command_goal(session_id, input).await,
                "agent" => self.command_agent(session_id, input).await,
                _ => (
                    false,
                    "unknown command — want compact | plan | permission | job-kill | goal                      | agent"
                        .into(),
                ),
            }
        };
        if self.session_exists(session_id).await {
            append_event(
                &self.sessions,
                session_id,
                EventKind::Command { name: name.into(), input: input.into(), ok },
            )
            .await;
        }
        text
    }

    /// `/goal` — read the objective; `/goal continue | pause | complete |
    /// block <note> | edit <text>` — change it, on the user's own authority.
    ///
    /// `continue` is the only way an autonomous loop ever (re)starts:
    /// arming is process-local, so a restored goal, a fork, or a session
    /// the user pressed Stop on all wait here until a person asks.
    async fn command_goal(&self, session_id: u64, input: &str) -> (bool, String) {
        let control = self.control();
        let Some(goal) = control.goal(session_id).await else {
            return (
                false,
                "this session has no goal — the agent sets one with `create-goal`".into(),
            );
        };
        let armed = control.armed(session_id).await;
        let (word, note) = match input.trim().split_once(char::is_whitespace) {
            Some((w, rest)) => (w.trim(), rest.trim()),
            None => (input.trim(), ""),
        };
        if word.is_empty() {
            return (true, format!("{}\nrounds armed: {armed}", goal.summary()));
        }
        // `edit` is not a phase change, so it does not go through `apply`:
        // it rewords the objective and leaves the phase, the rounds spent
        // and the arming exactly as they were.
        if word.eq_ignore_ascii_case("edit") {
            let next = match agents::goal::edit(&goal, goal.revision, note) {
                Ok(g) => g,
                Err(e) => return (false, e),
            };
            let summary = next.summary();
            control.put_goal(&self.sessions, session_id, next).await;
            return (true, summary);
        }
        let action = match agents::goal::GoalAction::parse(word) {
            Ok(a) => a,
            Err(e) => return (false, e),
        };
        // The user always has authority; only the compare-and-set applies,
        // and it cannot fail here because the revision comes from the goal
        // we just read.
        let next = match agents::goal::apply(&goal, goal.revision, action, Some(note)) {
            Ok(g) => g,
            Err(e) => return (false, e),
        };
        let resumed = next.phase == protocol::GoalPhase::Active && next.rounds_left();
        control.set_armed(session_id, resumed).await;
        let summary = next.summary();
        control.put_goal(&self.sessions, session_id, next).await;

        // Resuming while the session is idle starts the next round now —
        // otherwise "continue" would only take effect after the user sent
        // an unrelated message.
        if resumed && !self.active_turns.lock().await.contains_key(&session_id) {
            if let Some(goal) = control.goal(session_id).await {
                let started = agents::goal::start_round(&goal);
                let prompt = agents::goal::round_prompt(&started);
                control.put_goal(&self.sessions, session_id, started).await;
                tokio::spawn(self.start_boxed(
                    session_id,
                    prompt,
                    Vec::new(),
                    TurnSource::GoalRound,
                ));
            }
        }
        (true, summary)
    }

    /// Stop a background job from the UI. Routed through the same registry
    /// the `job-kill` skill uses, so the model is told the job ended by the
    /// usual completion notice rather than finding it gone.
    async fn command_job_kill(&self, session_id: u64, input: &str) -> (bool, String) {
        let id = input.trim();
        if id.is_empty() {
            return (false, "usage: /job-kill <job id>".into());
        }
        match self.jobs.kill(session_id, id) {
            Ok(()) => (true, format!("asked job {id} to stop")),
            Err(e) => (false, e),
        }
    }

    /// Manual compaction: fold older history into a summary right now
    /// instead of waiting for the threshold. Force semantics — the
    /// threshold check is skipped; the pruner still runs first and can
    /// clear the pressure on its own.
    async fn command_compact(&self, session_id: u64) -> (bool, String) {
        let Some(client) = self.llm.lock().await.clone() else {
            return (false, "no LLM connected — compaction needs the summariser".into());
        };
        let (tool_mode, opt_max_tokens, compact_policy) = {
            let opts = self.llm_opts.lock().await;
            (opts.tool_mode, opts.max_tokens, opts.compact)
        };
        let window = self.context_window.load(Ordering::Relaxed);
        let reserve = opt_max_tokens.unwrap_or(4096).saturating_add(512);
        let budget = window.saturating_sub(reserve).max(1024);
        let model = client.model.clone();
        let plan_policy = if self.plans.lock().await.get(&session_id).copied().unwrap_or(false) {
            Some(plan_policy_text())
        } else {
            None
        };
        let (skills, persona) = self.effective_agent(session_id).await;
        let wh = match build_history(
            &self.sessions, session_id, &skills, tool_mode, &model, plan_policy,
            persona.as_deref(),
        ).await {
            Ok(Some(wh)) => wh,
            Ok(None) => return (false, format!("session {session_id} vanished")),
            Err(e) => return (false, format!("prompt assembly failed: {e}")),
        };
        let ok = compact_session(
            &self.sessions,
            session_id,
            &client,
            &self.event_sink,
            budget,
            &compact_policy,
            &wh,
            tool_mode.native(),
            &CancellationToken::new(),
        )
        .await;
        (
            ok,
            if ok {
                "compaction finished — older history folded into a summary".into()
            } else {
                "compaction made no progress — history left untouched".into()
            },
        )
    }

    /// `/plan [on|off]` — bare `/plan` toggles.
    async fn command_plan(&self, session_id: u64, input: &str) -> (bool, String) {
        let active = match input.trim().to_lowercase().as_str() {
            "on" | "enter" | "start" => true,
            "off" | "leave" | "exit" | "stop" => false,
            _ => !self.plans.lock().await.get(&session_id).copied().unwrap_or(false),
        };
        self.set_plan_mode(session_id, active).await;
        (
            true,
            if active {
                "plan mode on — explore with read-only tools, finish with exit-plan-mode".into()
            } else {
                "plan mode off".into()
            },
        )
    }

    /// `/permission <mode>`.
    async fn command_permission(&self, session_id: u64, input: &str) -> (bool, String) {
        match PermissionMode::parse(input) {
            Some(mode) => {
                self.set_permission_mode(session_id, mode).await;
                (true, format!("permission mode → {} — {}", mode.label(), mode.description()))
            }
            None => (
                false,
                "unknown mode — want read-only | workspace-write | danger-full-access".into(),
            ),
        }
    }

    /// `/agent` — report the selection and what else is on offer;
    /// `/agent <name>` — run that `agents/<name>.md`; `/agent off` — clear
    /// it. The palette's AGENTS rows send `SetSessionAgent` directly; this
    /// is the typed route, and the only way to clear the selection.
    async fn command_agent(&self, session_id: u64, input: &str) -> (bool, String) {
        let dir = sica_core::paths::agents_dir();
        let word = input.trim();
        if word.is_empty() {
            let current = self.presets.lock().await.get(&session_id).cloned();
            let (available, _) = agents::preset::load_dir(&dir);
            let names: Vec<&str> = available.iter().map(|p| p.name.as_str()).collect();
            let line = match current {
                Some(n) => format!("agent: {n}"),
                None => "no agent selected".to_string(),
            };
            return (
                true,
                if names.is_empty() {
                    format!("{line}
none available — write one into {}", dir.display())
                } else {
                    format!("{line}
available: {}  (`/agent off` clears)", names.join(", "))
                },
            );
        }
        let name = (!matches!(word, "off" | "none" | "clear")).then(|| word.to_string());
        match self.set_session_agent(session_id, name).await {
            Ok(text) => (true, text),
            Err(text) => (false, text),
        }
    }

    /// Enqueue one inbox item and tell the FE what it became.
    async fn enqueue(&self, session_id: u64, item: Inbound) -> u32 {
        let accepted = item.accepted().to_string();
        let rows = self.inbox.push(session_id, item).await;
        publish_queue(&self.event_sink, session_id, rows, &accepted)
    }

    /// Rewrite a message still waiting in the queue.
    ///
    /// The id has to still name a waiting row: the loop may have claimed it
    /// between the frontend drawing the dock and the user pressing Enter,
    /// and an edit that quietly landed nowhere is indistinguishable from one
    /// that worked.
    pub async fn edit_queued(&self, session_id: u64, id: u64, text: String) -> Result<(), String> {
        if text.trim().is_empty() {
            return Err("a queued message cannot be emptied — remove it instead".into());
        }
        if !self.inbox.edit(session_id, id, text).await {
            return Err("that message already left the queue".into());
        }
        let rows = self.inbox.rows(session_id).await;
        publish_queue(&self.event_sink, session_id, rows, "edited");
        Ok(())
    }

    /// Drop a queued message before it ever runs.
    pub async fn remove_queued(&self, session_id: u64, id: u64) -> Result<(), String> {
        if self.inbox.remove(session_id, id).await.is_none() {
            return Err("that message already left the queue".into());
        }
        let rows = self.inbox.rows(session_id).await;
        publish_queue(&self.event_sink, session_id, rows, "removed");
        Ok(())
    }

    /// Promote a queued message into a steer on the running turn: it leaves
    /// the queue and joins that turn at its next hop instead of waiting for
    /// one of its own.
    pub async fn steer_queued(&self, session_id: u64, id: u64) -> Result<(), String> {
        if !self.active_turns.lock().await.contains_key(&session_id) {
            return Err("nothing is running to steer".into());
        }
        // Checked before the row is taken rather than after: a steer is
        // text, and a message whose images a steer would drop is better left
        // queued, where they still reach the model.
        if self
            .inbox
            .rows(session_id)
            .await
            .iter()
            .any(|r| r.id == id && r.images > 0)
        {
            return Err("a queued message with images cannot be steered".into());
        }
        let Some(Inbound::Followup { text, .. }) = self.inbox.remove(session_id, id).await else {
            return Err("that message already left the queue".into());
        };
        self.inbox.push(session_id, Inbound::Steer { text }).await;
        let rows = self.inbox.rows(session_id).await;
        publish_queue(&self.event_sink, session_id, rows, "steered");
        self.event_sink.emit(Event::LogLine {
            level:   "INFO".into(),
            message: "steering the running turn — the message lands at its next step".into(),
        });
        Ok(())
    }

    /// Splice user text into the running turn at its next hop. With no turn
    /// running there is nothing to steer, so it is an ordinary send.
    pub async fn steer_turn(&self, session_id: u64, text: String) {
        if text.trim().is_empty() {
            return;
        }
        if !self.active_turns.lock().await.contains_key(&session_id) {
            self.start_turn(session_id, text, Vec::new(), TurnSource::Human, None).await;
            return;
        }
        self.enqueue(session_id, Inbound::Steer { text }).await;
        self.event_sink.emit(Event::LogLine {
            level:   "INFO".into(),
            message: "steering the running turn — the message lands at its next step".into(),
        });
    }

    /// Push non-user context into the session. Reaches the model at the
    /// running turn's next hop, or at the start of the next turn when idle.
    pub async fn inject_context(&self, session_id: u64, text: String) {
        if text.trim().is_empty() {
            return;
        }
        self.enqueue(
            session_id,
            Inbound::Inject { content: text, source: ContextSource::Injected },
        )
        .await;
    }

    /// [`Self::start_turn`] as a type-erased future.
    ///
    /// A turn that ends with a followup queued starts the next turn itself,
    /// which makes `start_turn` indirectly recursive. Boxing through
    /// `dyn Future` is what keeps that future's type finite.
    fn start_boxed(
        &self,
        session_id: u64,
        text: String,
        images: Vec<UserImage>,
        source: TurnSource,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        let hub = self.clone();
        Box::pin(async move { hub.start_turn(session_id, text, images, source, None).await })
    }

    /// Accept a user message: queue it when a turn is already running,
    /// otherwise start a turn for it.
    pub async fn send_user_message(
        &self,
        session_id: u64,
        text: String,
        images: Vec<UserImage>,
    ) {
        // A send that arrives while a turn is running is *queued*, not a
        // cancellation: the running turn keeps its plan and this message
        // runs as the next turn, with no second send needed. The check and
        // the enqueue happen under the `active_turns` lock the turn task
        // also takes when it hands off, so a turn ending right now either
        // sees this followup or leaves its slot standing for us.
        {
            let guard = self.active_turns.lock().await;
            if guard.contains_key(&session_id) {
                let queued = self
                    .enqueue(session_id, Inbound::Followup { text, images })
                    .await;
                drop(guard);
                self.event_sink.emit(Event::LogLine {
                    level:   "INFO".into(),
                    message: format!("a turn is running — message queued ({queued} waiting)"),
                });
                return;
            }
        }
        self.start_turn(session_id, text, images, TurnSource::Human, None).await;
    }

    /// Rewrite an earlier prompt and re-run the conversation from it.
    ///
    /// The edited message keeps the original's images — this is an edit of
    /// what the user *said*, not of what they attached — and runs as an
    /// ordinary human turn once the span it supersedes has left the model's
    /// view. Errors are returned rather than logged so the dispatcher can
    /// answer the frontend with the reason.
    pub async fn edit_user_message(
        &self,
        session_id: u64,
        seq: u64,
        text: String,
    ) -> Result<(), String> {
        if text.trim().is_empty() {
            return Err("an edited prompt cannot be empty".into());
        }
        // A rewind while the loop is appending would race its own history.
        // The FE hides the affordance during a turn; this is the backstop.
        if self.active_turns.lock().await.contains_key(&session_id) {
            return Err("a turn is running — stop it before editing a prompt".into());
        }
        let images = {
            let g = self.sessions.lock().await;
            let log = g.get(&session_id).ok_or("no such session")?;
            // The message has to still be *visible*: one already folded away
            // by a compaction has no span left to rewind to.
            if !log.derive_surface().iter().any(|e| e.seq == seq) {
                return Err(
                    "that message is no longer part of the conversation \
                     (compacted away)"
                        .into(),
                );
            }
            match log.events.iter().find(|e| e.seq == seq).map(|e| &e.kind) {
                Some(EventKind::UserMessage { images, .. }) => images.clone(),
                Some(_) => return Err("only a user message can be edited".into()),
                None => return Err("no such message in this session".into()),
            }
        };
        self.start_turn(session_id, text, images, TurnSource::Human, Some(seq)).await;
        Ok(())
    }

    /// Run one user message as a turn, unconditionally.
    ///
    /// Separate from [`Self::send_user_message`] because the turn loop calls
    /// it to start a queued followup while still *holding* the session's
    /// slot: going back through the queue gate there would see that slot,
    /// re-queue the message it had just claimed, and strand it.
    ///
    /// `rewind` is the seq of a prompt being re-run after an edit: the span
    /// from it to the end of the log leaves the derived history *before*
    /// this turn's own snapshots are appended, so the instructions and
    /// runtime-context entries this turn writes are not swept away with it
    /// and the message order stays exactly what an ordinary send produces.
    async fn start_turn(
        &self,
        session_id: u64,
        text: String,
        images: Vec<UserImage>,
        source: TurnSource,
        rewind: Option<u64>,
    ) {
        let Some(client) = self.llm.lock().await.clone() else {
            // A followup handed this call a *reserved* slot. Nothing is
            // going to run now, so release it — otherwise the session reads
            // as busy forever and every later send queues behind a turn
            // that does not exist. A no-op on the ordinary path.
            self.active_turns.lock().await.remove(&session_id);
            let stranded = self.inbox.queued(session_id).await;
            self.event_sink.emit(Event::LogLine {
                level:   "WARN".into(),
                message: if stranded > 0 {
                    format!(
                        "no LLM connected — cannot send ({stranded} queued                          message(s) will not run; send them again once                          connected)"
                    )
                } else {
                    "no LLM connected — cannot send".into()
                },
            });
            return;
        };

        // `UserPromptSubmit` hooks (guide §13.1) run before anything is
        // written: a denied prompt must leave no trace of a turn that never
        // happened. The session's log may not exist yet — the hook's own
        // rows and any injected context land once it does, below.
        let prompt_hooks = if self.hooks.for_event(hooks::HookEvent::UserPromptSubmit).is_empty() {
            hooks::Merged::default()
        } else {
            hooks::run_event(
                &self.hooks,
                &self.sessions,
                &self.event_sink,
                hooks::HookEvent::UserPromptSubmit,
                session_id,
                Some(&text),
            )
            .await
        };
        if prompt_hooks.rank == hooks::Rank::Deny {
            // The slot a followup reserved has to come back, exactly as on
            // the no-LLM path — otherwise the session reads as busy forever.
            self.active_turns.lock().await.remove(&session_id);
            self.event_sink.emit(Event::LogLine {
                level:   "WARN".into(),
                message: format!("a hook refused this prompt: {}", prompt_hooks.reason_text()),
            });
            return;
        }

        // A new user message starts a fresh repeat-tool chain.
        self.repeat.lock().await.remove(&session_id);

        // `/name …` resolves against the three markdown families — a
        // palette pick and a typed token arrive here identically. A command
        // or skill body is injected as instructions *before* the message; an
        // agent name is a selection, the same one the palette's AGENTS row
        // and `/agent <name>` make. Either way the message itself is kept as
        // typed, so the transcript shows what the user sent.
        let mut expansion = None;
        let mut select_agent: Option<String> = None;
        match agents::invoke::resolve(&text, &agents::invoke::Roots::from_workspace()) {
            Some(agents::invoke::Invocation::Context(exp)) => {
                self.event_sink.emit(Event::LogLine {
                    level:   "INFO".into(),
                    message: format!(
                        "loaded /{} ({:?}) as context for this turn",
                        exp.name, exp.family
                    ),
                });
                expansion = Some(exp);
            }
            // Held rather than applied: the session's log may not exist
            // yet, and a pending rewind has to be the first thing appended
            // to it. Applied right after the log block below.
            Some(agents::invoke::Invocation::Agent { name }) => select_agent = Some(name),
            None => {}
        }

        // Ensure the session exists and record the user message straight
        // away — the log is flushed on every append, so the session is
        // recoverable even if the LLM call dies mid-stream.
        let outer_turn = self.next_turn.fetch_add(1, Ordering::Relaxed);
        // Live policy facts for the runtime-context snapshot.
        let perm_mode = self.permissions.lock().await.get(&session_id).copied().unwrap_or_default();
        let turn_plan_active = self.plans.lock().await.get(&session_id).copied().unwrap_or(false);
        // The placeholder-or-fallback title this send leaves behind, so the
        // LLM titler later knows the title is still automatic.
        let provisional_title;
        let user_seq;
        {
            let mut sessions = self.sessions.lock().await;
            let log = sessions
                .entry(session_id)
                .or_insert_with(|| SessionLog::new(session_id, default_title(session_id)));
            // The rewind goes first, before this turn's own snapshots exist:
            // it names the whole tail of the log, and anything appended
            // ahead of it would be inside the span it erases.
            if let Some(start_seq) = rewind {
                let end_seq = log.last_seq();
                log.append(EventKind::Rewind { start_seq, end_seq });
            }
            // Durable context snapshots for this turn, each shadowing its
            // predecessor so exactly one copy of each is model-visible:
            // workspace instructions (AGENTS.md chain) first, then the
            // runtime snapshot (time, cwd, os, model).
            if refresh_instructions(log) {
                self.event_sink.emit(Event::LogLine {
                    level: "INFO".into(),
                    message: "workspace instructions snapshot updated".into(),
                });
            }
            append_runtime_context(log, &client.model, perm_mode, turn_plan_active);
            if let Some(exp) = expansion {
                log.append(EventKind::ContextInjected {
                    surface: SurfaceOp::Append,
                    source: ContextSource::SkillInvocation { name: exp.name },
                    content: exp.content,
                });
            }
            // A `UserPromptSubmit` hook's context sits with the turn's other
            // snapshots, ahead of the message it is about.
            for extra in &prompt_hooks.context {
                if !extra.trim().is_empty() {
                    log.append(EventKind::ContextInjected {
                        surface: SurfaceOp::Append,
                        source:  ContextSource::Injected,
                        content: extra.clone(),
                    });
                }
            }
            user_seq = log.append(EventKind::UserMessage {
                surface: SurfaceOp::Append,
                content: text.clone(),
                images: images.clone(),
            });
            log.append(EventKind::TurnStart { turn_id: outer_turn, source });
            // First message into a still-placeholder session: name it from
            // the message right now, so the sidebar never shows "Session N"
            // for something that has content. The LLM title (below, after
            // the reply) replaces this.
            let mut title = log.title();
            if title == default_title(session_id) {
                let fb = title_gen::fallback(&text);
                if !fb.is_empty() {
                    log.append(EventKind::SessionTitle { title: fb.clone() });
                    self.event_sink.emit(Event::SessionTitleChanged {
                        session_id,
                        title: fb.clone(),
                    });
                    title = fb;
                }
            }
            provisional_title = title;
            if let Err(e) = sessions_store::flush(log) {
                warn!(error = %e, session_id, "flush session (after user msg) failed");
            }
        }
        // The agent a typed `/name` picked, now that the log exists and any
        // rewind has been recorded ahead of it. A refusal never costs the
        // user their message: it is logged, and the turn goes out under
        // whatever agent the session already had.
        if let Some(name) = select_agent {
            if let Err(e) = self.set_session_agent(session_id, Some(name)).await {
                self.event_sink.emit(Event::LogLine {
                    level:   "WARN".into(),
                    message: format!("agent not changed: {e}"),
                });
            }
        }

        // The durable handle for this prompt, so the transcript can offer an
        // edit on it without first reloading the session from disk.
        self.event_sink.emit(Event::UserMessageStored { session_id, seq: user_seq });

        // Register a cancellation token for this session. If a previous turn
        // is still in flight (shouldn't normally happen — the FE gates Send
        // while a turn is unfinished), cancel it before installing the new one.
        let cancel = CancellationToken::new();
        let marker = self.next_marker.fetch_add(1, Ordering::Relaxed);
        {
            let mut guard = self.active_turns.lock().await;
            // Any slot found here is either a turn the FE let overlap (a
            // safety net — cancel it) or this session's own reservation
            // from a followup handoff, whose token is already finished, so
            // cancelling is a no-op.
            if let Some((_, prev)) = guard.insert(session_id, (marker, cancel.clone())) {
                prev.cancel();
            }
        }

        let events = self.event_sink.clone();
        let sessions_map = self.sessions.clone();
        let inbox = self.inbox.clone();
        // The turn starts the next one itself when a followup is queued.
        let hub = self.clone();
        let control = ControlState { turn_source: source, ..self.control() };
        let plans = self.plans.clone();
        let active_turns = self.active_turns.clone();
        let next_turn = self.next_turn.clone();
        // Guide §5.2: one resolution per turn covers both halves of the
        // preset — the persona section of the prompt and the registry the
        // dispatcher answers from — so the prompt can never advertise a
        // skill the dispatch would refuse.
        let (skills, persona) = self.effective_agent(session_id).await;
        let title_client = client.clone();
        let event_sink = self.event_sink.clone();
        let meters = self.meters.clone();
        let (tool_mode, opt_max_tokens, compact_policy) = {
            let opts = self.llm_opts.lock().await;
            (opts.tool_mode, opts.max_tokens, opts.compact)
        };
        // Every wire-shaping site below asks the same yes/no question — PTC
        // rides the native transport, it is not a third one — so the mode
        // itself only reaches the two places that narrow the catalogue.
        let native_tools = tool_mode.native();
        let model_name = client.model.clone();
        let window = self.context_window.load(Ordering::Relaxed);
        // The options half of the request envelope. Snapshotted per turn
        // like the rest: a mid-turn settings change reaches the next turn,
        // and the envelope must describe the request that was sent.
        let envelope_options = {
            let opts = self.llm_opts.lock().await;
            envelope_options_json(&model_name, &opts, window)
        };
        tokio::spawn(async move {
            let mut hops: u8 = 0;
            // Retry budget for the *current* step; reset once a step lands.
            let mut retries: u32 = 0;
            // Why the loop ended, for the durable `TurnEnd`. An interrupt is
            // detected from the token after the loop.
            let mut finish = "done";
            // Always overwritten on the first iteration before the post-loop
            // read; the initial value is just to satisfy definite assignment.
            #[allow(unused_assignments)]
            let mut last_assistant = String::new();
            // Turn-level accounting for `Event::TurnUsage` (§3.5). The live
            // `TokenUsage` meter is per-session and cumulative; the tail
            // pills need this turn's own numbers, summed over its hops.
            let turn_clock = std::time::Instant::now();
            let mut turn_prompt: u32 = 0;
            let mut turn_completion: u32 = 0;
            let mut turn_reasoning: u32 = 0;
            let mut turn_ttft_ms: u64 = 0;
            loop {
                // Interrupts land between hops as often as mid-stream. Bailing
                // here keeps a cancelled turn from opening another request —
                // which would emit a fresh `TurnStarted` the FE renders as a
                // new (empty) turn, and burn a tokenize round-trip first.
                if cancel.is_cancelled() {
                    break;
                }

                // Claim whatever arrived since the last hop. This has to
                // happen before `build_history` or the request about to go
                // out would not contain it — that is the whole point of the
                // inbox: input reaches the model at the next step instead
                // of after the turn.
                for item in inbox.drain_mid_turn(session_id).await {
                    match item {
                        Inbound::Steer { text } => {
                            event_sink.emit(Event::LogLine {
                                level:   "INFO".into(),
                                message: format!("steer applied: {}", one_line(&text, 120)),
                            });
                            append_event(&sessions_map, session_id, EventKind::UserMessage {
                                surface: SurfaceOp::Append,
                                content: text,
                                images:  Vec::new(),
                            })
                            .await;
                        }
                        Inbound::Inject { content, source } => {
                            event_sink.emit(Event::LogLine {
                                level:   "INFO".into(),
                                message: format!("context injected ({})", source.label()),
                            });
                            append_event(&sessions_map, session_id, EventKind::ContextInjected {
                                surface: SurfaceOp::Append,
                                source,
                                content,
                            })
                            .await;
                        }
                        // Followups wait for their own turn; `drain_mid_turn`
                        // leaves them queued.
                        Inbound::Followup { .. } => {}
                    }
                }

                // Derive the history fresh from the event log each iteration:
                // the previous hop appended both the assistant message and
                // the tool result, so this picks them up uniformly.
                let plan_policy = if plans.lock().await.get(&session_id).copied().unwrap_or(false) {
                    Some(plan_policy_text())
                } else {
                    None
                };
                let mut wh = match build_history(
                    &sessions_map, session_id, &skills, tool_mode, &model_name, plan_policy,
                    persona.as_deref(),
                ).await {
                    Ok(Some(wh)) => wh,
                    Ok(None) => break, // session vanished mid-turn
                    Err(e) => {
                        // A malformed prompt fails loud rather than going out
                        // half-interpolated (dsh's stance).
                        warn!(session_id, error = %e, "prompt assembly failed");
                        event_sink.emit(Event::LogLine {
                            level: "ERROR".into(),
                            message: format!("prompt assembly failed: {e}"),
                        });
                        finish = "error";
                        break;
                    }
                };

                // Prompt budget: window minus room for the response (and a
                // small safety margin for template overhead).
                let reserve = opt_max_tokens.unwrap_or(4096).saturating_add(512);
                let budget = window.saturating_sub(reserve).max(1024);

                // Auto-compaction. Once the assembled prompt fills the
                // policy's threshold share of that budget, fold the older
                // part of the history into an LLM-written summary. This runs
                // *before* the trim so compaction is the primary mechanism
                // and the trimmer stays a backstop — otherwise the trimmer
                // would silently amputate history long before the meter ever
                // read the trigger, because the budget is already well under
                // the window. The usage-anchored meter prices the prompt
                // when it has an anchor for this exact envelope.
                // Usage-anchored pricing: the meter prices only what was
                // added since the last provider-reported envelope; the
                // heuristic covers the rest.
                let heuristic = agents::compact::approx_total_wire(&wh.messages);
                let mut anchored = meters
                    .lock()
                    .await
                    .get(&session_id)
                    .and_then(|m| m.estimate(wh.envelope, &wh.entries));
                let prompt_tokens = anchored.unwrap_or(heuristic);
                let over = u64::from(prompt_tokens) * 100
                    >= u64::from(budget) * u64::from(compact_policy.threshold_pct);
                if over
                    && compact_session(
                        &sessions_map, session_id, &client, &event_sink, budget,
                        &compact_policy, &wh, native_tools, &cancel,
                    )
                    .await
                {
                    wh = match build_history(
                        &sessions_map, session_id, &skills, tool_mode, &model_name,
                        if plans.lock().await.get(&session_id).copied().unwrap_or(false) {
                            Some(plan_policy_text())
                        } else {
                            None
                        },
                        persona.as_deref(),
                    ).await {
                        Ok(Some(wh)) => wh,
                        Ok(None) => break,
                        Err(e) => {
                            warn!(session_id, error = %e, "prompt assembly failed");
                            event_sink.emit(Event::LogLine {
                                level: "ERROR".into(),
                                message: format!("prompt assembly failed: {e}"),
                            });
                            finish = "error";
                            break;
                        }
                    };
                    // Re-price the rebuilt history — the pre-compaction
                    // estimate no longer describes what is about to be sent.
                    anchored = meters
                        .lock()
                        .await
                        .get(&session_id)
                        .and_then(|m| m.estimate(wh.envelope, &wh.entries));
                }

                // The envelope this request goes out with, recorded before
                // it does — and before the trimmer consumes `wh.messages`.
                // It is appended only when it differs from the last one in
                // the log, so a session whose prompt never changes stores
                // one copy, and one whose `memory.md` changed mid-session
                // stores the before and the after, which is the only way the
                // ledger can say which rows read which. The trimmer never
                // touches the system prompt or the tools array, so recording
                // ahead of it describes exactly what goes on the wire.
                record_envelope(&sessions_map, session_id, &wh, &envelope_options).await;

                // The trimmer's "context notice" marker is wire-only: it is
                // inserted here and never enters the log.
                let trimmed = agents::context::trim_to_budget(wh.messages, budget);
                if trimmed.dropped > 0 {
                    event_sink.emit(Event::LogLine {
                        level: "WARN".into(),
                        message: format!(
                            "context: dropped {} oldest message(s) to fit the \
                             {window}-token window",
                            trimmed.dropped
                        ),
                    });
                }

                // Seq of the newest surface entry actually sent — the meter
                // anchor records it. When the trimmer amputated anything the
                // envelope no longer matches what the anchor priced, so the
                // anchor is dropped instead.
                let anchor_seq = wh.entries.last().map(|e| e.seq);

                // Invariant companions (§14.3), only under `--invariants`.
                // The derivation here is deliberately *second*: the request
                // was built from one pass over the log, this is another, and
                // the whole point is whether two independent observations of
                // the same log agree.
                let seq_before_attempt = if crate::invariants::enabled() {
                    let (events, derived) = {
                        let g = sessions_map.lock().await;
                        match g.get(&session_id) {
                            Some(log) => (
                                log.events.clone(),
                                sica_core::event::derive_messages(&log.events),
                            ),
                            None => (Vec::new(), Vec::new()),
                        }
                    };
                    crate::invariants::report(
                        event_sink.as_ref(),
                        crate::invariants::check_request_matches_log(
                            &trimmed.messages,
                            &wire_messages(&derived, native_tools),
                        ),
                    );
                    crate::invariants::report(
                        event_sink.as_ref(),
                        crate::invariants::check_compaction_span_balanced(&events),
                    );
                    events.last().map(|e| e.seq).unwrap_or(0)
                } else {
                    0
                };

                let turn_id = next_turn.fetch_add(1, Ordering::Relaxed);
                let out = agents::turn::run_turn(
                    client.clone(),
                    events.clone(),
                    agents::turn::TurnInput {
                        session_id,
                        turn_id,
                        messages: trimmed.messages,
                        tools: tools_for(&skills, tool_mode),
                        limit: window,
                        budget,
                        cancel: Some(cancel.clone()),
                        estimate: anchored,
                        breakdown: Some(wh.breakdown),
                    },
                )
                .await;

                // A transport/server failure — or a clean stream that carried
                // nothing at all — is not an assistant reply. Nothing from the
                // attempt is persisted, so looping back rebuilds the identical
                // request over the same history: a retry the model cannot
                // tell from the first attempt. Fatal errors (4xx) and an
                // exhausted budget end the turn visibly instead of leaving a
                // blank bubble that looks like the model chose silence.
                let failure = match &out.error {
                    Some(e) => Some(llm::retry::classify(e)),
                    None if out.content.is_empty()
                        && out.reasoning.is_empty()
                        && out.tool_calls.is_empty()
                        && !cancel.is_cancelled() =>
                    {
                        Some(llm::retry::empty_response())
                    }
                    None => None,
                };
                if let Some(failure) = failure {
                    if failure.is_retryable() && retries < llm::retry::RETRY_MAX {
                        retries += 1;
                        let delay = llm::retry::backoff(retries);
                        let msg = format!(
                            "LLM request failed ({}) — retry {retries}/{} in {} ms",
                            failure.reason(),
                            llm::retry::RETRY_MAX,
                            delay.as_millis()
                        );
                        warn!(session_id, turn_id, "{msg}");
                        event_sink.emit(Event::LogLine { level: "WARN".into(), message: msg });
                        event_sink.emit(Event::LlmRetry {
                            session_id,
                            attempt:  retries,
                            max:      llm::retry::RETRY_MAX,
                            delay_ms: delay.as_millis() as u64,
                            reason:   failure.reason().to_string(),
                        });
                        if crate::invariants::enabled() {
                            let events = {
                                let g = sessions_map.lock().await;
                                g.get(&session_id).map(|l| l.events.clone()).unwrap_or_default()
                            };
                            crate::invariants::report(
                                event_sink.as_ref(),
                                crate::invariants::check_retry_appended_nothing(
                                    &events,
                                    seq_before_attempt,
                                ),
                            );
                        }
                        append_event(&sessions_map, session_id, EventKind::LlmRetry {
                            attempt: retries,
                            max: llm::retry::RETRY_MAX,
                            delay_ms: delay.as_millis() as u64,
                            reason: failure.reason().to_string(),
                        })
                        .await;
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => break,
                            _ = tokio::time::sleep(delay) => {}
                        }
                        continue;
                    }
                    let msg = if failure.is_retryable() {
                        format!(
                            "LLM request failed ({}) — giving up after {retries} retries",
                            failure.reason()
                        )
                    } else {
                        format!("LLM request failed ({}) — not retryable", failure.reason())
                    };
                    warn!(session_id, turn_id, "{msg}");
                    event_sink.emit(Event::LogLine { level: "ERROR".into(), message: msg });
                    finish = "error";
                    break;
                }
                retries = 0;

                // Anchor the usage meter on the provider's own count for
                // this exact envelope — the next request prices only what
                // was added since. Skipped when the trimmer amputated
                // anything (the request no longer matches the envelope the
                // anchor would price).
                if trimmed.dropped == 0 {
                    if let (Some(seq), Some(u)) = (anchor_seq, out.usage.as_ref()) {
                        if u.prompt_tokens > 0 {
                            meters
                                .lock()
                                .await
                                .entry(session_id)
                                .or_default()
                                .record(wh.envelope, seq, u.prompt_tokens);
                        }
                    }
                } else {
                    meters.lock().await.remove(&session_id);
                }

                if let Some(u) = out.usage.as_ref() {
                    turn_prompt = turn_prompt.saturating_add(u.prompt_tokens);
                    turn_completion = turn_completion.saturating_add(u.completion_tokens);
                }
                turn_reasoning = turn_reasoning.saturating_add(out.reasoning.len() as u32);
                if turn_ttft_ms == 0 {
                    turn_ttft_ms = out.ttft_ms.unwrap_or(0);
                }

                last_assistant = out.content.clone();

                // Persist the assistant message (it includes the tool_call
                // block if one was emitted — kept verbatim so re-loading the
                // session shows what the model actually said), then the
                // durable token reading for this hop.
                {
                    let reasoning = if out.reasoning.is_empty() {
                        None
                    } else {
                        Some(out.reasoning.clone())
                    };
                    // Native tool calls are persisted on the assistant
                    // message so history replay matches what the server saw.
                    // Skipped on interrupt: a dangling `tool_calls` with no
                    // tool responses would poison the next request's
                    // template.
                    let tool_calls = if out.tool_calls.is_empty() || cancel.is_cancelled() {
                        None
                    } else {
                        Some(native_calls_to_json(&out.tool_calls))
                    };
                    let mut g = sessions_map.lock().await;
                    let Some(log) = g.get_mut(&session_id) else {
                        debug!(session_id, "session vanished mid-turn, skipping persist");
                        return;
                    };
                    log.append(EventKind::AssistantMessage {
                        surface: SurfaceOp::Append,
                        content: out.content.clone(),
                        reasoning,
                        tool_calls,
                    });
                    log.append(EventKind::TokenUsage {
                        used: out.used_tokens,
                        limit: window,
                        budget,
                        prompt_tokens: out.usage.map(|u| u.prompt_tokens),
                        completion_tokens: out.usage.map(|u| u.completion_tokens),
                    });
                    if let Err(e) = sessions_store::flush(log) {
                        warn!(error = %e, session_id, "flush session (after assistant msg) failed");
                    }
                }

                // If the user hit Esc, drop out before we go shopping for a
                // tool call on a half-completed assistant reply.
                if cancel.is_cancelled() {
                    break;
                }

                // The provider stopped at `max_tokens`. A truncated reply is
                // not a place to go looking for a tool call, and looping would
                // ask the model to continue from a sentence it never finished.
                // End the turn under a reason the FE renders as its own row
                // (§3.5); the partial output is already persisted.
                if out.finish_reason == "max_tokens" {
                    finish = "max_tokens";
                    break;
                }

                // Native tool-calling path: dispatch every call the model
                // emitted, answer each `tool_call_id`, and loop for the
                // model's next turn. Raw outcomes are returned verbatim —
                // no expectation/summarizer indirection in native mode.
                // Consecutive read-only calls overlap (§6.2); the batch
                // runner preserves model order for every append.
                if native_tools && !out.tool_calls.is_empty() {
                    let over_limit = hops >= MAX_TOOL_HOPS;
                    if !over_limit {
                        hops += 1;
                    }
                    let concluded = control
                        .run_native_batch(
                            &sessions_map,
                            session_id,
                            &skills,
                            &out.tool_calls,
                            over_limit,
                            tool_mode.ptc(),
                            &client,
                            &cancel,
                        )
                        .await;
                    if over_limit {
                        event_sink.emit(Event::LogLine {
                            level: "WARN".into(),
                            message: format!(
                                "tool-hop limit ({MAX_TOOL_HOPS}) reached — aborting further skill calls"
                            ),
                        });
                        finish = "hop-limit";
                        break;
                    }
                    if concluded {
                        // `exit-plan-mode` approved: the plan review is the
                        // turn's result, no further model request needed.
                        finish = "done";
                        break;
                    }
                    continue;
                }

                // Look for a tool call. If none, we're done — but first
                // check whether the model *tried* to emit one in an
                // unrecognised shape (a `tool_call` JSON fence, etc.). That
                // path used to fail silently and look like "model chose not
                // to call a tool" in the FE; surface it as a WARN so the
                // miscall is visible.
                let Some(call) =
                    agents::extract_tool_call_known(&out.content, |name| {
                        skills.by_name.contains_key(name)
                    })
                else {
                    if let Some(reason) = agents::parse_tool_call::rejected_attempt(
                        &out.content,
                        |name| skills.by_name.contains_key(name),
                    ) {
                        let msg = format!(
                            "assistant emitted {reason} — no skill ran, so treat \
                             its reply as unverified"
                        );
                        warn!(session_id, "{msg}");
                        event_sink.emit(Event::LogLine {
                            level:   "WARN".into(),
                            message: msg,
                        });
                    }
                    break;
                };
                // Resolve once up front only to record the named arguments —
                // the dispatch below resolves again for the skill handle. A
                // reloaded transcript needs the real args to draw a body
                // (§3.4); `args_preview` truncates and drops the names.
                let call_args_json = skills
                    .resolve(&call)
                    .map(|(_, args)| args.to_string())
                    // A log line is read back in full on every start, so an
                    // outsized `write-file` body does not belong in one.
                    .filter(|json| json.len() <= LOGGED_ARGS_JSON_MAX);
                let call_seq = append_event(&sessions_map, session_id, EventKind::ToolCall {
                    name: call.skill.clone(),
                    args_preview: agents::parse_tool_call::render(&call.skill, &call.raw_args),
                    expectation: call.expectation.clone(),
                    call_id: None,
                    args_json: call_args_json,
                })
                .await
                .unwrap_or(0);
                if hops >= MAX_TOOL_HOPS {
                    let msg = format!(
                        "tool-hop limit ({MAX_TOOL_HOPS}) reached — aborting further skill calls"
                    );
                    event_sink.emit(Event::LogLine { level: "WARN".into(), message: msg.clone() });
                    append_tool_result(&sessions_map, session_id, call_seq, &call.skill, None, false, &msg, true)
                        .await;
                    finish = "hop-limit";
                    break;
                }
                hops += 1;

                // Dispatch the skill through the guarded pipeline. Harness
                // controls (`todo-write`, `exit-plan-mode`) run in the hub
                // instead of a sub-agent; unknown skills record an error
                // result and let the model recover on the next hop.
                // Successful outcomes get post-summarised through the same
                // `client` so the main agent receives a focused answer
                // instead of the raw skill output (matches the
                // natural-language contract in memory.md).
                let mut conclude_turn = false;
                let (outcome, trusted) = match skills.resolve(&call) {
                    Some((skill, args)) if agents::control::is_control_skill(skill.name()) => {
                        let preview =
                            agents::parse_tool_call::render(&call.skill, &call.raw_args);
                        let (outcome, conclude) = control
                            .handle_control(
                                &sessions_map,
                                skill.name(),
                                &args,
                                &preview,
                                &call.expectation,
                                session_id,
                                call_seq,
                                &cancel,
                            )
                            .await;
                        conclude_turn = conclude;
                        (outcome, true)
                    }
                    Some((skill, args)) => {
                        let sub = control
                            .sub_agent(&sessions_map, session_id, &client, cancel.clone())
                            .await
                            .with_log_seq(call_seq);
                        let report = sub
                            .run_report(agents::ToolInvocation {
                                skill: &*skill,
                                args,
                                raw_args: call.raw_args.clone(),
                                expectation: call.expectation.clone(),
                            })
                            .await;
                        if let Some(rec) = report.approval {
                            append_event(&sessions_map, session_id, EventKind::Approval {
                                skill: rec.skill,
                                args_preview: rec.args_preview,
                                decision: rec.decision.to_string(),
                            })
                            .await;
                        }
                        control
                            .inject_notices(&sessions_map, session_id, report.notices)
                            .await;
                        (report.outcome, skill.trusted())
                    }
                    None => {
                        let args = serde_json::json!(call.raw_args);
                        // Bypassed the pipeline — count it manually.
                        control.observe(&sessions_map, session_id, &call.skill, &args).await;
                        (
                            agents::SkillOutcome {
                                ok: false,
                                summary: format!("unknown skill `{}`", call.skill),
                            },
                            true,
                        )
                    }
                };

                append_tool_result(
                    &sessions_map,
                    session_id,
                    call_seq,
                    &call.skill,
                    None,
                    outcome.ok,
                    &outcome.summary,
                    trusted,
                )
                .await;
                if outcome.ok && is_fs_skill(&call.skill) {
                    reconcile_instructions_after_fs(&sessions_map, &event_sink, session_id).await;
                }
                // Pipeline dispatches counted their own repeat; control
                // and unknown outcomes were observed inline above.
                if conclude_turn {
                    finish = "done";
                    break;
                }
            }

            let finish = if cancel.is_cancelled() { "interrupted" } else { finish };
            append_event(&sessions_map, session_id, EventKind::TurnEnd {
                turn_id: outer_turn,
                finish_reason: finish.to_string(),
                hops,
            })
            .await;
            event_sink.emit(Event::TurnUsage {
                session_id,
                turn_id:     outer_turn,
                prompt:      turn_prompt,
                completion:  turn_completion,
                reasoning:   turn_reasoning,
                duration_ms: turn_clock.elapsed().as_millis() as u64,
                ttft_ms:     turn_ttft_ms,
            });

            // What happens next, decided under the session's slot lock so
            // the slot is never released for a turn that is about to start
            // anyway — a send arriving in that gap would otherwise race the
            // continuation instead of queueing behind it.
            //
            // Order matters: a queued human message wins over a goal round.
            // The person is here now; the objective can wait a turn.
            enum Next {
                Followup(String, Vec<UserImage>),
                GoalRound(Goal),
                Idle,
            }
            let next = {
                let mut guard = active_turns.lock().await;
                let mine = guard
                    .get(&session_id)
                    .is_some_and(|(slot_marker, _)| *slot_marker == marker);
                let next = if !mine {
                    // A newer send already replaced this slot; it owns what
                    // comes next.
                    Next::Idle
                } else if let Some((text, images)) = inbox.take_followup(session_id).await {
                    Next::Followup(text, images)
                } else if cancel.is_cancelled() {
                    // Stop means stop: a goal round would restart the work
                    // the user just interrupted. Disarming makes that
                    // explicit and durable until they say continue.
                    control.set_armed(session_id, false).await;
                    Next::Idle
                } else {
                    match control.goal(session_id).await {
                        Some(goal)
                            if goal.rounds_left() && control.armed(session_id).await =>
                        {
                            Next::GoalRound(goal)
                        }
                        _ => Next::Idle,
                    }
                };
                if mine && matches!(next, Next::Idle) {
                    guard.remove(&session_id);
                }
                next
            };

            match next {
                Next::Followup(next_text, next_images) => {
                    let rows = inbox.rows(session_id).await;
                    let queued = publish_queue(&event_sink, session_id, rows, "running");
                    event_sink.emit(Event::LogLine {
                        level:   "INFO".into(),
                        message: format!("running queued message ({queued} still waiting)"),
                    });
                    // Boxed: this is the turn loop starting the next turn,
                    // and the future's type has to stay finite. A queued
                    // message is still the user's own words, so it carries
                    // human authority.
                    tokio::spawn(hub.start_boxed(
                        session_id,
                        next_text,
                        next_images,
                        TurnSource::Followup,
                    ));
                }
                // Round driver (§12.3): with an active, armed goal and
                // rounds left, going idle opens the next round instead of
                // ending the session's work.
                Next::GoalRound(goal) => {
                    let next_goal = agents::goal::start_round(&goal);
                    let round = next_goal.rounds_started;
                    let max = next_goal.max_rounds;
                    let prompt = agents::goal::round_prompt(&next_goal);
                    // The round is recorded *before* it runs: a round that
                    // crashes must still cost a round, or an objective that
                    // crashes every time would loop until the process died.
                    control.put_goal(&sessions_map, session_id, next_goal).await;
                    event_sink.emit(Event::LogLine {
                        level:   "INFO".into(),
                        message: format!("goal round {round}/{max} starting"),
                    });
                    tokio::spawn(hub.start_boxed(
                        session_id,
                        prompt,
                        Vec::new(),
                        TurnSource::GoalRound,
                    ));
                }
                Next::Idle => {}
            }

            // Skip the auto-title work if the user interrupted — a partial
            // assistant reply isn't a useful title source.
            if cancel.is_cancelled() {
                return;
            }

            // Auto-title only fires once, after the first complete exchange
            // (user → assistant final). Count user messages to decide. The
            // title is "still automatic" when it is the placeholder or the
            // fallback this send wrote.
            let trigger_title = {
                let g = sessions_map.lock().await;
                let Some(log) = g.get(&session_id) else { return };
                let title_is_auto = log.title() == provisional_title;
                log.user_message_count() == 1 && title_is_auto && !last_assistant.is_empty()
            };

            if trigger_title {
                let sessions_map = sessions_map.clone();
                let event_sink = event_sink.clone();
                let user_text = text.clone();
                let assistant_text = last_assistant.clone();
                tokio::spawn(async move {
                    let Some(title) =
                        title_gen::summarize(&title_client, &user_text, &assistant_text).await
                    else {
                        return;
                    };
                    let mut g = sessions_map.lock().await;
                    let Some(log) = g.get_mut(&session_id) else {
                        return;
                    };
                    // Re-check — the user may have renamed it manually in
                    // the meantime (future feature, harmless now).
                    if log.title() != provisional_title || log.title() == title {
                        return;
                    }
                    log.append(EventKind::SessionTitle { title: title.clone() });
                    if let Err(e) = sessions_store::flush(log) {
                        warn!(error = %e, session_id, "flush session (after title-gen) failed");
                    }
                    event_sink.emit(Event::SessionTitleChanged {
                        session_id,
                        title,
                    });
                });
            }
        });
    }
}

/// The two events a queue change produces, and the depth it settled at.
///
/// `InboxChanged` carries the depth and the word for what the frontend just
/// optimistically rendered; `QueueChanged` carries the rows the composer's
/// queue dock draws. They always go out together and in this order, so the
/// dock and the count can never disagree about the same moment. A free
/// function because the turn task publishes from its own `event_sink`
/// clone, long after it stopped holding a `&ChatHub`.
fn publish_queue(
    sink: &Arc<dyn EventSink>,
    session_id: u64,
    rows: Vec<protocol::QueuedDump>,
    accepted: &str,
) -> u32 {
    let queued = rows.len() as u32;
    sink.emit(Event::InboxChanged {
        session_id,
        queued,
        accepted: accepted.to_string(),
    });
    sink.emit(Event::QueueChanged { session_id, rows });
    queued
}

/// One-line, length-capped preview of user text for a log line.
fn one_line(text: &str, cap: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let head = sica_core::retain::utf8_head(&flat, cap);
    if head.len() < flat.len() { format!("{head}…") } else { head.to_string() }
}

/// Append one event to a session's log and flush it. Returns the seq, or
/// `None` when the session no longer exists. A flush failure is logged and
/// the in-memory log keeps going — the next successful flush writes every
/// line still pending.
/// The sampling and mode facts that shaped a request, as JSON — the
/// inspector's Options tab. Deliberately not the whole `LlmOptions`: the
/// base URL and the API key are provider configuration, not part of what
/// the model was asked, and one of them is a secret.
fn envelope_options_json(model: &str, opts: &protocol::LlmOptions, window: u32) -> String {
    serde_json::json!({
        "model":          model,
        "temperature":    opts.temperature,
        "max_tokens":     opts.max_tokens,
        "context_window": window,
        "tool_mode":      opts.tool_mode.label(),
        "thinking":       opts.thinking,
        "compact": {
            "threshold_pct": opts.compact.threshold_pct,
            "retain_pct":    opts.compact.retain_pct,
        },
    })
    .to_string()
}

/// Append the request envelope for the hop about to run, unless the log
/// already ends on an identical one.
///
/// The comparison is against the *log* rather than against a cache: a
/// restart, a fork or a session reloaded from disk must not re-record an
/// envelope that is already there, and the log is the only thing that
/// survives all three.
async fn record_envelope(
    sessions: &Sessions,
    session_id: u64,
    wh: &WireHistory,
    options: &str,
) {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&(wh.envelope, options), &mut hasher);
    let fingerprint = std::hash::Hasher::finish(&hasher);
    {
        let g = sessions.lock().await;
        match g.get(&session_id) {
            Some(log) if log.latest_envelope() == Some(fingerprint) => return,
            Some(_) => {}
            None => return,
        }
    }
    append_event(
        sessions,
        session_id,
        EventKind::RequestEnvelope {
            fingerprint,
            system: wh.system_body.clone(),
            tools: wh.tools_body.clone(),
            options: options.to_string(),
        },
    )
    .await;
}

pub(crate) async fn append_event(
    sessions: &Sessions,
    session_id: u64,
    kind: EventKind,
) -> Option<u64> {
    let mut g = sessions.lock().await;
    let log = g.get_mut(&session_id)?;
    let seq = log.append(kind);
    if let Err(e) = sessions_store::flush(log) {
        warn!(error = %e, session_id, seq, "flush session failed");
    }
    Some(seq)
}

/// Snapshot the volatile runtime facts (time, cwd, os, model, permission
/// mode, plan mode) as a user-role message. The new snapshot shadows its
/// predecessor at the position the first one took, so one copy is ever
/// model-visible and the system-prompt prefix is never touched. Called once
/// per turn — dsh's refresh throttle. When the predecessor was itself
/// shadowed by a compaction, the new snapshot appends fresh instead of
/// replacing a span that is no longer on the surface.
fn append_runtime_context(
    log: &mut SessionLog,
    model: &str,
    perm_mode: PermissionMode,
    plan_active: bool,
) {
    // Elapsed since the newest surface event, so the model can tell how
    // stale its own last message is.
    let entries = log.derive_surface();
    let last_ts = entries.last().and_then(|e| {
        log.events
            .iter()
            .find(|ev| ev.seq == e.seq)
            .map(|ev| ev.ts)
    });
    let mut vars = agents::prompt::standard_vars(model);
    vars.insert("permission".into(), perm_mode.context_line().into());
    vars.insert(
        "plan".into(),
        if plan_active {
            "active — explore only, finish with exit-plan-mode".into()
        } else {
            "off".into()
        },
    );
    if let Some(ts) = last_ts {
        let secs = (chrono::Utc::now().timestamp_millis() - ts).max(0) / 1000;
        vars.insert("elapsed".into(), human_elapsed(secs));
    }
    let content = agents::prompt::runtime_context_text(&vars);
    let prev = entries
        .iter()
        .rev()
        .find(|e| matches!(e.context, Some(ContextSource::RuntimeContext)))
        .map(|e| e.seq);
    let surface = match prev {
        Some(seq) => SurfaceOp::Replace { start_seq: seq, end_seq: seq },
        None      => SurfaceOp::Append,
    };
    log.append(EventKind::ContextInjected {
        surface,
        source: ContextSource::RuntimeContext,
        content,
    });
}

fn human_elapsed(secs: i64) -> String {
    if secs < 60 {
        "less than a minute".into()
    } else if secs < 3600 {
        format!("{} minute(s)", secs / 60)
    } else if secs < 86_400 {
        format!("{} hour(s)", secs / 3600)
    } else {
        format!("{} day(s)", secs / 86_400)
    }
}

/// Reconcile the workspace-instruction snapshot with disk (`agents::
/// instructions`): reload the AGENTS.md/CLAUDE.md chain and, when it differs
/// from what the session last saw, append a replacement that shadows the
/// previous snapshot. Returns `true` when a new snapshot landed. No file
/// watcher — this runs at turn start and after successful filesystem tool
/// calls, which is when edits matter.
fn refresh_instructions(log: &mut SessionLog) -> bool {
    let root = sica_core::paths::working_dir();
    let baseline = agents::instructions::load(&root, &root, agents::instructions::MAX_BYTES);

    // Look at the surface, not the raw log: a predecessor shadowed by a
    // compaction is gone from the model's view and must not be "replaced"
    // (the replacement would land at a dead position).
    let entries = log.derive_surface();
    let prev = entries
        .iter()
        .rev()
        .find(|e| matches!(e.context, Some(ContextSource::Instructions)))
        .map(|e| e.seq);
    let prev_content = prev.and_then(|seq| {
        log.events.iter().find_map(|ev| match &ev.kind {
            EventKind::ContextInjected {
                source: ContextSource::Instructions, content, ..
            } if ev.seq == seq => Some(content.clone()),
            _ => None,
        })
    });

    let content = if baseline.is_empty() {
        // Only supersede when there is a previous snapshot to supersede.
        if prev.is_none() {
            return false;
        }
        "<system-reminder>\nWorkspace instruction files previously loaded \
         are no longer present.\n</system-reminder>"
            .to_string()
    } else {
        agents::instructions::render(&baseline)
    };

    if prev_content.as_deref() == Some(content.as_str()) {
        return false;
    }
    let surface = match prev {
        Some(seq) => SurfaceOp::Replace { start_seq: seq, end_seq: seq },
        None      => SurfaceOp::Append,
    };
    log.append(EventKind::ContextInjected {
        surface,
        source: ContextSource::Instructions,
        content,
    });
    true
}

/// After a successful filesystem-touching skill (`read-file`, `write-file`,
/// `edit-file`), give instruction-file edits a chance to reach the model:
/// reload the chain and replace the snapshot when it changed. This is the
/// reconciliation step — no file watcher, changes surface on the next
/// successful filesystem touch.
async fn reconcile_instructions_after_fs(
    sessions: &Sessions,
    events: &Arc<dyn EventSink>,
    session_id: u64,
) {
    let changed = {
        let mut g = sessions.lock().await;
        let Some(log) = g.get_mut(&session_id) else { return };
        if !refresh_instructions(log) {
            return;
        }
        if let Err(e) = sessions_store::flush(log) {
            warn!(error = %e, session_id, "flush session (after instructions refresh) failed");
        }
        true
    };
    if changed {
        events.emit(Event::LogLine {
            level: "INFO".into(),
            message: "workspace instructions changed on disk — snapshot updated".into(),
        });
    }
}

/// Plan-policy section body for a session in plan mode: the user-editable
/// `skills/plan-mode.md`, falling back to the built-in seed when the file
/// is absent. Loaded per hop so edits apply without a restart.
fn plan_policy_text() -> String {
    std::fs::read_to_string(
        sica_core::paths::skills_dir().join(agents::control::PLAN_MODE_DOC),
    )
    .unwrap_or_else(|_| agents::control::PLAN_MODE_SEED.to_string())
}

/// The parent session's *completed* turns as wire messages — the seed
/// `subagent-fork` hands its child (Wave 4, §12.1).
///
/// Cut at the last `TurnEnd`, so the in-flight turn never crosses: forking
/// mid-turn would hand the child an assistant message whose tool results
/// have not landed yet. The child converses in the text protocol whatever
/// the parent uses, so tool roles are downgraded here too.
async fn fork_seed(sessions: &Sessions, session_id: u64) -> Option<Arc<Vec<ChatMessage>>> {
    let g = sessions.lock().await;
    let log = g.get(&session_id)?;
    let cut = log
        .events
        .iter()
        .rposition(|e| matches!(e.kind, EventKind::TurnEnd { .. }))?;
    let messages = sica_core::event::derive_messages(&log.events[..=cut]);
    if messages.is_empty() {
        return None;
    }
    Some(Arc::new(wire_messages(&messages, false)))
}

/// Whether a skill touches the filesystem in a way that could change the
/// workspace-instruction files.
fn is_fs_skill(name: &str) -> bool {
    matches!(
        name,
        agents::builtins::READ_FILE_NAME
            | agents::builtins::WRITE_FILE_NAME
            | agents::builtins::EDIT_FILE_NAME
    )
}

/// Classify one native call for parallel scheduling (§6.2): known skill,
/// valid JSON args, `Parallel` class, and never a harness-control skill
/// (those mutate the log and run sequentially). Returns the resolved
/// skill + args for the group runner.
fn parallel_call(
    skills: &SkillRegistry,
    call: &agents::turn::NativeToolCall,
) -> Option<(Arc<dyn agents::Skill>, serde_json::Value)> {
    let skill = skills.get(&call.name)?;
    if agents::control::is_control_skill(skill.name()) {
        return None;
    }
    let args: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
    if !matches!(skill.concurrency(&args), agents::Concurrency::Parallel) {
        return None;
    }
    Some((skill, args))
}

/// Latest durable Wave-3 control state from a session's log: the newest
/// `PermissionMode` event wins, the newest `PlanMode` event wins.
fn control_state(log: &SessionLog) -> (PermissionMode, bool, Option<String>, Option<Goal>) {
    let mut mode = PermissionMode::default();
    let mut plan = false;
    let mut preset = None;
    let mut goal = None;
    for ev in &log.events {
        match &ev.kind {
            EventKind::PermissionMode { mode: m } => mode = *m,
            EventKind::PlanMode { active } => plan = *active,
            // Latest wins, and a `None` name is the cleared state — which is
            // why this assigns rather than only overwriting on `Some`.
            EventKind::AgentPreset { name } => preset = name.clone(),
            // Latest wins, exactly like the two above: `GoalChange` is a
            // full snapshot, so the last one is the goal.
            EventKind::GoalChange {
                goal_id, revision, objective, phase, rounds_started, max_rounds, blocker,
            } => {
                goal = Some(Goal {
                    id:             *goal_id,
                    revision:       *revision,
                    objective:      objective.clone(),
                    phase:          *phase,
                    rounds_started: *rounds_started,
                    max_rounds:     *max_rounds,
                    blocker:        blocker.clone(),
                });
            }
            _ => {}
        }
    }
    (mode, plan, preset, goal)
}

/// A goal on the wire. `armed` is process state, not part of the goal, so
/// it is passed in rather than read off it.
fn goal_dump(goal: &Goal, armed: bool) -> protocol::GoalDump {
    protocol::GoalDump {
        id:             goal.id,
        revision:       goal.revision,
        objective:      goal.objective.clone(),
        phase:          goal.phase,
        rounds_started: goal.rounds_started,
        max_rounds:     goal.max_rounds,
        blocker:        goal.blocker.clone(),
        armed,
    }
}

/// Fold the older part of `session_id`'s history into an LLM-written summary
/// and record it as a `CompactionSummary` event that shadows the folded
/// span. Returns `true` when that happened, in which case the caller must
/// rebuild its wire history. The shadowed events stay in the log.
///
/// The summarizer round-trip happens without the sessions lock held, so the
/// log is re-checked before the append: if anything was appended in the
/// meantime the compaction is discarded rather than shadowing a span the
/// summary never saw.
///
/// Emits `ContextCompacting` / `ContextCompacted` so the FE can show the
/// transcript notice, plus a log line either way. On failure the history is
/// left untouched and `trim_to_budget` takes over.
///
/// Before paying for the summariser, the *pruner* runs: every tool result
/// older than the tail and over `compact::PRUNE_THRESHOLD` is replaced (a
/// `Replace { seq, seq }` on its own seq) by its head/tail window — no
/// model call, and often enough on its own, in which case the summary is
/// skipped entirely.
async fn compact_session(
    sessions: &Sessions,
    session_id: u64,
    client: &LlmClient,
    events: &Arc<dyn EventSink>,
    budget: u32,
    policy: &protocol::CompactPolicy,
    wh: &WireHistory,
    native_tools: bool,
    cancel: &CancellationToken,
) -> bool {
    let (entries, last_seq) = {
        let g = sessions.lock().await;
        let Some(log) = g.get(&session_id) else { return false };
        (log.derive_surface(), log.last_seq())
    };
    let snapshot: Vec<Message> = entries.iter().map(|e| e.message.clone()).collect();
    // Everything in the request that is not surface history — the meter's
    // envelope — added back when judging whether pruning alone sufficed.
    let overhead = wh.breakdown.system.saturating_add(wh.breakdown.tools);

    let split = agents::compact::split_index(&snapshot, budget, policy.retain_pct);
    let pruned = prune_tool_results(sessions, session_id, &entries, split, last_seq).await;
    if pruned > 0 {
        events.emit(Event::LogLine {
            level: "INFO".into(),
            message: format!("context: pruned {pruned} oversized older tool result(s) to head/tail windows"),
        });
    }
    let (entries, last_seq, snapshot) = if pruned > 0 {
        let g = sessions.lock().await;
        let Some(log) = g.get(&session_id) else { return false };
        let entries = log.derive_surface();
        let snapshot: Vec<Message> = entries.iter().map(|e| e.message.clone()).collect();
        // Enough on its own? Then the summariser round-trip is not needed.
        let tokens = agents::compact::approx_total(&snapshot).saturating_add(overhead);
        if u64::from(tokens) * 100 < u64::from(budget) * u64::from(policy.threshold_pct) {
            events.emit(Event::ContextCompacted {
                session_id,
                ok: true,
                folded: 0,
                before_tokens: 0,
                after_tokens: tokens,
                summary: String::new(),
                pruned: pruned as u32,
            });
            return true;
        }
        (entries, log.last_seq(), snapshot)
    } else {
        (entries, last_seq, snapshot)
    };

    // Cheap pre-check: if there is nothing foldable, don't announce a
    // compaction that isn't going to happen (a single enormous message, say —
    // that's the trimmer's problem, not ours).
    let Some(split) = agents::compact::split_index(&snapshot, budget, policy.retain_pct) else {
        debug!(session_id, "context over threshold but nothing foldable");
        return pruned > 0;
    };

    let before_tokens = agents::compact::approx_total(&snapshot);
    events.emit(Event::ContextCompacting { session_id });
    events.emit(Event::LogLine {
        level: "INFO".into(),
        message: format!(
            "context: prompt reached {}% of the {budget}-token budget — \
             compressing {before_tokens} tokens of history",
            policy.threshold_pct,
        ),
    });

    let failed = |events: &Arc<dyn EventSink>| {
        events.emit(Event::ContextCompacted {
            session_id,
            ok: false,
            folded: 0,
            before_tokens,
            after_tokens: before_tokens,
            summary: String::new(),
            pruned: pruned as u32,
        });
    };

    // Prefix-preserving summarisation: the conversation's own system prompt
    // (same bytes as the real request) + the folded messages verbatim + the
    // directive as the final user message, so the provider's cache of the
    // last real request is reused.
    let system_wire: Vec<ChatMessage> = if wh.system_body.is_empty() {
        Vec::new()
    } else {
        vec![ChatMessage::text("system", wh.system_body.clone())]
    };
    let folded_wire = wire_messages(&snapshot[..split], native_tools);
    let summary = agents::compact::summarize_fold(
        client,
        policy,
        &system_wire,
        folded_wire,
        Some(cancel.clone()),
    )
    .await;
    let Some(summary) = summary else {
        warn!(session_id, "context compaction produced no summary");
        events.emit(Event::LogLine {
            level: "WARN".into(),
            message: "context: compression failed (summarizer returned nothing usable) \
                      — falling back to trimming the oldest messages"
                .into(),
        });
        failed(events);
        return false;
    };

    let content = agents::compact::summary_message(&summary);
    let after_tokens = {
        let mut after = vec![Message::system(content.clone())];
        after.extend_from_slice(&snapshot[split..]);
        agents::compact::approx_total(&after)
    };
    {
        let mut g = sessions.lock().await;
        let Some(log) = g.get_mut(&session_id) else {
            debug!(session_id, "session vanished during compaction");
            return false;
        };
        if log.last_seq() != last_seq {
            debug!(session_id, "history changed during compaction — discarding it");
            failed(events);
            return false;
        }
        log.append(EventKind::CompactionSummary {
            surface: SurfaceOp::Replace {
                start_seq: entries[0].seq,
                end_seq: entries[split - 1].seq,
            },
            content,
            summary: summary.clone(),
            folded: split as u32,
            before_tokens,
            after_tokens,
        });
        if let Err(e) = sessions_store::flush(log) {
            warn!(error = %e, session_id, "flush session (after compaction) failed");
        }
    }

    events.emit(Event::LogLine {
        level: "INFO".into(),
        message: format!(
            "context: compressed {split} message(s) into a summary — history {before_tokens} \
             → {after_tokens} tokens"
        ),
    });
    events.emit(Event::ContextCompacted {
        session_id,
        ok: true,
        folded: split as u32,
        before_tokens,
        after_tokens,
        summary,
        pruned: pruned as u32,
    });
    true
}

/// Replace every oversized tool result older than the tail with its pruned
/// window. `split` is where the verbatim tail begins (from `split_index`);
/// with no foldable split the last `compact::MIN_TAIL` entries are kept.
/// Returns the number of results pruned. Discarded wholesale if the log
/// moved under us (`last_seq` changed) — the next hop tries again.
async fn prune_tool_results(
    sessions: &Sessions,
    session_id: u64,
    entries: &[sica_core::event::SurfaceEntry],
    split: Option<usize>,
    last_seq: u64,
) -> usize {
    let keep_from = split.unwrap_or_else(|| entries.len().saturating_sub(agents::compact::MIN_TAIL));
    let candidates: Vec<EventKind> = entries[..keep_from]
        .iter()
        .filter_map(|e| {
            let t = e.tool.as_ref()?;
            let pruned = agents::compact::prune_summary(&t.summary)?;
            Some(EventKind::ToolResult {
                surface: SurfaceOp::Replace { start_seq: e.seq, end_seq: e.seq },
                call_seq: t.call_seq,
                skill: t.name.clone(),
                tool_call_id: e.message.tool_call_id.clone(),
                ok: t.ok,
                summary: pruned,
                trusted: t.trusted,
                pruned: true,
            })
        })
        .collect();
    if candidates.is_empty() {
        return 0;
    }
    let mut g = sessions.lock().await;
    let Some(log) = g.get_mut(&session_id) else { return 0 };
    if log.last_seq() != last_seq {
        debug!(session_id, "history changed before pruning — skipping");
        return 0;
    }
    let n = candidates.len();
    for kind in candidates {
        log.append(kind);
    }
    if let Err(e) = sessions_store::flush(log) {
        warn!(error = %e, session_id, "flush session (after pruning) failed");
    }
    n
}

/// Derive `session_id`'s history from its log and assemble the wire form.
/// Returns `None` when the session vanished; `Err` when the prompt failed to
/// assemble (a bad `{{variable}}` reference in `memory.md`), which the
/// caller must surface loudly instead of sending a malformed prompt.
async fn build_history(
    sessions: &Sessions,
    session_id: u64,
    skills: &SkillRegistry,
    mode: protocol::ToolMode,
    model: &str,
    plan_policy: Option<String>,
    persona: Option<&str>,
) -> Result<Option<WireHistory>, agents::prompt::PromptError> {
    let entries = {
        let g = sessions.lock().await;
        let Some(log) = g.get(&session_id) else { return Ok(None) };
        log.derive_surface()
    };
    let messages: Vec<Message> = entries.iter().map(|e| e.message.clone()).collect();
    let mut wh =
        build_wire_history(&messages, skills, mode, model, plan_policy.as_deref(), persona)?;
    wh.entries = entries;
    Ok(Some(wh))
}

/// Everything one hop needs from the assembled prompt: the wire messages,
/// the derived surface they came from (seqs for the meter anchor), and the
/// envelope facts the usage-anchored meter keys on.
pub struct WireHistory {
    pub messages:    Vec<ChatMessage>,
    pub entries:     Vec<SurfaceEntry>,
    /// The composed system-prompt body (empty when nothing was composed).
    pub system_body: String,
    /// The `tools` array as it goes on the wire, pretty-printed. Empty in
    /// text-protocol mode, where the catalogue is inside `system_body`.
    pub tools_body:  String,
    /// Fingerprint of system body + tools array — the meter's anchor key.
    pub envelope:    u64,
    /// Approximate per-part token counts for the status bar.
    pub breakdown:   protocol::TokenBreakdown,
}

/// The `tools` array for one mode. `Text` sends none — the catalogue is in
/// the system prompt. `Native` sends the whole registry. `Ptc` sends only
/// what the model may call directly (guide §7): `run-code`, plus the
/// harness controls whose bodies run in this dispatcher and so cannot run
/// inside a program. Shared by the request builder and the envelope
/// fingerprint so the two can never disagree about what was offered.
fn tools_for(skills: &SkillRegistry, mode: protocol::ToolMode) -> Option<serde_json::Value> {
    match mode {
        protocol::ToolMode::Text => None,
        // `run-code` is only offered where it is the point: under `Native`
        // the model already has every tool directly, and a second way to
        // reach them is pure confusion on the wire.
        protocol::ToolMode::Native => {
            Some(skills.excluding(&[agents::ptc::RUN_CODE_NAME]).tools_json())
        }
        protocol::ToolMode::Ptc => Some(agents::ptc::direct_view(skills).tools_json()),
    }
}

/// Assemble the LLM wire history: compose the system prompt through
/// `agents::prompt` (ordered sections, strict interpolation), then map
/// every derived message to its wire form. Tool-role messages are surfaced
/// to the local server as `user` content so even llama.cpp builds without
/// OpenAI tool-call awareness can read the result.
fn build_wire_history(
    messages: &[Message],
    skills: &SkillRegistry,
    mode: protocol::ToolMode,
    model: &str,
    plan_policy: Option<&str>,
    persona: Option<&str>,
) -> Result<WireHistory, agents::prompt::PromptError> {
    let mem = agents::memory::load(&sica_core::paths::memory_file()).unwrap_or_default();
    let vars = agents::prompt::standard_vars(model);
    let rendered =
        agents::prompt::for_main_agent(&mem, skills, mode, &vars, plan_policy, persona)?;
    let tools_json = tools_for(skills, mode);

    let mut out: Vec<ChatMessage> = Vec::with_capacity(messages.len() + 1);
    if !rendered.system.is_empty() {
        out.push(ChatMessage::text("system", rendered.system.clone()));
    }
    out.extend(wire_messages(messages, mode.native()));

    let breakdown = protocol::TokenBreakdown {
        system:  llm::tokenize::approx_tokens(&rendered.system),
        tools:   tools_json
            .as_ref()
            .map(|t| llm::tokenize::approx_tokens(&t.to_string()))
            .unwrap_or(0),
        history: messages
            .iter()
            .map(|m| llm::tokenize::approx_tokens(&m.content) + 4)
            .sum(),
    };

    let envelope = agents::meter::envelope_hash(&rendered.system, tools_json.as_ref());
    let tools_body = tools_json
        .as_ref()
        .and_then(|t| serde_json::to_string_pretty(t).ok())
        .unwrap_or_default();
    Ok(WireHistory {
        messages: out,
        entries: Vec::new(), // filled by build_history
        system_body: rendered.system,
        tools_body,
        envelope,
        breakdown,
    })
}

/// Wire form of derived messages (no system prompt): role mapping for the
/// text protocol, native `tool_calls` replay in native mode. Shared by the
/// live request builder and the prefix-preserving compaction call so the
/// two always agree on what the server sees.
fn wire_messages(messages: &[Message], native_tools: bool) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        // Text-protocol servers may lack a `tool` role in their template, so
        // tool results are surfaced as `user` there. Native mode keeps the
        // real `tool` role + correlation id the template expects.
        //
        // A derived `system` message — today only a `CompactionSummary` —
        // is downgraded to `user` unconditionally. Every caller splices this
        // output *after* the composed system prompt, so such a message can
        // never be the first on the wire, and several chat templates
        // (Qwen/GLM-family among them) hard-raise "System message must be at
        // the beginning" rather than tolerating a second one. The summary is
        // still recognisable by its `CONTEXT_SUMMARY_PREFIX` marker, and it
        // is stored as `system` in the log either way.
        let role = match m.role {
            Role::Tool if !native_tools => "user",
            Role::System => "user",
            other => role_to_str(other),
        };
        let tool_calls = if native_tools {
            m.tool_calls
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
        } else {
            None
        };
        let tool_call_id = if native_tools {
            m.tool_call_id.clone()
        } else {
            None
        };
        out.push(ChatMessage {
            role: role.into(),
            content: build_chat_content(&m.content, &m.images),
            tool_calls,
            tool_call_id,
        });
    }
    out
}

/// Serialize accumulated native calls into the OpenAI `tool_calls` array
/// shape, stored as a JSON string on the persisted assistant message.
fn native_calls_to_json(calls: &[agents::turn::NativeToolCall]) -> String {
    let arr: Vec<serde_json::Value> = calls
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "type": "function",
                "function": { "name": c.name, "arguments": c.arguments },
            })
        })
        .collect();
    serde_json::to_string(&arr).unwrap_or_else(|_| "[]".into())
}

/// Build the content payload for one persisted `Message`. When no images are
/// attached we send the plain string (max compatibility with text-only
/// servers); otherwise we send the OpenAI-vision `Parts` array with each
/// image inlined as a `data:` URL. Caller should only pass images on user
/// messages — other roles get empty `Vec`.
fn build_chat_content(text: &str, images: &[UserImage]) -> ChatContent {
    if images.is_empty() {
        return ChatContent::Text(text.to_string());
    }
    let mut parts: Vec<ContentPart> = Vec::with_capacity(images.len() + 1);
    if !text.is_empty() {
        parts.push(ContentPart::Text { text: text.to_string() });
    }
    for img in images {
        parts.push(ContentPart::ImageUrl {
            image_url: ImageUrl {
                url: format!("data:{};base64,{}", img.mime, img.data_base64),
            },
        });
    }
    ChatContent::Parts(parts)
}

/// Record the result of the skill invocation logged at `call_seq`. The
/// derived history renders it as a `Tool`-role `tool_result` fenced block
/// (see `sica_core::event::tool_result_message`), framed as untrusted data
/// unless `trusted`.
#[allow(clippy::too_many_arguments)]
async fn append_tool_result(
    sessions: &Sessions,
    session_id: u64,
    call_seq: u64,
    skill: &str,
    tool_call_id: Option<&str>,
    ok: bool,
    summary: &str,
    trusted: bool,
) {
    append_event(sessions, session_id, EventKind::ToolResult {
        surface: SurfaceOp::Append,
        call_seq,
        skill: skill.to_string(),
        tool_call_id: tool_call_id.map(str::to_string),
        ok,
        summary: summary.to_string(),
        trusted,
        pruned: false,
    })
    .await;
}

fn role_to_str(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::Tool => "tool",
    }
}

struct OutSink {
    tx: mpsc::UnboundedSender<Frame>,
}

impl EventSink for OutSink {
    fn emit(&self, ev: Event) {
        let _ = self.tx.send(Frame::event(ev));
    }
}

impl idealist::IdealistEventSink for OutSink {
    fn emit(&self, ev: Event) {
        let _ = self.tx.send(Frame::event(ev));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sica_core::event::{derive_messages, SessionEvent};

    fn registry() -> SkillRegistry {
        SkillRegistry::new()
    }

    /// The ordering rule `start_turn` depends on when re-running an edited
    /// prompt: the rewind is appended *before* the turn's own snapshots, so
    /// the fresh runtime context is not swept away with the span — and it
    /// still lands ahead of the edited message, exactly where an ordinary
    /// send would put it.
    #[test]
    fn a_rewind_keeps_this_turn_s_runtime_context_ahead_of_the_prompt() {
        let mut log = SessionLog::new(1, "t");
        append_runtime_context(&mut log, "m1", PermissionMode::default(), false);
        let edited = log.append(EventKind::UserMessage {
            surface: SurfaceOp::Append,
            content: "first draft".into(),
            images: Vec::new(),
        });
        log.append(EventKind::AssistantMessage {
            surface: SurfaceOp::Append,
            content: "a reply".into(),
            reasoning: None,
            tool_calls: None,
        });

        // What `start_turn` does for `rewind: Some(edited)`.
        log.append(EventKind::Rewind { start_seq: edited, end_seq: log.last_seq() });
        append_runtime_context(&mut log, "m1", PermissionMode::default(), false);
        log.append(EventKind::UserMessage {
            surface: SurfaceOp::Append,
            content: "second draft".into(),
            images: Vec::new(),
        });

        let surface = log.derive_surface();
        assert_eq!(surface.len(), 2, "the first draft and its reply are gone");
        assert!(matches!(
            surface[0].context,
            Some(ContextSource::RuntimeContext)
        ));
        assert_eq!(surface[1].message.content, "second draft");
    }

    #[test]
    fn wire_history_downgrades_tool_role_in_text_mode() {
        let msgs = vec![
            Message::user("hi"),
            Message {
                role: Role::Tool,
                content: "```tool_result\n{}\n```".into(),
                reasoning: None,
                images: Vec::new(),
                tool_calls: None,
                tool_call_id: Some("c1".into()),
            },
        ];
        let wire = build_wire_history(&msgs, &registry(), protocol::ToolMode::Text, "test", None, None).unwrap().messages;
        // memory.md may or may not exist on this machine; look at the tail.
        let n = wire.len();
        assert_eq!(wire[n - 2].role, "user");
        assert_eq!(wire[n - 1].role, "user");
        assert!(wire[n - 1].tool_call_id.is_none());
        assert!(wire[n - 1].tool_calls.is_none());
    }

    /// A compaction summary derives as a `system` message and is always
    /// spliced after the composed system prompt, so it must not go out as a
    /// second `system` — templates that require the system message to lead
    /// reject the whole request with a 400.
    #[test]
    fn wire_history_never_emits_a_second_system_message() {
        let msgs = vec![
            Message::system(agents::compact::summary_message("folded")),
            Message::user("continue"),
        ];
        for native in [protocol::ToolMode::Text, protocol::ToolMode::Native] {
            let wire = build_wire_history(&msgs, &registry(), native, "test", None, None)
                .unwrap()
                .messages;
            assert!(
                wire.iter().skip(1).all(|m| m.role != "system"),
                "{}: a non-leading system message reached the wire",
                native.label()
            );
            let n = wire.len();
            assert_eq!(wire[n - 2].role, "user");
            assert!(wire[n - 2]
                .content
                .text()
                .starts_with(protocol::CONTEXT_SUMMARY_PREFIX));
        }
    }

    #[test]
    fn wire_history_replays_native_tool_calls() {
        let msgs = vec![
            Message {
                role: Role::Assistant,
                content: String::new(),
                reasoning: None,
                images: Vec::new(),
                tool_calls: Some(r#"[{"id":"c1","type":"function","function":{"name":"run-cli","arguments":"{}"}}]"#.into()),
                tool_call_id: None,
            },
            Message {
                role: Role::Tool,
                content: "out".into(),
                reasoning: None,
                images: Vec::new(),
                tool_calls: None,
                tool_call_id: Some("c1".into()),
            },
        ];
        let wire = build_wire_history(&msgs, &registry(), protocol::ToolMode::Native, "test", None, None).unwrap().messages;
        let n = wire.len();
        assert_eq!(wire[n - 2].role, "assistant");
        assert!(wire[n - 2].tool_calls.is_some());
        assert_eq!(wire[n - 1].role, "tool");
        assert_eq!(wire[n - 1].tool_call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn wire_history_inlines_images_as_parts() {
        let msgs = vec![Message::user_with_images(
            "look",
            vec![UserImage { mime: "image/png".into(), data_base64: "AAAA".into() }],
        )];
        let wire = build_wire_history(&msgs, &registry(), protocol::ToolMode::Text, "test", None, None).unwrap().messages;
        let last = wire.last().unwrap();
        match &last.content {
            ChatContent::Parts(parts) => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(&parts[1], ContentPart::ImageUrl { image_url } if image_url.url.starts_with("data:image/png;base64,")));
            }
            other => panic!("expected parts, got {other:?}"),
        }
    }

    #[test]
    fn wire_history_native_keeps_memory_and_drops_catalogue() {
        let wire_text = build_wire_history(&[Message::user("hi")], &registry(), protocol::ToolMode::Text, "test", None, None).unwrap();
        let wire_native = build_wire_history(&[Message::user("hi")], &registry(), protocol::ToolMode::Native, "test", None, None).unwrap();
        let sys_native = &wire_native.system_body;
        assert!(sys_native.contains(agents::prompt::NATIVE_IDENTITY), "{sys_native}");
        assert!(!sys_native.contains("## Loaded skills"), "tools array carries the catalogue");
        assert!(!wire_text.system_body.contains(agents::prompt::NATIVE_IDENTITY));
        // The runtime snapshot never leaks into the system body.
        assert!(!sys_native.contains(agents::prompt::RUNTIME_CONTEXT_HEADER));
    }

    #[test]
    fn wire_history_breakdown_covers_all_three_parts() {
        let wh = build_wire_history(&[Message::user("hi")], &registry(), protocol::ToolMode::Text, "test", None, None).unwrap();
        assert!(wh.breakdown.history >= 5, "user message priced");
        assert_eq!(wh.breakdown.tools, 0, "text protocol sends no tools array");
        assert!(wh.envelope != 0);
    }

    #[test]
    fn runtime_snapshot_shadows_its_predecessor() {
        let mut log = SessionLog::new(1, "t");
        append_runtime_context(&mut log, "test-model", PermissionMode::WorkspaceWrite, false);
        log.append(EventKind::UserMessage {
            surface: SurfaceOp::Append,
            content: "hi".into(),
            images: Vec::new(),
        });
        append_runtime_context(&mut log, "test-model", PermissionMode::WorkspaceWrite, false);
        let entries = log.derive_surface();
        let snaps: Vec<_> = entries
            .iter()
            .filter(|e| matches!(e.context, Some(ContextSource::RuntimeContext)))
            .collect();
        assert_eq!(snaps.len(), 1, "one snapshot is ever model-visible");
        assert!(snaps[0].message.content.contains("Model: test-model"));
        assert!(snaps[0].message.content.starts_with(agents::prompt::RUNTIME_CONTEXT_HEADER));
        // It landed where the first one stood — before the user message.
        assert_eq!(entries.last().unwrap().message.content, "hi");
    }

    #[test]
    fn runtime_snapshot_after_compaction_appends_fresh() {
        let mut log = SessionLog::new(1, "t");
        append_runtime_context(&mut log, "m", PermissionMode::WorkspaceWrite, false); // seq 2
        log.append(EventKind::UserMessage { surface: SurfaceOp::Append, content: "u1".into(), images: Vec::new() });
        log.append(EventKind::AssistantMessage { surface: SurfaceOp::Append, content: "a1".into(), reasoning: None, tool_calls: None });
        // Compaction shadows the snapshot along with the early messages.
        let entries = log.derive_surface();
        log.append(EventKind::CompactionSummary {
            surface: SurfaceOp::Replace { start_seq: entries[0].seq, end_seq: entries.last().unwrap().seq },
            content: agents::compact::summary_message("S"),
            summary: "S".into(),
            folded: 3,
            before_tokens: 0,
            after_tokens: 0,
        });
        append_runtime_context(&mut log, "m", PermissionMode::WorkspaceWrite, false);
        let after = log.derive_surface();
        let snaps: Vec<_> = after
            .iter()
            .filter(|e| matches!(e.context, Some(ContextSource::RuntimeContext)))
            .collect();
        assert_eq!(snaps.len(), 1);
        // Appended fresh (newest entry), not resurrected at the dead span.
        assert_eq!(after.last().unwrap().seq, snaps[0].seq);
    }

    /// The Replace fold must produce exactly what the old in-place splice
    /// (`summary + messages[split..]`) produced.
    #[test]
    fn compaction_replace_matches_legacy_splice() {
        let mut log = SessionLog::new(1, "t");
        let texts = ["a", "b", "c", "d", "e", "f"];
        for (i, t) in texts.iter().enumerate() {
            if i % 2 == 0 {
                log.append(EventKind::UserMessage {
                    surface: SurfaceOp::Append,
                    content: (*t).into(),
                    images: Vec::new(),
                });
            } else {
                log.append(EventKind::AssistantMessage {
                    surface: SurfaceOp::Append,
                    content: (*t).into(),
                    reasoning: None,
                    tool_calls: None,
                });
            }
        }
        let entries = log.derive_surface();
        let snapshot: Vec<Message> = entries.iter().map(|e| e.message.clone()).collect();
        let split = 4;
        let content = agents::compact::summary_message("S");
        let mut legacy = vec![Message::system(content.clone())];
        legacy.extend_from_slice(&snapshot[split..]);

        log.append(EventKind::CompactionSummary {
            surface: SurfaceOp::Replace { start_seq: entries[0].seq, end_seq: entries[split - 1].seq },
            content,
            summary: "S".into(),
            folded: split as u32,
            before_tokens: 0,
            after_tokens: 0,
        });
        let events: &[SessionEvent] = &log.events;
        assert_eq!(derive_messages(events), legacy);
    }

    fn tool_pair(log: &mut SessionLog, skill: &str, summary: &str) {
        let call_seq = log.append(EventKind::ToolCall {
            name: skill.into(),
            args_preview: format!("{skill} 'x'"),
            expectation: String::new(),
            call_id: None,
            args_json: None,
        });
        log.append(EventKind::ToolResult {
            surface: SurfaceOp::Append,
            call_seq,
            skill: skill.into(),
            tool_call_id: None,
            ok: true,
            summary: summary.into(),
            trusted: false,
            pruned: false,
        });
    }

    #[tokio::test]
    async fn pruner_replaces_old_big_results_and_leaves_the_tail() {
        let mut log = SessionLog::new(1, "t");
        let big = "b".repeat(agents::compact::PRUNE_THRESHOLD + 100);
        log.append(EventKind::UserMessage { surface: SurfaceOp::Append, content: "u1".into(), images: Vec::new() });
        log.append(EventKind::AssistantMessage { surface: SurfaceOp::Append, content: "a1".into(), reasoning: None, tool_calls: None });
        tool_pair(&mut log, "run-cli", &big);
        tool_pair(&mut log, "read-file", "small");
        log.append(EventKind::AssistantMessage { surface: SurfaceOp::Append, content: "a2".into(), reasoning: None, tool_calls: None });
        log.append(EventKind::UserMessage { surface: SurfaceOp::Append, content: "u2".into(), images: Vec::new() });
        log.append(EventKind::AssistantMessage { surface: SurfaceOp::Append, content: "a3".into(), reasoning: None, tool_calls: None });
        tool_pair(&mut log, "run-cli", &big); // in the tail: must survive
        let entries = log.derive_surface();
        let last_seq = log.last_seq();
        let n = entries.len();

        let sessions: Sessions = Arc::new(Mutex::new(HashMap::from([(1u64, log)])));
        // Tail = the last two entries (a3 + the recent big result).
        let pruned = prune_tool_results(&sessions, 1, &entries, Some(n - 2), last_seq).await;
        assert_eq!(pruned, 1);

        let g = sessions.lock().await;
        let after = g[&1].derive_surface();
        assert_eq!(after.len(), n, "pruning replaces, never removes");
        let old = after.iter().find(|e| e.tool.as_ref().is_some_and(|t| t.pruned)).unwrap();
        assert!(old.tool.as_ref().unwrap().summary.len() <= agents::compact::PRUNE_THRESHOLD);
        assert_eq!(old.tool.as_ref().unwrap().name, "run-cli");
        assert!(!old.tool.as_ref().unwrap().trusted, "trust flag carries over");
        let recent = after.last().unwrap().tool.as_ref().unwrap();
        assert!(!recent.pruned);
        assert_eq!(recent.summary.len(), big.len());
        // Idempotent: a second pass finds nothing.
        drop(g);
        let entries = sessions.lock().await[&1].derive_surface();
        let last_seq = sessions.lock().await[&1].last_seq();
        assert_eq!(prune_tool_results(&sessions, 1, &entries, Some(n - 2), last_seq).await, 0);
    }

    #[tokio::test]
    async fn pruner_backs_off_when_the_log_moved() {
        let mut log = SessionLog::new(1, "t");
        let big = "b".repeat(agents::compact::PRUNE_THRESHOLD + 1);
        tool_pair(&mut log, "run-cli", &big);
        log.append(EventKind::UserMessage { surface: SurfaceOp::Append, content: "u".into(), images: Vec::new() });
        log.append(EventKind::AssistantMessage { surface: SurfaceOp::Append, content: "a".into(), reasoning: None, tool_calls: None });
        let entries = log.derive_surface();
        let stale_seq = log.last_seq() - 1;
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::from([(1u64, log)])));
        assert_eq!(prune_tool_results(&sessions, 1, &entries, Some(1), stale_seq).await, 0);
    }

    // ---- Wave 3 control plane ----

    struct Capture(std::sync::Mutex<Vec<Event>>);
    impl EventSink for Capture {
        fn emit(&self, ev: Event) {
            self.0.lock().unwrap().push(ev);
        }
    }

    fn control() -> (ControlState, Sessions, Arc<Capture>) {
        control_as(TurnSource::Human)
    }

    /// A control plane whose turn was opened by `source` — the goal skills'
    /// authority check reads exactly that.
    fn control_as(source: TurnSource) -> (ControlState, Sessions, Arc<Capture>) {
        let cap = Arc::new(Capture(std::sync::Mutex::new(Vec::new())));
        let cs = ControlState {
            events: cap.clone(),
            failure_sink: None,
            permissions: Arc::new(Mutex::new(HashMap::new())),
            plans: Arc::new(Mutex::new(HashMap::new())),
            repeat: Arc::new(Mutex::new(HashMap::new())),
            read_seen: Arc::new(Mutex::new(HashMap::new())),
            brokers: Arc::new(BrokerSet::new()),
            goals: Arc::new(Mutex::new(HashMap::new())),
            arm_set: Arc::new(Mutex::new(HashSet::new())),
            next_goal: Arc::new(AtomicU64::new(1)),
            turn_source: source,
            hooks: Arc::new(crate::hooks::HookConfig::default()),
        };
        (cs, Arc::new(Mutex::new(HashMap::new())), cap)
    }

    async fn with_log(sessions: &Sessions, id: u64) {
        sessions.lock().await.insert(id, SessionLog::new(id, default_title(id)));
    }

    #[test]
    fn control_state_restores_latest_events() {
        let mut log = SessionLog::new(1, "t");
        log.append(EventKind::PermissionMode { mode: PermissionMode::ReadOnly });
        log.append(EventKind::PlanMode { active: true });
        log.append(EventKind::PermissionMode { mode: PermissionMode::DangerFullAccess });
        log.append(EventKind::PlanMode { active: false });
        log.append(EventKind::PlanMode { active: true });
        let (mode, plan, agent, goal) = control_state(&log);
        assert!(agent.is_none());
        assert_eq!(mode, PermissionMode::DangerFullAccess);
        assert!(plan);
        assert!(goal.is_none());
    }

    #[test]
    fn control_state_restores_the_latest_goal_snapshot() {
        let mut log = SessionLog::new(1, "t");
        let snapshot = |rev: u32, rounds: u32, phase| EventKind::GoalChange {
            goal_id: 7,
            revision: rev,
            objective: "ship it".into(),
            phase,
            rounds_started: rounds,
            max_rounds: 8,
            blocker: None,
        };
        log.append(snapshot(1, 0, protocol::GoalPhase::Active));
        log.append(snapshot(2, 1, protocol::GoalPhase::Active));
        log.append(snapshot(3, 1, protocol::GoalPhase::Paused));
        let (_, _, _, goal) = control_state(&log).clone();
        let goal = goal.expect("goal restored");
        assert_eq!((goal.id, goal.revision, goal.rounds_started), (7, 3, 1));
        assert_eq!(goal.phase, protocol::GoalPhase::Paused);
    }

    #[tokio::test]
    async fn todo_write_control_validates_and_emits() {
        let (cs, sessions, cap) = control();
        with_log(&sessions, 1).await;
        let cancel = CancellationToken::new();
        let args = serde_json::json!({"items": "[{\"content\": \"a\", \"status\": \"pending\"}]"});
        let (out, conclude) = cs
            .handle_control(&sessions, "todo-write", &args, "todo-write", "", 1, 0, &cancel)
            .await;
        assert!(out.ok, "{}", out.summary);
        assert!(!conclude);
        assert!(out.summary.contains("1 items"));
        let evs = cap.0.lock().unwrap();
        assert!(evs.iter().any(|e| matches!(e, Event::TodosChanged { session_id: 1, .. })));
        assert!(evs.iter().any(|e| matches!(e, Event::ToolCallFinished { ok: true, .. })));
        drop(evs);
        let bad = serde_json::json!({"items": "[]"});
        let (out, _) = cs.handle_control(&sessions, "todo-write", &bad, "", "", 1, 0, &cancel).await;
        assert!(!out.ok);
        let g = sessions.lock().await;
        let todos = g[&1].events.iter().filter(|e| matches!(e.kind, EventKind::TodoWrite { .. })).count();
        assert_eq!(todos, 1);
    }

    #[tokio::test]
    async fn unrun_native_calls_still_get_a_result_each() {
        // `exit-plan-mode` ends the turn mid-batch. Every id the assistant
        // message carried must still be answered, or the next request
        // replays `tool_calls` with no matching `tool` messages.
        let (cs, sessions, _) = control();
        with_log(&sessions, 1).await;
        let calls = vec![
            agents::turn::NativeToolCall {
                id: "c1".into(),
                name: "read-file".into(),
                arguments: r#"{"path":"a"}"#.into(),
            },
            agents::turn::NativeToolCall {
                id: "c2".into(),
                name: "glob".into(),
                arguments: "{}".into(),
            },
        ];
        cs.answer_unrun(&sessions, 1, &calls, "not executed").await;
        let g = sessions.lock().await;
        let answered: Vec<String> = g[&1]
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::ToolResult { tool_call_id: Some(id), ok: false, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(answered, vec!["c1".to_string(), "c2".to_string()]);
    }

    #[tokio::test]
    async fn exit_plan_mode_rejects_outside_plan_mode() {
        let (cs, sessions, _) = control();
        with_log(&sessions, 1).await;
        let cancel = CancellationToken::new();
        let args = serde_json::json!({"plan": "# P"});
        let (out, conclude) = cs
            .handle_control(&sessions, "exit-plan-mode", &args, "", "", 1, 0, &cancel)
            .await;
        assert!(!out.ok);
        assert!(!conclude);
        assert!(out.summary.contains("outside plan mode"));
    }

    #[tokio::test]
    async fn exit_plan_mode_approve_leaves_plan_mode_and_concludes() {
        let (cs, sessions, cap) = control();
        with_log(&sessions, 1).await;
        cs.plans.lock().await.insert(1, true);
        let cancel = CancellationToken::new();
        let args = serde_json::json!({"plan": "# P"});
        let (res, _) = tokio::join!(
            cs.handle_control(&sessions, "exit-plan-mode", &args, "", "", 1, 0, &cancel),
            async {
                for _ in 0..200 {
                    if cs.brokers.answer_question(1, "Approve".into()).await {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }
        );
        let (out, conclude) = res;
        assert!(out.ok, "{}", out.summary);
        assert!(conclude);
        assert!(!cs.plans.lock().await.get(&1).copied().unwrap_or(true));
        let g = sessions.lock().await;
        assert!(g[&1].events.iter().any(|e| matches!(e.kind, EventKind::PlanMode { active: false })));
        let evs = cap.0.lock().unwrap();
        assert!(evs.iter().any(|e| matches!(e, Event::PlanModeChanged { session_id: 1, active: false })));
    }

    #[tokio::test]
    async fn exit_plan_mode_keep_planning_stays() {
        let (cs, sessions, _) = control();
        with_log(&sessions, 1).await;
        cs.plans.lock().await.insert(1, true);
        let cancel = CancellationToken::new();
        let args = serde_json::json!({"plan": "# P"});
        let (res, _) = tokio::join!(
            cs.handle_control(&sessions, "exit-plan-mode", &args, "", "", 1, 0, &cancel),
            async {
                for _ in 0..200 {
                    if cs.brokers.answer_question(1, "needs more detail".into()).await {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }
        );
        let (out, conclude) = res;
        assert!(!out.ok);
        assert!(!conclude);
        assert!(cs.plans.lock().await.get(&1).copied().unwrap_or(false));
    }

    /// A preset directory for one test, gone when it ends.
    fn preset_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sica-chat-agents-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("reviewer.md"),
            "---
name: reviewer
description: d
skills: [read-file, grep]
---
BE A REVIEWER
",
        )
        .unwrap();
        dir
    }

    /// Selecting a preset is durable, restricts the registry the turn
    /// dispatches against, and supplies the persona section — all from the
    /// one selection, so the prompt cannot advertise a hidden skill.
    #[tokio::test]
    async fn selecting_an_agent_sets_persona_and_narrows_the_registry() {
        let dir = preset_dir("select");
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(agents::RunCli(None)));
        reg.register(Arc::new(agents::ReadFile::new(std::path::PathBuf::from("."))));
        reg.register(Arc::new(agents::Grep::new(std::path::PathBuf::from("."))));
        let hub = ChatHub::new(tx, Arc::new(reg), None);
        let id = hub.create_session().await;

        let msg = hub
            .set_session_agent_in(&dir, id, Some("reviewer".into()))
            .await
            .expect("preset accepted");
        assert!(msg.contains("reviewer"), "{msg}");
        assert_eq!(hub.presets.lock().await.get(&id).cloned(), Some("reviewer".into()));

        let (skills, persona) = hub.effective_agent_in(&dir, id).await;
        assert_eq!(persona.as_deref(), Some("BE A REVIEWER"));
        assert!(skills.by_name.contains_key("read-file"));
        assert!(skills.by_name.contains_key("grep"));
        assert!(!skills.by_name.contains_key("run-cli"), "unlisted skills are hidden");

        // Durable, and the dump carries it to the frontend.
        let g = hub.sessions.lock().await;
        let log = g.get(&id).unwrap();
        assert!(log.events.iter().any(|e| matches!(
            &e.kind,
            EventKind::AgentPreset { name } if name.as_deref() == Some("reviewer")
        )));
        assert_eq!(control_state(log).2.as_deref(), Some("reviewer"));
        drop(g);
        assert_eq!(hub.dump_session(id).await.unwrap().agent.as_deref(), Some("reviewer"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The selection is fixed once the session has produced a reply: the
    /// persona lives in the system-prompt prefix, and swapping it would
    /// leave the earlier half of the transcript answering to rules that no
    /// longer apply.
    #[tokio::test]
    async fn the_agent_is_fixed_after_the_first_reply() {
        let dir = preset_dir("fixed");
        let (hub, _rx) = hub();
        let id = hub.create_session().await;
        hub.set_session_agent_in(&dir, id, Some("reviewer".into())).await.unwrap();
        {
            let mut g = hub.sessions.lock().await;
            g.get_mut(&id).unwrap().append(EventKind::AssistantMessage {
                surface:    SurfaceOp::Append,
                content:    "answered".into(),
                reasoning:  None,
                tool_calls: None,
            });
        }
        let err = hub.set_session_agent_in(&dir, id, None).await.unwrap_err();
        assert!(err.contains("fixed once"), "{err}");
        assert_eq!(hub.presets.lock().await.get(&id).cloned(), Some("reviewer".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A name with nothing behind it leaves the session exactly as it was —
    /// validation happens before any state moves.
    #[tokio::test]
    async fn an_unknown_or_unsafe_agent_is_refused_without_side_effects() {
        let dir = preset_dir("refuse");
        let (hub, _rx) = hub();
        let id = hub.create_session().await;
        for bad in ["ghost", "../secrets"] {
            let err = hub
                .set_session_agent_in(&dir, id, Some(bad.into()))
                .await
                .unwrap_err();
            assert!(!err.is_empty(), "{bad}");
        }
        assert!(hub.presets.lock().await.get(&id).is_none());
        let g = hub.sessions.lock().await;
        assert!(!g
            .get(&id)
            .unwrap()
            .events
            .iter()
            .any(|e| matches!(e.kind, EventKind::AgentPreset { .. })));
        drop(g);
        // A preset that vanishes under a live session degrades to the
        // default rather than failing the turn.
        hub.set_session_agent_in(&dir, id, Some("reviewer".into())).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let (skills, persona) = hub.effective_agent_in(&dir, id).await;
        assert!(persona.is_none());
        assert_eq!(skills.by_name.len(), hub.skills.by_name.len());
    }

    /// `/agent off` clears the selection; `/agent` with no argument reports.
    #[tokio::test]
    async fn the_agent_command_reports_and_clears() {
        let dir = preset_dir("command");
        let (hub, _rx) = hub();
        let id = hub.create_session().await;
        hub.set_session_agent_in(&dir, id, Some("reviewer".into())).await.unwrap();
        let (ok, text) = hub.command_agent(id, "off").await;
        assert!(ok, "{text}");
        assert!(hub.presets.lock().await.get(&id).is_none());
        let (ok, text) = hub.command_agent(id, "").await;
        assert!(ok);
        assert!(text.contains("no agent selected"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn hub() -> (ChatHub, mpsc::UnboundedReceiver<Frame>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let hub = ChatHub::new(tx, Arc::new(SkillRegistry::new()), None);
        (hub, rx)
    }

    /// Search reads the derived surface, so it finds what was said and
    /// skips the fenced tool blocks and injected snapshots around it.
    /// Archived sessions are out of both the list and the results.
    #[tokio::test]
    async fn search_matches_messages_and_skips_archived() {
        let (hub, _) = hub();
        {
            let mut g = hub.sessions.lock().await;
            let mut a = SessionLog::new(101, "Renderer work");
            a.append(EventKind::UserMessage {
                surface: SurfaceOp::Append,
                content: "the frobnicator is misaligned".into(),
                images: Vec::new(),
            });
            let mut b = SessionLog::new(102, "Archived one");
            b.append(EventKind::UserMessage {
                surface: SurfaceOp::Append,
                content: "the frobnicator again".into(),
                images: Vec::new(),
            });
            b.append(EventKind::SessionArchived);
            let mut c = SessionLog::new(103, "Frobnicator by title");
            c.append(EventKind::UserMessage {
                surface: SurfaceOp::Append,
                content: "nothing to see".into(),
                images: Vec::new(),
            });
            g.insert(101, a);
            g.insert(102, b);
            g.insert(103, c);
        }

        let hits = hub.search_sessions("frobnicator").await;
        let ids: Vec<u64> = hits.iter().map(|h| h.id).collect();
        assert!(ids.contains(&101), "content match missing: {ids:?}");
        assert!(ids.contains(&103), "title match missing: {ids:?}");
        assert!(!ids.contains(&102), "archived session must not be searchable");
        // A content hit explains itself; a title hit needs no snippet.
        let content_hit = hits.iter().find(|h| h.id == 101).unwrap();
        assert!(content_hit.snippet.contains("frobnicator"), "{:?}", content_hit.snippet);
        assert!(hits.iter().find(|h| h.id == 103).unwrap().snippet.is_empty());
        // An empty query is not "match everything".
        assert!(hub.search_sessions("   ").await.is_empty());

        let listed: Vec<u64> = hub.list_sessions().await.iter().map(|s| s.id).collect();
        assert!(listed.contains(&101));
        assert!(!listed.contains(&102), "archived session must leave the list");
    }

    #[tokio::test]
    async fn permission_mode_sets_once_and_reports() {
        let (hub, _) = hub();
        assert!(!hub.set_permission_mode(99, PermissionMode::ReadOnly).await, "unknown session");
        let id = hub.create_session().await;
        assert!(hub.set_permission_mode(id, PermissionMode::ReadOnly).await);
        assert!(!hub.set_permission_mode(id, PermissionMode::ReadOnly).await, "unchanged: no-op");
        assert!(hub.set_permission_mode(id, PermissionMode::DangerFullAccess).await);
        let g = hub.sessions.lock().await;
        let modes = g[&id]
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::PermissionMode { mode } => Some(*mode),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(modes, vec![PermissionMode::ReadOnly, PermissionMode::DangerFullAccess]);
    }

    #[tokio::test]
    async fn run_command_plan_and_permission() {
        let (hub, _) = hub();
        let id = hub.create_session().await;
        let text = hub.run_command(id, "plan", "on").await;
        assert!(text.contains("plan mode on"), "{text}");
        assert!(hub.plans.lock().await.get(&id).copied().unwrap_or(false));
        let text = hub.run_command(id, "plan", "").await;
        assert!(text.contains("plan mode off"), "{text}");
        let text = hub.run_command(id, "permission", "read-only").await;
        assert!(text.contains("read-only"), "{text}");
        let text = hub.run_command(id, "permission", "nuke").await;
        assert!(text.contains("unknown mode"), "{text}");
        let text = hub.run_command(id, "frobnicate", "").await;
        assert!(text.contains("unknown command"), "{text}");
        let text = hub.run_command(4242, "plan", "on").await;
        assert!(text.contains("unknown session"), "{text}");
        let g = hub.sessions.lock().await;
        let cmds = g[&id].events.iter().filter(|e| matches!(e.kind, EventKind::Command { .. })).count();
        assert_eq!(cmds, 5);
    }

    #[test]
    fn parallel_grouping_classifies_by_skill_and_args() {
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(agents::ReadFile::new(std::env::temp_dir())));
        reg.register(Arc::new(agents::RunCli(None)));
        let call = |name: &str, args: &str| agents::turn::NativeToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: args.into(),
        };
        assert!(parallel_call(&reg, &call("read-file", "{\"path\": \"a\"}")).is_some());
        assert!(parallel_call(&reg, &call("run-cli", "{\"command\": \"git status\"}")).is_some());
        assert!(parallel_call(&reg, &call("run-cli", "{\"command\": \"cargo build\"}")).is_none());
        assert!(parallel_call(&reg, &call("nope", "{}")).is_none());
        assert!(parallel_call(&reg, &call("read-file", "not json")).is_none());
    }

    /// Run one control skill through the harness path the dispatcher uses.
    async fn control_call(
        cs: &ControlState,
        sessions: &Sessions,
        name: &str,
        args: serde_json::Value,
    ) -> agents::SkillOutcome {
        let cancel = CancellationToken::new();
        cs.handle_control_body(sessions, name, &args, 1, &cancel).await.0
    }

    #[tokio::test]
    async fn creating_a_goal_persists_it_arms_it_and_pushes_it() {
        let (cs, sessions, cap) = control();
        with_log(&sessions, 1).await;
        let out = control_call(
            &cs,
            &sessions,
            agents::goal::CREATE_GOAL_NAME,
            serde_json::json!({ "objective": "make the tests pass", "max_rounds": "3" }),
        )
        .await;
        assert!(out.ok, "{}", out.summary);
        assert!(out.summary.contains("make the tests pass"));

        let goal = cs.goal(1).await.expect("goal stored");
        assert_eq!(goal.max_rounds, 3);
        assert!(cs.armed(1).await, "a goal set by a human is armed by that instruction");

        // Durable, and visible to the FE.
        let g = sessions.lock().await;
        assert!(g[&1]
            .events
            .iter()
            .any(|e| matches!(e.kind, EventKind::GoalChange { .. })));
        drop(g);
        let events = cap.0.lock().unwrap();
        assert!(events.iter().any(|e| matches!(e, Event::GoalChanged { .. })));
    }

    #[tokio::test]
    async fn an_automatic_round_cannot_set_a_goal() {
        let (cs, sessions, _) = control_as(TurnSource::GoalRound);
        with_log(&sessions, 1).await;
        let out = control_call(
            &cs,
            &sessions,
            agents::goal::CREATE_GOAL_NAME,
            serde_json::json!({ "objective": "something else entirely" }),
        )
        .await;
        assert!(!out.ok);
        assert!(out.summary.contains("only the user"), "{}", out.summary);
        assert!(cs.goal(1).await.is_none());
    }

    #[tokio::test]
    async fn a_second_goal_is_refused_while_the_first_is_live() {
        let (cs, sessions, _) = control();
        with_log(&sessions, 1).await;
        let mk = |o: &str| serde_json::json!({ "objective": o });
        assert!(control_call(&cs, &sessions, agents::goal::CREATE_GOAL_NAME, mk("first")).await.ok);
        let out = control_call(&cs, &sessions, agents::goal::CREATE_GOAL_NAME, mk("second")).await;
        assert!(!out.ok);
        assert!(out.summary.contains("already has a goal"), "{}", out.summary);
    }

    #[tokio::test]
    async fn update_goal_enforces_the_revision_and_disarms_on_completion() {
        let (cs, sessions, _) = control();
        with_log(&sessions, 1).await;
        control_call(
            &cs,
            &sessions,
            agents::goal::CREATE_GOAL_NAME,
            serde_json::json!({ "objective": "ship it" }),
        )
        .await;

        // A stale revision is refused rather than applied.
        let stale = control_call(
            &cs,
            &sessions,
            agents::goal::UPDATE_GOAL_NAME,
            serde_json::json!({ "revision": "99", "action": "complete", "note": "done" }),
        )
        .await;
        assert!(!stale.ok);
        assert!(stale.summary.contains("revision mismatch"), "{}", stale.summary);
        assert!(cs.armed(1).await, "a refused update changes nothing");

        let ok = control_call(
            &cs,
            &sessions,
            agents::goal::UPDATE_GOAL_NAME,
            serde_json::json!({ "revision": "1", "action": "complete", "note": "tests green" }),
        )
        .await;
        assert!(ok.ok, "{}", ok.summary);
        assert_eq!(cs.goal(1).await.unwrap().phase, protocol::GoalPhase::Completed);
        assert!(!cs.armed(1).await, "a completed goal opens no more rounds");
    }

    #[tokio::test]
    async fn get_goal_reports_the_revision_update_goal_needs() {
        let (cs, sessions, _) = control();
        with_log(&sessions, 1).await;
        assert!(!control_call(&cs, &sessions, agents::goal::GET_GOAL_NAME, serde_json::json!({}))
            .await
            .ok, "no goal yet");
        control_call(
            &cs,
            &sessions,
            agents::goal::CREATE_GOAL_NAME,
            serde_json::json!({ "objective": "x" }),
        )
        .await;
        let out =
            control_call(&cs, &sessions, agents::goal::GET_GOAL_NAME, serde_json::json!({})).await;
        assert!(out.ok);
        assert!(out.summary.contains("rev 1"), "{}", out.summary);
        assert!(out.summary.contains("rounds armed: true"), "{}", out.summary);
    }
}
