# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A two-binary Rust desktop app that hosts a local-LLM chat agent:

- **`backend`** — long-lived daemon (`crates/backend`). Holds chat sessions, the LLM connection, the skill (tool) registry, and the idealist daemon.
- **`frontend`** — egui/eframe GUI (`crates/frontend`) that spawns the backend as a child process, talks to it over a Windows named pipe, and offers rebuild/restart controls.

The split exists so the GUI can hot-reload backend logic. The FE watcher rebuilds
**only** `backend`, so a change to any other crate needs the FE restarted too.
See [docs/architecture.md](docs/architecture.md#what-this-is).

## Build / run / test

**Always use the wrapper scripts.** This workspace targets `x86_64-pc-windows-gnullvm` (pinned in [rust-toolchain.toml](rust-toolchain.toml)) and needs LLVM-MinGW on PATH; the wrappers prepend it. Direct `cargo …` invocations will fail.

```powershell
.\run.ps1 build --workspace
.\run.ps1 test  --workspace
.\run.ps1 run   -p frontend                  # launches the GUI
.\run.ps1 run   -p frontend --bin smoke      # headless E2E smoke test
```

Cargo filters are forwarded verbatim (`.\run.ps1 test -p agents md_skill`). To pass
flags to the test binary use PowerShell's stop-parsing token:
`.\run.ps1 --% test -p agents proc -- --nocapture`. `run.bat` is the cmd.exe
equivalent; `start.bat` builds + launches the GUI; `.\run.ps1 cmd <exe> <args…>`
runs any other binary with the same PATH.

No clippy/rustfmt/lints config and no CI. Tests are inline `#[cfg(test)]` modules
(~200, concentrated in `agents`). [crates/frontend/src/bin/smoke.rs](crates/frontend/src/bin/smoke.rs)
is the canonical end-to-end check — run it after any change to the protocol, IPC,
dispatcher, or `be_core`. It reads `target/debug/backend.exe`, so build first.

## Workspace layout

Seven crates, dependency direction strictly downward. Details in
[docs/architecture.md](docs/architecture.md#workspace-layout).

| Crate | Role |
| --- | --- |
| `protocol` | Wire types only + `PROTOCOL_VERSION`. Shared by both binaries. |
| `sica-core` | Shared utilities: `paths`, `event` (session log + `derive_surface`), `retain`, `message`/`session`, `build_id`, `theme`. |
| `llm` | HTTP client for OpenAI-compatible `/v1/chat/completions`, SSE streaming, connection state, token counting. |
| `agents` | Agent runtime: turns, skills, prompt assembly, compaction, delegation, jobs, goals, evals. |
| `idealist` | Classifies failures, writes improvement tickets to `idealist_workspace/`. |
| `backend` | Long-lived binary: dispatcher, `ChatHub` agent loop, legacy demo state. |
| `frontend` | egui GUI: supervisor (BE child + IPC + watcher), `app.rs` state, `ui/` surfaces. |

## Reference docs

- **[docs/architecture.md](docs/architecture.md)** — the crate graph, the wire
  protocol (framing, `PROTOCOL_VERSION` history v17–v22), every on-disk surface,
  how to add a new request, and the conventions to respect when editing.
- **[docs/agent-loop.md](docs/agent-loop.md)** — one turn end to end: history
  derivation, prune/compact/trim, retry classification, the two tool-calling
  modes, skills and the `ToolSubAgent` pipeline, the control plane, delegation,
  the inbox, background jobs, goals, and `model-eval`.
- [docs/harness-implementation-guide.md](docs/harness-implementation-guide.md) —
  every dsh feature and its concrete sica-rust design. Read the relevant section
  before adding a loop guard, prompt-assembly, approval, plan-mode, subagent, or
  jobs feature.
- [docs/harness-ui-guide.md](docs/harness-ui-guide.md) — the FE design system and
  every UI surface. Read it before restyling or adding a frontend surface.
- [docs/deepseek-harness-ideas.md](docs/deepseek-harness-ideas.md) — the ported
  ideas catalogue, and the ones deliberately left for later.

## Before you edit

- The workspace deliberately avoids MSVC. Don't switch the toolchain unless asked.
- Common dependency versions live in `[workspace.dependencies]` in the root [Cargo.toml](Cargo.toml).
- Three serialization formats coexist by design: `bincode` over the pipe (externally-tagged enums only), JSONL for session logs, `toml` for configs, `serde_json` for the LLM wire.
- Changing `Request`/`Response`/`Event` means bumping `PROTOCOL_VERSION` and rebuilding **both** binaries.
- Nothing is ever removed from a session event log; compaction and rewind shadow spans in the derived view.
- `Event::LogLine` is the channel for anything the operator should see in the GUI — a `warn!` alone is invisible.
- The FE design system is `sica_core::theme` + `ui::kit`; no module below `kit` names a literal colour.
