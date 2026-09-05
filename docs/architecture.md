# Architecture reference

Detail split out of [CLAUDE.md](../CLAUDE.md). Covers the crate graph, the wire
protocol, the on-disk surfaces, and the conventions to respect when editing.
The agent loop itself is in [docs/agent-loop.md](agent-loop.md).

## What this is

A two-binary Rust desktop app that hosts a local-LLM chat agent:

- **`backend`** — long-lived daemon (`crates/backend`). Holds chat sessions, the LLM connection, the skill (tool) registry, and the idealist daemon.
- **`frontend`** — egui/eframe GUI (`crates/frontend`) that spawns the backend as a child process, talks to it over a Windows named pipe, and offers rebuild/restart controls.

The split exists so the GUI can hot-reload backend logic: edit code, rebuild backend, supervisor respawns the child, IPC reconnects. The FE's watcher observes **all** of `crates/` (1 s debounce) but only ever runs `cargo build -p backend`, so a change to `protocol`/`llm`/`agents`/`sica-core` needs the FE restarted too. `sica_core::build_id::source_version()` (latest mtime under `crates/*/src` + `crates/*/Cargo.toml`) is computed by both sides; when the BE's `ServerHello.version` diverges from the FE's freshly-computed value the footer shows a pulsing RESTART button.

## Workspace layout

Seven crates, dependency direction strictly downward:

| Crate | Role |
| --- | --- |
| `protocol` | Wire types only (`Frame`, `Request`, `Response`, `Event`) + `PROTOCOL_VERSION`. No I/O, no dep on `sica-core`. Shared by both binaries — changes here force rebuilding both. |
| `sica-core` | Shared utilities: `paths` (every on-disk surface), `event` (the append-only session log + `derive_surface` fold), `project` (pure folds over that log — session stats, the turn outline, the last token reading), `snapshot` (tokenising a log so two runs of it can be diffed), `retain` (UTF-8-safe head/tail windows + the one omission sentence every cut uses), `message`/`session` (chat message types; `Session` survives only for legacy TOML migration), `build_id`, `theme`. |
| `llm` | HTTP client for OpenAI-compatible `/v1/chat/completions` (llama.cpp, vLLM, OpenAI, Anthropic-compat), SSE streaming + `<think>` splitting, connection state machine, token counting, `replay` (serve completions from a recorded log instead of a socket) and `mock` (a scripted fault server, test-only). |
| `agents` | Agent runtime: `turn` (one streaming request), `ToolSubAgent` (one tool call), `SkillRegistry`, built-in skills, markdown skills, `memory.md`, `prompt` (composed ordered system prompt + runtime-context snapshot + strict `{{var}}` interpolation), `instructions` (`AGENTS.md`/`CLAUDE.md` loader with a 64 KiB budget), `meter` (usage-anchored token meter), context `trim`/`compact` (prefix-preserving 8-section compaction + the tool-result pruner), tool-call parser, `guard` (repeat-tool reminder), `invoke` (`/name` expansion), `proc` (Windows Job Objects for shells), `spill`, `runner` (one delegated LLM conversation + structured output), `delegate` (`subagent`/`subagent-fork`), `ralph` (fresh-agent rounds), `web` (`web-fetch`/`web-search`), `mcp` (MCP servers bridged as skills), `ptc` (the `run-code` Rhai runtime behind programmatic tool calling). |
| `idealist` | Classifies failures (`FeBug` vs `BeFix`), writes improvement tickets to `idealist_workspace/`, optional BE auto-patching (off by default). |
| `backend` | Long-lived binary. `main.rs` parses `--ipc/--parent-pid/--log-level` and wires registry → idealist → `ChatHub`; `dispatcher.rs` routes requests; `chat.rs` owns the agent loop; `hooks.rs` runs the user's own shell hooks around it; `invariants.rs` holds the runtime invariant companions (`--invariants`); `be_core/` holds the legacy demo state. |
| `frontend` | egui GUI. `supervisor.rs` owns the BE child + IPC + watcher + cargo build; `app.rs` holds all UI state and drains `UiEvent`s; `ui/` holds the surfaces — `kit` (the design-system primitives), `icons`, `sidebar`, `chat/` (transcript, tool rows, composer, dock, control takeovers, `trajectory` (the event-log ledger), `details` (the tool / event inspector)), `settings/` (a modal). Styling is the dsh port described in [docs/harness-ui-guide.md](harness-ui-guide.md); waves UI-1…UI-5 are in. |

## Wire protocol

- Transport: Windows named pipe `\\.\pipe\sica-rust-<fe-pid>` via the `interprocess` crate's tokio API.
- Framing: length-delimited (`tokio_util::codec::LengthDelimitedCodec`).
- Payload: `bincode`-encoded `protocol::Frame`.
- Full duplex over one connection: requests, responses, and pushed events all multiplex. Each `Frame` carries a correlation ID; unsolicited events use ID 0.
- `PROTOCOL_VERSION` (currently 25) is exchanged via `ClientHello`/`ServerHello`; a mismatch raises a rebuild banner in the FE. **Bump it whenever `Request`/`Response`/`Event` change shape.**

Requests are split between the legacy demo set (`GetCounter`/`IncrementCounter`/`ResetCounter`/`ComputeFib`/`EchoText`, still exercised by `smoke` and the Settings → Communication tab) and the real surface (`SendUserMessage`, `InterruptTurn`, session CRUD, `ConnectLlm`/`DisconnectLlm`, `ReportFrontendError`, plus the Wave-3 control set: `RunCommand` (`compact`/`plan`/`permission`/`job-kill`/`goal`), `SetPermissionMode`, `SetPlanMode`, `ResolveApproval`, `AnswerQuestion`, the Wave-4 inbox pair `SteerTurn`/`InjectContext` plus the queue verbs `EditQueued`/`RemoveQueued`/`SteerQueued` the dock addresses rows with, and the UI-4 session verbs `RenameSession`/`ForkSession`/`ArchiveSession`/`SearchSessions` plus `ListModels`, and the UI-5 ledger request `LoadSessionEvents`, and the v23 projection request `SessionStats`, and the v24 agent-preset
  request `SetSessionAgent`).

Two v17 events exist purely so the transcript can show what the log already
records: `LlmRetry` (the retry chain row — the durable `EventKind::LlmRetry`
was previously only a `LogLine`) and `TurnUsage` (one turn's own token and
time totals, emitted at `TurnEnd`, distinct from the cumulative per-session
`TokenUsage` meter). `ListModels` answers `Ok` and reports through
`ModelsListed`, because the dispatcher loop is serial and a slow provider
must not stall it.

v18 adds prompt editing. `EditUserMessage { session_id, seq, text }` re-runs
the conversation from an earlier prompt: the backend appends
`EventKind::Rewind { start_seq, end_seq }` — the log's second shadowing
mechanism, and the only one that contributes no message of its own, so it
cannot be a `SurfaceOp::Replace` — and then runs an ordinary human turn
carrying the original message's images. The superseded turns stay in the log.
The rewind is appended *before* the turn's own instruction and
runtime-context snapshots, since it names the whole tail and anything written
ahead of it would fall inside the span it erases. It is refused while a turn
is running. Two additions carry the handle it addresses: `MessageDump.seq`
(every dumped message's durable id) and `Event::UserMessageStored`, pushed
once per turn-opening message so the transcript can offer the edit on a
prompt it has only seen live. The FE truncates optimistically and resyncs
from `Response::Error`, which now reaches the user as a toast rather than
only a raw line in the log panel.

v24 adds **agent presets** (harness guide §5.2). An `agents/*.md` file stops
being display-only: `Request::SetSessionAgent { session_id, name }` selects
one for a session, its body becomes the `PERSONA` section of the composed
system prompt (order -500, ahead of `memory.md`) and its frontmatter
`skills: [a, b]` restricts the registry the session's turns dispatch against.
Both halves are resolved once per turn from the one selection, so the prompt
can never advertise a skill the dispatcher would refuse. The selection is
durable (`EventKind::AgentPreset { name }`, latest wins, `None` = cleared),
pushed as `Event::SessionAgentChanged`, carried on `SessionDump.agent`, and
**fixed once the session has produced an assistant message** — the persona
sits in the system-prompt prefix, so a mid-session swap would both discard
the provider's cache and leave the earlier half of the transcript answering
to rules no longer in force. The control skills (`ask-user`, `todo-write`,
`exit-plan-mode`, the goal skills) survive every restriction: they are the
harness's own plane, not capabilities a persona chooses between. Three routes
reach it and all mean the same thing: the palette's AGENTS rows send the
request, a typed `/reviewer` resolves to `invoke::Invocation::Agent` and
selects rather than injecting a one-shot persona, and `/agent [<name>|off]`
does both plus clearing.

v25 adds **programmatic tool calling** (harness guide §7).
`LlmOptions.native_tools: bool` becomes `LlmOptions.tool_mode: ToolMode
{ Text, Native, Ptc }` — the only shape change in the bump, and no new
`Request` or `Event`. `Ptc` is the native wire with a narrowed catalogue,
not a third transport (`ToolMode::native()` is true for it), so the
`tools` array carries only `run-code` plus the harness controls, and every
other skill is reached from inside a Rhai program that `agents::ptc` runs
on a blocking thread with hard caps on operations, tool calls and wall
clock. A program's sub-calls go through the ordinary `ToolSubAgent`
pipeline and surface as live `ToolCallStarted`/`ToolCallFinished` events
nested under the `run-code` call — they are not written to the session
log, so the durable record is the `run-code` call and the one curated
result the model actually read. A model-direct call to anything else is
refused before the policy pipeline. The generated SDK is a prompt section
(`prompt::order::PTC_SDK`, 5000), so it costs nothing in `Text` or
`Native` mode, where `run-code` is also kept out of the catalogue
entirely.

v23 adds **session projections** (harness guide §3.3). `sica_core::project`
holds pure folds over the log — `SessionStats` (turns, messages, tool calls
and failures, retries, wall time), `TurnOutline` (one row per turn: the
opening user line, its source, hops, finish reason, and the `TurnStart` seq)
and `LastTokenUsage` — behind a `Projection { init, apply }` trait, and
`Request::SessionStats { session_id }` answers `Response::SessionStats
{ stats, outline, through_seq }`. They count *events*, not the derived
surface: a turn a compaction shadowed still happened, and a stats line that
shrank when the context was compacted would be lying about the session's
history. `through_seq` says how much of the log the answer covered, so a
client can tell a behind answer from a current one — the projection is never
wrong, only ever stale. Nothing is pushed: there is no state to subscribe
to, only a log that grew, so the FE re-asks on a session load and on
`TurnFinished`. It draws them as the stats line under the session crumb and,
in the sidebar under the open session's row, a collapsible turn outline whose
rows jump the Trajectory ledger to that turn's `TurnStart`. The same bump
adds `EventKind::Hook` and its `EventTag::Hook` for §13.1's user hooks.

v22 adds the **request envelope**. `EventKind::RequestEnvelope
{ fingerprint, system, tools, options }` records what one request went out
with — the composed system prompt, the `tools` array, and the sampling
options as JSON — and is appended by the hop that composed it **only when
the fingerprint differs from the last envelope in the log**, so a session
whose prompt never changes stores one copy and one whose `memory.md`
changed mid-session stores the before and the after. It never surfaces:
deriving it would send the system prompt twice. Every ledger row names the
envelope in force at it (`EventDump.envelope` — the newest at or before the
row), and the bodies travel once per page in
`Response::SessionEvents.envelopes` rather than once per row. This is what
makes the Trajectory inspector's Schema / System Prompt / Tools / Options
tabs truthful about the row you clicked rather than about the prompt as it
stands now.

v21 adds two optional fields to `Event::QuestionAsked`: `detail`, the body
under the headline, and `multi`, which turns the options into checkboxes.
`ask-user` gains the matching optional args (`detail`, `multi`); a
multi-select answer crosses back as the ticked labels joined with `; `, so
it is still one string and nothing downstream changes. Plan review passes
neither — it is Approve / Refuse.

v20 adds the Trajectory view's ledger (UI guide §10).
`LoadSessionEvents { session_id, from_seq, limit }` answers `SessionEvents
{ events, total, next_seq }` with the session's **raw** log rather than the
derived surface — `LoadSession` answers with what the model sees, this
answers with what the log holds, and the difference (the events a compaction
or a rewind shadowed) is the whole reason the view exists. `EventDump`
([backend/src/trajectory.rs](../crates/backend/src/trajectory.rs)) flattens each
`SessionEvent`: a coarse `EventTag`, one line of text, the payload/result
bodies, the provider's own token pair, the `ToolCall` join, the enclosing
`turn_id`, the event's JSON for the inspector's Raw tab, and the two fields
the transcript has no way to express — `shadowed` (asked of `derive_surface`
itself, so the ledger and the fold can never disagree) and `shadows`, the
span a `Replace` or a `Rewind` covered. Pages are capped at 500 rows and
`next_seq` says whether more remains.

`Event::ToolCallStarted` also gains `call_seq`: the live event carried only
the process-local tool id while a reloaded row carried the durable `ToolCall`
seq, so one call had two identities and the Inspect pill had nothing stable
to jump to. `ToolSubAgent::with_log_seq` carries it from the dispatch site;
it is `0` for a nested `SkillContext::sub` call, which is a live event only
and never reaches the log.

## On-disk surfaces (all at workspace root)

`sica_core::paths::workspace_root()` walks up from the running executable looking for `Cargo.toml`, so in dev everything below resolves against the repo root:

| Path | Owner | Notes |
| --- | --- | --- |
| `memory.md` | `agents::memory` | Prepended as the system message on **every** text-protocol turn; re-read from disk each turn, so edits apply without restarting. Seeded once from `memory::SEED` — which is also the normative spec of the tool-call syntax the parser implements, and now tells the model what the untrusted-result frame and the repeat-call notice mean. Strict `{{variable}}` interpolation applies (`{{cwd}}`, `{{os}}`, `{{date}}`, `{{model}}`); an unknown reference fails the turn loudly. |
| `AGENTS.md` / `CLAUDE.md` / `.sica/instructions.md` | `agents::instructions` | Discovered along the directory chain from the cwd up to the workspace root, combined under a 64 KiB budget (broadest omitted first, most specific truncated), and injected as one `<system-reminder>` snapshot (`ContextInjected { source: Instructions }`) that shadows its predecessor. Re-checked at turn start and after successful `read-file`/`write-file`/`edit-file` calls — no file watcher. `memory.md` is exempt from the budget. |
| `commands/*.md` | `agents::invoke`, `backend::catalog` | Listed in the `/` palette and resolved by a typed `/name` (`{{args}}` substitutes the rest of the line). Read per message — no restart needed. |
| `agents/*.md` | `agents::preset`, `agents::invoke`, `backend::catalog` | Session personas (v24): body → `PERSONA` prompt section, frontmatter `skills:` → registry view. A typed `/name` *selects* one rather than injecting it. Seeded once with `reviewer.md`. Read at selection and once per turn — no restart needed. |
| `skills/*.md` | `agents::md_skill` | Scanned at BE startup only — adding a skill needs a BE restart. `plan-mode.md` is the plan-policy config, excluded from the scan by name. |
| `sessions/<id>.jsonl` | `backend::sessions_store` | One append-only event log per chat session (`sica_core::event::SessionEvent`, one JSON object per line). A torn final line or a bad line mid-file is skipped, never fatal. Loaded eagerly at startup by `ChatHub::new_loaded`; a fresh session is not written until its first user message. Legacy `<id>.toml` files are migrated once into `LegacyMessage` events and renamed `<id>.toml.bak` (never deleted). |
| `spill/<session>/*.txt` | `agents::spill` | Full text of tool outputs too large to feed back into context; the model holds only a digest + this path. `.gitignore`d churn. |
| `sica-settings.json` | `frontend::settings_store` | FE settings, read at startup. Settings › General applies live (theme mode, content font size 12–17, Normal/Compact transcript, busy-Enter, reduce-motion) and writes through on every change; the other sections still have their own Apply / Connect buttons. |
| `sica-settings/llm-providers/*.toml` | `frontend::llm_providers` | One panel per provider; filename stem is the id. `.gitignore`d — may hold API keys. In the UI, `0` means "auto" for `max_tokens`/`context_window`. Each card shows a per-model recommendation (`llm::preset`, matched from the model string: temperature / thinking / tool mode per family) with a one-click Apply that persists to the TOML. |
| `idealist_workspace/Improvement-{BE,FE}-*.md` | `idealist` | Generated tickets. Append-only churn; don't treat as source. |
| `evals/*.toml` | `agents::model_eval` | One prompt suite per file; `default.toml` seeded once at BE start, user-owned after. Read per run, so edits need no restart. |
| `evals/reports/<suite>-<ts>.{md,json}` | `agents::model_eval` | Report + machine-readable baseline the next run of that suite diffs against. `.gitignore`d. |
| `.sica/hooks.json` (under the **working** directory) | `backend::hooks` | User hooks (guide §13.1), in Claude Code's own schema so an existing file can be copied across. Read once at BE start — a hooks file that could change under a running turn would make two calls in one turn answer to different rules. Absent by default; malformed is a `LogLine`, never fatal. `PreToolUse`/`PostToolUse` ride the tool pipeline as `HooksPolicy`; `UserPromptSubmit`/`SessionStart` are dispatched from `chat.rs`. A hook that fails to spawn, times out, or writes non-JSON **abstains** — the operator's script being broken must not become a permission decision. |
| `sica-settings/mcp/*.toml` | `agents::mcp` | One MCP server per file (`command`, `args`, `env`, `cwd`, `enabled`); the stem is the server name. Started at BE start over stdio, tools only, each bridged as a skill named `mcp__<server>__<tool>` whose JSON Schema goes into the `tools` array verbatim. A server that will not start is a `LogLine` and the agent comes up without it. |
| `sica-settings/web.toml` | `agents::web` | `provider` (`brave` \| `exa` \| `tavily`) + `api_key` for `web-search`. Absent by default; the tool still registers and its failure text says exactly which file to write, because a tool that disappears when unconfigured teaches the model the capability does not exist. `.gitignore` it — it holds a key. |
| `snapshots/<scenario>/` | `frontend::bin::replay` | Recorded-session evals (guide §14.1): `session.jsonl` (the recording, which is *also* the replay script), optional `scenario.toml`, `replay.override.json`, `workspace/` and `workspace.expected/`. Source, not churn. |

## Adding a new request (the common task)

1. Add a variant to `Request` (and matching `Response`) in [crates/protocol/src/lib.rs](../crates/protocol/src/lib.rs), and bump `PROTOCOL_VERSION`.
2. Handle it in [crates/backend/src/dispatcher.rs](../crates/backend/src/dispatcher.rs), delegating to `chat.rs` or `be_core/`.
3. In the FE, send it via `UiCommand::SendRequest`; if it returns data the UI needs, add a `UiEvent` variant and map the `Response`/`Event` to it (`supervisor::forward_event` for events).

Step 1 is a protocol change → rebuild both binaries (`.\run.ps1 build --workspace`) and restart the GUI; auto-watch alone only rebuilds the BE.

Long-running handlers must not block the dispatcher loop — `ConnectLlm` spawns onto the runtime and reports back via `LlmStateChanged`; `SendUserMessage` spawns the whole turn task and returns `Ok` immediately.

## Things to know before editing

- The workspace deliberately avoids MSVC to skip the multi-GB Visual Studio Build Tools dependency. Don't switch the toolchain unless asked.
- Common dependency versions live in `[workspace.dependencies]` in the root [Cargo.toml](../Cargo.toml); reference them in member crates with `{ workspace = true }`.
- `bincode` (v1) is the **pipe** format: types crossing the pipe must use externally-tagged enums — no `#[serde(tag/content)]`, no `untagged`, no `flatten` with maps. The `untagged`/`tag` attributes on `llm::client::ChatContent` and `ContentPart` are fine because those go out as JSON to the LLM, never over the pipe.
- Session event logs are JSONL (`serde_json`, internally-tagged enums are fine there — they never cross the pipe); provider configs and eval suites are `toml`; the LLM wire format is `serde_json`. Three serialization formats coexist by design.
- [docs/deepseek-harness-ideas.md](deepseek-harness-ideas.md) catalogues the agent-harness ideas ported from DeepSeek's `dsh` (event log, step-level retry, tool timeouts, spill-to-file, and the Wave 1 hygiene set: repeat-tool reminder, untrusted-content frame, tool-result pruner, `retain`, `/name` expansion, fallback titles, Job Objects, provider `usage`) and the ones deliberately left for later.
- The FE's `SessionDump` carries injected context under the string role `"context"`; since protocol v13 each such message also carries `context_source` (the `ContextSource` label) so the FE can present runtime-context / instructions snapshots without re-parsing prose. [docs/harness-implementation-guide.md](harness-implementation-guide.md) is the long form: every dsh feature/plugin, its mechanism, and a concrete sica-rust design (module, types, events, protocol impact) plus a five-wave roadmap and the list of `EventKind` variants each wave adds. Read the relevant section before adding a loop guard, prompt-assembly, approval, plan-mode, subagent, or jobs feature — the design is already sketched there. [docs/harness-ui-guide.md](harness-ui-guide.md) is the FE counterpart: dsh's web-client design system (tokens, type, geometry, elevation), every shell/transcript/composer/control-plane/settings surface with its concrete values, the egui port for each, the additive protocol changes (v17), and a five-wave UI roadmap. Read it before restyling or adding a frontend surface — all five waves are implemented: **UI-1 (foundation)**, **UI-2 (transcript)**, **UI-3 (composer + control plane)**, **UI-4 (settings modal + session rows)**, **UI-5 (the Trajectory ledger + the event inspector)** and **UI-6 (the open items: the `@` file picker, produced-file chips and the branch action on the turn tail, `/goal edit`, and the question takeover's `detail`/`multi`)**. What is left is listed as **Open** in its §12 — only the composer's ghost hint after a claimed command.

The `@` picker ([crates/frontend/src/ui/chat/at_menu.rs](../crates/frontend/src/ui/chat/at_menu.rs)) is the frontend's own: `@` names a path in `workspace_root()`, which the FE resolves for itself, so a keystroke never queues behind the dispatcher. It walks with the `ignore` crate (the same one `glob` uses, so the two agree on what is in the tree), re-walks when the index is over 30 s old, opens on an `@` token under the *caret*, and browses into a directory on accept. The path it inserts is plain text — nothing resolves it, and the model reads it as written.

Produced-file chips on the turn tail are derived from the turn's own successful `write-file`/`edit-file` rows rather than collected backend-side, so they cannot disagree with the transcript above them. The tail's branch action is `ForkSession`, offered only on the newest *finished* turn, because that is where `fork_session` actually cuts.
- **The FE design system is `sica_core::theme` + `ui::kit`.** `theme` holds the
  static ramps and the two semantic alias maps; every widget reads an alias
  through `kit` and no module below it branches on light/dark or names a
  literal colour. `App::apply_visuals` pours the tokens into `egui::Style` and
  stashes the `Theme` in `Context` memory, which is how `kit` reaches it
  without a palette threaded through every signature. Icons are painted by
  `ui::icons` (no SVG dependency); the UI face is the platform sans loaded at
  runtime and the code face is the vendored IBM Plex Mono.
- Tracing logs go to stderr; the GUI captures backend stderr and renders it color-coded in the log panel, **at its own level** (`Event::LogLine.level` reaches `LogKind`, and a WARN/ERROR line also raises a toast over the conversation). `Event::LogLine` is the deliberate channel for anything the operator should see in the GUI — a `warn!` alone is invisible unless it also emits a `LogLine`.
- The FE talks to the supervisor over `tokio::sync::mpsc` (commands) and back over `std::sync::mpsc` + `ctx.request_repaint()` (events). `App` state is only mutated while draining that channel on the UI thread.
- Heartbeats arrive every 2 s and feed the IPC-dot watchdog; they are intentionally *not* logged to the user-visible panel.
