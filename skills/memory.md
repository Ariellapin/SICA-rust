---
name: memory
description: The agent's memory tools — remember, recall, forget (on by default).
disable-model-invocation: true
---
# Memory

While this file is `skills/memory.md` the agent has three tools:

- `remember '<fact>' [scope]` — save one fact. `scope` is `project` (the
  default: true in this working directory), `global` (true of the user in
  every project) or `session` (a key fact for this session only).
- `recall '<query>'` — search long-term memory and the memories of earlier
  sessions.
- `forget '<id>'` — delete a long-term memory by its `[m-…]` id.

Every session also keeps its own memory — a running summary and the key
facts — updated in the background while it is idle, and re-attached after a
compaction. Long-term memories that apply to a session's folder are put in
front of the model at the start of each turn. Both are in the header's
Memory popover and in Settings › Memory; `sica-settings/memory.toml` holds
the knobs.

Rename this file to `memory.md.off` (or use Settings › Integrations) to take
the three tools away; the backend reads it at startup. Memories already
saved still reach the prompt unless `inject = false` in `memory.toml`.
