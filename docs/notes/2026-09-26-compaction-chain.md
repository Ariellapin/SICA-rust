# Agent Note

Status: implemented
Class: architecture
Date: 2026-09-26

## Problem

Four ways a long session lost what compaction was supposed to keep
([long-session-plan.md](../long-session-plan.md) items 1, 2, 3 and 5):

- The previous `<compacted-summary>` sits inside the span the next
  compaction folds, and `summarize_fold` excerpted every folded message to
  2 000 chars. The summariser saw the head and tail of the earlier
  checkpoint and nothing of its middle sections, so each re-compaction
  dropped Files and Code, Errors and Fixes and Pending Jobs from the
  session's memory.
- `trim_to_budget` removed the oldest non-system message first. On the
  wire the checkpoint is a `user` message right after the system prompt,
  so when the backstop fired it deleted the compressed past before any
  raw message.
- The same trimmer dropped single messages, so in native mode it could
  leave a `tool` result without its `tool_calls` message, which several
  chat templates reject outright.
- `TodoWrite` is not a surface event. The model's knowledge of its own
  checklist was the `todo-write` call in its history, which the fold
  summarised away; the UI kept showing a list the model no longer knew.

## Decision

1. **A checkpoint is folded whole.** `summarize_fold` skips the excerpt
   for a folded message that opens with `SUMMARY_PREFIX`. It is already
   bounded by `MAX_SUMMARY_CHARS` (8 000), so the summariser request grows
   by at most that.
2. **The trimmer has a protected head and drops in pairs.** The head is
   the system prompt plus every message after it that opens with
   `CONTEXT_SUMMARY_PREFIX`; a message with native `tool_calls` leaves
   with the run of `tool` messages that answers it, and a unit that would
   reach the final message is not dropped at all.
3. **The open todo list is re-attached after every summary**, by
   `chat::land_compaction`, as a `ContextInjected { source: ToolNotice }`
   rendered by `control::render_todo_checklist`. Appended, not positioned
   after the summary, and only when an item is not completed.
4. **A summary is checked for the eight headings in order** and retried
   within `CompactPolicy.retries` when one is missing; if no attempt has
   the shape, the longest malformed one is kept and a WARN `LogLine`
   names the missing section.

## Consequences

- A fact stated early in a session survives any number of compactions as
  long as one summariser reply carried it forward; the directive's
  "consolidate the earlier checkpoint" rule now has the whole checkpoint
  to consolidate.
- The trimmer's notice says "after the context summary" when a checkpoint
  was kept, so a reader of the request knows the past was compressed, not
  amputated.
- The re-attached checklist lands at the end of the surface, after the
  newest user message, because an append leaves the prefix before it
  unchanged (plan Wave B's rule). It rides `ToolNotice` rather than a new
  `ContextSource` so no protocol bump is needed.
- No replay recording changed: the replay serves replies in call order
  and the directive text is not logged. The `compaction-replace` recording
  has a heading-less summary; with `retries = 1` the harness now spends
  one extra queued reply on the retry before keeping it, which the
  scenario's `pad = 8` was recorded to absorb.

## Alternatives

- **Fail closed on a malformed summary** (the plan's first wording).
  Rejected: the fallback is the trimmer, which keeps nothing of the folded
  span, and small local models that cannot hold eight headings would then
  never compact at all. The WARN makes the defect visible without making
  it worse.
- **Position the checklist right after the summary** with a `Replace`.
  Rejected: it would change the prefix at the summary's position on every
  compaction, and the notice is read either way.
- **Make `TodoWrite` a surface event.** Rejected: every `todo-write` call
  would then appear twice in the model's history (the call and the list),
  and the list would age in place instead of being refreshed at the one
  moment the model has lost it.
