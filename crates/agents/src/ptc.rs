//! Programmatic tool calling — the `run-code` skill (guide §7).
//!
//! Under [`protocol::ToolMode::Ptc`] the model is handed a single data tool,
//! `run-code`, and a generated SDK listing every other skill as a script
//! function. It writes a short program; the program calls the tools. Three
//! things follow from that:
//!
//! - **N round-trips collapse into one.** A read-glob-grep-edit sequence that
//!   costs four model requests under native tool calling costs one here.
//! - **Intermediate data never reaches the context.** Only what the program
//!   prints, plus its final value, becomes the `tool_result` the model reads.
//!   A 4 MB file the script greps stays inside the script.
//! - **The model can loop and branch over tools** instead of unrolling the
//!   iteration into the conversation one hop at a time.
//!
//! The runtime is [Rhai](https://rhai.rs): pure Rust, no I/O of its own, and
//! hard-limited on operations, recursion, string/array/map size and wall
//! clock. Every capability a program has arrives as a host function that
//! re-enters the ordinary guarded pipeline — `ToolSubAgent::run`, so the
//! permission policies, the approval broker, the repeat guard, spilling and
//! the summariser all still apply, and every sub-call surfaces as a live
//! `ToolCallStarted`/`ToolCallFinished` pair nested under the `run-code`
//! chip.
//!
//! Rhai is synchronous and the skills are async, so the program runs on a
//! blocking thread and each host function blocks on the current runtime.
//! `on_progress` polls the interrupt token and the wall-clock deadline
//! between operations, which is what stops a runaway loop.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rhai::{Dynamic, Engine, EvalAltResult, ImmutableString, Position};
use serde_json::{Map, Value};
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

use crate::registry::SkillRegistry;
use crate::skill::{Skill, SkillContext, SkillOutcome};
use crate::subagent::{ToolInvocation, ToolSubAgent};

pub const RUN_CODE_NAME: &str = "run-code";

const RUN_CODE_DESCRIPTION: &str =
    "Run a short Rhai program that calls the other tools and prints what \
     matters. Only what the program prints or returns enters the \
     conversation.";

/// Skills a program can never call, because their bodies do not live in
/// `Skill::run` at all: the harness controls mutate the session log or end
/// the turn from inside the dispatcher, and `ask-user` is a turn-level
/// rendezvous with a human. Under `Ptc` these stay directly callable by the
/// model instead — see [`direct_callable`]. `run-code` excludes itself: a
/// program that starts a program buys nothing and nests two blocking
/// threads to get it.
pub const PROGRAM_EXCLUDED: &[&str] = &[
    RUN_CODE_NAME,
    crate::control::ASK_USER_NAME,
    crate::control::TODO_WRITE_NAME,
    crate::control::EXIT_PLAN_MODE_NAME,
    crate::goal::CREATE_GOAL_NAME,
    crate::goal::GET_GOAL_NAME,
    crate::goal::UPDATE_GOAL_NAME,
];

/// Wall-clock budget for one program, enforced by `on_progress`. Shorter
/// than [`Skill::timeout`] below so the script aborts itself — and reports
/// what it managed to print — before the pipeline abandons the call.
const PROGRAM_BUDGET: Duration = Duration::from_secs(540);

/// Rhai operation cap. A tight infinite loop burns this in well under a
/// second, so it costs a legitimate program nothing and bounds a runaway.
const MAX_OPERATIONS: u64 = 2_000_000;

/// How many tool calls one program may make. A loop over a directory is the
/// point of PTC, so this is generous; it exists so a bug cannot fire ten
/// thousand shells.
const MAX_SUB_CALLS: u32 = 96;

/// Cap on the program's own printed output before the pipeline's spill and
/// retention policies see it.
const MAX_PROGRAM_OUTPUT: usize = 64 * 1024;

/// Is `name` callable directly by the model under `Ptc`? Everything else is
/// refused before the policy pipeline and pointed at `run-code`.
pub fn direct_callable(name: &str) -> bool {
    name == RUN_CODE_NAME
        || name == crate::control::ASK_USER_NAME
        || crate::control::is_control_skill(name)
}

/// The refusal a direct call to a non-[`direct_callable`] tool gets.
pub fn direct_call_refused(name: &str) -> String {
    format!(
        "only `{RUN_CODE_NAME}` is callable directly in this mode — call \
         `{name}` from inside a `{RUN_CODE_NAME}` program instead"
    )
}

/// The registry a program sees: everything but [`PROGRAM_EXCLUDED`].
pub fn program_view(registry: &SkillRegistry) -> SkillRegistry {
    registry.excluding(PROGRAM_EXCLUDED)
}

/// The registry the model is offered as native tool schemas under `Ptc`:
/// `run-code` plus the harness controls a program cannot run.
pub fn direct_view(registry: &SkillRegistry) -> SkillRegistry {
    let names: Vec<&str> = registry
        .by_name
        .keys()
        .map(String::as_str)
        .filter(|n| direct_callable(n))
        .collect();
    registry.restricted_to(&names)
}

/// Skill name → script function name. Rhai identifiers are
/// `[A-Za-z_][A-Za-z0-9_]*`, so `read-file` becomes `read_file` and an
/// `mcp__server__tool` passes through unchanged. A name that would start
/// with a digit gets a `t_` prefix.
pub fn fn_name(skill: &str) -> String {
    let mut out = String::with_capacity(skill.len() + 2);
    for (i, ch) in skill.chars().enumerate() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if i == 0 && ch.is_ascii_digit() {
                out.push_str("t_");
            }
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("t_");
    }
    out
}

/// The generated SDK, composed into the system prompt at
/// [`crate::prompt::order::PTC_SDK`]. Deterministic: skills in name order,
/// one bullet each, so the prompt prefix stays byte-stable across requests.
pub fn sdk_markdown(registry: &SkillRegistry) -> String {
    let mut names: Vec<&str> = registry.by_name.keys().map(String::as_str).collect();
    names.sort_unstable();

    let mut out = String::from(SDK_HEADER);
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for name in names {
        let Some(skill) = registry.by_name.get(name) else { continue };
        let args = skill.positional_args();
        let fname = fn_name(name);
        // A collision (two skills whose names differ only in punctuation)
        // leaves the loser reachable through `tool()` and nothing else —
        // announcing two functions with one body would be a lie.
        let clash = !seen.insert(fname.clone());
        out.push_str("- `");
        if clash || args.len() > MAX_POSITIONAL {
            out.push_str(&format!("tool(\"{name}\", #{{ "));
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&format!("{a}: …"));
            }
            out.push_str(" })");
        } else {
            out.push_str(&fname);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(a);
            }
            out.push(')');
        }
        out.push('`');
        let desc = skill.description();
        if !desc.is_empty() {
            out.push_str(" — ");
            out.push_str(desc);
        }
        let optional = skill.optional_args();
        if !optional.is_empty() {
            out.push_str(&format!(
                " Optional (named form only): {}.",
                optional
                    .iter()
                    .map(|o| format!("`{o}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        out.push('\n');
    }
    out
}

/// Longest positional list still rendered as a plain function call. Past
/// this the named form is easier to get right than counting commas.
const MAX_POSITIONAL: usize = 5;

const SDK_HEADER: &str = "\
## Programmatic tool calling

`run-code` is the only data tool you can call. Its `code` argument is a
[Rhai](https://rhai.rs) program that calls the tools below as ordinary
functions and prints what you need to see.

Rules:

- **Only what you print or return is program output.** Every intermediate
  value stays inside the program and never enters this conversation — so
  read, filter and summarise there, and `print` only the answer.
- Every tool function returns a string (the tool's result). A failing tool
  call throws; catch it with `try { … } catch (err) { … }` when a failure is
  expected, otherwise let it end the program.
- `tool(\"skill-name\", #{ arg: value })` is the named form — the only way to
  pass an optional argument.
- Rhai basics: `let x = …;`, `if`/`else`, `for x in array`, `while`,
  `fn f(a) { … }`, string interpolation with `` `${x}` ``, `+` concatenates.
  There is no `await` and no I/O beyond these functions.
- Keep programs short and deterministic. They run with an operation cap, a
  tool-call cap and a wall-clock deadline, and they cannot import modules.

Tools:

";

/// One program's outcome: what it printed and how it ended.
struct ProgramResult {
    output: String,
    error:  Option<String>,
    calls:  u32,
}

/// The `run-code` skill. Holds a `Weak` back-reference to the registry it
/// itself lives in, attached by the backend once the registry is final —
/// the same pattern `subagent` and `ralph` use.
pub struct RunCode {
    registry: OnceLock<Weak<SkillRegistry>>,
}

impl RunCode {
    pub fn new() -> Self {
        Self { registry: OnceLock::new() }
    }

    /// Give programs access to the live skill catalogue. Must be called
    /// after the registry is wrapped in its final `Arc`; calling it twice
    /// is a no-op.
    pub fn attach_registry(&self, registry: &Arc<SkillRegistry>) {
        let _ = self.registry.set(Arc::downgrade(registry));
    }

    fn program_registry(&self) -> Option<Arc<SkillRegistry>> {
        let live = self.registry.get().and_then(Weak::upgrade)?;
        Some(Arc::new(program_view(&live)))
    }
}

impl Default for RunCode {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Skill for RunCode {
    fn name(&self) -> &str {
        RUN_CODE_NAME
    }

    fn description(&self) -> &str {
        RUN_CODE_DESCRIPTION
    }

    fn positional_args(&self) -> Vec<String> {
        vec!["code".into()]
    }

    fn optional_args(&self) -> Vec<String> {
        vec!["description".into()]
    }

    /// A program is a batch of tool calls, so it inherits the sum of their
    /// budgets rather than one call's. The script's own deadline
    /// ([`PROGRAM_BUDGET`]) is shorter, so the normal ending is the script
    /// aborting itself with its output intact.
    fn timeout(&self) -> Duration {
        PROGRAM_BUDGET + Duration::from_secs(60)
    }

    /// Program output is whatever the tools it ran produced — file bodies,
    /// command output, fetched pages. Data, not instructions.
    fn trusted(&self) -> bool {
        false
    }

    async fn run(&self, args: Value, ctx: SkillContext) -> SkillOutcome {
        let code = args.get("code").and_then(Value::as_str).unwrap_or_default().to_string();
        if code.trim().is_empty() {
            return SkillOutcome {
                ok:      false,
                summary: "`run-code` needs a `code` argument holding the program to run".into(),
            };
        }
        let Some(registry) = self.program_registry() else {
            return SkillOutcome {
                ok:      false,
                summary: "`run-code` has no skill catalogue attached — no tools are callable"
                    .into(),
            };
        };
        let sub = ctx.sub;
        let cancel = sub.cancel.clone();
        let handle = Handle::current();
        let deadline = Instant::now() + PROGRAM_BUDGET;

        // Rhai is synchronous and every host function blocks on a skill
        // future, so the program owns a blocking thread for its lifetime.
        // `spawn_blocking` cannot be cancelled from outside, which is why
        // the deadline and the interrupt token are polled from inside.
        let joined = tokio::task::spawn_blocking(move || {
            run_program(&code, &registry, &sub, &handle, cancel, deadline)
        })
        .await;

        let result = match joined {
            Ok(r) => r,
            Err(e) => {
                return SkillOutcome {
                    ok:      false,
                    summary: format!("the program's runtime thread failed: {e}"),
                }
            }
        };
        finish(result)
    }
}

/// Turn a finished program into the outcome the model reads. A program that
/// printed nothing and returned nothing is *not* a failure — but saying so
/// beats handing back an empty block the model has to guess about.
fn finish(result: ProgramResult) -> SkillOutcome {
    let ProgramResult { mut output, error, calls } = result;
    if output.len() > MAX_PROGRAM_OUTPUT {
        let window = sica_core::retain::head_tail(
            &output,
            MAX_PROGRAM_OUTPUT * 3 / 4,
            MAX_PROGRAM_OUTPUT / 4,
        );
        output = window.render("program output");
    }
    match error {
        Some(err) => SkillOutcome {
            ok:      false,
            summary: if output.trim().is_empty() {
                format!("the program failed after {calls} tool call(s): {err}")
            } else {
                format!("{output}\n\nthe program failed after {calls} tool call(s): {err}")
            },
        },
        None if output.trim().is_empty() => SkillOutcome {
            ok:      true,
            summary: format!(
                "the program ran {calls} tool call(s) and finished without printing or \
                 returning anything — print what you need to see"
            ),
        },
        None => SkillOutcome { ok: true, summary: output },
    }
}

/// Shared state every host function needs. Single-threaded by construction
/// (it lives on one blocking thread for one program), hence `Rc`/`Cell`.
struct Dispatcher<'a> {
    sub:    &'a ToolSubAgent,
    handle: &'a Handle,
    calls:  Rc<Cell<u32>>,
}

impl Dispatcher<'_> {
    /// Run one skill through the ordinary guarded pipeline and hand the
    /// script back its summary. A failed call throws so `try`/`catch` and
    /// "let it end the program" both behave the way the SDK promises.
    fn call(&self, skill: &Arc<dyn Skill>, args: Value) -> Result<Dynamic, Box<EvalAltResult>> {
        let n = self.calls.get();
        if n >= MAX_SUB_CALLS {
            return Err(throw(format!(
                "tool-call cap ({MAX_SUB_CALLS}) reached — the program was stopped; \
                 narrow the work or split it across calls"
            )));
        }
        self.calls.set(n + 1);
        let raw_args: Vec<String> = args
            .as_object()
            .map(|m| {
                m.values()
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let outcome = self.handle.block_on(self.sub.run(ToolInvocation {
            skill: &**skill,
            args,
            raw_args,
            // A program curates its own output, so there is nothing for the
            // summariser to focus on — the raw result is what the script
            // wants to work with.
            expectation: String::new(),
        }));
        if outcome.ok {
            Ok(Dynamic::from(outcome.summary))
        } else {
            Err(throw(format!("`{}` failed: {}", skill.name(), outcome.summary)))
        }
    }
}

fn throw(message: String) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorRuntime(Dynamic::from(message), Position::NONE))
}

/// Build the engine, register one host function per skill, and run the
/// program. Called on a blocking thread.
fn run_program(
    code: &str,
    registry: &Arc<SkillRegistry>,
    sub: &ToolSubAgent,
    handle: &Handle,
    cancel: Option<CancellationToken>,
    deadline: Instant,
) -> ProgramResult {
    let mut engine = Engine::new();
    // No filesystem, no module imports, no `eval` — a program's only
    // capabilities are the host functions registered below.
    engine.set_module_resolver(rhai::module_resolvers::DummyModuleResolver::new());
    engine.disable_symbol("eval");
    engine.set_max_operations(MAX_OPERATIONS);
    engine.set_max_call_levels(24);
    engine.set_max_expr_depths(64, 32);
    engine.set_max_string_size(4 * 1024 * 1024);
    engine.set_max_array_size(100_000);
    engine.set_max_map_size(100_000);

    let printed = Rc::new(RefCell::new(String::new()));
    {
        let sink = printed.clone();
        engine.on_print(move |s| {
            let mut buf = sink.borrow_mut();
            if buf.len() < MAX_PROGRAM_OUTPUT * 2 {
                buf.push_str(s);
                buf.push('\n');
            }
        });
    }
    {
        let sink = printed.clone();
        engine.on_debug(move |s, _src, pos| {
            let mut buf = sink.borrow_mut();
            if buf.len() < MAX_PROGRAM_OUTPUT * 2 {
                buf.push_str(&format!("[debug {pos}] {s}\n"));
            }
        });
    }
    // Polled between operations. Checking the clock on every one of two
    // million operations would dominate the runtime, so the deadline is
    // sampled; the interrupt token is a cheap atomic load and is not.
    engine.on_progress(move |ops| {
        if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
            return Some(Dynamic::from("interrupted"));
        }
        if ops % 4096 == 0 && Instant::now() >= deadline {
            return Some(Dynamic::from("deadline"));
        }
        None
    });

    let calls = Rc::new(Cell::new(0u32));
    register_tools(&mut engine, registry, sub, handle, &calls);

    let outcome = engine.eval::<Dynamic>(code);
    let mut output = printed.borrow().clone();
    let error = match outcome {
        Ok(value) => {
            if !value.is_unit() {
                if !output.is_empty() && !output.ends_with('\n') {
                    output.push('\n');
                }
                output.push_str(&format!("Result: {value}"));
            }
            None
        }
        Err(e) => Some(describe(&e)),
    };
    ProgramResult { output, error, calls: calls.get() }
}

/// A script error as the model should read it: the abort reasons the
/// progress hook raises are harness facts, not Rhai syntax, so they get
/// their own wording.
fn describe(err: &EvalAltResult) -> String {
    match err {
        EvalAltResult::ErrorTerminated(token, _) => match token.to_string().as_str() {
            "interrupted" => "the turn was interrupted — the program was stopped".into(),
            "deadline" => format!(
                "the program exceeded its {}s wall-clock budget and was stopped",
                PROGRAM_BUDGET.as_secs()
            ),
            other => format!("the program was stopped ({other})"),
        },
        EvalAltResult::ErrorTooManyOperations(_) => format!(
            "the program exceeded its operation cap ({MAX_OPERATIONS}) and was stopped — \
             it is probably looping"
        ),
        other => other.to_string(),
    }
}

/// Register `tool(name, args)` plus one function per skill, named after it.
/// Registration order is the registry's sorted name order so a collision
/// resolves the same way on every machine.
fn register_tools(
    engine: &mut Engine,
    registry: &Arc<SkillRegistry>,
    sub: &ToolSubAgent,
    handle: &Handle,
    calls: &Rc<Cell<u32>>,
) {
    // The named form. Present for every skill, including the ones whose
    // arity or name keeps them out of the generated function list.
    {
        let reg = registry.clone();
        let sub = sub.clone();
        let handle = handle.clone();
        let calls = calls.clone();
        engine.register_fn(
            "tool",
            move |name: ImmutableString, map: rhai::Map| -> Result<Dynamic, Box<EvalAltResult>> {
                let Some(skill) = reg.get(name.as_str()) else {
                    return Err(throw(unknown_tool(&reg, name.as_str())));
                };
                let mut args = Map::new();
                for (k, v) in map.iter() {
                    args.insert(k.to_string(), coerce(&skill, to_json(v)));
                }
                let d = Dispatcher { sub: &sub, handle: &handle, calls: calls.clone() };
                d.call(&skill, Value::Object(args))
            },
        );
    }
    {
        let reg = registry.clone();
        let sub = sub.clone();
        let handle = handle.clone();
        let calls = calls.clone();
        engine.register_fn(
            "tool",
            move |name: ImmutableString| -> Result<Dynamic, Box<EvalAltResult>> {
                let Some(skill) = reg.get(name.as_str()) else {
                    return Err(throw(unknown_tool(&reg, name.as_str())));
                };
                let d = Dispatcher { sub: &sub, handle: &handle, calls: calls.clone() };
                d.call(&skill, Value::Object(Map::new()))
            },
        );
    }

    let mut names: Vec<&str> = registry.by_name.keys().map(String::as_str).collect();
    names.sort_unstable();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for name in names {
        let Some(skill) = registry.by_name.get(name).cloned() else { continue };
        let fname = fn_name(name);
        if !seen.insert(fname.clone()) {
            continue;
        }
        let params = skill.positional_args();
        // Bind each arity explicitly: Rhai needs a concrete closure
        // signature, and `Dynamic` parameters keep a typed MCP tool
        // callable with real numbers and arrays.
        macro_rules! bind {
            ($($arg:ident),*) => {{
                let skill = skill.clone();
                let params = params.clone();
                let sub = sub.clone();
                let handle = handle.clone();
                let calls = calls.clone();
                engine.register_fn(
                    fname.as_str(),
                    move |$($arg: Dynamic),*| -> Result<Dynamic, Box<EvalAltResult>> {
                        let supplied = vec![$(to_json(&$arg)),*];
                        let mut args = Map::new();
                        for (n, v) in params.iter().zip(supplied) {
                            args.insert(n.clone(), coerce(&skill, v));
                        }
                        let d = Dispatcher { sub: &sub, handle: &handle, calls: calls.clone() };
                        d.call(&skill, Value::Object(args))
                    },
                );
            }};
        }
        match params.len() {
            0 => bind!(),
            1 => bind!(a),
            2 => bind!(a, b),
            3 => bind!(a, b, c),
            4 => bind!(a, b, c, d),
            5 => bind!(a, b, c, d, e),
            // Past `MAX_POSITIONAL` the SDK advertises the named form only,
            // so there is no function to register.
            _ => {}
        }
    }
}

/// The error for a `tool()` call naming something that is not there — with
/// the nearest few names, because a model that guessed a name once will
/// otherwise guess again.
fn unknown_tool(registry: &SkillRegistry, name: &str) -> String {
    let mut names: Vec<&str> = registry.by_name.keys().map(String::as_str).collect();
    names.sort_unstable();
    let near: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| n.contains(name) || name.contains(*n))
        .take(3)
        .collect();
    if near.is_empty() {
        format!("no tool named `{name}` — see the tool list in the system prompt")
    } else {
        format!("no tool named `{name}` — did you mean {}?", near.join(", "))
    }
}

/// Match the argument to the schema the registry advertises. A skill with
/// no schema of its own is announced as all-strings (`SkillRegistry::
/// tools_json`), and its body reads `args["x"].as_str()`; handing it a Rhai
/// integer would make the argument silently absent. A skill that carries
/// its own schema — an MCP tool — gets the value with its type intact.
fn coerce(skill: &Arc<dyn Skill>, value: Value) -> Value {
    if skill.parameters_schema().is_some() {
        return value;
    }
    match value {
        Value::String(_) | Value::Null => value,
        Value::Bool(b) => Value::String(b.to_string()),
        Value::Number(n) => Value::String(n.to_string()),
        other => Value::String(other.to_string()),
    }
}

/// Rhai value → JSON. Arrays and maps convert structurally so a typed MCP
/// tool can be handed `#{ paths: ["a", "b"] }`; anything exotic degrades to
/// its display form rather than failing the call.
fn to_json(d: &Dynamic) -> Value {
    if d.is_unit() {
        return Value::Null;
    }
    if let Ok(b) = d.as_bool() {
        return Value::Bool(b);
    }
    if let Ok(i) = d.as_int() {
        return Value::Number(i.into());
    }
    if let Ok(f) = d.as_float() {
        return serde_json::Number::from_f64(f as f64).map(Value::Number).unwrap_or(Value::Null);
    }
    if d.is_string() {
        return Value::String(d.clone().into_string().unwrap_or_default());
    }
    if let Some(arr) = d.clone().try_cast::<rhai::Array>() {
        return Value::Array(arr.iter().map(to_json).collect());
    }
    if let Some(map) = d.clone().try_cast::<rhai::Map>() {
        let mut obj = Map::new();
        for (k, v) in map.iter() {
            obj.insert(k.to_string(), to_json(v));
        }
        return Value::Object(obj);
    }
    Value::String(d.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::EventSink;
    use crate::skill::Concurrency;
    use std::sync::Mutex;

    struct Silent;
    impl EventSink for Silent {
        fn emit(&self, _e: protocol::Event) {}
    }

    /// Records every call it receives and echoes its arguments back.
    struct Echo {
        name:   &'static str,
        args:   Vec<String>,
        seen:   Mutex<Vec<Value>>,
        fails:  bool,
        schema: Option<Value>,
    }

    impl Echo {
        fn new(name: &'static str, args: &[&str]) -> Self {
            Self {
                name,
                args: args.iter().map(|s| s.to_string()).collect(),
                seen: Mutex::new(Vec::new()),
                fails: false,
                schema: None,
            }
        }
    }

    #[async_trait]
    impl Skill for Echo {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "echoes"
        }
        fn positional_args(&self) -> Vec<String> {
            self.args.clone()
        }
        fn parameters_schema(&self) -> Option<Value> {
            self.schema.clone()
        }
        fn concurrency(&self, _a: &Value) -> Concurrency {
            Concurrency::Parallel
        }
        async fn run(&self, args: Value, _c: SkillContext) -> SkillOutcome {
            self.seen.lock().unwrap().push(args.clone());
            SkillOutcome { ok: !self.fails, summary: args.to_string() }
        }
    }

    fn run(code: &str, skills: Vec<Arc<dyn Skill>>) -> SkillOutcome {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut reg = SkillRegistry::new();
            for s in skills {
                reg.register(s);
            }
            let run_code = Arc::new(RunCode::new());
            reg.register(run_code.clone());
            let reg = Arc::new(reg);
            run_code.attach_registry(&reg);
            let sub = ToolSubAgent::root(Arc::new(Silent));
            let ctx = SkillContext { sub: sub.child(1) };
            run_code.run(serde_json::json!({ "code": code }), ctx).await
        })
    }

    #[test]
    fn a_program_reports_only_what_it_prints() {
        let out = run(r#"let x = 6 * 7; print("answer " + x); "ignored";"#, vec![]);
        assert!(out.ok, "{}", out.summary);
        // The trailing expression is the return value, so it *is* reported;
        // the intermediate `x` is not.
        assert!(out.summary.contains("answer 42"), "{}", out.summary);
        assert!(out.summary.contains("Result: ignored"), "{}", out.summary);
    }

    #[test]
    fn a_program_with_no_output_says_so_rather_than_returning_nothing() {
        let out = run("let x = 1;", vec![]);
        assert!(out.ok);
        assert!(out.summary.contains("without printing or returning"), "{}", out.summary);
    }

    #[test]
    fn tools_are_callable_as_functions_named_after_the_skill() {
        let echo = Arc::new(Echo::new("read-file", &["path"]));
        let out = run(r#"print(read_file("src/main.rs"));"#, vec![echo]);
        assert!(out.ok, "{}", out.summary);
        assert!(out.summary.contains("src/main.rs"), "{}", out.summary);
    }

    #[test]
    fn the_named_form_reaches_arguments_the_positional_form_cannot() {
        let out = run(
            r#"print(tool("read-file", #{ path: "a.rs", start: "10" }));"#,
            vec![Arc::new(Echo::new("read-file", &["path"]))],
        );
        assert!(out.ok, "{}", out.summary);
        assert!(out.summary.contains("\"start\":\"10\""), "{}", out.summary);
    }

    #[test]
    fn arguments_are_stringified_for_a_skill_with_the_synthesised_schema() {
        // The registry announces these as strings, so the body reads
        // `as_str()`; a Rhai integer must arrive as `"40"`, not `40`.
        let out = run(
            r#"print(tool("read-file", #{ path: "a.rs", end: 40 }));"#,
            vec![Arc::new(Echo::new("read-file", &["path"]))],
        );
        assert!(out.summary.contains("\"end\":\"40\""), "{}", out.summary);
    }

    #[test]
    fn a_typed_schema_keeps_its_types() {
        let mut echo = Echo::new("mcp__fs__grep", &["query"]);
        echo.schema = Some(serde_json::json!({
            "type": "object",
            "properties": { "query": { "type": "string" }, "limit": { "type": "integer" } },
            "required": ["query"],
        }));
        let out = run(
            r#"print(tool("mcp__fs__grep", #{ query: "fn main", limit: 3 }));"#,
            vec![Arc::new(echo)],
        );
        assert!(out.summary.contains("\"limit\":3"), "{}", out.summary);
    }

    #[test]
    fn a_failing_tool_throws_and_is_catchable() {
        let mut echo = Echo::new("run-cli", &["command"]);
        echo.fails = true;
        let out = run(
            r#"try { run_cli("false"); print("unreachable"); }
               catch (err) { print("caught"); }"#,
            vec![Arc::new(echo)],
        );
        assert!(out.ok, "{}", out.summary);
        assert!(out.summary.contains("caught"), "{}", out.summary);
        assert!(!out.summary.contains("unreachable"), "{}", out.summary);
    }

    #[test]
    fn an_uncaught_tool_failure_ends_the_program() {
        let mut echo = Echo::new("run-cli", &["command"]);
        echo.fails = true;
        let out = run(
            r#"print("before"); run_cli("false"); print("unreachable");"#,
            vec![Arc::new(echo)],
        );
        assert!(!out.ok);
        assert!(out.summary.contains("before"), "{}", out.summary);
        assert!(!out.summary.contains("unreachable"), "{}", out.summary);
        assert!(out.summary.contains("run-cli"), "{}", out.summary);
    }

    #[test]
    fn a_loop_over_tools_collapses_into_one_call() {
        let echo = Arc::new(Echo::new("read-file", &["path"]));
        let out = run(
            r#"let total = 0;
               for p in ["a", "b", "c"] { total += read_file(p).len(); }
               print(`read 3 files, ${total} chars`);"#,
            vec![echo.clone()],
        );
        assert!(out.ok, "{}", out.summary);
        assert!(out.summary.contains("read 3 files"), "{}", out.summary);
        assert_eq!(echo.seen.lock().unwrap().len(), 3);
    }

    #[test]
    fn a_runaway_loop_hits_the_operation_cap() {
        let out = run("let i = 0; while true { i += 1; }", vec![]);
        assert!(!out.ok);
        assert!(out.summary.contains("operation cap"), "{}", out.summary);
    }

    #[test]
    fn the_tool_call_cap_stops_a_runaway_program() {
        let echo = Arc::new(Echo::new("read-file", &["path"]));
        let out = run(
            r#"let i = 0; while i < 1000 { read_file("x"); i += 1; }"#,
            vec![echo.clone()],
        );
        assert!(!out.ok);
        assert!(out.summary.contains("tool-call cap"), "{}", out.summary);
        assert_eq!(echo.seen.lock().unwrap().len(), MAX_SUB_CALLS as usize);
    }

    #[test]
    fn a_program_cannot_import_a_module_or_read_the_filesystem() {
        let out = run(r#"import "std" as s;"#, vec![]);
        assert!(!out.ok, "{}", out.summary);
    }

    #[test]
    fn a_syntax_error_comes_back_as_a_readable_failure() {
        let out = run("this is not rhai(((", vec![]);
        assert!(!out.ok);
        assert!(!out.summary.is_empty());
    }

    #[test]
    fn an_unknown_tool_name_suggests_the_near_misses() {
        let out = run(r#"tool("read");"#, vec![Arc::new(Echo::new("read-file", &["path"]))]);
        assert!(!out.ok);
        assert!(out.summary.contains("read-file"), "{}", out.summary);
    }

    #[test]
    fn skill_names_map_to_rhai_identifiers() {
        assert_eq!(fn_name("read-file"), "read_file");
        assert_eq!(fn_name("mcp__fs__grep"), "mcp__fs__grep");
        assert_eq!(fn_name("2fast"), "t_2fast");
        assert_eq!(fn_name("web.fetch"), "web_fetch");
    }

    fn catalogue() -> SkillRegistry {
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(Echo::new("read-file", &["path"])));
        reg.register(Arc::new(Echo::new("run-cli", &["command"])));
        reg.register(Arc::new(RunCode::new()));
        reg.register(Arc::new(Echo::new(crate::control::TODO_WRITE_NAME, &["todos"])));
        reg
    }

    #[test]
    fn a_program_sees_the_tools_but_not_the_harness_controls() {
        let view = program_view(&catalogue());
        assert!(view.get("read-file").is_some());
        assert!(view.get(RUN_CODE_NAME).is_none());
        assert!(view.get(crate::control::TODO_WRITE_NAME).is_none());
    }

    #[test]
    fn the_model_is_offered_run_code_and_the_controls_and_nothing_else() {
        let view = direct_view(&catalogue());
        assert!(view.get(RUN_CODE_NAME).is_some());
        assert!(view.get(crate::control::TODO_WRITE_NAME).is_some());
        assert!(view.get("read-file").is_none());
        assert!(!direct_callable("read-file"));
        assert!(direct_callable(RUN_CODE_NAME));
    }

    #[test]
    fn the_sdk_lists_every_tool_with_its_signature() {
        let sdk = sdk_markdown(&program_view(&catalogue()));
        assert!(sdk.contains("- `read_file(path)` — echoes"), "{sdk}");
        assert!(sdk.contains("- `run_cli(command)`"), "{sdk}");
        assert!(!sdk.contains("todo_write"), "{sdk}");
        assert!(!sdk.contains("run_code("), "{sdk}");
    }

    #[test]
    fn the_sdk_is_byte_stable_across_renders() {
        let a = sdk_markdown(&program_view(&catalogue()));
        let b = sdk_markdown(&program_view(&catalogue()));
        assert_eq!(a, b);
    }

    #[test]
    fn the_sdk_names_optional_arguments_under_the_named_form() {
        struct WithOpt;
        #[async_trait]
        impl Skill for WithOpt {
            fn name(&self) -> &str {
                "read-file"
            }
            fn positional_args(&self) -> Vec<String> {
                vec!["path".into()]
            }
            fn optional_args(&self) -> Vec<String> {
                vec!["start".into(), "end".into()]
            }
            async fn run(&self, _a: Value, _c: SkillContext) -> SkillOutcome {
                SkillOutcome { ok: true, summary: String::new() }
            }
        }
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(WithOpt));
        let sdk = sdk_markdown(&reg);
        assert!(sdk.contains("`start`, `end`"), "{sdk}");
    }

    #[test]
    fn a_direct_call_refusal_names_the_way_out() {
        let msg = direct_call_refused("read-file");
        assert!(msg.contains("run-code"));
        assert!(msg.contains("read-file"));
    }
}
