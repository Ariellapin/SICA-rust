# Agent Note

Status: implemented
Class: architecture
Date: 2026-11-03

## Problem

The first look at the Issues page showed 15 open tickets, and none of them
had been investigated. Read together, they came down to a few causes.

- **Six were one bug.** `run-cli` started `Command::new("cmd").args(["/C",
  line])`. Rust quotes `line` for the MSVC argument rules and turns every
  inner `"` into `\"`, and cmd.exe does not read `\"`. As a result,
  `type "C:\a b\x.md"` arrived as `type \"C:\a b\x.md\"` ("the filename,
  directory name, or volume label syntax is incorrect"), and
  `findstr /C:"peach fuzz"` searched for `"peach` in a file named `fuzz"`.
  User hooks spawned the same way.
- **A cmdlet sent to `run-cli`.** `Get-ChildItem` under cmd.exe fails with
  "is not recognized".
- **A TAB in a path.** `write-file` takes two arguments, so the call parser
  decodes escapes in both. In a Windows path written with single
  backslashes, `\transform.ps1` became a TAB followed by `ransform.ps1`.
- **A `.ps1` that Windows PowerShell could not parse.** `sessions/102` wrote
  an em dash inside a string. PowerShell 5.1 reads a script without a
  byte-order mark in the ANSI code page. The dash's last byte (0x94) is `”`
  there, and PowerShell treats it as a closing quote.
- **The model's own PowerShell mistakes.** These were `"L$i:"` (a
  drive-qualified variable), quotes doubled for the outer quoting
  (`''@@''`), and a loop that modified the hashtable it was enumerating.
- **Two already fixed.** The context overflow in `sessions/95` was fixed by
  `prune_oversized_results`, and the 17 `bad_call` turns in `sessions/102`
  by commit 8ac2761.

Reading this code also showed four gaps:

- A backend that died on its own went unnoticed by the supervisor.
- The backend's panic hook named dependency files from the cargo registry.
- The investigator's source hints did not know frontend panics.
- `SetTicketStatus` and `StartFixSession` turned an unchecked ticket id
  into a path.

## Decision

- **`agents::shell`** is the one place a command line reaches a shell.
  `shell_command` passes the line to cmd raw, as `/S /C "<line>"`, so cmd
  strips exactly the outer pair of quotes. `run-cli`, its background jobs
  and the backend's hooks all use it. `pwsh_command` keeps `Command::arg`,
  because `powershell.exe` splits its command line by the MSVC rules that
  `arg` writes for.
- **On Windows, `run-cli` runs a PowerShell line in PowerShell.** A line
  counts as PowerShell when it opens with a cmdlet (an approved verb, a
  dash, a noun) or a `$variable`. The result carries a `--- note ---`
  saying so. A program cmd can find by that name (a user's
  `Start-Server.cmd`) still wins. Failing with a message would cost the
  model a turn on a line that can only fail.
- **A path's control characters are read back as their escapes**, in the
  shared `resolve`, so the permission check sees the same path.
- **PowerShell files with non-ASCII text get a UTF-8 BOM** from both
  `write-file` and `edit-file`. An append that brings the first non-ASCII
  text rewrites the file with the BOM at the start. ASCII scripts and all
  other files are written byte for byte.
- **Known failures carry a `--- hint ---`** (`shell::failure_hint`), and
  such a failure is not filed as a ticket (`analyzer::is_caller_error`).
  The tool output already tells the model how to recover, which is the
  existing rule for caller errors. A failure with no recognised cause
  still gets a ticket; that is how the quoting bug surfaced.
- **The supervisor watches the backend's exit.** When the backend ends and
  nobody asked it to, the supervisor marks it stopped, logs a line and,
  unless the exit code was 0, queues `frontend::supervisor::be_exit`.
- **Both panic hooks name the first frame in our own `crates/`**
  (`sica_core::crash`), so a panic surfacing inside egui is filed under
  the file that called egui.
- **`TicketStore::load` refuses an id that is not hex.** Every read and
  write goes through `load`.

## Consequences

- Each fix has a regression test, and the four central ones fail on the old
  code (checked by reverting each fix): `cmd_gets_the_line_as_typed`,
  `run_cli_keeps_quotes_as_typed`, `powershell_files_with_non_ascii_get_a_bom`,
  and `windows_powershell_runs_a_script_write_file_saved`. The other tests
  are `run_cli_runs_a_cmdlet_in_powershell`,
  `a_decoded_escape_in_a_path_is_read_back`,
  `hints_name_the_fix_for_each_ticketed_failure` and
  `only_hex_ids_reach_the_file_system`.
- The 15 tickets are closed, each with a section naming its fix and its
  test. A resolved ticket that fires again reopens as a regression.
- A `run-cli` line that was really meant for cmd but starts like a cmdlet
  now runs in PowerShell. That would need a program named like a cmdlet
  that cmd cannot find, which cannot have worked under cmd anyway.

## Alternatives

- **Refuse PowerShell lines in `run-cli` with a message.** Rejected: it
  costs the model a turn on a line that has only one way to succeed.
- **Strip control characters in the call parser instead.** Rejected: the
  parser decodes `\n` on purpose for `write-file` bodies. Only a path
  argument can never mean a control character.
- **Always write a BOM for `.ps1`.** Rejected: it would churn ASCII scripts
  in diffs for no reader's benefit.
