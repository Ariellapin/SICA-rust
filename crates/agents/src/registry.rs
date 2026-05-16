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

    /// Map a parsed `ToolCall`'s positional values onto the skill's declared
    /// arg names and return the JSON `args` object the skill expects, along
    /// with the resolved skill handle. Returns `None` if no skill with that
    /// name is registered. Extra positional values past the declared list are
    /// silently dropped; missing trailing args become absent JSON keys.
    pub fn resolve(&self, call: &ToolCall) -> Option<(Arc<dyn Skill>, Value)> {
        let skill = self.get(&call.skill)?;
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
}
