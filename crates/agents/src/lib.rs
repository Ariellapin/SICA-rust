//! Agent runtime: main agent that drives chat turns, sub-agents that wrap each
//! tool call and may themselves spawn further sub-agents.

pub mod agent;
pub mod builtins;
pub mod md_skill;
pub mod memory;
pub mod parse_tool_call;
pub mod registry;
pub mod skill;
pub mod skill_creator;
pub mod subagent;
pub mod turn;

pub use agent::{EventSink, MainAgent};
pub use builtins::{ReadFile, RunCli, RunPwsh, WriteFile};
pub use md_skill::{LoadReport, MarkdownSkill};
pub use parse_tool_call::{extract as extract_tool_call, ToolCall};
pub use registry::SkillRegistry;
pub use skill::{Skill, SkillContext, SkillOutcome};
pub use skill_creator::SkillCreator;
pub use subagent::{ToolFailureReport, ToolFailureSink, ToolInvocation, ToolSubAgent};
