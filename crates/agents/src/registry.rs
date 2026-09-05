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

    /// A cheap dispatch view: this registry minus the named skills. Used
    /// for teammates, which must never reach harness controls (`ask-user`,
    /// `todo-write`, `exit-plan-mode`) or spawn their own team — anything
    /// they need from those arrives via their final report instead. The
    /// `Arc`s are shared, so filtering costs a map clone, not new skills.
    pub fn excluding(&self, names: &[&str]) -> Self {
        let mut out = Self::new();
        for (name, skill) in &self.by_name {
            if !names.contains(&name.as_str()) {
                out.by_name.insert(name.clone(), skill.clone());
            }
        }
        out
    }

    /// The complement of [`excluding`](Self::excluding): this registry
    /// narrowed to `names`. Used by agent presets (`crate::preset`), whose
    /// frontmatter `skills:` list is an allow-list rather than a deny-list.
    /// Names that match nothing are ignored — the caller reports them.
    pub fn restricted_to(&self, names: &[&str]) -> Self {
        let mut out = Self::new();
        for name in names {
            if let Some(skill) = self.by_name.get(*name) {
                out.by_name.insert((*name).to_string(), skill.clone());
            }
        }
        out
    }

    /// Render the live registry as a deterministic Markdown bullet list, sorted
    /// by skill name. Each line is `- **name** ('arg1' 'arg2') — description`,
    /// where the args section is omitted for skills that take none. Used by the
    /// backend to enumerate user-authored skills into the system prompt — the
    /// static `memory.md` only names the built-ins, so without this the LLM
    /// has no way to discover any skill the user has dropped into `skills/`.
    pub fn catalogue_markdown(&self) -> String {
        self.catalogue_markdown_excluding(&[])
    }

    /// [`catalogue_markdown`](Self::catalogue_markdown) minus the named
    /// skills. Used by `agent-team` to hide itself from its own teammates:
    /// a teammate spawning another team is pure cost on a small model, and
    /// the nesting only unwinds when it hits the sub-agent depth limit.
    pub fn catalogue_markdown_excluding(&self, exclude: &[&str]) -> String {
        let mut names: Vec<&str> = self
            .by_name
            .keys()
            .map(String::as_str)
            .filter(|n| !exclude.contains(n))
            .collect();
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

    /// Render the registry as an OpenAI-native `tools` array for servers with
    /// tool-call support (vLLM `--enable-auto-tool-choice`, etc.). Every
    /// declared positional arg becomes a required string parameter — the
    /// same contract `resolve` applies to the text protocol. Optional args
    /// (`Skill::optional_args`) appear as non-required properties: named
    /// calls can pass them, but a native call is valid without them.
    pub fn tools_json(&self) -> serde_json::Value {
        let mut names: Vec<&str> = self.by_name.keys().map(String::as_str).collect();
        names.sort_unstable();
        let tools: Vec<serde_json::Value> = names
            .iter()
            .filter_map(|name| {
                let skill = self.by_name.get(*name)?;
                // A skill that carries its own schema wins: the synthesised
                // all-strings shape below is a convenience for the built-ins,
                // not a contract the provider has to be told.
                let parameters = match skill.parameters_schema() {
                    Some(schema) => schema,
                    None => {
                        let mut props = Map::new();
                        let args = skill.positional_args();
                        for a in args.iter().chain(&skill.optional_args()) {
                            props.insert(
                                a.clone(),
                                serde_json::json!({ "type": "string" }),
                            );
                        }
                        serde_json::json!({
                            "type": "object",
                            "properties": Value::Object(props),
                            "required": args,
                        })
                    }
                };
                Some(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": skill.name(),
                        "description": skill.description(),
                        "parameters": parameters,
                    },
                }))
            })
            .collect();
        Value::Array(tools)
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
    ///   object. A surplus positional of the form `key=value` binds to a
    ///   *declared* optional arg (`run-cli 'cargo build' 'background=true'`)
    ///   — that is the only way the natural-language form can reach one;
    ///   any other surplus value is dropped, and missing trailing args
    ///   become absent JSON keys.
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
        // Surplus positionals of the form `key=value` bind to *declared*
        // optional args, so the natural-language form can reach them at all
        // — `run-cli 'cargo build' 'background=true'`. Without this the
        // extra value was silently dropped and the call did something other
        // than what it said. Only names the skill declares are accepted, so
        // a command that merely contains `=` cannot become an argument.
        let optional = skill.optional_args();
        if !optional.is_empty() {
            for raw in call.raw_args.iter().skip(names.len()) {
                let Some((key, value)) = raw.split_once('=') else { continue };
                let key = key.trim();
                if optional.iter().any(|o| o == key) {
                    obj.insert(key.to_string(), Value::String(value.trim().to_string()));
                }
            }
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
    fn tools_json_declares_string_params() {
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(Described));
        let tools = reg.tools_json();
        let f = &tools[0]["function"];
        assert_eq!(f["name"], "fetch");
        assert_eq!(f["description"], "Grab a URL and return the body.");
        assert_eq!(f["parameters"]["properties"]["url"]["type"], "string");
        assert_eq!(f["parameters"]["required"][0], "url");
        assert_eq!(tools[0]["type"], "function");
    }

    struct WithOptional;
    #[async_trait]
    impl Skill for WithOptional {
        fn name(&self) -> &str { "read-file" }
        fn positional_args(&self) -> Vec<String> { vec!["path".into()] }
        fn optional_args(&self) -> Vec<String> { vec!["start".into(), "end".into()] }
        async fn run(&self, _a: Value, _c: SkillContext) -> SkillOutcome {
            SkillOutcome { ok: true, summary: String::new() }
        }
    }

    struct OwnSchema;
    #[async_trait]
    impl Skill for OwnSchema {
        fn name(&self) -> &str { "mcp__fs__search" }
        fn positional_args(&self) -> Vec<String> { vec!["query".into()] }
        fn optional_args(&self) -> Vec<String> { vec!["limit".into()] }
        fn parameters_schema(&self) -> Option<Value> {
            Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1 },
                },
                "required": ["query"],
                "additionalProperties": false,
            }))
        }
        async fn run(&self, _a: Value, _c: SkillContext) -> SkillOutcome {
            SkillOutcome { ok: true, summary: String::new() }
        }
    }

    #[test]
    fn a_skill_with_its_own_schema_sends_it_verbatim() {
        // An MCP tool's arguments are typed. Flattening `limit` to a string
        // the way the built-ins are flattened would make the tool uncallable
        // for any server that validates its own schema.
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(OwnSchema));
        let params = &reg.tools_json()[0]["function"]["parameters"];
        assert_eq!(params["properties"]["limit"]["type"], "integer");
        assert_eq!(params["properties"]["limit"]["minimum"], 1);
        assert_eq!(params["additionalProperties"], false);
        assert_eq!(params["required"], serde_json::json!(["query"]));
    }

    #[test]
    fn tools_json_lists_optional_args_without_requiring_them() {
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(WithOptional));
        let f = &reg.tools_json()[0]["function"];
        assert_eq!(f["parameters"]["properties"]["start"]["type"], "string");
        assert_eq!(f["parameters"]["properties"]["end"]["type"], "string");
        assert_eq!(f["parameters"]["required"], serde_json::json!(["path"]));
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

    #[test]
    fn catalogue_can_exclude_names() {
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(Described));
        reg.register(Arc::new(Bare));
        let md = reg.catalogue_markdown_excluding(&["fetch"]);
        assert_eq!(md, "- **noop**\n");
    }

    struct ShellLike;
    #[async_trait]
    impl Skill for ShellLike {
        fn name(&self) -> &str { "sh" }
        fn positional_args(&self) -> Vec<String> { vec!["command".into()] }
        fn optional_args(&self) -> Vec<String> { vec!["cwd".into(), "background".into()] }
        async fn run(&self, _a: Value, _c: SkillContext) -> SkillOutcome {
            SkillOutcome { ok: true, summary: String::new() }
        }
    }

    fn call(args: &[&str]) -> ToolCall {
        ToolCall {
            skill: "sh".into(),
            raw_args: args.iter().map(|s| s.to_string()).collect(),
            expectation: "ok".into(),
            args_json: None,
        }
    }

    #[test]
    fn surplus_key_value_positionals_bind_to_declared_optional_args() {
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(ShellLike));
        let (_, args) = reg
            .resolve(&call(&["cargo build", "background=true", "cwd=/tmp"]))
            .unwrap();
        assert_eq!(args["command"], "cargo build");
        assert_eq!(args["background"], "true");
        assert_eq!(args["cwd"], "/tmp");
    }

    #[test]
    fn a_command_containing_an_equals_sign_is_not_mistaken_for_an_argument() {
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(ShellLike));
        // Positional 1 is the command, whatever it contains; and a surplus
        // pair whose key the skill never declared is ignored rather than
        // invented.
        let (_, args) = reg
            .resolve(&call(&["FOO=bar make", "colour=always"]))
            .unwrap();
        assert_eq!(args["command"], "FOO=bar make");
        assert!(args.get("colour").is_none());
        assert!(args.get("FOO").is_none());
    }

    #[test]
    fn a_skill_with_no_optional_args_still_drops_surplus_positionals() {
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(Tk));
        let c = ToolCall {
            skill: "tk".into(),
            raw_args: vec!["a.md".into(), "hi".into(), "x=1".into()],
            expectation: String::new(),
            args_json: None,
        };
        let (_, args) = reg.resolve(&c).unwrap();
        assert_eq!(args.as_object().unwrap().len(), 2);
    }
}
