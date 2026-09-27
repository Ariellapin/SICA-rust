# Agent Note

Status: implemented
Class: architecture
Date: 2026-09-27

## Problem

Long jobs kept stopping part-way with nothing on screen to say why. The
llama.cpp console's last line, `slot release: … stop processing: n_tokens =
1375`, is only the session's last small request finishing (the memory keeper
or the titler), after the turn had already ended. The session logs of
2026-09-27 (`sessions/92`-`95`, and `sessions/4` of the session-memory
worktree, all Qwen 3.6 on llama.cpp) show five separate causes:

1. **Reasoning-only replies read as answers.** Sessions 92, 93 and 95 each
   ended a turn on 10-24 K tokens of reasoning that stopped mid-sentence,
   with no visible text and no tool call. The loop found no call, so it
   ended the turn `done`. `done` turns get no completion check, so the
   session went idle. In session 95 the reasoning even held the finished
   `subagent` call, a few lines above where it stopped.
2. **Two refused tool calls ended the turn `done`.** Qwen writes calls in
   its own shapes: `<function=read-file>` with the argument on the line
   below and no quotes, the argument with no ` > <expectation>`, or
   `<parameter=path>` tags. A second rejection within a turn was taken as
   the answer (sessions 93, 94, and session-memory's 4).
3. **One oversized result killed the turn.** `read-file` is exempt from
   spilling, so a 325 KB wildcard file came back whole: 80 K tokens in a
   64 K window. The summariser was refused as too long itself, so the raw
   text went into the conversation. It sat in the verbatim tail, which the
   pruner spares, so the overflow recovery re-sent the same 80 272-token
   request three times and the turn died (session 95).
4. **A cut-off write starved its own continuation.** A whole-file
   `write-file` ran past the completion cap. The 50-60 K tokens of partial
   reply stayed in the tail, and the continuation had 14 K tokens left to
   answer in, so it was cut off again (session 93). Its prompt also said
   "work in larger steps", the hop-limit advice (session 92).
5. **Two auto-continues per request.** A 124-line job needs about ten
   12-hop turns, but it got three.

And the PowerShell errors had one root cause: the text protocol's argument
tokenizer. It processed `\n`, `\t`, `\r` and `\\` escapes in every argument,
so `…\Output\local\raw` reached PowerShell with a carriage return
(session 85). A quote inside the argument, such as `-ne ' '`, split it, and
the rest was dropped silently (session 92). Across 42 recorded shell calls
the model wrote every backslash single, never escaped.

## Decision

1. **A reply that is all reasoning is nudged, not accepted.** The loop
   appends `runner::empty_reply_correction` as a `ToolNotice`, with the
   last complete call drafted in the reasoning
   (`parse_tool_call::last_drafted_call`) or the last 1.2 KB of thinking,
   because the reasoning never reaches the next request. The next step
   goes out with thinking off, as the completion check already does. After
   `MAX_EMPTY_NUDGES` (2) the turn ends as `empty`.
2. **`parse_tool_call::extract_for` for the live loops.** It is told each
   skill's arity (`SkillRegistry::arity`). It takes a one-argument skill's
   argument verbatim between the outer quotes, splitting at the last
   ` > ` after a closing quote when an odd quote inside throws the first
   reading out of phase. It reads `<parameter=…>` blocks as
   `ToolCall::named`, which the registry binds by declared name and then
   by position. It treats a closed `<function=name>` block's body as the
   call, and accepts a wrapped or last-line call with no expectation.
   `extract_known` stays strict, because `model-eval` grades the contract.
   A second miscall now ends the turn as `bad-call`.
3. **`read-file` returns at most 48 KB**, cut at a line boundary. The
   result ends with a `[read-file: output capped …]` line naming the next
   `start`/`end`, and the sub-agent keeps that line when it summarises.
   The summariser reads at most 32 KB of head and tail. The overflow
   recovery prunes every oversized result, the newest included, before it
   compacts.
4. **A cut-off reply over 8 KiB is shadowed** by
   `compact::truncated_reply` (head, tail, and "no call in it ran"),
   through an `AssistantMessage { surface: Replace }` row. The replay
   script skips such rows, since they are not completions.
5. **The completion check tracks progress** (`verdict::Spent`). A
   continuation that ran a tool successfully is refunded. Two stalled
   continuations in a row stop the chain, and 12 bound it. A
   continuation that ends `done` is checked too. `continue_prompt` gives
   advice by stop: write in parts with `'append=true'` after `max_tokens`.
   `write-file` declares `append` so the one-line form can reach it.

## Consequences

- Two new `finish_reason` values, `empty` and `bad-call`. Both are
  abnormal, so they get the check. Both file a `backend::turn::…` ticket.
- The system prompt grew by about 70 tokens: `write-file`'s `append` and
  the verbatim-argument rule in `memory.md` and `agents::memory::SEED`.
  Every replay recording was re-blessed, and only `used` counts moved.
- A one-argument call no longer turns `\n` into a newline. A model that
  meant one inside a PowerShell string gets the literal `\n`, which is what
  PowerShell itself would do. Doubled backslashes in paths still resolve,
  because Win32 collapses them.
- The `compaction-span-balanced` invariant now accepts a result replaced in
  place by one for the same call. That is the pruner's shape, and it had
  never run under `--invariants` before `overflow-prunes-the-tail`.
- New replay scenarios: `reasoning-only-nudge`,
  `wrapped-call-no-expectation`, `overflow-prunes-the-tail`. A continuation
  cannot be a scenario, because the driver re-sends every user message
  and the judge draws from the script, so `Spent` and `needs_check` are
  unit-tested.

## Alternatives

- **Dispatch the call drafted in the reasoning.** Rejected: reasoning is
  full of drafts the model then abandons ("no wait…"). Handing it back
  costs one short reply and keeps the model the one who decides.
- **Check every `done` turn.** Rejected for its cost on ordinary
  conversation. Continuations are where early stops cluster, and they
  already paid for checks.
- **Raise `MAX_TOOL_HOPS`.** Deferred: a progress-refunded continuation
  gives long work the same reach, with a check between turns.
- **A default `max_tokens` when the provider sets none.** Deferred: with
  thinking on, a cap cuts reasoning into `max_tokens` stops that the
  continuation now handles. Choosing a reply reserve re-prices every
  recording, and is its own decision.
