# Agent Note

Status: implemented
Class: architecture
Date: 2026-09-26

## Problem

One human message could drive at most ~36 tool calls before the session
went idle: a hard cap of 12 hops per turn, then a completion check that
reopens the turn at most twice ([long-session-plan.md](../long-session-plan.md)
item 6). Both numbers were sized for a chat. A two-hour task run against
them stops every thirty-odd calls to be told "continue", and the only way
past was a goal — which then competed with the auto-continue for the same
stop: a hop-limit turn under an armed goal spent one of the two continues
on "carry on from here" before the goal driver got a round, so the goal's
own round budget and re-grounding prompt were the *third* thing tried,
not the first.

Separately, the judge that audits an abnormal stop saw only the tool calls
after a mid-turn compaction, because the calls before it were listed but
the checkpoint the model wrote about them was not.

## Decision

1. **The two budgets are a settings file, read once.** `agents::harness`
   reads `sica-settings/harness.toml` at backend start into
   `HarnessConfig { tool_hops_text, tool_hops_native, auto_continues }`,
   defaults 12 / 32 / 2, and `ChatHub::harness` carries it. The stance is
   the one every other file under `sica-settings/` takes: absent is the
   normal case, malformed is a `LogLine` and the defaults apply, and a
   `0` is refused per field (a zero hop cap ends the turn at the first
   call; a zero continue budget silently disables the check's follow-up),
   also as a `LogLine`. A file that loaded is announced with its values,
   because a budget that fell back silently is exactly what someone
   editing the file would go looking for. Read once, like the hooks: a
   cap that changed under a running turn would make two hops of one turn
   answer to different budgets.

2. **Native turns get 32 hops, text turns keep 12.** A native batch
   already overlaps its reads, the repeat guard catches loops at 3/5/8,
   and the completion check still audits every abnormal stop, so the
   longer leash costs nothing in safety. Text mode is where the small
   models that emit one call per reply run, and they drift; the shorter
   leash stays. PTC rides the native transport and gets the native cap.

3. **A hop-limit stop under an armed goal is the goal driver's.**
   `verdict::after_stop` decides the continuation four ways — met, goal
   round, auto-continue, exhausted — and prefers the round only for
   `hop-limit`: that stop means the model was still working and ran out
   of leash, which is what the round prompt (objective, workspace, a
   fresh round budget) is for, and the round costs nothing from the
   continue budget. The verdict is still taken and recorded, so the log
   and the operator see the audit either way. `max_tokens` cut a reply
   mid-sentence and `error` is a transport failure; neither is helped by
   re-grounding, so both keep the narrower continuation.

4. **The judge reads the checkpoint's Next Step.** `verdict::digest`
   quotes the latest `CompactionSummary`'s **Next Step** section
   (`agents::compact::section`) when the turn compacted, newest fold
   wins, `(none)` adds nothing. The calls before the fold stay listed;
   what the model itself said remained is the better evidence of where
   the work stood.

## Consequences

- No protocol change. The FE Harness tab lists the two budgets at their
  defaults and names the file; it cannot show the loaded values without
  a wire type, and the backend's startup `LogLine` does that instead.
- Replay recordings are unaffected: no scenario reaches a cap or runs a
  verdict, and recordings carry replies, not requests.
- The rest of remaining-work M1 (shell timeout, output caps, compaction
  thresholds, the editable tab) is still open; the loader uses
  `deny_unknown_fields` so a typo is reported now, and those keys join
  the same struct when they move.
- The goal-round path is exercised end to end by
  `chat::tests::a_hop_limit_under_an_armed_goal_opens_a_round_not_a_continuation`
  with the model served from a replay script — the first test to drive
  `send_user_message` through the real loop. It uses `notes-write` as
  the hop, since a harness control runs in the hub and no summariser
  call competes for the script.

## Alternatives considered

- **Raise the text cap too.** Rejected: the one-call-per-reply models
  that run text mode are the ones the repeat guard and the verdict exist
  for, and 12 was chosen against them.
- **Reload the file per turn.** Rejected for the same reason the hooks
  file is read once: two hops of one turn under different caps, and a
  budget that changes between a `hop-limit` stop and its verdict.
- **Feed the verdict's next step into the goal round prompt.** Not done:
  the round prompt's value is that it re-grounds on the objective rather
  than on the last turn's tail, and the `TurnVerdict` row is on the
  record for the operator. Revisit if rounds after a hop-limit stop
  visibly repeat the cut-short work.
- **A per-session `/hops` command.** Not needed yet; the file is enough
  for a budget that is chosen per machine and model, not per session.
