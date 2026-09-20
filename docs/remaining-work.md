# Remaining work after Wave 10 / UI-9

Status on 2026-09-19, commit `25d04d1`. Both port guides are complete as
plans: every section carries a status word, harness Waves 1–10 and UI-1
through UI-9 are implemented, and the protocol is at v29. What follows is
the set of items the guides' last roadmap rows name as **later**, with
enough of a recipe for each that a fresh session can start on it without
re-deriving the design. Sizes are the guides' own (S / M / L / XL).

Read [harness-implementation-guide.md](harness-implementation-guide.md) and
[harness-ui-guide.md](harness-ui-guide.md) at the sections cited before
starting any item; the dsh reference checkout is at
`C:\Users\User\Documents\Projects\deepseek-harness` and the package paths
below are relative to its `packages/` folder.

## Ground rules that apply to every item

- Build and test only through `.\run.ps1` (gnullvm toolchain). Type-check
  with `.\run.ps1 check --workspace --all-targets`, then
  `.\run.ps1 test --workspace`, then build and run the smoke binary, then the
  replay binary. All four must stay green; a backend ERROR line fails replay.
- Any new `Request` / `Response` / `Event` variant bumps `PROTOCOL_VERSION`
  in `crates/protocol/src/lib.rs` and needs both binaries rebuilt. Add a row
  to architecture.md's protocol history and to UI guide §11.
- A new skill that is always registered re-prices every replay recording
  (it joins the text catalogue). Either make it opt-in on a
  `skills/<name>.md` doc like `workflow`, `agent-team` and `schedule`, or
  re-bless every scenario one at a time with
  `.\run.ps1 --% run -p frontend --bin replay -- --bless <name>` and say so
  in the commit message.
- Nothing is removed from a session log. New durable facts are new
  `EventKind` variants folded by a pure function in `sica_core::project`.
- Anything the operator should see goes through `Event::LogLine`.
- The sources and both guides are CRLF. Patch guides from a script that
  reads with universal newlines and writes CRLF; the Edit tool is fine for
  small anchored edits.
- When an item ships, update its section heading's status word, add a
  "Shipped as" paragraph, move it out of the **later** roadmap row into a
  new wave row, and record any decision the code cannot explain in
  `docs/notes/<date>-<topic>.md`.

## Small items (do these first)

### S1. `skills/run-cli.md` and `run-pwsh.md` seeds are stale

The seed text in `crates/agents/src/builtins.rs` (`RUN_CLI_SEED_MD` and its
pwsh twin) still documents only `command` and `cwd`. It should also list
`background` (harness §12.4), `timeout_secs` (§6.3, clamped range) and the
overflow behaviour (§6.9: a stream over 32 KiB is spilled whole and the
result carries the head plus the spill path). Seeds are written only when
the file is absent, so also update the checked-in copies under `skills/`.
No replay impact: seed docs are not part of the catalogue text.

### S2. Model Experience blocks in the skill seeds (harness §14.4)

dsh's package READMEs end with a *Model Experience* section: what the model
sees, the token effect, the KV-cache effect. Add one block to each seed in
`skills/*.md` (and the seed constants they come from) and a short version to
CLAUDE.md's skill paragraph. Facts to state per built-in: the catalogue line
it adds, whether its result is trusted or wrapped in `UNTRUSTED_NOTICE`
(§9.4), its output caps, and whether it re-prices the prefix (only skills
that change the system prompt do). Measured numbers already known:
`workflow` +575 tokens on every prompt while on, the three schedule tools
+250 tokens of catalogue.

### S3. The output split (harness §6.1)

`SkillOutcome` in `crates/agents/src/skill.rs` is `{ ok, summary }`. Add
`value: Option<serde_json::Value>` and `presentation: Option<Value>`, both
defaulting to `None` so every existing skill compiles unchanged. Carry
`presentation` on `Event::ToolCallFinished` (protocol bump). First
consumer: `run-cli` / `run-pwsh` set `presentation` to
`{ "kind": "terminal", "exit_code": N, "stdout_bytes", "stderr_bytes",
"spill": path }` and the FE's `tool_row.rs` terminal body renders the exit
code from it instead of parsing the `[exit code: N]` marker. Keep the marker
in `summary` because the model reads that. Replay is unaffected as long as
`summary` text does not change.

### S4. A caller for `Request::MoveSession` (UI §4.3)

The backend implements `MoveSession { workspace_id, session_id, before }`
(dispatcher.rs:125) and nothing sends it. Add drag-to-reorder within a
workspace group in `crates/frontend/src/ui/sidebar.rs`, active only when the
order preference is *Manual*: on drop, send `MoveSession` with `before` set
to the row the item was dropped above (`None` for the end). Ungrouped stays
newest-first and does not accept the drag. The backend's
`Event::WorkspacesChanged` already refreshes the rows.

## Medium items

### M1. `harness.toml` and an editable Skills › Harness tab (UI §7.2)

The Harness tab (`crates/frontend/src/ui/settings/skills.rs`) lists
constants read-only. Make them settings:

1. Define `HarnessConfig` in `agents` with `serde` defaults equal to today's
   constants: shell timeout (`Skill::timeout`), stdout/stderr cap,
   `spill::SPILL_THRESHOLD`, `compact::PRUNE_THRESHOLD` and the 80/16
   `CompactPolicy`, `jobs::OUTPUT_CAP` / `READ_CAP`, `guard::THRESHOLDS`,
   the parallel pool cap, `MAX_TOOL_HOPS`, `registry::DESCRIPTION_CAP`.
2. Load it from `sica-settings/harness.toml` at backend start via
   `sica_core::paths::settings_dir()`; a malformed file is a `LogLine` and
   the defaults apply (same stance as hooks, MCP and web).
3. Thread it through `ChatHub` and the `ToolSubAgent` constructor instead
   of the constants. Keep the constants as the `Default` impl so tests do
   not change.
4. Frontend: the tab edits the values and writes the file with
   `sica_core::atomic::atomic_write`; the existing settings watcher
   (`watcher.rs`, `is_settings_path`) already covers `sica-settings/`, so
   add a `Request::ReloadHarnessConfig` (protocol bump) that the FE sends
   after a write, or have the backend watch the file itself.

Compaction thresholds affect replay recordings; keep the defaults
identical and add a test that `HarnessConfig::default()` equals the old
constants.

### M2. Lazy session bodies (harness §3.8)

`ChatHub::new_loaded` (chat.rs:1107) keeps every session log resident.
`sessions_store::list_headers` already reads only line 1. Plan:

1. Change the hub's session map value to an enum `Resident(Session) |
   Header(SessionHeader)`.
2. Materialise on first touch: `LoadSession`, a turn, search, projections
   (`SessionStats`), the invariants, `dump_session`, and the schedule timer's
   idle check. Grep chat.rs for every `sessions.get` / `get_mut` and route
   each through one `ensure_loaded(id)` helper.
3. `ListSessions` fills `SessionMeta` from headers plus a cheap tail read
   for `updated_at`, title and the `scheduled` flag (the flag needs the
   schedule fold, so either store it in the header on flush or accept a
   one-off fold at startup).
4. Measure startup with 200+ logs before and after; this buys startup time,
   not correctness, so do not ship it if the gain is small.

Smoke and replay must stay green; both open sessions through
`LoadSession`, so they exercise the materialise path.

## Large items

### L1. Persistent PTY: `terminal-*` tools (harness §6.7, §13.5)

Reference: `terminal/terminal`, `terminal/terminal-bash`,
`terminal/tool-terminal`, and the UI in `client/ui-sidebar-terminal`.

1. Add `portable-pty` to `[workspace.dependencies]` (it fetches fine).
2. New module `agents::terminal`: a registry keyed by owner (session id,
   or child id for delegates), one PTY per terminal id, bounded scrollback
   (dsh keeps a byte ring; 256 KiB is a reasonable start), one active
   `send` per owner at a time.
3. Skills `terminal-open` (shell, cwd), `terminal-send` (text, optional
   `wait_ms`), `terminal-read` (since cursor), `terminal-signal`,
   `terminal-list`, `terminal-close`. Long waits go through the jobs
   registry so the turn is not blocked (`run_in_background`).
4. Close every PTY the owner holds when the session ends or the child
   returns; include them in `control::CHILD_EXCLUDED` only if delegates
   should not get their own terminals.
5. Register the six tools opt-in on `skills/terminal.md` (seed as
   `terminal.md.off`) so recordings do not re-price; add the switch to
   Settings › Integrations next to the other three.
6. FE: a terminal block variant in `tool_row.rs` that follows the terminal
   id across rows (UI guide §13 mentions it as the surface this brings).

### L2. Continuable children and `send_message` / `interrupt_agent` (harness §12.1, §12.7)

Reference: `subagent/subagent`, `subagent/tool-subagent-control`,
`client/ui-subagent`.

1. A child today is one bounded `runner::run_conversation` whose report is
   its whole output. A continuable child is a real session in the hub with
   its own log, its own inbox (the inbox from §2.1 already exists), and a
   parent link in its `SessionCreated` header.
2. `subagent` gains `continuable: true`; the result is a child id plus the
   first report, and the child stays alive with its inbox open.
3. `send_message '<child>' '<text>'` pushes an inbox `followup` to the
   child and returns its next report; `interrupt_agent '<child>'` cancels
   the child's active turn (the same cancel path the FE's Stop uses).
4. Agent-team (§12.7) then logs teammate reports as `ToolResult` rows
   under the parent's `WorkflowRun` members instead of reconstructing the
   board from log lines.
5. Delegated conversations cannot be replay scenarios (§14.1), so cover
   this with `agents` unit tests over `llm::mock` and a smoke step.
6. UI: a child session row nested under its parent in the sidebar, and the
   run tree in `tool_row.rs` linking a member to the child's transcript.

Protocol: `SessionMeta.parent: Option<u64>`, a `Request::InterruptSession`
if the existing stop request is not reusable. Bump the version.

### L3. LSP client (harness §13.4)

Reference: `lsp/lsp`, `lsp/lsp-stdio`, `lsp/tool-lsp`.

1. Crate deps: `lsp-types` and a small stdio JSON-RPC framing of our own
   (the `tower-lsp` client half is heavier than needed).
2. `agents::lsp`: one server process per language id, configured in
   `sica-settings/lsp.toml` (command, args, file globs), spawned lazily on
   the first request for that language and killed with the backend.
3. Skills `lsp-definition`, `lsp-references`, `lsp-hover`,
   `lsp-symbols`, `lsp-diagnostics`; results rendered as a search card
   (UI §13 names it) reusing the grep row body.
4. Opt-in on `skills/lsp.md` like the others; add the Integrations switch.
5. First target is `rust-analyzer` on this workspace; test with a fixture
   crate under `target/` in a `#[ignore]` test that runs only when the
   binary is on PATH.

### L4. Windows sandbox for `run-cli` / `run-pwsh` (harness §10.4)

Reference: `sandbox/sandbox`, `sandbox/sandbox-policy`,
`sandbox/sandbox-windows-acl`, `shell/pwsh-sandbox`.

1. `windows-sys` features for `CreateRestrictedToken`,
   `CreateProcessAsUserW`, and the ACL APIs (`SetEntriesInAclW`,
   `SetNamedSecurityInfoW`).
2. `agents::sandbox::windows`: build a restricted token from the current
   one (drop admin SIDs, add a deny-only SID), grant that SID write access
   on the session's cwd by ACL, spawn the child under the token inside the
   existing Job Object from `agents::proc`.
3. Report enforcement as `Full | Partial | Unavailable`. Under the
   `ReadOnly` permission mode refuse to run unless `Full`; otherwise attach
   the level to the tool result so the trajectory shows it. Never run
   unconfined silently.
4. `sandbox_permissions` + `justification` args on the shell skills feed
   the approval broker (§10.2) when a command asks for more than the policy
   grants.
5. This is per-machine behaviour that the smoke test cannot assert; add a
   manual checklist to the section instead of a smoke step.

## UI polish still open

- **Models card** (UI §7.2, `ui/settings/llm.rs`): the collapsible
  *Customized settings* fold, a Cancel / Apply footer instead of applying
  on Connect, and a dashed *+ Add provider* card that writes a new provider
  TOML.
- **Sidebar collapse choreography** (UI §13): dsh's freeze-fade-slide when
  the sidebar toggles between 280 and 56 px; the animation hook is the
  `sidebar_width` id in `ui/mod.rs`.
- **Surfaces the large items bring**: a terminal block (L1) and a search
  card for LSP results (L3).

## Suggested order

1. S1 → S2 → S3 → S4 (one short session; S3 is the only protocol bump).
2. M1, then M2.
3. L1, then L2 (L2 reuses L1's owner-scoped registry shape), then L3.
4. L4 last, on a machine where the ACL behaviour can be checked by hand.
