---
kind: BeFix
module: agents::tool::run-pwsh
severity: Error
category: Filesystem
created: 20260516T201102Z
---

# Backend improvement: exit=1
--- stdout ---

--- stderr ---
npm warn exec The following package was not found and will be installed: playwright@1.60.0
Error: command.parse: Executable doesn't exist at C:\Users\User\AppData\Local\ms-playwright\chromium-1223\chrome-win64\chrome.exe
╔════════════════════════════════════════════════════════════╗
║ Looks like Playwright was just installed or updated.       ║
║ Please run the following command to download new browsers: ║
║                                                            ║
║     npx playwright install                                 ║
║                                                            ║
║ <3 Playwright Team                                         ║
╚════════════════════════════════════════════════════════════╝


**Module:** `agents::tool::run-pwsh`
**Trigger kind:** `tool_failed`

## Message

```
exit=1
--- stdout ---

--- stderr ---
npm warn exec The following package was not found and will be installed: playwright@1.60.0
Error: command.parse: Executable doesn't exist at C:\Users\User\AppData\Local\ms-playwright\chromium-1223\chrome-win64\chrome.exe
╔════════════════════════════════════════════════════════════╗
║ Looks like Playwright was just installed or updated.       ║
║ Please run the following command to download new browsers: ║
║                                                            ║
║     npx playwright install                                 ║
║                                                            ║
║ <3 Playwright Team                                         ║
╚════════════════════════════════════════════════════════════╝

```

## Traceback

```
host_os=windows
host_family=windows
depth=0
args=run-pwsh 'npx playwright open https://walla.co.il'
```

## Proposed fix

Sub-agent tool `run-pwsh` returned an error: exit=1
--- stdout ---

--- stderr ---
npm warn exec The following package was not found and will be installed: playwright@1.60.0
Error: command.parse: Executable doesn't exist at C:\Users\User\AppData\Local\ms-playwright\chromium-1223\chrom…. Investigate the args and retry; consider whether a different skill is a better fit for this environment.
