---
name: playwright
description: Browser automation via the Playwright CLI. Use when a task needs to drive, scrape, or screenshot a web page.
---
Playwright is the host's browser-automation tool. The agent drives it by
running `npx playwright …` commands through the `run-pwsh` skill (Windows
default) or `run-cli`. Playwright bundles its own Chromium / Firefox /
WebKit binaries — once installed, no system browser is needed.

## When to use this skill

- Render a JS-heavy page that `Invoke-WebRequest` / curl can't see.
- Take a screenshot or PDF of a URL for inspection.
- Run an end-to-end test suite (`*.spec.ts`, `playwright.config.ts`).
- Record a script by interacting with a real browser (`codegen`).
- Inspect or debug an existing trace file.

For a single HTTP GET, prefer `Invoke-RestMethod` (see the `powershell`
skill) — it's an order of magnitude cheaper.

## One-time setup

Browser binaries must be downloaded once per machine. Check first:

```powershell
npx playwright --version           # confirms the CLI resolves
Test-Path "$env:LOCALAPPDATA\ms-playwright"   # cached browsers location
```

Install if missing:

```powershell
npx playwright install              # all default browsers
npx playwright install chromium     # just one (smaller, faster)
npx playwright install --with-deps  # Linux only — skip on Windows
```

Inside a Node project that already lists `@playwright/test` as a dep,
`npm install` will pull the CLI automatically. Outside a project, prefix
every call with `npx` so npm fetches it on demand.

## Core commands

| Goal | Command |
| --- | --- |
| Print version | `npx playwright --version` |
| Install browsers | `npx playwright install [chromium|firefox|webkit]` |
| Screenshot URL | `npx playwright screenshot <url> out.png` |
| PDF a URL (Chromium) | `npx playwright pdf <url> out.pdf` |
| Open URL in inspector | `npx playwright open <url>` |
| Record a script | `npx playwright codegen <url>` |
| Show a trace | `npx playwright show-trace trace.zip` |
| Show last test report | `npx playwright show-report` |

Useful flags on `screenshot` / `pdf` / `open`:

- `--browser=chromium|firefox|webkit` (default chromium)
- `--device="iPhone 13"` to emulate
- `--viewport-size=1280,720`
- `--full-page` (screenshot only)
- `--wait-for-selector=<sel>` / `--wait-for-timeout=<ms>`
- `--user-agent="<ua>"`
- `--load-storage=state.json` to reuse a logged-in session
- `--save-storage=state.json` after `codegen` to capture login

Examples:

```powershell
# Full-page screenshot of a dashboard, mobile viewport, with auth.
npx playwright screenshot --full-page --device="iPhone 13" `
  --load-storage=auth.json https://app.example.com/dash dash.png

# Render a JS app and save the final HTML for inspection.
npx playwright open --save-har=trace.har --save-har-glob='**/*.json' https://example.com
```

## Running a Playwright test suite

Inside a project that has `playwright.config.ts`:

| Task | Command |
| --- | --- |
| Run all tests | `npx playwright test` |
| Run one file | `npx playwright test tests/login.spec.ts` |
| Run one test by title | `npx playwright test -g "logs in"` |
| One browser only | `npx playwright test --project=chromium` |
| Headed (visible) | `npx playwright test --headed` |
| Debug step-by-step | `npx playwright test --debug` |
| Record trace on failure | `npx playwright test --trace=retain-on-failure` |
| Update snapshots | `npx playwright test --update-snapshots` |
| List discovered tests | `npx playwright test --list` |
| Open HTML report | `npx playwright show-report` |

`--debug` and `--headed` open a window — fine locally, but they keep the
process alive past the 30 s `run-pwsh` cap and will be killed. Use
`--reporter=line` or `--reporter=dot` for compact non-interactive output.

## Capturing output the agent can read

The 30 s timeout and 32 KiB stdout/stderr caps in `run-pwsh` matter. To keep
output focused:

- Use `--reporter=line` (one line per test) instead of the default `list`.
- Pipe to a file for richer data: `npx playwright test --reporter=json | Out-File -Encoding utf8 results.json`, then read with the `read-file` skill.
- For screenshots, write the file then have the agent open the path — don't
  echo binary content through the pipe.

## Codegen recipe

To author a script by clicking through a flow:

```powershell
npx playwright codegen --save-storage=auth.json https://app.example.com
```

The inspector window records every interaction as TypeScript / Python /
.NET. Copy the emitted snippet into a `*.spec.ts` and run it under
`npx playwright test`.

## Gotchas

- Don't run `npx playwright install` in parallel with another invocation —
  the lockfile in `%LOCALAPPDATA%\ms-playwright` will conflict.
- Headless Chromium can't print PDFs from URLs that require auth unless you
  preload state via `--load-storage`.
- The CLI exits 0 only when every test passed. A nonzero exit from
  `run-pwsh` with `playwright test` almost always means a real test failure
  — read the summary before retrying.
- WebKit on Windows is downloaded but driven through a packaged build; some
  font-rendering differences vs. Safari are expected.

## When NOT to reach for Playwright

- Plain JSON / HTML fetch → `Invoke-RestMethod` is faster.
- Static-site smoke check → curl or `Invoke-WebRequest -UseBasicParsing`.
- Unit testing JS logic → the project's existing test runner (vitest, jest)
  — Playwright is for the integrated browser, not pure JS.
