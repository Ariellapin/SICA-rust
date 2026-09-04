//! Runs a single chat turn: opens an LLM stream, forwards `AssistantDelta`
//! events to the FE, and emits **live** `TokenUsage` updates every ~100 ms so
//! the token meter ticks during the response instead of jumping at the end
//! (which was the bug in the Python project).

use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::Event;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use llm::client::{ChatMessage, LlmClient};
use llm::tokenize::{approx_tokens, tokenize_exact};

use crate::agent::EventSink;

pub struct TurnInput {
    pub session_id: u64,
    pub turn_id:    u64,
    pub messages:   Vec<ChatMessage>,
    /// OpenAI-native tool definitions to send with the request; `None` for
    /// text-protocol tool calling.
    pub tools:      Option<serde_json::Value>,
    /// The model's full context window — the denominator of the raw
    /// `used / limit` readout.
    pub limit:      u32,
    /// Slice of `limit` available to the prompt (window minus the reply
    /// reserve). Reported alongside `used` so the FE's percentage matches the
    /// number auto-compaction triggers on.
    pub budget:     u32,
    /// Cancelled by `InterruptTurn`. When fired, the stream is dropped and
    /// the partial response is returned with `finish_reason = "interrupted"`.
    pub cancel:     Option<CancellationToken>,
}

/// One fully-accumulated native tool call from the response stream.
#[derive(Debug, Clone, Default)]
pub struct NativeToolCall {
    pub id:        String,
    pub name:      String,
    /// Raw JSON string of the arguments object, exactly as streamed.
    pub arguments: String,
}

/// Final accumulated state of one turn — what the caller needs to write
/// the assistant message back into session storage. Reasoning is split
/// out from content because `Message::reasoning` is its own field.
#[derive(Debug)]
pub struct TurnOutput {
    pub content:       String,
    pub reasoning:     String,
    pub finish_reason: String,
    /// Native tool calls emitted this turn (empty in text-protocol mode).
    pub tool_calls:    Vec<NativeToolCall>,
    /// Final prompt + reply token count — the provider's own `usage` sum
    /// when the stream carried one, else exact via `/tokenize` when the
    /// server offers it, else heuristic. The value the last live
    /// `TokenUsage` event carried.
    pub used_tokens:   u32,
    /// The provider's `usage` trailer, when it sent one.
    pub usage:         Option<llm::client::Usage>,
    /// Transport / server failure that ended the stream early (HTTP error,
    /// connection reset, SSE decode failure). `None` on a clean finish and
    /// on user interrupt. The caller decides whether it is retryable — a
    /// silent empty reply is indistinguishable from the model choosing to
    /// say nothing, so this must never be swallowed.
    pub error:         Option<anyhow::Error>,
}

pub async fn run_turn(
    client: LlmClient,
    events: Arc<dyn EventSink>,
    input: TurnInput,
) -> TurnOutput {
    let TurnInput { session_id, turn_id, messages, tools, limit, budget, cancel } = input;

    events.emit(Event::TurnStarted { session_id, turn_id });

    // Initial token count: try exact, fall back to heuristic. Image parts are
    // skipped — their cost is server-side and opaque to us.
    let prompt_concat = messages
        .iter()
        .map(|m| m.content.text())
        .collect::<Vec<_>>()
        .join("\n");
    let initial = match tokenize_exact(&client.base_url, &prompt_concat).await {
        Ok(n) => n,
        Err(_) => approx_tokens(&prompt_concat),
    };
    events.emit(Event::TokenUsage { session_id, used: initial, limit, budget });

    let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel();
    let client_clone = client.clone();
    let messages_for_stream = messages.clone();
    let stream_cancel = cancel.clone();
    let stream_handle = tokio::spawn(async move {
        client_clone
            .chat_stream(messages_for_stream, tools, chunk_tx, stream_cancel)
            .await
    });

    let mut running   = initial;
    let mut last_emit = Instant::now();
    let mut final_reason = String::from("stop");
    let mut accum_content   = String::new();
    let mut accum_reasoning = String::new();
    // Native tool calls accumulate by stream index: the first fragment for
    // an index carries id/name, later fragments append argument text.
    let mut accum_tools: Vec<NativeToolCall> = Vec::new();
    let mut usage: Option<llm::client::Usage> = None;

    let mut interrupted = false;
    loop {
        let next = match &cancel {
            Some(tok) => tokio::select! {
                biased;
                _ = tok.cancelled() => {
                    interrupted = true;
                    None
                }
                v = chunk_rx.recv() => v,
            },
            None => chunk_rx.recv().await,
        };
        let Some(chunk) = next else { break };
        if !chunk.delta_content.is_empty() || !chunk.delta_reasoning.is_empty() {
            events.emit(Event::AssistantDelta {
                session_id,
                turn_id,
                content:   chunk.delta_content.clone(),
                reasoning: chunk.delta_reasoning.clone(),
            });
            accum_content.push_str(&chunk.delta_content);
            accum_reasoning.push_str(&chunk.delta_reasoning);
            running = running.saturating_add(
                approx_tokens(&chunk.delta_content) + approx_tokens(&chunk.delta_reasoning),
            );
        }
        for tc in &chunk.delta_tool_calls {
            let idx = tc.index as usize;
            while accum_tools.len() <= idx {
                accum_tools.push(NativeToolCall::default());
            }
            let slot = &mut accum_tools[idx];
            if let Some(id) = &tc.id {
                slot.id = id.clone();
            }
            if let Some(name) = &tc.name {
                slot.name.push_str(name);
            }
            slot.arguments.push_str(&tc.arguments);
        }
        if let Some(reason) = chunk.finish_reason {
            final_reason = reason;
        }
        if let Some(u) = chunk.usage {
            usage = Some(u);
        }
        if last_emit.elapsed() >= Duration::from_millis(100) {
            events.emit(Event::TokenUsage { session_id, used: running, limit, budget });
            last_emit = Instant::now();
        }
    }

    if interrupted {
        final_reason = "interrupted".into();
        // Drop the receiver so the stream task's `tx.send` returns Err and it
        // unwinds promptly without us blocking on its handle.
        drop(chunk_rx);
    }
    // An interrupt makes `chat_stream` return `Ok(())` (its send fails and it
    // stops), so a stream error can only be reported for a turn the user did
    // not stop — an interrupt never masquerades as a failure.
    let error = match stream_handle.await {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e),
        Err(join) => Some(anyhow::anyhow!("chat_stream task panicked: {join}")),
    }
    .filter(|_| !interrupted);
    if let Some(e) = &error {
        warn!(error = %e, "chat_stream failed");
        final_reason = "error".into();
    }

    // Reasoning models that omit the opening `<think>` (it lives in the prompt
    // template) leak their reasoning into `content` with only a trailing
    // `</think>`. The streaming splitter can't catch that mid-stream, so peel
    // it back out of the accumulated text here — keeps the persisted message,
    // the reasoning chip, and the auto-title agent all seeing the right halves.
    if accum_reasoning.is_empty() {
        if let Some((content, reasoning)) =
            llm::streaming::split_orphan_reasoning(&accum_content)
        {
            accum_content = content;
            accum_reasoning = reasoning;
        }
    }

    // Final correction. The provider's own accounting wins when it sent
    // one — it counts the real chat template, tool schemas and image parts,
    // none of which `/tokenize` on the concatenated text can see. Otherwise
    // an exact tokenize of the full transcript (prompt + assistant), else
    // the heuristic.
    let final_used = match usage {
        Some(u) if u.total() > 0 => u.total(),
        _ => {
            let full = format!("{prompt_concat}\n{accum_content}\n{accum_reasoning}");
            tokenize_exact(&client.base_url, &full)
                .await
                .unwrap_or_else(|_| approx_tokens(&full))
        }
    };
    events.emit(Event::TokenUsage { session_id, used: final_used, limit, budget });

    events.emit(Event::TurnFinished {
        session_id,
        turn_id,
        finish_reason: final_reason.clone(),
    });
    info!(session_id, turn_id, "turn finished");

    // Drop empty slots (defensive: a server that skips indices would leave
    // nameless placeholders behind).
    accum_tools.retain(|t| !t.name.is_empty());

    TurnOutput {
        content:       accum_content,
        reasoning:     accum_reasoning,
        finish_reason: final_reason,
        tool_calls:    accum_tools,
        used_tokens:   final_used,
        usage,
        error,
    }
}
