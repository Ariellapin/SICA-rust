//! Agent runtime: main agent that drives chat turns, sub-agents that wrap each
//! tool call and may themselves spawn further sub-agents.

pub mod agent;
pub mod builtins;
pub mod compact;
pub mod context;
pub mod guard;
pub mod instructions;
pub mod invoke;
pub mod md_skill;
pub mod memory;
pub mod meter;
pub mod model_eval;
pub mod parse_tool_call;
pub mod proc;
pub mod prompt;
pub mod registry;
pub mod skill;
pub mod skill_creator;
pub mod spill;
pub mod subagent;
pub mod team;
pub mod turn;

pub use agent::{EventSink, MainAgent};
pub use builtins::{EditFile, Glob, Grep, ReadFile, RunCli, RunPwsh, WriteFile};
pub use md_skill::{LoadReport, MarkdownSkill};
pub use model_eval::ModelEval;
pub use parse_tool_call::{
    extract as extract_tool_call, extract_known as extract_tool_call_known, ToolCall,
};
pub use registry::SkillRegistry;
pub use skill::{Skill, SkillContext, SkillOutcome};
pub use skill_creator::SkillCreator;
pub use subagent::{ToolFailureReport, ToolFailureSink, ToolInvocation, ToolSubAgent};
pub use team::AgentTeam;
