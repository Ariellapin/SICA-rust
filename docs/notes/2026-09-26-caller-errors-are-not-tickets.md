# Agent Note

Status: implemented
Class: behaviour
Date: 2026-09-26

## Problem

Every failed sub-agent tool call was forwarded to the idealist, which wrote an
`Improvement-BE-tool_failed-*.md` ticket for it. That included failures the
model caused with its own input and that the tool's error already explains:
`read-file` on a path that does not exist, an `edit-file` anchor that does
not match, a missing argument. A session that probes error paths on purpose
(or a model that guesses a path once) filed a "backend improvement" per probe,
burying the tickets that point at real harness or environment problems.

## Decision

`idealist::analyzer::is_caller_error` recognises those caller-input failures
and the daemon loop drops them before classification. Missing-file errors
from `read-file` / `edit-file` now read `no such file: <path> — …` with the
recovery step, instead of the raw OS `stat …: (os error 2)` text.

## Consequences

The tool result is still red in the chat and the trajectory, and still
reaches the model unchanged; only the ticket is skipped. Timeouts, permission
errors, oversized files and shell failures still produce tickets.

## Alternatives

A `caller_error` flag on `SkillOutcome` would be exact, but touches every
skill's constructor; the message patterns are the ones this crate already
matches on in `propose_fix_for_tool`.
