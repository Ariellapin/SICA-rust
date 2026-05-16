//! Chat session bookkeeping + LLM connection wiring used by the dispatcher.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use protocol::{Event, Frame, LlmState, SessionMeta};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, warn};

use agents::{EventSink, SkillRegistry, ToolSubAgent};
use llm::client::LlmClient;
use sica_core::message::{Message, Role};
use sica_core::session::Session;

use crate::sessions_store;
use crate::title_gen;

/// Hard cap on tool hops within one user message. Stops a model from
/// ping-ponging skill calls forever when it cannot decide a final answer.
const MAX_TOOL_HOPS: u8 = 6;

/// Title given to a freshly minted session. Used both at creation time and
/// as the trigger for the auto-title agent — if the title still matches
/// this format after the first response, we replace it with a summary.
pub fn default_title(id: u64) -> String {
    format!("Session {id}")
}

#[derive(Clone)]
pub struct ChatHub {
    pub sessions:   Arc<Mutex<HashMap<u64, Session>>>,
    pub next_id:    Arc<AtomicU64>,
    pub next_turn:  Arc<AtomicU64>,
    pub llm:        Arc<Mutex<Option<LlmClient>>>,
    pub llm_state:  Arc<Mutex<LlmState>>,
    pub out_tx:     mpsc::UnboundedSender<Frame>,
    pub event_sink: Arc<dyn EventSink>,
    /// Skill catalogue used to dispatch `tool_call` blocks parsed from the
    /// assistant's reply. Shared (immutable post-startup) so cloning a hub
    /// does not copy the map.
    pub skills:     Arc<SkillRegistry>,
}

impl ChatHub {
    pub fn new(out_tx: mpsc::UnboundedSender<Frame>, skills: Arc<SkillRegistry>) -> Self {
        let sink: Arc<dyn EventSink> = Arc::new(OutSink { tx: out_tx.clone() });
        Self {
            sessions:   Arc::new(Mutex::new(HashMap::new())),
            next_id:    Arc::new(AtomicU64::new(1)),
            next_turn:  Arc::new(AtomicU64::new(1)),
            llm:        Arc::new(Mutex::new(None)),
            llm_state:  Arc::new(Mutex::new(LlmState::Disconnected)),
            out_tx,
            event_sink: sink,
            skills,
        }
    }

    /// Build a hub pre-populated with every session it can find on disk.
    /// `next_id` is advanced past the largest existing id so newly minted
    /// sessions never collide with restored ones.
    pub fn new_loaded(out_tx: mpsc::UnboundedSender<Frame>, skills: Arc<SkillRegistry>) -> Self {
        let hub = Self::new(out_tx, skills);
        let loaded = sessions_store::load_all();
        let max_id = loaded.iter().map(|s| s.id).max().unwrap_or(0);
        {
            let map = hub.sessions.clone();
            let mut g = map.try_lock().expect("fresh ChatHub, no contention");
            for s in loaded {
                g.insert(s.id, s);
            }
        }
        hub.next_id.store(max_id + 1, Ordering::Relaxed);
        hub
    }

    pub async fn list_sessions(&self) -> Vec<SessionMeta> {
        let g = self.sessions.lock().await;
        let mut out: Vec<SessionMeta> = g
            .values()
            .map(|s| SessionMeta {
                id: s.id,
                title: s.title.clone(),
                created_at: s.created_at,
            })
            .collect();
        out.sort_by_key(|s| s.created_at);
        out
    }

    pub async fn load_session(&self, id: u64) -> Option<Session> {
        self.sessions.lock().await.get(&id).cloned()
    }

    pub async fn create_session(&self) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let s = Session::new(id, default_title(id));
        self.sessions.lock().await.insert(id, s);
        id
    }

    pub async fn delete_session(&self, id: u64) -> bool {
        let removed = self.sessions.lock().await.remove(&id).is_some();
        if removed {
            sessions_store::delete(id);
        }
        removed
    }

    pub async fn set_llm_state(&self, st: LlmState) {
        *self.llm_state.lock().await = st.clone();
        let _ = self
            .out_tx
            .send(Frame::event(Event::LlmStateChanged { state: st }));
    }

    pub async fn connect_llm(&self, base_url: String, model: String, api_key: Option<String>) {
        self.set_llm_state(LlmState::Connecting).await;
        // Push a visible log line so the FE log panel reflects what's happening
        // — the dot transition can be subtle on first run.
        self.event_sink.emit(Event::LogLine {
            level: "INFO".into(),
            message: format!("LLM: connecting to {base_url} (model={model})"),
        });
        let client = LlmClient::new(base_url.clone(), model.clone(), api_key);
        match client.health().await {
            Ok(()) => {
                *self.llm.lock().await = Some(client);
                self.set_llm_state(LlmState::Ready {
                    model: model.clone(),
                    context_window: 24_000,
                })
                .await;
                self.event_sink.emit(Event::LogLine {
                    level: "INFO".into(),
                    message: format!("LLM: ready ({base_url}, model={model})"),
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
    pub fn spawn_connect_llm(&self, base_url: String, model: String, api_key: Option<String>) {
        let this = self.clone();
        tokio::spawn(async move {
            this.connect_llm(base_url, model, api_key).await;
        });
    }

    pub async fn disconnect_llm(&self) {
        *self.llm.lock().await = None;
        self.set_llm_state(LlmState::Disconnected).await;
    }

    pub async fn send_user_message(&self, session_id: u64, text: String) {
        let Some(client) = self.llm.lock().await.clone() else {
            self.event_sink.emit(Event::LogLine {
                level: "WARN".into(),
                message: "no LLM connected — cannot send".into(),
            });
            return;
        };

        // Ensure session exists, push the user message, and write the TOML
        // file straight away — that way a session is recoverable even if the
        // LLM call dies mid-stream.
        {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .entry(session_id)
                .or_insert_with(|| Session::new(session_id, default_title(session_id)));
            session.messages.push(Message::user(text.clone()));
            if let Err(e) = sessions_store::save(session) {
                warn!(error = %e, session_id, "save session (after user msg) failed");
            }
        }

        let events = self.event_sink.clone();
        let sessions_map = self.sessions.clone();
        let next_turn = self.next_turn.clone();
        let skills = self.skills.clone();
        let title_client = client.clone();
        let event_sink = self.event_sink.clone();
        tokio::spawn(async move {
            let mut hops: u8 = 0;
            // Always overwritten on the first iteration before the post-loop
            // read; the initial value is just to satisfy definite assignment.
            #[allow(unused_assignments)]
            let mut last_assistant = String::new();
            loop {
                // Rebuild history fresh from persisted session messages each
                // iteration: the previous hop appended both the assistant
                // call and the tool result, so this picks them up uniformly.
                let history = build_history(&sessions_map, session_id).await;
                let turn_id = next_turn.fetch_add(1, Ordering::Relaxed);

                let out = agents::turn::run_turn(
                    client.clone(),
                    events.clone(),
                    agents::turn::TurnInput {
                        session_id,
                        turn_id,
                        messages: history,
                        limit: 24_000,
                    },
                )
                .await;
                last_assistant = out.content.clone();

                // Persist the assistant message (it includes the tool_call
                // block if one was emitted — kept verbatim so re-loading the
                // session shows what the model actually said).
                {
                    let mut g = sessions_map.lock().await;
                    let Some(session) = g.get_mut(&session_id) else {
                        debug!(session_id, "session vanished mid-turn, skipping persist");
                        return;
                    };
                    let reasoning = if out.reasoning.is_empty() {
                        None
                    } else {
                        Some(out.reasoning.clone())
                    };
                    session.messages.push(Message {
                        role: Role::Assistant,
                        content: out.content.clone(),
                        reasoning,
                    });
                    if let Err(e) = sessions_store::save(session) {
                        warn!(error = %e, session_id, "save session (after assistant msg) failed");
                    }
                }

                // Look for a tool call. If none, we're done.
                let Some(call) = agents::extract_tool_call(&out.content) else {
                    break;
                };
                if hops >= MAX_TOOL_HOPS {
                    let msg = format!(
                        "tool-hop limit ({MAX_TOOL_HOPS}) reached — aborting further skill calls"
                    );
                    event_sink.emit(Event::LogLine { level: "WARN".into(), message: msg.clone() });
                    append_tool_result(&sessions_map, session_id, &call.skill, false, &msg).await;
                    break;
                }
                hops += 1;

                // Dispatch the skill. Unknown skill → record an error result
                // and let the model recover on the next hop.
                let outcome = match skills.get(&call.skill) {
                    Some(skill) => {
                        ToolSubAgent::root(events.clone())
                            .run(&*skill, call.args.clone())
                            .await
                    }
                    None => agents::SkillOutcome {
                        ok: false,
                        summary: format!("unknown skill `{}`", call.skill),
                    },
                };

                append_tool_result(
                    &sessions_map,
                    session_id,
                    &call.skill,
                    outcome.ok,
                    &outcome.summary,
                )
                .await;
            }

            // Auto-title only fires once, after the first complete exchange
            // (user → assistant final). Count user messages to decide.
            let trigger_title = {
                let g = sessions_map.lock().await;
                let Some(session) = g.get(&session_id) else { return };
                let user_count = session
                    .messages
                    .iter()
                    .filter(|m| m.role == Role::User)
                    .count();
                let title_is_default = session.title == default_title(session_id);
                user_count == 1 && title_is_default && !last_assistant.is_empty()
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
                    let Some(session) = g.get_mut(&session_id) else {
                        return;
                    };
                    // Re-check the default — the user may have renamed it
                    // manually in the meantime (future feature, harmless now).
                    if session.title != default_title(session_id) {
                        return;
                    }
                    session.title = title.clone();
                    if let Err(e) = sessions_store::save(session) {
                        warn!(error = %e, session_id, "save session (after title-gen) failed");
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

/// Rebuild the LLM wire history for `session_id`: prepend `memory.md` (if
/// present) as a system message, then every persisted message. Tool-role
/// messages are surfaced to the local server as `user` content so even
/// llama.cpp builds without OpenAI tool-call awareness can read the result.
async fn build_history(
    sessions: &Arc<Mutex<HashMap<u64, Session>>>,
    session_id: u64,
) -> Vec<llm::client::ChatMessage> {
    let g = sessions.lock().await;
    let Some(session) = g.get(&session_id) else { return Vec::new() };
    let mut out: Vec<llm::client::ChatMessage> = Vec::with_capacity(session.messages.len() + 1);
    if let Some(mem) = agents::memory::load(&sica_core::paths::memory_file()) {
        out.push(llm::client::ChatMessage { role: "system".into(), content: mem });
    }
    for m in &session.messages {
        let role = match m.role {
            Role::Tool => "user",
            other => role_to_str(other),
        };
        out.push(llm::client::ChatMessage {
            role:    role.into(),
            content: m.content.clone(),
        });
    }
    out
}

/// Append the result of one skill invocation as a `Tool` message, formatted
/// as a `tool_result` fenced block. Persists the session so a crash mid-loop
/// still preserves the partial transcript.
async fn append_tool_result(
    sessions: &Arc<Mutex<HashMap<u64, Session>>>,
    session_id: u64,
    skill: &str,
    ok: bool,
    summary: &str,
) {
    let block = format!(
        "```tool_result\n{}\n```",
        serde_json::json!({
            "skill":   skill,
            "ok":      ok,
            "summary": summary,
        })
    );
    let mut g = sessions.lock().await;
    let Some(session) = g.get_mut(&session_id) else { return };
    session.messages.push(Message {
        role: Role::Tool,
        content: block,
        reasoning: None,
    });
    if let Err(e) = sessions_store::save(session) {
        warn!(error = %e, session_id, "save session (after tool result) failed");
    }
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
