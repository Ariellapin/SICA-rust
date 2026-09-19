# Agent Note

Status: implemented
Class: architecture
Date: 2026-09-19 (records a decision made at the start of the port)

## Problem

A chat session needs a durable history the model reads from and a UI reads
from, and the two are not the same thing: compaction folds older turns into a
summary the model sees while the reader still wants the turns; a rewind
removes a span from what the model sees while the log must keep it; tool
results are pruned to a head/tail window for the prompt but the full text
was real. Holding the session as a `Vec<Message>` makes every one of those
a destructive edit, and a destructive edit is a fact the app can never
recover.

## Decision

The session is an **append-only event log** (`sica_core::event::EventKind`,
one JSONL row per event, `backend::sessions_store`), and the model-visible
history is a **derived surface** (`derive_surface`) folded from it. Nothing
is ever removed from the log: compaction, rewind and pruning append rows
that *shadow* earlier spans in the derived view (`SurfaceOp::Replace`,
`EventKind::Rewind`, `ToolResult.pruned`). Everything the model can see has
a durable row; everything durable can be re-derived.

## Consequences

- Two independent observers folding the same log must agree, which is what
  the invariant companions (`backend::invariants`, `--invariants`) check.
- Projections (`sica_core::project`: stats, turn outline, runs, reminders,
  feedback) are pure folds; a new surface is a new fold, never new state.
- Replay evals (`snapshots/`) are possible at all: a recording *is* the log,
  and a run either re-derives the same rows or diverges visibly.
- A durability barrier is meaningful: `chat::checkpoint` flushes the log
  before a request and before a tool body runs, and a flush that fails ends
  the turn rather than sending a request the log would not remember.
- Every bookkeeping kind (`WorkflowRun`, `Schedule`, `MessageFeedback`,
  `Hook`, `JobFinished`) is non-surface: written for the reader, skipped by
  the fold, loaded by older binaries as `Unknown`.

## Alternatives rejected

- `Vec<Message>` with in-place edits: simpler, and the shape the app started
  with (`sessions/<id>.toml`); rejected the first time compaction needed to
  be undone.
- SQLite with FTS (dsh's `session-query`): a second store to keep in step
  with the log; the log is small enough to scan.
