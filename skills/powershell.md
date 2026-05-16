---
name: powershell
description: Reference of PowerShell commands the agent can run on Windows via run-pwsh. Default terminal vocabulary.
---
PowerShell is the default terminal on this host. Use the `run-pwsh` tool to
execute any command listed here. Pass the command string as `command` and an
optional `cwd` (relative to workspace root or absolute).

The shell runs as `powershell -NoLogo -NoProfile -NonInteractive -Command <command>`.
That means: no interactive prompts, no profile, 30-second timeout, stdout +
stderr each capped at 32 KiB. Plan commands that finish quickly and produce
focused output — pipe through `Select-Object`, `Where-Object`, or
`Measure-Object` to trim noise before it hits the cap.

## Conventions

- Pipeline chain operators `&&` and `||` are **not** available in Windows
  PowerShell 5.1. Run conditionally with `; if ($?) { B }`. Chain
  unconditionally with `;`.
- Variables use `$` (`$env:NAME` for env vars, not `$NAME`).
- Default file encoding is UTF-16 LE with BOM. Pass `-Encoding utf8` to
  `Out-File`/`Set-Content` when other tools must read the result.
- `ConvertFrom-Json` returns `PSCustomObject`, not a hashtable.
- Avoid `2>&1` on native executables — it wraps stderr lines as
  `NativeCommandError` and flips `$?` to `$false` even on exit 0.
- Quote paths with spaces using double quotes: `Get-Item "C:\Program Files"`.
- Use the call operator `&` to invoke an exe whose path contains spaces:
  `& "C:\Path With Spaces\tool.exe" arg`.
- Never use interactive cmdlets: `Read-Host`, `Get-Credential`,
  `Out-GridView`, `pause`, `$Host.UI.PromptForChoice` — they hang.
- For destructive cmdlets (`Remove-Item`, `Stop-Process`) pass
  `-Confirm:$false` and `-Force` to skip prompts.

## Filesystem

| Task | Command |
| --- | --- |
| List directory | `Get-ChildItem <path>` (alias `ls`, `dir`) |
| Recursive list | `Get-ChildItem -Recurse -File <path>` |
| Filter by name | `Get-ChildItem -Recurse -Filter *.rs` |
| File size sum | `Get-ChildItem -Recurse | Measure-Object Length -Sum` |
| Test existence | `Test-Path <path>` |
| Create directory | `New-Item -ItemType Directory <path>` |
| Copy | `Copy-Item -Recurse <src> <dst>` |
| Move / rename | `Move-Item <src> <dst>` |
| Delete | `Remove-Item -Recurse -Force <path>` |
| Hash file | `Get-FileHash <path> -Algorithm SHA256` |
| File properties | `Get-Item <path> | Select-Object *` |

Prefer the workspace's dedicated `read-file` / `write-file` skills for file
content. Use PowerShell for filesystem operations (move, copy, delete, walk).

## Processes & services

| Task | Command |
| --- | --- |
| List processes | `Get-Process` |
| Find by name | `Get-Process backend* | Select Id, Name, CPU, WS` |
| Kill PID | `Stop-Process -Id <pid> -Force` |
| Kill by name | `Stop-Process -Name backend -Force` |
| Run + wait | `Start-Process -Wait -NoNewWindow <exe> -ArgumentList <args>` |
| Inspect service | `Get-Service <name>` |

## Environment & system

| Task | Command |
| --- | --- |
| Read env var | `$env:PATH` |
| Set env var (session) | `$env:FOO = "bar"` |
| Show all env vars | `Get-ChildItem env:` |
| Current dir | `Get-Location` (`pwd`) |
| Change dir | `Set-Location <path>` (`cd`) — but prefer passing `cwd` to run-pwsh |
| Host OS info | `Get-ComputerInfo | Select OsName, OsVersion, OsArchitecture` |
| PowerShell version | `$PSVersionTable.PSVersion` |

## Text & data

| Task | Command |
| --- | --- |
| Grep-like search | `Select-String -Path 'crates\**\*.rs' -Pattern 'TODO'` |
| Count matches | `Select-String -Path . -Pattern X | Measure-Object` |
| First N lines | `Get-Content <path> -TotalCount 50` |
| Last N lines | `Get-Content <path> -Tail 50` |
| Tail follow (avoid) | `Get-Content -Wait` — won't return inside the 30s cap; don't use |
| Parse JSON | `Get-Content <path> | ConvertFrom-Json` |
| Emit JSON | `$obj | ConvertTo-Json -Depth 5` |

Prefer the dedicated `Grep` and `Glob` tools over `Select-String` /
`Get-ChildItem -Recurse` when the workspace harness exposes them — they are
faster and respect ignore files. Use PowerShell text tools only inside ad-hoc
pipelines or when post-processing other command output.

## Networking

| Task | Command |
| --- | --- |
| HTTP GET | `Invoke-RestMethod https://api.example.com/x` |
| HTTP GET (raw) | `Invoke-WebRequest https://example.com -UseBasicParsing` |
| POST JSON | `Invoke-RestMethod -Method POST -ContentType application/json -Body ($obj | ConvertTo-Json) <url>` |
| Resolve DNS | `Resolve-DnsName example.com` |
| Test TCP port | `Test-NetConnection example.com -Port 443` |
| List listening | `Get-NetTCPConnection -State Listen | Select LocalPort, OwningProcess` |

## Multiline strings to native exes (git, cargo, etc.)

Use a literal single-quoted here-string — the closing `'@` **must** be at
column 0 with no leading whitespace, on its own line:

```powershell
git commit -m @'
Subject line.
Body line with $literal dollars and `backticks`.
'@
```

For arguments PowerShell would parse as operators, use the stop-parsing token:

```powershell
git log --% --format=%H
```

## Project-specific shortcuts

This workspace pins a Windows-gnullvm Rust toolchain that needs LLVM-MinGW on
PATH. Always invoke cargo through the wrapper, never `cargo …` directly:

```powershell
.\run.ps1 build --workspace
.\run.ps1 test  --workspace
.\run.ps1 run   -p frontend
.\run.ps1 run   -p frontend --bin smoke   # end-to-end check
```

`run.bat` is the cmd.exe equivalent. `start.bat` builds + launches the GUI in
one shot.

## When NOT to reach for PowerShell

- File reads → use the `read-file` skill (handles workspace-root resolution
  and traversal protection).
- File writes → use the `write-file` skill.
- Codebase-wide search → use the `Grep` / `Glob` tools if available.
- Anything cross-platform or POSIX-shaped → use `run-cli` (`cmd /C` on
  Windows, `/bin/sh -c` elsewhere) when the command is shell-portable.
