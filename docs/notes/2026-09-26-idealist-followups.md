# Agent Note

Status: implemented
Class: architecture
Date: 2026-09-26

Follow-up to [2026-09-26-idealist-investigator.md](2026-09-26-idealist-investigator.md):
it covers the items that note deferred, plus a shutdown bug the first
change introduced.

## Problem

1. **The backend hung on exit.** The idealist daemon waited for triggers
   with a blocking `recv()` on a `spawn_blocking` thread, and tokio's
   runtime waits for every blocking thread when `main` returns. Before the
   first change, every sender was dropped at exit, so `recv()` returned.
   The first change added two process-wide holders of a sender:
   `incident`'s reporter, and the investigator's hub (through its
   `ToolFailureBridge`). From then on `recv()` never returned and the
   backend never exited. The smoke test caught it: "deadline has elapsed"
   waiting for the child.
2. **A diagnosed harness bug had nowhere to go.** The investigator wrote
   what was wrong and which files to touch. Turning that into a fix still
   meant a person copying it into a new session by hand, in the right
   folder.
3. **Nothing announced that a session had ended.** Hooks could see a
   session start, but not its end.
4. **Nothing checked that every errored turn was ticketed.** A new error
   exit added without `incident::turn_error` would have gone unnoticed.

## Decision

1. **The daemon polls.** It waits with `recv_timeout(250 ms)` and loops
   back. Shutdown is now bounded no matter who still holds a sender, and
   this does not rely on remembering to release the statics.
2. **Fix sessions.** `StartFixSession { ticket_id }` opens a session in
   `workspace_root()` (the sica-rust checkout, not the user's working
   directory), titled `Fix <id>: <module>`, and answers with a prompt built
   from the ticket and its latest investigation. The FE switches to the
   session and puts the prompt in the composer. **Nothing is sent.** A
   person reads the diagnosis and presses Send. The ticket records the
   session (`fix_session`), so a second click reopens it instead of
   starting another.

   `auto_fix_session = true` in `idealist.toml` does the same by itself
   for `harness_bug` diagnoses with `high` confidence. Confidence is
   already lowered to `low` for a run that verified nothing, so `high`
   means the investigator read code that backs the claim. The FE keeps
   the prompt until the person opens that session. The switch is off by
   default.
3. **`SessionEnd` hooks** run when a session ends: idle past
   `idle_minutes`, or archived. They run at most once per quiet period, and
   the payload carries Claude Code's `reason` field. Session-end tracking
   now always runs; `investigate = false` only stops investigations from
   being queued.
4. **Invariant `error-turn-ticketed`** (§14.3). Ledger entries now record
   the turns they fired in. At every `TurnEnd` with `finish_reason =
   "error"`, the check requires the session's ledger to hold a turn-error
   entry for that turn. The log and the ledger are two files written by two
   code paths, so they are independent observations, and a missing
   `turn_error` call makes them disagree. It runs only with `--invariants`
   and only on the turn that just ended, so logs written before this change
   are never judged.

## Consequences

- Protocol v31. The smoke test now covers `ListTickets`,
  `StartFixSession` and `InvestigateSession`, and it passes with a clean
  exit.
- **No replay scenario was committed for the ticket flow.** The recordings
  are tied to the platform they were made on: the prompt contains the OS
  name and the absolute scratch path, and token counts follow from both.
  A scenario recorded on Linux would fail on Windows. The flow was checked
  with temporary scenarios instead (not committed):
  - a `run-cli` call to a command that does not exist produced a
    `TicketOpened` row, a ticket and a ledger entry;
  - a fatal LLM error produced `TicketOpened` before `TurnEnd { error }`,
    and the invariant passed.
- On Linux, all five existing scenarios diverge from their recordings only
  in the OS line and the token counts, and the divergence is byte-for-byte
  the same before and after these changes.

## Alternatives not taken

- **Drop the statics at shutdown** instead of polling. This is fragile: the
  next process-wide holder of the bus would bring the hang back.
- **Send the fix prompt automatically.** That would be an auto-patcher
  nobody approved, which is exactly what the first note avoided.
- **Pin the OS name for replay** (`SICA_OS_LABEL`). It was tried, and it
  fixed the OS line but not the scratch-path token drift, so it was not
  worth product code on its own.
