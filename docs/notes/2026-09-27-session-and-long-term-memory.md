# Session memory and long-term memory

2026-09-27 · protocol v32

## Problem

Two things were missing, and both showed up as "the agent forgot".

1. **Within a session.** The compaction summary was the only thing that
   carried the agent's own state across a fold. It is written under
   pressure, once per fold, and consolidated again at every later fold, so
   each pass loses a little (long-session plan, item 1). After an hour of
   work, the decisions from the first ten minutes were a paraphrase of a
   paraphrase. Nobody could see what the agent still knew, either.
2. **Across sessions.** Nothing carried over. A preference stated on Monday
   ("I use PowerShell 7", "short answers please") had to be stated again on
   Tuesday, and so did every fact about the project. `memory.md` is the
   person's instruction brief, not a place the agent writes.

## Decision

Two memories, with one tool surface and one settings page between them.

**Session memory** is a running summary plus up to 15 key facts, kept in the
session's own log as `EventKind::SessionMemory` (latest row wins, never
surfaced).

- *Written in the background.* `backend::memory_keeper` arms a short timer
  (`idle_seconds`, 45 s) when a session goes idle. When it fires, the keeper
  reads only the conversation after the memory's `through_seq` and asks the
  model for an updated `## Summary`, `## Key facts` and up to three
  `## Long-term` facts. Markdown with fixed headings rather than JSON: a
  small local model closes a heading far more reliably than it escapes
  quotes inside a JSON string.
- *Written by the model and the person too.* `remember '<fact>' session`
  adds a key fact. Settings has an editor. Each writer appends a whole new
  row. The keeper compares against the row it started from and drops its
  result if someone wrote in the meantime, so it never overwrites a newer
  edit; the next pass folds that edit in.
- *Used after a compaction.* `reattach_memory` appends a
  `ContextInjected { SessionMemory }` snapshot at the tail once something
  has actually been folded. Before that, the conversation it summarises is
  still on the surface verbatim, and a copy would only cost tokens and churn
  the prefix.

**Long-term memory** is a store of single facts
(`memories/long-term.json`, `agents::long_term`), each either global or tied
to one project folder.

- *Written by* the model (`remember`, default scope `project`), a person
  (Settings › Memory, and "Keep" on a session fact), and the keeper's
  promotions (`source: auto`, deduplicated loosely, three per pass at most).
- *Read* at turn start as a `ContextInjected { LongTermMemory }` snapshot
  holding the global facts and the ones for the session's folder, capped
  at 40 items and 4 000 characters. It is reconciled exactly like the
  `AGENTS.md` snapshot: a new row only when the store changed, shadowing
  the old one. After a compaction it is re-attached with the session
  memory.
- *Searched* with `recall`. That covers the long-term store plus every
  other session's memory, archived ones included. It needs no index,
  because every log is already loaded and the memory is a fold. A query
  that matches nothing lists the most recent sessions in the folder, which
  is what "what did we do last time?" means.

The three tools are harness controls, like `todo-write`. `remember … session`
writes the log and `recall` reads every session, which no `SkillContext`
can reach. `recall`'s result is framed untrusted (`control::control_trusted`):
it is text earlier conversations produced, and those read files and web
pages.

## Consequences

- **Prompt cost.** The three tools and two guidance sentences add ~170
  tokens to every request. All five replay recordings were re-blessed in
  this change for exactly that reason. The diff is the `token_usage.used`
  counts (+168) and the envelope bodies; line counts, event order and
  compaction points are unchanged. `skills/memory.md.off` removes the tools
  for anyone who needs the tokens back. Remembered facts still reach the
  prompt unless `inject = false`.
- **LLM time.** Each keeper pass is one completion (≤ 1 536 tokens out) on
  the connected model. It runs only while no turn runs anywhere, any turn
  cancels it, and it needs `min_new_chars` (400) of new conversation.
  "thanks!" is not worth a call. Not installed in replay runs.
- **Cache.** The long-term snapshot changes only when the store does, and
  the session snapshot only after a compaction, which has already
  invalidated the prefix. Neither is re-injected on ordinary turns.
- **Privacy.** `memories/` is `.gitignore`d. The log panel shows only a
  count for the `Memories` response.
- Long-session plan **C1** (`notes-write`) is covered by this, with the
  difference that the memory is written in the background rather than only
  by the model; small models are unreliable note-takers. **C2** (restart
  brief) is still open.

## Alternatives considered

- **Model-written notes only (plan C1 as written).** Rejected as the only
  writer: a 7–30B local model rarely calls a notes tool at the right moment.
  Kept as one writer among three.
- **Summarise at every turn end.** One extra completion per turn doubles the
  load on a single-slot local server. The idle timer gets the same result
  whenever the person pauses.
- **Long-term memory in the system prompt.** Simpler, but it made the system
  prompt differ per folder and change mid-session. The context snapshot
  follows the `AGENTS.md` precedent: logged, model-visible ⟺ logged, and
  replayable.
- **One file per memory with a Markdown index** (the Claude Code shape).
  More files to keep consistent for no gain at this size. One JSON document
  through `atomic_write` is crash-safe. A corrupt document is moved aside,
  never overwritten, and one from a newer build is never rewritten.
- **A search index for `recall`.** The logs are resident, and the memories
  are small folds. Word matching with prefix hits for terms of four or more
  letters is enough; an index can come when a machine has thousands of
  sessions (lazy session bodies, plan E3, would need one anyway).
