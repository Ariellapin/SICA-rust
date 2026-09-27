//! The completion check: when a turn stops *abnormally*, ask the model
//! whether the user actually got what they asked for.
//!
//! A turn can end for reasons that have nothing to do with the work being
//! finished — `MAX_TOOL_HOPS`, the completion cap, a retry budget running
//! out. Before this check those stops were silent: the session simply went
//! idle mid-task and the person had to notice, guess why, and type
//! "continue". Twelve `read-file` hops covering lines 29-100 of a 214-line
//! file, then nothing, is the shape of it.
//!
//! So every abnormal stop gets one cheap, tool-less LLM round-trip that
//! answers three things: was the request satisfied, why not, and what the
//! single next step is. The answer is recorded as `EventKind::TurnVerdict`
//! and, when the request was *not* satisfied, opens one more turn carrying
//! that next step — at most [`MAX_AUTO_CONTINUES`] times per human message.
//!
//! Three deliberate limits, each answering a way this goes wrong:
//!
//! - **Abnormal stops only.** A turn the model ended itself (`done`) is not
//!   checked, so a normal conversation costs exactly what it did before —
//!   with one exception: a *continuation* that ends `done` is checked too.
//!   Work big enough to be cut short once is where a model most often
//!   announces the next step and stops, and the chain it belongs to has
//!   already paid for checks.
//! - **Never after an interrupt.** Stop means stop; auto-continuing work a
//!   person just cancelled would make the Stop button a lie — the same
//!   reasoning that disarms the goal driver there.
//! - **The check never fails a turn.** Any error — no connection, a timeout,
//!   an unparseable reply — returns `Err` with a one-line reason, the caller
//!   surfaces it as a WARN `LogLine`, and the turn ends exactly as it would
//!   have. A broken judge must not be able to hold a session, and a broken
//!   judge must not be invisible either: session 85 stopped at the hop
//!   limit and went idle with no verdict and no line saying why.
//!
//! The judge runs with thinking **off**. It is a tool-less classification
//! with a small completion cap, and a reasoning model (Qwen 3.x) given that
//! cap otherwise spends all of it inside `<think>` and never reaches the
//! JSON — the reply is empty and the hop-limit stop goes unaudited.

use std::collections::HashMap;
use std::time::Duration;

use llm::client::{ChatMessage, LlmClient};
use sica_core::event::{EventKind, SessionEvent};
use tokio::sync::mpsc;
use tracing::warn;

/// Continuation turns the check may open for one human message, progress
/// or not. The count resets on the next human turn, never on a
/// continuation.
pub const MAX_AUTO_CONTINUES: u8 = 12;

/// Continuations in a row that may pass without one successful tool call.
/// A continuation that got something done is refunded, so a long job — a
/// 124-line file edited a dozen hops per turn — keeps going as long as each
/// turn moves it, while a request the model cannot make progress on still
/// stops after two tries, as it always did.
pub const MAX_STALLED_CONTINUES: u8 = 2;

/// What the completion check has spent on one human message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Spent {
    /// Continuations opened since the message.
    pub opened:  u8,
    /// Those still counted as stalled — opened, and not refunded by a
    /// successful tool call.
    pub stalled: u8,
}

impl Spent {
    /// Whether one more continuation may be opened.
    pub fn budget_left(&self) -> bool {
        self.stalled < MAX_STALLED_CONTINUES && self.opened < MAX_AUTO_CONTINUES
    }

    /// Count a continuation about to open. Counted *before* it runs, so a
    /// continuation that crashes still costs one.
    pub fn open(&mut self) -> u8 {
        self.opened = self.opened.saturating_add(1);
        self.stalled = self.stalled.saturating_add(1);
        self.opened
    }

    /// The continuation that just ended made progress.
    pub fn refund(&mut self) {
        self.stalled = self.stalled.saturating_sub(1);
    }
}

/// Bytes of the objective and of the turn digest the judge is shown.
const MAX_INPUT_BYTES: usize = 3072;
/// Completion cap on the verdict request. Thinking is disabled for the
/// call, so this only has to hold a three-field object plus the prose a
/// small model wraps it in.
const MAX_OUTPUT_TOKENS: u32 = 512;
/// Wall clock for the whole round-trip.
const TIMEOUT: Duration = Duration::from_secs(90);
/// Sanity cap on the accumulated reply.
const MAX_REPLY_BYTES: usize = 8192;
/// Longest `reason` / `next_step` kept. Both are read by a human in the
/// GUI and, for `next_step`, pasted into the continuation prompt.
const MAX_FIELD_LEN: usize = 400;
/// Tool calls listed in the digest, newest last. A hop-limited turn has 12;
/// showing all of them is what makes "it only reached line 100" visible.
const MAX_DIGEST_CALLS: usize = 16;

const SYSTEM_PROMPT: &str = "\
You audit whether an AI agent's turn actually completed the user's request. \
Assume nothing was finished unless the transcript shows it.

Reply with one JSON object and nothing else:
{\"reached\": true|false, \"reason\": \"<one sentence>\", \"next_step\": \"<one imperative sentence>\"}

- \"reached\" is true only if the user's request is fully satisfied by work \
visible in the transcript. Partial progress is false. It is also true when \
the agent's last message asks the user a question it cannot go on without — \
the next move is then the user's.
- \"reason\" says concretely what was and was not done. Name the specific \
thing left — a line number, a file, an unanswered question.
- \"next_step\" is the single most useful next action, addressed to the \
agent. Prefer one that makes more progress per tool call than the turn was \
making. Omit it when \"reached\" is true.";

/// The check's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub reached:   bool,
    pub reason:    String,
    /// The one action to take next. Always `None` when `reached`; may also
    /// be `None` when the model declined to name one, in which case the
    /// continuation prompt falls back to the reason.
    pub next_step: Option<String>,
}

/// Whether a `TurnEnd.finish_reason` describes a turn cut short by the
/// harness rather than one the model chose to end. `empty` is a turn whose
/// replies kept ending inside the model's reasoning with nothing said and
/// nothing called; `bad-call` one whose tool call still did not parse after
/// the correction.
///
/// `interrupted` is excluded on purpose, and is not merely absent: a person
/// pressing Stop has already answered the question this module asks.
pub fn abnormal(finish_reason: &str) -> bool {
    matches!(finish_reason, "hop-limit" | "max_tokens" | "error" | "empty" | "bad-call")
}

/// Whether a turn that ended with `finish_reason` gets the check:
/// every abnormal stop, and a continuation that the model ended itself.
pub fn needs_check(finish_reason: &str, continuation: bool) -> bool {
    abnormal(finish_reason) || (continuation && finish_reason == "done")
}

/// Whether the turn `turn_id` ran at least one tool call successfully —
/// what refunds a continuation (see [`Spent`]). Pruned re-writes of old
/// results are not new work.
pub fn made_progress(events: &[SessionEvent], turn_id: u64) -> bool {
    let Some(start) = events
        .iter()
        .rposition(|e| matches!(&e.kind, EventKind::TurnStart { turn_id: t, .. } if *t == turn_id))
    else {
        return false;
    };
    events[start..]
        .iter()
        .any(|e| matches!(&e.kind, EventKind::ToolResult { ok: true, pruned: false, .. }))
}

/// The human request a turn is ultimately serving.
///
/// Not simply the turn's own prompt: a continuation turn's prompt is
/// [`continue_prompt`]'s text and a goal round's is the round prompt, and
/// auditing either against itself would ask "did you do what you just told
/// yourself to do". So this walks forward to `turn_id` and keeps the last
/// user message that opened a turn with human authority — the thing the
/// person actually asked for, however many machine turns ago.
///
/// Empty when the log holds no human turn at or before `turn_id`, which
/// the caller treats as nothing to check.
pub fn objective(events: &[SessionEvent], turn_id: u64) -> String {
    let mut last_user = String::new();
    let mut human = String::new();
    for ev in events {
        match &ev.kind {
            EventKind::UserMessage { content, .. } => last_user = content.clone(),
            EventKind::TurnStart { turn_id: t, source } => {
                if source.is_human() && !last_user.is_empty() {
                    human = last_user.clone();
                }
                if *t == turn_id {
                    break;
                }
            }
            _ => {}
        }
    }
    human
}

/// A compact account of what the turn did, for the judge to audit.
///
/// Walks from the `TurnStart` carrying `turn_id` to the end of the log and
/// keeps the shape of the work: each tool call with its outcome, and the
/// agent's own last words. Tool *results* are omitted — the judge needs to
/// know that lines 65-70 were read, not what was on them, and a hop-limited
/// turn's results would swamp the budget.
pub fn digest(events: &[SessionEvent], turn_id: u64) -> String {
    let start = events
        .iter()
        .position(|e| matches!(&e.kind, EventKind::TurnStart { turn_id: t, .. } if *t == turn_id));
    let Some(start) = start else { return String::new() };
    let tail = &events[start..];

    let mut ok_by_call: HashMap<u64, bool> = HashMap::new();
    for ev in tail {
        if let EventKind::ToolResult { call_seq, ok, .. } = &ev.kind {
            ok_by_call.insert(*call_seq, *ok);
        }
    }

    let mut calls: Vec<String> = Vec::new();
    let mut last_said = String::new();
    for ev in tail {
        match &ev.kind {
            EventKind::ToolCall { args_preview, .. } => {
                let mark = match ok_by_call.get(&ev.seq) {
                    Some(true) => "ok",
                    Some(false) => "FAILED",
                    None => "no result",
                };
                calls.push(format!("- {} [{mark}]", one_line(args_preview, 200)));
            }
            EventKind::AssistantMessage { content, .. } if !content.trim().is_empty() => {
                last_said = content.clone();
            }
            _ => {}
        }
    }

    let mut out = String::new();
    if calls.is_empty() {
        out.push_str("The agent made no tool calls.\n");
    } else {
        let shown = calls.len().min(MAX_DIGEST_CALLS);
        let skipped = calls.len() - shown;
        out.push_str(&format!("Tool calls in order ({} total):\n", calls.len()));
        if skipped > 0 {
            out.push_str(&format!("- [{skipped} earlier call(s) omitted]\n"));
        }
        for c in &calls[calls.len() - shown..] {
            out.push_str(c);
            out.push('\n');
        }
    }
    if !last_said.trim().is_empty() {
        out.push_str("\nThe agent's last message:\n");
        out.push_str(&one_line(&last_said, 600));
        out.push('\n');
    }
    sica_core::retain::utf8_head(&out, MAX_INPUT_BYTES).to_string()
}

/// Ask the connected model whether `objective` was met. `Err` carries a
/// one-line reason on any failure — the caller reports it and then ends
/// the turn exactly as it would have.
pub async fn check(
    client: &LlmClient,
    objective: &str,
    finish_reason: &str,
    digest: &str,
) -> Result<Verdict, String> {
    let objective = sica_core::retain::utf8_head(objective, MAX_INPUT_BYTES);
    let prompt = format!(
        "The user asked for:\n{objective}\n\n{}\n\n{digest}\n\nVerdict JSON:",
        how_it_ended(finish_reason)
    );
    let messages = vec![
        ChatMessage::text("system", SYSTEM_PROMPT),
        ChatMessage::text("user", &prompt),
    ];
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut client = client.clone();
    client.max_tokens = Some(MAX_OUTPUT_TOKENS);
    // A judge that reasons out loud never answers within the cap; see the
    // module note. Servers that do not template the toggle ignore it.
    client.thinking = false;
    let stream_task =
        tokio::spawn(async move { client.chat_stream(messages, None, tx, None).await });

    let mut buf = String::new();
    let collect = async {
        while let Some(chunk) = rx.recv().await {
            buf.push_str(&chunk.delta_content);
            if buf.len() > MAX_REPLY_BYTES {
                break;
            }
        }
    };
    if tokio::time::timeout(TIMEOUT, collect).await.is_err() {
        warn!("completion check timed out after {}s", TIMEOUT.as_secs());
        stream_task.abort();
        return Err(format!("timed out after {}s", TIMEOUT.as_secs()));
    }
    match stream_task.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            warn!(error = %e, "completion check request failed");
            return Err(format!("request failed: {}", one_line(&e.to_string(), MAX_FIELD_LEN)));
        }
        Err(e) if e.is_cancelled() => {}
        Err(e) => warn!(error = %e, "completion check task join failed"),
    }
    if buf.trim().is_empty() {
        return Err("the model returned an empty reply".into());
    }
    parse(&buf).ok_or_else(|| {
        warn!(bytes = buf.len(), "completion check reply carried no verdict");
        format!(
            "the reply carried no `reached` verdict ({} bytes: {})",
            buf.len(),
            one_line(&buf, 120)
        )
    })
}

/// Pull the verdict object out of a model reply.
///
/// Lenient on purpose: the local models this harness targets wrap JSON in
/// prose or a fence, and leak `</think>`. What is *not* lenient is
/// `reached` — a reply that does not state it plainly is no verdict at all
/// and returns `None`, because defaulting it either way is worse than not
/// checking: `true` hides unfinished work, `false` auto-continues finished
/// work.
pub fn parse(raw: &str) -> Option<Verdict> {
    let body = match raw.rfind("</think>") {
        Some(i) => &raw[i + "</think>".len()..],
        None => raw,
    };
    let obj = first_object(body)?;
    let v: serde_json::Value = serde_json::from_str(obj).ok()?;
    let reached = v.get("reached").and_then(|r| match r {
        serde_json::Value::Bool(b) => Some(*b),
        // Some small models answer "true" / "yes" as a string.
        serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" => Some(true),
            "false" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    })?;
    let field = |k: &str| -> Option<String> {
        let s = v.get(k)?.as_str()?.trim();
        if s.is_empty() {
            return None;
        }
        Some(one_line(s, MAX_FIELD_LEN))
    };
    Some(Verdict {
        reached,
        reason: field("reason").unwrap_or_else(|| {
            if reached { "request satisfied".into() } else { "request not satisfied".into() }
        }),
        next_step: if reached { None } else { field("next_step") },
    })
}

/// The user-role message the continuation turn opens with.
///
/// Framed as the harness speaking, not the user: the model must not come
/// away thinking the person asked again. It also names the limit that was
/// hit, because "you were cut off at the hop limit" is what makes a model
/// widen its next `read-file` window instead of repeating six-line reads.
pub fn continue_prompt(v: &Verdict, finish_reason: &str, attempt: u8) -> String {
    let step = v.next_step.as_deref().unwrap_or(v.reason.as_str());
    let why = if finish_reason == "done" {
        "Your previous turn ended before the user's request was finished.".to_string()
    } else {
        format!(
            "Your previous turn was cut short by the harness ({finish_reason}) before the \
             user's request was finished."
        )
    };
    format!(
        "<auto_continue attempt=\"{attempt}\" of=\"{MAX_AUTO_CONTINUES}\">\n\
         {why} This is the harness continuing it — the user has not sent a new \
         message.\n\n\
         What is still missing: {}\n\
         Do this next: {step}\n\n\
         {} If the request is in fact already satisfied, say so and stop.\n\
         </auto_continue>",
        v.reason,
        advice(finish_reason),
    )
}

/// What to do differently next time, by how the previous turn ended. The
/// hop-limit advice — wider calls — is the wrong one for a reply that ran
/// out of room: `sessions/92` was told to "work in larger steps" after a
/// 60 K-token `write-file` hit the cap, and wrote a bigger one.
fn advice(finish_reason: &str) -> &'static str {
    match finish_reason {
        "max_tokens" => {
            "Your last reply ran into the output limit, so a tool call in it was cut off \
             and did NOT run. Say less per reply: keep your reasoning short, and write a \
             large file in parts — write-file the first part, then add each further part \
             with write-file '<path>' '<text>' 'append=true'."
        }
        "empty" => {
            "Your last replies ended inside your reasoning with nothing said and no tool \
             call, so nothing ran. Keep the thinking short and reply with the tool call \
             itself."
        }
        "bad-call" => {
            "Your last tool call could not be read, so it did not run. Use the one-line \
             form <skill> '<arg>' > <expectation>, or a ```tool_call JSON fence for \
             arguments full of quotes."
        }
        "error" => "The last request failed; carry on from the last step that succeeded.",
        _ => {
            "Work in larger steps than the previous turn did: you have a fresh but equally \
             limited tool budget, so prefer one wide call over several narrow ones."
        }
    }
}

/// The sentence that tells the judge how the turn ended.
fn how_it_ended(finish_reason: &str) -> String {
    if finish_reason == "done" {
        "The agent ended this turn itself, part-way through work the harness had \
         already resumed at least once — judge the whole request, not the turn's last \
         step."
            .into()
    } else {
        format!("The turn was cut short by the harness: {finish_reason}.")
    }
}

/// Collapse to one line and cut on a char boundary.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut = sica_core::retain::utf8_head(&flat, max);
    if cut.len() < flat.len() { format!("{cut}…") } else { cut.to_string() }
}

/// The first balanced `{…}` run in `s`, ignoring braces inside strings.
fn first_object(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    let start = s.find('{')?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    for i in start..b.len() {
        let c = b[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use sica_core::event::{SurfaceOp, TurnSource};

    fn ev(seq: u64, kind: EventKind) -> SessionEvent {
        SessionEvent { seq, ts: 0, kind }
    }

    fn call(seq: u64, preview: &str) -> SessionEvent {
        ev(seq, EventKind::ToolCall {
            name:         "read-file".into(),
            args_preview: preview.into(),
            expectation:  "e".into(),
            call_id:      None,
            args_json:    None,
        })
    }

    fn result(seq: u64, call_seq: u64, ok: bool, summary: &str) -> SessionEvent {
        ev(seq, EventKind::ToolResult {
            surface:      SurfaceOp::Append,
            call_seq,
            skill:        "read-file".into(),
            tool_call_id: None,
            ok,
            summary:      summary.into(),
            trusted:      true,
            pruned:       false,
        })
    }

    #[test]
    fn only_harness_stops_are_abnormal() {
        assert!(abnormal("hop-limit"));
        assert!(abnormal("max_tokens"));
        assert!(abnormal("error"));
        assert!(abnormal("empty"));
        assert!(abnormal("bad-call"));
        // The two that must never trigger a check.
        assert!(!abnormal("done"), "a model that finished is not cut short");
        assert!(!abnormal("interrupted"), "Stop means stop");
        assert!(!abnormal(""));
    }

    #[test]
    fn a_continuation_that_ends_itself_is_checked_a_human_turn_is_not() {
        assert!(needs_check("done", true));
        assert!(!needs_check("done", false), "ordinary conversation costs nothing extra");
        assert!(!needs_check("interrupted", true), "Stop means stop, continuation or not");
        assert!(needs_check("hop-limit", false));
    }

    /// The budget that let `sessions/92` stop after two continuations of a
    /// 124-line job: a continuation that got work done is refunded.
    #[test]
    fn continuations_that_make_progress_are_refunded() {
        let mut spent = Spent::default();
        for _ in 0..5 {
            assert!(spent.budget_left());
            spent.open();
            spent.refund();
        }
        assert_eq!(spent, Spent { opened: 5, stalled: 0 });
        // Two in a row with nothing done and the chain stops.
        spent.open();
        assert!(spent.budget_left());
        spent.open();
        assert!(!spent.budget_left());
        // And however well it goes, the total is bounded.
        let mut busy = Spent::default();
        while busy.budget_left() {
            busy.open();
            busy.refund();
        }
        assert_eq!(busy.opened, MAX_AUTO_CONTINUES);
    }

    #[test]
    fn progress_is_a_successful_tool_call_in_the_turn() {
        let events = vec![
            ev(1, EventKind::TurnStart { turn_id: 1, source: TurnSource::Human }),
            call(2, "read-file 'a'"),
            result(3, 2, true, "ok"),
            ev(4, EventKind::TurnStart { turn_id: 2, source: TurnSource::AutoContinue }),
            call(5, "read-file 'b'"),
            result(6, 5, false, "no such file"),
        ];
        assert!(made_progress(&events, 1));
        assert!(!made_progress(&events, 2), "a failed call is not progress");
        assert!(!made_progress(&events, 9));
    }

    #[test]
    fn the_advice_fits_how_the_turn_ended() {
        let v = Verdict { reached: false, reason: "file half written".into(), next_step: None };
        let p = continue_prompt(&v, "max_tokens", 1);
        assert!(p.contains("append=true"), "{p}");
        assert!(!p.contains("larger steps"), "wider calls are the wrong cure here: {p}");
        assert!(continue_prompt(&v, "hop-limit", 1).contains("larger steps"));
        assert!(continue_prompt(&v, "empty", 1).contains("inside your reasoning"));
        let done = continue_prompt(&v, "done", 3);
        assert!(done.contains("ended before the user's request was finished"), "{done}");
        assert!(!done.contains("cut short"), "{done}");
        assert!(done.contains(&format!("of=\"{MAX_AUTO_CONTINUES}\"")), "{done}");
    }

    #[test]
    fn parses_a_plain_object() {
        let v = parse(
            r#"{"reached": false, "reason": "read to line 100 of 214", "next_step": "read from 101"}"#,
        )
        .expect("valid verdict");
        assert!(!v.reached);
        assert_eq!(v.reason, "read to line 100 of 214");
        assert_eq!(v.next_step.as_deref(), Some("read from 101"));
    }

    #[test]
    fn parses_through_prose_fences_and_reasoning() {
        let raw = "Let me think.\n</think>\nHere you go:\n```json\n{\"reached\": true, \"reason\": \"all lines read\"}\n```\nHope that helps.";
        let v = parse(raw).expect("valid verdict");
        assert!(v.reached);
        assert_eq!(v.next_step, None, "a reached verdict carries no next step");
    }

    #[test]
    fn accepts_stringly_booleans() {
        assert!(!parse(r#"{"reached":"no","reason":"x"}"#).unwrap().reached);
        assert!(parse(r#"{"reached":"TRUE","reason":"x"}"#).unwrap().reached);
    }

    #[test]
    fn a_reply_without_reached_is_no_verdict() {
        assert_eq!(parse(r#"{"reason": "looks fine to me"}"#), None);
        assert_eq!(parse("I think it went well."), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse(r#"{"reached": 1}"#), None);
    }

    #[test]
    fn next_step_is_dropped_when_reached() {
        let v = parse(r#"{"reached":true,"reason":"done","next_step":"do more"}"#).unwrap();
        assert_eq!(v.next_step, None);
    }

    #[test]
    fn braces_inside_strings_do_not_end_the_object() {
        let v = parse(r#"{"reached":false,"reason":"saw a } brace","next_step":"go"}"#).unwrap();
        assert_eq!(v.reason, "saw a } brace");
    }

    #[test]
    fn fields_are_flattened_and_capped() {
        let long = "x".repeat(MAX_FIELD_LEN * 2);
        let raw = format!(r#"{{"reached":false,"reason":"a\nb","next_step":"{long}"}}"#);
        let v = parse(&raw).unwrap();
        assert_eq!(v.reason, "a b", "a newline would break the LogLine");
        let step = v.next_step.unwrap();
        assert!(step.chars().count() <= MAX_FIELD_LEN + 1, "{}", step.chars().count());
    }

    fn user(seq: u64, content: &str) -> SessionEvent {
        ev(seq, EventKind::UserMessage {
            surface: SurfaceOp::Append,
            content: content.into(),
            images:  Vec::new(),
        })
    }

    #[test]
    fn objective_is_the_humans_words_not_the_machines() {
        let events = vec![
            user(1, "read the wildcard file"),
            ev(2, EventKind::TurnStart { turn_id: 1, source: TurnSource::Human }),
            ev(3, EventKind::TurnEnd { turn_id: 1, finish_reason: "hop-limit".into(), hops: 12 }),
            user(4, "<auto_continue>keep reading</auto_continue>"),
            ev(5, EventKind::TurnStart { turn_id: 2, source: TurnSource::AutoContinue }),
        ];
        // The continuation turn is audited against the original request,
        // not against the prompt the harness wrote for it.
        assert_eq!(objective(&events, 2), "read the wildcard file");
        assert_eq!(objective(&events, 1), "read the wildcard file");
    }

    #[test]
    fn objective_ignores_goal_rounds_and_stops_at_the_turn() {
        let events = vec![
            user(1, "first ask"),
            ev(2, EventKind::TurnStart { turn_id: 1, source: TurnSource::Human }),
            user(3, "<goal_round/>"),
            ev(4, EventKind::TurnStart { turn_id: 2, source: TurnSource::GoalRound }),
            user(5, "second ask"),
            ev(6, EventKind::TurnStart { turn_id: 3, source: TurnSource::Followup }),
        ];
        assert_eq!(objective(&events, 2), "first ask", "a goal round is not the objective");
        // A later human turn must not leak backwards into an earlier one.
        assert_eq!(objective(&events, 1), "first ask");
        assert_eq!(objective(&events, 3), "second ask", "a followup is the person's own words");
    }

    #[test]
    fn objective_is_empty_without_a_human_turn() {
        let events = vec![ev(1, EventKind::TurnStart { turn_id: 1, source: TurnSource::GoalRound })];
        assert_eq!(objective(&events, 1), "");
    }

    #[test]
    fn digest_lists_calls_with_outcomes_and_last_words() {
        let events = vec![
            ev(1, EventKind::TurnStart { turn_id: 9, source: TurnSource::Human }),
            call(2, "read-file 'a.txt' 'start=1 end=6'"),
            result(3, 2, true, "1\tone"),
            call(4, "read-file 'a.txt' 'start=7 end=12'"),
            result(5, 4, false, "tool-hop limit (12) reached"),
            ev(6, EventKind::AssistantMessage {
                surface:    SurfaceOp::Append,
                content:    "Still reading.".into(),
                reasoning:  None,
                tool_calls: None,
            }),
        ];
        let d = digest(&events, 9);
        assert!(d.contains("2 total"), "{d}");
        assert!(d.contains("start=1 end=6") && d.contains("[ok]"), "{d}");
        assert!(d.contains("start=7 end=12") && d.contains("[FAILED]"), "{d}");
        assert!(d.contains("Still reading."), "{d}");
        assert!(!d.contains("1\tone"), "results are omitted, only calls: {d}");
    }

    #[test]
    fn digest_starts_at_the_named_turn() {
        let events = vec![
            ev(1, EventKind::TurnStart { turn_id: 1, source: TurnSource::Human }),
            call(2, "glob 'old'"),
            ev(3, EventKind::TurnEnd { turn_id: 1, finish_reason: "done".into(), hops: 1 }),
            ev(4, EventKind::TurnStart { turn_id: 2, source: TurnSource::Human }),
            call(5, "glob 'new'"),
        ];
        let d = digest(&events, 2);
        assert!(d.contains("new") && !d.contains("old"), "{d}");
        // A turn id that is not in the log yields nothing rather than the
        // whole session.
        assert_eq!(digest(&events, 99), "");
    }

    #[test]
    fn digest_keeps_the_newest_calls_when_over_the_cap() {
        let mut events =
            vec![ev(1, EventKind::TurnStart { turn_id: 1, source: TurnSource::Human })];
        for i in 0..MAX_DIGEST_CALLS + 5 {
            events.push(call(i as u64 + 2, &format!("grep 'call{i}'")));
        }
        let d = digest(&events, 1);
        assert!(d.contains("5 earlier call(s) omitted"), "{d}");
        assert!(d.contains(&format!("call{}", MAX_DIGEST_CALLS + 4)), "newest kept: {d}");
        assert!(!d.contains("'call0'"), "oldest dropped: {d}");
    }

    #[test]
    fn digest_says_so_when_nothing_ran() {
        let events = vec![ev(1, EventKind::TurnStart { turn_id: 1, source: TurnSource::Human })];
        assert!(digest(&events, 1).contains("no tool calls"));
    }

    #[test]
    fn continue_prompt_names_the_limit_and_disclaims_the_user() {
        let v = Verdict {
            reached:   false,
            reason:    "only lines 29-100 of 214 were read".into(),
            next_step: Some("read from line 101 in one call".into()),
        };
        let p = continue_prompt(&v, "hop-limit", 1);
        assert!(p.contains("hop-limit"));
        assert!(p.contains("read from line 101 in one call"));
        assert!(p.contains("only lines 29-100 of 214 were read"));
        assert!(p.contains("not sent a new message"));
        assert!(p.contains("attempt=\"1\""));
    }

    #[test]
    fn continue_prompt_falls_back_to_the_reason() {
        let v =
            Verdict { reached: false, reason: "half the files are unread".into(), next_step: None };
        let p = continue_prompt(&v, "max_tokens", 2);
        assert!(p.contains("Do this next: half the files are unread"));
    }
}
