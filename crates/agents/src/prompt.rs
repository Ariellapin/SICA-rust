//! Composed, ordered system-prompt assembly (dsh `dsh-system-prompt` port).
//!
//! The system prompt is not one hand-built string: it is a set of named
//! **sections**, each with a sparse `order` slot, rendered in `(order, name)`
//! order so the bytes are identical on every machine and on every hop.
//! Byte-stability is the point — a stable system prompt keeps the provider's
//! KV/prompt cache hot across requests.
//!
//! Volatile facts (time, working directory, permission mode…) are kept out of
//! the system prompt entirely: they become a **runtime-context snapshot**, a
//! user-role message the backend derives alongside the system prompt. The
//! caller persists it as a `ContextInjected { source: RuntimeContext }` event
//! that shadows its predecessor, so a mode change never invalidates the
//! system-prompt prefix.
//!
//! `{{variable}}` interpolation is **strict**: a reference to an unknown or
//! valueless variable fails the render. A malformed prompt is worse than a
//! loud failure.

use std::collections::BTreeMap;

use protocol::ToolMode;

use crate::registry::SkillRegistry;

/// Centrally allocated sparse order slots. Never renumber a published slot —
/// the ordering contract is what keeps prompts byte-stable across versions.
pub mod order {
    /// Who/what the harness is (native-mode identity, deployment persona).
    pub const IDENTITY: i32 = -1000;
    /// The selected agent preset's body (`agents/*.md`, guide §5.2). Ahead
    /// of `memory.md`: "who is answering" frames the workspace's standing
    /// instructions rather than trailing them. dsh puts its deployment
    /// persona at 0, which sica already spends on `MEMORY`.
    pub const PERSONA: i32 = -500;
    /// `memory.md` — the workspace's durable instruction file.
    pub const MEMORY: i32 = 0;
    /// Plan-mode policy (Wave 3).
    pub const PLAN_POLICY: i32 = 500;
    /// Per-skill usage guidance — one section per skill that provides any.
    pub const SKILL_GUIDANCE: i32 = 1000;
    /// The live skill catalogue (text-protocol mode only).
    pub const CATALOGUE: i32 = 2000;
    /// The generated `run-code` SDK (`ToolMode::Ptc` only, guide §7): the
    /// rules of the script runtime plus one signature per callable tool.
    /// dsh renders its TypeScript SDK at the same slot.
    pub const PTC_SDK: i32 = 5000;
    /// The `workflow` scripting reference (guide §12.5), present only when
    /// the skill is registered. Constant, unlike the PTC SDK: a workflow
    /// script has no tools, so there is no catalogue to generate.
    pub const WORKFLOW_SDK: i32 = 5100;
    /// Structured-output mandate for subagent rounds (Wave 4).
    pub const STRUCTURED_OUTPUT: i32 = 9900;
}

/// One named prompt fragment. `name` breaks order ties deterministically.
#[derive(Debug, Clone)]
pub struct Section {
    pub name:  String,
    pub order: i32,
    pub text:  String,
}

impl Section {
    pub fn new(name: impl Into<String>, order: i32, text: impl Into<String>) -> Self {
        Self { name: name.into(), order, text: text.into() }
    }
}

/// A failed render. Loud by design — see module docs.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum PromptError {
    /// `{{name}}` referenced but no value was registered for it.
    UnknownVariable { name: String, section: String },
}

impl std::fmt::Display for PromptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromptError::UnknownVariable { name, section } => {
                write!(f, "prompt section `{section}` references unknown variable {{{{{name}}}}}")
            }
        }
    }
}

impl std::error::Error for PromptError {}

/// The assembled prompt: the system message body plus the runtime-context
/// snapshot text (when any context section was registered).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rendered {
    pub system:          String,
    pub runtime_context: Option<String>,
}

/// A prompt under construction. Sections join into the system message;
/// contexts join into the runtime-context snapshot.
#[derive(Debug, Default)]
pub struct Assembly {
    sections: Vec<Section>,
    contexts: Vec<Section>,
    vars:     BTreeMap<String, String>,
}

impl Assembly {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a system-prompt section. Empty sections are dropped at render
    /// time so an absent `memory.md` leaves no blank slot behind.
    pub fn section(&mut self, s: Section) -> &mut Self {
        self.sections.push(s);
        self
    }

    /// Add a runtime-context fragment. Rendered into
    /// [`Rendered::runtime_context`], never into the system prompt.
    pub fn context(&mut self, s: Section) -> &mut Self {
        self.contexts.push(s);
        self
    }

    /// Register an interpolation value. Names are trusted as-is; `{{name}}`
    /// references in any section that has no registered value fail the
    /// render.
    pub fn var(&mut self, name: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.vars.insert(name.into(), value.into());
        self
    }

    /// Render the system prompt (and runtime-context snapshot). Sections are
    /// sorted by `(order, name)` — deterministic on every machine — joined
    /// by blank lines, then interpolated strictly.
    pub fn render(&self) -> Result<Rendered, PromptError> {
        let mut system = String::new();
        for s in sorted(&self.sections) {
            let text = interpolate(&s.text, &self.vars, &s.name)?;
            let text = text.trim_end();
            if text.is_empty() {
                continue;
            }
            if !system.is_empty() {
                system.push_str("\n\n");
            }
            system.push_str(text);
        }
        let mut runtime = String::new();
        for s in sorted(&self.contexts) {
            let text = interpolate(&s.text, &self.vars, &s.name)?;
            let text = text.trim_end();
            if text.is_empty() {
                continue;
            }
            if !runtime.is_empty() {
                runtime.push_str("\n\n");
            }
            runtime.push_str(text);
        }
        Ok(Rendered {
            system,
            runtime_context: (!runtime.is_empty()).then_some(runtime),
        })
    }
}

fn sorted(sections: &[Section]) -> Vec<&Section> {
    let mut v: Vec<&Section> = sections.iter().collect();
    v.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.name.cmp(&b.name)));
    v
}

/// Replace every `{{name}}` reference with its registered value. Any
/// reference without a value is an error naming the variable and the
/// section — dsh's "throw on unknown variable" stance.
pub fn interpolate(
    text: &str,
    vars: &BTreeMap<String, String>,
    section: &str,
) -> Result<String, PromptError> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let close = match after.find("}}") {
            Some(c) => c,
            None => {
                // Unterminated `{{`: emit verbatim — it is not a reference.
                out.push_str("{{");
                rest = after;
                continue;
            }
        };
        let name = after[..close].trim();
        match vars.get(name) {
            Some(v) => out.push_str(v),
            None => {
                return Err(PromptError::UnknownVariable {
                    name:    name.to_string(),
                    section: section.to_string(),
                })
            }
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Values every builder can interpolate: `{{cwd}}`, `{{os}}`, `{{date}}`,
/// `{{model}}`. `model` may be empty when no LLM is connected — the
/// variable is registered either way so references never fail; templates
/// that print it should tolerate an empty value.
pub fn standard_vars(model: &str) -> BTreeMap<String, String> {
    standard_vars_in(model, &sica_core::paths::working_dir())
}

/// [`standard_vars`] for a session with its own working directory
/// (guide §3.9). `{{cwd}}` is what the model reads to know where it is, so
/// it has to be the session's folder and not the process default.
pub fn standard_vars_in(model: &str, cwd: &std::path::Path) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    vars.insert("cwd".into(), cwd.display().to_string());
    vars.insert("os".into(), std::env::consts::OS.to_string());
    vars.insert(
        "date".into(),
        chrono::Local::now().format("%Y-%m-%d %H:%M (%:z)").to_string(),
    );
    vars.insert("model".into(), model.to_string());
    vars
}

/// Identity section used in native tool-calling mode. The text-protocol
/// invocation brief lives in `memory.md` instead; native servers get the
/// contract through the request's `tools` array.
pub const NATIVE_IDENTITY: &str =
    "You are running inside the sica-rust desktop app. Use the provided \
     tools (OpenAI function calling) to run commands and read/write files \
     when the task needs it.";

/// One-line runtime-context header. Wording matters: it tells the model the
/// snapshot replaces — not supplements — any earlier one.
pub const RUNTIME_CONTEXT_HEADER: &str =
    "Current runtime context. This snapshot supersedes earlier runtime-context snapshots.";

/// Build the main agent's prompt.
///
/// - **Text protocol**: `memory.md` (MEMORY) + one SKILL_GUIDANCE section
///   per skill that provides guidance + the live catalogue (CATALOGUE).
/// - **Persona**: when a session runs an agent preset (§5.2) its body is a
///   PERSONA section, between the native identity and `memory.md`.
/// - **Native**: an IDENTITY section + `memory.md` + skill guidance. The
///   catalogue is deliberately absent — the `tools` array carries it, and
///   the text-protocol invocation brief in `memory.md` is overridden by the
///   identity's function-calling instruction.
///
/// The runtime-context snapshot carries `{{date}}`, `{{cwd}}`, `{{os}}` and
/// (when known) the model name; volatile policy facts (permission mode,
/// plan mode) ride it as extra lines. The plan-mode policy itself is a
/// `PLAN_POLICY` section, present only while plan mode is active.
pub fn for_main_agent(
    memory: &str,
    registry: &SkillRegistry,
    mode: ToolMode,
    vars: &BTreeMap<String, String>,
    plan_policy: Option<&str>,
    persona: Option<&str>,
) -> Result<Rendered, PromptError> {
    let mut a = Assembly::new();
    a.vars = vars.clone();

    if mode.native() {
        a.section(Section::new("identity", order::IDENTITY, NATIVE_IDENTITY));
    }
    if let Some(persona) = persona.filter(|p| !p.trim().is_empty()) {
        a.section(Section::new("persona", order::PERSONA, persona));
    }
    a.section(Section::new("memory", order::MEMORY, memory));
    if let Some(policy) = plan_policy.filter(|p| !p.trim().is_empty()) {
        a.section(Section::new("plan-policy", order::PLAN_POLICY, policy));
    }
    // One guidance section per skill that has any, all in the SKILL_GUIDANCE
    // slot — ties break by (section) name, i.e. the skill name.
    let mut names: Vec<&str> = registry.by_name.keys().map(String::as_str).collect();
    names.sort_unstable();
    for name in names {
        let Some(skill) = registry.by_name.get(name) else { continue };
        if let Some(guidance) = skill.prompt_guidance() {
            a.section(Section::new(format!("guidance:{name}"), order::SKILL_GUIDANCE, guidance));
        }
    }
    if !mode.native() {
        // `run-code` stays out of the text-protocol catalogue: a program is
        // no use to a model that cannot reliably emit one call, and the
        // guide gates PTC on native-tools providers (§7). It is still
        // registered, so `/run-code` reaches it when a human asks for it.
        let catalogue = registry.catalogue_markdown_excluding(&[crate::ptc::RUN_CODE_NAME]);
        if !catalogue.is_empty() {
            a.section(Section::new(
                "catalogue",
                order::CATALOGUE,
                format!("## Loaded skills\n\n{catalogue}"),
            ));
        }
    }
    // Under PTC the `tools` array carries only `run-code` and the harness
    // controls, so the SDK *is* the catalogue for everything else — without
    // it a program has no way to learn what it can call.
    if mode.ptc() {
        let sdk = crate::ptc::sdk_markdown(&crate::ptc::program_view(registry));
        a.section(Section::new("ptc-sdk", order::PTC_SDK, sdk));
    }
    // A workflow script is a second language the model has to write, so its
    // reference is worth its tokens only where the skill actually exists.
    if registry.by_name.contains_key(crate::workflow::WORKFLOW_NAME) {
        a.section(Section::new("workflow-sdk", order::WORKFLOW_SDK, crate::workflow::SDK));
    }
    a.context(Section::new("runtime", 0, runtime_context_text(vars)));
    a.render()
}

/// The runtime-context snapshot body (without the header): one bullet per
/// volatile fact. Time first — it is the fact models most often need and
/// the one they cannot observe any other way.
pub fn runtime_context_text(vars: &BTreeMap<String, String>) -> String {
    let mut out = String::from(RUNTIME_CONTEXT_HEADER);
    if let Some(date) = vars.get("date") {
        out.push_str(&format!("\n- Local time: {date} — from the OS clock"));
    }
    if let Some(elapsed) = vars.get("elapsed") {
        out.push_str(&format!("\n- Time since the previous message: {elapsed}"));
    }
    if let Some(cwd) = vars.get("cwd") {
        out.push_str(&format!("\n- Working directory: {cwd}"));
    }
    if let Some(os) = vars.get("os") {
        out.push_str(&format!("\n- OS: {os}"));
    }
    match vars.get("model").map(String::as_str) {
        Some(m) if !m.is_empty() => out.push_str(&format!("\n- Model: {m}")),
        _ => {}
    }
    if let Some(perm) = vars.get("permission") {
        out.push_str(&format!("\n- Permission mode: {perm}"));
    }
    if let Some(plan) = vars.get("plan") {
        out.push_str(&format!("\n- Plan mode: {plan}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::{Skill, SkillContext, SkillOutcome};
    use async_trait::async_trait;
    use serde_json::Value;
    use std::sync::Arc;

    #[test]
    fn sections_sort_by_order_then_name() {
        let mut a = Assembly::new();
        a.section(Section::new("b", 0, "B"));
        a.section(Section::new("a", 0, "A"));
        a.section(Section::new("z", -10, "Z"));
        let r = a.render().unwrap();
        assert_eq!(r.system, "Z\n\nA\n\nB", "order first, name breaks ties");
    }

    #[test]
    fn empty_sections_are_dropped() {
        let mut a = Assembly::new();
        a.section(Section::new("empty", 0, ""));
        a.section(Section::new("kept", 1, "K"));
        assert_eq!(a.render().unwrap().system, "K");
    }

    #[test]
    fn interpolation_substitutes_known_variables() {
        let mut a = Assembly::new();
        a.var("cwd", "/work").var("model", "qwen");
        a.section(Section::new("s", 0, "dir is {{cwd}}, model is {{model}}"));
        assert_eq!(a.render().unwrap().system, "dir is /work, model is qwen");
    }

    #[test]
    fn unknown_variable_fails_loudly() {
        let mut a = Assembly::new();
        a.section(Section::new("s", 0, "hello {{who}}"));
        let err = a.render().unwrap_err();
        assert_eq!(
            err,
            PromptError::UnknownVariable { name: "who".into(), section: "s".into() }
        );
        assert!(err.to_string().contains("{{who}}"));
    }

    #[test]
    fn unterminated_braces_are_literal() {
        let mut a = Assembly::new();
        a.section(Section::new("s", 0, "json {{ and }} and {{x"));
        // `{{ and }}` references variable "and"… which is unknown → error.
        assert!(a.render().is_err());
        let mut a = Assembly::new();
        a.section(Section::new("s", 0, "open {{ but never closed"));
        assert_eq!(a.render().unwrap().system, "open {{ but never closed");
    }

    #[test]
    fn contexts_land_in_runtime_context_not_system() {
        let mut a = Assembly::new();
        a.section(Section::new("s", 0, "SYS"));
        a.context(Section::new("c", 0, "CTX"));
        let r = a.render().unwrap();
        assert_eq!(r.system, "SYS");
        assert_eq!(r.runtime_context.as_deref(), Some("CTX"));
    }

    struct Guided;
    #[async_trait]
    impl Skill for Guided {
        fn name(&self) -> &str { "run-cli" }
        fn prompt_guidance(&self) -> Option<&'static str> { Some("GUIDED") }
        async fn run(&self, _a: Value, _c: SkillContext) -> SkillOutcome {
            SkillOutcome { ok: true, summary: String::new() }
        }
    }

    struct Plain;
    #[async_trait]
    impl Skill for Plain {
        fn name(&self) -> &str { "noop" }
        async fn run(&self, _a: Value, _c: SkillContext) -> SkillOutcome {
            SkillOutcome { ok: true, summary: String::new() }
        }
    }

    #[test]
    fn text_mode_assembly_matches_legacy_shape() {
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(Guided));
        reg.register(Arc::new(Plain));
        let vars = standard_vars("m");
        let r = for_main_agent("MEMORY BODY", &reg, ToolMode::Text, &vars, None, None).unwrap();
        // memory → guidance → catalogue, blank-line joined.
        let sys = &r.system;
        assert!(sys.starts_with("MEMORY BODY"), "{sys}");
        assert!(sys.contains("\n\nGUIDED\n\n## Loaded skills"), "{sys}");
        assert!(sys.contains("- **run-cli**"));
        assert!(!sys.contains(NATIVE_IDENTITY));
        // The snapshot is separate from the system body.
        let ctx = r.runtime_context.unwrap();
        assert!(ctx.starts_with(RUNTIME_CONTEXT_HEADER));
        assert!(ctx.contains("Local time:"));
        assert!(ctx.contains("Working directory:"));
        assert!(ctx.contains("Model: m"));
    }

    #[test]
    fn native_mode_gets_memory_back_but_no_catalogue() {
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(Guided));
        let vars = standard_vars("m");
        let r = for_main_agent("MEMORY BODY", &reg, ToolMode::Native, &vars, None, None).unwrap();
        let sys = &r.system;
        assert!(sys.starts_with(NATIVE_IDENTITY), "{sys}");
        assert!(sys.contains("MEMORY BODY"), "native mode keeps memory.md");
        assert!(sys.contains("GUIDED"));
        assert!(!sys.contains("## Loaded skills"), "tools array carries it");
    }

    #[test]
    fn ptc_mode_renders_the_sdk_after_the_guidance_and_hides_the_controls() {
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(Guided));
        reg.register(Arc::new(crate::ptc::RunCode::new()));
        let vars = standard_vars("m");
        let r = for_main_agent("MEMORY BODY", &reg, ToolMode::Ptc, &vars, None, None).unwrap();
        let sys = &r.system;
        // PTC rides the native wire, so it keeps the native identity and
        // still drops the text-protocol catalogue.
        assert!(sys.starts_with(NATIVE_IDENTITY), "{sys}");
        assert!(!sys.contains("## Loaded skills"), "{sys}");
        assert!(sys.contains("## Programmatic tool calling"), "{sys}");
        assert!(sys.contains("- `run_cli()`"), "{sys}");
        // `run-code` is the tool being *called*, never one a program calls.
        assert!(!sys.contains("- `run_code("), "{sys}");
        // PTC_SDK (5000) is after SKILL_GUIDANCE (1000).
        assert!(sys.find("GUIDED").unwrap() < sys.find("## Programmatic").unwrap());
    }

    #[test]
    fn text_mode_keeps_run_code_out_of_the_catalogue() {
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(Plain));
        reg.register(Arc::new(crate::ptc::RunCode::new()));
        let vars = standard_vars("m");
        let r = for_main_agent("MEM", &reg, ToolMode::Text, &vars, None, None).unwrap();
        assert!(r.system.contains("## Loaded skills"), "{}", r.system);
        assert!(!r.system.contains("run-code"), "{}", r.system);
    }

    #[test]
    fn native_mode_renders_no_sdk() {
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(crate::ptc::RunCode::new()));
        let vars = standard_vars("m");
        let r = for_main_agent("MEM", &reg, ToolMode::Native, &vars, None, None).unwrap();
        assert!(!r.system.contains("## Programmatic tool calling"), "{}", r.system);
    }

    #[test]
    fn the_workflow_reference_costs_nothing_until_the_skill_is_registered() {
        let vars = standard_vars("m");
        let mut bare = SkillRegistry::new();
        bare.register(Arc::new(Plain));
        let without = for_main_agent("MEM", &bare, ToolMode::Native, &vars, None, None).unwrap();
        assert!(!without.system.contains("## Workflow scripts"), "{}", without.system);

        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(Plain));
        reg.register(Arc::new(crate::workflow::Workflow::new()));
        let with = for_main_agent("MEM", &reg, ToolMode::Native, &vars, None, None).unwrap();
        let sys = &with.system;
        assert!(sys.contains("## Workflow scripts"), "{sys}");
        assert!(sys.contains("`agent(prompt)`"), "{sys}");
        // WORKFLOW_SDK (5100) sits after SKILL_GUIDANCE (1000), so the
        // reference follows the one-liner that says when to reach for it.
        let guidance = sys.find("Reach for `workflow`").expect("the guidance renders");
        assert!(guidance < sys.find("## Workflow scripts").unwrap(), "{sys}");
    }

    #[test]
    fn plan_policy_section_appears_only_when_active() {
        let reg = SkillRegistry::new();
        let vars = standard_vars("m");
        let off = for_main_agent("MEM", &reg, ToolMode::Text, &vars, None, None).unwrap();
        assert!(!off.system.contains("plan mode"));
        let on =
            for_main_agent("MEM", &reg, ToolMode::Text, &vars, Some("PLAN RULES"), None).unwrap();
        assert!(on.system.contains("PLAN RULES"));
        // PLAN_POLICY (500) sits between memory (0) and guidance (1000).
        let mem = on.system.find("MEM").unwrap();
        let plan = on.system.find("PLAN RULES").unwrap();
        assert!(mem < plan);
    }

    #[test]
    fn persona_precedes_memory_and_follows_the_native_identity() {
        let reg = SkillRegistry::new();
        let vars = standard_vars("m");
        let r = for_main_agent("MEM", &reg, ToolMode::Native, &vars, None, Some("I AM REVIEWER")).unwrap();
        let sys = &r.system;
        let ident = sys.find(NATIVE_IDENTITY).unwrap();
        let persona = sys.find("I AM REVIEWER").unwrap();
        let mem = sys.find("MEM").unwrap();
        assert!(ident < persona && persona < mem, "{sys}");
        // Absent / blank personas leave no slot behind.
        let none = for_main_agent("MEM", &reg, ToolMode::Native, &vars, None, None).unwrap();
        assert!(!none.system.contains("I AM REVIEWER"));
        let blank = for_main_agent("MEM", &reg, ToolMode::Native, &vars, None, Some("  
 ")).unwrap();
        assert_eq!(blank.system, none.system);
    }

    #[test]
    fn a_persona_interpolates_like_any_other_section() {
        let reg = SkillRegistry::new();
        let mut vars = standard_vars("m");
        vars.insert("cwd".into(), "/work".into());
        let r = for_main_agent("MEM", &reg, ToolMode::Text, &vars, None, Some("dir {{cwd}}")).unwrap();
        assert!(r.system.starts_with("dir /work"), "{}", r.system);
    }

    #[test]
    fn render_is_deterministic_across_insertion_orders() {
        let build = |first: bool| {
            let mut a = Assembly::new();
            if first {
                a.section(Section::new("a", 0, "A")).section(Section::new("b", 0, "B"));
            } else {
                a.section(Section::new("b", 0, "B")).section(Section::new("a", 0, "A"));
            }
            a.render().unwrap().system
        };
        assert_eq!(build(true), build(false));
    }
}
