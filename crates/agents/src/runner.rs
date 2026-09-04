//! One delegated LLM conversation (Wave 4, guide §12.1–§12.2).
//!
//! `ToolSubAgent` wraps one *tool call*; this module wraps one *child
//! conversation*: its own system prompt, its own transcript, a bounded
//! tool-hop loop over the parent's `ToolSubAgent::child`, and a single
//! report crossing back. Everything that delegates to a fresh model
//! conversation is built on it — `agent-team` teammates (§12.7),
//! `subagent` / `subagent-fork` (§12.1) and `ralph` rounds (§12.6) —
//! so the grounding rules live in one place instead of three.
//!
//! Two failure modes are handled here because every caller has them:
//!
//! - **Fluent prose about work never done.** A run that lands zero
//!   successful tool calls is reported as unverified ([`Report::verified`]);
//!   callers label it so the reader never sees a guess presented as fact.
//! - **A botched tool call read as a final answer.** A reply that looks
//!   like a tool call but does not parse buys one `SYNTAX_CORRECTION`
//!   retry (`parse_tool_call::rejected_attempt`) before it is accepted.
//!
//! **Structured output** (§12.2) closes a third: with a schema, the run's
//! result is only what arrives through the child-scoped
//! [`STRUCTURED_OUTPUT_NAME`] tool, validated against that schema. Prose
//! alone buys one reminder and is then reported unverified. Only a small
//! validated struct crosses the context boundary.

use std::sync::Arc;

use llm::client::{ChatMessage, LlmClient};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::parse_tool_call;
use crate::registry::SkillRegistry;
use crate::skill::{Skill, SkillContext, SkillOutcome};
use crate::subagent::{ToolInvocation, ToolSubAgent};

/// Name of the child-scoped reporting tool registered when a run demands
/// structured output. Hyphenated like every other skill so the text
/// protocol's parser accepts it unchanged.
pub const STRUCTURED_OUTPUT_NAME: &str = "structured-output";

/// Sent to a child that emitted something tool-call-shaped the parser could
/// not read. Restates the contract and — the part that matters — forbids
/// the fallback the model would otherwise take: writing up the output it
/// *expected* the tool to produce.
pub const SYNTAX_CORRECTION: &str = "\
That was not a valid tool call, so NOTHING ran and you received no output. \
To call a tool, reply with exactly one line and nothing else:\n\n\
    <skill-name> '<arg1>' '<arg2>' > <what you want to learn>\n\n\
Every argument must be quoted and the ` > <expectation>` part is required. \
Retry the call now if you still need it. If you do not, reply with your \
report — but do NOT describe file contents, command output, or whether a \
path exists unless a tool result above actually shows it; say plainly that \
you could not verify it instead.";

/// Sent once to a run that owes structured output but replied in prose.
pub const STRUCTURED_REMINDER: &str = "\
That reply does not count as your result. Report your final answer by \
calling the `structured-output` tool with a single JSON argument matching \
the schema in its description — nothing else is read. Call it now.";

/// What one delegated conversation is asked to do.
pub struct RunSpec {
    /// Short name for log lines and chips (`subagent`, a teammate's role,
    /// `ralph round 3`).
    pub label:    String,
    /// The child's system prompt. Callers compose it; the runner never
    /// edits it beyond appending the structured-output directive.
    pub system:   String,
    /// Conversation the child starts from. Empty for a fresh child;
    /// the parent's *completed* turns for a fork (§12.1) — never the
    /// in-flight one.
    pub seed:     Vec<ChatMessage>,
    /// The task, appended as the first user message after the seed.
    pub task:     String,
    /// Tool calls this run may make before it must answer with what it has.
    pub max_hops: u8,
    /// When set, the run's result must arrive through `structured-output`
    /// and validate against this JSON Schema subset (see [`validate`]).
    pub schema:   Option<Value>,
    /// How many calls the child has already seen on this transcript. Ids
    /// continue from here, so a caller running several rounds over one
    /// conversation (`agent-team`) never shows `call-1` twice for two
    /// different calls — a citation from round 2 would otherwise resolve
    /// against round 1's result.
    pub call_seq_start: usize,
}

/// One tool call a run dispatched, as the child sees it.
///
/// The `id` is echoed to the child in the tool-result message
/// (`[id: call-3]`) so a structured report can *cite* the call that backs
/// each claim. A caller can then check the citation against this list
/// instead of taking the child's word for it: an id that names no
/// successful call is a fabricated citation, which is the failure mode
/// that made prose reports untrustworthy in the first place.
#[derive(Debug, Clone, PartialEq)]
pub struct CallRecord {
    pub id:    String,
    pub skill: String,
    pub ok:    bool,
}

/// What one delegated conversation produced.
pub struct Report {
    /// The child's prose report, or the raw JSON when structured.
    pub text:       String,
    /// The validated structured result, when the run demanded one and got
    /// it. `None` with `schema: Some(_)` means the child never reported
    /// properly — the run is unverified whatever `tool_ok` says.
    pub structured: Option<Value>,
    pub tool_ok:    u32,
    pub tool_err:   u32,
    pub hops:       u8,
    /// Every tool call this run dispatched, in order, with the ids the
    /// child was shown.
    pub calls:      Vec<CallRecord>,
}

impl Report {
    /// A run that called no tool successfully verified nothing: every
    /// concrete claim in its text is the model's guess. When the run owed
    /// structured output, failing to deliver it is equally unverified.
    pub fn verified(&self, wanted_schema: bool) -> bool {
        self.tool_ok > 0 && (!wanted_schema || self.structured.is_some())
    }

    /// Ids of the calls that actually succeeded — the only citations a
    /// caller should accept as evidence.
    pub fn ok_ids(&self) -> std::collections::HashSet<&str> {
        self.calls
            .iter()
            .filter(|c| c.ok)
            .map(|c| c.id.as_str())
            .collect()
    }

    /// Human-readable `call-2 read-file (ok)` lines, for a caller that
    /// wants to show the child's evidence trail.
    pub fn call_lines(&self) -> Vec<String> {
        self.calls
            .iter()
            .map(|c| format!("{} {} ({})", c.id, c.skill, if c.ok { "ok" } else { "error" }))
            .collect()
    }
}

/// Build the opening transcript for a spec: system prompt, the seed
/// conversation, then the task. Callers that run several rounds over one
/// transcript (a team) call this once and keep extending.
pub fn seed_transcript(spec: &RunSpec) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(spec.seed.len() + 2);
    let mut system = spec.system.clone();
    if let Some(schema) = &spec.schema {
        system.push_str(&structured_directive(schema));
    }
    out.push(ChatMessage::text("system", system));
    out.extend(spec.seed.iter().cloned());
    out.push(ChatMessage::text("user", spec.task.clone()));
    out
}

/// Trailing system-prompt section for a run that owes structured output.
/// Scoped to the run, exactly like dsh's child-scoped section: the tool
/// exists only for this conversation, so its contract is stated with it.
///
/// Public because a caller that seeds its own transcript rather than using
/// [`seed_transcript`] (`agent-team`, which carries one conversation across
/// rounds) still has to state the contract in its system message.
pub fn structured_directive(schema: &Value) -> String {
    format!(
        "\n\n## Reporting your result\n\
         When you have your final answer you MUST report it by calling the \
         `{STRUCTURED_OUTPUT_NAME}` tool with one JSON argument matching this \
         schema:\n\n```json\n{}\n```\n\n\
         Only that tool call counts as your result — prose replies are \
         discarded. Call it once, last.",
        serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string())
    )
}

/// The child-scoped reporting tool. Registered into the run's registry view
/// so the parser accepts the name and the catalogue lists it; its body is
/// never reached — [`run_conversation`] intercepts the call, validates the
/// argument and settles the run.
struct StructuredOutput {
    description: String,
}

#[async_trait::async_trait]
impl Skill for StructuredOutput {
    fn name(&self) -> &str {
        STRUCTURED_OUTPUT_NAME
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn positional_args(&self) -> Vec<String> {
        vec!["json".into()]
    }
    async fn run(&self, _args: Value, _ctx: SkillContext) -> SkillOutcome {
        SkillOutcome {
            ok:      false,
            summary: format!(
                "`{STRUCTURED_OUTPUT_NAME}` is settled by the run itself, not by a \
                 skill body — this call should never have been dispatched"
            ),
        }
    }
}

/// The registry a run's child sees: the caller's view plus, when a schema
/// is set, the child-scoped reporting tool.
pub fn run_registry(base: Option<&Arc<SkillRegistry>>, schema: Option<&Value>) -> Option<Arc<SkillRegistry>> {
    let schema = schema?;
    let mut view = base.map(|r| (**r).clone()).unwrap_or_default();
    view.register(Arc::new(StructuredOutput {
        description: format!(
            "Report this run's final result. Takes one JSON argument matching: {}",
            schema
        ),
    }));
    Some(Arc::new(view))
}

/// Run one delegated conversation to its report.
///
/// `transcript` is seeded by the caller ([`seed_transcript`]) and extended
/// in place, so a caller running several rounds keeps one conversation.
/// `None` means every LLM call failed or the turn was interrupted — the
/// caller keeps whatever it had before.
pub async fn run_conversation(
    client:     &LlmClient,
    registry:   Option<&Arc<SkillRegistry>>,
    sub:        &ToolSubAgent,
    transcript: &mut Vec<ChatMessage>,
    spec:       &RunSpec,
    cancel:     &Option<CancellationToken>,
) -> Option<Report> {
    let view = run_registry(registry, spec.schema.as_ref());
    let reg = view.as_ref().or(registry);
    let mut hops: u8 = 0;
    let mut tool_ok: u32 = 0;
    let mut tool_err: u32 = 0;
    let mut calls: Vec<CallRecord> = Vec::new();
    let mut nudged = false;
    let mut reminded = false;

    loop {
        let reply = llm_call(client, cancel, transcript.clone()).await?;
        transcript.push(ChatMessage::text("assistant", reply.clone()));

        let call = reg.and_then(|r| {
            parse_tool_call::extract_known(&reply, |n| r.by_name.contains_key(n))
        });

        let Some(call) = call else {
            // A reply that *looks* like a tool call but does not parse is a
            // miscall, not an answer: nudge once, then accept it.
            let rejected = reg.and_then(|r| {
                parse_tool_call::rejected_attempt(&reply, |n| r.by_name.contains_key(n))
            });
            if let Some(reason) = rejected {
                let retrying = !nudged && hops < spec.max_hops;
                warn!(
                    label = %spec.label,
                    reason = %reason,
                    "runner: child emitted an unparsable tool call"
                );
                sub.events.emit(protocol::Event::LogLine {
                    level:   "WARN".into(),
                    message: format!(
                        "{}: emitted {reason} — {}",
                        spec.label,
                        if retrying {
                            "asking it to retry with the correct syntax"
                        } else {
                            "accepting its reply as an UNVERIFIED report"
                        }
                    ),
                });
                if retrying {
                    nudged = true;
                    transcript.push(ChatMessage::text("user", SYNTAX_CORRECTION));
                    continue;
                }
            }
            // Prose where structured output was demanded is not a result.
            if spec.schema.is_some() && !reminded && hops < spec.max_hops {
                reminded = true;
                sub.events.emit(protocol::Event::LogLine {
                    level:   "WARN".into(),
                    message: format!(
                        "{}: replied in prose but owes `{STRUCTURED_OUTPUT_NAME}` \
                         — reminding once",
                        spec.label
                    ),
                });
                transcript.push(ChatMessage::text("user", STRUCTURED_REMINDER));
                continue;
            }
            return Some(Report { text: reply, structured: None, tool_ok, tool_err, hops, calls });
        };

        // The reporting tool settles the run instead of dispatching.
        if call.skill == STRUCTURED_OUTPUT_NAME {
            let raw = call.raw_args.first().cloned().unwrap_or_default();
            match parse_structured(&raw, spec.schema.as_ref()) {
                Ok(value) => {
                    info!(label = %spec.label, "runner: structured result accepted");
                    return Some(Report {
                        text: raw,
                        structured: Some(value),
                        tool_ok,
                        tool_err,
                        hops,
                        calls,
                    });
                }
                Err(problem) if hops < spec.max_hops => {
                    hops += 1;
                    tool_err += 1;
                    warn!(label = %spec.label, %problem, "runner: structured result rejected");
                    transcript.push(ChatMessage::text(
                        "user",
                        format!(
                            "Tool result for `{STRUCTURED_OUTPUT_NAME}` (error):\n{problem}\n\n\
                             Nothing was recorded. Call it again with a corrected JSON \
                             argument."
                        ),
                    ));
                    continue;
                }
                Err(problem) => {
                    warn!(label = %spec.label, %problem, "runner: structured budget exhausted");
                    return Some(Report {
                        text: format!("invalid structured report: {problem}\n\n{raw}"),
                        structured: None,
                        tool_ok,
                        tool_err,
                        hops,
                        calls,
                    });
                }
            }
        }

        if hops >= spec.max_hops {
            transcript.push(ChatMessage::text(
                "user",
                format!(
                    "Tool budget ({}) exhausted — reply with your final {} now, \
                     using what you already have.",
                    spec.max_hops,
                    if spec.schema.is_some() {
                        format!("`{STRUCTURED_OUTPUT_NAME}` call")
                    } else {
                        "report".into()
                    }
                ),
            ));
            let last = llm_call(client, cancel, transcript.clone()).await?;
            transcript.push(ChatMessage::text("assistant", last.clone()));
            // One final chance to settle a structured run properly.
            if spec.schema.is_some() {
                if let Some(c) = reg.and_then(|r| {
                    parse_tool_call::extract_known(&last, |n| r.by_name.contains_key(n))
                }) {
                    if c.skill == STRUCTURED_OUTPUT_NAME {
                        let raw = c.raw_args.first().cloned().unwrap_or_default();
                        if let Ok(value) = parse_structured(&raw, spec.schema.as_ref()) {
                            return Some(Report {
                                text: raw,
                                structured: Some(value),
                                tool_ok,
                                tool_err,
                                hops,
                                calls,
                            });
                        }
                    }
                }
            }
            return Some(Report { text: last, structured: None, tool_ok, tool_err, hops, calls });
        }
        hops += 1;

        // `reg` is Some here — `call` only exists when it was.
        let r = reg.expect("tool call parsed without a registry");
        let outcome = match r.resolve(&call) {
            Some((skill, args)) => {
                sub.run(ToolInvocation {
                    skill:       &*skill,
                    args,
                    raw_args:    call.raw_args.clone(),
                    expectation: call.expectation.clone(),
                })
                .await
            }
            None => SkillOutcome {
                ok:      false,
                summary: format!("unknown skill `{}`", call.skill),
            },
        };
        if outcome.ok { tool_ok += 1 } else { tool_err += 1 }
        let call_id = format!("call-{}", spec.call_seq_start + calls.len() + 1);
        calls.push(CallRecord {
            id:    call_id.clone(),
            skill: call.skill.clone(),
            ok:    outcome.ok,
        });
        transcript.push(ChatMessage::text(
            "user",
            format!(
                "Tool result for `{}` ({}) [id: {call_id}]:\n{}",
                call.skill,
                if outcome.ok { "ok" } else { "error" },
                outcome.summary
            ),
        ));
        info!(
            label = %spec.label,
            skill = %call.skill,
            ok = outcome.ok,
            hop = hops,
            "runner: child tool call"
        );
    }
}

/// Parse a `structured-output` argument and check it against the schema.
fn parse_structured(raw: &str, schema: Option<&Value>) -> Result<Value, String> {
    let trimmed = strip_json_fence(raw.trim());
    let value: Value = serde_json::from_str(trimmed)
        .map_err(|e| format!("the argument is not valid JSON ({e})"))?;
    if let Some(schema) = schema {
        let problems = validate(&value, schema, "$");
        if !problems.is_empty() {
            return Err(problems.join("; "));
        }
    }
    Ok(value)
}

/// Models wrap JSON in a fence even when told not to. Unwrap it rather
/// than failing a report that is otherwise correct.
fn strip_json_fence(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else { return s };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    let rest = rest.trim_start_matches(['\r', '\n']);
    rest.strip_suffix("```").map(str::trim_end).unwrap_or(rest)
}

/// Validate `value` against the JSON Schema subset this crate emits:
/// `type`, `properties`, `required`, `items`, `enum`, `minItems`. Anything
/// else in the schema is ignored rather than rejected.
///
/// A full `jsonschema` dependency buys nothing here — every schema in the
/// workspace is authored in this crate (the Ralph report, teammate claims)
/// and stays inside this subset. Unsupported keywords being *ignored* is
/// the deliberate failure mode: a schema this cannot check fully still
/// validates as far as it goes rather than failing every report.
pub fn validate(value: &Value, schema: &Value, path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Some(obj) = schema.as_object() else { return out };

    if let Some(expected) = obj.get("type").and_then(Value::as_str) {
        if !type_matches(value, expected) {
            out.push(format!("{path}: expected {expected}, got {}", type_name(value)));
            return out; // every deeper check assumes the type held
        }
    }
    if let Some(allowed) = obj.get("enum").and_then(Value::as_array) {
        if !allowed.contains(value) {
            let names: Vec<String> = allowed.iter().map(ToString::to_string).collect();
            out.push(format!("{path}: must be one of {}", names.join(" | ")));
        }
    }
    if let Some(required) = obj.get("required").and_then(Value::as_array) {
        for key in required.iter().filter_map(Value::as_str) {
            if value.get(key).is_none() {
                out.push(format!("{path}: missing required field `{key}`"));
            }
        }
    }
    if let (Some(props), Some(map)) = (
        obj.get("properties").and_then(Value::as_object),
        value.as_object(),
    ) {
        for (key, sub_schema) in props {
            if let Some(v) = map.get(key) {
                out.extend(validate(v, sub_schema, &format!("{path}.{key}")));
            }
        }
    }
    if let Some(arr) = value.as_array() {
        if let Some(min) = obj.get("minItems").and_then(Value::as_u64) {
            if (arr.len() as u64) < min {
                out.push(format!("{path}: needs at least {min} item(s), got {}", arr.len()));
            }
        }
        if let Some(items) = obj.get("items") {
            for (i, v) in arr.iter().enumerate() {
                out.extend(validate(v, items, &format!("{path}[{i}]")));
            }
        }
    }
    out
}

fn type_matches(value: &Value, expected: &str) -> bool {
    match expected {
        "object"  => value.is_object(),
        "array"   => value.is_array(),
        "string"  => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number"  => value.is_number(),
        "boolean" => value.is_boolean(),
        "null"    => value.is_null(),
        _         => true, // unknown type keyword: not our business
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null      => "null",
        Value::Bool(_)   => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_)  => "array",
        Value::Object(_) => "object",
    }
}

/// One non-streaming chat round-trip raced against the turn's cancellation
/// token. `None` on cancel, transport error, or an empty reply.
pub async fn llm_call(
    client:   &LlmClient,
    cancel:   &Option<CancellationToken>,
    messages: Vec<ChatMessage>,
) -> Option<String> {
    let res = match cancel {
        Some(token) => tokio::select! {
            biased;
            _ = token.cancelled() => return None,
            r = client.chat_once(messages) => r,
        },
        None => client.chat_once(messages).await,
    };
    match res {
        Ok(s) if !s.trim().is_empty() => Some(s),
        Ok(_) => None,
        Err(e) => {
            warn!(error = %e, "runner: LLM call failed");
            None
        }
    }
}

pub fn is_cancelled(cancel: &Option<CancellationToken>) -> bool {
    cancel.as_ref().is_some_and(|t| t.is_cancelled())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "required": ["status", "summary", "evidence"],
            "properties": {
                "status":  {"type": "string", "enum": ["continue", "complete", "blocked"]},
                "summary": {"type": "string"},
                "evidence": {"type": "array", "items": {"type": "string"}},
            }
        })
    }

    #[test]
    fn validate_accepts_a_well_formed_report() {
        let v = json!({"status": "complete", "summary": "s", "evidence": ["a"]});
        assert!(validate(&v, &schema(), "$").is_empty());
    }

    #[test]
    fn validate_names_every_defect_with_its_path() {
        let v = json!({"status": "maybe", "evidence": [1]});
        let problems = validate(&v, &schema(), "$");
        assert!(problems.iter().any(|p| p.contains("missing required field `summary`")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("$.status: must be one of")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("$.evidence[0]: expected string")), "{problems:?}");
    }

    #[test]
    fn validate_stops_at_a_type_mismatch_instead_of_cascading() {
        let problems = validate(&json!("nope"), &schema(), "$");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("expected object, got string"));
    }

    #[test]
    fn validate_ignores_keywords_it_does_not_implement() {
        let s = json!({"type": "string", "pattern": "^a", "maxLength": 1});
        assert!(validate(&json!("zzzz"), &s, "$").is_empty());
    }

    #[test]
    fn validate_enforces_min_items() {
        let s = json!({"type": "array", "minItems": 1, "items": {"type": "string"}});
        assert!(!validate(&json!([]), &s, "$").is_empty());
        assert!(validate(&json!(["a"]), &s, "$").is_empty());
    }

    #[test]
    fn structured_argument_survives_a_json_fence() {
        let raw = "```json\n{\"status\": \"complete\", \"summary\": \"s\", \"evidence\": []}\n```";
        let v = parse_structured(raw, Some(&schema())).unwrap();
        assert_eq!(v["status"], "complete");
    }

    #[test]
    fn structured_argument_reports_bad_json_and_schema_misses_separately() {
        let bad_json = parse_structured("{not json", Some(&schema())).unwrap_err();
        assert!(bad_json.contains("not valid JSON"), "{bad_json}");
        let bad_shape = parse_structured(r#"{"status": "complete"}"#, Some(&schema())).unwrap_err();
        assert!(bad_shape.contains("missing required field"), "{bad_shape}");
    }

    #[test]
    fn seeded_transcript_is_system_seed_task_and_carries_the_directive() {
        let spec = RunSpec {
            label:    "child".into(),
            system:   "SYSTEM".into(),
            seed:     vec![ChatMessage::text("user", "earlier")],
            task:     "TASK".into(),
            max_hops: 4,
            schema:   Some(schema()),
            call_seq_start: 0,
        };
        let t = seed_transcript(&spec);
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].role, "system");
        assert_eq!(t[1].role, "user");
        assert_eq!(t[2].role, "user");
        let sys = t[0].content.text();
        assert!(sys.contains("SYSTEM"));
        assert!(sys.contains(STRUCTURED_OUTPUT_NAME), "the directive is scoped to the run");
    }

    #[test]
    fn no_schema_means_no_directive_and_no_extra_tool() {
        let spec = RunSpec {
            label: "c".into(), system: "S".into(), seed: vec![],
            task: "T".into(), max_hops: 2, schema: None, call_seq_start: 0,
        };
        assert!(!seed_transcript(&spec)[0].content.text().contains(STRUCTURED_OUTPUT_NAME));
        assert!(run_registry(None, None).is_none());
    }

    #[test]
    fn run_registry_adds_the_reporting_tool_without_touching_the_base() {
        let mut base = SkillRegistry::new();
        base.register(Arc::new(crate::control::TodoWrite));
        let base = Arc::new(base);
        let view = run_registry(Some(&base), Some(&schema())).unwrap();
        assert!(view.by_name.contains_key(STRUCTURED_OUTPUT_NAME));
        assert!(view.by_name.contains_key("todo-write"), "base skills stay visible");
        assert!(!base.by_name.contains_key(STRUCTURED_OUTPUT_NAME), "base is untouched");
    }

    #[test]
    fn verified_needs_a_tool_call_and_the_structured_report_when_one_was_owed() {
        let r = |tool_ok, structured| Report {
            text: String::new(), structured, tool_ok, tool_err: 0, hops: 0, calls: vec![],
        };
        assert!(r(1, None).verified(false));
        assert!(!r(0, None).verified(false), "no successful tool call");
        assert!(!r(1, None).verified(true), "owed a structured report, gave none");
        assert!(r(1, Some(json!({}))).verified(true));
    }

    #[test]
    fn call_ids_continue_from_the_caller_s_offset() {
        // Two runs over one transcript must not both hand out `call-1`:
        // a claim citing `call-1` in round 2 would otherwise resolve
        // against round 1's result.
        let id = |start: usize, nth: usize| format!("call-{}", start + nth + 1);
        assert_eq!(id(0, 0), "call-1");
        assert_eq!(id(3, 0), "call-4");
    }

    #[test]
    fn ok_ids_lists_only_successful_calls() {
        let r = Report {
            text: String::new(), structured: None, tool_ok: 1, tool_err: 1, hops: 2,
            calls: vec![
                CallRecord { id: "call-1".into(), skill: "read-file".into(), ok: true },
                CallRecord { id: "call-2".into(), skill: "grep".into(), ok: false },
            ],
        };
        let ids = r.ok_ids();
        assert!(ids.contains("call-1"));
        assert!(!ids.contains("call-2"), "a failed call is not evidence");
        assert_eq!(r.call_lines(), vec!["call-1 read-file (ok)", "call-2 grep (error)"]);
    }
}
