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
    async fn run(&self, args: Value, ctx: SkillContext) -> SkillOutcome;
}
