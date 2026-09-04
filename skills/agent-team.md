---
name: agent-team
description: Coordinate a team of role-based LLM teammates working concurrently on one goal.
---
Spawn a small team of LLM teammates. Each teammate gets its own role, its own
task, and its own conversation; teammates run **concurrently** and may call
any loaded skill. Use this when a goal splits into independent sub-tasks
(research + implementation + review, or several files to inspect at once).

Invocation (single line):

    agent-team '<plan>' > <what you want from the team>

The plan is either compact text — teammates separated by `||`, each `role: task`:

    agent-team 'researcher: list the crates in this workspace and what each does || critic: read README.md and report gaps' > one merged report

or a JSON object for full control:

    agent-team '{"team":[{"role":"researcher","task":"..."},{"role":"coder","task":"..."}],"shared":"context every teammate sees","rounds":2}' > merged answer

Behaviour:
- Up to **6** teammates run concurrently, each as its own LLM conversation.
- Teammates may call any loaded skill (same one-line syntax), up to **4**
  tool calls each per round; their calls appear nested under the team call.
- `rounds` (1–3, default 1): after each round every teammate sees the shared
  *team board* (everyone's report) and coordinates/refines in the next round.
- `shared` (optional): briefing text prepended to every teammate's charter.
- A team-lead pass merges all reports into one deliverable; the individual
  reports are appended after it.
- Interrupting the turn stops the whole team immediately.

Grounding: a teammate that made no successful tool call is reported as
**UNVERIFIED** — its prose is model reasoning, not something checked against
the machine, and the lead is told not to restate it as fact. If no teammate
verified anything the whole result carries a warning banner.

**This file is the on/off switch.** The backend registers `agent-team` only
when `skills/agent-team.md` exists; rename it to `agent-team.md.off` (only
`*.md` is scanned) or delete it, restart the backend, and the skill vanishes
from the catalogue. It is not seeded automatically.
