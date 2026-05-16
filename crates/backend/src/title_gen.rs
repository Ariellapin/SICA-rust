//! One-shot agent that asks the connected LLM for a short session title.
//! Runs after the very first assistant message of a session lands, only if
//! the session still has its placeholder title.

use llm::client::{ChatMessage, LlmClient};
use tokio::sync::mpsc;
use tracing::warn;

const SYSTEM_PROMPT: &str =
    "You produce concise chat titles. Reply with at most 5 words. \
     No punctuation, no quotes, no trailing period. Title case.";

const MAX_TITLE_LEN: usize = 60;

/// Drive a non-streaming-style chat completion (still uses the streaming
/// endpoint, but we just accumulate the deltas) and return a trimmed title.
/// Returns `None` on any error so the caller can leave the session at its
/// default name rather than surface a failure.
pub async fn summarize(client: &LlmClient, user_msg: &str, assistant_msg: &str) -> Option<String> {
    let prompt = format!(
        "User said:\n{user_msg}\n\nAssistant replied:\n{assistant_msg}\n\nTitle:"
    );
    let messages = vec![
        ChatMessage { role: "system".into(), content: SYSTEM_PROMPT.into() },
        ChatMessage { role: "user".into(), content: prompt },
    ];
    let (tx, mut rx) = mpsc::unbounded_channel();
    let client = client.clone();
    let stream_task = tokio::spawn(async move {
        client.chat_stream(messages, tx).await
    });

    let mut buf = String::new();
    while let Some(chunk) = rx.recv().await {
        buf.push_str(&chunk.delta_content);
        if buf.len() > 4 * MAX_TITLE_LEN {
            // Sanity cap so a runaway model can't make us wait forever.
            break;
        }
    }
    match stream_task.await {
        Ok(Ok(())) | Ok(Err(_)) => {}
        Err(e) => warn!(error = %e, "title-gen task join failed"),
    }

    let title = clean(&buf);
    if title.is_empty() { None } else { Some(title) }
}

fn clean(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches(|c: char| c == '"' || c == '\'' || c == '.');
    let first_line = trimmed.lines().next().unwrap_or("").trim();
    let truncated: String = first_line.chars().take(MAX_TITLE_LEN).collect();
    truncated.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::clean;

    #[test]
    fn strips_quotes_and_period() {
        assert_eq!(clean("\"Hello World.\""), "Hello World");
    }

    #[test]
    fn takes_first_line() {
        assert_eq!(clean("Title One\nIgnored"), "Title One");
    }

    #[test]
    fn caps_length() {
        let long = "a".repeat(120);
        assert_eq!(clean(&long).len(), 60);
    }
}
