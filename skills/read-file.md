---
name: read-file
description: Read a UTF-8 file from disk with line numbers, optionally a line range.
---
Read a file from disk. The output is line-numbered (`  <n>\t<line>`) so you
can quote exact lines to `edit-file`.

Args (JSON):

```
{
  "path":  "crates/backend/src/main.rs",  // required
  "start": "1",                            // optional, 1-based first line
  "end":   "80"                            // optional, 1-based last line
}
```

Natural-language form reads the whole file:

    read-file 'skills/run-cli.md' > what positional args does run-cli accept

Behaviour:
- Relative paths resolve against the working directory.
- A `skills/<name>.md` the working directory does not have is read from the
  app's own `skills/` folder, so a skill's contract opens from any project.
- Relative paths may not escape the workspace via `..`.
- Files larger than **1 MiB** are rejected.
- `start` / `end` are named args. In the one-line form put them after the
  path: `read-file 'wk.txt' 'start=40' 'end=80' > lines 40-80`. Out-of-range
  bounds clamp; the output carries a `[lines a-b of N]` header when a range
  was requested.
- One call returns at most **48 KB** of numbered lines. A longer read stops
  at the last whole line that fits and ends with a
  `[read-file: output capped …]` line naming the `start` / `end` to read on.
- The path is taken exactly as written — backslashes included.
- The raw contents are summarised by the sub-agent against the expectation
  text after `>` before being returned to the main agent.
