---
name: read-file
description: Read a UTF-8 file from disk and return its contents to the agent.
---
Read a file from disk.

Args (JSON):

```
{ "path": "crates/backend/src/main.rs" }
```

Behaviour:
- Relative paths resolve against the workspace root.
- Relative paths may not escape the workspace via `..`.
- Files larger than **1 MiB** are rejected.
- File contents are returned as the skill outcome `summary`.
