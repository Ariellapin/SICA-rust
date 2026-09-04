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
//! On `run`, the skill returns its body — framed as a `<skill_content>`
//! block naming the directory it came from, so relative resource paths in
//! the body resolve — as the outcome summary, so the caller (typically a
//! `ToolSubAgent`) can feed those instructions back into the LLM. The same
//! frame is what a typed `/name` injects (see `crate::invoke`). Skills with
//! malformed frontmatter are skipped at load time and surfaced as a warning
//! to the caller — the runtime itself does not crash on a bad file.

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
    /// Names of positional args declared in frontmatter (`positional:` key,
    /// comma- or whitespace-separated). Empty when the skill carries no
    /// positional inputs.
    pub positionals: Vec<String>,
}

#[async_trait]
impl Skill for MarkdownSkill {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn positional_args(&self) -> Vec<String> {
        self.positionals.clone()
    }

    /// A markdown skill's body *is* the instruction — framing it as
    /// untrusted data would tell the model to ignore it.
    fn trusted(&self) -> bool {
        true
    }

    /// Return the skill body as instructions, framed as `<skill_content>`.
    /// The body is interpolated first: the skill's declared positional args
    /// (by name) plus the standard variables (`{{cwd}}`, `{{os}}`,
    /// `{{date}}`, `{{model}}`). Strict, per the prompt module — a body
    /// referencing a valueless variable fails the call loudly instead of
    /// feeding the model a malformed template.
    async fn run(&self, args: Value, _ctx: SkillContext) -> SkillOutcome {
        let mut vars = crate::prompt::standard_vars("");
        for name in self.positional_args() {
            if let Some(v) = args.get(&name) {
                let s = match v {
                    Value::String(s) => s.clone(),
                    other            => other.to_string(),
                };
                vars.insert(name, s);
            }
        }
        let source = format!("skills/{}.md", self.name);
        let body = match crate::prompt::interpolate(&self.body, &vars, &source) {
            Ok(b)  => b,
            Err(e) => {
                return SkillOutcome {
                    ok:      false,
                    summary: format!("skill `{}` failed to render: {e}", self.name),
                }
            }
        };
        let rendered = MarkdownSkill { body, ..self.clone() }.render_skill_content();
        SkillOutcome { ok: true, summary: rendered }
    }
}

impl MarkdownSkill {
    /// The fixed frame a loaded skill is delivered in:
    ///
    /// ```text
    /// <skill_content name="…">
    /// <skill_resources>Base directory for this skill: …</skill_resources>
    /// <skill_instructions>
    /// …body…
    /// </skill_instructions>
    /// </skill_content>
    /// ```
    ///
    /// The base directory lets a skill refer to sibling files by relative
    /// path; `read-file` resolves against the workspace root, so the
    /// absolute directory is spelled out rather than assumed.
    pub fn render_skill_content(&self) -> String {
        let base = self
            .source_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| ".".into());
        format!(
            "<skill_content name=\"{}\">\n\
             <skill_resources>Base directory for this skill: {base}</skill_resources>\n\
             <skill_instructions>\n{}\n</skill_instructions>\n\
             </skill_content>",
            self.name,
            self.body.trim_end(),
        )
    }
}

/// Parse one markdown skill file. Same rules as [`load_dir`], for a single
/// path — used to resolve a typed `/name` against `commands/`, `agents/`
/// and `skills/` without scanning whole directories.
pub fn load_file(path: &Path) -> Result<MarkdownSkill, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
    parse(&text, path)
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

/// Register every skill produced by `load_dir`, **only** for names that
/// aren't already in the registry. The built-in `seed_defaults` writes
/// markdown contracts like `skills/run-cli.md` so the LLM can read them,
/// but the file body must not shadow the real `RunCli` Rust skill — doing
/// so would make `run-cli` invocations return their own documentation
/// instead of executing the command (the bug seen in `sessions/8.toml`).
///
/// Returns the parse-error list so the caller can decide how to surface
/// them (the BE forwards them as `LogLine` events).
pub fn register_all(registry: &mut SkillRegistry, dir: &Path) -> Vec<(PathBuf, String)> {
    let report = load_dir(dir);
    for s in report.loaded {
        registry.register_if_absent(Arc::new(s));
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
    let mut positionals = Vec::new();
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
                "positional"  => positionals = split_positionals(&v),
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
        positionals,
    })
}

/// Parse a `positional:` frontmatter value into an ordered name list. Accepts
/// either comma- or whitespace-separated forms (`"path, content"` and
/// `"path content"` both work). Empty entries are dropped.
fn split_positionals(v: &str) -> Vec<String> {
    v.split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
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
    async fn run_returns_framed_body() {
        let s = MarkdownSkill {
            name: "n".into(),
            description: "d".into(),
            body: "instructions\n".into(),
            source_path: PathBuf::from("skills").join("n.md"),
            positionals: Vec::new(),
        };
        let cap: Arc<dyn crate::agent::EventSink> = Arc::new(Sink);
        let sub = crate::ToolSubAgent::root(cap);
        let ctx = SkillContext { sub };
        let out = s.run(Value::Null, ctx).await;
        assert!(out.ok);
        assert!(s.trusted());
        assert_eq!(
            out.summary,
            "<skill_content name=\"n\">\n\
             <skill_resources>Base directory for this skill: skills</skill_resources>\n\
             <skill_instructions>\ninstructions\n</skill_instructions>\n\
             </skill_content>"
        );
    }

    #[tokio::test]
    async fn run_interpolates_positionals_and_standard_vars() {
        let s = MarkdownSkill {
            name: "weather".into(),
            description: "d".into(),
            body: "Report weather for {{city}} ({{units}}) on {{date}} in {{cwd}}.".into(),
            source_path: PathBuf::from("skills").join("weather.md"),
            positionals: vec!["city".into(), "units".into()],
        };
        let cap: Arc<dyn crate::agent::EventSink> = Arc::new(Sink);
        let ctx = SkillContext { sub: crate::ToolSubAgent::root(cap) };
        let out = s.run(
            serde_json::json!({ "city": "Paris", "units": "metric" }),
            ctx,
        ).await;
        assert!(out.ok, "{}", out.summary);
        assert!(out.summary.contains("Report weather for Paris (metric)"), "{}", out.summary);
        assert!(!out.summary.contains("{{date}}"), "standard vars resolve");
    }

    #[tokio::test]
    async fn run_fails_loudly_on_unknown_variable() {
        let s = MarkdownSkill {
            name: "broken".into(),
            description: "d".into(),
            body: "hello {{nobody}}".into(),
            source_path: PathBuf::from("skills").join("broken.md"),
            positionals: Vec::new(),
        };
        let cap: Arc<dyn crate::agent::EventSink> = Arc::new(Sink);
        let ctx = SkillContext { sub: crate::ToolSubAgent::root(cap) };
        let out = s.run(Value::Null, ctx).await;
        assert!(!out.ok);
        assert!(out.summary.contains("{{nobody}}"), "{}", out.summary);
    }

    #[test]
    fn load_file_reads_one_skill() {
        let dir = std::env::temp_dir().join(format!("sica-md-skill-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("one.md");
        std::fs::write(&path, "---\nname: one\n---\nbody\n").unwrap();
        let s = load_file(&path).unwrap();
        assert_eq!(s.name, "one");
        assert_eq!(s.source_path, path);
        assert!(load_file(&dir.join("missing.md")).unwrap_err().starts_with("read:"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn frontmatter_carries_positional_args() {
        let text = "---\nname: x\npositional: city, units\n---\nbody\n";
        let s = parse(text, &dummy()).unwrap();
        assert_eq!(s.positionals, vec!["city".to_string(), "units".to_string()]);
        assert_eq!(s.positional_args(), vec!["city".to_string(), "units".to_string()]);
    }

    struct Sink;
    impl crate::agent::EventSink for Sink {
        fn emit(&self, _ev: protocol::Event) {}
    }
}
