//! Automatic context compression: fold the older part of a session's history
//! into a single LLM-written summary so the conversation can keep going once
//! the prompt approaches the model's window.
//!
//! This sits *in front of* [`crate::context::trim_to_budget`]. Trimming is
//! lossy amputation — it deletes the oldest messages outright and leaves a
//! "do not assume their contents" marker. Compaction instead asks the model to
//! write a dense briefing of what it is about to forget, keeps the recent tail
//! verbatim, and hands the summary back as a system message. Trimming stays as
//! the backstop for the case where even the compacted history is too large (or
//! the summarizer round-trip fails).
//!
//! The split point is chosen by walking backwards from the newest message
//! until the preserved tail fills [`TAIL_SHARE`] of the prompt budget. The tail
//! is never allowed to *open* on a `Tool` message: its originating assistant
//! message carries the matching `tool_calls`, and an orphaned tool result
//! breaks native-tool-calling chat templates.

use llm::client::{ChatMessage, LlmClient};
use llm::tokenize::approx_tokens;
use sica_core::message::{Message, Role};
use sica_core::retain::{head_tail, notice};
use tokio::sync::mpsc;
use tracing::warn;

/// Share of the prompt budget the verbatim tail is allowed to keep. The rest
/// of the budget is left for the summary plus the growing new conversation.
const TAIL_SHARE: f32 = 0.35;

/// Floor on the tail budget so a tiny window still keeps a usable tail.
const MIN_TAIL_TOKENS: f32 = 512.0;

/// Never fold fewer than this many messages — a small fold costs a full
/// summarizer round-trip and buys back almost nothing.
const MIN_FOLD: usize = 4;

/// Always keep at least this many trailing messages verbatim (the current user
/// request and the exchange it follows).
pub const MIN_TAIL: usize = 2;

/// A stored tool result longer than this (bytes of the raw summary) is
/// pruned to a head/tail window before compaction pays for a summariser
/// call. Mirrors dsh's `thresholdChars 8192`.
pub const PRUNE_THRESHOLD: usize = 8192;
/// Bytes kept at the front of a pruned tool result.
pub const PRUNE_HEAD: usize = 4096;
/// Bytes kept at the end of a pruned tool result.
pub const PRUNE_TAIL: usize = 1024;

/// The pruned form of an oversized tool result, or `None` when it is
/// already under [`PRUNE_THRESHOLD`]. A pruned result is always shorter
/// than the threshold, so pruning is idempotent. No model call.
pub fn prune_summary(summary: &str) -> Option<String> {
    if summary.len() <= PRUNE_THRESHOLD {
        return None;
    }
    let window = head_tail(summary, PRUNE_HEAD, PRUNE_TAIL);
    Some(window.render(&notice(
        window.omitted,
        "middle of this older tool result was pruned to free context; \
         re-run the tool if you need it",
    )))
}

/// Per-message cap on what goes into the summarizer prompt. One pathological
/// tool result (a 200 KB file dump) must not crowd out the rest of the history.
const MAX_EXCERPT_CHARS: usize = 2_000;

/// Cap on the whole summarizer prompt. Older text is dropped first — a summary
/// of the recent-but-folded history beats no summary at all.
const MAX_TRANSCRIPT_CHARS: usize = 48_000;

/// Sanity cap on the summary itself, so a runaway model can't hand back
/// something larger than what it replaced.
const MAX_SUMMARY_CHARS: usize = 8_000;

/// Marker prefixed to the summary message. Also the signal that a history has
/// already been compacted at least once. Opens with
/// [`protocol::CONTEXT_SUMMARY_PREFIX`] so the frontend can recognise it when
/// rebuilding a transcript from disk.
pub const SUMMARY_PREFIX: &str =
    "[context summary] Earlier conversation was compressed to fit the model's \
     context window. Treat the briefing below as established fact; do not \
     assume anything beyond it about the discarded messages.";

const SYSTEM_PROMPT: &str = "\
You compress conversation history for an AI coding agent that is running out of \
context window. Rewrite the transcript you are given as a dense factual briefing \
the agent can work from after the original messages are discarded.

Preserve: the user's goals and explicit instructions; decisions taken and the \
reasons for them; concrete identifiers (file paths, commands, function names, \
values, error messages); tool results that still matter; and anything left \
unfinished.

Drop: pleasantries, restated text, superseded attempts, and narration.

Answer with terse markdown under exactly these headings: `## Goal`, \
`## Decisions`, `## Facts`, `## Open items`. Use bullet points. Never invent \
detail that is not in the transcript, and never address the user directly.";

/// Approximate token cost of one persisted message, matching the accounting
/// [`crate::context::trim_to_budget`] uses (+4 for per-message role/framing
/// overhead) so the trigger and the trimmer agree on what "full" means.
pub fn msg_tokens(m: &Message) -> u32 {
    approx_tokens(&m.content) + 4
}

/// Approximate token cost of a persisted history.
pub fn approx_total(messages: &[Message]) -> u32 {
    messages.iter().map(msg_tokens).sum()
}

/// Approximate token cost of an assembled wire history (includes the system
/// preamble `build_history` prepends). This is what the trigger is measured
/// against, since it is what actually gets sent.
pub fn approx_total_wire(messages: &[ChatMessage]) -> u32 {
    messages
        .iter()
        .map(|m| approx_tokens(&m.content.text()) + 4)
        .sum()
}

/// Index at which the verbatim tail begins — everything before it is folded.
/// `None` when there is nothing worth folding, which the caller must treat as
/// "do not announce a compaction".
pub fn split_index(messages: &[Message], budget_tokens: u32) -> Option<usize> {
    if messages.len() < MIN_FOLD + MIN_TAIL {
        return None;
    }
    let tail_budget = (budget_tokens as f32 * TAIL_SHARE).max(MIN_TAIL_TOKENS) as u32;
    // Highest split that still leaves MIN_TAIL messages verbatim. `messages`
    // is at least MIN_FOLD + MIN_TAIL long, so this is a valid index.
    let max_split = messages.len() - MIN_TAIL;

    // Walk backwards, taking messages into the tail until it is full.
    let mut acc = 0u32;
    let mut split = messages.len();
    while split > 0 {
        let cost = msg_tokens(&messages[split - 1]);
        if acc.saturating_add(cost) > tail_budget && split <= max_split {
            break;
        }
        acc = acc.saturating_add(cost);
        split -= 1;
    }
    split = split.min(max_split);

    // The tail must not open on a tool result whose assistant message (with
    // the matching `tool_calls`) is about to be folded. Walking *backwards*
    // only ever grows the tail, so it can never invalidate the split.
    while split > 0 && messages[split].role == Role::Tool {
        split -= 1;
    }

    if split < MIN_FOLD {
        None
    } else {
        Some(split)
    }
}

/// Summarise `folded` (the messages about to leave the model's view) into
/// the bare briefing text. `None` when the summarizer produced nothing
/// usable — the caller leaves the history untouched and the trimmer takes
/// over. The caller frames the result with [`summary_message`] and records
/// it as a `CompactionSummary` event that shadows the folded span.
pub async fn summarize_fold(client: &LlmClient, folded: &[Message]) -> Option<String> {
    let transcript = render_transcript(folded);
    summarize(client, &transcript).await
}

/// The system-message text a compaction summary is stored and sent as.
pub fn summary_message(summary: &str) -> String {
    format!("{SUMMARY_PREFIX}\n\n{summary}")
}

/// Flatten the folded messages into a plain `ROLE: text` transcript for the
/// summarizer. Each message is excerpted, and if the whole thing still exceeds
/// [`MAX_TRANSCRIPT_CHARS`] the *oldest* entries are dropped first.
fn render_transcript(messages: &[Message]) -> String {
    let mut entries: Vec<String> = Vec::with_capacity(messages.len());
    for m in messages {
        let role = match m.role {
            Role::User => "USER",
            Role::Assistant => "ASSISTANT",
            Role::System => "SYSTEM",
            Role::Tool => "TOOL RESULT",
        };
        let body = excerpt(m.content.trim(), MAX_EXCERPT_CHARS);
        if body.is_empty() {
            continue;
        }
        let mut entry = format!("{role}: {body}");
        if !m.images.is_empty() {
            entry.push_str(&format!("\n[{} image attachment(s)]", m.images.len()));
        }
        entries.push(entry);
    }

    let mut total: usize = entries.iter().map(|e| e.len() + 2).sum();
    let mut start = 0usize;
    while total > MAX_TRANSCRIPT_CHARS && start + 1 < entries.len() {
        total -= entries[start].len() + 2;
        start += 1;
    }
    entries[start..].join("\n\n")
}

/// Middle-truncate so both the head and the tail of a long message survive —
/// a truncated-at-the-front tool result usually loses its conclusion, which is
/// the part worth summarising.
fn excerpt(text: &str, max_bytes: usize) -> String {
    let window = head_tail(text, max_bytes * 2 / 3, max_bytes / 3);
    window.render(&notice(window.omitted, ""))
}

/// Drive the summarizer over the streaming endpoint (same shape as
/// `title_gen`) and return the cleaned summary. `None` on any failure so the
/// caller can fall back to trimming rather than surface an error.
async fn summarize(client: &LlmClient, transcript: &str) -> Option<String> {
    let messages = vec![
        ChatMessage::text("system", SYSTEM_PROMPT),
        ChatMessage::text(
            "user",
            format!("Transcript to compress:\n\n{transcript}\n\nBriefing:"),
        ),
    ];
    let (tx, mut rx) = mpsc::unbounded_channel();
    let client = client.clone();
    let stream_task = tokio::spawn(async move { client.chat_stream(messages, None, tx, None).await });

    let mut buf = String::new();
    while let Some(chunk) = rx.recv().await {
        buf.push_str(&chunk.delta_content);
        if buf.len() > MAX_SUMMARY_CHARS * 2 {
            // Runaway model — take what we have and stop waiting.
            break;
        }
    }
    match stream_task.await {
        Ok(Ok(())) | Ok(Err(_)) => {}
        Err(e) => warn!(error = %e, "compaction summarizer task join failed"),
    }

    let summary = clean(&buf);
    if summary.is_empty() {
        None
    } else {
        Some(summary)
    }
}

/// Strip leaked reasoning and cap the length. Mirrors `title_gen::clean`:
/// reasoning models that omit the opening `<think>` leave everything before a
/// trailing `</think>` as chain-of-thought, which must not become the summary.
fn clean(raw: &str) -> String {
    let body = match raw.rfind("</think>") {
        Some(idx) => &raw[idx + "</think>".len()..],
        None => raw,
    };
    let trimmed = body.trim();
    if trimmed.chars().count() <= MAX_SUMMARY_CHARS {
        return trimmed.to_string();
    }
    trimmed.chars().take(MAX_SUMMARY_CHARS).collect::<String>().trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.to_string(),
            reasoning: None,
            images: Vec::new(),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// ~100 tokens per message.
    fn long(tag: &str) -> String {
        format!("{tag}{}", "x".repeat(400))
    }

    #[test]
    fn short_history_is_not_worth_folding() {
        let msgs = vec![
            msg(Role::User, "hi"),
            msg(Role::Assistant, "hello"),
            msg(Role::User, "bye"),
        ];
        assert!(split_index(&msgs, 1000).is_none());
    }

    #[test]
    fn roomy_budget_keeps_everything_in_the_tail() {
        let msgs: Vec<Message> = (0..10)
            .map(|i| msg(Role::User, &format!("m{i}")))
            .collect();
        // The whole history fits inside the tail share, so nothing is folded.
        assert!(split_index(&msgs, 1_000_000).is_none());
    }

    #[test]
    fn folds_oldest_and_keeps_a_tail() {
        let msgs: Vec<Message> = (0..20)
            .map(|i| msg(Role::User, &long(&format!("m{i}"))))
            .collect();
        // Tail share of 2000 is 700 tokens ≈ 6 messages.
        let split = split_index(&msgs, 2000).expect("should fold");
        assert!(split >= MIN_FOLD, "split {split} folds too little");
        assert!(msgs.len() - split >= MIN_TAIL, "tail too short");
        assert!(split < msgs.len() - MIN_TAIL + 1);
    }

    #[test]
    fn tail_never_opens_on_a_tool_result() {
        // Build a history whose natural split lands on a run of tool results.
        let mut msgs: Vec<Message> = (0..8)
            .map(|i| msg(Role::User, &long(&format!("m{i}"))))
            .collect();
        for i in 0..6 {
            msgs.push(msg(Role::Assistant, &long(&format!("a{i}"))));
            msgs.push(msg(Role::Tool, &long(&format!("t{i}"))));
        }
        for budget in [1500u32, 2000, 3000, 4000, 6000] {
            if let Some(split) = split_index(&msgs, budget) {
                assert_ne!(
                    msgs[split].role,
                    Role::Tool,
                    "budget {budget} split the tail onto an orphaned tool result"
                );
            }
        }
    }

    #[test]
    fn summary_prefix_is_recognisable_by_the_frontend() {
        assert!(SUMMARY_PREFIX.starts_with(protocol::CONTEXT_SUMMARY_PREFIX));
    }

    #[test]
    fn compact_output_starts_with_the_summary_marker() {
        // `summarize_fold` needs a live client, so exercise the assembly
        // the same way `chat.rs` does after it.
        let msgs: Vec<Message> = (0..12)
            .map(|i| msg(Role::User, &long(&format!("m{i}"))))
            .collect();
        let split = split_index(&msgs, 2000).unwrap();
        let mut out = vec![Message::system(summary_message("summary"))];
        out.extend_from_slice(&msgs[split..]);
        assert_eq!(out[0].role, Role::System);
        assert!(out[0].content.starts_with(protocol::CONTEXT_SUMMARY_PREFIX));
        assert_eq!(out.len(), msgs.len() - split + 1);
    }

    #[test]
    fn excerpt_keeps_head_and_tail() {
        let text = format!("START{}END", "x".repeat(5000));
        let e = excerpt(&text, 300);
        assert!(e.starts_with("START"));
        assert!(e.ends_with("END"));
        assert!(e.contains("bytes omitted"));
        assert!(e.chars().count() < text.chars().count());
    }

    #[test]
    fn prune_leaves_small_results_alone_and_is_idempotent() {
        assert!(prune_summary("small").is_none());
        let big = format!("HEAD{}TAIL", "m".repeat(PRUNE_THRESHOLD * 2));
        let pruned = prune_summary(&big).expect("over threshold");
        assert!(pruned.starts_with("HEAD"));
        assert!(pruned.ends_with("TAIL"));
        assert!(pruned.contains("pruned to free context"));
        assert!(pruned.len() <= PRUNE_THRESHOLD, "{}", pruned.len());
        assert!(prune_summary(&pruned).is_none(), "a pruned result must not prune again");
    }

    #[test]
    fn transcript_drops_oldest_when_oversized() {
        let msgs: Vec<Message> = (0..200)
            .map(|i| msg(Role::User, &format!("m{i} {}", "y".repeat(1500))))
            .collect();
        let t = render_transcript(&msgs);
        assert!(t.len() <= MAX_TRANSCRIPT_CHARS + MAX_EXCERPT_CHARS * 2);
        // The newest entry always survives; the oldest is the one dropped.
        assert!(t.contains("m199"));
        assert!(!t.contains("m0 "));
    }

    #[test]
    fn clean_strips_leaked_reasoning() {
        assert_eq!(clean("thinking aloud\n</think>\n## Goal\n- ship it"), "## Goal\n- ship it");
    }

    #[test]
    fn clean_caps_length() {
        let raw = "z".repeat(MAX_SUMMARY_CHARS * 3);
        assert_eq!(clean(&raw).chars().count(), MAX_SUMMARY_CHARS);
    }
}
