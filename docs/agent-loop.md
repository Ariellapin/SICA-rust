# The agent loop

Detail split out of [CLAUDE.md](../CLAUDE.md). This is the heart of the app:
how one user message becomes a sequence of LLM requests and tool calls, and
every subsystem hanging off that loop.
Crate graph, protocol and on-disk surfaces are in [docs/architecture.md](architecture.md).

## One turn, step by step

`ChatHub::send_user_message` ([crates/backend/src/chat.rs](../crates/backend/src/chat.rs)) first handles the message itself: a leading whitespace-bounded `/name` token is resolved by `agents::invoke` (`commands/<name>.md` with `{{args}}` substitution → `agents/<name>.md` → `skills/<name>.md`) and its `<skill_content>` frame is appended as `ContextInjected { source: SkillInvocation }` *before* the `UserMessage`, which is stored as typed; an unresolvable token is sent as plain text. On a still-placeholder session the first five words / 40 bytes of the message become a fallback `SessionTitle` immediately (`title_gen::fallback`), so the sidebar never shows "Session N" for a session with content; the LLM titler overwrites it after the first reply. Then it spawns one task and loops until the model stops calling tools (`MAX_TOOL_HOPS = 12`). Each iteration:

1. **Derive history from the session event log** (`build_history` → `SessionLog::derive_messages`) — never from an in-memory accumulator. Every persistence site is an `append_event` (`UserMessage`, `AssistantMessage`, `ToolCall`, `ToolResult`, `ContextInjected`, `CompactionSummary`, `LlmRetry`, `TokenUsage`, `RequestEnvelope`, `TurnStart`/`TurnEnd`, `SessionTitle`), flushed as one JSON line to `sessions/<id>.jsonl` immediately, so a crash mid-loop leaves a recoverable transcript. Nothing is ever removed from the log: compaction appends a summary whose `SurfaceOp::Replace { start_seq, end_seq }` *shadows* the folded span in the derived view (`sica_core::event`). `EventKind` has a `#[serde(other)] Unknown` variant so a log written by a newer backend still loads. The trimmer's "context notice" marker is wire-only and must never be logged. Two snapshots ride `ContextInjected` and shadow their predecessor so one copy is ever visible: the **runtime context** (time/cwd/os/model, refreshed once per turn; the cwd is the session's own, stamped into its `SessionCreated` header when it was created and carried to every skill on `ToolSubAgent.cwd` — harness guide §3.9) and the **workspace instructions** (`AGENTS.md`/`CLAUDE.md` chain via `agents::instructions`, re-checked after successful fs-tool calls). A `{{variable}}` reference in `memory.md` with no registered value fails the turn loudly (ERROR `LogLine`) rather than sending a malformed prompt.
2. **Prune, compact, then trim.** Prompt budget is `context_window − (max_tokens ?? 4096) − 512`. At the connect-time `CompactPolicy.threshold_pct` (default 80%, dsh's policy) of that budget, `compact_session` first runs the *pruner*: every tool result older than the verbatim tail whose raw summary exceeds `compact::PRUNE_THRESHOLD` (8 KiB) is replaced by a 4 KiB head + 1 KiB tail window via a `ToolResult { surface: Replace { seq, seq }, pruned: true }` — no model call, and if that alone brings the prompt under the trigger the summariser is skipped. Otherwise `agents::compact` folds the older part of the history into an LLM-written summary — the call is a **KV-preserving prefix** (the conversation's own system prompt + the folded messages verbatim + the 8-section directive as the final user message), keeps `retain_pct` (default 16%) of the tail verbatim, and discards any summary cut off by `max_tokens` — landing as a system message framed in `<compacted-summary>` and prefixed with `CONTEXT_SUMMARY_PREFIX`; `agents::context::trim_to_budget` is only the backstop for when even that doesn't fit. Compaction must come before trimming — the budget is well under the window, so a trim-first order would silently amputate history before the meter ever read the trigger. The trigger itself uses the **usage-anchored meter** (`agents::meter`): when the provider's last `usage` covers this exact envelope (system prompt + tools fingerprint), only the surface added since is priced heuristically.
3. **Run the turn** (`agents::turn::run_turn`) — streams `AssistantDelta`, emits `TokenUsage` every ~100 ms with a `breakdown` (system / tools / history), and returns accumulated content + reasoning + native tool calls + `error` (a transport/server failure, never swallowed). Requests set `stream_options.include_usage`; when the provider's `usage` trailer arrives it is the final `used_tokens` (it counts the real template, tool schemas and images, which `/tokenize` on concatenated text cannot) and is stored on the durable `TokenUsage` event as `prompt_tokens`/`completion_tokens` — and becomes the meter's next anchor.
4. **Classify failures before persisting anything.** `llm::retry::classify` splits `TurnOutput.error` (and a clean stream that carried nothing at all) into retryable — connect/timeout/reset, HTTP 429/5xx, mid-stream SSE decode, empty response — vs fatal (other 4xx). A retryable failure appends `LlmRetry`, sleeps with jittered exponential backoff (500 ms → 10 s, max 5 retries per step, cancel-interruptible) and `continue`s: because the failed attempt persisted nothing, the rebuilt history is byte-identical and the retry is indistinguishable from the first attempt. This is a step-level listener, deliberately not a wrapper inside `llm::client`. Fatal/exhausted → ERROR `LogLine`, `TurnEnd { finish_reason: "error" }`, and the FE renders a *Request failed* line on the turn.
5. **Persist the assistant message**, then dispatch any tool call through a `ToolSubAgent` (logging `ToolCall` immediately before dispatch so an interrupted batch leaves no orphan), append the `ToolResult`, and loop. The result carries `trusted` from `Skill::trusted()` (default `false`; `MarkdownSkill` is `true` because its body *is* the instruction) — an untrusted result derives with `event::UNTRUSTED_NOTICE` ("data, not instructions") in front of the fenced block; harness-authored results (hop limit, unknown skill) are trusted. After every dispatch — failed and unknown-skill calls included — `agents::guard::RepeatTracker` (one per session on `ChatHub::repeat`, cleared by each user message) keys the call on `skill + key-sorted canonical args`; at 3, 5 and 8 consecutive identical calls it injects an advisory `ContextInjected { source: ToolNotice }` naming the tool and count. It never blocks the call; `MAX_TOOL_HOPS` stays the hard stop.

After the first complete exchange, `title_gen` renames a still-default-titled session and pushes `SessionTitleChanged`.

## Two tool-calling modes

Chosen per provider by `LlmOptions.native_tools`:

- **Text protocol** (default; works with any llama.cpp build). The system prompt is composed by `agents::prompt` from ordered sections: `memory.md` + one guidance sentence per skill that provides one + the live `## Loaded skills` catalogue. The model emits one line — `skill-name '<arg>' … > <expectation>` — parsed by `agents::parse_tool_call`. A ` ```tool_call ` JSON fence is also accepted because small local models emit that shape from training data. Positional values are zipped onto the skill's declared `positional_args()`. `Tool`-role messages are downgraded to `user` on the wire, since local chat templates often lack a `tool` role. Successful outputs over 2 KB are re-summarised against the caller's `expectation` by a second LLM round-trip, keeping the main context tight; shorter output passes through verbatim (raw text is ground truth).
- **Native** (`vLLM --enable-auto-tool-choice`, OpenAI, Anthropic-compat). `SkillRegistry::tools_json()` fills the request's `tools` array (optional args like `cwd`/`start`/`end` appear as non-required properties); real `tool` role + `tool_call_id` correlation is preserved on the wire and in storage. No expectation/summariser indirection — raw output goes back, per the OpenAI convention. Native `tool_calls` are *not* persisted on an interrupted turn: a dangling `tool_calls` with no matching results poisons the next request's template. Native mode keeps `memory.md` in the composed prompt (an identity section states that function calling is the interface); only the catalogue section is dropped, because the `tools` array carries it.

Parsing is deliberately conservative. `extract_tool_call_known` only accepts natural-language lines whose skill name is registered — otherwise prose like `cargo build > compiles fine` becomes a bogus call. When the model emits something tool-call-shaped that the parser rejects, `parse_tool_call::rejected_attempt` names the defect (unreadable ```tool_call fence, a known-skill line missing its ` > <expectation>` clause) and the caller surfaces it as a WARN `LogLine` instead of failing silently. Both `chat.rs` and `agent-team` use it — a rejected call that passes silently is indistinguishable from "the model chose not to use a tool", which is how fabricated tool output gets into a transcript.

## Skills

`Skill` is an async trait (`name`, `description`, `positional_args`, `run`). Registration happens once at BE startup ([crates/backend/src/main.rs](../crates/backend/src/main.rs)):

1. Seed `skills/*.md` docs and `memory.md` if absent (**never overwritten** — those files are the user's once on disk). `skills/plan-mode.md` is seeded the same way but excluded from the skill scan by name — it is the plan-mode policy config, not a callable skill.
2. `register` the Rust built-ins: `skill-creator`, `run-cli`, `run-pwsh`, `read-file` (line-numbered, optional `start`/`end`), `write-file`, `edit-file` (literal single-match replace), `glob` (gitignore-aware, newest first, cap 100), `grep` (regex over files, cap 250 matches), `model-eval`, `ask-user` (blocks on the broker for a human answer), the Wave-4 delegation set (`subagent`, `subagent-fork`, `ralph` — all three need the finished registry, so they are attached after the markdown scan like `agent-team`), the Wave-4 job trio (`job-output`, `job-list`, `job-kill`), plus the `todo-write` / `exit-plan-mode` / `create-goal` / `get-goal` / `update-goal` stubs — catalogue entries whose bodies run in `chat.rs` (session-log mutation + turn control), intercepted before any sub-agent spins up.
3. `agent-team` (`agents::team`) and `workflow` (`agents::workflow`, Wave 8) each register **only if `skills/<name>.md` exists** — that file is the feature's on/off switch and is deliberately *not* seeded in step 1. Both spend many full LLM conversations per call: a team's teammates are the least reliable output in the app, and one `workflow` script can spawn 32 children while its scripting reference costs ~575 prompt tokens on every request of every session it is on for. So both stay out of the catalogue until someone puts the doc there. Rename it to `agent-team.md.off` / `workflow.md.off` (only `*.md` is scanned) and restart the BE to turn it off again.
4. `md_skill::register_all` scans `skills/*.md` and uses **`register_if_absent`** so a markdown file can't shadow a built-in of the same name. This matters: the seeded `skills/run-cli.md` is documentation *for* `RunCli`, and shadowing it would make `run-cli` return its own docs instead of executing anything.

A `MarkdownSkill` returns its body as the outcome, i.e. instructions fed back to the model, wrapped in the fixed `<skill_content name="…"><skill_resources>Base directory…</skill_resources><skill_instructions>…</skill_instructions></skill_content>` frame (`render_skill_content`) so relative resource paths in the body resolve. The same frame is what a typed `/name` injects. Frontmatter keys: `name` (required), `description`, `positional`.

`run-cli`/`run-pwsh` share `builtins::run_shell`: the child is `kill_on_drop` *and*, on Windows, placed in a kill-on-close Job Object (`agents::proc::JobGuard`) so a timed-out or interrupted `cmd /C npm install` takes `node` with it instead of leaving it detached. `SICA_SESSION_ID` is set in the child environment. Each stream is capped at 32 KiB with the shared `retain` omission sentence.

`ToolSubAgent` carries `depth`/`parent_id` (`max_depth = 4`) so a skill can spawn nested calls via `SkillContext::sub` and the FE can render the chain. Its pipeline (`agents::pipeline`, Wave 3) is: `pre_execute` policies (permission mode → plan mode → read-before-edit; first non-allow wins, `Ask` routes to the approval broker) → monotonic `guard`s (deny-only) → **cancel check** → **`Skill::timeout()`** (default 120 s; `agent-team` 30 min, `model-eval` 60 min, `skill-creator` 10 min, `ask-user`/`exit-plan-mode` 15 min — override it on any skill that drives its own LLM conversations, or the default kills it) → **spill-to-file** (`agents::spill`: a successful output over 48 KB is written to `spill/<session>/…` and the model gets a 4 KB head + omission marker naming the path + 1 KB tail; `read-file` is exempt so a follow-up read can't spill again) → `post_execute` (repeat-tool reminder rides `extra_context`; a `Block` replaces the outcome) → expectation summariser → failure sink. A `Deny`/`Block` is a failed outcome the model reads, never a defect: it skips the body and the sink, but still runs `post_execute` (so the repeat reminder counts denied calls) and still opens/closes its chip. Every failed *body* call (timeouts included) is also forwarded to a `ToolFailureSink`, which `main.rs` bridges into the idealist `TriggerBus` as a `tool_failed` trigger tagged `agents::tool::<skill>` — that's how a `cmd.exe`-only failure becomes a ticket suggesting `run-pwsh`. User interrupts are excluded (pressing Stop is not a defect). Every approval round-trip is appended as an `Approval` event for the audit; the model saw only the outcome.

## Control plane (Wave 3)

Permission modes (`read-only | workspace-write | danger-full-access`, policy level — no OS enforcement) and plan mode are per-session state on `ChatHub`, restored from the log's latest `PermissionMode`/`PlanMode` event on load, and rebuilt into pipeline policies on every dispatch so a flip applies on the next hop. The model is told via the runtime-context line plus (for plan mode) the `PLAN_POLICY` prompt section loaded from `skills/plan-mode.md`. Destructive-looking shell commands under `workspace-write` emit `ApprovalRequested` and wait on the broker (5 min → deny); `ask-user` and plan review emit `QuestionAsked` (10 min → fail the call). The FE answers via `ResolveApproval`/`AnswerQuestion` — both are **composer takeovers**: while a call is blocked on a human the composer is replaced in place by the approval card or the question panel, and the transcript stays scrollable above it. It renders the `todo-write` checklist as a dock card over the composer (cleared on the next turn start), and sends `/compact`/`/plan`/`permission` as `RunCommand` — harness commands that never create a model message and are audited as `Command` events. In native mode, consecutive `Parallel` calls (`read-file`, read-only shell) overlap in a bounded pool (cap 4) with model-order appends; everything else is an ordering barrier.

## agent-team grounding

`ToolSubAgent` wraps one tool call; `agents::team::AgentTeam` (opt-in, above) instead runs up to 6 *LLM* teammates concurrently, each with its own transcript, and merges their reports through a lead pass. Its failure mode is the opposite of a skill's: a teammate that calls nothing still writes fluent prose about files it never opened, and the lead launders that into the deliverable. Three guards, all in [crates/agents/src/team.rs](../crates/agents/src/team.rs):

- **Reports are typed** (`teammate_schema`, Wave 4): a teammate reports through the child-scoped `structured-output` tool as a list of claims, each citing the ids of the tool results that back it. The citations are checkable, not asserted — `runner` gives every dispatched call a stable id, echoes it to the child (`[id: call-2]`) and returns the trail on `Report.calls`, and `RunSpec.call_seq_start` keeps ids unique across the rounds a team runs over one transcript. A claim citing nothing, or citing an id that named no *successful* call, renders as `unverified:` in the board, the lead prompt and the final summary; a teammate with no cited claim is headed **UNVERIFIED**, a partly-cited one says so rather than passing as clean, and if nothing anywhere is cited the whole outcome gets a warning banner — that string is all the main agent ever sees.
- A reply with no parsable tool call is checked with `parse_tool_call::rejected_attempt`. A botched call (`read-file 'README.md'` with no ` > ` clause) buys one `SYNTAX_CORRECTION` retry plus a WARN `LogLine`; previously it was silently accepted as the teammate's final answer, which is exactly how "the file exists" reached the user for a file that didn't.
- Teammates see the catalogue via `catalogue_markdown_excluding(&[AGENT_TEAM_NAME])` — a teammate spawning its own team only unwinds at the depth limit.

## Delegation (Wave 4)

`agents::runner::run_conversation` is the one place a *child conversation*
runs: its own system prompt, its own transcript, a bounded hop loop over
`ToolSubAgent::child`, and one report crossing back. `agent-team`'s
teammates, `subagent`/`subagent-fork` and every `ralph` round go through it,
so the two grounding rules live once instead of three times — a run with
zero successful tool calls is reported **UNVERIFIED**, and a reply that
looks like a tool call but does not parse (`parse_tool_call::rejected_attempt`)
buys one `SYNTAX_CORRECTION` retry before it is accepted as an answer.

**Structured output.** `RunSpec.schema` registers a *child-scoped*
`structured-output` skill (present only in that run's registry view) and
appends its contract to the child's system prompt: only a call to it counts
as the result. The argument is validated against a JSON Schema subset
implemented in `runner::validate` — `type`, `properties`, `required`,
`items`, `enum`, `minItems`, with unknown keywords deliberately ignored
rather than rejected. There is no `jsonschema` dependency: every schema in
the workspace is authored in this crate and stays inside that subset. A
rejected argument is fed back as a tool error and retried within the hop
budget; prose where a schema was demanded buys one reminder and is then
reported unverified.

- **`subagent 'task'`** — fresh child, empty conversation, so the task must
  be self-contained. **`subagent-fork 'task'`** — child seeded with the
  parent session's *completed* turns (`chat::fork_seed` cuts at the last
  `TurnEnd`; the in-flight turn never crosses, since its tool results have
  not landed). The two descriptions differ deliberately: the description is
  what tells the model how to write the task. A fork with nothing to
  inherit fails loudly rather than silently running as `subagent`.
- **`ralph 'objective' 'max_rounds'`** — up to `MAX_ROUNDS` (64, default 8)
  brand-new agents against one immutable objective. A round sees no parent
  transcript and no earlier round — only the workspace (the stated source of
  truth) and the previous round's bounded 16 KiB report. Each round must
  report `{status, summary, evidence, next_steps, blocker}` through
  `structured-output`; `ralph::check_report` then enforces the cross-field
  rules the schema cannot express (`complete` needs evidence and no
  `next_steps`; `continue` needs a `next_step` and no blocker; `blocked`
  needs a concrete blocker). The loop stops on complete / blocked / round
  limit / a round that fails to report. The portable idea is the one to
  keep: *only a small validated struct crosses a context boundary.*

- **`workflow '<script>'`** (Wave 8, §12.5) — a Rhai script whose only
  verbs are delegation, run in the same sandbox as `run-code`
  (`agents::script`) with **no tools bound at all**: `agent(prompt)` and
  `agent(prompt, #{label, max_hops, schema})` for one child,
  `parallel([…])` for up to 8 concurrently (genuinely concurrent — the
  futures belong to the host, not the single-threaded engine), `pipeline`,
  `phase`/`log` to the operator's log, and `args` for the `input` argument.
  A child failure throws from `agent` and becomes `()` inside `parallel`.
  Caps: 32 children per script, 200 000 operations, 45 minutes. Like
  `agent-team` it registers **only if `skills/workflow.md` exists** — one
  call can spend 32 conversations, and its scripting reference costs ~575
  prompt tokens per request, so both stay off until someone asks.

Every delegated child runs on `registry.excluding(control::CHILD_EXCLUDED)`
— no harness controls (`ask-user`, `todo-write`, `exit-plan-mode`) and no
further delegation (`subagent`, `subagent-fork`, `ralph`, `agent-team`,
`workflow`), since nested delegation would otherwise only unwind at
`ToolSubAgent::max_depth` after spending a whole conversation per level.

## The inbox (Wave 4)

`ChatHub.inbox` ([crates/backend/src/inbox.rs](../crates/backend/src/inbox.rs))
is where input waits when the loop is busy, and the loop claims from it at
two points: **at the top of every hop** it drains `Steer` (user text) and
`Inject` (runtime context) into the log *before* `build_history`, so they
ride the very next request; **at the end of the turn** it claims one
`Followup` and starts the next turn itself. A `SendUserMessage` while a
turn is running therefore queues instead of cancelling that turn;
`SteerTurn` and `InjectContext` are the other two doors. `start_turn` is
split from `send_user_message` for the handoff: the slot stays *reserved*
across the gap (so a send arriving mid-handoff still queues), and going
back through the queue gate while holding it would re-queue the followup it
just claimed. Interrupting drops steers and injects aimed at the dying turn
but keeps queued user messages — sending a message and then pressing Stop
is how a user says "do this instead".

What waits there is visible and addressable: every change publishes
`InboxChanged` (the depth) and `QueueChanged` (the rows) together, and the
composer's queue dock renders the rows with Edit · Remove · Steer.
`Inbox` mints a stable id per item for this — a *position* stops naming the
same message the moment the loop claims one, so an edit racing a claim would
rewrite the wrong text. A verb whose id no longer names a waiting row is
answered with an error rather than a silent no-op, because "the loop already
took it" is a normal outcome the user has to see. Only followups are rows: a
steer or inject is spent at the next hop, so there is never a moment to edit
one. Steering a queued message is a promotion — it leaves the queue and joins
the running turn — and is refused for a message carrying images, which a
steer cannot take. In the FE the composer stays live during a
turn: plain Enter follows the Settings > General "Enter behavior while busy"
preference (Queue by default, so a send queues and shows in the queue dock)
and Ctrl+Enter always does the other one - dsh's accelerated submit.

## Background jobs (Wave 4)

`run-cli` / `run-pwsh` with `background=true` start a job under
`agents::jobs::JobRegistry` instead of waiting out the 30 s foreground cap,
and return `started job cli-3`. Three generic tools cover it from then on —
`job-output` (everything since the last read, ending in `[status: …]`),
`job-list`, `job-kill` — so a PTY or a detached subagent would need no new
controls. Jobs are per session (ids are invisible to any other) and die
with the process; 10 running per session, 256 KiB of retained output each,
and a read that lost bytes to that cap says so. Completion is **pushed**:
`backend::jobs_bridge` turns a finished job into a durable `JobFinished`
line plus a `ContextInjected { source: JobNotice }` in that session's
inbox, so the model is told at its next step whether or not it thought to
ask. It never wakes an idle session — that is the goal driver's job.

Note the enabling change in `SkillRegistry::resolve`: a surplus positional
of the form `key=value` binds to a **declared** optional arg. Without it
the text protocol could not reach `background` or `cwd` at all — the value
was dropped and the call quietly did something other than what it said.

## Goals and the round driver (Wave 4)

One durable objective per session (`agents::goal`, `EventKind::GoalChange`).
While a goal is `Active` **and armed** and under its round cap, the driver
in `chat.rs` opens a fresh turn against it every time the agent goes idle,
carrying the `<goal_round>` prompt (`round_prompt`) that tells the model the
workspace — not its own earlier narration — is authoritative. Skills
`create-goal`, `get-goal`, `update-goal` are harness controls like
`todo-write`; `/goal [continue|pause|complete|block <why>|edit <text>]` is the human
door.

Four rules bound it, and each answers a specific way autonomy goes wrong:

- **Compare-and-set on `revision`.** A round superseded by a human edit is
  refused, not silently applied over it.
- **Authority at execution.** Create / pause / resume need a direct human
  turn — which is what `TurnStart.source` (`sica_core::event::TurnSource`)
  records. A queued followup still carries human authority; a goal round
  does not. Complete / block also accept the current round.
- **`BLOCKED_AFTER_CONSECUTIVE_ROUNDS`.** A round may not declare the goal
  blocked in its first three attempts; a human may at any time.
- **Arming is process-local and never persisted.** A restored active goal
  comes back disarmed and waits for `/goal continue`, and pressing Stop
  disarms — otherwise the Stop button would be a lie.

The round is recorded *before* it runs, so an objective that crashes every
time still exhausts its budget. At turn end the continuation — queued
followup, auto-continue, goal round, or idle — is decided under one
`active_turns` lock; a queued human message wins, because the person is
here now.

## The completion check (stopping is not finishing)

A turn can end for reasons that say nothing about whether the work is
done: `hop-limit`, `max_tokens`, `error`. Those stops used to be silent —
the session went idle mid-task and the person had to notice, guess why,
and type "continue".

So `backend::verdict` gives every **abnormal** stop one tool-less LLM
round-trip. It is shown the human objective
(`verdict::objective` — the last user message that opened a turn with
human authority, so a continuation turn is audited against the original
request rather than against the prompt the harness wrote for it) and a
digest of the turn's tool *calls* and their outcomes (`verdict::digest`;
results are omitted — the judge needs to know lines 65-70 were read, not
what was on them). It answers `{ reached, reason, next_step }`, recorded
as `EventKind::TurnVerdict` and surfaced as a `LogLine`.

When `reached` is false and budget remains, the harness opens one more
turn carrying `verdict::continue_prompt` under
`TurnSource::AutoContinue`. Five rules bound it:

- **Abnormal stops only.** A `done` turn is never checked, so ordinary
  conversation costs exactly what it did before.
- **Never after an interrupt.** `finish` is already `interrupted` when the
  user pressed Stop, and that is not abnormal — Stop has answered the
  question this check asks, the same reasoning that disarms the goal
  driver there.
- **`MAX_AUTO_CONTINUES = 2` per human message**, counted *before* the
  continuation runs so one that crashes still costs an attempt, and reset
  only by a human message — never by a continuation.
- **Machine authority.** `TurnSource::AutoContinue` is not `is_human()`,
  so a continuation cannot create, pause or resume a goal.
- **The check never fails a turn.** No connection, a timeout, an
  unparseable reply, or a `reached` the model would not state plainly all
  return `None`, and the turn ends exactly as it would have. Defaulting
  `reached` either way is worse than not checking: `true` hides
  unfinished work, `false` auto-continues finished work.

Ordering matters at the continuation point: a queued human message beats
an auto-continue, which beats a goal round. The person waiting wins; but
finishing the request a limit interrupted comes before starting a fresh
objective round, which would otherwise leave the cut-short work undone
*and* spend a round.

## model-eval (measuring the prompt configuration)

`agents::model_eval::ModelEval` replays a suite of prompts against the **connected** model and scores each reply, so "did that `memory.md` edit help" stops being a matter of opinion. One run: load `evals/<suite>.toml` → per case, `repeats` fresh single-turn conversations carrying the *real* system prompt (`memory.md` + the live catalogue, the same shape `chat.rs::build_history` builds) → score → write `evals/reports/<suite>-<ts>.md` plus a `.json` baseline → diff against the previous baseline for that suite.

- **Nothing is dispatched.** Tool-call cases are validated with `parse_tool_call::extract_known` / `rejected_attempt` — the same parser `chat.rs` dispatches through — so a passing case is a call the backend would really have executed, and a suite is safe to run unattended.
- Failures are bucketed by `FailKind`, and each bucket carries a `lever()` naming the fix (a `memory.md` section, a skill description, the sampling temperature). The buckets exist because "answered from memory instead of calling the tool" and "reached for the tool and fumbled the syntax" look identical in a pass/fail column and need opposite fixes.
- `repeats` (default 2, max 5) turns a coin flip into a pass *rate*; a case that passes some repeats and fails others is reported as **FLAKY**, which points at sampling settings rather than wording.
- Caps: 40 cases/suite, 150 LLM calls/run, and the returned summary is held under 2 KB so `ToolSubAgent`'s summarizer never paraphrases the numbers.
- Judge cases (`judge = "<rubric>"`) are graded by the same model under test — the weakest signal in the report, labelled as such; an unparsable verdict counts as a pass.

