# Agent Note

Status: implemented
Class: architecture
Date: 2026-09-26

## Problem

Across a compaction the only thing that carries the model's own state is
the checkpoint summary, and that is an LLM's paraphrase: Wave A made it
whole but not exact. Across a backend restart — which the FE does as a
matter of course to hot-reload — nothing carries process state at all:
background jobs vanish with no row saying so, an open turn stays open,
and in native mode a call whose result never landed leaves the next
request's template with a `tool_calls` it cannot answer
([long-session-plan.md](../long-session-plan.md) items 5 and 7).

## Decision

1. **Working notes are a harness control, like the todo list.**
   `notes-write` replaces the session's notes (4 KiB cap); the row is
   `EventKind::Notes`, non-surface, latest wins. The model sees them as its
   own call while that is in the tail, and as a `WorkingMemory` context
   the harness appends after every compaction summary and in a restart
   brief. The compaction directive says not to restate them.
2. **The file is a door, not the record.** `sessions/<id>/notes.md` is
   written on every change so the operator can read and edit it; an edit
   is folded back at turn start as a new `Notes` row plus a `WorkingMemory`
   context. `Request::WriteNotes` from the notes card lands the same row
   and queues the same context for the model's next hop.
3. **Re-attachment is by append, and only when the model has lost them.**
   After a compaction (the call was just folded away) and in a restart
   brief (the session was cut mid-turn). Not on every turn, and not at
   the first turn after every restart: the notes are visible already,
   and an append that repeats them churns the prefix Wave B stabilised.
4. **A restart repairs by appending.** `restart::repair` closes the open
   turn (`finish_reason: "restart"`), answers its dangling calls with
   failed `ABORTED_BY_RESTART` results — including native `tool_calls`
   ids the batch never reached, which get a logged `ToolCall` too — and
   marks jobs that started and never finished `lost`. Only the open
   turn's calls are answered: an older gap was already what the model
   saw when that turn ran, and answering it at the end of the log would
   put a `tool` message where no template expects one.
5. **The brief rides the inbox.** Three injects per repaired session —
   the notice, the todo list, the notes — drained at the first hop of the
   session's next turn. Nothing starts a turn: a person or the goal
   driver still decides.

## Consequences

- A long task keeps a small exact memory the summariser cannot garble; the
  cost is one `notes-write` call per decision and up to 4 KiB re-sent
  after each compaction.
- A native-mode session cut mid-batch loads clean: every `tool_calls` id
  has a `tool` answer before the next request is built.
- A job lost to a restart is a durable row and a notice, so the model
  restarts it instead of polling an id the registry no longer knows.
- Old logs with unfinished jobs from before this note get a `lost` row and
  a brief once, at the first load; the brief waits in the inbox until the
  session's next turn, if ever.
- Protocol v30. Both binaries rebuild.

## Alternatives

- **A `memory` tool with free-form files.** More capable, more to prompt;
  one bounded replace-all block is what a small model uses correctly, as
  the todo list showed.
- **Make `Notes` a surface event.** The notes would age in place and be
  repeated on every write; re-attaching at the two moments they are lost
  is cheaper and keeps the prefix stable.
- **Fold the notes into the checkpoint summary.** That is the paraphrase
  problem this wave exists to avoid.
- **Answer every dangling call in the log, not only the open turn's.**
  See decision 4.
