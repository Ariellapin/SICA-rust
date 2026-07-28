//! Chat session bookkeeping + LLM connection wiring used by the dispatcher.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use protocol::{Event, Frame, LlmOptions, LlmState, SessionMeta, UserImage};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use agents::{EventSink, SkillRegistry, ToolFailureSink, ToolSubAgent};
use llm::client::{ChatContent, ChatMessage, ContentPart, ImageUrl, LlmClient};
use sica_core::message::{Message, Role};
use sica_core::session::Session;

use crate::sessions_store;
use crate::title_gen;

/// Hard cap on tool hops within one user message. Stops a model from
/// ping-ponging skill calls forever when it cannot decide a final answer.
/// Generous because the documented workflow spends hops on `read-file`ing
/// `skills/*.md` contracts before the real calls.
const MAX_TOOL_HOPS: u8 = 12;

/// Title given to a freshly minted session. Used both at creation time and
/// as the trigger for the auto-title agent — if the title still matches
/// this format after the first response, we replace it with a summary.
pub fn default_title(id: u64) -> String {
    format!("Session {id}")
}

#[derive(Clone)]
pub struct ChatHub {
    pub sessions:      Arc<Mutex<HashMap<u64, Session>>>,
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
}

/// Fallback prompt window when neither the user nor the server reports one.
const DEFAULT_CONTEXT_WINDOW: u32 = 24_000;

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
        }
    }

    /// Build a hub pre-populated with every session it can find on disk.
    /// `next_id` is advanced past the largest existing id so newly minted
    /// sessions never collide with restored ones.
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
        match client.health().await {
            Ok(()) => {
                // Prompt window: explicit setting wins; otherwise ask the
                // server (vLLM `max_model_len`, llama.cpp `n_ctx_train`).
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
                self.set_llm_state(LlmState::Ready {
                    model: model.clone(),
                    context_window: window,
                })
                .await;
                self.event_sink.emit(Event::LogLine {
                    level: "INFO".into(),
                    message: format!(
                        "LLM: ready ({base_url}, model={model}, ctx={window}, \
                         temp={}, max_tokens={}, native_tools={})",
                        options.temperature,
                        options
                            .max_tokens
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "server-default".into()),
                        options.native_tools,
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

    pub async fn disconnect_llm(&self) {
        *self.llm.lock().await = None;
        self.set_llm_state(LlmState::Disconnected).await;
    }

    /// Cancel the in-flight turn (if any) for `session_id`. Idempotent.
    pub async fn interrupt_session(&self, session_id: u64) {
        if let Some((_, tok)) = self.active_turns.lock().await.get(&session_id) {
            tok.cancel();
        }
    }

    pub async fn send_user_message(
        &self,
        session_id: u64,
        text: String,
        images: Vec<UserImage>,
    ) {
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
            session.messages.push(Message::user_with_images(text.clone(), images.clone()));
            if let Err(e) = sessions_store::save(session) {
                warn!(error = %e, session_id, "save session (after user msg) failed");
            }
        }

        // Register a cancellation token for this session. If a previous turn
        // is still in flight (shouldn't normally happen — the FE gates Send
        // while a turn is unfinished), cancel it before installing the new one.
        let cancel = CancellationToken::new();
        let marker = self.next_marker.fetch_add(1, Ordering::Relaxed);
        {
            let mut guard = self.active_turns.lock().await;
            if let Some((_, prev)) = guard.insert(session_id, (marker, cancel.clone())) {
                prev.cancel();
            }
        }

        let events = self.event_sink.clone();
        let sessions_map = self.sessions.clone();
        let active_turns = self.active_turns.clone();
        let next_turn = self.next_turn.clone();
        let skills = self.skills.clone();
        let failure_sink = self.failure_sink.clone();
        let title_client = client.clone();
        let event_sink = self.event_sink.clone();
        let (native_tools, opt_max_tokens) = {
            let opts = self.llm_opts.lock().await;
            (opts.native_tools, opts.max_tokens)
        };
        let window = self.context_window.load(Ordering::Relaxed);
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
                let history =
                    build_history(&sessions_map, session_id, &skills, native_tools).await;

                // Trim to the prompt budget: window minus room for the
                // response (and a small safety margin for template overhead).
                let reserve = opt_max_tokens.unwrap_or(4096).saturating_add(512);
                let budget = window.saturating_sub(reserve).max(1024);
                let trimmed = agents::context::trim_to_budget(history, budget);
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

                let turn_id = next_turn.fetch_add(1, Ordering::Relaxed);
                let out = agents::turn::run_turn(
                    client.clone(),
                    events.clone(),
                    agents::turn::TurnInput {
                        session_id,
                        turn_id,
                        messages: trimmed.messages,
                        tools: if native_tools {
                            Some(skills.tools_json())
                        } else {
                            None
                        },
                        limit: window,
                        cancel: Some(cancel.clone()),
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
                    session.messages.push(Message {
                        role: Role::Assistant,
                        content: out.content.clone(),
                        reasoning,
                        images: Vec::new(),
                        tool_calls,
                        tool_call_id: None,
                    });
                    if let Err(e) = sessions_store::save(session) {
                        warn!(error = %e, session_id, "save session (after assistant msg) failed");
                    }
                }

                // If the user hit Esc, drop out before we go shopping for a
                // tool call on a half-completed assistant reply.
                if cancel.is_cancelled() {
                    break;
                }

                // Native tool-calling path: dispatch every call the model
                // emitted, answer each `tool_call_id`, and loop for the
                // model's next turn. Raw outcomes are returned verbatim —
                // no expectation/summarizer indirection in native mode.
                if native_tools && !out.tool_calls.is_empty() {
                    let over_limit = hops >= MAX_TOOL_HOPS;
                    if !over_limit {
                        hops += 1;
                    }
                    for call in &out.tool_calls {
                        let outcome = if over_limit {
                            agents::SkillOutcome {
                                ok: false,
                                summary: format!(
                                    "tool-hop limit ({MAX_TOOL_HOPS}) reached — call not executed"
                                ),
                            }
                        } else {
                            dispatch_native_call(
                                call,
                                &skills,
                                &events,
                                failure_sink.clone(),
                            )
                            .await
                        };
                        append_tool_result(
                            &sessions_map,
                            session_id,
                            &call.name,
                            Some(&call.id),
                            outcome.ok,
                            &outcome.summary,
                        )
                        .await;
                    }
                    if over_limit {
                        event_sink.emit(Event::LogLine {
                            level: "WARN".into(),
                            message: format!(
                                "tool-hop limit ({MAX_TOOL_HOPS}) reached — aborting further skill calls"
                            ),
                        });
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
                    if looks_like_tool_call_attempt(&out.content) {
                        let msg = "assistant emitted a tool-call-shaped block \
                                   the parser could not read (malformed JSON \
                                   inside the ```tool_call fence, or missing \
                                   `skill`/`args` keys)";
                        warn!(session_id, "{msg}");
                        event_sink.emit(Event::LogLine {
                            level:   "WARN".into(),
                            message: msg.into(),
                        });
                    }
                    break;
                };
                if hops >= MAX_TOOL_HOPS {
                    let msg = format!(
                        "tool-hop limit ({MAX_TOOL_HOPS}) reached — aborting further skill calls"
                    );
                    event_sink.emit(Event::LogLine { level: "WARN".into(), message: msg.clone() });
                    append_tool_result(&sessions_map, session_id, &call.skill, None, false, &msg).await;
                    break;
                }
                hops += 1;

                // Dispatch the skill. Unknown skill → record an error result
                // and let the model recover on the next hop. Successful
                // outcomes get post-summarised through the same `client` so
                // the main agent receives a focused answer instead of the
                // raw skill output (matches the natural-language contract in
                // memory.md).
                let outcome = match skills.resolve(&call) {
                    Some((skill, args)) => {
                        let mut sub = ToolSubAgent::root(events.clone())
                            .with_summarizer(client.clone());
                        if let Some(fs) = failure_sink.clone() {
                            sub = sub.with_failure_sink(fs);
                        }
                        sub.run(agents::ToolInvocation {
                            skill: &*skill,
                            args,
                            raw_args: call.raw_args.clone(),
                            expectation: call.expectation.clone(),
                        }).await
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
                    None,
                    outcome.ok,
                    &outcome.summary,
                )
                .await;
            }

            // Release this turn's slot, but only if a *newer* send hasn't
            // already replaced it (marker comparison avoids clobbering).
            {
                let mut guard = active_turns.lock().await;
                if let Some((slot_marker, _)) = guard.get(&session_id) {
                    if *slot_marker == marker {
                        guard.remove(&session_id);
                    }
                }
            }

            // Skip the auto-title work if the user interrupted — a partial
            // assistant reply isn't a useful title source.
            if cancel.is_cancelled() {
                return;
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
/// present) plus a live `## Loaded skills` listing built from the registry as
/// a single system message, then every persisted message. The dynamic list
/// matters because `memory.md` only enumerates the built-ins — without this
/// step, user-authored skills are callable but invisible to the model.
/// Tool-role messages are surfaced to the local server as `user` content so
/// even llama.cpp builds without OpenAI tool-call awareness can read the
/// result.
async fn build_history(
    sessions: &Arc<Mutex<HashMap<u64, Session>>>,
    session_id: u64,
    skills: &SkillRegistry,
    native_tools: bool,
) -> Vec<ChatMessage> {
    let g = sessions.lock().await;
    let Some(session) = g.get(&session_id) else { return Vec::new() };
    let mut out: Vec<ChatMessage> = Vec::with_capacity(session.messages.len() + 1);
    let mem = agents::memory::load(&sica_core::paths::memory_file()).unwrap_or_default();
    let catalogue = skills.catalogue_markdown();
    // In native mode the tool contract travels in the request's `tools`
    // array, so the text-protocol invocation brief would only confuse the
    // model; send just the skill catalogue as orientation.
    let system_body = if native_tools {
        if catalogue.is_empty() {
            String::new()
        } else {
            format!(
                "You are running inside the sica-rust desktop app. Use the \
                 provided tools (OpenAI function calling) to run commands and \
                 read/write files when the task needs it. Base every claim \
                 about the host system on an actual tool result.\n\n\
                 ## Available skills\n\n{catalogue}"
            )
        }
    } else {
        let mut content = mem;
        if !catalogue.is_empty() {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str("## Loaded skills\n\n");
            content.push_str(&catalogue);
        }
        content
    };
    if !system_body.is_empty() {
        out.push(ChatMessage::text("system", system_body));
    }
    for m in &session.messages {
        // Text-protocol servers may lack a `tool` role in their template, so
        // tool results are surfaced as `user` there. Native mode keeps the
        // real `tool` role + correlation id the template expects.
        let role = match m.role {
            Role::Tool if !native_tools => "user",
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

/// Dispatch one native tool call through the sub-agent machinery. No
/// summarizer is attached: native mode returns raw tool output, which is
/// what the OpenAI tool-call convention (and the model's training) expects.
async fn dispatch_native_call(
    call: &agents::turn::NativeToolCall,
    skills: &SkillRegistry,
    events: &Arc<dyn EventSink>,
    failure_sink: Option<Arc<dyn ToolFailureSink>>,
) -> agents::SkillOutcome {
    let Some(skill) = skills.get(&call.name) else {
        return agents::SkillOutcome {
            ok: false,
            summary: format!("unknown skill `{}`", call.name),
        };
    };
    let args: serde_json::Value = match serde_json::from_str(&call.arguments) {
        Ok(v) => v,
        Err(e) => {
            return agents::SkillOutcome {
                ok: false,
                summary: format!(
                    "invalid JSON in tool-call arguments ({e}); raw: {}",
                    call.arguments
                ),
            };
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
    let mut sub = ToolSubAgent::root(events.clone());
    if let Some(fs) = failure_sink {
        sub = sub.with_failure_sink(fs);
    }
    sub.run(agents::ToolInvocation {
        skill: &*skill,
        args,
        raw_args,
        expectation: String::new(),
    })
    .await
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

/// Append the result of one skill invocation as a `Tool` message, formatted
/// as a `tool_result` fenced block. Persists the session so a crash mid-loop
/// still preserves the partial transcript.
async fn append_tool_result(
    sessions: &Arc<Mutex<HashMap<u64, Session>>>,
    session_id: u64,
    skill: &str,
    tool_call_id: Option<&str>,
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
        images: Vec::new(),
        tool_calls: None,
        tool_call_id: tool_call_id.map(str::to_string),
    });
    if let Err(e) = sessions_store::save(session) {
        warn!(error = %e, session_id, "save session (after tool result) failed");
    }
}

/// True when `content` contains a shape the model commonly *thinks* is a
/// tool call but the parser does not accept. Used to surface the silent-
/// drop case as a `LogLine` — without it, the FE just sees an ordinary
/// assistant message and the operator has no signal that a tool was meant
/// to fire. Kept conservative: matches the explicit ```tool_call fence and
/// the OpenAI-ish `"skill": "..."` + `"args":` JSON pair.
fn looks_like_tool_call_attempt(content: &str) -> bool {
    if content.contains("```tool_call") {
        return true;
    }
    let has_skill_key = content.contains("\"skill\"") || content.contains("'skill'");
    let has_args_key  = content.contains("\"args\"")  || content.contains("'args'");
    has_skill_key && has_args_key
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
