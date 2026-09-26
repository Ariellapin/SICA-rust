# Agent Note

Status: implemented
Class: architecture
Date: 2026-09-26

## Problem

The idealist wrote an `Improvement-*.md` file for two kinds of failure only:
a failed sub-agent tool call and a frontend panic. Everything else was an
ERROR `LogLine` and nothing more. That covered a turn ending on an LLM error,
a prompt that would not assemble, a checkpoint flush failing, a backend
panic and an invariant violation. The files that did exist had four
problems:

- Nothing ever read them again: they had no status and no follow-up.
- They had no session context, so nobody could find the error in the log.
- They were never deduplicated: an agent looping on one failing `read-file`
  wrote forty copies.
- A failure the agent recovered from on its next hop looked exactly like a
  real one.

## Decision

1. **One reporting path.** `backend::incident::report` is where every
   failure goes, alongside its `LogLine`. It is a process global, like
   `invariants::ENABLED`, because the call sites are inside the turn task,
   a panic hook and a `ToolFailureSink`, and none of these can reach the
   hub. When it is not installed (tests), it does nothing.
2. **The ticket id comes from the fingerprint.** The fingerprint is a
   SHA-256 of (origin, module, normalised message), and the id is its first
   12 hex characters. Because the id is derived, the place where an error
   happens can name its ticket without waiting for the daemon. It can then
   write the `TicketOpened` row and the ledger entry at once. Repeats bump
   `occurrences` on one ticket. A `resolved` ticket that fires again reopens
   as a regression.
3. **`TicketOpened` is written once per ticket per session**, at the first
   failure. A turn error's row is awaited before `TurnEnd`, so the log
   reads in order. The per-session ledger tracks the repeats instead of the
   log.
4. **Session end is defined by the harness**, because the protocol has no
   such moment. A session has ended when it has been idle for `idle_minutes`
   after its last turn, when it is archived, when someone requests it, or at
   the startup sweep. Shutdown never waits for an investigation.
5. **The investigator lives in `backend`, not `idealist`.** `idealist`
   stays LLM-free: it avoids a dependency cycle on `llm`, and it can be
   tested without a server. The investigator reuses `agents::runner`
   (structured output checked against a schema) with a read-only view
   capped at `read-file` / `glob` / `grep`. Its sub-agent has no failure
   sink, and it uses a `Quiet` event sink, so its calls never show up as
   chips in the open chat.
6. **The investigator never competes with the person.** It starts only when
   no turn is running and an LLM is connected. `session_active` cancels a
   run in flight, puts its tickets back to `open`, and re-queues the
   session.
7. **Lessons are opt-in.** Only `model_mistake` and `environment` findings
   become lessons. They reach the prompt only with `lessons_in_prompt =
   true`, appended to the memory brief, with `{{` escaped so strict
   interpolation cannot fail.

## Consequences

- The investigator does not run in replay mode. A recording is the
  provider there, and an investigation would ask it for completions it
  never recorded. The recordings in `snapshots/` contain no failures, so
  they gain no `TicketOpened` rows.
- The protocol is now v30. The new requests are `InvestigateSession`,
  `ListTickets` and `SetTicketStatus`; the new event is
  `IdealistInvestigated`.
- The "Idealist auto-apply" switch in Settings › General was never wired to
  the backend, and it still is not. Its description now says so.

## Alternatives not taken

*Update:* the invariant and the fix session below were later built in a
narrower form — see
[2026-09-26-idealist-followups.md](2026-09-26-idealist-followups.md).

- **Investigate at the moment of failure.** This would compete with the
  running turn for a single-slot local LLM, and it would investigate
  failures the agent was about to recover from.
- **One file per failure, grouped later.** This is what existed before, and
  it is the noise this change removes.
- **A runtime invariant saying "every `TurnEnd{error}` has a `TicketOpened`".**
  It is not an invariant in the §14.3 sense, because the two records are not
  independent observations. It would also flag every error in a log written
  before this change.
- **An auto-fix session for high-confidence harness bugs** (plan §4.3).
  This was deferred: the protocol has no "draft message" to put the ticket
  in for a person to review, and a session that sends it by itself would be
  the auto-patcher this design avoids.
