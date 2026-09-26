//! Context-window management: trim the oldest conversation turns when the
//! assembled history would overflow the model's prompt budget.
//!
//! Without this, `build_history` sends the entire session every turn; once
//! the sum crosses the server's context length the request either errors or
//! the server silently drops the *front* of the prompt — which is where the
//! system message with the tool contract lives. Trimming from the oldest
//! user/assistant messages instead keeps the contract and the recent
//! conversation intact and inserts a visible marker so the model knows
//! history is missing rather than hallucinating it.

use llm::client::{ChatMessage, ChatContent};
use llm::tokenize::approx_tokens;

/// Result of a trim pass: the (possibly shortened) history and how many
/// messages were dropped.
pub struct TrimReport {
    pub messages: Vec<ChatMessage>,
    pub dropped:  usize,
}

/// Prefix of the trimmer's wire-only marker. Exported so a reader of a
/// request can tell that message apart from anything the log holds — it is
/// inserted here and never appended to a session.
pub const CONTEXT_NOTICE_PREFIX: &str = "[context notice:";

/// Trim `messages` to fit `budget_tokens` (approximate). The leading system
/// message (if any), any compaction checkpoint(s) right after it, and the
/// final message are never dropped. Oldest messages after that protected
/// head go first, in *pairs* where native tool calling made them one — an
/// assistant message carrying `tool_calls` leaves with the `tool` results
/// that answer it, and never one without the other, because a template
/// given half a pair raises rather than tolerates it. When anything was
/// dropped, a short user-role marker is inserted right after the protected
/// head so the model knows.
pub fn trim_to_budget(messages: Vec<ChatMessage>, budget_tokens: u32) -> TrimReport {
    let total = |msgs: &[ChatMessage]| -> u32 {
        msgs.iter().map(|m| approx_tokens(&m.content.text()) + 4).sum()
    };

    if total(&messages) <= budget_tokens {
        return TrimReport { messages, dropped: 0 };
    }

    let mut msgs = messages;
    let head = protected_head(&msgs);
    let summaries = head - usize::from(msgs.first().is_some_and(|m| m.role == "system"));
    let mut dropped = 0usize;

    // Drop the oldest unit first, always keeping the protected head and at
    // least the final message (the user's current request).
    while total(&msgs) > budget_tokens && msgs.len() > head + 1 {
        let unit = drop_unit_len(&msgs[head..]);
        if head + unit >= msgs.len() {
            // The unit reaches the final message; it cannot go.
            break;
        }
        msgs.drain(head..head + unit);
        dropped += unit;
    }

    if dropped > 0 {
        let after = if summaries > 0 { " after the context summary" } else { "" };
        msgs.insert(
            head,
            ChatMessage {
                role: "user".into(),
                content: ChatContent::Text(format!(
                    "{CONTEXT_NOTICE_PREFIX} the {dropped} oldest message(s) of this \
                     conversation{after} were removed to fit the model's context \
                     window. Do not assume their contents.]"
                )),
                tool_calls: None,
                tool_call_id: None,
            },
        );
    }

    TrimReport { messages: msgs, dropped }
}

/// Number of leading messages the trimmer must keep: the system prompt and
/// every compaction checkpoint that directly follows it. A checkpoint is
/// what the folded history became; dropping it first — which "oldest
/// first" would do, since on the wire it is a `user` message right after
/// the system prompt — throws away the whole compressed past to save one
/// message's worth of tokens.
fn protected_head(msgs: &[ChatMessage]) -> usize {
    let mut head = usize::from(msgs.first().is_some_and(|m| m.role == "system"));
    while msgs
        .get(head)
        .is_some_and(|m| m.content.text().starts_with(protocol::CONTEXT_SUMMARY_PREFIX))
    {
        head += 1;
    }
    head
}

/// How many messages, starting at `msgs[0]`, leave together. An assistant
/// message with native `tool_calls` takes the run of `tool` messages that
/// answers it; a stray `tool` message takes the rest of its run; anything
/// else is one message.
fn drop_unit_len(msgs: &[ChatMessage]) -> usize {
    let Some(first) = msgs.first() else { return 0 };
    let paired = first.tool_calls.is_some() || first.role == "tool";
    if !paired {
        return 1;
    }
    1 + msgs[1..].iter().take_while(|m| m.role == "tool").count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage::text(role, content)
    }

    #[test]
    fn under_budget_is_untouched() {
        let msgs = vec![msg("system", "sys"), msg("user", "hi")];
        let r = trim_to_budget(msgs, 1000);
        assert_eq!(r.dropped, 0);
        assert_eq!(r.messages.len(), 2);
    }

    #[test]
    fn drops_oldest_keeps_system_and_last() {
        let long = "x".repeat(400); // ~100 tokens each
        let msgs = vec![
            msg("system", "sys"),
            msg("user", &long),
            msg("assistant", &long),
            msg("user", "latest question"),
        ];
        // Budget fits system + marker + last message only.
        let r = trim_to_budget(msgs, 60);
        assert!(r.dropped >= 2);
        assert_eq!(r.messages[0].role, "system");
        assert!(r.messages[1].content.text().contains("context notice"));
        assert_eq!(
            r.messages.last().unwrap().content.text(),
            "latest question"
        );
    }

    #[test]
    fn the_compaction_summary_is_protected() {
        let long = "x".repeat(400);
        let summary = format!("{} folded history", protocol::CONTEXT_SUMMARY_PREFIX);
        let msgs = vec![
            msg("system", "sys"),
            msg("user", &summary),
            msg("user", &long),
            msg("assistant", &long),
            msg("user", "latest question"),
        ];
        // Fits system + summary + notice + last message only.
        let r = trim_to_budget(msgs, 80);
        assert_eq!(r.dropped, 2);
        assert_eq!(r.messages[0].role, "system");
        assert_eq!(r.messages[1].content.text(), summary);
        assert!(r.messages[2].content.text().contains("after the context summary"));
        assert_eq!(r.messages.last().unwrap().content.text(), "latest question");
    }

    #[test]
    fn a_native_pair_is_never_split() {
        let long = "x".repeat(400);
        let mut call = msg("assistant", &long);
        call.tool_calls = Some(serde_json::json!([{"id": "c1"}]));
        let mut result_a = msg("tool", &long);
        result_a.tool_call_id = Some("c1".into());
        let mut result_b = msg("tool", &long);
        result_b.tool_call_id = Some("c2".into());
        let msgs = vec![
            msg("system", "sys"),
            msg("user", &long),
            call,
            result_a,
            result_b,
            msg("assistant", &long),
            msg("user", "latest"),
        ];
        for budget in [60u32, 150, 250, 350, 450] {
            let r = trim_to_budget(msgs.clone(), budget);
            let kept = &r.messages;
            for (i, m) in kept.iter().enumerate() {
                if m.tool_calls.is_some() {
                    assert_eq!(kept[i + 1].role, "tool", "budget {budget}: call lost its results");
                    assert_eq!(kept[i + 2].role, "tool", "budget {budget}: call lost a result");
                }
                if m.role == "tool" {
                    assert!(
                        kept[i - 1].role == "tool" || kept[i - 1].tool_calls.is_some(),
                        "budget {budget}: orphaned tool result at {i}"
                    );
                }
            }
            assert_eq!(kept.last().unwrap().content.text(), "latest");
        }
    }

    #[test]
    fn a_pair_that_reaches_the_final_message_stays() {
        let mut call = msg("assistant", &"x".repeat(400));
        call.tool_calls = Some(serde_json::json!([{"id": "c1"}]));
        let mut result = msg("tool", &"x".repeat(400));
        result.tool_call_id = Some("c1".into());
        let r = trim_to_budget(vec![msg("system", "sys"), call, result], 10);
        assert_eq!(r.dropped, 0);
        assert_eq!(r.messages.len(), 3);
    }

    #[test]
    fn never_drops_final_message() {
        let huge = "x".repeat(4000);
        let msgs = vec![msg("user", &huge)];
        let r = trim_to_budget(msgs, 10);
        assert_eq!(r.dropped, 0);
        assert_eq!(r.messages.len(), 1);
    }
}
