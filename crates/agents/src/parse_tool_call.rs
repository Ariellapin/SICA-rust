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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub skill:       String,
    pub raw_args:    Vec<String>,
    pub expectation: String,
}

/// Return the first plausible tool-call line in `text`, if any.
///
/// "Plausible" = the line splits on the first lone ` > ` separator (a `>`
/// surrounded by ASCII whitespace) into a left side starting with a
/// `[a-z][a-z0-9-]*` skill identifier, followed by zero or more quoted
/// positional arguments. The right side becomes the expectation.
pub fn extract(text: &str) -> Option<ToolCall> {
    for line in text.lines() {
        let trimmed = strip_fence_indent(line);
        if let Some(tc) = parse_line(trimmed) {
            return Some(tc);
        }
    }
    None
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
}
