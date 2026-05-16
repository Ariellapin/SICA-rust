use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::parse_tool_call::ToolCall;
use crate::skill::Skill;

#[derive(Default, Clone)]
pub struct SkillRegistry {
    pub by_name: HashMap<String, Arc<dyn Skill>>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, skill: Arc<dyn Skill>) {
        self.by_name.insert(skill.name().to_string(), skill);
    }

    /// Register `skill` only if no skill is already bound to its name. Used
    /// when loading user-authored markdown skills so they don't shadow the
    /// built-ins they're meant to document (the seeded `skills/run-cli.md`
    /// is documentation for the real `RunCli` skill, not a replacement).
    pub fn register_if_absent(&mut self, skill: Arc<dyn Skill>) -> bool {
        let name = skill.name().to_string();
        if self.by_name.contains_key(&name) {
            false
        } else {
            self.by_name.insert(name, skill);
            true
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Skill>> {
        self.by_name.get(name).cloned()
    }

    /// Render the live registry as a deterministic Markdown bullet list, sorted
    /// by skill name. Each line is `- **name** ('arg1' 'arg2') — description`,
    /// where the args section is omitted for skills that take none. Used by the
    /// backend to enumerate user-authored skills into the system prompt — the
    /// static `memory.md` only names the built-ins, so without this the LLM
    /// has no way to discover any skill the user has dropped into `skills/`.
    pub fn catalogue_markdown(&self) -> String {
        let mut names: Vec<&str> = self.by_name.keys().map(String::as_str).collect();
        names.sort_unstable();
        let mut out = String::new();
        for name in names {
            let Some(skill) = self.by_name.get(name) else { continue };
            out.push_str("- **");
            out.push_str(skill.name());
            out.push_str("**");
            let args = skill.positional_args();
            if !args.is_empty() {
                out.push_str(" (");
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    out.push('\'');
                    out.push('<');
                    out.push_str(a);
                    out.push('>');
                    out.push('\'');
                }
                out.push(')');
            }
            let desc = skill.description();
            if !desc.is_empty() {
                out.push_str(" — ");
                out.push_str(desc);
            }
            out.push('\n');
        }
        out
    }

    /// Resolve a parsed `ToolCall` to its skill handle and the JSON `args`
    /// object to pass into `Skill::run`. Returns `None` if no skill with that
    /// name is registered.
    ///
    /// Two paths:
    /// - `args_json: Some(_)` — the call came from a JSON-fenced shape; the
    ///   args object is forwarded verbatim (the model already named each arg).
    /// - `args_json: None` — natural-language shape; positional values are
    ///   zipped onto the skill's declared `positional_args()` to form the
    ///   object. Extra positional values past the declared list are silently
    ///   dropped; missing trailing args become absent JSON keys.
    pub fn resolve(&self, call: &ToolCall) -> Option<(Arc<dyn Skill>, Value)> {
        let skill = self.get(&call.skill)?;
        if let Some(json) = &call.args_json {
            return Some((skill, json.clone()));
        }
        let names = skill.positional_args();
        let mut obj = Map::new();
        for (name, val) in names.iter().zip(call.raw_args.iter()) {
            obj.insert(name.clone(), Value::String(val.clone()));
        }
        Some((skill, Value::Object(obj)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_tool_call::ToolCall;
    use crate::skill::{SkillContext, SkillOutcome};
    use async_trait::async_trait;

    struct Tk;
    #[async_trait]
    impl Skill for Tk {
        fn name(&self) -> &str { "tk" }
        fn positional_args(&self) -> Vec<String> { vec!["path".into(), "content".into()] }
        async fn run(&self, _a: Value, _c: SkillContext) -> SkillOutcome {
            SkillOutcome { ok: true, summary: String::new() }
        }
    }

    #[test]
    fn resolve_maps_positionals_to_names() {
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(Tk));
        let call = ToolCall {
            skill: "tk".into(),
            raw_args: vec!["a.md".into(), "hi".into()],
            expectation: "ok".into(),
            args_json: None,
        };
        let (_, args) = reg.resolve(&call).unwrap();
        assert_eq!(args["path"], "a.md");
        assert_eq!(args["content"], "hi");
    }

    #[test]
    fn resolve_unknown_skill_is_none() {
        let reg = SkillRegistry::new();
        let call = ToolCall {
            skill: "nope".into(),
            raw_args: vec![],
            expectation: "".into(),
            args_json: None,
        };
        assert!(reg.resolve(&call).is_none());
    }

    #[test]
    fn register_if_absent_does_not_clobber() {
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(Tk));
        let inserted = reg.register_if_absent(Arc::new(Tk));
        assert!(!inserted);
        assert_eq!(reg.by_name.len(), 1);
    }

    struct Described;
    #[async_trait]
    impl Skill for Described {
        fn name(&self) -> &str { "fetch" }
        fn description(&self) -> &str { "Grab a URL and return the body." }
        fn positional_args(&self) -> Vec<String> { vec!["url".into()] }
        async fn run(&self, _a: Value, _c: SkillContext) -> SkillOutcome {
            SkillOutcome { ok: true, summary: String::new() }
        }
    }

    struct Bare;
    #[async_trait]
    impl Skill for Bare {
        fn name(&self) -> &str { "noop" }
        async fn run(&self, _a: Value, _c: SkillContext) -> SkillOutcome {
            SkillOutcome { ok: true, summary: String::new() }
        }
    }

    #[test]
    fn catalogue_is_sorted_and_formats_args_and_description() {
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(Described));
        reg.register(Arc::new(Bare));
        reg.register(Arc::new(Tk));
        let md = reg.catalogue_markdown();
        let expected = "\
- **fetch** ('<url>') — Grab a URL and return the body.
- **noop**
- **tk** ('<path>' '<content>')
";
        assert_eq!(md, expected);
    }
}
