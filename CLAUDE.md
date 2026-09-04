# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A two-binary Rust desktop app that hosts a local-LLM chat agent:

- **`backend`** — long-lived daemon (`crates/backend`). Holds chat sessions, the LLM connection, the skill (tool) registry, and the idealist daemon.
- **`frontend`** — egui/eframe GUI (`crates/frontend`) that spawns the backend as a child process, talks to it over a Windows named pipe, and offers rebuild/restart controls.

The split exists so the GUI can hot-reload backend logic: edit code, rebuild backend, supervisor respawns the child, IPC reconnects. The FE's watcher observes **all** of `crates/` (1 s debounce) but only ever runs `cargo build -p backend`, so a change to `protocol`/`llm`/`agents`/`sica-core` needs the FE restarted too. `sica_core::build_id::source_version()` (latest mtime under `crates/*/src` + `crates/*/Cargo.toml`) is computed by both sides; when the BE's `ServerHello.version` diverges from the FE's freshly-computed value the footer shows a pulsing RESTART button.

## Build / run / test

**Always use the wrapper scripts.** This workspace targets `x86_64-pc-windows-gnullvm` (pinned in [rust-toolchain.toml](rust-toolchain.toml)) and needs LLVM-MinGW on PATH. The wrappers prepend `%USERPROFILE%\.cargo\bin` and the winget LLVM-MinGW `bin/` dir before calling cargo. Direct `cargo …` invocations will fail unless the user has already added both to PATH.

```powershell
.\run.ps1 build --workspace
.\run.ps1 test  --workspace
.\run.ps1 run   -p frontend                  # launches the GUI
.\run.ps1 run   -p frontend --bin smoke      # headless E2E smoke test
.\run.ps1 run   -p backend -- --ipc <pipe>   # rarely needed; FE normally spawns BE
```

Single crate / single test (plain cargo filters, forwarded verbatim):

```powershell
.\run.ps1 test -p agents
.\run.ps1 test -p agents md_skill
.\run.ps1 test -p agents -- --exact md_skill::tests::parses_well_formed
```

`run.bat` is the cmd.exe equivalent of `run.ps1`. `start.bat` is a one-shot that builds + launches the GUI. `.\run.ps1 cmd <exe> <args…>` runs any other binary with the same PATH set up.

There is no `clippy.toml`, `rustfmt.toml`, lints config, or CI. Tests are inline `#[cfg(test)]` modules (~200 tests, concentrated in `agents`; also `backend`, `sica-core`, `idealist`, `llm`, `protocol`, `frontend`). To pass flags to the test binary through the wrapper use PowerShell's stop-parsing token: `.\run.ps1 --% test -p agents proc -- --nocapture`. The `smoke` binary ([crates/frontend/src/bin/smoke.rs](crates/frontend/src/bin/smoke.rs)) is the canonical end-to-end check — it spawns the backend, exchanges the handshake, sends `IncrementCounter`/`ComputeFib`, asserts responses, then `Shutdown`s and confirms exit 0. Run it after any change that touches the protocol, IPC, dispatcher, or `be_core`. It reads `target/debug/backend.exe` directly, so build first.

## Workspace layout

Seven crates, dependency direction strictly downward:

| Crate | Role |
| --- | --- |
| `protocol` | Wire types only (`Frame`, `Request`, `Response`, `Event`) + `PROTOCOL_VERSION`. No I/O, no dep on `sica-core`. Shared by both binaries — changes here force rebuilding both. |
| `sica-core` | Shared utilities: `paths` (every on-disk surface), `event` (the append-only session log + `derive_surface` fold), `retain` (UTF-8-safe head/tail windows + the one omission sentence every cut uses), `message`/`session` (chat message types; `Session` survives only for legacy TOML migration), `build_id`, `theme`. |
| `llm` | HTTP client for OpenAI-compatible `/v1/chat/completions` (llama.cpp, vLLM, OpenAI, Anthropic-compat), SSE streaming + `<think>` splitting, connection state machine, token counting. |
| `agents` | Agent runtime: `turn` (one streaming request), `ToolSubAgent` (one tool call), `SkillRegistry`, built-in skills, markdown skills, `memory.md`, `prompt` (composed ordered system prompt + runtime-context snapshot + strict `{{var}}` interpolation), `instructions` (`AGENTS.md`/`CLAUDE.md` loader with a 64 KiB budget), `meter` (usage-anchored token meter), context `trim`/`compact` (prefix-preserving 8-section compaction + the tool-result pruner), tool-call parser, `guard` (repeat-tool reminder), `invoke` (`/name` expansion), `proc` (Windows Job Objects for shells), `spill`. |
| `idealist` | Classifies failures (`FeBug` vs `BeFix`), writes improvement tickets to `idealist_workspace/`, optional BE auto-patching (off by default). |
| `backend` | Long-lived binary. `main.rs` parses `--ipc/--parent-pid/--log-level` and wires registry → idealist → `ChatHub`; `dispatcher.rs` routes requests; `chat.rs` owns the agent loop; `be_core/` holds the legacy demo state. |
| `frontend` | egui GUI. `supervisor.rs` owns the BE child + IPC + watcher + cargo build; `app.rs` holds all UI state and drains `UiEvent`s; `ui/` holds the panels. |

## Wire protocol

- Transport: Windows named pipe `\\.\pipe\sica-rust-<fe-pid>` via the `interprocess` crate's tokio API.
- Framing: length-delimited (`tokio_util::codec::LengthDelimitedCodec`).
- Payload: `bincode`-encoded `protocol::Frame`.
- Full duplex over one connection: requests, responses, and pushed events all multiplex. Each `Frame` carries a correlation ID; unsolicited events use ID 0.
- `PROTOCOL_VERSION` (currently 13) is exchanged via `ClientHello`/`ServerHello`; a mismatch raises a rebuild banner in the FE. **Bump it whenever `Request`/`Response`/`Event` change shape.**

Requests are split between the legacy demo set (`GetCounter`/`IncrementCounter`/`ResetCounter`/`ComputeFib`/`EchoText`, still exercised by `smoke` and the Settings → Communication tab) and the real surface (`SendUserMessage`, `InterruptTurn`, session CRUD, `ConnectLlm`/`DisconnectLlm`, `ReportFrontendError`, plus the Wave-3 control set: `RunCommand` (`compact`/`plan`/`permission`), `SetPermissionMode`, `SetPlanMode`, `ResolveApproval`, `AnswerQuestion`).

## The agent loop (the heart of the app)

`ChatHub::send_user_message` ([crates/backend/src/chat.rs](crates/backend/src/chat.rs)) first handles the message itself: a leading whitespace-bounded `/name` token is resolved by `agents::invoke` (`commands/<name>.md` with `{{args}}` substitution → `agents/<name>.md` → `skills/<name>.md`) and its `<skill_content>` frame is appended as `ContextInjected { source: SkillInvocation }` *before* the `UserMessage`, which is stored as typed; an unresolvable token is sent as plain text. On a still-placeholder session the first five words / 40 bytes of the message become a fallback `SessionTitle` immediately (`title_gen::fallback`), so the sidebar never shows "Session N" for a session with content; the LLM titler overwrites it after the first reply. Then it spawns one task and loops until the model stops calling tools (`MAX_TOOL_HOPS = 12`). Each iteration:

1. **Derive history from the session event log** (`build_history` → `SessionLog::derive_messages`) — never from an in-memory accumulator. Every persistence site is an `append_event` (`UserMessage`, `AssistantMessage`, `ToolCall`, `ToolResult`, `ContextInjected`, `CompactionSummary`, `LlmRetry`, `TokenUsage`, `TurnStart`/`TurnEnd`, `SessionTitle`), flushed as one JSON line to `sessions/<id>.jsonl` immediately, so a crash mid-loop leaves a recoverable transcript. Nothing is ever removed from the log: compaction appends a summary whose `SurfaceOp::Replace { start_seq, end_seq }` *shadows* the folded span in the derived view (`sica_core::event`). `EventKind` has a `#[serde(other)] Unknown` variant so a log written by a newer backend still loads. The trimmer's "context notice" marker is wire-only and must never be logged. Two snapshots ride `ContextInjected` and shadow their predecessor so one copy is ever visible: the **runtime context** (time/cwd/os/model, refreshed once per turn) and the **workspace instructions** (`AGENTS.md`/`CLAUDE.md` chain via `agents::instructions`, re-checked after successful fs-tool calls). A `{{variable}}` reference in `memory.md` with no registered value fails the turn loudly (ERROR `LogLine`) rather than sending a malformed prompt.
2. **Prune, compact, then trim.** Prompt budget is `context_window − (max_tokens ?? 4096) − 512`. At the connect-time `CompactPolicy.threshold_pct` (default 80%, dsh's policy) of that budget, `compact_session` first runs the *pruner*: every tool result older than the verbatim tail whose raw summary exceeds `compact::PRUNE_THRESHOLD` (8 KiB) is replaced by a 4 KiB head + 1 KiB tail window via a `ToolResult { surface: Replace { seq, seq }, pruned: true }` — no model call, and if that alone brings the prompt under the trigger the summariser is skipped. Otherwise `agents::compact` folds the older part of the history into an LLM-written summary — the call is a **KV-preserving prefix** (the conversation's own system prompt + the folded messages verbatim + the 8-section directive as the final user message), keeps `retain_pct` (default 16%) of the tail verbatim, and discards any summary cut off by `max_tokens` — landing as a system message framed in `<compacted-summary>` and prefixed with `CONTEXT_SUMMARY_PREFIX`; `agents::context::trim_to_budget` is only the backstop for when even that doesn't fit. Compaction must come before trimming — the budget is well under the window, so a trim-first order would silently amputate history before the meter ever read the trigger. The trigger itself uses the **usage-anchored meter** (`agents::meter`): when the provider's last `usage` covers this exact envelope (system prompt + tools fingerprint), only the surface added since is priced heuristically.
3. **Run the turn** (`agents::turn::run_turn`) — streams `AssistantDelta`, emits `TokenUsage` every ~100 ms with a `breakdown` (system / tools / history), and returns accumulated content + reasoning + native tool calls + `error` (a transport/server failure, never swallowed). Requests set `stream_options.include_usage`; when the provider's `usage` trailer arrives it is the final `used_tokens` (it counts the real template, tool schemas and images, which `/tokenize` on concatenated text cannot) and is stored on the durable `TokenUsage` event as `prompt_tokens`/`completion_tokens` — and becomes the meter's next anchor.
4. **Classify failures before persisting anything.** `llm::retry::classify` splits `TurnOutput.error` (and a clean stream that carried nothing at all) into retryable — connect/timeout/reset, HTTP 429/5xx, mid-stream SSE decode, empty response — vs fatal (other 4xx). A retryable failure appends `LlmRetry`, sleeps with jittered exponential backoff (500 ms → 10 s, max 5 retries per step, cancel-interruptible) and `continue`s: because the failed attempt persisted nothing, the rebuilt history is byte-identical and the retry is indistinguishable from the first attempt. This is a step-level listener, deliberately not a wrapper inside `llm::client`. Fatal/exhausted → ERROR `LogLine`, `TurnEnd { finish_reason: "error" }`, and the FE renders a *Request failed* line on the turn.
5. **Persist the assistant message**, then dispatch any tool call through a `ToolSubAgent` (logging `ToolCall` immediately before dispatch so an interrupted batch leaves no orphan), append the `ToolResult`, and loop. The result carries `trusted` from `Skill::trusted()` (default `false`; `MarkdownSkill` is `true` because its body *is* the instruction) — an untrusted result derives with `event::UNTRUSTED_NOTICE` ("data, not instructions") in front of the fenced block; harness-authored results (hop limit, unknown skill) are trusted. After every dispatch — failed and unknown-skill calls included — `agents::guard::RepeatTracker` (one per session on `ChatHub::repeat`, cleared by each user message) keys the call on `skill + key-sorted canonical args`; at 3, 5 and 8 consecutive identical calls it injects an advisory `ContextInjected { source: ToolNotice }` naming the tool and count. It never blocks the call; `MAX_TOOL_HOPS` stays the hard stop.

After the first complete exchange, `title_gen` renames a still-default-titled session and pushes `SessionTitleChanged`.

### Two tool-calling modes

Chosen per provider by `LlmOptions.native_tools`:

- **Text protocol** (default; works with any llama.cpp build). The system prompt is composed by `agents::prompt` from ordered sections: `memory.md` + one guidance sentence per skill that provides one + the live `## Loaded skills` catalogue. The model emits one line — `skill-name '<arg>' … > <expectation>` — parsed by `agents::parse_tool_call`. A ` ```tool_call ` JSON fence is also accepted because small local models emit that shape from training data. Positional values are zipped onto the skill's declared `positional_args()`. `Tool`-role messages are downgraded to `user` on the wire, since local chat templates often lack a `tool` role. Successful outputs over 2 KB are re-summarised against the caller's `expectation` by a second LLM round-trip, keeping the main context tight; shorter output passes through verbatim (raw text is ground truth).
- **Native** (`vLLM --enable-auto-tool-choice`, OpenAI, Anthropic-compat). `SkillRegistry::tools_json()` fills the request's `tools` array (optional args like `cwd`/`start`/`end` appear as non-required properties); real `tool` role + `tool_call_id` correlation is preserved on the wire and in storage. No expectation/summariser indirection — raw output goes back, per the OpenAI convention. Native `tool_calls` are *not* persisted on an interrupted turn: a dangling `tool_calls` with no matching results poisons the next request's template. Native mode keeps `memory.md` in the composed prompt (an identity section states that function calling is the interface); only the catalogue section is dropped, because the `tools` array carries it.

Parsing is deliberately conservative. `extract_tool_call_known` only accepts natural-language lines whose skill name is registered — otherwise prose like `cargo build > compiles fine` becomes a bogus call. When the model emits something tool-call-shaped that the parser rejects, `parse_tool_call::rejected_attempt` names the defect (unreadable ```tool_call fence, a known-skill line missing its ` > <expectation>` clause) and the caller surfaces it as a WARN `LogLine` instead of failing silently. Both `chat.rs` and `agent-team` use it — a rejected call that passes silently is indistinguishable from "the model chose not to use a tool", which is how fabricated tool output gets into a transcript.

### Skills

`Skill` is an async trait (`name`, `description`, `positional_args`, `run`). Registration happens once at BE startup ([crates/backend/src/main.rs](crates/backend/src/main.rs)):

1. Seed `skills/*.md` docs and `memory.md` if absent (**never overwritten** — those files are the user's once on disk). `skills/plan-mode.md` is seeded the same way but excluded from the skill scan by name — it is the plan-mode policy config, not a callable skill.
2. `register` the Rust built-ins: `skill-creator`, `run-cli`, `run-pwsh`, `read-file` (line-numbered, optional `start`/`end`), `write-file`, `edit-file` (literal single-match replace), `glob` (gitignore-aware, newest first, cap 100), `grep` (regex over files, cap 250 matches), `model-eval`, `ask-user` (blocks on the broker for a human answer), plus the `todo-write` / `exit-plan-mode` stubs — catalogue entries whose bodies run in `chat.rs` (session-log mutation + turn control), intercepted before any sub-agent spins up.
3. `agent-team` (`agents::team`) registers **only if `skills/agent-team.md` exists** — that file is the feature's on/off switch and is deliberately *not* seeded in step 1. A team is N concurrent LLM conversations per call and its teammates are the least reliable output in the app, so it stays out of the catalogue until someone puts the doc there. Rename it to `agent-team.md.off` (only `*.md` is scanned) and restart the BE to turn it off.
4. `md_skill::register_all` scans `skills/*.md` and uses **`register_if_absent`** so a markdown file can't shadow a built-in of the same name. This matters: the seeded `skills/run-cli.md` is documentation *for* `RunCli`, and shadowing it would make `run-cli` return its own docs instead of executing anything.

A `MarkdownSkill` returns its body as the outcome, i.e. instructions fed back to the model, wrapped in the fixed `<skill_content name="…"><skill_resources>Base directory…</skill_resources><skill_instructions>…</skill_instructions></skill_content>` frame (`render_skill_content`) so relative resource paths in the body resolve. The same frame is what a typed `/name` injects. Frontmatter keys: `name` (required), `description`, `positional`.

`run-cli`/`run-pwsh` share `builtins::run_shell`: the child is `kill_on_drop` *and*, on Windows, placed in a kill-on-close Job Object (`agents::proc::JobGuard`) so a timed-out or interrupted `cmd /C npm install` takes `node` with it instead of leaving it detached. `SICA_SESSION_ID` is set in the child environment. Each stream is capped at 32 KiB with the shared `retain` omission sentence.

`ToolSubAgent` carries `depth`/`parent_id` (`max_depth = 4`) so a skill can spawn nested calls via `SkillContext::sub` and the FE can render the chain. Its pipeline (`agents::pipeline`, Wave 3) is: `pre_execute` policies (permission mode → plan mode → read-before-edit; first non-allow wins, `Ask` routes to the approval broker) → monotonic `guard`s (deny-only) → **cancel check** → **`Skill::timeout()`** (default 120 s; `agent-team` 30 min, `model-eval` 60 min, `skill-creator` 10 min, `ask-user`/`exit-plan-mode` 15 min — override it on any skill that drives its own LLM conversations, or the default kills it) → **spill-to-file** (`agents::spill`: a successful output over 48 KB is written to `spill/<session>/…` and the model gets a 4 KB head + omission marker naming the path + 1 KB tail; `read-file` is exempt so a follow-up read can't spill again) → `post_execute` (repeat-tool reminder rides `extra_context`; a `Block` replaces the outcome) → expectation summariser → failure sink. A `Deny`/`Block` is a failed outcome the model reads, never a defect: it skips the body and the sink, but still runs `post_execute` (so the repeat reminder counts denied calls) and still opens/closes its chip. Every failed *body* call (timeouts included) is also forwarded to a `ToolFailureSink`, which `main.rs` bridges into the idealist `TriggerBus` as a `tool_failed` trigger tagged `agents::tool::<skill>` — that's how a `cmd.exe`-only failure becomes a ticket suggesting `run-pwsh`. User interrupts are excluded (pressing Stop is not a defect). Every approval round-trip is appended as an `Approval` event for the audit; the model saw only the outcome.

### Control plane (Wave 3)

Permission modes (`read-only | workspace-write | danger-full-access`, policy level — no OS enforcement) and plan mode are per-session state on `ChatHub`, restored from the log's latest `PermissionMode`/`PlanMode` event on load, and rebuilt into pipeline policies on every dispatch so a flip applies on the next hop. The model is told via the runtime-context line plus (for plan mode) the `PLAN_POLICY` prompt section loaded from `skills/plan-mode.md`. Destructive-looking shell commands under `workspace-write` emit `ApprovalRequested` and wait on the broker (5 min → deny); `ask-user` and plan review emit `QuestionAsked` (10 min → fail the call). The FE answers via `ResolveApproval`/`AnswerQuestion` (approval strip + modal), renders the `todo-write` checklist above the composer (cleared on the next turn start), and sends `/compact`/`/plan`/`permission` as `RunCommand` — harness commands that never create a model message and are audited as `Command` events. In native mode, consecutive `Parallel` calls (`read-file`, read-only shell) overlap in a bounded pool (cap 4) with model-order appends; everything else is an ordering barrier.

### agent-team grounding

`ToolSubAgent` wraps one tool call; `agents::team::AgentTeam` (opt-in, above) instead runs up to 6 *LLM* teammates concurrently, each with its own transcript, and merges their reports through a lead pass. Its failure mode is the opposite of a skill's: a teammate that calls nothing still writes fluent prose about files it never opened, and the lead launders that into the deliverable. Three guards, all in [crates/agents/src/team.rs](crates/agents/src/team.rs):

- `TeammateOutcome` counts successful tool calls per teammate. `tool_ok == 0` tags the report **UNVERIFIED** everywhere it appears — inter-round board, lead prompt, final summary — and the lead is instructed to attribute or drop those claims, never restate them as fact. If *no* teammate verified anything the whole outcome gets a warning banner, because that string is all the main agent ever sees.
- A reply with no parsable tool call is checked with `parse_tool_call::rejected_attempt`. A botched call (`read-file 'README.md'` with no ` > ` clause) buys one `SYNTAX_CORRECTION` retry plus a WARN `LogLine`; previously it was silently accepted as the teammate's final answer, which is exactly how "the file exists" reached the user for a file that didn't.
- Teammates see the catalogue via `catalogue_markdown_excluding(&[AGENT_TEAM_NAME])` — a teammate spawning its own team only unwinds at the depth limit.

### model-eval (measuring the prompt configuration)

`agents::model_eval::ModelEval` replays a suite of prompts against the **connected** model and scores each reply, so "did that `memory.md` edit help" stops being a matter of opinion. One run: load `evals/<suite>.toml` → per case, `repeats` fresh single-turn conversations carrying the *real* system prompt (`memory.md` + the live catalogue, the same shape `chat.rs::build_history` builds) → score → write `evals/reports/<suite>-<ts>.md` plus a `.json` baseline → diff against the previous baseline for that suite.

- **Nothing is dispatched.** Tool-call cases are validated with `parse_tool_call::extract_known` / `rejected_attempt` — the same parser `chat.rs` dispatches through — so a passing case is a call the backend would really have executed, and a suite is safe to run unattended.
- Failures are bucketed by `FailKind`, and each bucket carries a `lever()` naming the fix (a `memory.md` section, a skill description, the sampling temperature). The buckets exist because "answered from memory instead of calling the tool" and "reached for the tool and fumbled the syntax" look identical in a pass/fail column and need opposite fixes.
- `repeats` (default 2, max 5) turns a coin flip into a pass *rate*; a case that passes some repeats and fails others is reported as **FLAKY**, which points at sampling settings rather than wording.
- Caps: 40 cases/suite, 150 LLM calls/run, and the returned summary is held under 2 KB so `ToolSubAgent`'s summarizer never paraphrases the numbers.
- Judge cases (`judge = "<rubric>"`) are graded by the same model under test — the weakest signal in the report, labelled as such; an unparsable verdict counts as a pass.

## On-disk surfaces (all at workspace root)

`sica_core::paths::workspace_root()` walks up from the running executable looking for `Cargo.toml`, so in dev everything below resolves against the repo root:

| Path | Owner | Notes |
| --- | --- | --- |
| `memory.md` | `agents::memory` | Prepended as the system message on **every** text-protocol turn; re-read from disk each turn, so edits apply without restarting. Seeded once from `memory::SEED` — which is also the normative spec of the tool-call syntax the parser implements, and now tells the model what the untrusted-result frame and the repeat-call notice mean. Strict `{{variable}}` interpolation applies (`{{cwd}}`, `{{os}}`, `{{date}}`, `{{model}}`); an unknown reference fails the turn loudly. |
| `AGENTS.md` / `CLAUDE.md` / `.sica/instructions.md` | `agents::instructions` | Discovered along the directory chain from the cwd up to the workspace root, combined under a 64 KiB budget (broadest omitted first, most specific truncated), and injected as one `<system-reminder>` snapshot (`ContextInjected { source: Instructions }`) that shadows its predecessor. Re-checked at turn start and after successful `read-file`/`write-file`/`edit-file` calls — no file watcher. `memory.md` is exempt from the budget. |
| `commands/*.md`, `agents/*.md` | `agents::invoke`, `backend::catalog` | Listed in the `/` palette and resolved by a typed `/name` (commands substitute `{{args}}`). Read per message — no restart needed. |
| `skills/*.md` | `agents::md_skill` | Scanned at BE startup only — adding a skill needs a BE restart. `plan-mode.md` is the plan-policy config, excluded from the scan by name. |
| `sessions/<id>.jsonl` | `backend::sessions_store` | One append-only event log per chat session (`sica_core::event::SessionEvent`, one JSON object per line). A torn final line or a bad line mid-file is skipped, never fatal. Loaded eagerly at startup by `ChatHub::new_loaded`; a fresh session is not written until its first user message. Legacy `<id>.toml` files are migrated once into `LegacyMessage` events and renamed `<id>.toml.bak` (never deleted). |
| `spill/<session>/*.txt` | `agents::spill` | Full text of tool outputs too large to feed back into context; the model holds only a digest + this path. `.gitignore`d churn. |
| `sica-settings.json` | `frontend::settings_store` | FE settings, read at startup / written on Apply. |
| `sica-settings/llm-providers/*.toml` | `frontend::llm_providers` | One panel per provider; filename stem is the id. `.gitignore`d — may hold API keys. In the UI, `0` means "auto" for `max_tokens`/`context_window`. Each card shows a per-model recommendation (`llm::preset`, matched from the model string: temperature / thinking / tool mode per family) with a one-click Apply that persists to the TOML. |
| `idealist_workspace/Improvement-{BE,FE}-*.md` | `idealist` | Generated tickets. Append-only churn; don't treat as source. |
| `evals/*.toml` | `agents::model_eval` | One prompt suite per file; `default.toml` seeded once at BE start, user-owned after. Read per run, so edits need no restart. |
| `evals/reports/<suite>-<ts>.{md,json}` | `agents::model_eval` | Report + machine-readable baseline the next run of that suite diffs against. `.gitignore`d. |

## Adding a new request (the common task)

1. Add a variant to `Request` (and matching `Response`) in [crates/protocol/src/lib.rs](crates/protocol/src/lib.rs), and bump `PROTOCOL_VERSION`.
2. Handle it in [crates/backend/src/dispatcher.rs](crates/backend/src/dispatcher.rs), delegating to `chat.rs` or `be_core/`.
3. In the FE, send it via `UiCommand::SendRequest`; if it returns data the UI needs, add a `UiEvent` variant and map the `Response`/`Event` to it (`supervisor::forward_event` for events).

Step 1 is a protocol change → rebuild both binaries (`.\run.ps1 build --workspace`) and restart the GUI; auto-watch alone only rebuilds the BE.

Long-running handlers must not block the dispatcher loop — `ConnectLlm` spawns onto the runtime and reports back via `LlmStateChanged`; `SendUserMessage` spawns the whole turn task and returns `Ok` immediately.

## Things to know before editing

- The workspace deliberately avoids MSVC to skip the multi-GB Visual Studio Build Tools dependency. Don't switch the toolchain unless asked.
- Common dependency versions live in `[workspace.dependencies]` in the root [Cargo.toml](Cargo.toml); reference them in member crates with `{ workspace = true }`.
- `bincode` (v1) is the **pipe** format: types crossing the pipe must use externally-tagged enums — no `#[serde(tag/content)]`, no `untagged`, no `flatten` with maps. The `untagged`/`tag` attributes on `llm::client::ChatContent` and `ContentPart` are fine because those go out as JSON to the LLM, never over the pipe.
- Session event logs are JSONL (`serde_json`, internally-tagged enums are fine there — they never cross the pipe); provider configs and eval suites are `toml`; the LLM wire format is `serde_json`. Three serialization formats coexist by design.
- [docs/deepseek-harness-ideas.md](docs/deepseek-harness-ideas.md) catalogues the agent-harness ideas ported from DeepSeek's `dsh` (event log, step-level retry, tool timeouts, spill-to-file, and the Wave 1 hygiene set: repeat-tool reminder, untrusted-content frame, tool-result pruner, `retain`, `/name` expansion, fallback titles, Job Objects, provider `usage`) and the ones deliberately left for later.
- The FE's `SessionDump` carries injected context under the string role `"context"`; since protocol v13 each such message also carries `context_source` (the `ContextSource` label) so the FE can present runtime-context / instructions snapshots without re-parsing prose. [docs/harness-implementation-guide.md](docs/harness-implementation-guide.md) is the long form: every dsh feature/plugin, its mechanism, and a concrete sica-rust design (module, types, events, protocol impact) plus a five-wave roadmap and the list of `EventKind` variants each wave adds. Read the relevant section before adding a loop guard, prompt-assembly, approval, plan-mode, subagent, or jobs feature — the design is already sketched there.
- Tracing logs go to stderr; the GUI captures backend stderr and renders it color-coded in the log panel. `Event::LogLine` is the deliberate channel for anything the operator should see in the GUI — a `warn!` alone is invisible unless it also emits a `LogLine`.
- The FE talks to the supervisor over `tokio::sync::mpsc` (commands) and back over `std::sync::mpsc` + `ctx.request_repaint()` (events). `App` state is only mutated while draining that channel on the UI thread.
- Heartbeats arrive every 2 s and feed the IPC-dot watchdog; they are intentionally *not* logged to the user-visible panel.
