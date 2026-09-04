//! Chat session bookkeeping + LLM connection wiring used by the dispatcher.
//!
//! Sessions are append-only event logs ([`SessionLog`]); the history sent
//! to the model is derived from them on every hop. Nothing in this module
//! mutates a message list — every persistence site is an [`append_event`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use protocol::{Event, Frame, LlmOptions, LlmState, MessageDump, SessionDump, SessionMeta, UserImage};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use agents::guard::RepeatTracker;
use agents::meter::TokenMeter;
use agents::{EventSink, SkillRegistry, ToolFailureSink, ToolSubAgent};
use llm::client::{ChatContent, ChatMessage, ContentPart, ImageUrl, LlmClient};
use sica_core::event::{ContextSource, EventKind, SurfaceEntry, SurfaceOp};
use sica_core::message::{Message, Role};

use crate::sessions_store::{self, SessionLog};
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

type Sessions = Arc<Mutex<HashMap<u64, SessionLog>>>;

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
    /// Repeat-tool-reminder chain per session (`agents::guard`). Cleared
    /// by every user message; consulted after every dispatched call.
    pub repeat:        Arc<Mutex<HashMap<u64, RepeatTracker>>>,
    /// Usage-anchored token meter per session (`agents::meter`). Anchored on
    /// the provider's own `usage` after each successful request; cleared on
    /// reconnect.
    pub meters:        Arc<Mutex<HashMap<u64, TokenMeter>>>,
}

type Repeats = Arc<Mutex<HashMap<u64, RepeatTracker>>>;

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
            repeat:       Arc::new(Mutex::new(HashMap::new())),
            meters:       Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Build a hub pre-populated with every session it can find on disk
    /// (migrating legacy TOML files on the way). `next_id` is advanced past
    /// the largest existing id so newly minted sessions never collide with
    /// restored ones.
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
                title: s.title(),
                created_at: s.created_at(),
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
    pub async fn dump_session(&self, id: u64) -> Option<SessionDump> {
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
                let (content, tool_name, tool_ok, tool_args_preview, tool_expectation) = match e.tool {
                    Some(t) => (t.summary, Some(t.name), Some(t.ok), Some(t.args_preview), Some(t.expectation)),
                    None => (e.message.content, None, None, None, None),
                };
                MessageDump {
                    role: role.into(),
                    content,
                    reasoning: e.message.reasoning,
                    images: e.message.images,
                    tool_name,
                    tool_ok,
                    tool_args_preview,
                    tool_expectation,
                    context_source: e.context.as_ref().map(|c| c.label()),
                }
            })
            .collect();
        Some(SessionDump {
            id: log.id,
            title: log.title(),
            created_at: log.created_at(),
            messages,
        })
    }

    /// Mint a session in memory only. It reaches disk with its first user
    /// message, so an unused "new session" leaves no file behind.
    pub async fn create_session(&self) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let s = SessionLog::new(id, default_title(id));
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
                         temp={}, max_tokens={}, native_tools={}, thinking={})",
                        options.temperature,
                        options
                            .max_tokens
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "server-default".into()),
                        options.native_tools,
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

        // A new user message starts a fresh repeat-tool chain.
        self.repeat.lock().await.remove(&session_id);

        // `/name …` loads the named command / agent / skill body as
        // instructions *before* the message — a palette pick and a typed
        // token arrive here identically. The message itself is kept as
        // typed so the transcript shows what the user sent.
        let expansion = agents::invoke::expand(&text, &agents::invoke::Roots::from_workspace());
        if let Some(exp) = &expansion {
            self.event_sink.emit(Event::LogLine {
                level: "INFO".into(),
                message: format!("loaded /{} ({:?}) as context for this turn", exp.name, exp.family),
            });
        }

        // Ensure the session exists and record the user message straight
        // away — the log is flushed on every append, so the session is
        // recoverable even if the LLM call dies mid-stream.
        let outer_turn = self.next_turn.fetch_add(1, Ordering::Relaxed);
        // The placeholder-or-fallback title this send leaves behind, so the
        // LLM titler later knows the title is still automatic.
        let provisional_title;
        {
            let mut sessions = self.sessions.lock().await;
            let log = sessions
                .entry(session_id)
                .or_insert_with(|| SessionLog::new(session_id, default_title(session_id)));
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
            append_runtime_context(log, &client.model);
            if let Some(exp) = expansion {
                log.append(EventKind::ContextInjected {
                    surface: SurfaceOp::Append,
                    source: ContextSource::SkillInvocation { name: exp.name },
                    content: exp.content,
                });
            }
            log.append(EventKind::UserMessage {
                surface: SurfaceOp::Append,
                content: text.clone(),
                images: images.clone(),
            });
            log.append(EventKind::TurnStart { turn_id: outer_turn });
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
        let repeats = self.repeat.clone();
        let active_turns = self.active_turns.clone();
        let next_turn = self.next_turn.clone();
        let skills = self.skills.clone();
        let failure_sink = self.failure_sink.clone();
        let title_client = client.clone();
        let event_sink = self.event_sink.clone();
        let meters = self.meters.clone();
        let (native_tools, opt_max_tokens, compact_policy) = {
            let opts = self.llm_opts.lock().await;
            (opts.native_tools, opts.max_tokens, opts.compact)
        };
        let model_name = client.model.clone();
        let window = self.context_window.load(Ordering::Relaxed);
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
            loop {
                // Interrupts land between hops as often as mid-stream. Bailing
                // here keeps a cancelled turn from opening another request —
                // which would emit a fresh `TurnStarted` the FE renders as a
                // new (empty) turn, and burn a tokenize round-trip first.
                if cancel.is_cancelled() {
                    break;
                }

                // Derive the history fresh from the event log each iteration:
                // the previous hop appended both the assistant message and
                // the tool result, so this picks them up uniformly.
                let mut wh = match build_history(
                    &sessions_map, session_id, &skills, native_tools, &model_name,
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
                        &sessions_map, session_id, &skills, native_tools, &model_name,
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
                        // A model can emit several calls at once; an interrupt
                        // part-way through must stop the rest, not run them all
                        // because they were already parsed. Logging the call
                        // only here keeps an interrupted batch free of
                        // `ToolCall` events that never got a result.
                        if cancel.is_cancelled() {
                            break;
                        }
                        let call_seq = append_event(&sessions_map, session_id, EventKind::ToolCall {
                            name: call.name.clone(),
                            args_preview: format!("{} {}", call.name, call.arguments),
                            expectation: String::new(),
                            call_id: Some(call.id.clone()),
                        })
                        .await
                        .unwrap_or(0);
                        let (outcome, trusted) = if over_limit {
                            (
                                agents::SkillOutcome {
                                    ok: false,
                                    summary: format!(
                                        "tool-hop limit ({MAX_TOOL_HOPS}) reached — call not executed"
                                    ),
                                },
                                true,
                            )
                        } else {
                            (
                                dispatch_native_call(
                                    call,
                                    session_id,
                                    &skills,
                                    &events,
                                    &client,
                                    failure_sink.clone(),
                                    cancel.clone(),
                                )
                                .await,
                                skills.get(&call.name).is_some_and(|s| s.trusted()),
                            )
                        };
                        append_tool_result(
                            &sessions_map,
                            session_id,
                            call_seq,
                            &call.name,
                            Some(&call.id),
                            outcome.ok,
                            &outcome.summary,
                            trusted,
                        )
                        .await;
                        if outcome.ok && is_fs_skill(&call.name) {
                            reconcile_instructions_after_fs(&sessions_map, &event_sink, session_id).await;
                        }
                        let args = serde_json::from_str(&call.arguments)
                            .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone()));
                        observe_repeat(&repeats, &sessions_map, &event_sink, session_id, &call.name, &args)
                            .await;
                    }
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
                let call_seq = append_event(&sessions_map, session_id, EventKind::ToolCall {
                    name: call.skill.clone(),
                    args_preview: agents::parse_tool_call::render(&call.skill, &call.raw_args),
                    expectation: call.expectation.clone(),
                    call_id: None,
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

                // Dispatch the skill. Unknown skill → record an error result
                // and let the model recover on the next hop. Successful
                // outcomes get post-summarised through the same `client` so
                // the main agent receives a focused answer instead of the
                // raw skill output (matches the natural-language contract in
                // memory.md).
                let (outcome, trusted, observed_args) = match skills.resolve(&call) {
                    Some((skill, args)) => {
                        let mut sub = ToolSubAgent::root(events.clone())
                            .with_summarizer(client.clone())
                            .with_cancel(cancel.clone())
                            .with_spill_label(session_id.to_string());
                        if let Some(fs) = failure_sink.clone() {
                            sub = sub.with_failure_sink(fs);
                        }
                        let outcome = sub
                            .run(agents::ToolInvocation {
                                skill: &*skill,
                                args: args.clone(),
                                raw_args: call.raw_args.clone(),
                                expectation: call.expectation.clone(),
                            })
                            .await;
                        (outcome, skill.trusted(), args)
                    }
                    None => (
                        agents::SkillOutcome {
                            ok: false,
                            summary: format!("unknown skill `{}`", call.skill),
                        },
                        true,
                        serde_json::json!(call.raw_args),
                    ),
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
                observe_repeat(&repeats, &sessions_map, &event_sink, session_id, &call.skill, &observed_args)
                    .await;
            }

            let finish = if cancel.is_cancelled() { "interrupted" } else { finish };
            append_event(&sessions_map, session_id, EventKind::TurnEnd {
                turn_id: outer_turn,
                finish_reason: finish.to_string(),
                hops,
            })
            .await;

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

/// Append one event to a session's log and flush it. Returns the seq, or
/// `None` when the session no longer exists. A flush failure is logged and
/// the in-memory log keeps going — the next successful flush writes every
/// line still pending.
async fn append_event(sessions: &Sessions, session_id: u64, kind: EventKind) -> Option<u64> {
    let mut g = sessions.lock().await;
    let log = g.get_mut(&session_id)?;
    let seq = log.append(kind);
    if let Err(e) = sessions_store::flush(log) {
        warn!(error = %e, session_id, seq, "flush session failed");
    }
    Some(seq)
}

/// Snapshot the volatile runtime facts (time, cwd, os, model) as a
/// user-role message. The new snapshot shadows its predecessor at the
/// position the first one took, so one copy is ever model-visible and the
/// system-prompt prefix is never touched. Called once per turn — dsh's
/// refresh throttle. When the predecessor was itself shadowed by a
/// compaction, the new snapshot appends fresh instead of replacing a span
/// that is no longer on the surface.
fn append_runtime_context(log: &mut SessionLog, model: &str) {
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
    let root = sica_core::paths::workspace_root();
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

/// Feed one dispatched call to the session's repeat-tool chain and, at a
/// threshold, inject the advisory notice as context for the next step.
/// Runs for failed and unknown-skill calls too — a model hammering a
/// failing call is exactly the loop worth breaking.
async fn observe_repeat(
    repeats: &Repeats,
    sessions: &Sessions,
    events: &Arc<dyn EventSink>,
    session_id: u64,
    skill: &str,
    args: &serde_json::Value,
) {
    let notice = repeats
        .lock()
        .await
        .entry(session_id)
        .or_default()
        .observe(skill, args);
    let Some(notice) = notice else { return };
    warn!(session_id, skill, "repeat-tool reminder issued");
    events.emit(Event::LogLine {
        level: "WARN".into(),
        message: format!("loop guard: `{skill}` called repeatedly with identical arguments — reminder injected"),
    });
    append_event(sessions, session_id, EventKind::ContextInjected {
        surface: SurfaceOp::Append,
        source: ContextSource::ToolNotice,
        content: notice,
    })
    .await;
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
    native_tools: bool,
    model: &str,
) -> Result<Option<WireHistory>, agents::prompt::PromptError> {
    let entries = {
        let g = sessions.lock().await;
        let Some(log) = g.get(&session_id) else { return Ok(None) };
        log.derive_surface()
    };
    let messages: Vec<Message> = entries.iter().map(|e| e.message.clone()).collect();
    let mut wh = build_wire_history(&messages, skills, native_tools, model)?;
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
    /// Fingerprint of system body + tools array — the meter's anchor key.
    pub envelope:    u64,
    /// Approximate per-part token counts for the status bar.
    pub breakdown:   protocol::TokenBreakdown,
}

/// Assemble the LLM wire history: compose the system prompt through
/// `agents::prompt` (ordered sections, strict interpolation), then map
/// every derived message to its wire form. Tool-role messages are surfaced
/// to the local server as `user` content so even llama.cpp builds without
/// OpenAI tool-call awareness can read the result.
fn build_wire_history(
    messages: &[Message],
    skills: &SkillRegistry,
    native_tools: bool,
    model: &str,
) -> Result<WireHistory, agents::prompt::PromptError> {
    let mem = agents::memory::load(&sica_core::paths::memory_file()).unwrap_or_default();
    let vars = agents::prompt::standard_vars(model);
    let rendered = agents::prompt::for_main_agent(&mem, skills, native_tools, &vars)?;
    let tools_json = native_tools.then(|| skills.tools_json());

    let mut out: Vec<ChatMessage> = Vec::with_capacity(messages.len() + 1);
    if !rendered.system.is_empty() {
        out.push(ChatMessage::text("system", rendered.system.clone()));
    }
    out.extend(wire_messages(messages, native_tools));

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
    Ok(WireHistory {
        messages: out,
        entries: Vec::new(), // filled by build_history
        system_body: rendered.system,
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

/// Dispatch one native tool call through the sub-agent machinery. The LLM
/// client is attached to the sub-agent's summarizer slot, but native mode
/// still returns raw tool output — the summarizer only ever rewrites when an
/// `expectation` is set, and native calls always pass an empty one. The
/// attach exists so LLM-driven skills (`agent-team`) can reach the connected
/// client through `SkillContext`.
async fn dispatch_native_call(
    call: &agents::turn::NativeToolCall,
    session_id: u64,
    skills: &SkillRegistry,
    events: &Arc<dyn EventSink>,
    client: &LlmClient,
    failure_sink: Option<Arc<dyn ToolFailureSink>>,
    cancel: CancellationToken,
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
    let mut sub = ToolSubAgent::root(events.clone())
        .with_summarizer(client.clone())
        .with_cancel(cancel)
        .with_spill_label(session_id.to_string());
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
        let wire = build_wire_history(&msgs, &registry(), false, "test").unwrap().messages;
        // memory.md may or may not exist on this machine; look at the tail.
        let n = wire.len();
        assert_eq!(wire[n - 2].role, "user");
        assert_eq!(wire[n - 1].role, "user");
        assert!(wire[n - 1].tool_call_id.is_none());
        assert!(wire[n - 1].tool_calls.is_none());
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
        let wire = build_wire_history(&msgs, &registry(), true, "test").unwrap().messages;
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
        let wire = build_wire_history(&msgs, &registry(), false, "test").unwrap().messages;
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
        let wire_text = build_wire_history(&[Message::user("hi")], &registry(), false, "test").unwrap();
        let wire_native = build_wire_history(&[Message::user("hi")], &registry(), true, "test").unwrap();
        let sys_native = &wire_native.system_body;
        assert!(sys_native.contains(agents::prompt::NATIVE_IDENTITY), "{sys_native}");
        assert!(!sys_native.contains("## Loaded skills"), "tools array carries the catalogue");
        assert!(!wire_text.system_body.contains(agents::prompt::NATIVE_IDENTITY));
        // The runtime snapshot never leaks into the system body.
        assert!(!sys_native.contains(agents::prompt::RUNTIME_CONTEXT_HEADER));
    }

    #[test]
    fn wire_history_breakdown_covers_all_three_parts() {
        let wh = build_wire_history(&[Message::user("hi")], &registry(), false, "test").unwrap();
        assert!(wh.breakdown.history >= 5, "user message priced");
        assert_eq!(wh.breakdown.tools, 0, "text protocol sends no tools array");
        assert!(wh.envelope != 0);
    }

    #[test]
    fn runtime_snapshot_shadows_its_predecessor() {
        let mut log = SessionLog::new(1, "t");
        append_runtime_context(&mut log, "test-model");
        log.append(EventKind::UserMessage {
            surface: SurfaceOp::Append,
            content: "hi".into(),
            images: Vec::new(),
        });
        append_runtime_context(&mut log, "test-model");
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
        append_runtime_context(&mut log, "m"); // seq 2
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
        append_runtime_context(&mut log, "m");
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
}
