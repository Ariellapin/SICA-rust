//! `model-eval`: run a suite of prompts against the connected model and score
//! every reply against declarative checks.
//!
//! This is the measurement counterpart to `memory.md` and the skill docs. The
//! app's accuracy problem is not "is the model good" but "does *this* prompt
//! configuration make *this* model behave" — and that question is only
//! answerable by replaying a fixed set of prompts and diffing the score after
//! each edit. So a run:
//!
//! 1. rebuilds the **real** system prompt (`memory.md` + the live
//!    `## Loaded skills` catalogue, exactly as `chat.rs::build_history` does)
//!    so the thing under test is the shipped configuration, not a lab one;
//! 2. sends each case prompt as a fresh single-turn conversation, `repeats`
//!    times, so flapping shows up as a pass *rate* rather than a coin flip;
//! 3. scores the reply with checks that mirror the runtime's own contracts —
//!    tool calls are validated with `parse_tool_call`, the same parser
//!    `chat.rs` dispatches through, so a case that passes here is a call the
//!    backend would actually have executed;
//! 4. writes a Markdown report plus a JSON baseline, and diffs the current run
//!    against the previous baseline for the same suite.
//!
//! Nothing is ever dispatched. A case that expects `run-cli 'rm -rf …'` only
//! checks that the *call was formed correctly* — the eval never executes a
//! skill, so a suite is safe to run unattended.
//!
//! Failures are bucketed by [`FailKind`], and each bucket maps to a concrete
//! lever (a `memory.md` section, a skill description, the sampling
//! temperature). That mapping is the deliverable: a score alone tells the
//! operator nothing about what to change next.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Instant;

use async_trait::async_trait;
use protocol::Event;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use llm::client::{ChatMessage, LlmClient};

use crate::parse_tool_call;
use crate::registry::SkillRegistry;
use crate::skill::{Skill, SkillContext, SkillOutcome};

pub const MODEL_EVAL_NAME: &str = "model-eval";

pub const MODEL_EVAL_DESCRIPTION: &str =
    "Benchmark the connected model against a prompt suite and report what to \
     fix. Positional args: <suite> (name in evals/, or a .toml path; empty = \
     default) <repeats> (1-5) <filter> (case id / category substring). Writes \
     a full report under evals/reports/ and returns the score, the failing \
     cases and the prompt changes they point at.";

/// Hard cap on cases per suite — each case is `repeats` LLM round-trips.
const MAX_CASES: usize = 40;
/// More than five repeats measures the server's mood, not the prompt.
const MAX_REPEATS: u32 = 5;
/// Global stop on LLM calls (cases × repeats, plus judge calls) so a fat
/// suite cannot turn one tool call into an hour of generation.
const MAX_CALLS: u32 = 150;
/// Reply excerpt kept per failing case in the report.
const EXCERPT_CHARS: usize = 600;
/// The returned `summary` is what the main agent re-ingests. `ToolSubAgent`
/// re-summarises anything over 2 KB through the LLM, which would paraphrase
/// the numbers — so the summary stays under that threshold and points at the
/// report file for the rest.
const SUMMARY_CAP: usize = 1800;

pub const MODEL_EVAL_SEED_MD: &str = r#"---
name: model-eval
description: Benchmark the connected model against a prompt suite and report which prompt changes would fix the failures.
---
Replay a fixed suite of prompts against the **currently connected** model,
score every reply against declarative checks, and write a report saying what
to change to make the model behave better.

Invocation (single line):

    model-eval '<suite>' '<repeats>' '<filter>' > <what you want to know>

Examples:

    model-eval > overall score and the worst category
    model-eval 'default' '3' > which cases are flaky
    model-eval 'default' '2' 'tool-syntax' > do tool calls parse after my memory.md edit

Arguments (all optional):
- `suite` — a name under `evals/` (`default` → `evals/default.toml`) or a
  path to a `.toml` file. Empty means `default`.
- `repeats` — how many times each case runs, 1–5 (suite default is 2). More
  than one repeat is what turns a pass/fail into a **pass rate**, which is
  the only way to see non-determinism.
- `filter` — substring matched against each case's `id` and `category`; only
  matching cases run.

What it does:
- Rebuilds the real system prompt (`memory.md` + the live skill catalogue),
  so the run measures the shipped configuration.
- Sends each case as its own single-turn conversation — no history, so cases
  cannot contaminate each other.
- Scores replies with the **same** tool-call parser the backend dispatches
  through. A case that expects a tool call passes only if the backend would
  really have run that call.
- **Never executes a skill.** Tool-call cases are parse-only, so a suite is
  safe to run unattended.
- Writes `evals/reports/<suite>-<timestamp>.md` (full detail, every failing
  reply excerpt) and `<suite>-<timestamp>.json` (the baseline the next run
  diffs against), then returns a compact score + fix list.

Suite format — `evals/default.toml`:

    [suite]
    name        = "default"
    description = "what this suite is for"
    system      = "live"   # "live" = memory.md + skill catalogue, "none", or literal prompt text
    repeats     = 2
    # temperature = 0.0    # optional override; omitted = the connection's own setting

    [[case]]
    id       = "read-file-basic"
    category = "tool-syntax"
    prompt   = "Read the file skills/read-file.md and say which arguments it declares."
    expect_tool         = ["read-file"]
    expect_args_contain = ["read-file.md"]
    single_tool_call    = true

Checks (all optional, all combinable):
- `expect_tool` — string or list; the reply must parse as a call to one of them.
- `expect_args_contain` / `min_args` — argument content and count.
- `require_expectation` — the ` > <expectation>` clause must be present
  (defaults to true whenever `expect_tool` is set).
- `expect_no_tool` — the reply must contain no tool call *and* nothing
  tool-call-shaped that the parser rejected.
- `single_tool_call` — at most one call per message (only the first ever runs).
- `contains` / `contains_any` / `not_contains` — case-insensitive substrings.
- `regex` / `not_regex` — full regex syntax, validated when the suite loads.
- `json` — the reply (or its single fenced block) must parse as JSON.
- `min_words` / `max_words` — length contract.
- `judge` — a rubric graded by a second LLM call. The judge is the *same*
  model, so it is the weakest signal in the report and is labelled as such.

Reading the report: every failure is bucketed (missing call, malformed call,
wrong skill, bad args, over-calling, content, format, length, judge) and each
bucket names the lever that fixes it — a `memory.md` section, a skill
description, or the sampling temperature. A case that passes some repeats and
fails others is flagged as flaky: that is a sampling problem, not a prompt one.
"#;

/// Seeded on first backend start. Deliberately small, fast and boring: eleven
/// cases covering the failure modes this app actually hits — call syntax,
/// over-calling, argument order, the one-call rule, grounding, output shape.
pub const DEFAULT_SUITE_TOML: &str = r#"# Default model-eval suite.
#
# Every case runs as its own single-turn conversation against the connected
# model, with the real system prompt (memory.md + the live skill catalogue).
# Nothing here is executed — tool-call cases only check that the call the
# model wrote would have parsed and dispatched.
#
# Edit freely: add cases for the prompts your own work actually depends on.
# Re-run after every memory.md / skill-doc edit; the report diffs against the
# previous run so you can see whether the edit helped.

[suite]
name        = "default"
description = "Baseline behaviour: tool-call syntax, restraint, grounding, output shape."
system      = "live"
repeats     = 2

# --- tool syntax: does the model form calls the parser accepts? -------------

[[case]]
id       = "read-file-basic"
category = "tool-syntax"
prompt   = "Which positional arguments does the read-file skill accept? Check its documentation file skills/read-file.md before answering."
expect_tool         = ["read-file"]
expect_args_contain = ["read-file.md"]
single_tool_call    = true

[[case]]
id       = "shell-version"
category = "tool-syntax"
prompt   = "Find out which version of cargo is installed on this machine."
expect_tool         = ["run-pwsh", "run-cli"]
expect_args_contain = ["cargo"]
single_tool_call    = true

[[case]]
id       = "write-two-args"
category = "tool-args"
prompt   = "Create a file at notes/eval-probe.md whose entire contents are the line: hello from the eval"
expect_tool         = ["write-file"]
min_args            = 2
expect_args_contain = ["notes/eval-probe.md", "hello from the eval"]

[[case]]
id       = "one-call-at-a-time"
category = "protocol"
prompt   = "Compare memory.md with skills/run-cli.md and tell me which one documents the ` > ` expectation clause."
expect_tool      = ["read-file"]
single_tool_call = true

# --- restraint: does it stop reaching for tools it doesn't need? ------------

[[case]]
id       = "no-tool-for-arithmetic"
category = "restraint"
prompt   = "What is 17 * 3? Reply with just the number."
expect_no_tool = true
contains       = ["51"]
max_words      = 6

[[case]]
id       = "no-tool-for-definition"
category = "restraint"
prompt   = "In one sentence, what is a trait in Rust?"
expect_no_tool = true
max_words      = 60

# --- grounding: does it admit what it has not checked? ----------------------

[[case]]
id       = "grounding-missing-file"
category = "grounding"
prompt   = 'What are the contents of C:\sica-eval\does-not-exist-9f3a.txt?'
judge    = "PASS if the reply either issues a tool call to look the file up, or says plainly that it has not read the file / cannot know without checking. FAIL if it states or paraphrases any supposed contents of the file, or asserts the file exists or does not exist as established fact."

[[case]]
id       = "no-fabricated-tool-result"
category = "grounding"
prompt   = "Does a file named README.md exist at the workspace root?"
not_contains = ["tool_result"]
judge        = "PASS if the reply issues a tool call to check, or says it needs to check. FAIL if it answers yes or no as established fact without a tool call, or if it writes out what a tool supposedly returned."

# --- output shape: does it follow an explicit format instruction? -----------

[[case]]
id       = "format-single-word"
category = "format"
prompt   = "Reply with exactly one word: OK. No punctuation, no explanation, no preamble."
regex     = "(?i)^\\W*ok\\W*$"
max_words = 2

[[case]]
id       = "format-json"
category = "format"
prompt   = "Reply with a JSON object with exactly two keys, name and purpose, describing the app you are running inside. No prose, no code fence."
json      = true
contains  = ["name", "purpose"]

[[case]]
id       = "follow-negative-constraint"
category = "instruction"
prompt   = "In at most 25 words, say what a named pipe is. Do not use the word 'very' anywhere in your answer."
not_contains = ["very"]
max_words    = 35
"#;

// ---------------------------------------------------------------------------
// Suite model
// ---------------------------------------------------------------------------

/// Raw TOML shape. Normalised into [`Suite`] by [`load_suite`].
#[derive(Debug, Default, Deserialize)]
struct SuiteFile {
    #[serde(default)]
    suite: SuiteMeta,
    /// `[[case]]` blocks; `[[cases]]` is accepted too because that is the
    /// spelling everyone tries first.
    #[serde(default, alias = "cases")]
    case: Vec<Case>,
}

#[derive(Debug, Default, Deserialize)]
struct SuiteMeta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    /// `"live"` (default), `"none"`, or a literal system prompt.
    #[serde(default)]
    system: String,
    #[serde(default)]
    repeats: Option<u32>,
    #[serde(default)]
    temperature: Option<f32>,
}

/// One prompt plus every check applied to its reply.
#[derive(Debug, Clone, Deserialize)]
pub struct Case {
    pub id: String,
    #[serde(default = "default_category")]
    pub category: String,
    pub prompt: String,

    #[serde(default)]
    pub expect_tool: Option<StringOrList>,
    #[serde(default)]
    pub expect_args_contain: Vec<String>,
    #[serde(default)]
    pub min_args: Option<usize>,
    /// Defaults to true when `expect_tool` is set — the missing ` > ` clause
    /// is the single most common malformed call.
    #[serde(default)]
    pub require_expectation: Option<bool>,
    #[serde(default)]
    pub expect_no_tool: bool,
    #[serde(default)]
    pub single_tool_call: bool,

    #[serde(default)]
    pub contains: Vec<String>,
    #[serde(default)]
    pub contains_any: Vec<String>,
    #[serde(default)]
    pub not_contains: Vec<String>,
    #[serde(default)]
    pub regex: Option<String>,
    #[serde(default)]
    pub not_regex: Option<String>,
    #[serde(default)]
    pub json: bool,
    #[serde(default)]
    pub min_words: Option<usize>,
    #[serde(default)]
    pub max_words: Option<usize>,
    #[serde(default)]
    pub judge: Option<String>,
}

fn default_category() -> String {
    "general".to_string()
}

/// `expect_tool = "read-file"` and `expect_tool = ["read-file", "run-cli"]`
/// are both natural to write, so both are accepted.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StringOrList {
    One(String),
    Many(Vec<String>),
}

impl StringOrList {
    fn as_slice(&self) -> Vec<&str> {
        match self {
            StringOrList::One(s) => vec![s.as_str()],
            StringOrList::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SystemMode {
    /// `memory.md` + the live skill catalogue — what a real turn sends.
    Live,
    /// No system message at all: measures the bare model.
    None,
    Custom(String),
}

impl SystemMode {
    fn label(&self) -> &'static str {
        match self {
            SystemMode::Live => "live (memory.md + skill catalogue)",
            SystemMode::None => "none (bare model)",
            SystemMode::Custom(_) => "custom (suite-provided)",
        }
    }
}

#[derive(Debug)]
pub struct Suite {
    pub name: String,
    pub description: String,
    pub system: SystemMode,
    pub repeats: u32,
    pub temperature: Option<f32>,
    pub cases: Vec<Case>,
}

/// Parse suite TOML and validate everything that can be validated without an
/// LLM — a broken regex or an empty case list should fail before the first
/// (slow, billable) round-trip, not halfway through one.
pub fn parse_suite(text: &str, fallback_name: &str) -> Result<Suite, String> {
    let file: SuiteFile =
        toml::from_str(text).map_err(|e| format!("suite is not valid TOML: {e}"))?;
    if file.case.is_empty() {
        return Err("suite has no `[[case]]` blocks".into());
    }
    let mut seen: Vec<&str> = Vec::new();
    for (i, c) in file.case.iter().enumerate() {
        if c.id.trim().is_empty() {
            return Err(format!("case[{i}] has an empty `id`"));
        }
        if c.prompt.trim().is_empty() {
            return Err(format!("case `{}` has an empty `prompt`", c.id));
        }
        if seen.contains(&c.id.as_str()) {
            return Err(format!("duplicate case id `{}`", c.id));
        }
        seen.push(&c.id);
        for (field, pat) in [("regex", &c.regex), ("not_regex", &c.not_regex)] {
            if let Some(p) = pat {
                regex::Regex::new(p)
                    .map_err(|e| format!("case `{}`: invalid {field}: {e}", c.id))?;
            }
        }
    }
    let mut cases = file.case;
    let dropped = cases.len().saturating_sub(MAX_CASES);
    cases.truncate(MAX_CASES);
    if dropped > 0 {
        warn!(dropped, "model-eval: suite truncated to the case cap");
    }
    let system = match file.suite.system.trim() {
        "" | "live" => SystemMode::Live,
        "none" => SystemMode::None,
        other => SystemMode::Custom(other.to_string()),
    };
    let name = if file.suite.name.trim().is_empty() {
        fallback_name.to_string()
    } else {
        file.suite.name.trim().to_string()
    };
    Ok(Suite {
        name,
        description: file.suite.description,
        system,
        repeats: file.suite.repeats.unwrap_or(2).clamp(1, MAX_REPEATS),
        temperature: file.suite.temperature,
        cases,
    })
}

/// Resolve the `suite` argument to a file path.
///
/// `""` → `<evals>/default.toml`; a bare name → `<evals>/<name>.toml`;
/// anything ending in `.toml` → that path, relative to the workspace root.
pub fn resolve_suite_path(root: &Path, evals: &Path, arg: &str) -> PathBuf {
    let arg = arg.trim();
    if arg.is_empty() {
        return evals.join("default.toml");
    }
    if arg.ends_with(".toml") {
        let p = Path::new(arg);
        return if p.is_absolute() { p.to_path_buf() } else { root.join(p) };
    }
    evals.join(format!("{arg}.toml"))
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

/// Failure buckets. The bucket — not the individual case — is what maps to an
/// action, so this enum is effectively the report's table of contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FailKind {
    /// Expected a tool call, got prose.
    NoCall,
    /// Something tool-call-shaped that the parser refused (so nothing ran).
    MalformedCall,
    /// A valid call to the wrong skill.
    WrongTool,
    /// A valid call missing its ` > <expectation>` clause.
    MissingExpectation,
    /// Right skill, wrong arguments.
    BadArgs,
    /// Called a tool for a question that needed none.
    UnexpectedCall,
    /// More than one call in a single message — only the first would run.
    ExtraCalls,
    /// Substring / regex content contract broken.
    Content,
    /// Output-shape contract broken (regex, JSON).
    Format,
    /// Length contract broken.
    Length,
    /// The LLM judge rejected the answer.
    Judge,
}

impl FailKind {
    pub fn label(self) -> &'static str {
        match self {
            FailKind::NoCall => "no tool call",
            FailKind::MalformedCall => "malformed tool call",
            FailKind::WrongTool => "wrong skill",
            FailKind::MissingExpectation => "missing ` > ` clause",
            FailKind::BadArgs => "bad arguments",
            FailKind::UnexpectedCall => "unnecessary tool call",
            FailKind::ExtraCalls => "more than one call",
            FailKind::Content => "wrong content",
            FailKind::Format => "wrong format",
            FailKind::Length => "wrong length",
            FailKind::Judge => "judge rejected",
        }
    }

    /// What to change. Deliberately concrete: the operator should be able to
    /// act on this without re-reading the transcript.
    pub fn lever(self) -> &'static str {
        match self {
            FailKind::NoCall =>
                "The model answered from memory where the case required a tool. \
                 Make the skill's one-line `description:` in `skills/<name>.md` say **when** to \
                 reach for it, not only what it does; check the skill is listed in `memory.md`'s \
                 `## Skills` index; add a worked call to the skill doc so the shape is in context.",
            FailKind::MalformedCall =>
                "The reply was tool-call-shaped but the parser rejected it, so nothing ran — this \
                 is pure syntax. Drop `temperature` to 0.0–0.2, paste the failing shape into \
                 `memory.md`'s `## Invocation` section as a counter-example beside the correct one, \
                 and if the server supports OpenAI function calling turn on `native_tools` for the \
                 provider to bypass the text protocol entirely.",
            FailKind::WrongTool =>
                "A valid call to the wrong skill: two descriptions read alike. Sharpen them against \
                 each other (`run-cli` vs `run-pwsh` is the usual pair) and add an explicit \
                 'use X instead when …' clause to both.",
            FailKind::MissingExpectation =>
                "Calls arrived without the ` > <expectation>` clause, which the parser requires — \
                 the call is dropped. State in `memory.md` that a line without ` > ` does not run \
                 at all, and make every example in every skill doc carry the clause.",
            FailKind::BadArgs =>
                "Right skill, wrong arguments. The `positional:` frontmatter order is the contract: \
                 restate it in the skill doc with one example per argument, and name the argument \
                 in the description (`Positional args: <path> <content>`).",
            FailKind::UnexpectedCall =>
                "The model called a tool for a question it could answer directly — that costs a \
                 round-trip and invites fabricated tool output. Add a rule to `memory.md`: answer \
                 directly when nothing about the host system is involved.",
            FailKind::ExtraCalls =>
                "More than one call in one message; the backend executes only the first, so the \
                 rest silently vanish. `memory.md` already carries the one-call rule — move it \
                 above the examples, or repeat it in the `## Loaded skills` header.",
            FailKind::Content =>
                "The answer was wrong or omitted what the case required. If the case needed host \
                 facts, this is a grounding failure: strengthen the 'base every claim on a tool \
                 result' rule. If it needed reasoning, the model is likely too small for the task — \
                 compare against a larger one before rewriting prompts.",
            FailKind::Format =>
                "Explicit output-shape instructions were ignored. Put the format requirement in the \
                 **last** line of the prompt, show one line of the exact expected output, and lower \
                 the temperature; small models follow a shown example far better than a described one.",
            FailKind::Length =>
                "The length contract was ignored — usually preamble ('Sure! Here is …'). Say 'answer \
                 with X and nothing else' and, in `memory.md`, forbid preambles globally.",
            FailKind::Judge =>
                "The judge rejected the answer on the rubric — typically an ungrounded claim stated \
                 as fact. Strengthen the grounding rule in `memory.md`. Note the judge is the same \
                 model being tested, so read the excerpt before acting on it.",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub kind: FailKind,
    pub detail: String,
}

fn fail(kind: FailKind, detail: impl Into<String>) -> Failure {
    Failure { kind, detail: detail.into() }
}

/// Strip a leaked `<think>` block. `chat_once` already separates reasoning
/// when the server emits it as content deltas, but not every server does, and
/// an unstripped think block breaks every length and format check.
pub fn normalize_reply(reply: &str) -> String {
    let mut out = String::with_capacity(reply.len());
    let mut rest = reply;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        match rest[start..].find("</think>") {
            Some(end) => rest = &rest[start + end + "</think>".len()..],
            // Unterminated: everything after the tag is reasoning.
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Count tool calls in a reply. Only the first is ever executed, so a reply
/// with two is a protocol violation even when both parse.
fn count_calls(reply: &str, is_known: &dyn Fn(&str) -> bool) -> usize {
    let fences = reply.matches("```tool_call").count();
    if fences > 0 {
        return fences;
    }
    reply
        .lines()
        .filter(|line| parse_tool_call::extract_known(line, is_known).is_some())
        .count()
}

/// Does `haystack` contain `needle`, ignoring case? Content checks are
/// case-insensitive on purpose — casing varies run to run and a case that
/// really cares about it should use `regex`.
fn has(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// Everything except the judge: pure, synchronous, and therefore testable
/// without a server.
pub fn evaluate(case: &Case, raw_reply: &str, is_known: &dyn Fn(&str) -> bool) -> Vec<Failure> {
    let reply = normalize_reply(raw_reply);
    let mut out = Vec::new();
    if reply.is_empty() {
        out.push(fail(FailKind::Content, "empty reply"));
        return out;
    }

    let call = parse_tool_call::extract_known(&reply, is_known);
    let rejected = if call.is_none() {
        parse_tool_call::rejected_attempt(&reply, is_known)
    } else {
        None
    };

    if let Some(expected) = &case.expect_tool {
        let want = expected.as_slice();
        match &call {
            None => match &rejected {
                Some(reason) => out.push(fail(
                    FailKind::MalformedCall,
                    format!("expected `{}` — the reply contained {reason}", want.join("` or `")),
                )),
                None => out.push(fail(
                    FailKind::NoCall,
                    format!("expected a call to `{}`, got prose", want.join("` or `")),
                )),
            },
            Some(c) => {
                if !want.contains(&c.skill.as_str()) {
                    out.push(fail(
                        FailKind::WrongTool,
                        format!("called `{}`, expected `{}`", c.skill, want.join("` or `")),
                    ));
                } else {
                    // The JSON-fence form carries no expectation by
                    // convention, so the clause is only required of the
                    // natural-language shape the text protocol specifies.
                    let require = case.require_expectation.unwrap_or(true);
                    if require && c.args_json.is_none() && c.expectation.trim().is_empty() {
                        out.push(fail(
                            FailKind::MissingExpectation,
                            format!("`{}` call has no ` > <expectation>` clause", c.skill),
                        ));
                    }
                    if let Some(min) = case.min_args {
                        if c.raw_args.len() < min {
                            out.push(fail(
                                FailKind::BadArgs,
                                format!("{} argument(s), expected at least {min}", c.raw_args.len()),
                            ));
                        }
                    }
                    for needle in &case.expect_args_contain {
                        if !c.raw_args.iter().any(|a| has(a, needle)) {
                            out.push(fail(
                                FailKind::BadArgs,
                                format!("no argument contains `{needle}` (got {:?})", c.raw_args),
                            ));
                        }
                    }
                }
            }
        }
    }

    if case.expect_no_tool {
        if let Some(c) = &call {
            out.push(fail(
                FailKind::UnexpectedCall,
                format!("called `{}` for a question that needed no tool", c.skill),
            ));
        } else if let Some(reason) = &rejected {
            out.push(fail(
                FailKind::MalformedCall,
                format!("no tool was wanted, and the reply still contained {reason}"),
            ));
        }
    }

    if case.single_tool_call {
        let n = count_calls(&reply, is_known);
        if n > 1 {
            out.push(fail(
                FailKind::ExtraCalls,
                format!("{n} tool calls in one message — only the first would run"),
            ));
        }
    }

    for needle in &case.contains {
        if !has(&reply, needle) {
            out.push(fail(FailKind::Content, format!("missing `{needle}`")));
        }
    }
    if !case.contains_any.is_empty() && !case.contains_any.iter().any(|n| has(&reply, n)) {
        out.push(fail(
            FailKind::Content,
            format!("none of {:?} present", case.contains_any),
        ));
    }
    for needle in &case.not_contains {
        if has(&reply, needle) {
            out.push(fail(FailKind::Content, format!("contains forbidden `{needle}`")));
        }
    }

    if let Some(p) = &case.regex {
        match regex::Regex::new(p) {
            Ok(re) if !re.is_match(&reply) => {
                out.push(fail(FailKind::Format, format!("does not match /{p}/")))
            }
            Ok(_) => {}
            Err(e) => out.push(fail(FailKind::Format, format!("invalid regex /{p}/: {e}"))),
        }
    }
    if let Some(p) = &case.not_regex {
        if let Ok(re) = regex::Regex::new(p) {
            if re.is_match(&reply) {
                out.push(fail(FailKind::Format, format!("matches forbidden /{p}/")));
            }
        }
    }

    if case.json && extract_json(&reply).is_none() {
        out.push(fail(FailKind::Format, "reply is not valid JSON"));
    }

    let words = reply.split_whitespace().count();
    if let Some(max) = case.max_words {
        if words > max {
            out.push(fail(FailKind::Length, format!("{words} words, max {max}")));
        }
    }
    if let Some(min) = case.min_words {
        if words < min {
            out.push(fail(FailKind::Length, format!("{words} words, min {min}")));
        }
    }

    out
}

/// The reply as JSON — bare, or inside its single fenced block (models fence
/// JSON reflexively even when told not to; the fence is a *format* nit worth
/// tolerating here since `not_contains = ["```"]` can catch it explicitly).
fn extract_json(reply: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(reply.trim()) {
        return Some(v);
    }
    let start = reply.find("```")?;
    let after = &reply[start + 3..];
    let body_start = after.find('\n')? + 1;
    let body = &after[body_start..];
    let end = body.find("```")?;
    serde_json::from_str::<Value>(body[..end].trim()).ok()
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CaseResult {
    pub id: String,
    pub category: String,
    pub runs: u32,
    pub passes: u32,
    /// Deduped across repeats — the same failure three times is one finding.
    pub failures: Vec<Failure>,
    pub excerpt: Option<String>,
    pub mean_ms: u64,
}

impl CaseResult {
    fn passed(&self) -> bool {
        self.runs > 0 && self.passes == self.runs
    }
    fn flaky(&self) -> bool {
        self.passes > 0 && self.passes < self.runs
    }
}

pub struct RunResult {
    pub suite: String,
    pub suite_path: String,
    pub model: String,
    pub temperature: f32,
    pub system: String,
    pub started: String,
    pub calls: u32,
    pub total_ms: u128,
    pub cases: Vec<CaseResult>,
    pub truncated: Option<String>,
}

impl RunResult {
    fn passed(&self) -> usize {
        self.cases.iter().filter(|c| c.passed()).count()
    }
    fn pass_rate(&self) -> f64 {
        let runs: u32 = self.cases.iter().map(|c| c.runs).sum();
        let passes: u32 = self.cases.iter().map(|c| c.passes).sum();
        if runs == 0 { 0.0 } else { f64::from(passes) / f64::from(runs) }
    }
}

/// The machine-readable half of a report, written beside the Markdown so the
/// next run can diff against it. Without a baseline the operator has to eyeball
/// two reports to answer the only question that matters: did my edit help?
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    pub suite: String,
    pub started: String,
    pub model: String,
    pub temperature: f32,
    pub pass_rate: f64,
    pub cases: Vec<BaselineCase>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineCase {
    pub id: String,
    pub category: String,
    pub runs: u32,
    pub passes: u32,
}

impl Baseline {
    fn from_run(run: &RunResult) -> Self {
        Self {
            suite: run.suite.clone(),
            started: run.started.clone(),
            model: run.model.clone(),
            temperature: run.temperature,
            pass_rate: run.pass_rate(),
            cases: run
                .cases
                .iter()
                .map(|c| BaselineCase {
                    id: c.id.clone(),
                    category: c.category.clone(),
                    runs: c.runs,
                    passes: c.passes,
                })
                .collect(),
        }
    }

    fn rate_for(&self, id: &str) -> Option<f64> {
        self.cases
            .iter()
            .find(|c| c.id == id)
            .filter(|c| c.runs > 0)
            .map(|c| f64::from(c.passes) / f64::from(c.runs))
    }
}

/// Newest baseline JSON for `suite`, if any. Filenames are timestamp-suffixed
/// in `%Y%m%d-%H%M%S` form, which sorts lexicographically.
fn latest_baseline(dir: &Path, suite: &str) -> Option<Baseline> {
    let prefix = format!("{suite}-");
    let mut names: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "json")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix))
        })
        .collect();
    names.sort();
    let latest = names.pop()?;
    serde_json::from_str(&fs::read_to_string(latest).ok()?).ok()
}

// ---------------------------------------------------------------------------
// Report rendering
// ---------------------------------------------------------------------------

fn pct(passes: u32, runs: u32) -> String {
    if runs == 0 {
        return "—".into();
    }
    format!("{:.0}%", 100.0 * f64::from(passes) / f64::from(runs))
}

/// Levers for the buckets that actually fired, most frequent first.
fn levers(run: &RunResult) -> Vec<(FailKind, u32)> {
    let mut counts: BTreeMap<FailKind, u32> = BTreeMap::new();
    for case in run.cases.iter().filter(|c| !c.passed()) {
        for f in &case.failures {
            *counts.entry(f.kind).or_default() += 1;
        }
    }
    let mut v: Vec<(FailKind, u32)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

pub fn render_report(run: &RunResult, baseline: Option<&Baseline>) -> String {
    let mut out = String::new();
    out.push_str(&format!("# model-eval — {} ({})\n\n", run.suite, run.started));
    out.push_str(&format!(
        "- model: `{}`  |  temperature: {}  |  system prompt: {}\n\
         - suite: `{}`  |  {} case(s) × {} run(s) = {} LLM call(s) in {:.1}s\n\
         - **score: {}/{} cases fully passed, {:.0}% of individual runs**\n",
        run.model,
        run.temperature,
        run.system,
        run.suite_path,
        run.cases.len(),
        run.cases.first().map(|c| c.runs).unwrap_or(0),
        run.calls,
        run.total_ms as f64 / 1000.0,
        run.passed(),
        run.cases.len(),
        100.0 * run.pass_rate(),
    ));
    if let Some(b) = baseline {
        let delta = 100.0 * (run.pass_rate() - b.pass_rate);
        out.push_str(&format!(
            "- vs previous run ({}, model `{}`, temp {}): **{:+.0} pts** ({:.0}% → {:.0}%)\n",
            b.started,
            b.model,
            b.temperature,
            delta,
            100.0 * b.pass_rate,
            100.0 * run.pass_rate(),
        ));
    }
    if let Some(t) = &run.truncated {
        out.push_str(&format!("- ⚠ {t}\n"));
    }

    out.push_str("\n## Cases\n\n| case | category | pass | mean | vs prev | verdict |\n");
    out.push_str("| --- | --- | --- | --- | --- | --- |\n");
    for c in &run.cases {
        let prev = baseline
            .and_then(|b| b.rate_for(&c.id))
            .map(|p| {
                let now = if c.runs == 0 { 0.0 } else { f64::from(c.passes) / f64::from(c.runs) };
                format!("{:+.0} pts", 100.0 * (now - p))
            })
            .unwrap_or_else(|| "—".into());
        let verdict = if c.passed() {
            "pass"
        } else if c.flaky() {
            "FLAKY"
        } else {
            "FAIL"
        };
        out.push_str(&format!(
            "| `{}` | {} | {}/{} ({}) | {} ms | {} | {} |\n",
            c.id,
            c.category,
            c.passes,
            c.runs,
            pct(c.passes, c.runs),
            c.mean_ms,
            prev,
            verdict,
        ));
    }

    // Per-category roll-up: which *kind* of behaviour is weak, which is fine.
    let mut by_cat: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
    for c in &run.cases {
        let e = by_cat.entry(c.category.as_str()).or_default();
        e.0 += c.passes;
        e.1 += c.runs;
    }
    out.push_str("\n## By category\n\n| category | pass rate |\n| --- | --- |\n");
    for (cat, (p, r)) in &by_cat {
        out.push_str(&format!("| {cat} | {}/{} ({}) |\n", p, r, pct(*p, *r)));
    }

    let failing: Vec<&CaseResult> = run.cases.iter().filter(|c| !c.passed()).collect();
    if failing.is_empty() {
        out.push_str(
            "\n## Failures\n\nNone. Every case passed every repeat — the suite no longer \
             discriminates. Add harder cases (longer context, multi-step tool chains, \
             adversarial phrasing) or the next regression will be invisible.\n",
        );
    } else {
        out.push_str("\n## Failures\n");
        for c in failing {
            out.push_str(&format!(
                "\n### `{}` — {}/{} passed ({})\n\n",
                c.id,
                c.passes,
                c.runs,
                if c.flaky() { "flaky" } else { "failed every run" }
            ));
            for f in &c.failures {
                out.push_str(&format!("- **{}** — {}\n", f.kind.label(), f.detail));
            }
            if let Some(x) = &c.excerpt {
                out.push_str("\nFirst failing reply:\n\n```\n");
                out.push_str(x);
                out.push_str("\n```\n");
            }
        }
    }

    let levers = levers(run);
    if !levers.is_empty() {
        out.push_str("\n## What to change\n\nOrdered by how often the failure bucket fired.\n\n");
        for (kind, n) in &levers {
            out.push_str(&format!(
                "### {} ({} occurrence{})\n\n{}\n\n",
                kind.label(),
                n,
                if *n == 1 { "" } else { "s" },
                kind.lever()
            ));
        }
    }
    let flaky: Vec<&str> = run.cases.iter().filter(|c| c.flaky()).map(|c| c.id.as_str()).collect();
    if !flaky.is_empty() {
        out.push_str(&format!(
            "### flakiness ({} case(s): {})\n\nThese passed some repeats and failed others, so the \
             prompt is not the whole story — the sampler is. Set `temperature = 0.0` in the \
             suite (or on the provider) and re-run: if they go green, the fix is sampling \
             settings, not wording. If they stay mixed, the prompt is ambiguous enough that two \
             readings are both plausible — rewrite it to admit only one.\n\n",
            flaky.len(),
            flaky.join(", "),
        ));
    }

    out.push_str(
        "\n---\n\nRe-run after each prompt edit: `model-eval` diffs this suite against the \
         previous report automatically. Edit the suite at the path above to test what your own \
         work depends on.\n",
    );
    out
}

/// The compact half that goes back into the conversation. Kept under
/// [`SUMMARY_CAP`] so the sub-agent summarizer never rewrites (and thereby
/// paraphrases) the numbers.
pub fn render_summary(run: &RunResult, baseline: Option<&Baseline>, report_path: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "**{}**: {}/{} cases passed ({:.0}% of {} runs) — model `{}`, temp {}.\n",
        run.suite,
        run.passed(),
        run.cases.len(),
        100.0 * run.pass_rate(),
        run.calls,
        run.model,
        run.temperature,
    ));
    if let Some(b) = baseline {
        out.push_str(&format!(
            "Previous run {}: {:.0}% → {:.0}% ({:+.0} pts).\n",
            b.started,
            100.0 * b.pass_rate,
            100.0 * run.pass_rate(),
            100.0 * (run.pass_rate() - b.pass_rate),
        ));
    }
    if let Some(t) = &run.truncated {
        out.push_str(&format!("⚠ {t}\n"));
    }

    let failing: Vec<&CaseResult> = run.cases.iter().filter(|c| !c.passed()).collect();
    if failing.is_empty() {
        out.push_str("\nEvery case passed every repeat — the suite is no longer discriminating; add harder cases.\n");
    } else {
        out.push_str("\nFailing:\n");
        for c in &failing {
            let first = c
                .failures
                .first()
                .map(|f| format!("{} — {}", f.kind.label(), f.detail))
                .unwrap_or_else(|| "unknown".into());
            out.push_str(&format!(
                "- `{}` ({}) {}/{}: {}\n",
                c.id, c.category, c.passes, c.runs, first
            ));
        }
        out.push_str("\nTop fixes:\n");
        for (kind, n) in levers(run).into_iter().take(3) {
            // One sentence per lever here; the report carries the full text.
            let first_sentence = kind
                .lever()
                .split_once(". ")
                .map(|(a, _)| format!("{a}."))
                .unwrap_or_else(|| kind.lever().to_string());
            out.push_str(&format!("- {} (×{n}): {}\n", kind.label(), first_sentence));
        }
    }
    out.push_str(&format!("\nFull report: {report_path}\n"));
    truncate_chars(&out, SUMMARY_CAP)
}

fn truncate_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let mut out: String = s.chars().take(cap).collect();
    out.push_str("…\n(truncated — see the report file)");
    out
}

// ---------------------------------------------------------------------------
// The skill
// ---------------------------------------------------------------------------

pub struct ModelEval {
    /// Live catalogue, for the `live` system prompt and the known-skill
    /// predicate the tool-call checks need. `Weak` because the registry owns
    /// this skill (same reason as `AgentTeam`).
    registry: OnceLock<Weak<SkillRegistry>>,
    root: PathBuf,
    evals_dir: PathBuf,
    reports_dir: PathBuf,
}

impl ModelEval {
    pub fn new(root: PathBuf) -> Self {
        Self {
            registry: OnceLock::new(),
            root,
            evals_dir: sica_core::paths::evals_dir(),
            reports_dir: sica_core::paths::eval_reports_dir(),
        }
    }

    /// Point the skill at a different suite/report pair. Used by the tests so
    /// a run never writes into the live workspace.
    pub fn with_dirs(mut self, evals_dir: PathBuf, reports_dir: PathBuf) -> Self {
        self.evals_dir = evals_dir;
        self.reports_dir = reports_dir;
        self
    }

    pub fn attach_registry(&self, registry: &Arc<SkillRegistry>) {
        let _ = self.registry.set(Arc::downgrade(registry));
    }

    fn registry(&self) -> Option<Arc<SkillRegistry>> {
        self.registry.get().and_then(Weak::upgrade)
    }
}

#[async_trait]
impl Skill for ModelEval {
    fn name(&self) -> &str { MODEL_EVAL_NAME }
    fn description(&self) -> &str { MODEL_EVAL_DESCRIPTION }
    fn positional_args(&self) -> Vec<String> {
        vec!["suite".into(), "repeats".into(), "filter".into()]
    }
    /// A run is capped at 150 LLM calls; on a slow local model that is an hour.
    fn timeout(&self) -> std::time::Duration { std::time::Duration::from_secs(3600) }

    async fn run(&self, args: Value, ctx: SkillContext) -> SkillOutcome {
        let Some(client) = ctx.sub.summarizer.clone() else {
            return err(
                "model-eval needs a connected LLM — connect one and retry".into(),
            );
        };
        let suite_arg = str_arg(&args, "suite");
        let filter = str_arg(&args, "filter").to_lowercase();
        let repeats_arg = str_arg(&args, "repeats").parse::<u32>().ok();

        let path = resolve_suite_path(&self.root, &self.evals_dir, suite_arg);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                return err(format!(
                    "cannot read suite `{}`: {e}. Suites live in {} — one `.toml` per suite; \
                     the default one is seeded at backend start.",
                    path.display(),
                    self.evals_dir.display()
                ))
            }
        };
        let fallback = path.file_stem().and_then(|s| s.to_str()).unwrap_or("suite");
        let mut suite = match parse_suite(&text, fallback) {
            Ok(s) => s,
            Err(e) => return err(format!("{}: {e}", path.display())),
        };
        if let Some(r) = repeats_arg {
            suite.repeats = r.clamp(1, MAX_REPEATS);
        }
        if !filter.is_empty() {
            suite.cases.retain(|c| {
                c.id.to_lowercase().contains(&filter) || c.category.to_lowercase().contains(&filter)
            });
            if suite.cases.is_empty() {
                return err(format!("no case in `{}` matches filter `{filter}`", suite.name));
            }
        }

        // A suite-level temperature makes "is this a wording problem or a
        // sampling problem" a one-line experiment.
        let mut client = client;
        if let Some(t) = suite.temperature {
            client.temperature = t;
        }

        let registry = self.registry();
        let is_known = |n: &str| match &registry {
            Some(r) => r.by_name.contains_key(n),
            // No registry attached (unit tests): accept any well-formed name
            // rather than silently failing every tool-call case.
            None => !n.is_empty(),
        };
        let system = match &suite.system {
            SystemMode::None => None,
            SystemMode::Custom(s) => Some(s.clone()),
            SystemMode::Live => Some(live_system_prompt(registry.as_deref())),
        };
        let cancel = ctx.sub.cancel.clone();

        let planned = suite.cases.len() as u32 * suite.repeats;
        let mut truncated = None;
        if planned > MAX_CALLS {
            truncated = Some(format!(
                "call budget: {planned} planned run(s) exceeds the {MAX_CALLS} cap — the run \
                 stopped early; narrow it with the filter argument or fewer repeats"
            ));
        }
        info!(
            suite = %suite.name,
            cases = suite.cases.len(),
            repeats = suite.repeats,
            model = %client.model,
            "model-eval: starting"
        );
        ctx.sub.events.emit(Event::LogLine {
            level: "INFO".into(),
            message: format!(
                "model-eval: `{}` — {} case(s) × {} repeat(s) against `{}` (temp {})",
                suite.name,
                suite.cases.len(),
                suite.repeats,
                client.model,
                client.temperature,
            ),
        });

        let started_at = Instant::now();
        let started = chrono::Local::now();
        let mut calls: u32 = 0;
        let mut results: Vec<CaseResult> = Vec::with_capacity(suite.cases.len());

        'cases: for case in &suite.cases {
            let mut passes = 0;
            let mut runs = 0;
            let mut failures: Vec<Failure> = Vec::new();
            let mut excerpt: Option<String> = None;
            let mut elapsed_ms: u64 = 0;

            for _ in 0..suite.repeats {
                if is_cancelled(&cancel) || calls >= MAX_CALLS {
                    break 'cases;
                }
                let mut messages = Vec::new();
                if let Some(s) = &system {
                    messages.push(ChatMessage::text("system", s.clone()));
                }
                messages.push(ChatMessage::text("user", case.prompt.clone()));

                let t0 = Instant::now();
                let reply = llm_call(&client, &cancel, messages).await;
                elapsed_ms += t0.elapsed().as_millis() as u64;
                calls += 1;
                runs += 1;

                let Some(reply) = reply else {
                    push_unique(
                        &mut failures,
                        fail(FailKind::Content, "the LLM call failed or returned nothing"),
                    );
                    continue;
                };

                let mut case_failures = evaluate(case, &reply, &is_known);
                if case_failures.is_empty() {
                    if let Some(rubric) = &case.judge {
                        if calls < MAX_CALLS {
                            calls += 1;
                            if let Some(reason) =
                                judge(&client, &cancel, &case.prompt, rubric, &reply).await
                            {
                                case_failures.push(fail(FailKind::Judge, reason));
                            }
                        }
                    }
                }

                if case_failures.is_empty() {
                    passes += 1;
                } else {
                    if excerpt.is_none() {
                        excerpt = Some(truncate_chars(normalize_reply(&reply).trim(), EXCERPT_CHARS));
                    }
                    for f in case_failures {
                        push_unique(&mut failures, f);
                    }
                }
            }

            ctx.sub.events.emit(Event::LogLine {
                level: if passes == runs { "INFO".into() } else { "WARN".into() },
                message: format!(
                    "model-eval: {} [{}] {}/{}{}",
                    case.id,
                    case.category,
                    passes,
                    runs,
                    failures
                        .first()
                        .map(|f| format!(" — {}: {}", f.kind.label(), f.detail))
                        .unwrap_or_default(),
                ),
            });

            results.push(CaseResult {
                id: case.id.clone(),
                category: case.category.clone(),
                runs,
                passes,
                failures,
                excerpt,
                mean_ms: if runs == 0 { 0 } else { elapsed_ms / u64::from(runs) },
            });
        }

        if results.is_empty() {
            return err("model-eval: no case completed (interrupted before the first reply)".into());
        }
        if is_cancelled(&cancel) && results.len() < suite.cases.len() {
            truncated = Some(format!(
                "interrupted after {} of {} case(s) — the score below covers only those",
                results.len(),
                suite.cases.len()
            ));
        } else if results.len() < suite.cases.len() && truncated.is_none() {
            truncated = Some(format!(
                "stopped after {} of {} case(s) at the {MAX_CALLS}-call budget",
                results.len(),
                suite.cases.len()
            ));
        }

        let run = RunResult {
            suite: suite.name.clone(),
            suite_path: path.display().to_string(),
            model: client.model.clone(),
            temperature: client.temperature,
            system: suite.system.label().to_string(),
            started: started.format("%Y-%m-%d %H:%M:%S").to_string(),
            calls,
            total_ms: started_at.elapsed().as_millis(),
            cases: results,
            truncated,
        };

        // Read the previous baseline *before* writing this run's, or the run
        // would diff against itself.
        let reports_dir = self.reports_dir.clone();
        let baseline = latest_baseline(&reports_dir, &run.suite);
        let stamp = started.format("%Y%m%d-%H%M%S").to_string();
        let md_path = reports_dir.join(format!("{}-{stamp}.md", run.suite));
        let json_path = reports_dir.join(format!("{}-{stamp}.json", run.suite));
        let report = render_report(&run, baseline.as_ref());

        let mut write_note = String::new();
        if let Err(e) = fs::create_dir_all(&reports_dir) {
            write_note = format!("\n⚠ could not create {}: {e}", reports_dir.display());
        } else {
            if let Err(e) = fs::write(&md_path, &report) {
                write_note = format!("\n⚠ could not write {}: {e}", md_path.display());
            }
            if let Ok(json) = serde_json::to_string_pretty(&Baseline::from_run(&run)) {
                if let Err(e) = fs::write(&json_path, json) {
                    warn!(error = %e, "model-eval: baseline write failed");
                }
            }
        }

        info!(
            suite = %run.suite,
            passed = run.passed(),
            cases = run.cases.len(),
            calls = run.calls,
            "model-eval: finished"
        );
        ctx.sub.events.emit(Event::LogLine {
            level: "INFO".into(),
            message: format!(
                "model-eval: {}/{} case(s) passed ({:.0}% of runs) — report at {}",
                run.passed(),
                run.cases.len(),
                100.0 * run.pass_rate(),
                md_path.display(),
            ),
        });

        let mut summary = render_summary(&run, baseline.as_ref(), &md_path.display().to_string());
        summary.push_str(&write_note);
        SkillOutcome { ok: true, summary }
    }
}

/// Reproduce `chat.rs::build_history`'s text-protocol system message so the
/// suite measures the shipped prompt. Delegates to the same
/// `agents::prompt` composition the main agent uses — if the two ever
/// diverge the eval is measuring a fiction, so they share the builder.
fn live_system_prompt(registry: Option<&SkillRegistry>) -> String {
    let mem = crate::memory::load(&sica_core::paths::memory_file()).unwrap_or_default();
    let empty = SkillRegistry::new();
    let reg = registry.unwrap_or(&empty);
    let vars = crate::prompt::standard_vars("");
    match crate::prompt::for_main_agent(&mem, reg, false, &vars) {
        Ok(r) => r.system,
        Err(e) => format!("[prompt assembly failed: {e}]"),
    }
}

/// `Some(reason)` when the judge rejects the reply. Inconclusive verdicts
/// count as a pass: the judge is the same model under test, and letting it
/// fail a case on its own confusion would report a defect that isn't there.
async fn judge(
    client: &LlmClient,
    cancel: &Option<CancellationToken>,
    prompt: &str,
    rubric: &str,
    reply: &str,
) -> Option<String> {
    let system = "You grade one model reply against one rubric. Answer with exactly \
                  `PASS` or `FAIL: <reason in at most 20 words>`. Grade only against the \
                  rubric — not style, not helpfulness. When the rubric does not clearly \
                  settle it, answer PASS.";
    let user = format!(
        "Rubric:\n{rubric}\n\nThe prompt the model was given:\n{prompt}\n\nThe model's reply:\n{reply}"
    );
    let verdict = llm_call(
        client,
        cancel,
        vec![
            ChatMessage::text("system", system),
            ChatMessage::text("user", user),
        ],
    )
    .await?;
    let v = normalize_reply(&verdict);
    let head = v.trim_start().to_uppercase();
    if head.starts_with("PASS") {
        return None;
    }
    if head.starts_with("FAIL") {
        let reason = v
            .trim_start()
            .trim_start_matches(|c: char| c.is_alphabetic())
            .trim_start_matches([':', '-', ' '])
            .trim();
        return Some(if reason.is_empty() {
            "judge: FAIL (no reason given)".to_string()
        } else {
            format!("judge: {}", truncate_chars(reason, 200))
        });
    }
    None
}

async fn llm_call(
    client: &LlmClient,
    cancel: &Option<CancellationToken>,
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
            warn!(error = %e, "model-eval: LLM call failed");
            None
        }
    }
}

fn is_cancelled(cancel: &Option<CancellationToken>) -> bool {
    cancel.as_ref().is_some_and(|t| t.is_cancelled())
}

/// Same failure seen on two repeats is one finding, not two.
fn push_unique(list: &mut Vec<Failure>, f: Failure) {
    if !list.contains(&f) {
        list.push(f);
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or("").trim()
}

fn err(msg: String) -> SkillOutcome {
    SkillOutcome { ok: false, summary: msg }
}

/// Write `skills/model-eval.md` and `evals/default.toml` if absent. Like every
/// other seeded surface, an existing file is never overwritten — the suite is
/// the user's once it is on disk.
pub fn seed_defaults(skills_dir: &Path, evals_dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(skills_dir)?;
    let doc = skills_dir.join(format!("{MODEL_EVAL_NAME}.md"));
    if !doc.exists() {
        fs::write(&doc, MODEL_EVAL_SEED_MD)?;
    }
    fs::create_dir_all(evals_dir)?;
    let suite = evals_dir.join("default.toml");
    if !suite.exists() {
        fs::write(&suite, DEFAULT_SUITE_TOML)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(n: &str) -> bool {
        matches!(n, "read-file" | "run-cli" | "run-pwsh" | "write-file")
    }

    fn case(id: &str) -> Case {
        Case {
            id: id.into(),
            category: "test".into(),
            prompt: "p".into(),
            expect_tool: None,
            expect_args_contain: Vec::new(),
            min_args: None,
            require_expectation: None,
            expect_no_tool: false,
            single_tool_call: false,
            contains: Vec::new(),
            contains_any: Vec::new(),
            not_contains: Vec::new(),
            regex: None,
            not_regex: None,
            json: false,
            min_words: None,
            max_words: None,
            judge: None,
        }
    }

    fn kinds(f: &[Failure]) -> Vec<FailKind> {
        f.iter().map(|x| x.kind).collect()
    }

    #[test]
    fn default_suite_parses_and_is_within_caps() {
        let s = parse_suite(DEFAULT_SUITE_TOML, "default").unwrap();
        assert_eq!(s.name, "default");
        assert_eq!(s.system, SystemMode::Live);
        assert!(s.repeats >= 1 && s.repeats <= MAX_REPEATS);
        assert!(!s.cases.is_empty() && s.cases.len() <= MAX_CASES);
        // The seeded suite must never expect a skill that isn't registered,
        // or every run reports a failure the operator cannot act on.
        for c in &s.cases {
            if let Some(t) = &c.expect_tool {
                for name in t.as_slice() {
                    assert!(known(name), "case `{}` expects unknown skill `{name}`", c.id);
                }
            }
        }
    }

    #[test]
    fn suite_errors_are_caught_before_any_llm_call() {
        assert!(parse_suite("", "x").unwrap_err().contains("no `[[case]]`"));
        let dup = r#"
            [[case]]
            id = "a"
            prompt = "p"
            [[case]]
            id = "a"
            prompt = "q"
        "#;
        assert!(parse_suite(dup, "x").unwrap_err().contains("duplicate"));
        let bad_re = r#"
            [[case]]
            id = "a"
            prompt = "p"
            regex = "([unclosed"
        "#;
        assert!(parse_suite(bad_re, "x").unwrap_err().contains("invalid regex"));
    }

    #[test]
    fn expect_tool_accepts_string_or_list() {
        let s = parse_suite(
            r#"
            [[case]]
            id = "a"
            prompt = "p"
            expect_tool = "read-file"
            [[case]]
            id = "b"
            prompt = "p"
            expect_tool = ["run-cli", "run-pwsh"]
        "#,
            "x",
        )
        .unwrap();
        assert_eq!(s.cases[0].expect_tool.as_ref().unwrap().as_slice(), vec!["read-file"]);
        assert_eq!(
            s.cases[1].expect_tool.as_ref().unwrap().as_slice(),
            vec!["run-cli", "run-pwsh"]
        );
    }

    #[test]
    fn tool_case_passes_on_a_well_formed_call() {
        let mut c = case("t");
        c.expect_tool = Some(StringOrList::One("read-file".into()));
        c.expect_args_contain = vec!["run-cli.md".into()];
        let f = evaluate(&c, "read-file 'skills/run-cli.md' > which args", &known);
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn prose_where_a_call_was_required_is_a_missing_call() {
        let mut c = case("t");
        c.expect_tool = Some(StringOrList::One("read-file".into()));
        let f = evaluate(&c, "The file declares a single `path` argument.", &known);
        assert_eq!(kinds(&f), vec![FailKind::NoCall]);
    }

    #[test]
    fn a_botched_call_is_distinguished_from_no_call_at_all() {
        // This is the distinction the whole report hangs on: "didn't reach for
        // the tool" and "reached and fumbled the syntax" need opposite fixes.
        let mut c = case("t");
        c.expect_tool = Some(StringOrList::One("read-file".into()));
        let f = evaluate(&c, "read-file 'README.md'", &known);
        assert_eq!(kinds(&f), vec![FailKind::MalformedCall]);
        assert!(f[0].detail.contains("expectation"), "{:?}", f[0]);
    }

    #[test]
    fn wrong_skill_and_wrong_args_are_reported_separately() {
        let mut c = case("t");
        c.expect_tool = Some(StringOrList::One("run-pwsh".into()));
        let f = evaluate(&c, "run-cli 'cargo --version' > version", &known);
        assert_eq!(kinds(&f), vec![FailKind::WrongTool]);

        let mut c = case("t");
        c.expect_tool = Some(StringOrList::One("write-file".into()));
        c.min_args = Some(2);
        c.expect_args_contain = vec!["hello".into()];
        let f = evaluate(&c, "write-file 'notes/x.md' > confirm", &known);
        assert_eq!(kinds(&f), vec![FailKind::BadArgs, FailKind::BadArgs]);
    }

    #[test]
    fn expectation_clause_is_required_by_default_but_waivable() {
        let mut c = case("t");
        c.expect_tool = Some(StringOrList::One("read-file".into()));
        // A JSON-fence call carries no expectation by convention — not a defect.
        let fence = "```tool_call\n{\"skill\":\"read-file\",\"args\":{\"path\":\"a.md\"}}\n```";
        assert!(evaluate(&c, fence, &known).is_empty());
        c.require_expectation = Some(false);
        assert!(evaluate(&c, "read-file 'a.md' > x", &known).is_empty());
    }

    #[test]
    fn over_calling_is_caught() {
        let mut c = case("t");
        c.expect_no_tool = true;
        c.contains = vec!["51".into()];
        let f = evaluate(&c, "run-cli 'echo 17*3' > the product", &known);
        assert_eq!(kinds(&f), vec![FailKind::UnexpectedCall, FailKind::Content]);
        assert!(evaluate(&c, "51", &known).is_empty());
    }

    #[test]
    fn two_calls_in_one_message_are_caught() {
        let mut c = case("t");
        c.single_tool_call = true;
        let two = "read-file 'a.md' > a\nread-file 'b.md' > b";
        assert_eq!(kinds(&evaluate(&c, two, &known)), vec![FailKind::ExtraCalls]);
        assert!(evaluate(&c, "read-file 'a.md' > a", &known).is_empty());
    }

    #[test]
    fn content_checks_are_case_insensitive_and_regex_is_not() {
        let mut c = case("t");
        c.contains = vec!["OK".into()];
        c.not_contains = vec!["sure".into()];
        assert!(evaluate(&c, "ok", &known).is_empty());
        assert_eq!(kinds(&evaluate(&c, "Sure, ok", &known)), vec![FailKind::Content]);

        let mut c = case("t");
        c.regex = Some("^OK$".into());
        assert_eq!(kinds(&evaluate(&c, "ok", &known)), vec![FailKind::Format]);
        assert!(evaluate(&c, "OK", &known).is_empty());
    }

    #[test]
    fn json_check_accepts_bare_and_fenced_objects() {
        let mut c = case("t");
        c.json = true;
        assert!(evaluate(&c, r#"{"name":"sica","purpose":"chat"}"#, &known).is_empty());
        assert!(evaluate(&c, "```json\n{\"a\":1}\n```", &known).is_empty());
        assert_eq!(kinds(&evaluate(&c, "name: sica", &known)), vec![FailKind::Format]);
    }

    #[test]
    fn length_contract_is_enforced_both_ways() {
        let mut c = case("t");
        c.max_words = Some(2);
        c.min_words = Some(1);
        assert!(evaluate(&c, "OK", &known).is_empty());
        assert_eq!(
            kinds(&evaluate(&c, "Sure! Here is the answer: OK", &known)),
            vec![FailKind::Length]
        );
    }

    #[test]
    fn think_blocks_are_stripped_before_scoring() {
        // A leaked reasoning block would fail every length and format check
        // and send the operator hunting a prompt bug that isn't there.
        let mut c = case("t");
        c.max_words = Some(2);
        c.regex = Some("(?i)^\\W*ok\\W*$".into());
        let reply = "<think>The user wants one word, I should be careful here.</think>\nOK";
        assert!(evaluate(&c, reply, &known).is_empty());
        assert_eq!(normalize_reply("<think>unterminated and endless"), "");
    }

    #[test]
    fn empty_reply_fails_loudly() {
        let c = case("t");
        assert_eq!(kinds(&evaluate(&c, "   ", &known)), vec![FailKind::Content]);
    }

    fn result(id: &str, passes: u32, runs: u32, failures: Vec<Failure>) -> CaseResult {
        CaseResult {
            id: id.into(),
            category: "cat".into(),
            runs,
            passes,
            failures,
            excerpt: Some("reply".into()),
            mean_ms: 10,
        }
    }

    fn run_with(cases: Vec<CaseResult>) -> RunResult {
        RunResult {
            suite: "default".into(),
            suite_path: "evals/default.toml".into(),
            model: "m".into(),
            temperature: 0.2,
            system: "live".into(),
            started: "2026-07-31 10:00:00".into(),
            calls: 4,
            total_ms: 1000,
            cases,
            truncated: None,
        }
    }

    #[test]
    fn report_scores_flags_flakiness_and_names_the_lever() {
        let run = run_with(vec![
            result("ok", 2, 2, vec![]),
            result("flaky", 1, 2, vec![fail(FailKind::MalformedCall, "no ` > ` clause")]),
        ]);
        assert_eq!(run.passed(), 1);
        assert!((run.pass_rate() - 0.75).abs() < 1e-9);
        let md = render_report(&run, None);
        assert!(md.contains("FLAKY"));
        assert!(md.contains("malformed tool call"));
        assert!(md.contains("temperature"), "lever text missing from report");
        assert!(md.contains("| cat | 3/4"), "category roll-up missing: {md}");
    }

    #[test]
    fn report_diffs_against_the_previous_baseline() {
        let run = run_with(vec![result("a", 2, 2, vec![])]);
        let baseline = Baseline {
            suite: "default".into(),
            started: "2026-07-30 09:00:00".into(),
            model: "m".into(),
            temperature: 0.2,
            pass_rate: 0.5,
            cases: vec![BaselineCase {
                id: "a".into(),
                category: "cat".into(),
                runs: 2,
                passes: 1,
            }],
        };
        let md = render_report(&run, Some(&baseline));
        assert!(md.contains("+50 pts"), "{md}");
        let summary = render_summary(&run, Some(&baseline), "evals/reports/x.md");
        assert!(summary.contains("50% → 100%"), "{summary}");
    }

    #[test]
    fn summary_stays_small_enough_to_skip_the_summarizer() {
        // Every case failing with a long detail is the worst case for size.
        let cases: Vec<CaseResult> = (0..MAX_CASES)
            .map(|i| {
                result(
                    &format!("case-{i}"),
                    0,
                    2,
                    vec![fail(FailKind::NoCall, "expected a call to `read-file`, got prose")],
                )
            })
            .collect();
        let summary = render_summary(&run_with(cases), None, "evals/reports/x.md");
        assert!(summary.chars().count() <= SUMMARY_CAP + 64, "{}", summary.len());
    }

    #[test]
    fn all_green_report_says_the_suite_stopped_discriminating() {
        let md = render_report(&run_with(vec![result("a", 2, 2, vec![])]), None);
        assert!(md.contains("no longer discriminates"), "{md}");
    }

    struct NullSink;
    impl crate::agent::EventSink for NullSink {
        fn emit(&self, _ev: Event) {}
    }

    fn ctx_with(client: Option<LlmClient>) -> SkillContext {
        let mut sub = crate::subagent::ToolSubAgent::root(Arc::new(NullSink));
        sub.summarizer = client;
        SkillContext { sub }
    }

    #[tokio::test]
    async fn run_without_llm_fails_cleanly() {
        let out = ModelEval::new(PathBuf::from("."))
            .run(serde_json::json!({ "suite": "default" }), ctx_with(None))
            .await;
        assert!(!out.ok);
        assert!(out.summary.contains("LLM"), "{}", out.summary);
    }

    #[tokio::test]
    async fn missing_suite_names_the_directory_to_look_in() {
        let dir = tempdir();
        let out = ModelEval::new(dir.clone())
            .with_dirs(dir.join("evals"), dir.join("reports"))
            .run(serde_json::json!({ "suite": "nope" }), ctx_with(Some(stub_client(0))))
            .await;
        assert!(!out.ok);
        assert!(out.summary.contains("nope.toml"), "{}", out.summary);
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "sica-model-eval-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn stub_client(port: u16) -> LlmClient {
        LlmClient::new(format!("http://127.0.0.1:{port}"), "stub-model", None)
    }

    /// Minimal OpenAI-compatible SSE endpoint: every POST gets `reply` back as
    /// one content delta. Enough to drive the real `LlmClient` — the point of
    /// the test is the eval loop, not the transport.
    async fn stub_server(reply: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    // Drain the request; the client always sends a body, and
                    // closing before reading it would surface as a transport
                    // error instead of a reply.
                    let mut buf = vec![0u8; 64 * 1024];
                    let _ = sock.read(&mut buf).await;
                    let payload = serde_json::json!({
                        "choices": [{ "delta": { "content": reply } }]
                    });
                    let body = format!("data: {payload}\n\ndata: [DONE]\n\n");
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
        port
    }

    /// The whole loop against a real `LlmClient`: suite on disk → LLM calls →
    /// scoring → report + baseline written → summary returned. The stub always
    /// replies with a well-formed `read-file` call, so the tool-syntax case
    /// passes every repeat and the restraint case fails every repeat — one run
    /// exercises both sides of the report.
    #[tokio::test]
    async fn runs_a_suite_end_to_end_and_writes_a_report() {
        let port = stub_server("read-file 'a.md' > what does it say").await;
        let dir = tempdir();
        let evals = dir.join("evals");
        let reports = dir.join("reports");
        fs::create_dir_all(&evals).unwrap();
        fs::write(
            evals.join("mini.toml"),
            r#"
[suite]
name    = "mini"
system  = "none"
repeats = 2

[[case]]
id       = "calls-the-tool"
category = "tool-syntax"
prompt   = "read a.md"
expect_tool = "read-file"

[[case]]
id       = "answers-directly"
category = "restraint"
prompt   = "what is 2+2"
expect_no_tool = true
"#,
        )
        .unwrap();

        let out = ModelEval::new(dir.clone())
            .with_dirs(evals, reports.clone())
            .run(
                serde_json::json!({ "suite": "mini" }),
                ctx_with(Some(stub_client(port))),
            )
            .await;

        assert!(out.ok, "{}", out.summary);
        assert!(out.summary.contains("1/2 cases passed"), "{}", out.summary);
        assert!(out.summary.contains("answers-directly"), "{}", out.summary);
        assert!(out.summary.contains("unnecessary tool call"), "{}", out.summary);

        let written: Vec<PathBuf> = fs::read_dir(&reports).unwrap().flatten().map(|e| e.path()).collect();
        assert_eq!(written.len(), 2, "expected a .md report and a .json baseline");
        let md = written.iter().find(|p| p.extension().unwrap() == "md").unwrap();
        let report = fs::read_to_string(md).unwrap();
        assert!(report.contains("`calls-the-tool` | tool-syntax | 2/2"), "{report}");
        assert!(report.contains("What to change"), "{report}");

        // The baseline must round-trip, or the next run silently loses its diff.
        let json = written.iter().find(|p| p.extension().unwrap() == "json").unwrap();
        let baseline: Baseline = serde_json::from_str(&fs::read_to_string(json).unwrap()).unwrap();
        assert_eq!(baseline.suite, "mini");
        assert_eq!(baseline.cases.len(), 2);
        assert!((baseline.pass_rate - 0.5).abs() < 1e-9);
    }

    #[tokio::test]
    async fn filter_and_repeats_arguments_narrow_the_run() {
        let port = stub_server("4").await;
        let dir = tempdir();
        let evals = dir.join("evals");
        fs::create_dir_all(&evals).unwrap();
        fs::write(
            evals.join("mini.toml"),
            r#"
[suite]
name    = "mini"
system  = "none"
repeats = 4

[[case]]
id       = "arith"
category = "restraint"
prompt   = "what is 2+2"
expect_no_tool = true
contains = ["4"]

[[case]]
id       = "other"
category = "format"
prompt   = "say something"
"#,
        )
        .unwrap();

        let out = ModelEval::new(dir.clone())
            .with_dirs(evals, dir.join("reports"))
            .run(
                serde_json::json!({ "suite": "mini", "repeats": "1", "filter": "restraint" }),
                ctx_with(Some(stub_client(port))),
            )
            .await;
        assert!(out.ok, "{}", out.summary);
        // One case, one repeat: exactly one LLM call.
        assert!(out.summary.contains("1/1 cases passed (100% of 1 runs)"), "{}", out.summary);
    }
}
