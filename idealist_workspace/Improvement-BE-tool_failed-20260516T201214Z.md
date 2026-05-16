---
kind: BeFix
module: agents::tool::run-pwsh
severity: Error
category: Logic
created: 20260516T201214Z
---

# Backend improvement: timeout after 30s

**Module:** `agents::tool::run-pwsh`
**Trigger kind:** `tool_failed`

## Message

```
timeout after 30s
```

## Traceback

```
host_os=windows
host_family=windows
depth=0
args=run-pwsh 'npx playwright open https://walla.co.il'
```

## Proposed fix

Sub-agent tool `run-pwsh` returned an error: timeout after 30s. Investigate the args and retry; consider whether a different skill is a better fit for this environment.
