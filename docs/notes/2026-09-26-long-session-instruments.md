# Agent Note

Status: implemented
Class: process
Date: 2026-09-26

## Problem

[long-session-plan.md](../long-session-plan.md) Wave F asked for three
instruments before the waves that change prompt shape (B) and cost (E)
ship: a per-turn series, a chained-compaction replay scenario, and a
summary-chain invariant. Two of the three could not ship as written.

- Time to first token, the number that says whether a provider served the
  prompt prefix from cache, existed only as `Event::TurnUsage.ttft_ms` on
  the wire; the log had no copy, so no fold could read it back.
- A replay recording cannot catch a regression of the checkpoint exemption
  (Wave A1): the replay serves the *recorded* summariser replies whatever
  request the harness builds, and the request body is not logged. A
  recording with two compactions would diff clean with the exemption
  removed.

## Decision

1. **`ttft_ms` is a log-only optional field on `EventKind::TokenUsage`.**
   Serde-defaulted, so every existing log reads as `None`; not a protocol
   change, since the wire carries it already.
2. **`project::TurnSeries` is the series**, one row per turn from
   `TurnStart`/`TurnEnd`, `TokenUsage`, `CompactionSummary`, pruned
   `ToolResult`s, `LlmRetry` and `ToolCall`. `prompt_tokens` is the turn's
   *largest* prompt, not the sum: the question is how full the window got.
   `ttft_ms` is the first hop's: later hops ride a warm prefix by
   construction.
3. **`/stats` prints the series as text**, newest twenty rows, oldest
   first. The FE renders the command's reply as it does today, so there is
   no `Response` change and no bump.
4. **F1 ships as tests, not a recording.** `compact::fold_request` is the
   request assembly pulled out of `summarize_fold` so a unit test can pin
   that a checkpoint goes whole; `chat::tests::two_chained_compactions…`
   drives the real `compact_session` twice over a replay script and checks
   the span, the invariant and the todo re-attachment. A recording remains
   worth making on Windows for E4 and for running F2 on real summaries;
   the plan's F1 carries the recipe.
5. **`summary-chain` checks identifiers, not length.** Every backticked
   span of the newest earlier checkpoint inside the fold must appear in the
   later summary. A shorter consolidated checkpoint is what consolidation
   produces; a lost path is what it must not.

## Consequences

- The series is readable on any log written from now on; older logs show
  `-` in the `ttft_ms` column and everything else filled.
- `summary-chain` is an ERROR `LogLine` under `--invariants`, so a replay
  run fails on it. Today's recordings have no backticks in their summaries
  and pass trivially; a future recording on a real model that drops a
  path will fail the run, which is the point.
- `TurnSeries` reaches only the `/stats` command. A sidebar chart would
  need a wire type; that is left until someone wants it.

## Alternatives

- **Add `series` to `Response::SessionStats`.** A protocol bump and FE
  work for a table read once a day while tuning; deferred.
- **Author the recording by hand.** A `session.jsonl` written without
  running the harness would either diff red on the first run or, worse,
  encode a wrong expectation as green; not done.
