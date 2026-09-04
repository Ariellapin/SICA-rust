use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::subagent::ToolSubAgent;

/// Pipeline-level wall-clock limit applied to every skill that does not
/// override [`Skill::timeout`]. Long enough for a slow local model to answer
/// a summariser round-trip, short enough that a hung child process or a
/// skill stuck on I/O cannot pin the turn open indefinitely.
pub const DEFAULT_SKILL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub struct SkillOutcome {
    pub ok:      bool,
    pub summary: String,
}

/// Per-call scheduling class for native multi-call batches (guide §6.2).
/// `Parallel` calls overlap in a bounded pool; `Exclusive` calls are
/// ordering barriers. Classification is per-call from args only and
/// fail-closed: when in doubt, a skill is `Exclusive`. The text protocol
/// emits one call per hop, so this only affects native mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Concurrency {
    Exclusive,
    Parallel,
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

    /// One-line human-readable summary used in the system prompt's loaded-
    /// skill listing. Should fit on a single line; the registry concatenates
    /// it directly into a Markdown bullet. Default `""` keeps test doubles
    /// terse — production skills override.
    fn description(&self) -> &str {
        ""
    }

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

    /// Ordered names of *optional* named arguments. They appear in the
    /// native `tools` schema as non-required properties and are honoured in
    /// JSON-fenced / native calls, but the natural-language positional form
    /// can never reach them — that stays `positional_args()` territory.
    /// Default `vec![]`.
    fn optional_args(&self) -> Vec<String> {
        Vec::new()
    }

    /// Wall-clock budget `ToolSubAgent` enforces around `run`. When it
    /// elapses the skill future is dropped (a child process survives only
    /// if the skill spawned it without `kill_on_drop`) and the call is
    /// reported as a failed outcome the model can read. Never visible to
    /// the model. Skills that legitimately run for minutes — anything that
    /// drives its own LLM conversations — must override this.
    fn timeout(&self) -> Duration {
        DEFAULT_SKILL_TIMEOUT
    }

    /// Whether this skill's output is *instructions* the model should
    /// follow (`true`) or *data* it fetched from somewhere — a file, a
    /// command, the web — that may contain text posing as instructions
    /// (`false`). Untrusted results are framed with
    /// `sica_core::event::UNTRUSTED_NOTICE` in the derived history. The
    /// default is the safe one; only a skill whose body *is* the
    /// instruction (a markdown skill) should return `true`.
    fn trusted(&self) -> bool {
        false
    }

    /// One-sentence usage guidance composed into the system prompt (the
    /// `SKILL_GUIDANCE` slot), never into a persona. Tool usage rules live
    /// with the tool — a new skill brings its own sentence instead of
    /// editing a central blob. `None` (the default) contributes nothing.
    fn prompt_guidance(&self) -> Option<&'static str> {
        None
    }

    /// Scheduling class of one call (native multi-call batches only).
    /// Default `Exclusive`. `read-file` is `Parallel`; the shells are
    /// `Parallel` only for read-only commands (same predicate the
    /// permission and plan policies use), else `Exclusive`.
    fn concurrency(&self, _args: &Value) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn run(&self, args: Value, ctx: SkillContext) -> SkillOutcome;
}
