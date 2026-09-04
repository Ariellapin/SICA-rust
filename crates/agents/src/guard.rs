//! Loop guards that watch the tool-call stream and *advise* the model.
//!
//! [`RepeatTracker`] is the repeat-tool reminder: a per-session chain of
//! `(skill, canonical args)` counting consecutive identical calls. At each
//! of [`THRESHOLDS`] it hands back a notice for the caller to inject as
//! context — never a block. A model that hammers the same failing call is
//! exactly the loop worth breaking, so failed and denied calls count too.
//! A new user message clears the chain.
//!
//! The key canonicalises the JSON arguments with object keys sorted
//! recursively, so `{"path":"a","cwd":"."}` and `{"cwd":".","path":"a"}`
//! are one call.

use serde_json::Value;

/// Consecutive-call counts at which a notice is issued. The first is a
/// gentle heads-up; the later ones repeat it with the count.
pub const THRESHOLDS: [u32; 3] = [3, 5, 8];
/// Longest argument preview quoted in a notice.
pub const PREVIEW_CHARS: usize = 500;

/// State for one session.
#[derive(Debug, Default, Clone)]
pub struct RepeatTracker {
    key: String,
    count: u32,
}

impl RepeatTracker {
    /// Record one dispatched call and return the advisory notice when the
    /// run of identical calls hits a threshold.
    pub fn observe(&mut self, skill: &str, args: &Value) -> Option<String> {
        let key = format!("{skill}\u{0}{}", canonical_json(args));
        if key == self.key {
            self.count += 1;
        } else {
            self.key = key;
            self.count = 1;
        }
        if !THRESHOLDS.contains(&self.count) {
            return None;
        }
        let full_preview = args_preview(args);
        let preview = sica_core::retain::utf8_head(&full_preview, PREVIEW_CHARS);
        Some(format!(
            "You have now called `{skill}` {} times in a row with identical arguments: \
             `{preview}`. Repeating the same call has produced the same result each time. \
             Change the arguments, use a different tool, or explain to the user why you \
             are stuck.",
            self.count
        ))
    }

    /// A new user message starts a fresh chain.
    pub fn reset(&mut self) {
        self.key.clear();
        self.count = 0;
    }

    pub fn count(&self) -> u32 {
        self.count
    }
}

/// Deterministic JSON text: object keys sorted at every level.
pub fn canonical_json(v: &Value) -> String {
    fn sort(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut out = serde_json::Map::new();
                for k in keys {
                    out.insert(k.clone(), sort(&m[k]));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
    sort(v).to_string()
}

/// One-line rendering of the arguments for the notice: an object's values
/// in key order, quoted — the same shape the model itself writes.
fn args_preview(args: &Value) -> String {
    match args {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            keys.iter()
                .map(|k| match &m[*k] {
                    Value::String(s) => format!("'{}'", s.replace('\n', "\\n")),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join(" ")
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identical_args_in_different_key_order_collide() {
        let mut t = RepeatTracker::default();
        assert!(t.observe("read-file", &json!({"path": "a", "cwd": "."})).is_none());
        assert!(t.observe("read-file", &json!({"cwd": ".", "path": "a"})).is_none());
        let n = t.observe("read-file", &json!({"path": "a", "cwd": "."})).expect("third call");
        assert!(n.contains("`read-file` 3 times"), "{n}");
        assert!(n.contains("'.' 'a'"), "{n}");
        assert_eq!(t.count(), 3);
    }

    #[test]
    fn different_args_or_skill_restart_the_chain() {
        let mut t = RepeatTracker::default();
        for _ in 0..2 {
            t.observe("run-cli", &json!({"command": "dir"}));
        }
        assert!(t.observe("run-cli", &json!({"command": "dir /w"})).is_none());
        assert_eq!(t.count(), 1);
        t.observe("run-pwsh", &json!({"command": "dir /w"}));
        assert_eq!(t.count(), 1);
    }

    #[test]
    fn notices_fire_exactly_at_thresholds_and_reset_clears() {
        let mut t = RepeatTracker::default();
        let mut fired = Vec::new();
        for i in 1..=10 {
            if t.observe("x", &Value::Null).is_some() {
                fired.push(i);
            }
        }
        assert_eq!(fired, THRESHOLDS.to_vec());
        t.reset();
        assert_eq!(t.count(), 0);
        assert!(t.observe("x", &Value::Null).is_none());
        assert_eq!(t.count(), 1);
    }

    #[test]
    fn preview_is_capped() {
        let mut t = RepeatTracker::default();
        let long = json!({"content": "y".repeat(5000)});
        for _ in 0..2 {
            t.observe("write-file", &long);
        }
        let n = t.observe("write-file", &long).unwrap();
        assert!(n.len() < 5000);
    }

    #[test]
    fn canonical_json_sorts_nested_keys() {
        let v = json!({"b": {"z": 1, "a": [ {"y": 2, "x": 1} ]}, "a": 0});
        assert_eq!(canonical_json(&v), r#"{"a":0,"b":{"a":[{"x":1,"y":2}],"z":1}}"#);
    }
}
