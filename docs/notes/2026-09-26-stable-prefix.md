# Agent Note

Status: implemented
Class: architecture
Date: 2026-09-26

## Problem

The runtime-context snapshot (time, elapsed, cwd, OS, model, permission
mode, plan mode) was `Replace`d in place at the top of every turn. Its
position on the surface is early — right after the workspace instructions
— and its content changed every turn because the clock line did. A local
server with prompt caching (llama.cpp, vLLM prefix cache) matches the new
request against the previous one token by token from the front, so a
change at that position threw away the cache of the entire conversation
after it, and every turn re-prefilled the whole prompt
([long-session-plan.md](../long-session-plan.md) item 4). On a long
session that is the difference between a first token in under a second
and one in tens of seconds.

## Decision

1. **The snapshot holds only stable facts** — cwd, OS, model, permission
   mode, plan mode — and is re-landed only when one of them changes.
   `chat::upsert_context` is the one place that rule lives: find the
   newest visible row from the same source, append nothing if its content
   is identical, otherwise shadow it in place (or append fresh when a
   compaction removed it). `refresh_instructions` goes through the same
   helper.
2. **The clock is its own row.** `ContextSource::Clock`, rendered by
   `agents::prompt::clock_text` (local time with offset and source, then
   the gap since the previous message), is *appended* at the top of every
   turn and never replaced. Old clock rows stay where they were; a
   compaction folds them like any message.
3. **No protocol change.** The FE reads the source as its label string
   and renders "clock" as it renders any injected context.

## Consequences

- Between two turns with the same permission and plan mode, nothing ahead
  of the previous turn's last message changes. The provider's cache holds
  through the whole conversation; only the new clock row, the new user
  message and the reply are new tokens.
- The clock costs about twenty-five tokens per turn, cumulatively, until
  compaction folds them. Forty turns are a thousand tokens; the snapshot
  it replaced was re-sent whole every turn and still cost the prefill.
- The model sees a history of clock rows rather than one current one.
  The newest is always the last context row before the user message, and
  each says it is the local time at that moment, so nothing is ambiguous.
- **Every replay recording changes shape**: one runtime-context row per
  turn becomes one snapshot plus one clock row per turn, seqs shift, and
  the heuristic `used` counts in `token_usage` rows move with the prompt.
  All five scenarios need `--bless` on the Windows machine before replay
  is green. The snapshot normaliser already masks the clock's time.
- The measurement the plan asks for — `ttft_ms` on turn N of a 30-turn
  session with prompt caching on, before and after — has not been taken
  here; the `/stats` table (Wave F) is where it reads.

## Alternatives

- **Replace the clock row too, but at the tail.** Removing the old row
  still shifts everything after its old position; a removal is a prefix
  change like any other.
- **Keep the clock wire-only**, added to the request but never logged.
  Breaks the rule the whole log rests on: everything the model sees has a
  row (notes/2026-09-19). The trimmer's notice is the one exception and
  it is a marker, not a fact.
- **Drop the clock altogether.** dsh's time context (§9.3) exists because
  a model that has been idle for hours should know it; the cost is a
  line per turn.
