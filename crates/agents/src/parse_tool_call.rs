//! Parse a natural-language tool call from an assistant message.
//!
//! The on-wire contract (see `agents::memory::SEED`) is a single line:
//!
//! ```text
//!     <skill-name> <positional-arg-1> [<positional-arg-2> ...] > <expectation>
//! ```
//!
//! where every positional argument is single- or double-quoted and the text
//! after the lone `>` is what the main agent wants the sub-agent to focus its
//! summary on. The same line can optionally appear inside a ```tool fenced
//! block for robustness when the model wraps tool output in fences.
//!
//! Examples (all valid):
//!
//! ```text
//!     read-file 'skills/run-cli.md' > what args does run-cli accept
//!     run-cli "cargo --version" > confirm cargo is installed and report the version
//!     ```tool
//!     write-file 'notes/x.md' 'hello\nworld' > confirm bytes written
//!     ```
//! ```
//!
//! The parser is intentionally permissive: it scans every line of the input
//! and returns the first one that looks like a tool call. The skill name is
//! the first whitespace-separated token and must look like `[a-z][a-z0-9-]*`
//! — known-skill validation happens later, in the dispatcher.
//!
//! A quoted argument may also run past its line break, for the models that
//! write a `write-file` body with real newlines rather than the `\n` escape
//! above. See [`parse_multiline`] for the shape and the guard that keeps it
//! from swallowing prose.
//!
//! Hermes-style chat templates (Qwen 3.x among them) train the model to wrap
//! a call in `<tool_call>` … `</tool_call>` with a `<function=name>` opener.
//! Under the text protocol the model then mixes the two: the wrapper from
//! its training data around this contract's line. [`unwrap_xml_call`]
//! strips the wrapper before the scan, and a `{"name": …, "arguments": …}`
//! body inside the envelope is read like the JSON fence (`sessions/84`).
//!
//! The live loops call [`extract_for`], which also knows each skill's
//! arity and so can take a one-argument skill's argument verbatim, read
//! Qwen's `<parameter=…>` form, and accept a wrapped or trailing call that
//! left out its expectation. [`extract_known`] stays strict: `model-eval`
//! grades the model on this contract and must not be told a sloppy call was
//! fine.

// `Eq` is not derived because `serde_json::Value` only implements `PartialEq`
// (`f64` inside `Value::Number` rules out total equality).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolCall {
    pub skill:       String,
    pub raw_args:    Vec<String>,
    pub expectation: String,
    /// When the call was parsed from a ```tool_call``` JSON fence, the raw
    /// `args` object is preserved here so the registry can dispatch it
    /// directly without round-tripping through positional inference. `None`
    /// for the natural-language form, which still relies on the skill's
    /// declared `positional_args()` to map values to names.
    pub args_json:   Option<serde_json::Value>,
    /// Arguments the model named itself, in the order it wrote them — the
    /// `<parameter=key>` form Qwen's chat template trains. The registry
    /// binds a name the skill declares to that argument and hands the rest
    /// to the positionals still missing, in order, so `name` / `file_path`
    /// reach `path`. Empty for every other shape.
    pub named:       Vec<(String, String)>,
}

/// Return the first plausible tool call in `text`, if any.
///
/// Two shapes are recognised:
/// 1. **Natural-language** (preferred): a single line `skill 'arg' > expectation`.
/// 2. **JSON fence** (tolerated): a ```tool_call``` block containing
///    `{ "skill": "...", "args": { ... }, "expectation": "..." }`. Small local
///    models frequently emit this shape because it matches the OpenAI tool-call
///    convention in their training data — the parser accepts it rather than
///    silently dropping the call (the bug seen in `sessions/10.toml`).
pub fn extract(text: &str) -> Option<ToolCall> {
    extract_known(text, |_| true)
}

/// Like [`extract`], but the permissive natural-language line scan only
/// accepts skill names for which `is_known` returns true. Without this
/// filter, ordinary prose like `cargo build > compiles fine` parses as a
/// tool call to the unknown skill `cargo`, which injects a spurious error
/// tool-result into the conversation and derails the model. The explicit
/// ```tool_call``` JSON fence is still accepted regardless — there the
/// model's intent to call a tool is unambiguous, so an unknown name should
/// surface as an "unknown skill" error the model can correct.
pub fn extract_known(text: &str, is_known: impl Fn(&str) -> bool) -> Option<ToolCall> {
    let wrapped = has_xml_wrapper(text);
    let text = unwrap_xml_call(text);
    let text = text.as_ref();
    if let Some(tc) = extract_json_fence(text) {
        return Some(tc);
    }
    if wrapped {
        // The envelope makes the intent unambiguous, so a JSON body inside
        // it is read even without a fence — but only inside it, so a stray
        // `{` in prose never becomes a call.
        if let Some(tc) = extract_json_envelope(text) {
            return Some(tc);
        }
    }
    let mut offset = 0usize;
    for line in text.split('\n') {
        let trimmed = strip_fence_indent(line);
        if let Some(tc) = parse_line(trimmed) {
            if is_known(&tc.skill) {
                return Some(tc);
            }
        } else if !trimmed.is_empty() {
            // The one-line parse failed. When the line opens a quote it never
            // closes, the argument body continues on the lines below, so
            // retry from this line's start over the rest of the text.
            let indent = line.len() - line.trim_start().len();
            if let Some(tc) = parse_multiline(&text[offset + indent..]) {
                if is_known(&tc.skill) {
                    return Some(tc);
                }
            }
        }
        offset += line.len() + 1;
    }
    None
}

/// Like [`extract_known`], but told how many positional arguments each
/// registered skill takes (`arity` is `None` for a name nobody registered),
/// which buys the live loops three recoveries a model eval must not grant:
///
/// - **One argument, verbatim.** For a skill with a single positional
///   (`run-pwsh`, `read-file`, `subagent`, …) everything between the outer
///   quotes is the argument as written. Only an escaped outer quote is
///   unescaped, so a Windows path keeps its `\n`, `\t` and `\r` and a regex
///   keeps its `\\` (`sessions/85` ran `…\Output\local\raw` as a carriage
///   return), and a quote *inside* the argument — PowerShell's `-ne ' '`,
///   an apostrophe — no longer cuts the command short (`sessions/92` ran
///   half a command and dropped the rest). Trailing `'key=value'` tokens
///   stay separate, so optional args still bind.
/// - **Qwen's own parameters.** `<function=name>` followed by
///   `<parameter=key>value</parameter>` blocks is read as named arguments
///   (`sessions/93` wrote a file that way twice, and was refused twice).
/// - **No expectation.** A call inside a closed `<function=name>` block —
///   on the tag's line or below it, quoted or, for a one-argument skill,
///   not — or on the reply's last line, is dispatched without its
///   ` > <expectation>` (`sessions/94` ended its turn on two
///   `read-file 'wk.txt'` calls).
pub fn extract_for(text: &str, arity: impl Fn(&str) -> Option<usize>) -> Option<ToolCall> {
    if let Some(tc) = extract_xml_params(text) {
        return Some(tc);
    }
    let raw = text;
    let wrapped = has_xml_wrapper(text);
    let functions = wrapper_functions(text);
    let unwrapped = unwrap_xml_call(text);
    let text = unwrapped.as_ref();
    if let Some(tc) = extract_json_fence(text) {
        return Some(tc);
    }
    if wrapped {
        if let Some(tc) = extract_json_envelope(text) {
            return Some(tc);
        }
    }
    let mut offset = 0usize;
    for line in text.split('\n') {
        let trimmed = strip_fence_indent(line);
        if let Some(tc) = parse_line_for(trimmed, &arity) {
            if arity(&tc.skill).is_some() {
                return Some(tc);
            }
        } else if !trimmed.is_empty() {
            let indent = line.len() - line.trim_start().len();
            if let Some(tc) = parse_multiline_for(&text[offset + indent..], &arity) {
                if arity(&tc.skill).is_some() {
                    return Some(tc);
                }
            }
        }
        offset += line.len() + 1;
    }
    if let Some(tc) = extract_function_body(raw, &arity) {
        return Some(tc);
    }
    bare_call(text, &functions, &arity)
}

/// A closed `<function=name>` … `</function>` block whose body is the
/// call's arguments rather than `<parameter=…>` tags — most often on the
/// line *below* the tag, where the line scan cannot pair them:
///
/// ```text
/// <function=read-file>
/// wk.txt
/// </function>
/// ```
///
/// (a Qwen 3.6 session of 2026-09-27 was refused on exactly that twice,
/// and its turn ended). The body is read as the rest of a call line — quoted
/// args with or without an expectation — and, for a one-argument skill, an
/// unquoted body is the argument itself, as written.
fn extract_function_body(text: &str, arity: &impl Fn(&str) -> Option<usize>) -> Option<ToolCall> {
    for (i, _) in text.match_indices('<') {
        let Some((XmlTag::FunctionOpen(name), end)) = xml_tag_at(text, i) else { continue };
        let skill = skill_spelling(name);
        let Some(n) = arity(&skill) else { continue };
        let rest = &text[end..];
        // Only a block the model closed: a tag named in passing is prose.
        let Some(close) = ["</function>", "</tool_call>"]
            .iter()
            .filter_map(|tag| rest.find(tag))
            .min()
        else {
            continue;
        };
        let body = rest[..close].trim();
        if body.is_empty() || body.contains("<function=") || body.starts_with("<parameter=") {
            continue;
        }
        let line = format!("{skill} {body}");
        let parsed = if body.contains('\n') {
            parse_multiline_for(&line, arity)
        } else {
            parse_line_for(&line, arity)
        };
        if let Some(tc) = parsed {
            return Some(tc);
        }
        if let Some(raw_args) = bare_args(body, n) {
            return Some(ToolCall { skill, raw_args, ..ToolCall::default() });
        }
        if n == 1 && !body.starts_with(['\'', '"']) {
            return Some(ToolCall { skill, raw_args: vec![body.to_string()], ..ToolCall::default() });
        }
    }
    None
}

/// The arguments of a call written without an expectation: all quoted —
/// one verbatim argument for a one-argument skill, escapes processed for
/// the rest. `None` when any of it is bare.
fn bare_args(rest: &str, arity: usize) -> Option<Vec<String>> {
    if arity == 1 {
        one_arg(rest)
    } else {
        verbatim_tokens(rest).and_then(|_| tokenize_args(rest))
    }
}

/// [`parse_line`], except that a one-argument skill takes its argument
/// verbatim (see [`extract_for`]).
fn parse_line_for(line: &str, arity: &impl Fn(&str) -> Option<usize>) -> Option<ToolCall> {
    let line = line.trim();
    let (skill, rest) = take_skill_name(line)?;
    if arity(&skill) != Some(1) {
        return parse_line(line);
    }
    let rest = rest.trim_start();
    let (expectation, raw_args) = split_one_arg(rest).or_else(|| {
        let (args_part, expectation) = split_on_expectation(rest)?;
        Some((expectation, tokenize_args(args_part)?))
    })?;
    Some(ToolCall {
        skill,
        raw_args,
        expectation: expectation.trim().to_string(),
        ..ToolCall::default()
    })
}

/// [`parse_multiline`], with a one-argument skill's body taken verbatim.
fn parse_multiline_for(text: &str, arity: &impl Fn(&str) -> Option<usize>) -> Option<ToolCall> {
    let (skill, rest) = take_skill_name(text)?;
    if arity(&skill) != Some(1) {
        return parse_multiline(text);
    }
    let rest = rest.trim_start();
    if !quote_open_at_eol(rest) {
        return None;
    }
    let (args_part, expectation) = split_on_expectation(rest)?;
    let raw_args = one_arg(args_part).or_else(|| tokenize_args(args_part))?;
    Some(ToolCall {
        skill,
        raw_args,
        expectation: expectation.lines().next().unwrap_or("").trim().to_string(),
        ..ToolCall::default()
    })
}

/// Split a one-argument call's `rest` so the left side reads as that one
/// argument: at the first ` > ` outside quotes when that works, else at the
/// last ` > ` straight after a closing quote — an odd quote *inside* the
/// argument (an apostrophe) throws the first reading out of phase, and
/// the second is what the line meant. Returns (expectation, args).
fn split_one_arg(rest: &str) -> Option<(&str, Vec<String>)> {
    if let Some((args_part, expectation)) = split_on_expectation(rest) {
        if let Some(args) = one_arg(args_part) {
            return Some((expectation, args));
        }
    }
    let (args_part, expectation) = split_after_last_quote(rest)?;
    Some((expectation, one_arg(args_part)?))
}

/// The args of a one-argument skill: the argument itself, then any extras
/// the model appended as separate simple tokens (`'background=true'`,
/// `'80'`). A second token that is not simple — ` }).Count"` — means the
/// quotes belong to the argument, so the whole span is the argument.
fn one_arg(args_part: &str) -> Option<Vec<String>> {
    let s = args_part.trim();
    if let Some(tokens) = verbatim_tokens(s) {
        if tokens.len() <= 1 || tokens[1..].iter().all(|t| simple_extra(t)) {
            return Some(tokens);
        }
    }
    single_span(s)
}

/// Every token of `s`, each quoted, taken as written: only an escaped
/// quote of the token's own kind is unescaped. `None` when a token is bare
/// or never closed.
fn verbatim_tokens(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(q) = chars.next() else { break };
        if q != '\'' && q != '"' {
            return None;
        }
        let mut buf = String::new();
        let mut closed = false;
        while let Some(c) = chars.next() {
            if c == '\\' && chars.peek() == Some(&q) {
                chars.next();
                buf.push(q);
            } else if c == q {
                closed = true;
                break;
            } else {
                buf.push(c);
            }
        }
        if !closed {
            return None;
        }
        out.push(buf);
    }
    Some(out)
}

/// `s` as one quoted span whose inside may hold quotes of its own, with
/// trailing `'key=value'` tokens peeled off as extras. `None` unless `s`
/// (after the peel) opens and closes with the same quote.
fn single_span(s: &str) -> Option<Vec<String>> {
    let mut s = s.trim();
    let mut extras: Vec<String> = Vec::new();
    while let Some((rest, kv)) = peel_kv_token(s) {
        extras.insert(0, kv);
        s = rest;
    }
    let q = s.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    if s.len() < 2 || !s.ends_with(q) {
        return None;
    }
    let inner = &s[1..s.len() - 1];
    let mut out = vec![inner.replace(&format!("\\{q}"), &q.to_string())];
    out.extend(extras);
    Some(out)
}

/// Peel one trailing `'key=value'` token off `s`: it has to stand alone —
/// whitespace before it, and before that the quote that closed the
/// argument it follows. Returns (what is left, the token's text).
fn peel_kv_token(s: &str) -> Option<(&str, String)> {
    let q = s.chars().last().filter(|c| *c == '\'' || *c == '"')?;
    let body = &s[..s.len() - 1];
    let open = body.rfind(q)?;
    let inner = &body[open + 1..];
    let before = &body[..open];
    let rest = before.trim_end();
    if rest.len() == before.len() || !(rest.ends_with('\'') || rest.ends_with('"')) {
        return None;
    }
    let (key, _) = inner.split_once('=')?;
    if !is_arg_name(key.trim()) {
        return None;
    }
    Some((rest, inner.to_string()))
}

/// A token worth keeping apart from a one-argument skill's argument: a
/// `key=value` pair, or one short word with no quotes or spaces in it.
fn simple_extra(t: &str) -> bool {
    let t = t.trim();
    if t.is_empty() || t.len() > 200 {
        return false;
    }
    if let Some((key, _)) = t.split_once('=') {
        if is_arg_name(key.trim()) {
            return true;
        }
    }
    !t.contains(char::is_whitespace) && !t.contains(['\'', '"'])
}

/// `[A-Za-z_][A-Za-z0-9_]*` — what an optional argument's name looks like.
fn is_arg_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The last ` > ` that directly follows a quote of the kind `rest` opens
/// with. Returns (args part, expectation), the args part ending on that
/// quote.
fn split_after_last_quote(rest: &str) -> Option<(&str, &str)> {
    let q = rest.chars().next().filter(|c| *c == '\'' || *c == '"')? as u8;
    let bytes = rest.as_bytes();
    let mut i = bytes.len();
    while i > 1 {
        i -= 1;
        if bytes[i] != q {
            continue;
        }
        let after = &rest[i + 1..];
        let spaced = after.trim_start();
        if spaced.len() == after.len() {
            continue;
        }
        if let Some(expectation) = spaced.strip_prefix('>') {
            if expectation.starts_with(char::is_whitespace) {
                return Some((&rest[..=i], expectation.trim_start()));
            }
        }
    }
    None
}

/// A call written with no ` > <expectation>` at all, accepted only where
/// the intent is not in doubt: a line opened by one of the reply's
/// `<function=name>` tags (`functions`), or, with no such tag, the reply's
/// last line. Every argument must be quoted — a bare word after a skill
/// name is prose ("read-file is the tool"), not an argument.
fn bare_call(
    text: &str,
    functions: &[String],
    arity: &impl Fn(&str) -> Option<usize>,
) -> Option<ToolCall> {
    let lines: Vec<&str> = text
        .lines()
        .map(|l| strip_fence_indent(l).trim())
        .filter(|l| !l.is_empty())
        .collect();
    let candidates: Vec<&str> = if functions.is_empty() {
        lines.last().copied().into_iter().collect()
    } else {
        lines
            .into_iter()
            .filter(|l| {
                functions.iter().any(|f| {
                    l.strip_prefix(f.as_str())
                        .is_some_and(|r| r.is_empty() || r.starts_with(char::is_whitespace))
                })
            })
            .collect()
    };
    for line in candidates {
        let (skill, rest) = match take_skill_name(line) {
            Some(split) => split,
            None if is_valid_skill_name(line) => (line.to_string(), ""),
            None => continue,
        };
        let Some(n) = arity(&skill) else { continue };
        let rest = rest.trim();
        let raw_args = if rest.is_empty() {
            Vec::new()
        } else {
            match bare_args(rest, n) {
                Some(args) => args,
                None => continue,
            }
        };
        return Some(ToolCall { skill, raw_args, ..ToolCall::default() });
    }
    None
}

/// The skill names of every `<function=name>` tag in `text`, spelled the
/// way the line scan will see them.
fn wrapper_functions(text: &str) -> Vec<String> {
    text.match_indices('<')
        .filter_map(|(i, _)| match xml_tag_at(text, i)? {
            (XmlTag::FunctionOpen(name), _) => Some(skill_spelling(name)),
            _ => None,
        })
        .collect()
}

/// A tool name as this contract spells skills: `read_file` → `read-file`.
/// Models trained on snake_case tool names write them that way inside
/// their native wrappers.
fn skill_spelling(name: &str) -> String {
    name.trim().replace('_', "-")
}

/// Read Qwen's native parameter form:
///
/// ```text
/// <function=write-file>
/// <parameter=path>
/// notes.txt
/// </parameter>
/// <parameter=content>
/// …
/// </parameter>
/// </function>
/// ```
///
/// A value is everything between its tags less the one line break on each
/// side the template puts there, so a file body arrives exactly as written
/// — no quoting, no escapes. A missing `</parameter>` ends the value at the
/// next tag. A `<parameter=expectation>` becomes the call's expectation.
fn extract_xml_params(text: &str) -> Option<ToolCall> {
    for (i, _) in text.match_indices('<') {
        let Some((XmlTag::FunctionOpen(name), end)) = xml_tag_at(text, i) else { continue };
        let params = xml_params(&text[end..]);
        if params.is_empty() {
            continue;
        }
        let skill = skill_spelling(name);
        if !is_valid_skill_name(&skill) {
            return None;
        }
        let mut expectation = String::new();
        let mut named = Vec::with_capacity(params.len());
        for (key, value) in params {
            if key == "expectation" {
                expectation = value.trim().to_string();
            } else {
                named.push((key, value));
            }
        }
        let raw_args = named.iter().map(|(_, v)| v.clone()).collect();
        return Some(ToolCall { skill, raw_args, expectation, args_json: None, named });
    }
    None
}

/// The `<parameter=key>value</parameter>` blocks at the start of `body`.
fn xml_params(body: &str) -> Vec<(String, String)> {
    const OPEN: &str = "<parameter=";
    const CLOSE: &str = "</parameter>";
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let Some(after) = rest.trim_start().strip_prefix(OPEN) else { break };
        let Some(gt) = after.find('>') else { break };
        let key = after[..gt].trim().to_string();
        let value_on = &after[gt + 1..];
        let (value, next) = match value_on.find(CLOSE) {
            Some(end) => (&value_on[..end], &value_on[end + CLOSE.len()..]),
            None => {
                let end = [OPEN, "</function>", "</tool_call>"]
                    .iter()
                    .filter_map(|tag| value_on.find(tag))
                    .min()
                    .unwrap_or(value_on.len());
                (&value_on[..end], &value_on[end..])
            }
        };
        if key.is_empty() {
            break;
        }
        out.push((key, strip_template_breaks(value)));
        rest = next;
    }
    out
}

/// `value` less one line break at each end — the ones the template writes
/// around every value, which are no part of it.
fn strip_template_breaks(value: &str) -> String {
    let v = value
        .strip_prefix("\r\n")
        .or_else(|| value.strip_prefix('\n'))
        .unwrap_or(value);
    let v = v.strip_suffix("\r\n").or_else(|| v.strip_suffix('\n')).unwrap_or(v);
    v.to_string()
}

/// The newest complete tool call drafted in `reasoning`, as the line the
/// model wrote. A reasoning model that stops mid-thought (`sessions/95`
/// ended 24 K tokens of reasoning mid-sentence, the call it had settled on
/// a few lines up) never sends that call, and its reasoning is not part of
/// the next request — handing the line back spares it re-deriving the
/// plan. Lines are read newest first; a line over 8 KiB is not offered.
pub fn last_drafted_call(reasoning: &str, is_known: impl Fn(&str) -> bool) -> Option<String> {
    reasoning
        .lines()
        .rev()
        .map(|l| l.trim().trim_matches('`').trim())
        .filter(|l| !l.is_empty() && l.len() <= 8 * 1024)
        .find(|l| parse_line(l).is_some_and(|tc| is_known(&tc.skill)))
        .map(str::to_string)
}

/// True when `text` contains a shape the model commonly *thinks* is a tool
/// call but this parser does not accept. Callers use it to surface the
/// silent-drop case instead of treating the reply as ordinary prose. Kept
/// conservative: the explicit ```tool_call fence, and the OpenAI-ish
/// `"skill": … "args": …` JSON pair.
pub fn looks_like_attempt(text: &str) -> bool {
    if text.contains("```tool_call") || has_xml_wrapper(text) {
        return true;
    }
    let has_skill_key = text.contains("\"skill\"") || text.contains("'skill'");
    let has_args_key  = text.contains("\"args\"")  || text.contains("'args'");
    has_skill_key && has_args_key
}

/// Explain, in one human-readable clause, why a reply that produced no
/// parsable tool call still looks like an attempt at one. `None` means the
/// reply is ordinary prose and the model simply chose not to call a tool.
///
/// Only meaningful *after* [`extract_known`] returned `None` for the same
/// text and predicate. The natural-language branch is deliberately narrow:
/// the line must start with a **registered** skill name and still carry a
/// quote or a `>`, so prose like "read-file is the skill you want" is not
/// mistaken for a botched call.
pub fn rejected_attempt(text: &str, is_known: impl Fn(&str) -> bool) -> Option<String> {
    let wrapped = has_xml_wrapper(text);
    let text = unwrap_xml_call(text);
    let text = text.as_ref();
    if text.contains("```tool_call") {
        // The reason reaches the model in the syntax correction, so it names
        // the defect precisely enough to fix rather than restating the
        // contract the model already tried to follow.
        let why = match json_fence_body(text) {
            Some(body) => parse_json_body(body)
                .err()
                .unwrap_or_else(|| "whose body could not be read".into()),
            None => "with no closing ``` line after its JSON".into(),
        };
        return Some(format!("a ```tool_call block {why}"));
    }
    for line in text.lines() {
        let trimmed = strip_fence_indent(line);
        let Some((name, rest)) = take_skill_name(trimmed) else { continue };
        if !is_known(&name) {
            continue;
        }
        // Without one of these the line is prose that merely happens to open
        // with a skill name.
        if !rest.contains('\'') && !rest.contains('"') && !rest.contains('>') {
            continue;
        }
        let rest = rest.trim_start();
        if split_on_expectation(rest).is_none() {
            // A quote still open at the end of the line swallowed any ` > `
            // after it. Shell commands hit this constantly — a PowerShell
            // `' '` inside a '…' argument closes it early.
            if quote_open_at_eol(rest) {
                return Some(format!(
                    "a `{name}` line whose quoted argument is never closed \
                     (a quote of the same kind inside an argument must be \
                     escaped, as `\\'` inside '…')"
                ));
            }
            return Some(format!("a `{name}` line with no ` > <expectation>` part"));
        }
        return Some(format!(
            "a `{name}` line whose arguments are not correctly quoted"
        ));
    }
    if wrapped {
        return Some(
            "a `<tool_call>` / `<function=…>` wrapper whose call the parser \
             could not read (the skill name and its quoted arguments belong \
             on one line, followed by ` > <expectation>`)"
                .into(),
        );
    }
    if looks_like_attempt(text) {
        return Some(
            "a JSON object with `skill`/`args` keys outside a ```tool_call fence"
                .into(),
        );
    }
    None
}

/// Scan `text` for the first ```tool_call``` fenced block and parse its body
/// as JSON. Returns `None` if no such fence exists, the JSON is malformed,
/// or the required `skill` / `args` keys are missing.
fn extract_json_fence(text: &str) -> Option<ToolCall> {
    parse_json_body(json_fence_body(text)?).ok()
}

/// The body of the first ```tool_call``` fence in `text`, or `None` when
/// there is no such fence or it is never closed.
fn json_fence_body(text: &str) -> Option<&str> {
    let mut rest = text;
    while let Some(open_idx) = rest.find("```") {
        let after_ticks = &rest[open_idx + 3..];
        let (lang, after_lang) = match after_ticks.find('\n') {
            Some(nl) => (after_ticks[..nl].trim(), &after_ticks[nl + 1..]),
            None     => return None,
        };
        // Only recognise the `tool_call` info-string. Other fences (e.g. a
        // sample ```json block in a chat reply) are deliberately ignored
        // so the parser never hijacks unrelated content.
        if !lang.eq_ignore_ascii_case("tool_call") {
            rest = after_lang;
            continue;
        }
        let close_idx = after_lang.find("```")?;
        return Some(&after_lang[..close_idx]);
    }
    None
}

/// Read a tool-call JSON body. The error completes the sentence
/// "a ```tool_call block …" in [`rejected_attempt`].
fn parse_json_body(body: &str) -> Result<ToolCall, String> {
    let value = parse_json_lenient(body.trim())?;
    let obj = value.as_object().ok_or("whose JSON is not an object")?;
    // `name` / `arguments` are the OpenAI and Hermes spellings of the same
    // pair; a model that wraps the call in `<tool_call>` uses those.
    let skill = obj
        .get("skill")
        .or_else(|| obj.get("name"))
        .and_then(|v| v.as_str())
        .map(skill_spelling)
        .ok_or("whose JSON has no `skill` name")?;
    if !is_valid_skill_name(&skill) {
        return Err(format!("whose `skill` ({skill:?}) is not a skill name"));
    }
    let mut args = obj
        .get("args")
        .or_else(|| obj.get("arguments"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let mut expectation = obj
        .get("expectation")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // The model that loses count of its braces (see `parse_json_lenient`)
    // has written the expectation inside `args`. The contract reserves that
    // key for the call itself, so it goes back to the top level.
    if expectation.is_empty() {
        if let Some(serde_json::Value::String(inner)) =
            args.as_object_mut().and_then(|m| m.remove("expectation"))
        {
            expectation = inner;
        }
    }
    // Populate `raw_args` from the args object's *values* so the UI's
    // `args_preview` chip still shows something useful even though the
    // dispatcher will route via `args_json` instead.
    let raw_args = args
        .as_object()
        .map(|m| {
            m.values()
                .map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(ToolCall {
        skill,
        raw_args,
        expectation,
        args_json: Some(args),
        named: Vec::new(),
    })
}

/// Parse tool-call JSON, forgiving the two slips a model makes writing it
/// around a script: a raw line break inside a string value, and stopping
/// short of the closing brackets. A command full of `{ … }` blocks makes
/// the model lose count of its own braces — `sessions/3` holds five
/// `run-pwsh` calls in a row that were each exactly one `}` short, and the
/// turn died on them. Both repairs are unambiguous; a string that is never
/// closed is left open, since where it should end is anyone's guess.
///
/// The error is the reason sentence for [`rejected_attempt`], and names the
/// first defect the repairs leave — not a slip they would have forgiven,
/// which would spend the model's one retry on the wrong fix.
fn parse_json_lenient(body: &str) -> Result<serde_json::Value, String> {
    let strict = match serde_json::from_str(body) {
        Ok(value) => return Ok(value),
        Err(e) => e,
    };
    let fixed = repair_json(body);
    if fixed == body {
        return Err(json_error_reason(body, &strict));
    }
    serde_json::from_str(&fixed).map_err(|e| json_error_reason(&fixed, &e))
}

/// `body` with raw control characters inside strings escaped and, when
/// every string is closed and every closing bracket matches its opener, the
/// brackets still open at its end closed. A mismatched closer means the
/// structure is wrong rather than short, so nothing is appended to it.
fn repair_json(body: &str) -> String {
    let mut out = String::with_capacity(body.len() + 8);
    let mut open: Vec<char> = Vec::new();
    let mut mismatched = false;
    let mut in_string = false;
    let mut escaped = false;
    for c in body.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            } else if c < ' ' {
                match c {
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    other => out.push_str(&format!("\\u{:04x}", other as u32)),
                }
                continue;
            }
        } else {
            match c {
                '"' => in_string = true,
                '{' => open.push('}'),
                '[' => open.push(']'),
                '}' | ']' => mismatched |= open.pop() != Some(c),
                _ => {}
            }
        }
        out.push(c);
    }
    if !in_string && !mismatched {
        out.extend(open.into_iter().rev());
    }
    out
}

/// Name a JSON syntax error the way a model can act on: serde's message and
/// position, the text just before it (a column number alone is useless in a
/// one-line body of a thousand characters), and the escaping rule that the
/// shell commands and Windows paths inside tool calls break most often.
fn json_error_reason(body: &str, e: &serde_json::Error) -> String {
    let before = text_before(body, e.line(), e.column());
    let at = if before.is_empty() {
        String::new()
    } else {
        format!(", right after `{before}`")
    };
    format!(
        "whose JSON does not parse: {e}{at} (inside a JSON string write every \
         `\"` as `\\\"` and every `\\` as `\\\\`)"
    )
}

/// Up to 40 bytes of `body`'s line `line` (1-based) ending at byte column
/// `column` — where serde found the error.
fn text_before(body: &str, line: usize, column: usize) -> String {
    let line_start: usize = body
        .split_inclusive('\n')
        .take(line.saturating_sub(1))
        .map(str::len)
        .sum();
    let mut end = (line_start + column).min(body.len());
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    let mut start = end.saturating_sub(40).max(line_start).min(end);
    while !body.is_char_boundary(start) {
        start += 1;
    }
    let cut = if start > line_start { "…" } else { "" };
    format!("{cut}{}", &body[start..end])
}

fn is_valid_skill_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else { return false };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Strip a leading ```tool fence marker and surrounding whitespace; leave
/// non-fenced lines unchanged (minus leading whitespace).
fn strip_fence_indent(line: &str) -> &str {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        // The fence delimiter itself is never a tool-call line.
        ""
    } else {
        trimmed
    }
}

/// One recognised piece of the Hermes / Qwen wrapper.
enum XmlTag<'a> {
    /// `<tool_call>` or `</tool_call>`.
    Envelope,
    /// `<function=name>`: the skill name it carries.
    FunctionOpen(&'a str),
    /// `</function>`.
    FunctionClose,
}

/// Recognise a wrapper tag starting at byte `at` of `text` (which must be a
/// `<`). Returns the tag and the byte offset just past its `>`. Whitespace
/// inside the angle brackets is tolerated — chat surfaces sometimes insert
/// it when the text is pasted back.
fn xml_tag_at(text: &str, at: usize) -> Option<(XmlTag<'_>, usize)> {
    let s = text.get(at..)?;
    let mut s = s.strip_prefix('<')?.trim_start();
    let closing = if let Some(r) = s.strip_prefix('/') {
        s = r.trim_start();
        true
    } else {
        false
    };
    let word_end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    let word = &s[..word_end];
    let rest = s[word_end..].trim_start();
    let consumed = |rest_after: &str| text.len() - rest_after.len();
    if word.eq_ignore_ascii_case("tool_call") {
        let r = rest.strip_prefix('>')?;
        return Some((XmlTag::Envelope, consumed(r)));
    }
    if !word.eq_ignore_ascii_case("function") {
        return None;
    }
    if closing {
        let r = rest.strip_prefix('>')?;
        return Some((XmlTag::FunctionClose, consumed(r)));
    }
    let after_eq = rest.strip_prefix('=')?.trim_start();
    let name_end = after_eq
        .find(|c: char| c.is_whitespace() || c == '>' || c == '<')
        .unwrap_or(after_eq.len());
    let name = &after_eq[..name_end];
    if name.is_empty() {
        return None;
    }
    let r = after_eq[name_end..].trim_start().strip_prefix('>')?;
    Some((XmlTag::FunctionOpen(name), consumed(r)))
}

/// True when `text` carries the `<tool_call>` envelope or a
/// `<function=name>` opener anywhere.
pub fn has_xml_wrapper(text: &str) -> bool {
    text.match_indices('<').any(|(i, _)| xml_tag_at(text, i).is_some())
}

/// Strip the Hermes / Qwen wrapper so the contract's line scan sees
/// `read-file 'x' > expectation` where the model wrote
/// `<tool_call>\n<function=read-file> 'x' > expectation\n</function>\n</tool_call>`.
/// Envelope and closing tags become line breaks, a `<function=name>` opener
/// becomes the bare name. Text without a wrapper is returned untouched.
pub fn unwrap_xml_call(text: &str) -> std::borrow::Cow<'_, str> {
    if !has_xml_wrapper(text) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text.as_bytes()[i] == b'<' {
            if let Some((tag, end)) = xml_tag_at(text, i) {
                match tag {
                    XmlTag::Envelope | XmlTag::FunctionClose => out.push('\n'),
                    XmlTag::FunctionOpen(name) => {
                        out.push_str(&skill_spelling(name));
                        out.push(' ');
                    }
                }
                i = end;
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    std::borrow::Cow::Owned(out)
}

/// Read a `{ "name": …, "arguments": … }` body that sat inside a
/// `<tool_call>` envelope: the first `{` to the last `}` of the unwrapped
/// text. Only called when the envelope was present.
fn extract_json_envelope(text: &str) -> Option<ToolCall> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    parse_json_body(&text[start..=end]).ok()
}

fn parse_line(line: &str) -> Option<ToolCall> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    // First token: the skill name.
    let (skill, rest) = take_skill_name(line)?;
    let rest = rest.trim_start();

    // Split into left-of-`>` (args) and right-of-`>` (expectation). The
    // separator is a `>` surrounded by whitespace so `>` characters inside
    // quoted strings (e.g. `'foo > bar'`) don't confuse the split.
    let (args_part, expectation) = split_on_expectation(rest)?;

    let raw_args = tokenize_args(args_part)?;
    Some(ToolCall {
        skill,
        raw_args,
        expectation: expectation.trim().to_string(),
        ..ToolCall::default()
    })
}

/// Parse a call whose quoted argument runs across several physical lines.
///
/// The contract asks the model to escape a body as `\n`, but a `write-file`
/// of any real size comes back with literal newlines instead — the call then
/// fails [`parse_line`], is dropped, and the model, never seeing a result,
/// retries the same unparseable text turn after turn (`sessions/79.jsonl`).
/// Accepting the shape is cheaper than fighting it.
///
/// Only attempted when the first line leaves a quote open, which is what
/// separates a wrapped argument body from prose that merely happens to start
/// with a skill name. The expectation still ends at its own line break.
fn parse_multiline(text: &str) -> Option<ToolCall> {
    let (skill, rest) = take_skill_name(text)?;
    let rest = rest.trim_start();
    if !quote_open_at_eol(rest) {
        return None;
    }
    let (args_part, expectation) = split_on_expectation(rest)?;
    let raw_args = tokenize_args(args_part)?;
    Some(ToolCall {
        skill,
        raw_args,
        expectation: expectation.lines().next().unwrap_or("").trim().to_string(),
        ..ToolCall::default()
    })
}

/// True when the first physical line of `s` opens a quote it never closes.
fn quote_open_at_eol(s: &str) -> bool {
    let line = s.split('\n').next().unwrap_or("");
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        match quote {
            // Inside a quote a backslash escapes the next byte, so a line
            // ending in an escaped quote does not read as closed.
            Some(q) => {
                if bytes[i] == b'\\' {
                    i += 1;
                } else if bytes[i] == q {
                    quote = None;
                }
            }
            None => {
                if bytes[i] == b'\'' || bytes[i] == b'"' {
                    quote = Some(bytes[i]);
                }
            }
        }
        i += 1;
    }
    quote.is_some()
}

/// Pull off a leading `[a-z][a-z0-9-]*` identifier followed by whitespace.
/// Returns `(name, remainder)` or `None` if the line doesn't start with a
/// well-formed skill name.
fn take_skill_name(s: &str) -> Option<(String, &str)> {
    let mut chars = s.char_indices();
    let (_, first) = chars.next()?;
    if !first.is_ascii_lowercase() {
        return None;
    }
    let mut end = first.len_utf8();
    for (i, c) in chars {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
            end = i + c.len_utf8();
        } else if c.is_whitespace() {
            return Some((s[..end].to_string(), &s[i..]));
        } else {
            return None;
        }
    }
    // Line is just the skill name with no args and no `>`.
    None
}

/// Find the first ` > ` (whitespace-flanked `>`) that lies *outside* a
/// quoted region. Returns `(left, right)`.
fn split_on_expectation(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        match quote {
            Some(q) => {
                if b == b'\\' {
                    escaped = true;
                } else if b == q {
                    quote = None;
                }
            }
            None => {
                if b == b'\'' || b == b'"' {
                    quote = Some(b);
                } else if b == b'>'
                    && i > 0
                    && bytes[i - 1].is_ascii_whitespace()
                    && i + 1 < bytes.len()
                    && bytes[i + 1].is_ascii_whitespace()
                {
                    return Some((s[..i].trim_end(), s[i + 1..].trim_start()));
                }
            }
        }
        i += 1;
    }
    None
}

/// Split `args_part` into zero or more quoted strings. Supports `\\`, `\n`,
/// `\t`, `\'`, `\"` escapes inside quoted strings. Bare unquoted tokens are
/// also accepted as a fallback so simple cases like `run-cli echo hi >` keep
/// working — they're glued into one positional value.
fn tokenize_args(args_part: &str) -> Option<Vec<String>> {
    let s = args_part.trim();
    if s.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        if c == '\'' || c == '"' {
            let quote = c;
            chars.next();
            let mut buf = String::new();
            let mut closed = false;
            while let Some(c2) = chars.next() {
                if c2 == '\\' {
                    if let Some(esc) = chars.next() {
                        match esc {
                            'n'  => buf.push('\n'),
                            't'  => buf.push('\t'),
                            'r'  => buf.push('\r'),
                            '\\' => buf.push('\\'),
                            '\'' => buf.push('\''),
                            '"'  => buf.push('"'),
                            other => { buf.push('\\'); buf.push(other); }
                        }
                    }
                } else if c2 == quote {
                    closed = true;
                    break;
                } else {
                    buf.push(c2);
                }
            }
            if !closed {
                return None;
            }
            out.push(buf);
        } else {
            // Bare unquoted run — take the rest of the args part as one value.
            let mut buf = String::new();
            for c2 in chars.by_ref() {
                buf.push(c2);
            }
            out.push(buf.trim().to_string());
            break;
        }
    }
    Some(out)
}

/// Render a parsed `ToolCall` back to its canonical natural-language form.
/// Used by the sub-agent to populate the `args_preview` UI field.
pub fn render(skill: &str, raw_args: &[String]) -> String {
    let mut out = String::with_capacity(skill.len() + raw_args.iter().map(|a| a.len() + 4).sum::<usize>());
    out.push_str(skill);
    for a in raw_args {
        out.push(' ');
        out.push('\'');
        for ch in a.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '\'' => out.push_str("\\'"),
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                other => out.push(other),
            }
        }
        out.push('\'');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_basic_line() {
        let s = "Sure, let me check.\n\nread-file 'skills/run-cli.md' > what args does run-cli accept\n";
        let tc = extract(s).unwrap();
        assert_eq!(tc.skill, "read-file");
        assert_eq!(tc.raw_args, vec!["skills/run-cli.md".to_string()]);
        assert_eq!(tc.expectation, "what args does run-cli accept");
    }

    #[test]
    fn extracts_double_quoted() {
        let tc = extract(r#"run-cli "cargo --version" > confirm cargo is installed"#).unwrap();
        assert_eq!(tc.skill, "run-cli");
        assert_eq!(tc.raw_args, vec!["cargo --version".to_string()]);
        assert_eq!(tc.expectation, "confirm cargo is installed");
    }

    #[test]
    fn handles_multiple_positional_args() {
        let tc = extract(r#"write-file 'notes/x.md' 'hello\nworld' > confirm bytes"#).unwrap();
        assert_eq!(tc.skill, "write-file");
        assert_eq!(tc.raw_args, vec!["notes/x.md".to_string(), "hello\nworld".to_string()]);
    }

    #[test]
    fn ignores_gt_inside_quotes() {
        let tc = extract(r#"run-cli 'echo foo > bar.txt' > should write the file"#).unwrap();
        assert_eq!(tc.raw_args, vec!["echo foo > bar.txt".to_string()]);
        assert_eq!(tc.expectation, "should write the file");
    }

    #[test]
    fn tolerates_fence_wrapper() {
        let s = "Here's the call:\n```tool\nread-file 'a.md' > tell me the title\n```\n";
        let tc = extract(s).unwrap();
        assert_eq!(tc.skill, "read-file");
        assert_eq!(tc.expectation, "tell me the title");
    }

    #[test]
    fn no_skill_returns_none() {
        assert!(extract("just normal prose, no tools").is_none());
    }

    #[test]
    fn missing_expectation_returns_none() {
        // No ` > ` separator → not a tool call (the expectation is required).
        assert!(extract("read-file 'a.md'").is_none());
    }

    #[test]
    fn rejects_capitalised_skill_name() {
        assert!(extract("ReadFile 'a.md' > foo").is_none());
    }

    #[test]
    fn handles_unquoted_single_positional() {
        // Fallback for the common case `run-cli echo hi > foo` — everything
        // up to the ` > ` becomes one positional value.
        let tc = extract("run-cli echo hi > does echo work").unwrap();
        assert_eq!(tc.raw_args, vec!["echo hi".to_string()]);
    }

    #[test]
    fn accepts_multiline_quoted_body() {
        // The shape `sessions/79.jsonl` died on three turns running: a
        // well-formed call whose body carries real newlines instead of `\n`.
        let s = "I'll create the file.\n\n\
                 write-file 'a/b.txt' 'line one\nline two\nline three\n' \
                 > confirm bytes written\n";
        let tc = extract(s).unwrap();
        assert_eq!(tc.skill, "write-file");
        assert_eq!(
            tc.raw_args,
            vec!["a/b.txt".to_string(), "line one\nline two\nline three\n".to_string()]
        );
        assert_eq!(tc.expectation, "confirm bytes written");
    }

    #[test]
    fn multiline_expectation_stops_at_its_line_break() {
        // Trailing prose after the call must not be swallowed into the
        // expectation the sub-agent is briefed with.
        let s = "write-file 'a.txt' 'x\ny' > confirm the write\n\nI'll verify it next.";
        let tc = extract(s).unwrap();
        assert_eq!(tc.expectation, "confirm the write");
    }

    #[test]
    fn multiline_body_keeps_inner_gt_out_of_the_split() {
        // A wildcard body is full of `<random: … >`; none of them is the
        // expectation separator because they sit inside the quotes.
        let s = "write-file 'w.txt' 'Maya, <random: a || b >, 85mm lens\nNia, <random: c || d >, 85mm lens\n' > confirm first blocks written";
        let tc = extract(s).unwrap();
        assert_eq!(tc.expectation, "confirm first blocks written");
        assert!(tc.raw_args[1].contains("<random: a || b >"));
        assert!(tc.raw_args[1].contains("85mm lens\nNia"));
    }

    #[test]
    fn multiline_never_fires_without_an_open_quote() {
        // Prose starting with a skill name and a *closed* quote must not glue
        // itself to the ` > ` on a later line. Without the open-quote guard
        // this parses as `write-file` with the prose as its body.
        let s = "write-file 'a.txt' is what you want.\nrun-cli 'x' > y";
        let tc = extract(s).unwrap();
        assert_eq!(tc.skill, "run-cli", "must match the real call, not the prose");
        assert_eq!(tc.raw_args, vec!["x".to_string()]);
    }

    #[test]
    fn unterminated_multiline_body_is_still_rejected() {
        // A quote that never closes anywhere is not a recoverable call.
        assert!(extract("write-file 'a.txt' 'body starts\nand never ends").is_none());
    }

    #[test]
    fn first_match_wins() {
        let s = "blah\nread-file 'a.md' > A\nrun-cli 'echo b' > B\n";
        let tc = extract(s).unwrap();
        assert_eq!(tc.skill, "read-file");
    }

    #[test]
    fn round_trip_render() {
        let r = render("write-file", &["a/b".to_string(), "line1\nline2".to_string()]);
        assert_eq!(r, r"write-file 'a/b' 'line1\nline2'");
    }

    #[test]
    fn extracts_json_fence_with_args_object() {
        // Exactly the shape session 11 produced and the natural-language
        // parser used to silently drop.
        let s = "```tool_call\n{ \"skill\": \"run-cli\", \"args\": { \"command\": \"curl example.com\" } }\n```";
        let tc = extract(s).unwrap();
        assert_eq!(tc.skill, "run-cli");
        assert!(tc.args_json.is_some());
        let args = tc.args_json.unwrap();
        assert_eq!(args["command"], "curl example.com");
        // raw_args mirrors the JSON's *values* for the UI preview.
        assert_eq!(tc.raw_args, vec!["curl example.com".to_string()]);
        assert_eq!(tc.expectation, "");
    }

    /// The exact shape session 84 recorded from Qwen 3.6 under the text
    /// protocol: the contract's line inside the Hermes wrapper.
    #[test]
    fn unwraps_qwen_function_wrapper() {
        let s = "I'll investigate.\n\n<tool_call>\n<function=read-file> 'src/Utils/OutputMetadataTracker.cs' > understand how the metadata DB is read\n</function>\n</tool_call>";
        let tc = extract_known(s, |n| n == "read-file").unwrap();
        assert_eq!(tc.skill, "read-file");
        assert_eq!(tc.raw_args, vec!["src/Utils/OutputMetadataTracker.cs".to_string()]);
        assert_eq!(tc.expectation, "understand how the metadata DB is read");
    }

    /// Same wrapper on one line, no `</function>`, and spaces inside the
    /// angle brackets — the shape the operator pasted back from the GUI.
    #[test]
    fn unwraps_single_line_wrapper_with_spaces() {
        let s = "< tool_call> < function=read-file> 'src/x.cs' > where the file lives  </tool_call>";
        let tc = extract_known(s, |n| n == "read-file").unwrap();
        assert_eq!(tc.skill, "read-file");
        assert_eq!(tc.raw_args, vec!["src/x.cs".to_string()]);
        assert_eq!(tc.expectation, "where the file lives");
    }

    /// The pure Hermes shape: a JSON body with `name`/`arguments` inside
    /// the envelope, no fence.
    #[test]
    fn reads_hermes_json_envelope() {
        let s = "<tool_call>\n{\"name\": \"read-file\", \"arguments\": {\"path\": \"a.md\"}}\n</tool_call>";
        let tc = extract_known(s, |n| n == "read-file").unwrap();
        assert_eq!(tc.skill, "read-file");
        assert_eq!(tc.args_json.unwrap()["path"], "a.md");
    }

    /// A wrapper whose body still does not parse is reported as an attempt,
    /// not passed off as prose.
    #[test]
    fn wrapper_with_parameter_tags_is_a_rejected_attempt() {
        let s = "<tool_call>\n<function=read-file>\n<parameter=path>a.md</parameter>\n</function>\n</tool_call>";
        assert!(extract_known(s, |n| n == "read-file").is_none());
        assert!(looks_like_attempt(s));
        let why = rejected_attempt(s, |n| n == "read-file").unwrap();
        assert!(why.contains("<tool_call>"), "{why}");
    }

    /// Angle brackets in prose are not a wrapper.
    #[test]
    fn plain_angle_brackets_are_not_a_wrapper() {
        let s = "Use <path> as the argument, and a < b holds for these.";
        assert!(!has_xml_wrapper(s));
        assert!(matches!(unwrap_xml_call(s), std::borrow::Cow::Borrowed(_)));
        assert!(extract(s).is_none());
        assert!(rejected_attempt(s, |_| true).is_none());
    }

    #[test]
    fn json_fence_carries_optional_expectation() {
        let s = "```tool_call\n{ \"skill\": \"read-file\", \"args\": { \"path\": \"a.md\" }, \"expectation\": \"frontmatter only\" }\n```";
        let tc = extract(s).unwrap();
        assert_eq!(tc.expectation, "frontmatter only");
    }

    #[test]
    fn json_fence_missing_skill_returns_none() {
        let s = "```tool_call\n{ \"args\": { \"command\": \"x\" } }\n```";
        assert!(extract(s).is_none());
    }

    #[test]
    fn json_fence_rejects_invalid_skill_name() {
        // Uppercase, slashes, etc. — never a real skill identifier.
        let s = "```tool_call\n{ \"skill\": \"Run-CLI\", \"args\": {} }\n```";
        assert!(extract(s).is_none());
    }

    #[test]
    fn json_fence_takes_precedence_over_natural_form() {
        // If both are present, the JSON fence is dispatched (the model is
        // explicit about its tool choice; the trailing prose is incidental).
        let s = "```tool_call\n{ \"skill\": \"run-cli\", \"args\": { \"command\": \"a\" } }\n```\nrun-cli 'b' > confirm";
        let tc = extract(s).unwrap();
        assert!(tc.args_json.is_some());
        assert_eq!(tc.args_json.unwrap()["command"], "a");
    }

    #[test]
    fn rejected_attempt_flags_missing_expectation() {
        let known = |n: &str| n == "read-file";
        // The single most common miscall: quoted arg, no ` > ` clause. The
        // parser drops it, and without this the caller would treat the reply
        // as a finished answer about a file it never opened.
        let reason = rejected_attempt("read-file 'README.md'", known).unwrap();
        assert!(reason.contains("read-file"), "{reason}");
        assert!(reason.contains("expectation"), "{reason}");
    }

    #[test]
    fn rejected_attempt_flags_unreadable_fence() {
        let reason = rejected_attempt("```tool_call\n{ not json\n```", |_| true).unwrap();
        assert!(reason.contains("tool_call"), "{reason}");
    }

    #[test]
    fn rejected_attempt_ignores_prose_and_unknown_skills() {
        let known = |n: &str| n == "read-file";
        // Prose that merely opens with a skill name.
        assert!(rejected_attempt("read-file is the skill you want here.", known).is_none());
        // Ordinary prose.
        assert!(rejected_attempt("The workspace has seven crates.", known).is_none());
        // Shaped like a call, but for a skill nobody registered.
        assert!(rejected_attempt("frobnicate 'a.md'", known).is_none());
    }

    #[test]
    fn rejected_attempt_flags_unbalanced_quotes() {
        let known = |n: &str| n == "run-cli";
        // The unclosed quote swallows the ` > ` separator; the reason names
        // the quote, since adding an expectation would not fix the call.
        let reason = rejected_attempt("run-cli 'cargo test > report the failures", known).unwrap();
        assert!(reason.contains("run-cli"), "{reason}");
        assert!(reason.contains("never closed"), "{reason}");
    }

    /// `sessions/85`: a PowerShell `-join " '` closed the '…' argument early,
    /// and the quote after it ran to the end of the line.
    #[test]
    fn rejected_attempt_flags_a_quote_a_shell_command_left_open() {
        let known = |n: &str| n == "run-pwsh";
        let s = "run-pwsh '$h = ($b | % { $_.ToString(\"x2\") }) -join \" '; $h' > the hex header";
        assert!(extract_known(s, known).is_none());
        let reason = rejected_attempt(s, known).unwrap();
        assert!(reason.contains("never closed"), "{reason}");
        assert!(reason.contains(r"\'"), "{reason}");
    }

    #[test]
    fn looks_like_attempt_matches_fence_and_json_pair() {
        assert!(looks_like_attempt("```tool_call\n{}\n```"));
        assert!(looks_like_attempt(r#"{ "skill": "run-cli", "args": {} }"#));
        assert!(!looks_like_attempt("no tools here"));
    }

    /// `sessions/3`: five `run-pwsh` calls in a row whose PowerShell script
    /// blocks made the model lose count of its braces. Each fence is exactly
    /// one `}` short, with the expectation left inside `args`; every one was
    /// rejected, and the turn ended on the second.
    #[test]
    fn json_fence_one_brace_short_is_repaired() {
        let body = r#"{"skill": "run-pwsh", "args": {"command": "$lines = Get-Content \"wk.txt\"; for ($i = 0; $i -lt [Math]::Min(124, $lines.Count); $i++) { $l = $lines[$i]; $f = 0; if ($l -match \"<random: (.*?)>\") { $p = $Matches[1] -split \"\\|\\|\"; $f = @($p | Where-Object { $_ -match \"front\" }).Count }; Write-Output (\"{0}|{1}\" -f ($i+1), $f) }", "expectation": "front-pose count for each line"}"#;
        let tc = extract(&format!("Let me check.\n\n```tool_call\n{body}\n```")).unwrap();
        assert_eq!(tc.skill, "run-pwsh");
        assert_eq!(tc.expectation, "front-pose count for each line");
        let args = tc.args_json.unwrap();
        assert_eq!(args.as_object().unwrap().len(), 1, "only the command stays in args: {args}");
        let command = args["command"].as_str().unwrap();
        assert!(command.starts_with(r#"$lines = Get-Content "wk.txt";"#), "{command}");
        assert!(command.contains(r#"-split "\|\|""#), "{command}");
        assert!(command.ends_with("$f) }"), "{command}");
    }

    /// The same slip inside a Hermes envelope is read the same way.
    #[test]
    fn json_envelope_one_brace_short_is_repaired() {
        let s = "<tool_call>\n{\"name\": \"read-file\", \"arguments\": {\"path\": \"a.md\"}\n</tool_call>";
        let tc = extract_known(s, |n| n == "read-file").unwrap();
        assert_eq!(tc.args_json.unwrap()["path"], "a.md");
    }

    /// Balanced braces, expectation still inside `args`: it goes back to the
    /// top level instead of reaching the skill as an argument nobody declared.
    #[test]
    fn json_fence_hoists_expectation_out_of_args() {
        let s = "```tool_call\n{\"skill\": \"read-file\", \"args\": {\"path\": \"a.md\", \"expectation\": \"the title\"}}\n```";
        let tc = extract(s).unwrap();
        assert_eq!(tc.expectation, "the title");
        assert_eq!(tc.args_json.unwrap(), serde_json::json!({ "path": "a.md" }));
        assert_eq!(tc.raw_args, vec!["a.md".to_string()]);
    }

    #[test]
    fn json_fence_accepts_raw_line_breaks_in_a_string() {
        let s = "```tool_call\n{\"skill\": \"run-pwsh\", \"args\": {\"command\": \"$a = 1\n$b = 2\n\t$a + $b\"}, \"expectation\": \"the sum\"}\n```";
        let tc = extract(s).unwrap();
        assert_eq!(tc.args_json.unwrap()["command"], "$a = 1\n$b = 2\n\t$a + $b");
    }

    /// Where an unterminated string should end is a guess, and a closer that
    /// does not match its opener is a wrong structure rather than a short
    /// one — neither is repaired into a call that runs something made up.
    #[test]
    fn json_fence_repair_does_not_guess() {
        let open_string = "```tool_call\n{\"skill\": \"run-pwsh\", \"args\": {\"command\": \"Get-ChildItem\n```";
        assert!(extract(open_string).is_none());
        let why = rejected_attempt(open_string, |_| true).unwrap();
        assert!(why.contains("EOF while parsing a string"), "{why}");

        let mismatched = "```tool_call\n{\"skill\": \"run-cli\", \"args\": {\"command\": \"dir\"]\n```";
        assert!(extract(mismatched).is_none());
    }

    /// The reason quotes serde's error and the text just before it, so the
    /// model sees which backslash broke the JSON — a Windows path, usually.
    #[test]
    fn rejected_fence_names_the_json_error_and_where() {
        let s = "```tool_call\n{\"skill\": \"read-file\", \"args\": {\"path\": \"C:\\Users\\me\\a.txt\"}}\n```";
        assert!(extract(s).is_none());
        let why = rejected_attempt(s, |_| true).unwrap();
        assert!(why.starts_with("a ```tool_call block whose JSON does not parse"), "{why}");
        assert!(why.contains("invalid escape"), "{why}");
        assert!(why.contains(r#""C:\U`"#), "the excerpt ends at the bad escape: {why}");
        assert!(why.contains(r"every `\` as `\\`"), "{why}");
    }

    /// A body the repairs cannot finish is reported by the defect they
    /// leave, not by a slip they forgave: the raw line break is fine, the
    /// unescaped quote is what the model must fix on its one retry.
    #[test]
    fn rejected_fence_reports_the_defect_the_repair_leaves() {
        let s = "```tool_call\n{\"skill\": \"run-pwsh\", \"args\": {\"command\": \"$a = 1\nGet-Content \"x.txt\"\"}}\n```";
        assert!(extract(s).is_none());
        let why = rejected_attempt(s, |_| true).unwrap();
        assert!(!why.contains("control character"), "{why}");
        assert!(why.contains("expected `,` or `}`"), "{why}");
        assert!(why.contains(r#"Get-Content "x`"#), "{why}");
    }

    #[test]
    fn rejected_fence_without_a_closing_line_says_so() {
        let why = rejected_attempt("```tool_call\n{\"skill\": \"glob\", \"args\": {}}", |_| true).unwrap();
        assert!(why.contains("no closing ```"), "{why}");
    }

    #[test]
    fn ignores_other_fenced_blocks() {
        // A plain ```json block is NOT a tool call.
        let s = "```json\n{ \"skill\": \"run-cli\", \"args\": {} }\n```";
        assert!(extract(s).is_none());
    }

    /// The registry's arities for the skills these tests name.
    fn arity(name: &str) -> Option<usize> {
        match name {
            "run-cli" | "run-pwsh" | "read-file" | "subagent" => Some(1),
            "write-file" => Some(2),
            "edit-file" => Some(3),
            "job-list" => Some(0),
            _ => None,
        }
    }

    /// `sessions/92`: PowerShell's `-ne ' '` inside the '…' argument. The
    /// strict tokenizer ended the command at the inner quote, dropped the
    /// rest, and cmd.exe reported a missing terminator.
    #[test]
    fn a_quote_inside_a_command_stays_in_the_command() {
        let s = r#"run-cli 'powershell -Command "(Get-Content wk.txt | Where-Object { $_.Trim() -ne ' ' }).Count"' > count the non-blank lines"#;
        let tc = extract_for(s, arity).unwrap();
        assert_eq!(
            tc.raw_args,
            vec![r#"powershell -Command "(Get-Content wk.txt | Where-Object { $_.Trim() -ne ' ' }).Count""#.to_string()]
        );
        assert_eq!(tc.expectation, "count the non-blank lines");
    }

    /// `sessions/85`: `…\Output\local\raw\…` reached PowerShell with a
    /// carriage return where `\r` was. A command is taken as written.
    #[test]
    fn a_windows_path_keeps_its_backslashes() {
        let s = r#"<function=run-pwsh> 'Get-ChildItem -Path "C:\Pograms\Output\local\raw\2026-09-05" -Filter "*.ldb"' > the metadata files"#;
        let tc = extract_for(s, arity).unwrap();
        assert_eq!(
            tc.raw_args,
            vec![r#"Get-ChildItem -Path "C:\Pograms\Output\local\raw\2026-09-05" -Filter "*.ldb""#.to_string()]
        );
        let tc = extract_for(r#"read-file '\\server\share\new\a.txt' > the file"#, arity).unwrap();
        assert_eq!(tc.raw_args, vec![r"\\server\share\new\a.txt".to_string()]);
        let tc = extract_for(r#"run-pwsh '$a -split "\\|"' > the parts"#, arity).unwrap();
        assert_eq!(tc.raw_args, vec![r#"$a -split "\\|""#.to_string()], "a regex keeps `\\`");
    }

    #[test]
    fn apostrophes_inside_a_one_argument_call() {
        let tc = extract_for(r#"run-pwsh 'Write-Output "it's Ann's"' > the text"#, arity).unwrap();
        assert_eq!(tc.raw_args, vec![r#"Write-Output "it's Ann's""#.to_string()]);
        // An odd quote throws the first ` > ` reading out of phase; the call
        // still ends at the last ` > ` after a closing quote.
        let tc = extract_for(r#"run-pwsh '"don't" > out.txt' > write it"#, arity).unwrap();
        assert_eq!(tc.raw_args, vec![r#""don't" > out.txt"#.to_string()]);
        assert_eq!(tc.expectation, "write it");
        // A trailing backslash does not escape the closing quote.
        let tc = extract_for(r#"read-file 'C:\Users\me\' > list it"#, arity).unwrap();
        assert_eq!(tc.raw_args, vec![r"C:\Users\me\".to_string()]);
        // The documented escape still works.
        let tc = extract_for(r#"run-pwsh 'Write-Output \'hi\'' > greet"#, arity).unwrap();
        assert_eq!(tc.raw_args, vec!["Write-Output 'hi'".to_string()]);
    }

    #[test]
    fn extras_after_a_one_argument_call_stay_separate() {
        let tc = extract_for("run-cli 'cargo build' 'background=true' > start it", arity).unwrap();
        assert_eq!(tc.raw_args, vec!["cargo build".to_string(), "background=true".to_string()]);
        let tc = extract_for("read-file 'wk.txt' 'start=1 end=2' > two lines", arity).unwrap();
        assert_eq!(tc.raw_args, vec!["wk.txt".to_string(), "start=1 end=2".to_string()]);
        let tc = extract_for("read-file 'src/main.rs' '1' '80' > the top", arity).unwrap();
        assert_eq!(tc.raw_args, vec!["src/main.rs".to_string(), "1".into(), "80".into()]);
        // Peeled even when the argument has quotes of its own.
        let tc = extract_for(r#"run-pwsh 'Get-Item 'a b'' 'cwd=C:\x' > the item"#, arity).unwrap();
        assert_eq!(tc.raw_args, vec!["Get-Item 'a b'".to_string(), r"cwd=C:\x".to_string()]);
    }

    #[test]
    fn a_multi_argument_call_keeps_its_escapes() {
        let tc = extract_for(r#"write-file 'a.txt' 'x\ny' > confirm"#, arity).unwrap();
        assert_eq!(tc.raw_args, vec!["a.txt".to_string(), "x\ny".to_string()]);
    }

    /// `sessions/93`: Qwen's native form, refused twice before the turn hit
    /// max_tokens rewriting the same file.
    #[test]
    fn reads_qwen_parameter_tags() {
        let s = "I'll build it in chunks.\n\n<tool_call>\n<function=write-file>\n<parameter=name>\nwc_part1.txt\n</parameter>\n<parameter=content>\nline one\nline two\n</parameter>\n</function>\n</tool_call>";
        let tc = extract_for(s, arity).unwrap();
        assert_eq!(tc.skill, "write-file");
        assert_eq!(
            tc.named,
            vec![
                ("name".to_string(), "wc_part1.txt".to_string()),
                ("content".to_string(), "line one\nline two".to_string()),
            ]
        );
        assert_eq!(tc.raw_args, vec!["wc_part1.txt".to_string(), "line one\nline two".into()]);
        assert_eq!(tc.expectation, "");

        let s = "<function=read_file>\n<parameter=path>\na.md\n</parameter>\n<parameter=expectation>\nthe title\n</parameter>\n</function>";
        let tc = extract_for(s, arity).unwrap();
        assert_eq!(tc.skill, "read-file", "snake_case is this contract's kebab-case");
        assert_eq!(tc.named, vec![("path".to_string(), "a.md".to_string())]);
        assert_eq!(tc.expectation, "the title");

        // An unclosed value ends at the next tag.
        let s = "<function=read-file>\n<parameter=path>\na.md\n</function>";
        let tc = extract_for(s, arity).unwrap();
        assert_eq!(tc.named, vec![("path".to_string(), "a.md".to_string())]);
    }

    /// `sessions/94`: the wrapped call with no expectation, twice — the
    /// second refusal ended the turn.
    #[test]
    fn a_wrapped_call_needs_no_expectation() {
        let s = "I'll start by reading the file.\n\n<tool_call>\n<function=read-file> 'wk.txt'\n</function>";
        let tc = extract_for(s, arity).unwrap();
        assert_eq!(tc.skill, "read-file");
        assert_eq!(tc.raw_args, vec!["wk.txt".to_string()]);
        assert_eq!(tc.expectation, "");
        // No arguments at all.
        let tc = extract_for("<tool_call>\n<function=job-list>\n</function>\n</tool_call>", arity).unwrap();
        assert_eq!(tc.skill, "job-list");
        assert!(tc.raw_args.is_empty());
        // The strict entry point still refuses it.
        assert!(extract_known(s, |n| arity(n).is_some()).is_none());
    }

    /// Qwen 3.6, 2026-09-27: the argument on the line below the tag,
    /// unquoted, no expectation — refused twice, and the turn ended.
    #[test]
    fn a_function_block_body_is_the_call() {
        let s = "I'll start by reading the `wk.txt` file.\n\n<tool_call>\n<function=read-file>\nwk.txt\n</function>\n</tool_call>";
        let tc = extract_for(s, arity).unwrap();
        assert_eq!(tc.skill, "read-file");
        assert_eq!(tc.raw_args, vec!["wk.txt".to_string()]);
        // Quoted, with an expectation, on the next line.
        let s = "<tool_call>\n<function=read-file>\n'wk.txt' > the structure\n</function>\n</tool_call>";
        let tc = extract_for(s, arity).unwrap();
        assert_eq!(tc.raw_args, vec!["wk.txt".to_string()]);
        assert_eq!(tc.expectation, "the structure");
        // A script over several lines is one command.
        let s = "<function=run-pwsh>\n$a = Get-Content wk.txt\n$a.Count\n</function>";
        let tc = extract_for(s, arity).unwrap();
        assert_eq!(tc.raw_args, vec!["$a = Get-Content wk.txt\n$a.Count".to_string()]);
        // A tag named in passing, never closed, is prose.
        assert!(extract_for("I would use <function=read-file> for that", arity).is_none());
    }

    #[test]
    fn a_trailing_call_needs_no_expectation_but_prose_is_not_a_call() {
        let tc = extract_for("Let me look.\n\nread-file 'wk.txt'", arity).unwrap();
        assert_eq!(tc.raw_args, vec!["wk.txt".to_string()]);
        // Mid-reply, followed by prose: the model may be quoting it.
        let s = "read-file 'wk.txt'\nThen I will edit it.";
        assert!(extract_for(s, arity).is_none());
        assert!(rejected_attempt(s, |n| arity(n).is_some()).unwrap().contains("expectation"));
        assert!(extract_for("The tool you want:\nread-file is the skill", arity).is_none());
        assert!(extract_for("read-file 'a' is the call", arity).is_none(), "a bare word is prose");
        assert!(extract_for("frobnicate 'a.md'", arity).is_none(), "unknown skill");
    }

    #[test]
    fn the_last_drafted_call_is_found_in_reasoning() {
        let known = |n: &str| arity(n).is_some();
        let r = "Plan: read it first.\nread-file 'a.txt' > first look\nNo wait, the whole thing.\n`run-pwsh 'Get-Content a.txt' > all of it`\nOK let me send that. Hmm, and";
        assert_eq!(last_drafted_call(r, known).as_deref(), Some("run-pwsh 'Get-Content a.txt' > all of it"));
        assert_eq!(last_drafted_call("just thinking, no calls", known), None);
        assert_eq!(last_drafted_call("frobnicate 'x' > y", known), None);
    }
}
