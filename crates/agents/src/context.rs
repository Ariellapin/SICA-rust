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

/// Trim `messages` to fit `budget_tokens` (approximate). The leading system
/// message (if any) and the final message are never dropped. Oldest
/// non-system messages go first; when anything was dropped, a short user-role
/// marker is inserted right after the system message so the model knows.
pub fn trim_to_budget(messages: Vec<ChatMessage>, budget_tokens: u32) -> TrimReport {
    let total = |msgs: &[ChatMessage]| -> u32 {
        msgs.iter().map(|m| approx_tokens(&m.content.text()) + 4).sum()
    };

    if total(&messages) <= budget_tokens {
        return TrimReport { messages, dropped: 0 };
    }

    let mut msgs = messages;
    let sys_count = usize::from(
        msgs.first().map(|m| m.role == "system").unwrap_or(false),
    );
    let mut dropped = 0usize;

    // Drop oldest first, always keeping the system prompt and at least the
    // final message (the user's current request).
    while total(&msgs) > budget_tokens && msgs.len() > sys_count + 1 {
        msgs.remove(sys_count);
        dropped += 1;
    }

    if dropped > 0 {
        msgs.insert(
            sys_count,
            ChatMessage {
                role: "user".into(),
                content: ChatContent::Text(format!(
                    "[context notice: the {dropped} oldest message(s) of this \
                     conversation were removed to fit the model's context \
                     window. Do not assume their contents.]"
                )),
                tool_calls: None,
                tool_call_id: None,
            },
        );
    }

    TrimReport { messages: msgs, dropped }
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
    fn never_drops_final_message() {
        let huge = "x".repeat(4000);
        let msgs = vec![msg("user", &huge)];
        let r = trim_to_budget(msgs, 10);
        assert_eq!(r.dropped, 0);
        assert_eq!(r.messages.len(), 1);
    }
}
