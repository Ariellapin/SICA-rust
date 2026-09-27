---
name: run-pwsh
description: Execute a PowerShell command on the host. Preferred on Windows.
---
Run a command through PowerShell. Use this instead of `run-cli` when the host
is Windows and you need PowerShell-specific cmdlets, aliases, or PATH lookup
behaviour (notably: anything that fails under `cmd.exe` with
`is not recognized as an internal or external command`).

Args (JSON):

```
{
  "command": "Get-ChildItem | Measure-Object",
  "cwd":     "."
}
```

One-line form:

    run-pwsh '<command>' > <what you want to know>

The command reaches PowerShell exactly as written between the outer quotes:
backslashes are literal (`C:\new\raw` stays a path, `\s+` stays a regex),
and quotes inside it are fine (`-ne ' '`, `"it's"`). Put the
` > <expectation>` straight after the closing quote.

Behaviour:
- Windows: invokes `powershell -NoLogo -NoProfile -NonInteractive -Command <command>`.
  Falls back to `pwsh` (PowerShell Core) if `powershell.exe` is missing.
- Non-Windows: invokes `pwsh -NoLogo -NoProfile -NonInteractive -Command <command>`.
- Stdout and stderr are each capped to **32 KiB** before being returned.
- A timeout of **30 seconds** kills the child and reports an error outcome.
- The outcome `ok` mirrors the child exit code (0 = ok).
