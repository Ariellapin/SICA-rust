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
- Relative paths resolve against the workspace root.
- Relative paths may not escape the workspace via `..`.
- Files larger than **1 MiB** are rejected.
- `start` / `end` are named args — use the JSON-fenced tool_call form to
  pass them. Out-of-range bounds clamp; the output carries a
  `[lines a-b of N]` header when a range was requested.
- The raw contents are summarised by the sub-agent against the expectation
  text after `>` before being returned to the main agent.
