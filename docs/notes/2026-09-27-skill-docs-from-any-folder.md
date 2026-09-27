# Agent Note

Status: implemented
Class: behaviour
Date: 2026-09-27

## Problem

The prompt tells the model a skill's contract is at `skills/<name>.md` —
`memory.md` says so and gives `read-file 'skills/run-cli.md'` as its example,
the `read-file` doc repeats it, and the idealist's tickets point there too.
That path is relative to the **app's** folder. `read-file` resolves relative
paths against the session's **working directory**, and since sessions got
folders of their own (guide §3.9) that is usually a project somewhere else.

`sessions/96` ran in a folder outside the app. Asked to use five agents, the
model tried to read `skills/agent-team.md` first, got
`no such file: <working dir>\skills/agent-team.md — check the path is
relative to the workspace root`, then `glob 'skills/*.md'` matched nothing.
Every contract was unreachable from every session not opened on the app's
own checkout, and the error named the "workspace root" — which in this
codebase is the app's folder, not the one relative paths resolve against.

## Decision

The three read-only file tools fall back to the app's `skills/` folder for a
relative `skills/…` path the working directory has nothing for
(`builtins::resolve_read`):

- `read-file 'skills/<name>.md'` reads the app's doc and opens its result
  with `[read from the app's skill folder: <absolute path>]`, so the model
  knows where the file really is.
- `glob 'skills/…'` with no match in the working directory lists the app's
  docs, as absolute paths, under a line saying so.
- `grep '<re>' 'skills…'` searches the app's folder the same way.

A project's own `skills/<name>.md` still wins. Only plain names follow the
first segment, so `skills/../sica-settings/.env` never leaves the folder.
Writes never fall back: `write-file` / `edit-file` on `skills/…` land in the
working directory, where the permission policy checks them.

The not-found error now says relative paths resolve against the working
directory. `memory.md`'s seed says the docs are in the app's own folder and
that `read-file 'skills/<name>.md'` opens one from any working directory
(and says "most skills" — Rust-only tools like `subagent` have no doc).

## Consequences

Every existing prompt text, including a user's edited `memory.md`, is true
again without re-teaching anything. The `skill-doc-from-another-folder`
replay scenario pins it: the replay's working directory (`scratch/work`) is
not its app root (`scratch`), which is exactly the `sessions/96` shape.

## Alternatives

Rendering the absolute skills folder into the prompt (a `{{skills_dir}}`
variable in `memory.md`) would be explicit, but it only reaches a
`memory.md` seeded after the change — the file is the user's once on disk —
costs a machine-specific path's tokens on every request, and leaves the
idealist tickets, the `read-file` doc and the model's own habit of writing
`skills/<name>.md` pointing at the wrong folder.
