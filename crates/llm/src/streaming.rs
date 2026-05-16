//! SSE parsing for chat completion streams and `<think>`-tag reasoning split.

use serde::Deserialize;

/// One streamed chunk parsed from an `OpenAI`-compatible `/v1/chat/completions`
/// SSE event. Either `delta_content` or `delta_reasoning` (or both) may be empty.
#[derive(Debug, Clone, Default)]
pub struct StreamChunk {
    pub delta_content:   String,
    pub delta_reasoning: String,
    pub finish_reason:   Option<String>,
}

#[derive(Debug, Deserialize)]
struct SseEnvelope {
    choices: Vec<SseChoice>,
}

#[derive(Debug, Deserialize)]
struct SseChoice {
    #[serde(default)]
    delta: SseDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct SseDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
}

/// Stateful splitter that pulls reasoning text out of `<think>...</think>` blocks
/// embedded in `content`. Mirrors Python `src/sica/llm/reasoning.py`.
#[derive(Default, Debug, Clone)]
pub struct ThinkSplitter {
    in_think: bool,
    buf: String, // unmatched tag prefix that crossed a chunk boundary
}

impl ThinkSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `(content, reasoning)` extracted from a raw delta string.
    pub fn feed(&mut self, raw: &str) -> (String, String) {
        let mut content = String::new();
        let mut reasoning = String::new();

        let combined = if self.buf.is_empty() { raw.to_owned() } else {
            let mut s = std::mem::take(&mut self.buf);
            s.push_str(raw);
            s
        };

        let mut i = 0usize;
        let bytes = combined.as_bytes();
        while i < bytes.len() {
            let rest = &combined[i..];
            if !self.in_think {
                if let Some(idx) = rest.find("<think>") {
                    content.push_str(&rest[..idx]);
                    i += idx + "<think>".len();
                    self.in_think = true;
                } else if rest.ends_with('<') || rest.ends_with("<t") || rest.ends_with("<th")
                    || rest.ends_with("<thi") || rest.ends_with("<thin")
                    || rest.ends_with("<think")
                {
                    let cut = rest.rfind('<').unwrap_or(rest.len());
                    content.push_str(&rest[..cut]);
                    self.buf.push_str(&rest[cut..]);
                    i = bytes.len();
                } else {
                    content.push_str(rest);
                    i = bytes.len();
                }
            } else if let Some(idx) = rest.find("</think>") {
                reasoning.push_str(&rest[..idx]);
                i += idx + "</think>".len();
                self.in_think = false;
            } else if rest.ends_with('<') || rest.ends_with("</") || rest.ends_with("</t")
                || rest.ends_with("</th") || rest.ends_with("</thi")
                || rest.ends_with("</thin") || rest.ends_with("</think")
            {
                let cut = rest.rfind('<').unwrap_or(rest.len());
                reasoning.push_str(&rest[..cut]);
                self.buf.push_str(&rest[cut..]);
                i = bytes.len();
            } else {
                reasoning.push_str(rest);
                i = bytes.len();
            }
        }

        (content, reasoning)
    }
}

/// Parse a single `data:` SSE line payload (the JSON envelope), splitting any
/// embedded `<think>...</think>` reasoning out of `content` and merging it with
/// any `reasoning_content` field if the server returns one separately.
pub fn parse_sse_event(json: &str, splitter: &mut ThinkSplitter) -> Option<StreamChunk> {
    let env: SseEnvelope = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return None,
    };
    let choice = env.choices.into_iter().next()?;

    let raw_content = choice.delta.content.unwrap_or_default();
    let (mut content, mut reasoning) = splitter.feed(&raw_content);
    if let Some(rc) = choice.delta.reasoning_content {
        if !rc.is_empty() {
            if !reasoning.is_empty() {
                reasoning.push('\n');
            }
            reasoning.push_str(&rc);
        }
    }
    // Trim leading/trailing newlines on tiny chunks for cleaner display.
    if content.is_empty() && reasoning.is_empty() && choice.finish_reason.is_none() {
        return None;
    }
    Some(StreamChunk {
        delta_content: std::mem::take(&mut content),
        delta_reasoning: std::mem::take(&mut reasoning),
        finish_reason: choice.finish_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn think_split_single_chunk() {
        let mut s = ThinkSplitter::new();
        let (c, r) = s.feed("hello <think>secret</think> world");
        assert_eq!(c, "hello  world");
        assert_eq!(r, "secret");
        assert!(!s.in_think);
    }

    #[test]
    fn think_split_across_chunks() {
        let mut s = ThinkSplitter::new();
        let (c1, r1) = s.feed("hello <thi");
        let (c2, r2) = s.feed("nk>secret</think> world");
        assert_eq!(c1, "hello ");
        assert!(r1.is_empty());
        assert_eq!(c2, " world");
        assert_eq!(r2, "secret");
    }
}
