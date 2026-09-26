# Plan: making the agent hold up over long sessions

Status on 2026-09-26, commit `f90197e`. A code survey of what happens to a
session that runs for hours: dozens of turns, several compactions, a few
backend restarts, and a goal or auto-continue chain driving most of the
work. The turn loop, compaction, the meter and the overflow recovery are
all in place ([agent-loop.md](agent-loop.md)); what follows is what still
loses information, wastes time, or stops the agent once a session gets
long, with a recipe per item. Sizes use the guides' scale (S / M / L).

The items are grouped into six waves in the order they should ship. Wave A
is a set of correctness fixes to the compaction chain that any long session
hits today and that need no protocol change; do it first.

## What "long session" breaks today

| # | Symptom | Cause | Where |
| --- | --- | --- | --- |
| 1 | Facts from early in the session vanish after the second compaction | The previous `<compacted-summary>` sits inside the folded span and is cut to 2 000 chars (1 333 head + 667 tail) by the per-message excerpt guard before the summariser sees it, so the middle sections (Files and Code, Errors and Fixes, Pending Jobs) of the earlier checkpoint are dropped on every re-compaction | `compact::summarize_fold`, `MAX_EXCERPT_CHARS` |
| 2 | When the trimmer backstop fires, the first thing it drops is the compaction summary | `trim_to_budget` removes the oldest non-system message first; on the wire the summary is a `user` message right after the system prompt | `context::trim_to_budget` |
| 3 | In native mode a trim can orphan a `tool` result from its `tool_calls` | The trimmer drops single messages with no pairing rule, unlike `split_index` | same |
| 4 | Every turn re-prefills the whole prompt on llama.cpp / vLLM prefix caching | The runtime-context snapshot (time, elapsed) is `Replace`d in place near the *front* of the surface each turn, so the prefix changes at an early position and the server's KV cache for everything after it is invalidated | `chat::append_runtime_context`, `derive_surface` Replace positioning |
| 5 | The todo list is gone from the model's view after a compaction | `TodoWrite` is not a surface event; the model only ever saw its own `todo-write` call, which the fold summarises away | `EventKind::surface`, `handle_control` |
| 6 | A human message can drive at most ~36 tool calls before the session goes idle | `MAX_TOOL_HOPS = 12` per turn × (1 + `MAX_AUTO_CONTINUES = 2`); a goal is the only way past it — *fixed by Wave D: both are `harness.toml` settings, native turns get 32 hops, and a hop-limit stop under an armed goal opens a round instead of spending a continue* | `chat.rs:40`, `verdict.rs:48` |
| 7 | After a backend restart (the FE's rebuild/restart, or a crash) the model is never told what it lost | Background jobs die with the process with no `JobFinished`; an open `TurnStart` without `TurnEnd` is left as is; in native mode a `ToolCall` whose result never landed leaves dangling `tool_calls` on the next request | `ChatHub::new_loaded`, `jobs_bridge` |
| 8 | Each hop costs a full fold of the log, several times | `derive_surface` runs in `build_history`, again in `compact_session`, again in `prune_tool_results`' caller, in `refresh_instructions`, and once more under `--invariants`; every fold is O(events) with a `HashMap` of calls | `sica_core::event::derive_surface` |
| 9 | `spill/<session>/` grows without bound | `spill::write` has no sweeper; a long session with many `run-cli` results leaves hundreds of files | `agents::spill` |
| 10 | A compaction on a large window stalls the turn for minutes | The summariser replays the whole folded span (capped only per message) through the same local model; there is no smaller summarisation model and no chunking | `compact::summarize_fold`, `CompactPolicy` |
| 11 | Nothing measures whether any of this is getting better | `TurnUsage.ttft_ms` and `TokenUsage` are logged but there is no per-session series, no replay scenario with two chained compactions, and no invariant on the summary chain | `sica_core::project`, `snapshots/` |

Items 1–3 and 5 lose information; 4, 8 and 10 lose time; 6 and 7 stop the
agent; 9 fills the disk; 11 is why the rest cannot be tuned.

## Ground rules

Same as [remaining-work.md](remaining-work.md#ground-rules-that-apply-to-every-item):
build only through `.\run.ps1`, keep smoke and replay green, bump
`PROTOCOL_VERSION` for any wire change, never remove from a log, report to
the operator through `LogLine`, and write a `docs/notes/` entry for any
decision the code cannot explain. Two rules specific to this plan:

- **Compaction defaults are replay-priced.** Changing `threshold_pct`,
  `retain_pct`, the excerpt cap or the directive text changes what the
  `compaction-replace` scenario sends. Re-bless it in the same commit and
  say so.
- **Cache stability is a design constraint.** Anything that touches the
  surface before the tail — a re-injected snapshot, a notice, a summary —
  must either be stable across turns or be appended at the end. Check
  every wave against item 4.

## Wave A — compaction chain correctness (S × 4, no protocol change) — **shipped**

**Shipped as** (2026-09-26): `compact::summarize_fold` passes an earlier
checkpoint whole and judges every attempt against
`compact::REQUIRED_HEADINGS`, returning a `FoldSummary` whose
`missing_heading` the loop reports; `context::trim_to_budget` keeps a
protected head (system prompt plus the checkpoints after it) and drops
native call/result pairs together; `chat::land_compaction` appends the
open todo list as a `ToolNotice` after every summary, rendered by
`control::render_todo_checklist`. Decisions in
[notes/2026-09-26-compaction-chain.md](notes/2026-09-26-compaction-chain.md).
A4 shipped softer than planned below: a summary that never reaches the
eight-section shape is *kept* after the retries, with a WARN naming the
missing section, rather than failing closed to the trimmer — the note says
why. No recording needed re-blessing: the replay serves replies in call
order, the directive is not logged, and the `compaction-replace` scenario
has no todo list.

### A1. The previous checkpoint is folded whole

In `compact::summarize_fold`, exempt a folded message that starts with
`SUMMARY_PREFIX` from the `MAX_EXCERPT_CHARS` cut; it is already capped at
`MAX_SUMMARY_CHARS` (8 000) by `clean`. The directive's "consolidate an
earlier checkpoint" rule can only work when the whole checkpoint is in
front of the summariser. Test: a fold containing an 8 000-char summary
reaches the request intact while a 200 KB tool result is still excerpted.

### A2. The trimmer never drops the summary and never orphans a pair

In `context::trim_to_budget`:

1. Treat a leading run of messages that begin with
   `protocol::CONTEXT_SUMMARY_PREFIX` as part of the protected head, like
   the system prompt.
2. When the message being dropped is an assistant message with
   `tool_calls`, drop its following `tool` messages with it; when it is a
   `tool` message, drop back to its assistant message. Reuse the pairing
   rule from `compact::split_index` (extract it into a shared helper).
3. The notice keeps counting dropped messages, but says "after the
   summary" when one was kept.

Tests: the summary survives a trim to a budget that only fits system +
summary + last message; a native pair is never split.

### A3. The todo list survives compaction

Two changes:

1. `compact_session`, after landing the summary, re-injects the latest
   `TodoWrite` items (fold from the log with the same code
   `dump_session` uses at `chat.rs:1420`) as a
   `ContextInjected { source: ToolNotice }` appended after the summary,
   rendered as the checklist the model wrote. Only when at least one item
   is not `Completed`.
2. The compaction directive gains one line under **Current Work**:
   "If a todo list was in use, do not restate it; it is re-attached after
   this checkpoint." This keeps the summary from duplicating it.

Same treatment for an active goal: the round prompt already carries the
objective, so nothing is needed there.

### A4. Summary shape is validated before it lands

`summarize_fold` used to accept any non-empty text. It now checks that the
cleaned summary contains all eight headings in order; a summary missing one
is retried within `policy.retries`, and if no attempt has the shape the
longest malformed one is kept and a WARN `LogLine` names the missing
heading. Kept rather than discarded because the alternative is the trimmer,
which keeps nothing of the folded span at all; the warning is what makes a
model that cannot follow the directive visible.

## Wave B — a stable prefix across turns (M × 2) — **shipped, recordings to re-bless**

**Shipped as** (2026-09-26): `ContextSource::Clock` and
`agents::prompt::clock_text`, appended at the top of every turn by
`chat::append_runtime_context`, which now lands the runtime snapshot
through `chat::upsert_context` — the shared rule for every once-visible
context row, `refresh_instructions` included — so a snapshot is replaced
only when its content changed. No protocol change: `MessageDump`
carries the source as its label string, and the FE renders "clock" like
any injected context. Decisions in
[notes/2026-09-26-stable-prefix.md](notes/2026-09-26-stable-prefix.md).

**Every replay recording must be re-blessed on the Windows machine
before replay is green again**: each one logs one runtime-context row
per turn and compares heuristic token counts, and both change here.
`.\run.ps1 --% run -p frontend --bin replay -- --bless` once per
scenario (`compaction-replace`, `empty-response-retry`, `ptc-program`,
`spill-digest`, `write-file-effect`), then a plain replay run to confirm.
The TTFT measurement below is still to be taken; the `/stats` table from
Wave F is where to read it.

### B1. Split the runtime context into a stable snapshot and a clock line

Today one `ContextInjected { RuntimeContext }` carries cwd, OS, model,
permission, plan state, local time and elapsed time, and is `Replace`d in
place every turn. Split it:

- **`RuntimeContext`** keeps the stable facts (cwd, OS, model, permission
  mode, plan state) and is replaced *only when its content changes*, the
  way `refresh_instructions` already works. Most turns append nothing.
- **A new `ContextSource::Clock`** carries the two time lines and is
  **appended** at the top of every turn, never replaced. A clock line is
  ~25 tokens; forty turns cost 1 000 tokens, which compaction folds like
  anything else, and the prefix before the newest turn never changes.

The FE renders `Clock` like any injected context (collapsed by default;
`context_source` on `MessageDump` carries the label as a string, so the
new variant needed no protocol bump).

Measure before and after with the number already in the log:
`TurnUsage.ttft_ms` on turn N of a 30-turn session against a llama.cpp
server with prompt caching on. Expected: TTFT stops growing with history
length once the prefix is stable. Record the numbers in the note.

### B2. Position every other re-injection at the tail

Audited: the only in-place `Replace { seq, seq }` sites are the two
snapshots (now both through `chat::upsert_context`, which appends nothing
for unchanged content — `upsert_context_lands_nothing_for_unchanged_content`
pins it) and the pruner, whose replacement is by construction a change.
`@session` and `@file` snapshots, job notices, tool notices and the
re-attached todo list are all appends.

## Wave C — durable working memory (M × 2, protocol bump) — **shipped**

**Shipped as** (2026-09-26, protocol v30): `notes-write`
(`control::NotesWrite`, `EventKind::Notes`, `project::notes`,
`sessions/<id>/notes.md`, `ContextSource::WorkingMemory`, re-attached by
`chat::land_compaction` and picked up from disk by
`reconcile_notes_file`), `Request::WriteNotes` / `Event::NotesChanged` /
`SessionDump.notes` and the FE notes card (UI guide §6.6a); and
`backend::restart::repair` plus `ChatHub::deliver_restart_briefs` for the
restart brief. Two departures from the recipes below: the notes are not
re-injected at the start of *every* first turn after a restart, only in
the brief of a session the restart actually cut (otherwise the notes are
already visible, as the model's own call or the copy after the last
compaction); and the smoke step is replaced by unit tests over the log
(`restart::tests`), since the smoke binary needs Windows. Decisions in
[notes/2026-09-26-working-memory.md](notes/2026-09-26-working-memory.md).
The replay recordings are unaffected: no scenario writes notes or is cut.

The compaction summary is the only thing that carries the agent's own
state across a fold, and it is LLM-written and lossy (item 1 made it
worse; A1 makes it whole but not exact). Long tasks need a small piece of
state the harness carries verbatim.

### C1. `notes-write`: a pinned working-memory block

A harness control skill like `todo-write` (body runs in `chat.rs`,
excluded from children via `control::CHILD_EXCLUDED`):

- **Contract.** `notes-write '<markdown>'` replaces the session's working
  notes. Cap 4 KiB (a `LogLine` and a failed outcome above that). The
  catalogue line says what it is for: decisions taken, file paths in play,
  commands that worked, open questions — "what you would want to know
  after your memory is wiped".
- **Durable.** `EventKind::Notes { content }`, non-surface, folded by
  `sica_core::project::notes` (latest wins). Written to
  `sessions/<id>/notes.md` as well so the operator can read and edit it;
  an edit on disk is picked up at turn start like `memory.md`.
- **Visible.** Injected as `ContextInjected { source: WorkingMemory }`
  appended right after each compaction summary (with A3's todo block) and
  at the start of the first turn after a backend restart. Not on every
  turn: between compactions the model's own `notes-write` call is still in
  the tail and the pinned copy would only churn the prefix (Wave B).
- **Prompted.** One sentence in the compaction directive: "The working
  notes are re-attached after this checkpoint; do not restate them." And
  one in `memory.md`'s Rules: update the notes when a decision is made or
  a step completes, not every hop.
- **FE.** A dock card next to the todo checklist, editable, sending
  `Request::WriteNotes` (protocol bump alongside the event and the
  `SessionDump.notes` field).

### C2. Restart brief

At `ChatHub::new_loaded`, for each session whose log ends inside a turn
(a `TurnStart` without its `TurnEnd`):

1. Append `TurnEnd { finish_reason: "restart" }` so the outline and the
   verdict logic see a closed turn.
2. For every `ToolCall` without a `ToolResult`, append a failed result
   `ABORTED_BY_RESTART` (the `answer_unrun` shape at `chat.rs:808`), so a
   native transcript never carries dangling `tool_calls`.
3. For every background job started in the session and not `JobFinished`,
   append `JobFinished { status: "lost" }` and queue the same
   `JobNotice` the bridge sends, saying the job died with the process.
4. Queue one `ContextInjected { source: Injected }` for the next turn:
   "The backend restarted at <time>; the turn in progress was cut short
   after <n> tool calls; jobs <ids> were lost; your working notes and todo
   list follow." followed by C1's block.

Nothing here starts a turn — the person or the goal driver does. Smoke
step: kill the backend mid-turn, restart, load the session, assert the
four rows and that the next request derives cleanly in native mode.

## Wave D — autonomy budget for long runs (S × 3) — **shipped**

**Shipped as** (2026-09-26, no protocol change): `agents::harness`
(`HarnessConfig { tool_hops_text 12, tool_hops_native 32, auto_continues 2 }`
read once at backend start from `sica-settings/harness.toml`, malformed or
zero values reported as `LogLine`s and the defaults kept; `ChatHub::harness`
/ `with_harness`, the turn loop's `max_hops = harness.hop_cap(native)`, and
`verdict::continue_prompt`'s `of` argument); `verdict::after_stop` (the
four-way choice at the continuation point, with `AfterStop::GoalRound` for a
`hop-limit` stop under an armed goal with rounds left, proved end to end by
`chat::tests::a_hop_limit_under_an_armed_goal_opens_a_round_not_a_continuation`
over a replay script); and `agents::compact::section` feeding the latest
checkpoint's **Next Step** into `verdict::digest`. The Settings › Skills ›
Harness tab lists the two budgets at their defaults and names the file.
One departure from D1's recipe: only the two budgets moved into the file,
not the whole of remaining-work M1's list — the rest of that item (shell
timeout, caps, compaction thresholds, the editable tab) is untouched and
still open, and the loader is written so those keys can join it. Decisions
in [notes/2026-09-26-autonomy-budget.md](notes/2026-09-26-autonomy-budget.md).
The replay recordings are unaffected: no scenario reaches a cap or runs a
verdict, and the recordings do not carry requests.

### D1. Hop and continue budgets become settings

`MAX_TOOL_HOPS` (12) and `verdict::MAX_AUTO_CONTINUES` (2) were sized for
a chat, not a two-hour task. Fold both into the `harness.toml` item
(remaining-work M1) with defaults unchanged, and raise the *native-mode*
default hop cap to 32: native batches already overlap reads, the repeat
guard catches loops at 3/5/8, and the verdict check still audits every
abnormal stop. Text mode keeps 12; small models that emit one call per
message need the shorter leash. Replay recordings are unaffected (no
scenario reaches the cap).

### D2. A hop-limit stop under an active goal does not spend an auto-continue

At the continuation point (`chat.rs:3674`) a `hop-limit` stop with an
armed goal should open a goal round directly rather than an auto-continue,
because the round prompt re-grounds the model on the workspace and the
goal's own round cap bounds it. Keep auto-continue for `max_tokens` and
`error`, where re-grounding is not the point. One test over `llm::mock`.

### D3. Compaction's Next Step feeds the verdict

`verdict::digest` lists the turn's tool calls; when the turn contained a
compaction, include the summary's **Next Step** section so the judge sees
what the model itself said remained. Pure function change, one test.

## Wave E — cost per hop and per session (M × 2, S × 2)

### E1. Cache the surface fold

Give `SessionLog` a memoised `derive_surface`: recompute only when
`last_seq` moved (an `append` invalidates). Every caller in `chat.rs`
already goes through the log, so the change is local. Measure with a
synthetic 20 000-event log (`sessions_store` tests have the builders):
fold time per hop before and after. If the fold is under a millisecond at
that size, ship the cache anyway for the invariants path and stop there;
do not build incremental folds.

### E2. Spill sweeper

At backend start and once an hour, delete `spill/<session>/` files older
than 7 days *or* beyond 256 MiB per session, oldest first, and log one
line per sweep. A spilled file the model may still read is one the
summary names; 7 days is well past any tail. Make both numbers
`harness.toml` fields (M1).

### E3. Lazy session bodies

Remaining-work M2 as written. It matters here because a machine with
months of long sessions pays the whole set at every restart, and Wave C2
adds a fold per session at load.

### E4. Summarisation model and chunked folds

Add `CompactPolicy.model: Option<String>` (protocol bump, Models card
row): the summariser call goes to that model on the same provider when
set. dsh has the same knob. Then, only if measured compaction time on a
64 k window is still over a minute: fold in chunks — summarise the oldest
half of the span, then summarise that result plus the newer half — so
each summariser request stays under half the window. Keep the prefix
property for the first chunk (it is the conversation's own prefix); the
second chunk cannot be a prefix and that is the price.

## Wave F — measuring long sessions (S × 3) — **shipped**

**Shipped as** (2026-09-26): `project::TurnSeries` and the `/stats`
per-turn table, with `ttft_ms` made durable on `TokenUsage` (F3);
`invariants::check_summary_chain`, run with the other companions under
`--invariants` and on every replay (F2); and, in place of a recording, a
harness test in `chat.rs` that drives two chained compactions through the
real `compact_session` with the summariser served from a replay script,
plus a pure `compact::fold_request` whose test pins the checkpoint
exemption (F1). Two departures from the recipes below, both in
[notes/2026-09-26-long-session-instruments.md](notes/2026-09-26-long-session-instruments.md):
the series reaches the FE as the command's text instead of a new wire
type, and the `compaction-chain` recording is deferred to a Windows
session with the recipe under F1.

### F1. A chained-compaction replay scenario

Shipped as two tests rather than a recording. A recording cannot catch an
A1 regression: the replay serves the *recorded* summaries whatever request
the harness builds, and the request is not logged. So the guard is
`compact::tests::fold_request_keeps_an_earlier_checkpoint_whole…`, which
pins the request shape, and
`chat::tests::two_chained_compactions_carry_the_first_checkpoint_forward`,
which runs `compact_session` twice over a scripted summariser and checks
the second span shadows the first checkpoint, the chain invariant holds
and fires, and the todo list follows each fold.

A `snapshots/compaction-chain` recording is still worth having as the
fixture for E4's chunked fold and to run F2 on a real model's summaries.
Recipe, on the Windows machine: connect the model the other scenarios
use, open a session with `window` pinned small enough to compact twice in
six turns (the `compaction-replace` scenario uses 9 000), state a file
path and an error string in turn 1, ask about both in turn 6, then
`.\run.ps1 --% run -p frontend --bin replay -- --bless compaction-chain`
with a `scenario.toml` copied from `compaction-replace` (`pad = 8`).

### F2. `check_summary_chain` invariant

In `backend::invariants`: every `CompactionSummary` whose folded span
contains an earlier summary must contain every backticked identifier the
newest such summary contained (paths, commands, error strings — what the
directive says to keep exact). Cheap, purely over the log, and the kind of
invariant a second observer can check. The length rule first proposed here
was dropped: a shorter consolidated checkpoint is not a defect, a lost
identifier is.

### F3. A per-session series in `SessionStats`

`sica_core::project::TurnSeries` folds, per turn: source, hops, finish
reason, the largest `prompt_tokens`, summed completion tokens, the first
hop's `ttft_ms`, compactions, pruned results, retries and tool calls.
`ttft_ms` had only ever been a wire event, so it is now a log-only
optional field on `TokenUsage` (older logs read as `None`). `/stats`
prints the newest twenty rows under the counters, oldest first so a
climbing column reads as a trend. No protocol change: the table travels
as the command's text. This is the instrument for B1 and E4; without it
the two waves are opinions.

## Suggested order

1. ~~**A1 → A2 → A3 → A4**~~ shipped, see above.
2. ~~**F3 → F1 → F2**~~ shipped, see above.
3. ~~**B1 → B2**~~ shipped, see above; re-bless the recordings.
4. ~~**C1 → C2**~~ shipped (v30), see above.
5. ~~**D1 → D2 → D3**~~ shipped, see above.
6. **E1 → E2**, then E3 and E4 only if the numbers from F3 say so.
