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

// `Eq` is not derived because `serde_json::Value` only implements `PartialEq`
// (`f64` inside `Value::Number` rules out total equality).
#[derive(Debug, Clone, PartialEq)]
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
    if let Some(tc) = extract_json_fence(text) {
        return Some(tc);
    }
    for line in text.lines() {
        let trimmed = strip_fence_indent(line);
        if let Some(tc) = parse_line(trimmed) {
            return Some(tc);
        }
    }
    None
}

/// Scan `text` for the first ```tool_call``` fenced block and parse its body
/// as JSON. Returns `None` if no such fence exists, the JSON is malformed,
/// or the required `skill` / `args` keys are missing.
fn extract_json_fence(text: &str) -> Option<ToolCall> {
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
        let body = &after_lang[..close_idx];
        return parse_json_body(body);
    }
    None
}

fn parse_json_body(body: &str) -> Option<ToolCall> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let obj = value.as_object()?;
    let skill = obj.get("skill")?.as_str()?.to_string();
    if !is_valid_skill_name(&skill) {
        return None;
    }
    let args = obj.get("args").cloned().unwrap_or(serde_json::Value::Null);
    let expectation = obj
        .get("expectation")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Populate `raw_args` from the args object's *values* (in insertion
    // order — serde_json's Map preserves it) so the UI's `args_preview`
    // chip still shows something useful even though the dispatcher will
    // route via `args_json` instead.
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
    Some(ToolCall {
        skill,
        raw_args,
        expectation,
        args_json: Some(args),
    })
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
        args_json: None,
    })
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
    fn ignores_other_fenced_blocks() {
        // A plain ```json block is NOT a tool call.
        let s = "```json\n{ \"skill\": \"run-cli\", \"args\": {} }\n```";
        assert!(extract(s).is_none());
    }
}
