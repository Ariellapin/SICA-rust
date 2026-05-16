//! Extract a `tool_call` fenced block from an assistant message. The on-wire
//! contract (see `memory.md` SEED) is:
//!
//! ```text
//!     ```tool_call
//!     { "skill": "<name>", "args": { ... } }
//!     ```
//! ```
//!
//! The model occasionally produces leading whitespace or extra prose around
//! the block, so the scanner walks line-by-line and accepts any indentation.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub skill: String,
    pub args:  Value,
}

/// Return the first `tool_call` block in `text`, if any. The JSON inside the
/// fence must parse and have a string `skill` field; `args` defaults to
/// `Value::Null` when absent so skills can decide what missing means.
pub fn extract(text: &str) -> Option<ToolCall> {
    let mut lines = text.lines().enumerate();
    let mut start_body: Option<usize> = None;
    while let Some((i, line)) = lines.next() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("```") {
            if rest.trim() == "tool_call" {
                start_body = Some(i + 1);
                break;
            }
        }
    }
    let start = start_body?;
    let mut end = None;
    for (j, line) in text.lines().enumerate().skip(start) {
        if line.trim_start().starts_with("```") {
            end = Some(j);
            break;
        }
    }
    let body_lines: Vec<&str> = text
        .lines()
        .skip(start)
        .take(end? - start)
        .collect();
    let body = body_lines.join("\n");
    let v: Value = serde_json::from_str(&body).ok()?;
    let skill = v.get("skill")?.as_str()?.to_string();
    let args = v.get("args").cloned().unwrap_or(Value::Null);
    Some(ToolCall { skill, args })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_basic_block() {
        let s = "Sure, let me check.\n\n```tool_call\n{ \"skill\": \"run-cli\", \"args\": { \"command\": \"echo hi\" } }\n```\n";
        let tc = extract(s).unwrap();
        assert_eq!(tc.skill, "run-cli");
        assert_eq!(tc.args, json!({ "command": "echo hi" }));
    }

    #[test]
    fn extracts_indented_block() {
        let s = "    ```tool_call\n    { \"skill\": \"read-file\", \"args\": { \"path\": \"a.txt\" } }\n    ```\n";
        // Indented JSON body still parses because serde_json tolerates the
        // leading whitespace on each line.
        let tc = extract(s).unwrap();
        assert_eq!(tc.skill, "read-file");
    }

    #[test]
    fn no_block_returns_none() {
        assert!(extract("just a normal answer, no tools").is_none());
    }

    #[test]
    fn malformed_json_returns_none() {
        let s = "```tool_call\nnot json at all\n```";
        assert!(extract(s).is_none());
    }

    #[test]
    fn missing_skill_returns_none() {
        let s = "```tool_call\n{ \"args\": {} }\n```";
        assert!(extract(s).is_none());
    }

    #[test]
    fn missing_args_defaults_null() {
        let s = "```tool_call\n{ \"skill\": \"ping\" }\n```";
        let tc = extract(s).unwrap();
        assert_eq!(tc.args, Value::Null);
    }
}
