//! Runs a single chat turn: opens an LLM stream, forwards `AssistantDelta`
//! events to the FE, and emits **live** `TokenUsage` updates every ~100 ms so
//! the token meter ticks during the response instead of jumping at the end
//! (which was the bug in the Python project).

use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::Event;
use tokio::sync::mpsc;
use tracing::{info, warn};

use llm::client::{ChatMessage, LlmClient};
use llm::tokenize::{approx_tokens, tokenize_exact};

use crate::agent::EventSink;

pub struct TurnInput {
    pub session_id: u64,
    pub turn_id:    u64,
    pub messages:   Vec<ChatMessage>,
    pub limit:      u32,
}

pub async fn run_turn(
    client: LlmClient,
    events: Arc<dyn EventSink>,
    input: TurnInput,
) {
    let TurnInput { session_id, turn_id, messages, limit } = input;

    events.emit(Event::TurnStarted { session_id, turn_id });

    // Initial token count: try exact, fall back to heuristic.
    let prompt_concat = messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let initial = match tokenize_exact(&client.base_url, &prompt_concat).await {
        Ok(n) => n,
        Err(_) => approx_tokens(&prompt_concat),
    };
    events.emit(Event::TokenUsage { session_id, used: initial, limit });

    let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel();
    let client_clone = client.clone();
    let messages_for_stream = messages.clone();
    let stream_handle = tokio::spawn(async move {
        if let Err(e) = client_clone.chat_stream(messages_for_stream, chunk_tx).await {
            warn!(error = %e, "chat_stream failed");
        }
    });

    let mut running   = initial;
    let mut last_emit = Instant::now();
    let mut final_reason = String::from("stop");
    let mut accum_content   = String::new();
    let mut accum_reasoning = String::new();

    while let Some(chunk) = chunk_rx.recv().await {
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
        if let Some(reason) = chunk.finish_reason {
            final_reason = reason;
        }
        if last_emit.elapsed() >= Duration::from_millis(100) {
            events.emit(Event::TokenUsage { session_id, used: running, limit });
            last_emit = Instant::now();
        }
    }

    let _ = stream_handle.await;

    // Final correction: exact tokenize of full transcript (prompt + assistant).
    let full = format!("{prompt_concat}\n{accum_content}\n{accum_reasoning}");
    let final_used = tokenize_exact(&client.base_url, &full)
        .await
        .unwrap_or_else(|_| approx_tokens(&full));
    events.emit(Event::TokenUsage { session_id, used: final_used, limit });

    events.emit(Event::TurnFinished {
        session_id,
        turn_id,
        finish_reason: final_reason,
    });
    info!(session_id, turn_id, "turn finished");
}
