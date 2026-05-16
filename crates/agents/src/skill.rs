use async_trait::async_trait;
use serde_json::Value;

use crate::subagent::ToolSubAgent;

#[derive(Debug)]
pub struct SkillOutcome {
    pub ok:      bool,
    pub summary: String,
}

/// Context handed to a skill while it runs. Carries a `ToolSubAgent` configured
/// as a *child* of the current call so nested tool invocations inherit the
/// parent chain and depth.
pub struct SkillContext {
    pub sub: ToolSubAgent,
}

#[async_trait]
pub trait Skill: Send + Sync {
    fn name(&self) -> &str;

    /// Ordered names of the positional arguments this skill accepts in the
    /// natural-language tool-call form. The parser hands the dispatcher a
    /// `ToolCall` carrying a `Vec<String>` of raw positional values; the
    /// dispatcher uses these names to assemble the JSON `args` object passed
    /// to `run`.
    ///
    /// Returns owned `String`s so dynamic skills (e.g. `MarkdownSkill`) can
    /// declare their args from frontmatter. Default `vec![]`: skill takes no
    /// positional args (any supplied positional values are dropped — usually
    /// a sign the call was malformed).
    fn positional_args(&self) -> Vec<String> {
        Vec::new()
    }

    async fn run(&self, args: Value, ctx: SkillContext) -> SkillOutcome;
}
