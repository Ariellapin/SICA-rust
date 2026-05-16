//! Filesystem-backed skills loaded from `skills/*.md`.
//!
//! Each file has a tiny YAML-style frontmatter block:
//!
//! ```text
//! ---
//! name: my-skill
//! description: One-line summary of what this skill does.
//! ---
//! Body of the skill — instructions / prompt content the agent should
//! follow when this skill fires.
//! ```
//!
//! On `run`, the skill returns its body as the outcome summary, so the
//! caller (typically a `ToolSubAgent`) can feed those instructions back
//! into the LLM. Skills with malformed frontmatter are skipped at load
//! time and surfaced as a warning to the caller — the runtime itself
//! does not crash on a bad file.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::registry::SkillRegistry;
use crate::skill::{Skill, SkillContext, SkillOutcome};

/// One parsed `*.md` skill.
#[derive(Debug, Clone)]
pub struct MarkdownSkill {
    pub name:        String,
    pub description: String,
    pub body:        String,
    pub source_path: PathBuf,
}

#[async_trait]
impl Skill for MarkdownSkill {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, _args: Value, _ctx: SkillContext) -> SkillOutcome {
        SkillOutcome {
            ok:      true,
            summary: self.body.clone(),
        }
    }
}

/// Outcome of a `load_dir` call. Skills that parsed successfully are in
/// `loaded`; per-file parse errors are surfaced in `errors` so the BE
/// can log them without losing the rest.
#[derive(Debug, Default)]
pub struct LoadReport {
    pub loaded: Vec<MarkdownSkill>,
    pub errors: Vec<(PathBuf, String)>,
}

/// Scan `dir` (non-recursive) for `*.md` files, parse each, and return
/// the discovered skills + any per-file errors. Missing directory is
/// not an error — it returns an empty report.
pub fn load_dir(dir: &Path) -> LoadReport {
    let mut report = LoadReport::default();
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return report,
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        match fs::read_to_string(&path) {
            Ok(text) => match parse(&text, &path) {
                Ok(skill) => report.loaded.push(skill),
                Err(e)    => report.errors.push((path, e)),
            },
            Err(e) => report.errors.push((path, format!("read: {e}"))),
        }
    }
    report
}

/// Register every skill produced by `load_dir`. Returns the parse-error
/// list so the caller can decide how to surface them (the BE forwards
/// them as `LogLine` events).
pub fn register_all(registry: &mut SkillRegistry, dir: &Path) -> Vec<(PathBuf, String)> {
    let report = load_dir(dir);
    for s in report.loaded {
        registry.register(Arc::new(s));
    }
    report.errors
}

/// Parse one MD file body into a `MarkdownSkill`. Expects the first
/// non-blank line to be `---`, a key/value block, then a closing `---`,
/// then the body. Unknown frontmatter keys are ignored.
fn parse(text: &str, source: &Path) -> Result<MarkdownSkill, String> {
    let mut lines = text.lines().peekable();

    // Skip a leading UTF-8 BOM if present.
    let first = match lines.peek().copied() {
        Some(l) => l.trim_start_matches('\u{feff}'),
        None    => return Err("empty file".into()),
    };
    if first.trim() != "---" {
        return Err("missing leading `---` frontmatter delimiter".into());
    }
    let _ = lines.next();

    let mut name        = String::new();
    let mut description = String::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        if let Some((k, v)) = split_kv(line) {
            match k.as_str() {
                "name"        => name        = v,
                "description" => description = v,
                _ => {}
            }
        }
    }
    if !closed {
        return Err("missing closing `---` frontmatter delimiter".into());
    }
    if name.is_empty() {
        return Err("frontmatter missing required `name:` field".into());
    }

    let body: String = lines.collect::<Vec<_>>().join("\n");
    Ok(MarkdownSkill {
        name,
        description,
        body: body.trim_start_matches('\n').to_string(),
        source_path: source.to_path_buf(),
    })
}

fn split_kv(line: &str) -> Option<(String, String)> {
    let (k, v) = line.split_once(':')?;
    let k = k.trim().to_string();
    let v = v.trim().trim_matches(|c: char| c == '"' || c == '\'').to_string();
    if k.is_empty() {
        return None;
    }
    Some((k, v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dummy() -> PathBuf { PathBuf::from("test.md") }

    #[test]
    fn parses_well_formed() {
        let text = "---\nname: hello\ndescription: says hi\n---\nbody line 1\nbody line 2\n";
        let s = parse(text, &dummy()).unwrap();
        assert_eq!(s.name, "hello");
        assert_eq!(s.description, "says hi");
        assert!(s.body.starts_with("body line 1"));
    }

    #[test]
    fn rejects_missing_frontmatter() {
        let err = parse("body only", &dummy()).unwrap_err();
        assert!(err.contains("missing leading"));
    }

    #[test]
    fn rejects_missing_name() {
        let text = "---\ndescription: nameless\n---\nbody\n";
        let err = parse(text, &dummy()).unwrap_err();
        assert!(err.contains("name"));
    }

    #[test]
    fn tolerates_quoted_values_and_bom() {
        let text = "\u{feff}---\nname: \"with-bom\"\ndescription: 'quoted'\n---\nx\n";
        let s = parse(text, &dummy()).unwrap();
        assert_eq!(s.name, "with-bom");
        assert_eq!(s.description, "quoted");
    }

    #[tokio::test]
    async fn run_returns_body() {
        let s = MarkdownSkill {
            name: "n".into(),
            description: "d".into(),
            body: "instructions".into(),
            source_path: dummy(),
        };
        let cap: Arc<dyn crate::agent::EventSink> = Arc::new(Sink);
        let sub = crate::ToolSubAgent::root(cap);
        let ctx = SkillContext { sub };
        let out = s.run(Value::Null, ctx).await;
        assert!(out.ok);
        assert_eq!(out.summary, "instructions");
    }

    struct Sink;
    impl crate::agent::EventSink for Sink {
        fn emit(&self, _ev: protocol::Event) {}
    }
}
