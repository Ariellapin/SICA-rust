//! Session titles: a deterministic fallback from the first user message,
//! then a one-shot agent that asks the connected LLM for something better.
//!
//! The fallback lands the moment the message is sent, so the sidebar never
//! shows "Session 63" for a session that has content. The LLM title runs
//! after the first assistant message and overwrites it, under input and
//! output budgets so a chatty model or a hung server cannot hold the task.

use std::time::Duration;

use llm::client::{ChatMessage, LlmClient};
use tokio::sync::mpsc;
use tracing::warn;

const SYSTEM_PROMPT: &str =
    "You produce concise chat titles. Reply with at most 5 words. \
     No punctuation, no quotes, no trailing period. Title case.";

const MAX_TITLE_LEN: usize = 60;

/// Words of the first message a fallback title keeps.
pub const FALLBACK_MAX_WORDS: usize = 5;
/// Bytes a fallback title keeps (after the word cut).
pub const FALLBACK_MAX_BYTES: usize = 40;
/// Bytes of each transcript half the LLM titler is shown.
const MAX_INPUT_BYTES: usize = 4096;
/// Completion cap on the title request.
const MAX_OUTPUT_TOKENS: u32 = 64;
/// Wall clock for the whole title round-trip.
const TIMEOUT: Duration = Duration::from_secs(60);

/// A title made from the message itself: the first five words, cut to 40
/// bytes on a char boundary, whitespace collapsed. Empty when the message
/// has no words (an image-only send), in which case the caller keeps the
/// placeholder.
pub fn fallback(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().take(FALLBACK_MAX_WORDS).collect();
    let joined = words.join(" ");
    let cut = sica_core::retain::utf8_head(&joined, FALLBACK_MAX_BYTES);
    // Don't end on a half word if the byte cut landed mid-word and there is
    // an earlier boundary to fall back to.
    let trimmed = if cut.len() < joined.len() {
        match cut.rfind(' ') {
            Some(i) if i > 0 => &cut[..i],
            _ => cut,
        }
    } else {
        cut
    };
    trimmed.trim().to_string()
}

/// Drive a non-streaming-style chat completion (still uses the streaming
/// endpoint, but we just accumulate the deltas) and return a trimmed title.
/// Returns `None` on any error so the caller can leave the session at its
/// fallback name rather than surface a failure.
pub async fn summarize(client: &LlmClient, user_msg: &str, assistant_msg: &str) -> Option<String> {
    let user_msg = sica_core::retain::utf8_head(user_msg, MAX_INPUT_BYTES);
    let assistant_msg = sica_core::retain::utf8_head(assistant_msg, MAX_INPUT_BYTES);
    let prompt = format!(
        "User said:\n{user_msg}\n\nAssistant replied:\n{assistant_msg}\n\nTitle:"
    );
    let messages = vec![
        ChatMessage::text("system", SYSTEM_PROMPT),
        ChatMessage::text("user", prompt),
    ];
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut client = client.clone();
    client.max_tokens = Some(MAX_OUTPUT_TOKENS);
    let stream_task = tokio::spawn(async move {
        client.chat_stream(messages, None, tx, None).await
    });

    let mut buf = String::new();
    let collect = async {
        while let Some(chunk) = rx.recv().await {
            buf.push_str(&chunk.delta_content);
            if buf.len() > 4 * MAX_TITLE_LEN {
                // Sanity cap so a runaway model can't make us wait forever.
                break;
            }
        }
    };
    if tokio::time::timeout(TIMEOUT, collect).await.is_err() {
        warn!("title-gen timed out after {}s", TIMEOUT.as_secs());
        stream_task.abort();
        return None;
    }
    match stream_task.await {
        Ok(Ok(())) | Ok(Err(_)) => {}
        Err(e) if e.is_cancelled() => {}
        Err(e) => warn!(error = %e, "title-gen task join failed"),
    }

    let title = clean(&buf);
    if title.is_empty() { None } else { Some(title) }
}

fn clean(raw: &str) -> String {
    // Defence in depth: if any reasoning leaked through (a stray `</think>`),
    // keep only what follows it so the title isn't built from chain-of-thought.
    let body = match raw.rfind("</think>") {
        Some(idx) => &raw[idx + "</think>".len()..],
        None => raw,
    };
    let trimmed = body
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '.' || c == ':');
    let first_line = trimmed.lines().next().unwrap_or("").trim();
    let truncated: String = first_line.chars().take(MAX_TITLE_LEN).collect();
    truncated
        .trim()
        .trim_end_matches(|c: char| c == ':' || c == '.' || c == ',')
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{clean, fallback, FALLBACK_MAX_BYTES};

    #[test]
    fn fallback_keeps_five_words_within_forty_bytes() {
        assert_eq!(fallback("  fix   the build\non windows please now "), "fix the build on windows");
        assert_eq!(fallback("one"), "one");
        assert_eq!(fallback(""), "");
        assert_eq!(fallback("   \n"), "");
        let long = fallback("supercalifragilistic expialidocious antidisestablishmentarianism x y");
        assert!(long.len() <= FALLBACK_MAX_BYTES, "{long:?}");
        assert_eq!(long, "supercalifragilistic expialidocious");
        // A single word longer than the cap is cut on a char boundary.
        let word = fallback(&"é".repeat(50));
        assert!(word.len() <= FALLBACK_MAX_BYTES);
        assert!(!word.is_empty());
    }

    #[test]
    fn strips_quotes_and_period() {
        assert_eq!(clean("\"Hello World.\""), "Hello World");
    }

    #[test]
    fn takes_first_line() {
        assert_eq!(clean("Title One\nIgnored"), "Title One");
    }

    #[test]
    fn strips_trailing_colon() {
        assert_eq!(clean("Image Prompt Request:"), "Image Prompt Request");
    }

    #[test]
    fn drops_leaked_reasoning_before_close_tag() {
        assert_eq!(clean("Here's a thinking process:\n...\n</think>\nSDXL Prompts"), "SDXL Prompts");
    }

    #[test]
    fn caps_length() {
        let long = "a".repeat(120);
        assert_eq!(clean(&long).len(), 60);
    }
}
