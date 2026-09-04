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
//! until the preserved tail fills the policy's `retain_pct` of the prompt
//! budget (dsh's default 16 %). The tail is never allowed to *open* on a
//! `Tool` message, and the fold is never allowed to *close* on an assistant
//! message whose native `tool_calls` point into the tail — an orphaned half
//! of a call/result pair breaks native-tool-calling chat templates.
//!
//! **The summarisation call is a KV-cache-preserving prefix.** It replays the
//! conversation's own system prompt and the folded messages verbatim, then
//! appends the compaction directive as the *final user message* — so the
//! provider's prompt cache for the last real request is reused instead of
//! re-priced from scratch. The directive demands a fixed eight-section
//! checkpoint (dsh `dsh-compaction-basic`); a summary cut off by `max_tokens`
//! is discarded, never kept.

use llm::client::{ChatMessage, LlmClient};
use llm::tokenize::approx_tokens;
use sica_core::message::{Message, Role};
use sica_core::retain::{head_tail, notice};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

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
/// tool result (a 200 KB file dump) must not crowd out the rest of the
/// history. Applied per message in the folded wire form.
const MAX_EXCERPT_CHARS: usize = 2_000;

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

/// Preamble dsh stamps on the landed replacement. Tells the model what to do
/// with the checkpoint without ever acknowledging it mid-task.
pub const SUMMARY_PREAMBLE: &str =
    "Treat the captured context as established background and build on it \
     without restating it. Continue the task directly from the messages that \
     follow, without acknowledging this checkpoint.";

/// The eight-section checkpoint directive, appended as the final user message
/// of the summarisation call (dsh `compaction-basic`). Fixed sections, `(none)`
/// for empty ones, exact identifiers preserved, no mention that compaction is
/// happening — the summary must read as a neutral briefing.
pub const COMPACTION_INSTRUCTION: &str = "\
Summarize the conversation above into a checkpoint the conversation can \
continue from after the original messages are dropped.

Write terse markdown with EXACTLY these eight headings, in this order:

## Primary Request and Intent
## Key Technical Concepts
## Files and Code
## Errors and Fixes
## Pending Jobs
## Current Work
## Next Step
## Critical Context

Rules:
- Write `(none)` under a heading that has nothing to report.
- Preserve exact identifiers: file paths, commands, function names, values, \
error strings, URLs. Never paraphrase them.
- Capture corrections the user made and decisions taken, with their reasons.
- If an earlier checkpoint summary appears in the conversation, consolidate \
it into this one rather than mentioning it.
- Do not mention that the conversation is being summarized or compressed.
- Do not call any tools. Do not address the user. Output only the eight \
sections.";

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
/// against when no usage anchor exists, since it is what actually gets sent.
pub fn approx_total_wire(messages: &[ChatMessage]) -> u32 {
    messages
        .iter()
        .map(|m| approx_tokens(&m.content.text()) + 4)
        .sum()
}

/// Index at which the verbatim tail begins — everything before it is folded.
/// `None` when there is nothing worth folding, which the caller must treat as
/// "do not announce a compaction".
///
/// `retain_pct` is the share of the budget (percent) the tail may fill —
/// dsh's default is 16.
pub fn split_index(messages: &[Message], budget_tokens: u32, retain_pct: u32) -> Option<usize> {
    if messages.len() < MIN_FOLD + MIN_TAIL {
        return None;
    }
    let tail_budget =
        (budget_tokens as f32 * retain_pct.max(1) as f32 / 100.0).max(MIN_TAIL_TOKENS) as u32;
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
    // the matching `tool_calls`) is about to be folded, and the fold must
    // not close on an assistant message whose native `tool_calls` point into
    // the tail. Walking *backwards* only ever grows the tail, so it can
    // never invalidate either condition once met.
    loop {
        let opens_on_tool = split < messages.len() && messages[split].role == Role::Tool;
        let closes_on_pending_calls =
            split > 0 && messages[split - 1].tool_calls.is_some();
        if !opens_on_tool && !closes_on_pending_calls {
            break;
        }
        if split == 0 {
            break;
        }
        split -= 1;
    }

    if split < MIN_FOLD {
        None
    } else {
        Some(split)
    }
}

/// Summarise `folded` (the messages about to leave the model's view) into
/// the bare briefing text. **Prefix-preserving**: the request is the
/// conversation's own system prompt (`system_wire`, the same bytes the real
/// request sends) followed by the folded messages verbatim, then
/// [`COMPACTION_INSTRUCTION`] as the final user message — a cache prefix of
/// the last routed request.
///
/// `None` when every attempt produced nothing usable — the caller leaves the
/// history untouched and the trimmer takes over. A summary cut off by
/// `max_tokens` (`finish_reason == "length"`) fails closed and is retried
/// rather than kept. The caller frames the result with [`summary_message`]
/// and records it as a `CompactionSummary` event that shadows the folded span.
pub async fn summarize_fold(
    client: &LlmClient,
    policy: &protocol::CompactPolicy,
    system_wire: &[ChatMessage],
    folded: Vec<ChatMessage>,
    cancel: Option<CancellationToken>,
) -> Option<String> {
    let mut messages: Vec<ChatMessage> = Vec::with_capacity(system_wire.len() + folded.len() + 1);
    messages.extend_from_slice(system_wire);
    messages.extend(folded);
    messages.push(ChatMessage::text("user", COMPACTION_INSTRUCTION));

    // Per-message excerpt guard: one pathological entry must not crowd out
    // the rest of the fold. (The directive itself is exempt.)
    let folded_count = messages.len().saturating_sub(system_wire.len() + 1);
    for m in messages.iter_mut().skip(system_wire.len()).take(folded_count) {
        let text = m.content.text();
        if text.len() > MAX_EXCERPT_CHARS {
            m.content = llm::client::ChatContent::Text(excerpt(&text, MAX_EXCERPT_CHARS));
        }
    }

    let mut summarizer = client.clone();
    if summarizer.max_tokens.is_none() || summarizer.max_tokens.unwrap_or(0) < policy.max_tokens {
        summarizer.max_tokens = Some(policy.max_tokens);
    }

    for attempt in 0..=policy.retries {
        let Some((raw, finish_reason)) = stream_summary(&summarizer, messages.clone(), &cancel).await
        else {
            warn!(attempt, "compaction summarizer stream failed");
            continue;
        };
        if finish_reason.as_deref() == Some("length") {
            // Truncated summary fails closed: a checkpoint cut mid-sentence is
            // worse than the raw history it would replace.
            warn!(attempt, "compaction summary hit max_tokens — discarding");
            continue;
        }
        let summary = clean(&raw);
        if !summary.is_empty() {
            return Some(summary);
        }
        warn!(attempt, "compaction summarizer returned nothing");
    }
    None
}

/// The system-message text a compaction summary is stored and sent as: the
/// recognisable marker first (the FE matches on it), then the preamble, then
/// the summary in its `<compacted-summary>` frame — which a later compaction
/// is told to consolidate.
pub fn summary_message(summary: &str) -> String {
    format!(
        "{SUMMARY_PREFIX}\n\n{SUMMARY_PREAMBLE}\n\n<compacted-summary>\n{summary}\n</compacted-summary>"
    )
}

/// Middle-truncate so both the head and the tail of a long message survive —
/// a truncated-at-the-front tool result usually loses its conclusion, which is
/// the part worth summarising.
fn excerpt(text: &str, max_bytes: usize) -> String {
    let window = head_tail(text, max_bytes * 2 / 3, max_bytes / 3);
    window.render(&notice(window.omitted, ""))
}

/// Drive one summarisation attempt over the streaming endpoint. Returns the
/// accumulated text plus the finish reason, or `None` on a transport failure.
/// Interruptible: a fired token abandons the attempt.
async fn stream_summary(
    client: &LlmClient,
    messages: Vec<ChatMessage>,
    cancel: &Option<CancellationToken>,
) -> Option<(String, Option<String>)> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let client = client.clone();
    let stream_cancel = cancel.clone();
    let stream_task = tokio::spawn(async move {
        client.chat_stream(messages, None, tx, stream_cancel).await
    });

    let mut buf = String::new();
    let mut finish_reason = None;
    loop {
        let next = match cancel {
            Some(tok) => tokio::select! {
                biased;
                _ = tok.cancelled() => None,
                v = rx.recv() => v,
            },
            None => rx.recv().await,
        };
        let Some(chunk) = next else { break };
        buf.push_str(&chunk.delta_content);
        if chunk.finish_reason.is_some() {
            finish_reason = chunk.finish_reason;
        }
        if buf.len() > MAX_SUMMARY_CHARS * 2 {
            // Runaway model — take what we have and stop waiting.
            break;
        }
    }
    if cancel.as_ref().is_some_and(|t| t.is_cancelled()) {
        return None;
    }
    match stream_task.await {
        Ok(Ok(())) => Some((buf, finish_reason)),
        Ok(Err(e)) => {
            warn!(error = %e, "compaction summarizer request failed");
            None
        }
        Err(e) => {
            warn!(error = %e, "compaction summarizer task join failed");
            None
        }
    }
}

/// Strip leaked reasoning and cap the length. Mirrors `title_gen::clean`:
/// reasoning models that omit the opening `<think>` leave everything before a
/// trailing `</think>` as chain-of-thought, which must not become the summary.
fn clean(raw: &str) -> String {
    let body = match raw.rfind("</think>") {
        Some(idx) => &raw[idx + "</think>".len()..],
        None      => raw,
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

    const POLICY: protocol::CompactPolicy = protocol::CompactPolicy {
        threshold_pct: 80,
        retain_pct:    16,
        max_tokens:    8192,
        retries:       1,
    };

    #[test]
    fn short_history_is_not_worth_folding() {
        let msgs = vec![
            msg(Role::User, "hi"),
            msg(Role::Assistant, "hello"),
            msg(Role::User, "bye"),
        ];
        assert!(split_index(&msgs, 1000, POLICY.retain_pct).is_none());
    }

    #[test]
    fn roomy_budget_keeps_everything_in_the_tail() {
        let msgs: Vec<Message> = (0..10)
            .map(|i| msg(Role::User, &format!("m{i}")))
            .collect();
        // The whole history fits inside the tail share, so nothing is folded.
        assert!(split_index(&msgs, 1_000_000, POLICY.retain_pct).is_none());
    }

    #[test]
    fn folds_oldest_and_keeps_a_tail() {
        let msgs: Vec<Message> = (0..20)
            .map(|i| msg(Role::User, &long(&format!("m{i}"))))
            .collect();
        let split = split_index(&msgs, 2000, POLICY.retain_pct).expect("should fold");
        assert!(split >= MIN_FOLD, "split {split} folds too little");
        assert!(msgs.len() - split >= MIN_TAIL, "tail too short");
        assert!(split < msgs.len() - MIN_TAIL + 1);
    }

    #[test]
    fn larger_retain_keeps_a_larger_tail() {
        let msgs: Vec<Message> = (0..20)
            .map(|i| msg(Role::User, &long(&format!("m{i}"))))
            .collect();
        let s16 = split_index(&msgs, 2000, 16).unwrap();
        let s50 = split_index(&msgs, 2000, 50).unwrap();
        assert!(s50 <= s16, "a bigger retain share folds less");
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
            if let Some(split) = split_index(&msgs, budget, POLICY.retain_pct) {
                assert_ne!(
                    msgs[split].role,
                    Role::Tool,
                    "budget {budget} split the tail onto an orphaned tool result"
                );
            }
        }
    }

    #[test]
    fn fold_never_closes_on_pending_native_tool_calls() {
        // An assistant message with tool_calls must stay with its results —
        // in whichever half the results land.
        let mut msgs: Vec<Message> = (0..8)
            .map(|i| msg(Role::User, &long(&format!("m{i}"))))
            .collect();
        let mut with_calls = msg(Role::Assistant, &long("call"));
        with_calls.tool_calls = Some("[{\"id\":\"c1\"}]".into());
        msgs.push(with_calls);
        msgs.push(msg(Role::Tool, &long("result")));
        msgs.push(msg(Role::User, "latest"));
        for budget in [1500u32, 2000, 2500, 3000, 4000, 8000] {
            if let Some(split) = split_index(&msgs, budget, POLICY.retain_pct) {
                assert!(
                    msgs[split - 1].tool_calls.is_none(),
                    "budget {budget} closed the fold on an assistant message with pending tool_calls"
                );
            }
        }
    }

    #[test]
    fn summary_prefix_is_recognisable_by_the_frontend() {
        assert!(SUMMARY_PREFIX.starts_with(protocol::CONTEXT_SUMMARY_PREFIX));
    }

    #[test]
    fn summary_message_frames_the_checkpoint() {
        let m = summary_message("## Primary Request and Intent\n- x");
        assert!(m.starts_with(protocol::CONTEXT_SUMMARY_PREFIX));
        assert!(m.contains(SUMMARY_PREAMBLE));
        assert!(m.contains("<compacted-summary>\n## Primary Request"));
        assert!(m.ends_with("</compacted-summary>"));
    }

    #[test]
    fn compact_output_starts_with_the_summary_marker() {
        // `summarize_fold` needs a live client, so exercise the assembly
        // the same way `chat.rs` does after it.
        let msgs: Vec<Message> = (0..12)
            .map(|i| msg(Role::User, &long(&format!("m{i}"))))
            .collect();
        let split = split_index(&msgs, 2000, POLICY.retain_pct).unwrap();
        let mut out = vec![Message::system(summary_message("summary"))];
        out.extend_from_slice(&msgs[split..]);
        assert_eq!(out[0].role, Role::System);
        assert!(out[0].content.starts_with(protocol::CONTEXT_SUMMARY_PREFIX));
        assert_eq!(out.len(), msgs.len() - split + 1);
    }

    #[test]
    fn instruction_names_all_eight_sections() {
        for heading in [
            "## Primary Request and Intent",
            "## Key Technical Concepts",
            "## Files and Code",
            "## Errors and Fixes",
            "## Pending Jobs",
            "## Current Work",
            "## Next Step",
            "## Critical Context",
        ] {
            assert!(COMPACTION_INSTRUCTION.contains(heading), "missing {heading}");
        }
        assert!(COMPACTION_INSTRUCTION.contains("(none)"));
        assert!(COMPACTION_INSTRUCTION.contains("Do not call any tools"));
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
    fn clean_strips_leaked_reasoning() {
        assert_eq!(clean("thinking aloud\n</think>\n## Goal\n- ship it"), "## Goal\n- ship it");
    }

    #[test]
    fn clean_caps_length() {
        let raw = "z".repeat(MAX_SUMMARY_CHARS * 3);
        assert_eq!(clean(&raw).chars().count(), MAX_SUMMARY_CHARS);
    }
}