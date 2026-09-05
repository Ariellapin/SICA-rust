//! The shared script sandbox behind `run-code` (§7) and `workflow` (§12.5).
//!
//! Both skills hand a model-written script to [Rhai](https://rhai.rs) and
//! run it on a blocking thread. Neither gives the script a capability of its
//! own: no filesystem, no network, no clock, no module imports, no `eval`.
//! Everything a script can reach arrives as a host function its owner
//! registers — the skill catalogue for `run-code`, the delegation
//! primitives for `workflow`.
//!
//! What is left over is the same both times: the limits, the printed-output
//! capture, the abort reasons the progress hook raises, and the Rhai↔JSON
//! conversions. They live here so the two runtimes cannot drift apart, and
//! so replacing Rhai with a JavaScript engine later is one seam rather than
//! two.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use rhai::{Dynamic, Engine, EvalAltResult, Position};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

/// The caps one script runs under. The two callers set very different ones:
/// a `run-code` program loops over files and is cheap per operation, while a
/// `workflow` spends a whole agent conversation per step and should be doing
/// almost no computing of its own.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Rhai operation cap. A tight infinite loop burns this in well under a
    /// second, so it costs a legitimate script nothing and bounds a runaway.
    pub operations: u64,
    /// Wall-clock budget, enforced from `on_progress`. Set it shorter than
    /// the owning [`crate::skill::Skill::timeout`] so the script aborts
    /// itself — keeping whatever it printed — before the pipeline abandons
    /// the call.
    pub budget:     Duration,
    /// Cap on captured output. The sandbox stops appending at twice this, so
    /// a runaway `print` cannot exhaust memory before [`window`] trims it.
    pub output_max: usize,
}

/// What a finished script produced: everything it printed, plus how it
/// ended.
pub struct Outcome {
    pub output: String,
    pub error:  Option<String>,
}

/// A configured engine and its output buffer. Build one, register the host
/// functions the caller wants to expose on [`Sandbox::engine`], then
/// [`Sandbox::run`].
pub struct Sandbox {
    pub engine: Engine,
    printed:    Rc<RefCell<String>>,
    limits:     Limits,
    scope:      rhai::Scope<'static>,
}

impl Sandbox {
    pub fn new(limits: Limits, cancel: Option<CancellationToken>, deadline: Instant) -> Self {
        let mut engine = Engine::new();
        // No filesystem, no module imports, no `eval` — a script's only
        // capabilities are the host functions its owner registers.
        engine.set_module_resolver(rhai::module_resolvers::DummyModuleResolver::new());
        engine.disable_symbol("eval");
        engine.set_max_operations(limits.operations);
        engine.set_max_call_levels(24);
        engine.set_max_expr_depths(64, 32);
        engine.set_max_string_size(4 * 1024 * 1024);
        engine.set_max_array_size(100_000);
        engine.set_max_map_size(100_000);

        let printed = Rc::new(RefCell::new(String::new()));
        {
            let sink = printed.clone();
            let cap = limits.output_max;
            engine.on_print(move |s| append(&sink, cap, s));
        }
        {
            let sink = printed.clone();
            let cap = limits.output_max;
            engine.on_debug(move |s, _src, pos| append(&sink, cap, &format!("[debug {pos}] {s}")));
        }
        // Polled between operations. Reading the clock on every one of two
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

        Self { engine, printed, limits, scope: rhai::Scope::new() }
    }

    /// A handle on the output buffer, so a host function can add to what the
    /// script "printed" without going through `print`.
    pub fn output(&self) -> Rc<RefCell<String>> {
        self.printed.clone()
    }

    /// Put a read-only value in the script's scope — an input the script
    /// reads by name rather than calls for.
    pub fn constant(&mut self, name: &str, value: Dynamic) {
        self.scope.push_constant_dynamic(name.to_string(), value);
    }

    /// Evaluate `code` and collect what it left behind. A non-unit final
    /// value is appended as `Result: …` — a script whose last expression is
    /// its answer should not also have to print it.
    pub fn run(self, code: &str) -> Outcome {
        let Sandbox { engine, printed, limits, mut scope } = self;
        let evaluated = engine.eval_with_scope::<Dynamic>(&mut scope, code);
        let mut output = printed.borrow().clone();
        let error = match evaluated {
            Ok(value) => {
                if !value.is_unit() {
                    if !output.is_empty() && !output.ends_with('\n') {
                        output.push('\n');
                    }
                    output.push_str(&format!("Result: {value}"));
                }
                None
            }
            Err(e) => Some(describe(&e, &limits)),
        };
        Outcome { output, error }
    }
}

/// Append one line to a script's output, stopping at twice the cap: past
/// that the text is going to be trimmed anyway, and the only thing still
/// growing is memory.
pub fn append(sink: &Rc<RefCell<String>>, cap: usize, line: &str) {
    let mut buf = sink.borrow_mut();
    if buf.len() < cap * 2 {
        buf.push_str(line);
        buf.push('\n');
    }
}

/// The error a host function returns to abort the script. It is an ordinary
/// Rhai runtime error, so `try`/`catch` catches it — which is what lets a
/// script decide for itself whether a failed step is fatal.
pub fn throw(message: String) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorRuntime(Dynamic::from(message), Position::NONE))
}

/// A script error as the model should read it. The abort reasons the
/// progress hook raises are harness facts, not Rhai syntax, so they get
/// their own wording — "terminated: deadline" would tell the model nothing
/// about what to do differently.
pub fn describe(err: &EvalAltResult, limits: &Limits) -> String {
    match err {
        EvalAltResult::ErrorTerminated(token, _) => match token.to_string().as_str() {
            "interrupted" => "the turn was interrupted — the script was stopped".into(),
            "deadline" => format!(
                "the script exceeded its {}s wall-clock budget and was stopped",
                limits.budget.as_secs()
            ),
            other => format!("the script was stopped ({other})"),
        },
        EvalAltResult::ErrorTooManyOperations(_) => format!(
            "the script exceeded its operation cap ({}) and was stopped — it is \
             probably looping",
            limits.operations
        ),
        other => other.to_string(),
    }
}

/// Trim `output` to `max` bytes, keeping the head and the tail. `label`
/// names the elided middle in the notice the model reads.
pub fn window(output: String, max: usize, label: &str) -> String {
    if output.len() <= max {
        return output;
    }
    sica_core::retain::head_tail(&output, max * 3 / 4, max / 4).render(label)
}

/// Rhai → JSON. Arrays and maps convert structurally so a typed tool can be
/// handed `#{ paths: ["a", "b"] }`; anything exotic degrades to its display
/// form rather than failing the call.
pub fn to_json(d: &Dynamic) -> Value {
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

/// JSON → Rhai, for a structured result the script is meant to index into
/// (`report.status`) rather than re-parse out of a string.
pub fn from_json(v: &Value) -> Dynamic {
    match v {
        Value::Null => Dynamic::UNIT,
        Value::Bool(b) => Dynamic::from(*b),
        Value::Number(n) => n
            .as_i64()
            .map(Dynamic::from)
            .or_else(|| n.as_f64().map(Dynamic::from))
            .unwrap_or(Dynamic::UNIT),
        Value::String(s) => Dynamic::from(s.clone()),
        Value::Array(a) => Dynamic::from(a.iter().map(from_json).collect::<rhai::Array>()),
        Value::Object(o) => {
            let mut map = rhai::Map::new();
            for (k, val) in o {
                map.insert(k.as_str().into(), from_json(val));
            }
            Dynamic::from(map)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn limits() -> Limits {
        Limits { operations: 10_000, budget: Duration::from_secs(5), output_max: 1024 }
    }

    fn sandbox() -> Sandbox {
        Sandbox::new(limits(), None, Instant::now() + Duration::from_secs(5))
    }

    #[test]
    fn a_script_reports_what_it_printed_and_what_it_returned() {
        let out = sandbox().run("print(\"hello\"); 40 + 2");
        assert_eq!(out.error, None);
        assert_eq!(out.output, "hello\nResult: 42");
    }

    #[test]
    fn a_unit_result_adds_nothing_to_the_output() {
        let out = sandbox().run("print(\"only this\");");
        assert_eq!(out.output, "only this\n");
    }

    #[test]
    fn the_operation_cap_reads_as_a_harness_fact_not_a_rhai_error() {
        let out = sandbox().run("let i = 0; while true { i += 1; }");
        let err = out.error.expect("a runaway loop must fail");
        assert!(err.contains("operation cap"), "{err}");
        assert!(err.contains("10000"), "{err}");
    }

    #[test]
    fn an_interrupt_stops_the_script_and_says_so() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let s = Sandbox::new(limits(), Some(cancel), Instant::now() + Duration::from_secs(5));
        let err = s.run("let i = 0; while true { i += 1; }").error.expect("must stop");
        assert!(err.contains("interrupted"), "{err}");
    }

    #[test]
    fn a_passed_deadline_stops_the_script_and_names_the_budget() {
        let s = Sandbox::new(limits(), None, Instant::now() - Duration::from_secs(1));
        let err = s.run("let i = 0; while true { i += 1; }").error.expect("must stop");
        assert!(err.contains("5s wall-clock budget"), "{err}");
    }

    #[test]
    fn a_script_cannot_import_a_module() {
        let err = sandbox().run("import \"std\" as s;").error.expect("imports are refused");
        assert!(err.to_lowercase().contains("module"), "{err}");
    }

    #[test]
    fn a_constant_is_readable_by_name() {
        let mut s = sandbox();
        s.constant("args", Dynamic::from("the input".to_string()));
        let out = s.run("print(`got ${args}`);");
        assert_eq!(out.error, None);
        assert_eq!(out.output, "got the input\n");
    }

    #[test]
    fn json_survives_a_round_trip_through_rhai() {
        let value = json!({
            "status": "complete",
            "count": 3,
            "ok": true,
            "steps": ["a", "b"],
            "nested": {"k": "v"},
        });
        assert_eq!(to_json(&from_json(&value)), value);
    }

    #[test]
    fn output_past_the_cap_keeps_the_head_and_the_tail() {
        let long = format!("{}\nTAIL", "x".repeat(4000));
        let trimmed = window(long, 200, "script output");
        assert!(trimmed.len() < 600, "{}", trimmed.len());
        assert!(trimmed.ends_with("TAIL"), "{trimmed}");
    }
}
