# Agent Note

Status: implemented
Class: architecture
Date: 2026-09-26

## Problem

Three costs grow with a session rather than with a turn
([long-session-plan.md](../long-session-plan.md) items 8, 9 and 10). Every
hop folded the whole event log several times over — history, compaction,
the pruner, the instructions refresh, the invariants — and each fold is
O(events); at twenty thousand events one fold measured 45 ms in the debug
build the app actually runs, so a hop paid a quarter of a second before
any request went out. `spill/<session>/` had no sweeper, so a session
that spilled a `run-cli` result every few hops left hundreds of files
that nothing ever read again. And a compaction on a large window went to
the same local model that runs the conversation, whole span in, with no
way to hand the fold to a smaller model.

## Decision

1. **The fold is memoised on the log, keyed by its length.** The log only
   grows, and only through `SessionLog::append`, so the number of events
   is the whole cache key: a fold is recomputed exactly when something was
   appended since. `derive_surface` now hands back an
   `Arc<Vec<SurfaceEntry>>`, the same allocation until the next append,
   and `WireHistory.entries` holds that allocation rather than a copy.
   Measured by the ignored `fold_time_at_twenty_thousand_events` test:
   45 ms per uncached fold, 2 ms through the log. The plan's cut-off was
   "under a millisecond, ship the cache anyway and stop": the uncached
   number is well over it, so the cache is worth having, and incremental
   folds are still not — the cached path is two orders of magnitude
   cheaper than a request.

   The invariants path keeps its own direct `derive_messages` call on
   purpose: its job is a second, independent observation of the log, and
   reading the cache would make it the same observation.

2. **Spilled files are swept by age and by size.** `spill::sweep` removes
   files older than `spill_max_age_days` (7) and then, per session,
   the oldest until the total is under `spill_max_mib_per_session` (256);
   both are `harness.toml` keys and `0` switches a rule off — the one
   place in that file a zero is a setting rather than a typo, because a
   sweeper that never sweeps is a thing someone may want. It runs at
   backend start and hourly. Nothing under `spill/` is read back by the
   harness, so a sweep can only cost the model a `read-file` on a path a
   checkpoint still names, and no checkpoint reaches back a week. Empty
   session directories are left in place: a running session may be about
   to write into one, and a job's spill file is appended to as it runs.

3. **The summariser may be a different model on the same provider.**
   `CompactPolicy.model` (v31), from the provider TOML's `compact_model`
   and the Models card's *Sum model* row. `summarize_fold` swaps only the
   model name on its clone of the client — same endpoint, key and
   sampling — so a name the provider does not serve fails the call and the
   retry budget reports it like any other summariser failure. The request
   envelope names it only when set, so existing recordings do not change.

## What was not done, and why

- **E3, lazy session bodies** (remaining-work M2). The plan makes it
  conditional on restart time with months of sessions, a number only a
  machine that has them can produce; a fresh checkout loads in no time
  either way. The recipe in M2 stands.
- **E4's chunked folds.** Conditional on the compaction time on a 64k
  window *with the model knob set* being over a minute. The knob is the
  instrument for that measurement, so it ships first; chunking gives up
  the prefix property for the second chunk and should not be paid for
  until the number says so.

## Alternatives considered

- **Incremental folds** (apply only the appended events to the previous
  surface). A `Replace` or a `Rewind` can shadow any earlier span, so an
  incremental step has to be able to undo — the full fold at 45 ms is
  simpler than that machinery and the cache already removes the repeats.
- **Keying the cache on `last_seq`.** Equivalent while `append` is the
  only writer; the length is the cheaper read and the same invariant.
- **Sweeping on every spill write.** Would put directory scans on the
  tool path; an hourly pass is enough for a directory that grows by
  files, not by gigabytes per minute.
- **Deleting empty session directories.** Saves nothing and races a job
  about to append.
- **A separate provider for the summariser.** dsh scopes the knob to a
  model name; a second endpoint means a second key and a second
  connection state, and the case that matters — a small model beside the
  big one on the same llama.cpp or vLLM — needs neither.
