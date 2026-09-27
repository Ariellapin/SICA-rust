//! The memory keeper: brings each session's memory up to date in the
//! background, and promotes what should outlive the session into long-term
//! memory.
//!
//! When a session goes idle a short timer starts (`idle_seconds` in
//! `sica-settings/memory.toml`, 45 s by default). If nothing happens before
//! it fires, the session is queued; the worker reads the part of the
//! conversation its memory has not covered yet (`through_seq` onwards) and
//! asks the model for an updated summary, key facts, and up to three facts
//! worth keeping for later sessions. The result lands as one
//! `SessionMemory { author: "auto" }` row; the promotions go to the
//! long-term store, deduplicated.
//!
//! The rules are the investigator's (`crate::investigate`), for the same
//! reasons:
//!
//! - **Never compete with the person.** A local LLM serves one request at a
//!   time. A pass starts only when no turn is running anywhere, and any turn
//!   starting cancels it; the session is queued again unless it was that
//!   session's own turn (whose end re-arms the timer anyway).
//! - **Never clobber a newer write.** The pass remembers which memory row it
//!   started from. If the model's `remember … session` or a person's edit
//!   landed meanwhile, the result is dropped and the session re-queued, so
//!   the next pass folds the new row in instead of overwriting it.
//! - **Never fails anything.** A timeout, a lost connection or a reply
//!   without a usable summary leaves the memory as it was and says why in
//!   `SessionMemoryUpdate` — and in a log line when a person asked for it.
//!
//! Not installed in replay runs: the recording is the provider there, and a
//! pass would ask it for completions it never recorded.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use agents::long_term::{Added, MemoryConfig};
use agents::session_memory::{self, KeeperReply, PromoteScope, Transcript};
use llm::client::{ChatMessage, LlmClient};
use protocol::Event;
use sica_core::event::EventKind;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::chat::ChatHub;
use crate::sessions_store;

/// How often the worker looks for a quiet moment while it holds a job.
const QUIET_POLL: Duration = Duration::from_secs(5);
/// Completion cap for one pass: a summary, fifteen facts and three
/// promotions fit well inside it; a reply cut off by it is discarded.
const MAX_OUTPUT_TOKENS: u32 = 1_536;
/// A runaway reply is abandoned past this many characters.
const MAX_REPLY_CHARS: usize = 64 * 1024;

#[derive(Debug, Clone, Copy)]
struct Job {
    session_id: u64,
    /// A person asked (`RefreshSessionMemory`): the size threshold and the
    /// `session_summary` switch do not apply, and the outcome is logged.
    forced:     bool,
}

struct Keeper {
    hub:     ChatHub,
    tx:      mpsc::UnboundedSender<Job>,
    /// One pending idle timer per session.
    idle:    std::sync::Mutex<HashMap<u64, CancellationToken>>,
    /// The pass in flight, so a starting turn can stop it.
    current: std::sync::Mutex<Option<CancellationToken>>,
}

static KEEPER: OnceLock<Arc<Keeper>> = OnceLock::new();

/// Start the worker. Called once from `main`, inside the runtime, and never
/// in replay; every hook below is a no-op until it has run.
pub fn install(hub: ChatHub) {
    let (tx, rx) = mpsc::unbounded_channel();
    let keeper = Arc::new(Keeper {
        hub,
        tx,
        idle: std::sync::Mutex::new(HashMap::new()),
        current: std::sync::Mutex::new(None),
    });
    if KEEPER.set(keeper.clone()).is_err() {
        return;
    }
    tokio::spawn(worker(keeper, rx));
}

/// A turn is starting in `session_id`: its pending timer is void, and a pass
/// in flight — for any session — gives the LLM back.
pub fn session_active(session_id: u64) {
    let Some(k) = KEEPER.get() else { return };
    if let Some(t) = k.idle.lock().unwrap_or_else(|p| p.into_inner()).remove(&session_id) {
        t.cancel();
    }
    if let Some(t) = k.current.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        t.cancel();
    }
}

/// `session_id` went idle: arm its timer.
pub fn session_idle(session_id: u64) {
    let Some(k) = KEEPER.get() else { return };
    let cfg = MemoryConfig::current();
    if !cfg.session_summary {
        return;
    }
    let token = CancellationToken::new();
    if let Some(old) = k
        .idle
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(session_id, token.clone())
    {
        old.cancel();
    }
    let wait = Duration::from_secs(cfg.idle_seconds.max(1));
    let k = k.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = token.cancelled() => {}
            _ = tokio::time::sleep(wait) => {
                k.idle.lock().unwrap_or_else(|p| p.into_inner()).remove(&session_id);
                let _ = k.tx.send(Job { session_id, forced: false });
            }
        }
    });
}

/// `RefreshSessionMemory`: queue a pass now. `Err` says why nothing will
/// run — the keeper is off, there is no model, or nothing is new.
pub async fn request(session_id: u64) -> Result<(), String> {
    let k = KEEPER
        .get()
        .ok_or_else(|| "the memory keeper is not running in this backend".to_string())?;
    if k.hub.llm.lock().await.is_none() {
        return Err("no LLM connected — the memory is written by the model".into());
    }
    let job = Job { session_id, forced: true };
    match k.plan(job, &MemoryConfig::current()).await {
        Err(why) => Err(why),
        Ok(_) => k.tx.send(job).map_err(|_| "the memory keeper has stopped".to_string()),
    }
}

async fn worker(k: Arc<Keeper>, mut rx: mpsc::UnboundedReceiver<Job>) {
    while let Some(job) = rx.recv().await {
        let cfg = MemoryConfig::current();
        if !job.forced && !cfg.session_summary {
            continue;
        }
        // A session queued twice (idle, then a manual request) runs once:
        // the second pull finds nothing new.
        if k.plan(job, &cfg).await.is_err() {
            continue;
        }
        let client = loop {
            if let Some(c) = k.quiet_client().await {
                break c;
            }
            tokio::time::sleep(QUIET_POLL).await;
        };
        // Plan again: the log may have grown while the worker waited.
        let Ok(plan) = k.plan(job, &cfg).await else { continue };
        k.run(&client, job, plan, &cfg).await;
    }
}

/// What one pass works from.
struct Plan {
    /// Seq of the memory row the pass started from, `0` for none — the
    /// handle that says whether someone else wrote in the meantime.
    base_seq:   u64,
    summary:    String,
    facts:      Vec<String>,
    transcript: Transcript,
    cwd:        PathBuf,
}

impl Keeper {
    /// The LLM client, when one is connected and no turn is running.
    async fn quiet_client(&self) -> Option<LlmClient> {
        if !self.hub.active_turns.lock().await.is_empty() {
            return None;
        }
        self.hub.llm.lock().await.clone()
    }

    /// Whether `job` has anything to do, and what with. `Err` is the reason
    /// it does not — which a manual request reports as its answer.
    async fn plan(&self, job: Job, cfg: &MemoryConfig) -> Result<Plan, String> {
        let g = self.hub.sessions.lock().await;
        let log = g
            .get(&job.session_id)
            .ok_or_else(|| format!("session {} not found", job.session_id))?;
        let raw = sica_core::project::latest_session_memory(&log.events);
        let after = raw.as_ref().map(|m| m.through_seq).unwrap_or(0);
        let transcript = session_memory::transcript(&log.derive_surface(), after);
        if transcript.messages == 0 {
            return Err(format!("session {}'s memory is already up to date", job.session_id));
        }
        if !job.forced && transcript.new_chars < cfg.min_new_chars {
            return Err("not enough new conversation to be worth a pass".into());
        }
        let (summary, facts, base_seq) = match raw {
            Some(m) => (m.summary, m.facts, m.seq),
            None => (String::new(), Vec::new(), 0),
        };
        Ok(Plan {
            base_seq,
            summary,
            facts,
            transcript,
            cwd: log.cwd().unwrap_or_else(sica_core::paths::working_dir),
        })
    }

    fn emit(&self, ev: Event) {
        self.hub.event_sink.emit(ev);
    }

    async fn run(&self, client: &LlmClient, job: Job, plan: Plan, cfg: &MemoryConfig) {
        let session_id = job.session_id;
        let cancel = CancellationToken::new();
        *self.current.lock().unwrap_or_else(|p| p.into_inner()) = Some(cancel.clone());
        // Publish the token, *then* look again: a turn that started between
        // the quiet check and now had nothing to cancel.
        if !self.hub.active_turns.lock().await.is_empty() {
            cancel.cancel();
        }
        self.emit(Event::SessionMemoryUpdate { session_id, running: true, error: None });
        if job.forced {
            self.emit(Event::LogLine {
                level:   "INFO".into(),
                message: format!("memory: updating session {session_id}'s memory"),
            });
        }

        let known: Vec<String> =
            self.hub.memory.for_folder(&plan.cwd).into_iter().map(|m| m.text).collect();
        let messages = vec![
            ChatMessage::text("system", session_memory::KEEPER_SYSTEM),
            ChatMessage::text(
                "user",
                session_memory::keeper_task(&plan.summary, &plan.facts, &known, &plan.transcript.text),
            ),
        ];
        let reply = tokio::time::timeout(
            Duration::from_secs(cfg.timeout_secs.max(10)),
            complete(client, messages, &cancel),
        )
        .await;
        *self.current.lock().unwrap_or_else(|p| p.into_inner()) = None;

        if cancel.is_cancelled() {
            // A turn started. The memory is untouched; the session goes back
            // in the queue unless the turn is its own — that turn's end
            // re-arms the timer with more to read.
            self.emit(Event::SessionMemoryUpdate { session_id, running: false, error: None });
            if !self.hub.active_turns.lock().await.contains_key(&session_id) {
                let _ = self.tx.send(job);
            }
            return;
        }

        let outcome = match reply {
            Err(_) => Err(format!("timed out after {} s", cfg.timeout_secs)),
            Ok(Err(e)) => Err(e),
            Ok(Ok(raw)) => match session_memory::parse_keeper_reply(&raw) {
                None => Err("the model's reply had no usable summary".into()),
                Some(reply) => self.apply(session_id, &plan, reply, cfg).await,
            },
        };
        match outcome {
            Ok(Applied::Written { promoted }) => {
                info!(session_id, promoted, "session memory updated");
                self.emit(Event::SessionMemoryUpdate { session_id, running: false, error: None });
                if job.forced || promoted > 0 {
                    let mut message = format!("memory: session {session_id}'s memory updated");
                    if promoted > 0 {
                        message.push_str(&format!(
                            " — {promoted} fact(s) kept in long-term memory (Settings › Memory)"
                        ));
                    }
                    self.emit(Event::LogLine { level: "INFO".into(), message });
                }
            }
            Ok(Applied::Superseded) => {
                // Someone wrote first; fold their row in on the next pass.
                self.emit(Event::SessionMemoryUpdate { session_id, running: false, error: None });
                let _ = self.tx.send(job);
            }
            Err(why) => {
                warn!(session_id, reason = %why, "session memory pass produced nothing");
                self.emit(Event::SessionMemoryUpdate {
                    session_id,
                    running: false,
                    error:   Some(why.clone()),
                });
                if job.forced {
                    self.emit(Event::LogLine {
                        level:   "WARN".into(),
                        message: format!("memory: session {session_id}'s memory was not updated — {why}"),
                    });
                }
            }
        }
    }

    /// Land a pass: the new memory row (compare-and-set on the row the pass
    /// started from), then the promotions.
    async fn apply(
        &self,
        session_id: u64,
        plan: &Plan,
        reply: KeeperReply,
        cfg: &MemoryConfig,
    ) -> Result<Applied, String> {
        let memory = {
            let mut g = self.hub.sessions.lock().await;
            let log = g.get_mut(&session_id).ok_or_else(|| format!("session {session_id} vanished"))?;
            let now = sica_core::project::latest_session_memory(&log.events).map(|m| m.seq).unwrap_or(0);
            if now != plan.base_seq {
                return Ok(Applied::Superseded);
            }
            log.append(EventKind::SessionMemory {
                summary:     reply.summary.clone(),
                facts:       reply.facts.clone(),
                through_seq: plan.transcript.last_seq,
                author:      "auto".into(),
            });
            sessions_store::flush(log).map_err(|e| format!("the session log could not be written: {e}"))?;
            sica_core::project::session_memory(&log.events).map(|m| crate::chat::session_memory_dump(&m))
        };
        self.emit(Event::SessionMemoryChanged { session_id, memory });

        let mut promoted = 0;
        if cfg.auto_remember {
            for (scope, text) in &reply.promote {
                let project = (*scope == PromoteScope::Project).then_some(plan.cwd.as_path());
                match self.hub.memory.add(text, project, "auto", Some(session_id)) {
                    Ok(Added::New(_)) => promoted += 1,
                    Ok(Added::Duplicate(_)) => {}
                    Err(e) => warn!(session_id, error = %e, "memory: promotion not saved"),
                }
            }
            if promoted > 0 {
                self.hub.publish_memories();
            }
        }
        Ok(Applied::Written { promoted })
    }
}

enum Applied {
    Written { promoted: usize },
    /// A newer memory row landed while the pass ran.
    Superseded,
}

/// One non-tool completion, streamed and collected. `Err` names what went
/// wrong in words a log line can carry; a reply cut off by the token cap is
/// an error, never a memory.
async fn complete(
    client: &LlmClient,
    messages: Vec<ChatMessage>,
    cancel: &CancellationToken,
) -> Result<String, String> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut client = client.clone();
    client.max_tokens = Some(MAX_OUTPUT_TOKENS);
    let stream_cancel = Some(cancel.clone());
    let task = tokio::spawn(async move { client.chat_stream(messages, None, tx, stream_cancel).await });
    let mut buf = String::new();
    let mut finish: Option<String> = None;
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            v = rx.recv() => v,
        };
        let Some(chunk) = next else { break };
        buf.push_str(&chunk.delta_content);
        if chunk.finish_reason.is_some() {
            finish = chunk.finish_reason;
        }
        if buf.len() > MAX_REPLY_CHARS {
            break;
        }
    }
    if cancel.is_cancelled() {
        task.abort();
        return Err("cancelled".into());
    }
    match task.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(format!("the LLM request failed: {e}")),
        Err(e) if e.is_cancelled() => {}
        Err(e) => return Err(format!("the LLM request task failed: {e}")),
    }
    if finish.as_deref() == Some("length") {
        return Err(format!("the reply was cut off at {MAX_OUTPUT_TOKENS} tokens"));
    }
    Ok(buf)
}
