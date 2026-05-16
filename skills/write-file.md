---
name: write-file
description: Write UTF-8 content to a file. Creates parent dirs; supports append.
---
Write text to a file.

Args (JSON):

```
{
  "path":    "notes/scratch.md",   // required, relative to workspace root or absolute
  "content": "hello, world\n",     // required
  "append":  false                  // optional, default false (overwrites)
}
```

Behaviour:
- Parent directories are created automatically.
- Relative paths may not escape the workspace via `..`.
- Returns the number of bytes written in the outcome summary.
