# sica-rust

A Rust desktop app split into two cooperating binaries:

- **`backend`** — a long-lived background process holding the business logic.
  Edit the code under `crates/backend/src/be_core/`, rebuild, restart, and the
  running process picks up the new behavior.
- **`frontend`** — a native GUI (egui/eframe). Spawns the backend as a child
  process, talks to it over a named pipe, displays responses and pushed events,
  and offers buttons to rebuild + restart the backend (manually or on file
  change).

## Layout

```
crates/
├── protocol/   shared wire types (no I/O)
├── backend/    long-lived BE binary; src/be_core/ is the editable surface
└── frontend/   eframe GUI + supervisor (spawn, IPC, build, watch)
```

## Prerequisites

This workspace is configured for the **`x86_64-pc-windows-gnullvm`** Rust
target, which uses **LLVM-MinGW** for linking and runtime DLLs.

Install both:

```powershell
# Rust toolchain (per-workspace pin is in rust-toolchain.toml; rustup will
# install the right channel automatically when you first run `cargo`).
# If you do not yet have rustup:
#   irm https://win.rustup.rs | iex

# Linker + runtime DLLs.
winget install MartinStorsjo.LLVM-MinGW.UCRT
```

The LLVM-MinGW `bin/` directory must be on `PATH` both at build time (so cargo
finds the linker) and at run time (so the binaries find their runtime DLLs).
Two options:

1. **Use the wrapper** — `run.ps1` in the workspace root sets `PATH` for you.
2. **Add to user PATH permanently** — `winget` already added LLVM-MinGW to
   user PATH; for `cargo` you may also want `%USERPROFILE%\.cargo\bin`:
   ```powershell
   [Environment]::SetEnvironmentVariable("Path", $env:Path + ";$env:USERPROFILE\.cargo\bin", "User")
   ```

## Build & run

Through the wrapper (works without touching your environment):

```powershell
.\run.ps1 build --workspace
.\run.ps1 test  --workspace
.\run.ps1 run   -p frontend                    # the GUI
.\run.ps1 run   -p frontend --bin smoke        # end-to-end smoke test (no GUI)
```

Or, if both `cargo` and the LLVM-MinGW bin are on your PATH:

```powershell
cargo build --workspace
cargo run -p frontend
```

## What the GUI gives you

Top bar:
- **Start BE / Stop BE** — start or stop the backend child process.
- **Rebuild** — kills the BE, runs `cargo build -p backend`, streams output.
- **Rebuild & Restart** — same, then respawns BE on success.
- **Auto-watch** — when checked, edits to `crates/backend/src/**` debounce
  (1s) and trigger Rebuild & Restart automatically.
- **Release profile** — toggle between `--profile dev` and `--release` for
  the BE build.

Request bar:
- Pick one of `GetCounter | Increment | Reset | Fib | Echo`, set its arg,
  click **Send**. The log panel shows the request and matching response.

Log panel:
- Color-coded lines for backend stdout/stderr, build output, IPC frames, FS
  events. Autoscroll toggle in the top bar.

Status bar:
- BE state + PID, IPC connection state, last build result + duration.

## End-to-end verification

```powershell
.\run.ps1 run -p frontend --bin smoke
```

The `smoke` binary spawns `backend.exe`, connects, exchanges a handshake,
sends `IncrementCounter { by: 3 }` + `ComputeFib { n: 10 }`, asserts the
expected responses, sends `Shutdown`, and confirms the backend exits 0.

## Editing business logic

The only files you should typically edit when iterating on logic:

```
crates/backend/src/be_core/
├── mod.rs       — BeState wiring
├── counter.rs   — example state
└── fib.rs       — example compute
```

To add a new request:
1. Add a variant to `Request` (and matching `Response`) in
   `crates/protocol/src/lib.rs`.
2. Handle it in `crates/backend/src/dispatcher.rs`.
3. (Optional) Add a UI control in `crates/frontend/src/ui/controls.rs` to
   send it.

A protocol change requires rebuilding **both** binaries — Auto-watch only
watches the BE crate by design. Either restart the FE manually or run
`cargo build --workspace` (the FE supervisor will reconnect).

## Wire protocol

- Transport: Windows named pipe (`\\.\pipe\sica-rust-<fe-pid>`) via the
  `interprocess` crate's `tokio` API.
- Framing: length-delimited (`tokio_util::codec::LengthDelimitedCodec`).
- Payload: `bincode`-encoded `protocol::Frame`.
- Fully duplex over a single connection: requests / responses / pushed
  events / heartbeats all multiplex.

## Toolchain notes

- `rust-toolchain.toml` pins `stable-x86_64-pc-windows-gnullvm`.
- The MSVC toolchain is intentionally not used here (avoids the 3–5 GB
  Visual Studio Build Tools dependency).
- If you want to switch back to MSVC, edit `rust-toolchain.toml` and remove
  `windows-sys` from the backend's `[target.'cfg(windows)'.dependencies]`
  block (or keep it — both ABIs link against it).
