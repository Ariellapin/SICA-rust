//! Keyless replay of a recorded session (guide §14.1).
//!
//! A session log already holds everything a provider said: every
//! `AssistantMessage` is one completion, in order, with its reasoning and
//! its `tool_calls` array. So a recorded `session.jsonl` *is* the script —
//! nothing extra has to be captured, and a scenario is recorded by having
//! a real session once.
//!
//! [`ReplayScript::from_log`] groups those into per-call entries bound by
//! first-call order, and [`LlmClient::chat_stream`] serves from the queue
//! instead of opening a socket when a script is attached. The whole harness
//! above it — retry classification, compaction, the spill policy, the tool
//! pipeline — runs exactly as it does against a real provider, which is the
//! point: the eval exercises the harness, not the model.
//!
//! Some things a log cannot express, because they never produced a durable
//! row: a request that threw before any chunk, a request that hangs, an
//! injected retry. Those come from a sidecar `replay.override.json`, whose
//! entries are *inserted* into the queue rather than replacing anything —
//! so an injected failure at index 0 makes the first attempt fail and the
//! recorded reply serve on the retry, which is what "injected retry" means.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use sica_core::event::{EventKind, SessionEvent};

use crate::streaming::{StreamChunk, ToolCallDelta, Usage};

/// What one scripted call does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayCall {
    /// A recorded completion.
    Message {
        content:    String,
        reasoning:  Option<String>,
        /// The OpenAI `tool_calls` array as JSON text, as the log stores it.
        tool_calls: Option<String>,
    },
    /// The request failed before producing anything. `message` is served as
    /// the error text, so a scenario can choose whether the harness sees a
    /// retryable failure (`sse decode: …`) or a fatal one.
    Error { message: String },
    /// A clean stream that carried nothing at all — what a local server does
    /// under memory pressure, and the case `empty-response-retry` covers.
    Empty,
    /// Never answers, for the timeout paths. The client's cancellation or
    /// the caller's own timeout is what ends it.
    Hang { hold: Duration },
}

/// One `replay.override.json` entry.
#[derive(Debug, Clone, Deserialize)]
struct RawOverride {
    /// Queue position to insert at, counted in *recorded* calls. `0` means
    /// "before the first recorded reply".
    #[serde(default)]
    before_call: usize,
    /// `error` | `empty` | `hang`.
    kind:        String,
    #[serde(default)]
    message:     String,
    /// Seconds, `hang` only.
    #[serde(default)]
    hold_secs:   Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawOverrideFile {
    #[serde(default)]
    calls: Vec<RawOverride>,
}

/// A queue of scripted calls, served in order.
///
/// Interior-mutable and `Send + Sync`: `LlmClient` is cloned freely across
/// tasks and the script is shared by every clone, which is what makes
/// "first-call order" a property of the run rather than of one clone.
#[derive(Debug)]
pub struct ReplayScript {
    queue: Mutex<VecDeque<ReplayCall>>,
    /// Calls the script started with — for the "did the run consume the
    /// whole script" assertion a scenario ends with.
    total: usize,
}

impl ReplayScript {
    pub fn new(calls: Vec<ReplayCall>) -> Self {
        Self { total: calls.len(), queue: Mutex::new(calls.into()) }
    }

    /// Build from a recorded session log (JSONL, one `SessionEvent` per
    /// line). Unparseable lines are skipped: a log with a torn tail is
    /// still a usable recording of everything before the tear.
    pub fn from_log(jsonl: &str) -> Self {
        let calls = jsonl
            .lines()
            .filter_map(|l| serde_json::from_str::<SessionEvent>(l).ok())
            .filter_map(|ev| match ev.kind {
                EventKind::AssistantMessage { content, reasoning, tool_calls, .. } => {
                    Some(ReplayCall::Message { content, reasoning, tool_calls })
                }
                // The summariser is a completion too. It is recorded as a
                // `CompactionSummary` rather than an assistant message, so a
                // script built only from assistant rows would run one call
                // short and every later reply would answer the wrong request.
                // `summary` is the bare LLM output — exactly what the call
                // returned before the marker was wrapped around it.
                EventKind::CompactionSummary { summary, .. } => Some(ReplayCall::Message {
                    content:    summary,
                    reasoning:  None,
                    tool_calls: None,
                }),
                _ => None,
            })
            .collect();
        Self::new(calls)
    }

    /// Apply a `replay.override.json` body. Entries are inserted at their
    /// `before_call` position, later ones first so earlier insertions do not
    /// shift the positions of the ones still to be applied.
    pub fn with_overrides(mut self, json: &str) -> Result<Self, String> {
        let file: RawOverrideFile =
            serde_json::from_str(json).map_err(|e| format!("replay.override.json: {e}"))?;
        let mut entries = file.calls;
        entries.sort_by_key(|e| std::cmp::Reverse(e.before_call));
        let queue = self.queue.get_mut().expect("uncontended");
        for e in entries {
            let call = match e.kind.as_str() {
                "error" => ReplayCall::Error {
                    message: if e.message.is_empty() {
                        "replay: injected failure".into()
                    } else {
                        e.message
                    },
                },
                "empty" => ReplayCall::Empty,
                "hang" => ReplayCall::Hang {
                    hold: Duration::from_secs(e.hold_secs.unwrap_or(600)),
                },
                other => return Err(format!("replay.override.json: unknown kind {other:?}")),
            };
            let at = e.before_call.min(queue.len());
            queue.insert(at, call);
        }
        self.total = self.queue.get_mut().expect("uncontended").len();
        Ok(self)
    }

    /// Append `n` more copies of the last recorded completion.
    ///
    /// Compaction consumes calls the log cannot account for: the policy
    /// retries a summary it judges unusable, and a compaction whose history
    /// moved underneath it is discarded — in both cases a completion was
    /// spent and no row records it. A recording therefore always holds
    /// *fewer* calls than a run with compaction needs, and re-recording
    /// would shed one turn each time.
    ///
    /// Padding is the honest way out, and it is only sound for a scenario
    /// whose completions are interchangeable — which is why the scenario
    /// declares it rather than the driver guessing. `pad` on a script whose
    /// replies differ would make the run depend on *which* entry served a
    /// request, and that is exactly what a snapshot must not do.
    pub fn pad(mut self, n: usize) -> Self {
        let queue = self.queue.get_mut().expect("uncontended");
        let Some(last) = queue
            .iter()
            .rev()
            .find(|c| matches!(c, ReplayCall::Message { .. }))
            .cloned()
        else {
            return self;
        };
        for _ in 0..n {
            queue.push_back(last.clone());
        }
        self.total = queue.len();
        self
    }

    /// Take the next scripted call. `None` once the script is exhausted —
    /// the client turns that into an error rather than a silent success,
    /// because a run that asks for more calls than were recorded has
    /// diverged from the recording and the test must see it.
    pub fn next_call(&self) -> Option<ReplayCall> {
        self.queue.lock().expect("replay queue").pop_front()
    }

    pub fn remaining(&self) -> usize {
        self.queue.lock().expect("replay queue").len()
    }

    pub fn total(&self) -> usize {
        self.total
    }
}

/// The chunks one recorded completion streams. Content and reasoning go out
/// as single deltas — a recording holds the assembled text, and re-splitting
/// it into token-sized pieces would be inventing detail the log never had.
pub fn chunks_for(call: &ReplayCall) -> Vec<StreamChunk> {
    let ReplayCall::Message { content, reasoning, tool_calls } = call else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(r) = reasoning.as_ref().filter(|r| !r.is_empty()) {
        out.push(StreamChunk { delta_reasoning: r.clone(), ..Default::default() });
    }
    if !content.is_empty() {
        out.push(StreamChunk { delta_content: content.clone(), ..Default::default() });
    }
    if let Some(json) = tool_calls {
        let deltas = parse_tool_calls(json);
        if !deltas.is_empty() {
            out.push(StreamChunk {
                delta_tool_calls: deltas,
                finish_reason: Some("tool_calls".into()),
                ..Default::default()
            });
        }
    }
    // The usage frame a provider sends last. Zeroes rather than invented
    // numbers: the recording did not keep them per call, and a fabricated
    // count would make the meter's anchor assert against fiction.
    out.push(StreamChunk {
        usage: Some(Usage::default()),
        ..Default::default()
    });
    out
}

/// Turn the log's stored `tool_calls` array into the streamed fragments the
/// caller reassembles. One fragment per call, complete — a recording has no
/// fragmentation to reproduce.
fn parse_tool_calls(json: &str) -> Vec<ToolCallDelta> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(json)
    else {
        return Vec::new();
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let f = item.get("function");
            ToolCallDelta {
                index:     item.get("index").and_then(|v| v.as_u64()).unwrap_or(i as u64) as u32,
                id:        item.get("id").and_then(|v| v.as_str()).map(str::to_string),
                name:      f
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                arguments: f
                    .and_then(|f| f.get("arguments"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = r#"
{"seq":1,"ts":0,"type":"session_created","id":1,"title":"t","created_at":0}
{"seq":2,"ts":0,"type":"user_message","surface":{"op":"append"},"content":"hi"}
{"seq":3,"ts":0,"type":"assistant_message","surface":{"op":"append"},"content":"first"}
{"seq":4,"ts":0,"type":"assistant_message","surface":{"op":"append"},"content":"second","reasoning":"because"}
"#;

    #[test]
    fn a_recorded_log_is_the_script() {
        let s = ReplayScript::from_log(LOG);
        assert_eq!(s.total(), 2, "only the assistant messages are calls");
        assert_eq!(
            s.next_call(),
            Some(ReplayCall::Message {
                content: "first".into(),
                reasoning: None,
                tool_calls: None,
            })
        );
        assert_eq!(s.remaining(), 1);
        let second = s.next_call().unwrap();
        assert!(matches!(&second, ReplayCall::Message { reasoning: Some(r), .. } if r == "because"));
        assert_eq!(s.next_call(), None, "an exhausted script serves nothing");
    }

    #[test]
    fn a_compaction_summary_is_a_call_too() {
        // Otherwise a replayed session with compaction runs one call short
        // and every reply after it answers the previous request.
        let log = concat!(
            r#"{"seq":1,"ts":0,"type":"assistant_message","surface":{"op":"append"},"content":"a"}"#,
            "\n",
            r#"{"seq":2,"ts":0,"type":"compaction_summary","surface":{"op":"replace","start_seq":1,"end_seq":1},"content":"[context summary] s","summary":"s","folded":1,"before_tokens":9,"after_tokens":2}"#,
            "\n",
            r#"{"seq":3,"ts":0,"type":"assistant_message","surface":{"op":"append"},"content":"b"}"#,
        );
        let script = ReplayScript::from_log(log);
        assert_eq!(script.total(), 3);
        assert!(matches!(script.next_call(), Some(ReplayCall::Message { content, .. }) if content == "a"));
        assert!(matches!(script.next_call(), Some(ReplayCall::Message { content, .. }) if content == "s"));
        assert!(matches!(script.next_call(), Some(ReplayCall::Message { content, .. }) if content == "b"));
    }

    #[test]
    fn padding_repeats_the_last_recorded_completion() {
        let s = ReplayScript::from_log(LOG).pad(2);
        assert_eq!(s.total(), 4);
        assert!(matches!(s.next_call(), Some(ReplayCall::Message { content, .. }) if content == "first"));
        for _ in 0..3 {
            assert!(matches!(s.next_call(), Some(ReplayCall::Message { content, .. }) if content == "second"));
        }
        assert_eq!(s.next_call(), None);
    }

    #[test]
    fn padding_an_empty_script_adds_nothing() {
        // Nothing to repeat, and inventing a reply would be worse than
        // failing with "the script is exhausted".
        assert_eq!(ReplayScript::from_log("").pad(3).total(), 0);
    }

    #[test]
    fn a_torn_line_does_not_lose_the_rest_of_the_recording() {
        let torn = format!("{LOG}\n{{\"seq\":5,\"ts\":0,\"typ");
        assert_eq!(ReplayScript::from_log(&torn).total(), 2);
    }

    #[test]
    fn an_override_is_inserted_so_the_recorded_reply_serves_the_retry() {
        // This *is* "injected retry": the first attempt fails, and the
        // recording answers the second one.
        let s = ReplayScript::from_log(LOG)
            .with_overrides(r#"{"calls":[{"before_call":0,"kind":"empty"}]}"#)
            .unwrap();
        assert_eq!(s.total(), 3);
        assert_eq!(s.next_call(), Some(ReplayCall::Empty));
        assert!(matches!(s.next_call(), Some(ReplayCall::Message { content, .. }) if content == "first"));
    }

    #[test]
    fn several_overrides_land_at_the_positions_they_name() {
        let s = ReplayScript::from_log(LOG)
            .with_overrides(
                r#"{"calls":[
                     {"before_call":0,"kind":"error","message":"sse decode: boom"},
                     {"before_call":1,"kind":"empty"}
                   ]}"#,
            )
            .unwrap();
        // error, first, empty, second — the second entry's position is
        // counted in *recorded* calls, not in the shifted queue.
        assert!(matches!(s.next_call(), Some(ReplayCall::Error { message }) if message.contains("sse decode")));
        assert!(matches!(s.next_call(), Some(ReplayCall::Message { content, .. }) if content == "first"));
        assert_eq!(s.next_call(), Some(ReplayCall::Empty));
        assert!(matches!(s.next_call(), Some(ReplayCall::Message { content, .. }) if content == "second"));
    }

    #[test]
    fn an_unknown_override_kind_is_refused_rather_than_ignored() {
        // Silently skipping it would make a scenario pass while testing
        // something other than what it says it tests.
        let e = ReplayScript::from_log(LOG)
            .with_overrides(r#"{"calls":[{"kind":"explode"}]}"#)
            .unwrap_err();
        assert!(e.contains("unknown kind"), "{e}");
    }

    #[test]
    fn an_override_past_the_end_lands_at_the_end() {
        let s = ReplayScript::from_log(LOG)
            .with_overrides(r#"{"calls":[{"before_call":99,"kind":"empty"}]}"#)
            .unwrap();
        assert!(matches!(s.next_call(), Some(ReplayCall::Message { .. })));
        assert!(matches!(s.next_call(), Some(ReplayCall::Message { .. })));
        assert_eq!(s.next_call(), Some(ReplayCall::Empty));
    }

    #[test]
    fn a_recorded_tool_call_streams_as_a_complete_fragment() {
        let call = ReplayCall::Message {
            content:    String::new(),
            reasoning:  None,
            tool_calls: Some(
                r#"[{"id":"call_1","type":"function",
                     "function":{"name":"read-file","arguments":"{\"path\":\"a.txt\"}"}}]"#
                    .into(),
            ),
        };
        let chunks = chunks_for(&call);
        let delta = chunks
            .iter()
            .flat_map(|c| c.delta_tool_calls.iter())
            .next()
            .expect("a tool-call delta");
        assert_eq!(delta.id.as_deref(), Some("call_1"));
        assert_eq!(delta.name.as_deref(), Some("read-file"));
        assert_eq!(delta.arguments, r#"{"path":"a.txt"}"#);
        assert!(chunks.iter().any(|c| c.finish_reason.as_deref() == Some("tool_calls")));
    }

    #[test]
    fn reasoning_streams_before_content() {
        // The order the splitter produces against a real provider, so the
        // replayed transcript has the same shape.
        let call = ReplayCall::Message {
            content:    "answer".into(),
            reasoning:  Some("thinking".into()),
            tool_calls: None,
        };
        let chunks = chunks_for(&call);
        assert_eq!(chunks[0].delta_reasoning, "thinking");
        assert_eq!(chunks[1].delta_content, "answer");
        assert!(chunks.last().unwrap().usage.is_some());
    }

    #[test]
    fn a_malformed_tool_calls_field_streams_no_fragments() {
        let call = ReplayCall::Message {
            content:    "x".into(),
            reasoning:  None,
            tool_calls: Some("not json".into()),
        };
        assert!(chunks_for(&call).iter().all(|c| c.delta_tool_calls.is_empty()));
    }
}
