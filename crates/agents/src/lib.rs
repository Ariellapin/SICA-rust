//! Agent runtime: main agent that drives chat turns, sub-agents that wrap each
//! tool call and may themselves spawn further sub-agents.

pub mod agent;
pub mod registry;
pub mod skill;
pub mod subagent;
pub mod turn;

pub use agent::{EventSink, MainAgent};
pub use registry::SkillRegistry;
pub use skill::{Skill, SkillContext, SkillOutcome};
pub use subagent::ToolSubAgent;
