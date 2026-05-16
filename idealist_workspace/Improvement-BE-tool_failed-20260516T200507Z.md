---
kind: BeFix
module: agents::tool::run-cli
severity: Error
category: Logic
created: 20260516T200507Z
---

# Backend improvement: exit=1
--- stdout ---

--- stderr ---
'playwright' is not recognized as an internal or external command,
operable program or batch file.


**Module:** `agents::tool::run-cli`
**Trigger kind:** `tool_failed`

## Message

```
exit=1
--- stdout ---

--- stderr ---
'playwright' is not recognized as an internal or external command,
operable program or batch file.

```

## Traceback

```
host_os=windows
host_family=windows
depth=0
args=run-cli 'playwright codegen https://example.com'
```

## Proposed fix

On Windows, `cmd.exe` couldn't resolve the command. Retry with the `run-pwsh` skill — PowerShell uses a different PATH/alias resolver and is the wrapper-script shell the project targets (see CLAUDE.md). Failing summary: exit=1
--- stdout ---

--- stderr ---
'playwright' is not recognized as an internal or external command,
operable program or batch file.


## Suggested skill swap

Retry the failing operation with **`run-pwsh`** instead of the skill that just failed. See `skills/run-pwsh.md` for the contract.
