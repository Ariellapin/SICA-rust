//! SSE parsing for chat completion streams and `<think>`-tag reasoning split.

use serde::Deserialize;

/// One streamed chunk parsed from an `OpenAI`-compatible `/v1/chat/completions`
/// SSE event. Either `delta_content` or `delta_reasoning` (or both) may be empty.
#[derive(Debug, Clone, Default)]
pub struct StreamChunk {
    pub delta_content:   String,
    pub delta_reasoning: String,
    /// OpenAI-native tool-call fragments: `(index, id?, name?, arguments-fragment)`.
    /// The caller accumulates fragments by index into complete calls.
    pub delta_tool_calls: Vec<ToolCallDelta>,
    pub finish_reason:   Option<String>,
}

/// One streamed fragment of a native tool call. The first fragment for an
/// index usually carries `id` + `name`; later fragments append to `arguments`.
#[derive(Debug, Clone, Default)]
pub struct ToolCallDelta {
    pub index:     u32,
    pub id:        Option<String>,
    pub name:      Option<String>,
    pub arguments: String,
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
    #[serde(default)]
    tool_calls: Option<Vec<SseToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct SseToolCallDelta {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<SseToolCallFunction>,
}

#[derive(Debug, Deserialize)]
struct SseToolCallFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
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

/// Peel a dangling reasoning prefix out of fully-accumulated `content`.
///
/// Some reasoning models stream `reasoning…</think>answer` with the *opening*
/// `<think>` baked into the prompt template, so it never reaches us — only the
/// trailing `</think>` does. The streaming [`ThinkSplitter`] only recognises a
/// *paired* `<think>…</think>`, so that leading reasoning is misclassified as
/// content (it then leaks into the chat bubble and poisons the auto-title
/// agent). Call this on the complete content once the turn finishes: if a
/// closing tag appears with no opening tag before it, everything up to the
/// first `</think>` is reasoning and the remainder is the real answer.
///
/// Returns `None` when the content is already well-formed (no orphan close),
/// so callers can leave a properly-split turn untouched.
pub fn split_orphan_reasoning(content: &str) -> Option<(String, String)> {
    let close = content.find("</think>")?;
    if content[..close].contains("<think>") {
        return None; // properly paired — ThinkSplitter already handled it.
    }
    let reasoning = content[..close].trim().to_string();
    let answer = content[close + "</think>".len()..].trim_start().to_string();
    Some((answer, reasoning))
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
    let delta_tool_calls: Vec<ToolCallDelta> = choice
        .delta
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .map(|tc| ToolCallDelta {
            index: tc.index,
            id: tc.id,
            name: tc.function.as_ref().and_then(|f| f.name.clone()),
            arguments: tc
                .function
                .and_then(|f| f.arguments)
                .unwrap_or_default(),
        })
        .collect();

    // Trim leading/trailing newlines on tiny chunks for cleaner display.
    if content.is_empty()
        && reasoning.is_empty()
        && delta_tool_calls.is_empty()
        && choice.finish_reason.is_none()
    {
        return None;
    }
    Some(StreamChunk {
        delta_content: std::mem::take(&mut content),
        delta_reasoning: std::mem::take(&mut reasoning),
        delta_tool_calls,
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
    fn orphan_reasoning_peeled_when_open_tag_missing() {
        let raw = "Here's a thinking process:\n1. do x\n</think>\n\nThe real answer.";
        let (content, reasoning) = super::split_orphan_reasoning(raw).unwrap();
        assert_eq!(content, "The real answer.");
        assert_eq!(reasoning, "Here's a thinking process:\n1. do x");
    }

    #[test]
    fn orphan_reasoning_left_alone_when_paired_or_absent() {
        // Properly paired — ThinkSplitter owns this; don't double-handle.
        assert!(super::split_orphan_reasoning("<think>r</think>answer").is_none());
        // No reasoning at all.
        assert!(super::split_orphan_reasoning("just an answer").is_none());
    }

    #[test]
    fn parses_tool_call_deltas() {
        let mut s = ThinkSplitter::new();
        let first = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"run-cli","arguments":""}}]}}]}"#;
        let chunk = parse_sse_event(first, &mut s).unwrap();
        assert_eq!(chunk.delta_tool_calls.len(), 1);
        assert_eq!(chunk.delta_tool_calls[0].id.as_deref(), Some("call_1"));
        assert_eq!(chunk.delta_tool_calls[0].name.as_deref(), Some("run-cli"));

        let frag = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"command\":"}}]}}]}"#;
        let chunk = parse_sse_event(frag, &mut s).unwrap();
        assert_eq!(chunk.delta_tool_calls[0].arguments, "{\"command\":");
        assert!(chunk.delta_tool_calls[0].id.is_none());
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
