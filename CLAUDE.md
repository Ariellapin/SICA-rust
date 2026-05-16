# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A two-binary Rust desktop app:

- **`backend`** — long-lived daemon (`crates/backend`) holding business logic. Its editable surface is `crates/backend/src/be_core/`.
- **`frontend`** — egui/eframe GUI (`crates/frontend`) that spawns the backend as a child process, talks to it over a Windows named pipe, and offers rebuild/restart controls (including a 1-second-debounced auto-watch on `crates/backend/src/**`).

The split exists so the GUI can hot-reload backend logic: edit code, rebuild backend, supervisor respawns the child, IPC reconnects. A protocol change requires rebuilding **both** binaries — auto-watch only covers the backend crate by design.

## Build / run / test

**Always use the wrapper scripts.** This workspace targets `x86_64-pc-windows-gnullvm` (pinned in [rust-toolchain.toml](rust-toolchain.toml)) and needs LLVM-MinGW on PATH. The wrappers prepend `%USERPROFILE%\.cargo\bin` and the winget LLVM-MinGW `bin/` dir before calling cargo. Direct `cargo …` invocations will fail unless the user has already added both to PATH.

```powershell
.\run.ps1 build --workspace
.\run.ps1 test  --workspace
.\run.ps1 run   -p frontend                  # launches the GUI
.\run.ps1 run   -p frontend --bin smoke      # headless E2E smoke test
.\run.ps1 run   -p backend -- --ipc <pipe>   # rarely needed; FE normally spawns BE
```

`run.bat` is the cmd.exe equivalent of `run.ps1`. `start.bat` is a one-shot that builds + launches the GUI (`cargo run -p frontend`).

There is no `clippy.toml`, `rustfmt.toml`, lints config, or CI. The `smoke` binary (`crates/frontend/src/bin/smoke.rs`) is the canonical end-to-end check — it spawns the backend, exchanges the handshake, sends `IncrementCounter`/`ComputeFib`, asserts responses, then `Shutdown`s and confirms exit 0. Run it after any change that touches the protocol, IPC, dispatcher, or `be_core`.

## Workspace layout

Seven crates, dependency direction strictly downward:

| Crate | Role |
| --- | --- |
| `protocol` | Wire types only (`Frame`, `Request`, `Response`, `Event`). No I/O. Shared by both binaries — changes here force rebuilding both. |
| `sica-core` | Shared utilities (paths, config helpers). |
| `llm` | HTTP client for the llama.cpp OpenAI-compatible API, streaming, state machine (`Disconnected → Connecting → Ready / Error`), token counting. |
| `agents` | Agent runtime: `MainAgent` drives chat turns, `ToolSubAgent` wraps each tool call, `Skill` + `registry` for tool discovery. |
| `idealist` | Error classification (FeBug vs BeFix) and auto-fix suggestions. |
| `backend` | Long-lived binary. `main.rs` parses `--ipc/--parent-pid/--log-level`, accepts the named-pipe connection, `dispatcher.rs` routes requests to handlers, `be_core/` holds editable state (`BeState`). |
| `frontend` | egui GUI. `supervisor.rs` spawns/kills the backend child; `ipc_client.rs` connects the pipe; `settings_store.rs` persists JSON settings; `watcher.rs` debounces FS events for auto-rebuild; `ui/` holds the panels (controls, chat, log, settings, sidebar). |

## Wire protocol

- Transport: Windows named pipe `\\.\pipe\sica-rust-<fe-pid>` via the `interprocess` crate's tokio API.
- Framing: length-delimited (`tokio_util::codec::LengthDelimitedCodec`).
- Payload: `bincode`-encoded `protocol::Frame`.
- Full duplex over one connection: requests, responses, pushed events (`Heartbeat`, `Progress`, `LogLine`, `LlmStateChanged`), all multiplex. Each `Frame` carries an ID; unsolicited events use ID 0.

## Adding a new request (the common task)

1. Add a variant to `Request` (and matching `Response`) in [crates/protocol/src/lib.rs](crates/protocol/src/lib.rs).
2. Handle it in [crates/backend/src/dispatcher.rs](crates/backend/src/dispatcher.rs), delegating to logic in `crates/backend/src/be_core/`.
3. *(Optional)* Add a UI control in [crates/frontend/src/ui/controls.rs](crates/frontend/src/ui/controls.rs).

Step 1 is a protocol change → rebuild both binaries. Either restart the GUI manually or run `.\run.ps1 build --workspace` (the supervisor will reconnect after the FE restarts).

## Things to know before editing

- The workspace deliberately avoids MSVC to skip the multi-GB Visual Studio Build Tools dependency. Don't switch the toolchain unless asked.
- Common dependency versions live in `[workspace.dependencies]` in the root [Cargo.toml](Cargo.toml); reference them in member crates with `{ workspace = true }`.
- `bincode` (not `serde_json`) is the wire format — types crossing the pipe must be `serde::Serialize + Deserialize`-compatible with it (no untagged enums, no `serde(flatten)` with maps).
- Tracing logs go to stderr; the GUI captures backend stderr and renders it color-coded in the log panel.
