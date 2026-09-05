//! `workflow` — a model-written orchestration script (Wave 8, guide §12.5).
//!
//! `subagent` delegates one task. `agent-team` runs a fixed roster for a
//! fixed number of rounds. `ralph` iterates one objective. Between them
//! there is no way to say *"summarise each of these six modules, then
//! reconcile the six summaries"* without spending a main-agent turn — and a
//! main-agent context — on every step.
//!
//! A workflow is that missing shape: the model writes a short script whose
//! only verbs are delegation, and the harness runs it. dsh's rule is the one
//! worth keeping — **the agents do the work, the script only coordinates
//! them** — so the script gets no tools at all. It cannot read a file, run a
//! command or reach the network. It can spawn children, fan out, name
//! phases, log and print. Anything that needs a tool is something a child
//! does, and that constraint is what keeps a workflow readable: every line
//! is either control flow or a delegation.
//!
//! It runs in the same sandbox as `run-code` ([`crate::script`]) with a
//! different set of host functions bound, and every child goes through
//! [`crate::runner::run_conversation`] — the driver behind `subagent` and
//! `ralph` — so a child's tool calls re-enter the ordinary guarded pipeline
//! and its chips nest under the `workflow` chip.
//!
//! **`parallel` is genuinely concurrent**, which nothing else in either
//! script runtime is. Rhai is single-threaded, so dsh's `parallel(thunks)`
//! could not be: a thunk that calls `agent()` blocks the one thread there
//! is. The futures, though, belong to the *host* — so `parallel` takes a
//! list of agent specifications instead of thunks and drives them with
//! `join_all`. That is the one deliberate departure from dsh's API, and it
//! is the one that makes the primitive worth having rather than a loop
//! wearing a costume.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::future::join_all;
use llm::client::LlmClient;
use rhai::{Dynamic, Engine, EvalAltResult, FnPtr, ImmutableString, NativeCallContext};
use serde_json::Value;
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;
use sica_core::event::RunState;
use tracing::{info, warn};

use crate::agent::EventSink;
use crate::registry::SkillRegistry;
use crate::runner::{self, RunSpec};
use crate::script::{self, throw};
use crate::skill::{Skill, SkillContext, SkillOutcome};
use crate::subagent::ToolSubAgent;

pub const WORKFLOW_NAME: &str = "workflow";

const WORKFLOW_DESCRIPTION: &str = "Run a short Rhai script whose only verbs \
    are delegation: `agent(prompt)` spawns one child agent and returns its \
    report, `parallel([...])` runs several at once. Use it for a fan-out / \
    reconcile shape that would otherwise cost one turn per step. The script \
    has no tools of its own — the children do the work.";

/// Children one workflow may spend. Each is a full LLM conversation with its
/// own tool budget, so this is the expensive cap — two orders of magnitude
/// below `run-code`'s tool-call cap, because a conversation costs about that
/// much more than a tool call.
const MAX_AGENTS: u32 = 32;

/// Widest single `parallel` fan-out. Concurrent children share one provider
/// connection and one workspace, so past this they mostly queue — and a
/// workflow that wants 30 at once has almost certainly miscounted.
const MAX_PARALLEL: usize = 8;

/// Tool calls one child may make before it must report. Same budget a
/// `subagent` gets: a workflow child is a `subagent` with a shorter leash on
/// how it was asked.
const DEFAULT_AGENT_HOPS: u8 = 8;

/// Ceiling on the `max_hops` a script may ask for.
const MAX_AGENT_HOPS: i64 = 24;

/// Cap on one child's report as the script sees it. A child exists to
/// *shrink* what crosses back; an unbounded report defeats the point, and
/// the script is going to concatenate several of them.
const MAX_REPORT_CHARS: usize = 6000;

/// Wall-clock budget for the whole script, enforced by the sandbox. Long,
/// because 32 children at several minutes each legitimately take a while.
const WORKFLOW_BUDGET: Duration = Duration::from_secs(45 * 60);

/// Operation cap. Far below `run-code`'s: a workflow that needs two million
/// operations of its own has stopped coordinating and started computing.
const MAX_OPERATIONS: u64 = 200_000;

/// Cap on what the script prints, before the pipeline's spill and retention
/// policies see it.
const MAX_OUTPUT: usize = 32 * 1024;

/// The scripting reference, composed into the system prompt at
/// [`crate::prompt::order::WORKFLOW_SDK`] whenever the skill is registered.
/// Unlike the PTC SDK this is a constant: a workflow script cannot call
/// tools, so there is no catalogue to generate.
pub const SDK: &str = "\
## Workflow scripts

`workflow` takes a [Rhai](https://rhai.rs) script whose only verbs are
delegation. It has **no tools** — no file access, no commands, no network.
The child agents it spawns have the full toolset; the script only decides
who runs, in what order, and what to do with the reports.

- `agent(prompt)` → the child's report as a string. It starts from an empty
  conversation and sees nothing of this one, so write the prompt
  standalone: the goal, the paths, and what to report back. A child that
  fails throws; catch it with `try { … } catch (err) { … }`.
- `agent(prompt, #{ label: \"…\", max_hops: 8, schema: #{ … } })` — `label`
  names the child in the log, `max_hops` bounds its tool calls, and
  `schema` (a JSON Schema object) makes it report through
  `structured-output`, in which case `agent` returns a **map** you can index
  (`report.status`) instead of a string.
- `parallel([spec, spec, …])` → an array of results, run **concurrently**.
  Each spec is either a prompt string or the same option map with a
  `prompt` field. A child that fails contributes `()` rather than throwing,
  so check with `if result == () { … }`.
- `pipeline(items, stage_1, stage_2)` → run each item through the stages in
  order, sequentially. A stage is a function of one item returning the next
  item's value; returning `()` drops that item from the later stages.
- `phase(\"title\")` and `log(\"message\")` write to the operator's log — the
  reader of this conversation never sees them.
- `args` is the workflow's `input` argument, as a string.
- **Only what you print or return is the result.** Every child report you
  do not print stays inside the script.

Keep the script short: it is control flow, not work.
";

/// One child a script asked for.
struct AgentSpec {
    prompt:   String,
    label:    Option<String>,
    schema:   Option<Value>,
    max_hops: u8,
}

/// Everything a host function needs to drive a child. Cloned into every
/// registered closure, which is why each field is cheap to clone.
#[derive(Clone)]
struct Deps {
    client:    LlmClient,
    registry:  Option<Arc<SkillRegistry>>,
    catalogue: Option<Arc<String>>,
    sub:       ToolSubAgent,
    handle:    Handle,
    cancel:    Option<CancellationToken>,
    /// Identity of this run for the durable rows (§6.11).
    run_id:    u64,
}

impl Deps {
    /// Report one edge of the run. Silently does nothing when the call has
    /// no notifier or no session — a workflow run outside a session (a test,
    /// an eval) has no log to be durable in, and that is not a failure.
    fn edge(&self, member: Option<&Member>, state: RunState) {
        self.sub.run_edge(
            self.run_id,
            member.map(|m| m.phase.as_str()),
            member.map(|m| (m.id, m.label.as_str())),
            state,
        );
    }
}

/// Script-lifetime bookkeeping. Single-threaded by construction (one script,
/// one blocking thread), hence `Cell`/`RefCell`.
struct State {
    spent: Cell<u32>,
    phase: RefCell<String>,
}

/// One child's identity for the durable rows (§6.11): the phase it belongs
/// to, the label the reader sees, and an id so its *end* finds its own
/// start even when two members share a label.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Member {
    id:    u64,
    phase: String,
    label: String,
}

/// What one finished script produced.
struct ScriptResult {
    output: String,
    error:  Option<String>,
    agents: u32,
}

/// The `workflow` skill. Holds a `Weak` back-reference to the registry it
/// itself lives in, attached by the backend once the registry is final —
/// the same pattern `subagent`, `ralph` and `run-code` use.
pub struct Workflow {
    registry: OnceLock<Weak<SkillRegistry>>,
}

impl Workflow {
    pub fn new() -> Self {
        Self { registry: OnceLock::new() }
    }

    /// Give a workflow's children access to the live skill catalogue. Must
    /// be called after the registry is wrapped in its final `Arc`; calling
    /// it twice is a no-op.
    pub fn attach_registry(&self, registry: &Arc<SkillRegistry>) {
        let _ = self.registry.set(Arc::downgrade(registry));
    }

    /// The registry a child sees — the same restriction every other
    /// runtime-owned child runs under, which is what stops a workflow child
    /// from starting a workflow.
    fn child_registry(&self) -> Option<Arc<SkillRegistry>> {
        let live = self.registry.get().and_then(Weak::upgrade)?;
        Some(Arc::new(live.excluding(crate::control::CHILD_EXCLUDED)))
    }
}

impl Default for Workflow {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Skill for Workflow {
    fn name(&self) -> &str {
        WORKFLOW_NAME
    }

    fn description(&self) -> &str {
        WORKFLOW_DESCRIPTION
    }

    fn positional_args(&self) -> Vec<String> {
        vec!["script".into()]
    }

    fn optional_args(&self) -> Vec<String> {
        vec!["input".into()]
    }

    fn prompt_guidance(&self) -> Option<&'static str> {
        Some(
            "Reach for `workflow` when the same delegation shape repeats — \
             the same question against many files, or a fan-out whose \
             answers then have to be reconciled. One `subagent` call is \
             cheaper for one task; a workflow pays off from about three.",
        )
    }

    /// A workflow is a batch of conversations, so it inherits the sum of
    /// their budgets. The script's own deadline ([`WORKFLOW_BUDGET`]) is
    /// shorter, so the normal ending is the script stopping itself with its
    /// output intact.
    fn timeout(&self) -> Duration {
        WORKFLOW_BUDGET + Duration::from_secs(120)
    }

    /// The output is child reports, which are child-summarised tool output.
    /// Data, not instructions.
    fn trusted(&self) -> bool {
        false
    }

    async fn run(&self, args: Value, ctx: SkillContext) -> SkillOutcome {
        let code = args.get("script").and_then(Value::as_str).unwrap_or_default().to_string();
        if code.trim().is_empty() {
            return fail("`workflow` needs a `script` argument holding the script to run");
        }
        let Some(client) = ctx.sub.summarizer.clone() else {
            return fail(
                "a workflow spawns agents and so needs an LLM, but none is attached to \
                 this call — connect an LLM and retry",
            );
        };
        let input = args.get("input").and_then(Value::as_str).unwrap_or_default().to_string();

        let registry = self.child_registry();
        let catalogue = registry
            .as_ref()
            .map(|r| r.catalogue_markdown_excluding(&[]))
            .filter(|c| !c.is_empty())
            .map(Arc::new);

        let events = ctx.sub.events.clone();
        events.emit(protocol::Event::LogLine {
            level:   "INFO".into(),
            message: format!("workflow: running a {} line script", code.lines().count()),
        });
        info!(lines = code.lines().count(), "workflow: starting");

        let deps = Deps {
            client,
            registry,
            catalogue,
            cancel: ctx.sub.cancel.clone(),
            sub: ctx.sub,
            handle: Handle::current(),
            run_id: crate::subagent::next_run_id(),
        };
        // The run opens before the script does. If the turn is interrupted
        // from here on, this row stands alone — which is what makes an
        // interrupted run visible instead of absent (§6.11).
        deps.edge(None, RunState::Started);
        let deadline = Instant::now() + WORKFLOW_BUDGET;

        // Rhai is synchronous and every host function blocks on child
        // conversations, so the script owns a blocking thread for its
        // lifetime. `spawn_blocking` cannot be cancelled from outside, which
        // is why the deadline and the interrupt token are polled from
        // inside by the sandbox's progress hook.
        let deps_for_close = deps.clone();
        let joined =
            tokio::task::spawn_blocking(move || run_script(&code, &input, deps, deadline)).await;

        let result = match joined {
            Ok(r) => r,
            Err(e) => return fail(&format!("the workflow's runtime thread failed: {e}")),
        };
        info!(agents = result.agents, failed = result.error.is_some(), "workflow: finished");
        deps_for_close.edge(
            None,
            if result.error.is_some() { RunState::Failed } else { RunState::Done },
        );
        events.emit(protocol::Event::LogLine {
            level:   "INFO".into(),
            message: format!("workflow: finished after {} agent(s)", result.agents),
        });
        finish(result)
    }
}

/// Turn a finished script into the outcome the model reads.
fn finish(result: ScriptResult) -> SkillOutcome {
    let ScriptResult { output, error, agents } = result;
    let output = script::window(output, MAX_OUTPUT, "workflow output");
    match error {
        Some(err) => SkillOutcome {
            ok:      false,
            summary: if output.trim().is_empty() {
                format!("the workflow failed after {agents} agent(s): {err}")
            } else {
                format!("{output}\n\nthe workflow failed after {agents} agent(s): {err}")
            },
        },
        None if output.trim().is_empty() => SkillOutcome {
            ok:      true,
            summary: format!(
                "the workflow ran {agents} agent(s) and finished without printing or \
                 returning anything — print the reports you want to keep"
            ),
        },
        None => SkillOutcome { ok: true, summary: output },
    }
}

/// Build the sandbox, bind the delegation primitives, run the script.
/// Called on a blocking thread.
fn run_script(code: &str, input: &str, deps: Deps, deadline: Instant) -> ScriptResult {
    let limits = script::Limits {
        operations: MAX_OPERATIONS,
        budget:     WORKFLOW_BUDGET,
        output_max: MAX_OUTPUT,
    };
    let mut sandbox = script::Sandbox::new(limits, deps.cancel.clone(), deadline);
    sandbox.constant("args", Dynamic::from(input.to_string()));
    let state = Rc::new(State { spent: Cell::new(0), phase: RefCell::new(String::new()) });
    register(&mut sandbox.engine, &deps, &state);
    let out = sandbox.run(code);
    ScriptResult { output: out.output, error: out.error, agents: state.spent.get() }
}

/// Bind `agent`, `parallel`, `pipeline`, `phase` and `log`. Nothing else —
/// the absence of every other verb is the design.
fn register(engine: &mut Engine, deps: &Deps, state: &Rc<State>) {
    {
        let state = state.clone();
        let events = deps.sub.events.clone();
        engine.register_fn("phase", move |title: ImmutableString| {
            let title = title.to_string();
            *state.phase.borrow_mut() = title.clone();
            note(&events, &format!("phase — {title}"));
        });
    }
    {
        let state = state.clone();
        let events = deps.sub.events.clone();
        engine.register_fn("log", move |message: ImmutableString| {
            let phase = state.phase.borrow().clone();
            if phase.is_empty() {
                note(&events, &message.to_string());
            } else {
                note(&events, &format!("[{phase}] {message}"));
            }
        });
    }
    {
        let deps = deps.clone();
        let state = state.clone();
        engine.register_fn("agent", move |prompt: ImmutableString| {
            one(&deps, &state, spec_from(&prompt, None)?)
        });
    }
    {
        let deps = deps.clone();
        let state = state.clone();
        engine.register_fn("agent", move |prompt: ImmutableString, opts: rhai::Map| {
            one(&deps, &state, spec_from(&prompt, Some(&opts))?)
        });
    }
    {
        let deps = deps.clone();
        let state = state.clone();
        engine.register_fn("parallel", move |items: rhai::Array| many(&deps, &state, items));
    }
    // `pipeline` is variadic in dsh; Rhai needs a concrete arity, and three
    // stages is past the point where a plain `for` loop reads better anyway.
    {
        let state = state.clone();
        engine.register_fn(
            "pipeline",
            move |ctx: NativeCallContext, items: rhai::Array, a: FnPtr| {
                pipeline(&ctx, &state, items, &[a])
            },
        );
    }
    {
        let state = state.clone();
        engine.register_fn(
            "pipeline",
            move |ctx: NativeCallContext, items: rhai::Array, a: FnPtr, b: FnPtr| {
                pipeline(&ctx, &state, items, &[a, b])
            },
        );
    }
    {
        let state = state.clone();
        engine.register_fn(
            "pipeline",
            move |ctx: NativeCallContext, items: rhai::Array, a: FnPtr, b: FnPtr, c: FnPtr| {
                pipeline(&ctx, &state, items, &[a, b, c])
            },
        );
    }
}

/// A line for the operator, not for the model: `log` and `phase` describe
/// the run's progress, and the run's *result* is whatever the script prints.
fn note(events: &Arc<dyn EventSink>, message: &str) {
    events.emit(protocol::Event::LogLine {
        level:   "INFO".into(),
        message: format!("workflow: {message}"),
    });
}

/// Read one `parallel` element: a bare prompt string, or an option map with
/// a `prompt` field.
fn item_spec(item: &Dynamic) -> Result<AgentSpec, Box<EvalAltResult>> {
    if item.is_string() {
        let prompt = item.clone().into_string().unwrap_or_default();
        return spec_from(&prompt, None);
    }
    let Some(map) = item.clone().try_cast::<rhai::Map>() else {
        return Err(throw(
            "each `parallel` item must be a prompt string or a map with a `prompt` field"
                .to_string(),
        ));
    };
    let prompt = map
        .get("prompt")
        .map(|p| p.to_string())
        .ok_or_else(|| throw("a `parallel` item map needs a `prompt` field".to_string()))?;
    spec_from(&prompt, Some(&map))
}

/// Validate one agent request. Everything a script can get wrong here is
/// caught before a child is spawned, because the cheapest failed agent is
/// the one that never ran.
fn spec_from(prompt: &str, opts: Option<&rhai::Map>) -> Result<AgentSpec, Box<EvalAltResult>> {
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(throw(
            "`agent` needs a non-empty prompt — the child sees nothing of this \
             conversation, so say what to do, where, and what to report"
                .to_string(),
        ));
    }
    let mut spec =
        AgentSpec { prompt, label: None, schema: None, max_hops: DEFAULT_AGENT_HOPS };
    let Some(opts) = opts else { return Ok(spec) };

    if let Some(label) = opts.get("label") {
        let label = label.to_string();
        if !label.trim().is_empty() {
            spec.label = Some(crate::team::truncate_chars(label.trim(), 40));
        }
    }
    if let Some(hops) = opts.get("max_hops") {
        let n = hops.as_int().map_err(|_| {
            throw("`max_hops` must be a whole number of tool calls".to_string())
        })?;
        if !(1..=MAX_AGENT_HOPS).contains(&n) {
            return Err(throw(format!(
                "`max_hops` must be between 1 and {MAX_AGENT_HOPS}; got {n}"
            )));
        }
        spec.max_hops = n as u8;
    }
    if let Some(schema) = opts.get("schema").filter(|s| !s.is_unit()) {
        let json = script::to_json(schema);
        if !json.is_object() {
            return Err(throw(
                "`schema` must be a JSON Schema object, e.g. \
                 #{ type: \"object\", required: [\"status\"], properties: #{ … } }"
                    .to_string(),
            ));
        }
        spec.schema = Some(json);
    }
    Ok(spec)
}

/// Claim `n` children against the budget, returning the index the first one
/// gets. Reserved up front so a `parallel` that would overrun fails before
/// spending anything, rather than half-way through.
fn claim(state: &State, n: u32) -> Result<u32, Box<EvalAltResult>> {
    let used = state.spent.get();
    if used + n > MAX_AGENTS {
        return Err(throw(format!(
            "agent cap ({MAX_AGENTS}) reached — the workflow was stopped after \
             {used}; narrow the fan-out or split the work across calls"
        )));
    }
    state.spent.set(used + n);
    Ok(used)
}

/// Budget and name one child before it is spawned.
fn plan_one(state: &State, spec: &AgentSpec) -> Result<Member, Box<EvalAltResult>> {
    let index = claim(state, 1)?;
    Ok(member_of(state, spec, index))
}

/// The index a child was budgeted at is already unique within the run, so
/// it is also its member id — no second counter to keep in step.
fn member_of(state: &State, spec: &AgentSpec, index: u32) -> Member {
    Member {
        id:    index as u64 + 1,
        phase: state.phase.borrow().clone(),
        label: label_for(state, spec, index),
    }
}

/// Validate, budget and name a whole fan-out before any of it is spawned.
/// Separate from [`many`] so the caps, the spec parsing and the labels are
/// reachable without an LLM behind them.
fn plan(state: &State, items: &rhai::Array) -> Result<Vec<(AgentSpec, Member)>, Box<EvalAltResult>> {
    if items.len() > MAX_PARALLEL {
        return Err(throw(format!(
            "`parallel` runs at most {MAX_PARALLEL} agents at once; got {} — split the \
             fan-out into batches",
            items.len()
        )));
    }
    let mut specs = Vec::with_capacity(items.len());
    for item in items.iter() {
        specs.push(item_spec(item)?);
    }
    let first = claim(state, specs.len() as u32)?;
    Ok(specs
        .into_iter()
        .enumerate()
        .map(|(i, spec)| {
            let member = member_of(state, &spec, first + i as u32);
            (spec, member)
        })
        .collect())
}

/// `agent(…)`: one child, blocking, throwing on failure.
fn one(deps: &Deps, state: &Rc<State>, spec: AgentSpec) -> Result<Dynamic, Box<EvalAltResult>> {
    let member = plan_one(state, &spec)?;
    let label = member.label.clone();
    note(&deps.sub.events, &format!("{label} — starting"));
    match deps.handle.block_on(drive(deps, spec, member)) {
        Ok(value) => {
            note(&deps.sub.events, &format!("{label} — reported"));
            Ok(value)
        }
        Err(problem) => {
            warn!(%label, %problem, "workflow: agent failed");
            Err(throw(problem))
        }
    }
}

/// `parallel([…])`: several children at once. A failed child is `()` rather
/// than an abort — dsh's per-item null — because the whole point of a
/// fan-out is that one branch failing does not invalidate the others.
fn many(
    deps: &Deps,
    state: &Rc<State>,
    items: rhai::Array,
) -> Result<Dynamic, Box<EvalAltResult>> {
    if items.is_empty() {
        return Ok(Dynamic::from(rhai::Array::new()));
    }
    let labelled = plan(state, &items)?;
    note(
        &deps.sub.events,
        &format!("{} agent(s) in parallel — starting", labelled.len()),
    );

    let results = deps.handle.block_on(async {
        join_all(labelled.into_iter().map(|(spec, member)| {
            let deps = deps.clone();
            let label = member.label.clone();
            async move { (label, drive(&deps, spec, member).await) }
        }))
        .await
    });

    let mut out = rhai::Array::with_capacity(results.len());
    for (label, result) in results {
        match result {
            Ok(value) => out.push(value),
            Err(problem) => {
                warn!(%label, %problem, "workflow: parallel agent failed");
                // The script sees `()`; the operator sees why.
                note(&deps.sub.events, &format!("{label} — FAILED: {problem}"));
                out.push(Dynamic::UNIT);
            }
        }
    }
    Ok(Dynamic::from(out))
}

/// `pipeline(items, …stages)`: each item through each stage, in order.
/// Sequential — a stage is a script closure and Rhai runs on one thread, so
/// there is nothing to overlap. Use `parallel` inside a stage for width.
/// A stage returning `()` drops that item from the remaining stages, which
/// is how a per-item failure stops costing agents.
fn pipeline(
    ctx: &NativeCallContext,
    state: &Rc<State>,
    items: rhai::Array,
    stages: &[FnPtr],
) -> Result<Dynamic, Box<EvalAltResult>> {
    let mut carried = items;
    for (n, stage) in stages.iter().enumerate() {
        let mut next = rhai::Array::with_capacity(carried.len());
        for item in std::mem::take(&mut carried) {
            if item.is_unit() {
                next.push(Dynamic::UNIT);
                continue;
            }
            let produced: Dynamic = stage.call_within_context(ctx, (item,))?;
            next.push(produced);
        }
        carried = next;
        let live = carried.iter().filter(|v| !v.is_unit()).count();
        let phase = state.phase.borrow().clone();
        info!(stage = n + 1, live, %phase, "workflow: pipeline stage done");
    }
    Ok(Dynamic::from(carried))
}

/// The name this child appears under in the log: the script's `label` when
/// it gave one, and the current phase when it named one.
fn label_for(state: &State, spec: &AgentSpec, index: u32) -> String {
    let base = spec.label.clone().unwrap_or_else(|| format!("agent {}", index + 1));
    let phase = state.phase.borrow();
    if phase.is_empty() {
        base
    } else {
        format!("{phase}/{base}")
    }
}

/// Run one child conversation to its report. `Err` is a message the script
/// can read — either as a thrown error or, inside `parallel`, as a log line
/// standing behind a `()`.
async fn drive(deps: &Deps, spec: AgentSpec, member: Member) -> Result<Dynamic, String> {
    let label = member.label.clone();
    deps.edge(Some(&member), RunState::Started);
    let outcome = drive_inner(deps, spec, &label).await;
    deps.edge(
        Some(&member),
        if outcome.is_ok() { RunState::Done } else { RunState::Failed },
    );
    outcome
}

async fn drive_inner(deps: &Deps, spec: AgentSpec, label: &str) -> Result<Dynamic, String> {
    let structured = spec.schema.is_some();
    let run = RunSpec {
        label:          format!("workflow {label}"),
        system:         child_system(deps.catalogue.as_deref().map(String::as_str), spec.max_hops),
        // A workflow child is a fresh agent: what it needs to know is in the
        // prompt the script wrote, which is the whole reason the script had
        // to write it down.
        seed:           Vec::new(),
        task:           spec.prompt,
        max_hops:       spec.max_hops,
        schema:         spec.schema,
        call_seq_start: 0,
    };
    let mut transcript = runner::seed_transcript(&run);
    let report = runner::run_conversation(
        &deps.client,
        deps.registry.as_ref(),
        &deps.sub,
        &mut transcript,
        &run,
        &deps.cancel,
    )
    .await;

    let Some(report) = report else {
        return Err(
            "produced nothing (every LLM call failed, or the turn was interrupted)".to_string()
        );
    };
    if structured {
        let Some(value) = report.structured else {
            return Err(
                "never reported through `structured-output`, so it produced no result"
                    .to_string(),
            );
        };
        return Ok(script::from_json(&value));
    }

    // The script sees only this string, so the grounding caveat has to
    // travel with it: an all-prose child reads exactly like a researched
    // one, and a workflow's job is to concatenate several of them.
    let mut text = String::new();
    if !report.verified(false) {
        text.push_str(
            "UNVERIFIED — this agent made no successful tool call, so nothing below was \
             checked against the machine.\n\n",
        );
    }
    text.push_str(crate::team::truncate_chars(report.text.trim(), MAX_REPORT_CHARS).trim_end());
    Ok(Dynamic::from(text))
}

/// A workflow child's system prompt. Same grounding contract as a
/// `subagent`'s — the failure mode is identical — plus the line that says
/// nobody will follow up, because unlike a teammate it gets no second round.
fn child_system(catalogue: Option<&str>, max_hops: u8) -> String {
    let mut charter = String::from(
        "You are one agent inside a workflow running in the sica-rust desktop app, \
         working on ONE task handed to you by an orchestration script. You cannot \
         ask questions, you get no second round, and nobody reads anything but \
         your final report.\n\n",
    );
    if let Some(cat) = catalogue {
        charter.push_str(&format!(
            "You may use tools. To call one, reply with a SINGLE line of exactly \
             this form and nothing else:\n\n\
             <skill-name> '<arg1>' '<arg2>' > <what you want to learn>\n\n\
             One tool call per reply, at most {max_hops} in total. When you have \
             what you need, reply with your final report as plain text containing \
             no tool-call line.\n\n\
             Available skills:\n{cat}\n\n"
        ));
    }
    charter.push_str(
        "Keep your final report concise and factual. Quote exact values (numbers, \
         paths, errors) verbatim from tool output, and name the files you actually \
         opened. Start directly with content — no preamble.\n\n\
         Grounding rule, and it is absolute: state a file's contents, a command's \
         output, or whether a path exists ONLY if a tool result in this \
         conversation shows it. You cannot see the disk otherwise. If you have not \
         run the tool, write `unverified:` in front of the claim and name the call \
         you would need — never invent output, and never report a file as existing \
         because the name sounds plausible.",
    );
    charter
}

fn fail(msg: &str) -> SkillOutcome {
    SkillOutcome { ok: false, summary: msg.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::Concurrency;

    struct Silent;
    impl EventSink for Silent {
        fn emit(&self, _e: protocol::Event) {}
    }

    /// Run a script with no LLM behind it. Every `agent` call therefore
    /// fails, which is exactly what the control-flow tests want to observe;
    /// the tests that care about a *successful* child are the replay
    /// scenario's job, not a unit test's.
    fn run(code: &str) -> ScriptResult {
        let limits = script::Limits {
            operations: MAX_OPERATIONS,
            budget:     Duration::from_secs(5),
            output_max: MAX_OUTPUT,
        };
        let mut sandbox =
            script::Sandbox::new(limits, None, Instant::now() + Duration::from_secs(5));
        sandbox.constant("args", Dynamic::from("the input".to_string()));
        let state = Rc::new(State { spent: Cell::new(0), phase: RefCell::new(String::new()) });
        register_stub(&mut sandbox.engine, &state);
        let out = sandbox.run(code);
        ScriptResult { output: out.output, error: out.error, agents: state.spent.get() }
    }

    /// The real bindings, with only [`drive`] — the part that needs a
    /// provider — replaced by a deterministic double: a prompt containing
    /// `boom` fails, anything else reports `report(<prompt>)`. Everything
    /// else is production code: [`spec_from`], [`plan`], [`claim`],
    /// [`label_for`], [`pipeline`] and the `()`-on-failure rule.
    fn register_stub(engine: &mut Engine, state: &Rc<State>) {
        {
            let state = state.clone();
            engine.register_fn("phase", move |t: ImmutableString| {
                *state.phase.borrow_mut() = t.to_string();
            });
        }
        engine.register_fn("log", |_m: ImmutableString| {});
        {
            let state = state.clone();
            engine.register_fn("agent", move |p: ImmutableString| {
                let spec = spec_from(&p, None)?;
                let label = plan_one(&state, &spec)?;
                stub_drive(&spec, &label.label).map_err(throw)
            });
        }
        {
            let state = state.clone();
            engine.register_fn("agent", move |p: ImmutableString, o: rhai::Map| {
                let spec = spec_from(&p, Some(&o))?;
                let label = plan_one(&state, &spec)?;
                stub_drive(&spec, &label.label).map_err(throw)
            });
        }
        {
            let state = state.clone();
            engine.register_fn("parallel", move |items: rhai::Array| {
                let mut out = rhai::Array::new();
                for (spec, label) in plan(&state, &items)? {
                    out.push(stub_drive(&spec, &label.label).unwrap_or(Dynamic::UNIT));
                }
                Ok::<Dynamic, Box<EvalAltResult>>(Dynamic::from(out))
            });
        }
        {
            let state = state.clone();
            engine.register_fn(
                "pipeline",
                move |ctx: NativeCallContext, items: rhai::Array, a: FnPtr| {
                    pipeline(&ctx, &state, items, &[a])
                },
            );
        }
        {
            let state = state.clone();
            engine.register_fn(
                "pipeline",
                move |ctx: NativeCallContext, items: rhai::Array, a: FnPtr, b: FnPtr| {
                    pipeline(&ctx, &state, items, &[a, b])
                },
            );
        }
    }

    fn stub_drive(spec: &AgentSpec, label: &str) -> Result<Dynamic, String> {
        if spec.prompt.contains("boom") {
            return Err(format!("{label} exploded"));
        }
        if spec.schema.is_some() {
            let mut map = rhai::Map::new();
            map.insert("status".into(), Dynamic::from("complete".to_string()));
            map.insert("label".into(), Dynamic::from(label.to_string()));
            return Ok(Dynamic::from(map));
        }
        Ok(Dynamic::from(format!("report({})", spec.prompt)))
    }

    #[test]
    fn a_workflow_reports_only_what_it_prints() {
        let out = run("let r = agent(\"look at alpha\"); print(r);");
        assert_eq!(out.error, None);
        assert_eq!(out.output, "report(look at alpha)\n");
        assert_eq!(out.agents, 1);
    }

    #[test]
    fn the_input_argument_reaches_the_script_as_args() {
        let out = run("print(agent(`summarise ${args}`));");
        assert_eq!(out.error, None);
        assert!(out.output.contains("summarise the input"), "{}", out.output);
    }

    #[test]
    fn a_failing_agent_throws_and_is_catchable() {
        let out = run(
            "try { agent(\"boom\"); print(\"unreachable\"); } \
             catch (err) { print(`caught: ${err}`); }",
        );
        assert_eq!(out.error, None);
        assert!(out.output.contains("caught:"), "{}", out.output);
        assert!(!out.output.contains("unreachable"), "{}", out.output);
    }

    #[test]
    fn an_uncaught_agent_failure_ends_the_workflow() {
        let out = run("agent(\"boom\"); print(\"never\");");
        let err = out.error.expect("an uncaught failure must fail the run");
        assert!(err.contains("exploded"), "{err}");
        assert!(!out.output.contains("never"), "{}", out.output);
    }

    #[test]
    fn a_failed_child_in_parallel_is_a_unit_not_an_abort() {
        let out = run(
            "let rs = parallel([\"alpha\", \"boom\", \"gamma\"]); \
             for r in rs { print(if r == () { \"missing\" } else { r }); }",
        );
        assert_eq!(out.error, None);
        assert_eq!(out.output, "report(alpha)\nmissing\nreport(gamma)\n");
        assert_eq!(out.agents, 3);
    }

    #[test]
    fn parallel_accepts_option_maps_beside_bare_prompts() {
        let out = run(
            "let rs = parallel([\"plain\", #{ prompt: \"mapped\", label: \"scout\" }]); \
             print(rs.len());",
        );
        assert_eq!(out.error, None);
        assert_eq!(out.output, "2\n");
    }

    #[test]
    fn a_fan_out_wider_than_the_limit_is_refused_before_anything_runs() {
        let out = run("parallel([\"a\",\"b\",\"c\",\"d\",\"e\",\"f\",\"g\",\"h\",\"i\"]);");
        let err = out.error.expect("an over-wide fan-out must fail");
        assert!(err.contains("at most 8"), "{err}");
        assert_eq!(out.agents, 0, "nothing may be spent on a refused fan-out");
    }

    #[test]
    fn the_agent_cap_stops_a_runaway_workflow() {
        let out = run("let n = 0; while true { agent(`step ${n}`); n += 1; }");
        let err = out.error.expect("a runaway workflow must be stopped");
        assert!(err.contains("agent cap"), "{err}");
        assert_eq!(out.agents, MAX_AGENTS);
    }

    #[test]
    fn a_schema_turns_the_report_into_an_indexable_map() {
        let out = run(
            "let r = agent(\"check\", #{ schema: #{ type: \"object\" } }); \
             print(r.status);",
        );
        assert_eq!(out.error, None);
        assert_eq!(out.output, "complete\n");
    }

    #[test]
    fn a_schema_that_is_not_an_object_is_refused() {
        let out = run("agent(\"check\", #{ schema: \"object\" });");
        let err = out.error.expect("a non-object schema must fail");
        assert!(err.contains("JSON Schema object"), "{err}");
    }

    #[test]
    fn max_hops_is_bounded_in_both_directions() {
        let low = run("agent(\"x\", #{ max_hops: 0 });").error.expect("0 hops must fail");
        assert!(low.contains("between 1 and 24"), "{low}");
        let high = run("agent(\"x\", #{ max_hops: 99 });").error.expect("99 hops must fail");
        assert!(high.contains("between 1 and 24"), "{high}");
    }

    #[test]
    fn an_empty_prompt_is_refused_before_a_child_is_spawned() {
        let out = run("agent(\"   \");");
        let err = out.error.expect("an empty prompt must fail");
        assert!(err.contains("non-empty prompt"), "{err}");
        assert_eq!(out.agents, 0);
    }

    #[test]
    fn a_label_and_a_phase_both_reach_the_child() {
        let out = run(
            "phase(\"survey\"); \
             print(agent(\"x\", #{ label: \"scout\", schema: #{ type: \"object\" } }).label);",
        );
        assert_eq!(out.error, None);
        assert_eq!(out.output, "survey/scout\n");
    }

    #[test]
    fn a_pipeline_carries_each_item_through_every_stage() {
        let out = run(
            "let rs = pipeline([\"a\", \"b\"], |x| agent(`read ${x}`), |r| r.len()); \
             print(rs);",
        );
        assert_eq!(out.error, None);
        assert_eq!(out.output, "[14, 14]\n");
        assert_eq!(out.agents, 2);
    }

    #[test]
    fn a_pipeline_stage_returning_unit_drops_the_item_from_later_stages() {
        let out = run(
            "let rs = pipeline([\"keep\", \"drop\"], \
                               |x| if x == \"drop\" { () } else { x }, \
                               |x| agent(`read ${x}`)); \
             print(rs);",
        );
        assert_eq!(out.error, None);
        assert!(out.output.contains("report(read keep)"), "{}", out.output);
        assert_eq!(out.agents, 1, "the dropped item must not cost an agent");
    }

    #[test]
    fn the_script_has_no_tools_of_its_own() {
        for code in ["read_file(\"a\");", "tool(\"read-file\");", "run_cli(\"ls\");"] {
            let err = run(code).error.unwrap_or_else(|| panic!("{code} must not resolve"));
            assert!(err.contains("Function not found"), "{code}: {err}");
        }
    }

    #[test]
    fn the_sdk_documents_every_bound_verb() {
        for verb in ["agent(", "parallel(", "pipeline(", "phase(", "log(", "args"] {
            assert!(SDK.contains(verb), "the SDK never mentions `{verb}`");
        }
        assert!(SDK.contains("no tools"), "the SDK must say the script has no tools");
    }

    #[test]
    fn a_workflow_child_cannot_start_another_workflow() {
        assert!(
            crate::control::CHILD_EXCLUDED.contains(&WORKFLOW_NAME),
            "a delegated child must not reach `workflow`"
        );
    }

    #[test]
    fn the_skill_declares_its_arguments() {
        let w = Workflow::new();
        assert_eq!(w.name(), WORKFLOW_NAME);
        assert_eq!(w.positional_args(), vec!["script".to_string()]);
        assert_eq!(w.optional_args(), vec!["input".to_string()]);
        assert!(!w.trusted());
        assert!(matches!(w.concurrency(&Value::Null), Concurrency::Exclusive));
    }

    #[test]
    fn a_workflow_without_a_script_says_so() {
        let ctx = SkillContext { sub: ToolSubAgent::root(Arc::new(Silent)) };
        let out = futures::executor::block_on(
            Workflow::new().run(serde_json::json!({"script": "  "}), ctx),
        );
        assert!(!out.ok);
        assert!(out.summary.contains("needs a `script`"), "{}", out.summary);
    }

    /// The one test that runs the real [`Workflow::run`] all the way
    /// through: `spawn_blocking`, the sandbox, the output capture and
    /// [`finish`]. It calls no agent, so it needs no provider — the client
    /// only has to exist, because a workflow refuses to start without one.
    #[test]
    fn the_blocking_runtime_path_runs_a_script_end_to_end() {
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let out = rt.block_on(async {
            let sub = ToolSubAgent::root(Arc::new(Silent))
                .with_summarizer(LlmClient::new("http://127.0.0.1:1/v1", "none", None));
            Workflow::new()
                .run(
                    serde_json::json!({
                        "script": "print(`args=[${args}]`); 6 * 7",
                        "input":  "seven",
                    }),
                    SkillContext { sub },
                )
                .await
        });
        assert!(out.ok, "{}", out.summary);
        assert_eq!(out.summary, "args=[seven]\nResult: 42");
    }

    #[test]
    fn a_workflow_without_an_llm_says_so_rather_than_running_an_empty_script() {
        let ctx = SkillContext { sub: ToolSubAgent::root(Arc::new(Silent)) };
        let out = futures::executor::block_on(
            Workflow::new().run(serde_json::json!({"script": "agent(\"x\");"}), ctx),
        );
        assert!(!out.ok);
        assert!(out.summary.contains("needs an LLM"), "{}", out.summary);
    }
}
