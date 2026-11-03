---
name: run-cli
description: Execute a shell command on the host (cmd.exe on Windows, /bin/sh elsewhere).
---
Run a shell command on the host. stdout and stderr are captured and returned
to the agent in the outcome `summary`.

Args (JSON):

```
{
  "command": "git status",   // required, the shell command line
  "cwd":     "."             // optional, working dir (relative to workspace root or absolute)
}
```

One-line form — the command is taken exactly as written between the outer
quotes, backslashes and inner quotes included:

    run-cli 'git status' > what changed

On Windows this is `cmd.exe`, and the line reaches it exactly as typed —
`type "C:\My Files\a.md"` and `findstr /C:"two words" f.txt` work as they
would at a prompt. cmd has no `;`: chain commands with `&` (or `&&` to stop
at the first failure). A line that starts with a PowerShell cmdlet
(`Get-ChildItem …`) or a `$variable` runs in PowerShell instead, and the
result says so in a `--- note ---` section — call `run-pwsh` for PowerShell
rather than wrapping it in `powershell -Command "…"`.

Behaviour:
- Windows: invokes `cmd /S /C "<command>"`. Other OSes: `/bin/sh -c <command>`.
- A failure with a known cause ends with a `--- hint ---` section saying how
  to fix it.
- Stdout and stderr are each capped to **32 KiB** before being returned.
- A timeout of **30 seconds** kills the child and reports an error outcome.
- The outcome `ok` mirrors the child exit code (0 = ok).

Use this for build tools, git, package managers, or one-shot scripts.
