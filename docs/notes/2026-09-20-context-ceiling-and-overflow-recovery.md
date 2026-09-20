# Agent Note

Status: implemented
Class: architecture
Date: 2026-09-20

## Problem

The prompt budget was policed only from the inside: a heuristic (or
usage-anchored) token count against `threshold_pct` of the budget, and the
trimmer as a backstop. Three things slipped past that:

- The threshold is a share of the *budget* (window minus the reply reserve),
  tunable up to 99 %. On a large window with a small `max_tokens` that share
  can sit above 95 % of the window itself, which is where local servers
  start refusing or silently shifting context.
- The window is learned once, at connect, and a provider TOML can pin one
  that is larger than what the server was actually launched with
  (`--ctx-size`). Nothing corrected it afterwards.
- When the server *did* refuse a prompt as too long, the 400 was classified
  `Fatal` and the turn ended with "not retryable". The failed request had
  persisted nothing, so the loop could have shrunk the history and tried
  again — dsh's `context-overflow` compaction trigger — but the client
  dropped the response body, so the loop could not tell this 400 from a
  malformed-request 400.

## Decision

1. **A hard ceiling above the policy.** `protocol::CONTEXT_CEILING_PCT = 95`
   is a percent of the *window*. The per-hop trigger is now
   `prompt ≥ threshold_pct · budget || prompt ≥ 95 % · window`; the pruner's
   "enough on its own" shortcut checks both too. It is a constant, not a
   knob: the policy threshold is the tunable, the ceiling is the safety line.
2. **Keep the error body.** `chat_stream` reads the body of an error status
   and carries a one-line, 400-char excerpt as anyhow context
   (`HTTP 400: …`), keeping the `reqwest` error underneath so the status
   split still works. The operator's "LLM request failed (…)" line now
   says why.
3. **Classify overflow by wording.** `Failure::ContextOverflow { reason,
   limit }` is judged before the status split from provider phrasings
   (llama.cpp `exceed_context_size_error`, vLLM / OpenAI
   `context_length_exceeded`, Anthropic-style "prompt is too long"), and
   `context_limit_in` parses the window the refusal names (vLLM's
   "maximum context length is N", llama.cpp's `"n_ctx":N`).
4. **Recover in the loop.** On overflow the turn adopts a *smaller* window
   the refusal names (or a fresh `detect_context_window` reports), stores it
   on the hub, re-emits `LlmStateChanged`, rewrites the envelope options,
   force-compacts against the corrected budget and re-enters the step. Up to
   three times per step. When neither the refusal nor the server names a
   limit, the budget shrinks 10 % per attempt for the rest of the turn so the
   trimmer drops more. Each attempt is an `LlmRetry` row with `delay_ms: 0`.
   `retry_always` never repeats an overflow verbatim.

## Consequences

- No policy setting can send a prompt past 95 % of the window, and a wrong
  window is corrected the first time the server says so, session-wide.
- A refused request costs one round-trip plus a summariser call, then
  continues — the session no longer dies on a mis-sized `--ctx-size`.
- Only a *smaller* reported window is adopted. A larger one means the
  heuristic under-priced the prompt, which the shrink covers; adopting a
  larger window would loop.
- The shrink is per turn, not per session: the next turn starts at the full
  budget and may pay one refused request again. A learned window persists.
- The envelope options JSON is unchanged in shape, so the replay snapshots
  still diff clean; a replay client is never probed for its window.

## Alternatives

- **Parse the limit only, never probe.** llama.cpp's text-only refusal
  (older builds) names nothing; the `/props` probe covers it for free.
- **Adopt the window from every successful `usage`.** `usage` reports what
  was used, not what fits; it cannot say where the ceiling is.
- **Make the ceiling a policy field.** It would need a protocol bump and a
  settings row for a number nobody should lower, and the tunable threshold
  already exists for "compact earlier".
- **Shrink the budget on every overflow, never adopt the window.** Converges
  slower and forgets the lesson at the next turn; the named limit is exact.
