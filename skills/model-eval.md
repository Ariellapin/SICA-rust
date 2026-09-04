---
name: model-eval
description: Benchmark the connected model against a prompt suite and report which prompt changes would fix the failures.
---
Replay a fixed suite of prompts against the **currently connected** model,
score every reply against declarative checks, and write a report saying what
to change to make the model behave better.

Invocation (single line):

    model-eval '<suite>' '<repeats>' '<filter>' > <what you want to know>

Examples:

    model-eval > overall score and the worst category
    model-eval 'default' '3' > which cases are flaky
    model-eval 'default' '2' 'tool-syntax' > do tool calls parse after my memory.md edit

Arguments (all optional):
- `suite` — a name under `evals/` (`default` → `evals/default.toml`) or a
  path to a `.toml` file. Empty means `default`.
- `repeats` — how many times each case runs, 1–5 (suite default is 2). More
  than one repeat is what turns a pass/fail into a **pass rate**, which is
  the only way to see non-determinism.
- `filter` — substring matched against each case's `id` and `category`; only
  matching cases run.

What it does:
- Rebuilds the real system prompt (`memory.md` + the live skill catalogue),
  so the run measures the shipped configuration.
- Sends each case as its own single-turn conversation — no history, so cases
  cannot contaminate each other.
- Scores replies with the **same** tool-call parser the backend dispatches
  through. A case that expects a tool call passes only if the backend would
  really have run that call.
- **Never executes a skill.** Tool-call cases are parse-only, so a suite is
  safe to run unattended.
- Writes `evals/reports/<suite>-<timestamp>.md` (full detail, every failing
  reply excerpt) and `<suite>-<timestamp>.json` (the baseline the next run
  diffs against), then returns a compact score + fix list.

Suite format — `evals/default.toml`:

    [suite]
    name        = "default"
    description = "what this suite is for"
    system      = "live"   # "live" = memory.md + skill catalogue, "none", or literal prompt text
    repeats     = 2
    # temperature = 0.0    # optional override; omitted = the connection's own setting

    [[case]]
    id       = "read-file-basic"
    category = "tool-syntax"
    prompt   = "Read the file skills/read-file.md and say which arguments it declares."
    expect_tool         = ["read-file"]
    expect_args_contain = ["read-file.md"]
    single_tool_call    = true

Checks (all optional, all combinable):
- `expect_tool` — string or list; the reply must parse as a call to one of them.
- `expect_args_contain` / `min_args` — argument content and count.
- `require_expectation` — the ` > <expectation>` clause must be present
  (defaults to true whenever `expect_tool` is set).
- `expect_no_tool` — the reply must contain no tool call *and* nothing
  tool-call-shaped that the parser rejected.
- `single_tool_call` — at most one call per message (only the first ever runs).
- `contains` / `contains_any` / `not_contains` — case-insensitive substrings.
- `regex` / `not_regex` — full regex syntax, validated when the suite loads.
- `json` — the reply (or its single fenced block) must parse as JSON.
- `min_words` / `max_words` — length contract.
- `judge` — a rubric graded by a second LLM call. The judge is the *same*
  model, so it is the weakest signal in the report and is labelled as such.

Reading the report: every failure is bucketed (missing call, malformed call,
wrong skill, bad args, over-calling, content, format, length, judge) and each
bucket names the lever that fixes it — a `memory.md` section, a skill
description, or the sampling temperature. A case that passes some repeats and
fails others is flagged as flaky: that is a sampling problem, not a prompt one.
