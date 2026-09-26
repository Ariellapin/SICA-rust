# Plan: error tickets during a session, investigation at its end

Goal: when something goes wrong while the agent works, the harness **opens a
ticket** right away. When the session **ends**, an investigator agent reads
each open ticket with the session log and the source, and writes its root
cause, a proposed fix, and a one-line lesson back into the ticket.

This builds on the existing `idealist` crate. It does not replace it.

---

## 1. What exists today

| Piece | Where | What it does |
| --- | --- | --- |
| `TriggerBus` | `crates/idealist/src/trigger_bus.rs` | An unbounded channel of `Trigger { kind, module, message, traceback }`. |
| Daemon loop | `crates/idealist/src/lib.rs` | Classifies each trigger and writes one markdown file per trigger to `idealist_workspace/`. |
| Classifier | `crates/idealist/src/classifier.rs` | Uses the module prefix to pick FE, BE, SubAgentTool, LLM or Unknown. |
| Analyzer | `crates/idealist/src/analyzer.rs` | Keyword heuristics. It can suggest a skill swap, such as `run-cli` to `run-pwsh` on Windows. |
| Ticket writers | `be_autofix.rs`, `fe_ticket.rs` | `Improvement-BE-<kind>-<ts>.md` / `Improvement-FE-<ts>.md`. |
| Producers | `backend/src/main.rs` `ToolFailureBridge`, `dispatcher.rs` `ReportFrontendError` | **Only two:** failed sub-agent tool calls, and FE panics. |

### Gaps

1. **Most errors never become tickets.** The turn loop in `backend/src/chat.rs`
   ends a turn with `finish = "error"` at 7 sites. These cover LLM retries
   running out, context overflow after compaction, prompt assembly failures,
   and checkpoint flush failures. The LLM connect failure and the preset load
   failure are further error sites. Every one of these emits only a
   `LogLine`. The backend also has no panic hook, and invariant violations
   (`backend/src/invariants.rs`) go nowhere.
2. **A ticket has no session context.** `Trigger` has no `session_id`,
   `turn_id` or event `seq`. That means nobody can find the part of the
   session log where the error happened.
3. **There is no deduplication.** An agent that loops on the same failing
   `read-file` writes one new file per hop, so the useful signal gets lost
   in the noise.
4. **Tool failures that the agent recovered from count the same as real
   failures.** "File not found" followed by the right path on the next hop
   is normal behaviour, not a bug.
5. **Tickets are write-only.** Nothing ever reads them again: they have no
   status, no investigation and no feedback into later sessions. The
   `auto_apply` flag reaches `write_be_ticket` as `_auto_apply` and is
   ignored.
6. **The "end of session" moment does not exist.** Hooks have
   `SessionStart` and a per-turn `Stop`, but nothing marks a session as
   finished.

---

## 2. The target flow

```
 error anywhere ──► Trigger(+session_id, turn_id, seq, origin)
                        │
                        ▼
                 idealist daemon
          fingerprint → open new ticket, or bump occurrences on the existing one
          record the ticket in the session ledger
          append EventKind::TicketOpened to the session log
                        │
      … session continues; more tickets may open or dedupe …
                        │
 session end (idle timeout / archive / manual / next-startup sweep)
                        │
                        ▼
              Investigator (background, one at a time,
              only when the LLM is free, read-only tools)
          input:  ticket + log window around seq + analyzer hint
                  + earlier investigations of the same fingerprint
          output: JSON {root_cause, category, confidence, evidence,
                        proposed_fix, files_to_touch, lesson}
                        │
                        ▼
      the ticket gets an "## Investigation" section; status → diagnosed
      FE shows it; a lesson optionally feeds the next session's prompt
```

**Where the code lives.** `idealist` stays LLM-free: the doc comment at the
top of `analyzer.rs` states that design choice, and it avoids a circular
dependency on `llm`. Ticket storage, fingerprinting and the ledger go in
`idealist`. The investigator needs `ChatHub`, the LLM client and the skill
registry, so it goes in `backend` as `backend::investigate`, next to
`verdict.rs` and `title_gen.rs`, which already run background LLM calls.

---

## 3. Phases

### Phase 1: better tickets, no LLM (size M)

**1a. Richer `Trigger`** (`idealist/src/trigger_bus.rs`)

```rust
pub struct Trigger {
    pub kind:       String,
    pub module:     String,
    pub message:    String,
    pub traceback:  Option<String>,
    // new
    pub origin:     TriggerOrigin,      // ToolCall | TurnError | LlmConnect | Panic | Invariant | Frontend | Config
    pub session_id: Option<u64>,
    pub turn_id:    Option<u64>,
    pub seq:        Option<u64>,        // session-log seq nearest the failure
}
```

`ToolFailureReport` (`agents/src/subagent.rs`) needs `session_id`,
`turn_id` and `seq` as well. `ToolSubAgent` already carries
`session_id` (`subagent.rs:149`, set by `with_session`).

**1b. A ticket model** (a new module, `idealist/src/ticket.rs`)

- `Ticket { id, fingerprint, status, source, category, severity,
  occurrences, first_seen, last_seen, sessions: Vec<u64>, recovered: bool }`
  sits in TOML front-matter (`+++`), because the workspace already uses
  `toml`. The markdown body goes below it.
- `status`: `open → investigating → diagnosed → resolved | wontfix | noise`,
  with `investigation_failed` as a retryable side state.
- **Fingerprint** = a hash of `(origin, module, normalize(message))`.
  `normalize` removes digits, quoted paths, hex ids and durations, so
  `timeout after 30s` and `timeout after 31s` match.
- `TicketStore::upsert(trigger)` works like this. It looks for a ticket with
  the same fingerprint that is not resolved, not wontfix and not noise. If
  one exists, it bumps `occurrences` and `last_seen` and adds the session.
  If none exists, it writes a new ticket. A match on a `resolved` ticket
  **reopens** it with a `regression` note.
- There is one file per ticket: `idealist_workspace/tickets/<id>.md`. The
  old `Improvement-*.md` files stay readable and are not migrated.

**1c. A session ledger.** `idealist_workspace/sessions/<session_id>.toml`
lists the tickets that were raised or bumped in that session, along with
`investigated_at`. The investigator reads this file at session end.

**1d. More producers.** Every error site goes through one helper,
`ChatHub::report_error(session_id, turn_id, origin, module, msg)`. The helper
emits the `LogLine`, as the code does today, **and** publishes the trigger.
The call sites are:

- the 7 `finish = "error"` sites in `chat.rs`, using origin `TurnError` and
  module `backend::turn::<reason>`
- LLM connect failure (around `chat.rs:1717`), using origin `LlmConnect`
- preset load failure (around `chat.rs:1988`), using origin `Config`
- a **backend panic hook**: `std::panic::set_hook` in `backend/src/main.rs`.
  It publishes to the bus and also writes `idealist_workspace/crash-<ts>.md`
  synchronously, because the bus may already be dead.
- `backend::invariants` violations, using origin `Invariant`
- hooks, MCP and web config load warnings, using origin `Config` and
  severity `Warning`. These tickets are never investigated automatically.

**1e. Tag recovered failures.** A tool failure is marked `recovered = true`
when a later call in the same turn to the same skill succeeds. The session
log already holds this information, and the ledger records it at `TurnEnd`.
The investigator skips a recovered ticket unless its fingerprint has
`occurrences ≥ 3` across sessions. That repeat count means the model keeps
making the same mistake, which is worth a lesson.

**1f. Session log event.** Add `EventKind::TicketOpened { ticket_id,
fingerprint, origin }` in `sica-core/src/event.rs`. The replay tool and the
UI can then show where the error happened, and the investigator can find its
place in the log. The event is append-only and follows the no-removal rule
of the event log.

Tests: fingerprint normalisation, upsert/dedupe/reopen, a front-matter round
trip, the ledger, and recovered-failure detection. All are unit tests in
`idealist`.

### Phase 2: detecting session end (size S)

This codebase has no explicit end, so the plan defines one.
`SessionEndReason` can be any of the following:

| Reason | When | Action |
| --- | --- | --- |
| `Idle` | The session goes idle after `TurnEnd` (`Next::Idle`, around `chat.rs:3672`) and stays idle for `idle_minutes`, default **10**. A new turn, a steer or a followup cancels the timer. | Queue the investigation. |
| `Archived` | `Request::ArchiveSession` | Queue it right away. |
| `Manual` | New `Request::InvestigateSession { session_id }` | Queue it right away. |
| `Shutdown` | `Request::Shutdown` | **Do not** block shutdown. The ledger keeps `investigated_at = None`. |
| `StartupSweep` | Backend start | Queue every ledger that has open tickets and `investigated_at = None`. |

The timer is a per-session `CancellationToken` plus a `tokio::time::sleep`,
stored beside the inbox. Also add `HookEvent::SessionEnd` to
`backend/src/hooks.rs`, which uses the Claude Code spelling, so user hooks
can react as well.

### Phase 3: the investigator (size M/L)

**The agent.** A seeded preset, `agents/investigator.md`, in the same format
as `agents/reviewer.md`:

```markdown
---
name: investigator
description: Diagnoses a harness error ticket from the session log and the source. Never edits.
skills: [read-file, glob, grep]
---
You investigate one error ticket from a sica-rust session …
(the output JSON schema, the rule that every claim quotes a file:line or a log seq,
 and the category definitions)
```

The investigator can only read. `preset::view` already enforces the skill
subset, and its sub-agent gets **no** `failure_sink`, so an investigator's
own tool failures cannot open tickets and cannot recurse.

**The runner** (`backend/src/investigate.rs`):

- A single worker with a FIFO queue of `(session_id, reason)`. It runs **only
  when no turn is running in any session and the LLM is `Connected`**. A
  local LLM has a single slot, so the investigation must never compete with
  the person.
- For each ledger, it takes the open tickets that are not recovered and that
  are ranked by severity then occurrences, with a cap of **5 per session**.
  The rest stay `open` for the next time.
- The input for each ticket is:
  1. the ticket body, which already carries the analyzer hint and any
     suggested skill
  2. the log window around `seq`: 30 events before and 10 after, rendered
     the way `derive_surface` renders them and capped at about 6 KB through
     the spill policy
  3. the `## Investigation` sections of earlier tickets with the same
     fingerprint, so repeat findings add up instead of being rediscovered
  4. a module-to-path hint (`agents::turn` → `crates/agents/src/turn.rs`)
- The budgets are 12 tool hops, a 5-minute wall clock, and thinking **on**.
  `verdict.rs` turns thinking off, but it is a classifier and this is a
  diagnosis, so thinking should be on here.
- The output is parsed the same way `verdict.rs` parses its JSON:

  ```json
  {
    "category":   "harness_bug | model_mistake | environment | config | external",
    "root_cause": "…",
    "confidence": "high | medium | low",
    "evidence":   ["crates/agents/src/turn.rs:412 …", "log seq 188 …"],
    "proposed_fix": "…",
    "files_to_touch": ["…"],
    "lesson": "one sentence the model should know next time, or null"
  }
  ```

- On success, the runner appends `## Investigation — <date> (session N)` and
  sets `status = diagnosed`. On failure, such as a parse error, a timeout or
  a lost connection, it writes the raw reply, sets
  `status = investigation_failed`, and emits a `WARN` `LogLine`. **An
  investigation never fails anything else** (the rule `verdict.rs` follows).
- Events emitted: `IdealistStatus { activity: "investigating #id" }`, then a
  new `Event::IdealistInvestigated { ticket_id, category, confidence }`.

### Phase 4: closing the loop (size M)

1. **FE Idealist panel.** It lists tickets grouped by status, with the
   occurrence count and the sessions each ticket appeared in. Actions:
   *Investigate now*, *Mark resolved / wontfix / noise*, and *Open session at
   seq*. It is built only from `ui::kit` and `sica_core::theme`, following
   `docs/harness-ui-guide.md`. New requests: `ListTickets`,
   `SetTicketStatus { id, status }`, `InvestigateSession`.
2. **Lessons feed the next session.** A diagnosed ticket with category
   `model_mistake` or `environment` and a non-null `lesson` is added to
   `idealist_workspace/lessons.md`, deduped by fingerprint. Prompt assembly
   includes the 10 newest lessons, about 300 tokens. This option is **off by
   default** and has a switch under Settings › Integrations, following the
   rule for `workflow` and `schedule`: anything that costs prompt tokens is
   opt-in. Later, lessons about one skill can move into that skill's *Model
   Experience* block (remaining-work S2).
3. **Harness bugs lead to a fix session, optionally.** This gives
   `auto_apply_be` a real job: for a `harness_bug` with high confidence, it
   creates a **new session** whose first message is the ticket. A person
   still reviews and sends it. It is off by default and never applies to FE
   tickets, which matches the existing policy.
4. **Regression signal.** When a `resolved` fingerprint fires again, the
   ticket reopens and shows a `regression` badge in the panel.

### Phase 5: verification (size S)

- A replay scenario, `snapshots/tool-failure-investigation`: a tool fails,
  a ticket opens, the session goes idle, the investigator runs against the
  mock LLM, and the ticket ends `diagnosed`. Run it with
  `.\run.ps1 run -p frontend --bin replay`.
- **An invariant (§14.3):** every `TurnEnd { finish_reason: "error" }` has a
  `TicketOpened` earlier in the same turn.
- `smoke.rs` must pass, because the protocol changes.
- Record the "idealist stays LLM-free; the investigator lives in `backend`"
  decision in `docs/notes/<date>-idealist-investigator.md`.

---

## 4. Protocol and format changes

| Change | Crate | Bump |
| --- | --- | --- |
| `Request::InvestigateSession`, `ListTickets`, `SetTicketStatus` | `protocol` | `PROTOCOL_VERSION` 29 → **30**; rebuild both binaries |
| `Event::IdealistInvestigated`; `IdealistTicketWritten` gains `ticket_id`, `occurrences` | `protocol` | same bump |
| `EventKind::TicketOpened` | `sica-core` | JSONL, additive; old logs still load |
| `ToolFailureReport` gains session fields | `agents` | internal |

These changes touch `protocol`, `sica-core` and `agents`, so the FE must be
restarted. Rebuilding only `backend` is not enough.

## 5. Risks and mitigations

| Risk | Mitigation |
| --- | --- |
| The investigator competes with the person for the local LLM | It runs only when every session is idle, and it stops (cancel token) the moment a turn starts. |
| A flood of tickets from a looping agent | The fingerprint dedupe, plus the cap of 5 per session. |
| Noise from errors the model recovered from | The `recovered` flag, and repeated occurrences as the only way to promote one. |
| The investigator changes the code | It gets read-only skills through `preset::view` and never receives `auto_apply`. |
| Recursion (the investigator's failures open tickets) | Its sub-agent gets no `failure_sink`. `origin = Investigator` is dropped at the bus. |
| Tickets leak secrets from tool args | Tickets stay local in `idealist_workspace/`. `args_preview` is already truncated, and a redact pass adds `***` for `*_KEY` and `token=`-style args. |
| A shutdown during an investigation | The ticket stays `investigating`. The startup sweep resets it to `open`. |

## 6. Suggested order and effort

1. Phase 1a–1d: richer triggers, the ticket store, dedupe, all producers.
   This alone fixes gaps 1–3.
2. Phase 1e–1f: the recovered flag and `TicketOpened`.
3. Phase 2: session-end detection with the manual request.
4. Phase 3: the investigator. This is the core value.
5. Phase 5: the replay scenario and the invariant. They land with Phase 3,
   not after it.
6. Phase 4: the FE panel, then lessons, then the auto-fix session.

About 2 to 3 waves the size of the recent ones (Wave 5 / Wave 10).

## 7. Decisions for the owner

- **Idle timeout** before a session counts as ended. Proposed: 10 min.
- **Lessons in the prompt**: opt-in (proposed) or on by default?
- **Auto-fix sessions** for high-confidence harness bugs: allowed at all?
  Proposed: yes, but off by default and never auto-sent.
- **Per-session investigation cap.** Proposed: 5 tickets.
