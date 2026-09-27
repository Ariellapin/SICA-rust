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

One-line form (escape newlines in the content as `\n`):

    write-file 'notes/x.md' 'hello\nworld\n' > confirm bytes written

Behaviour:
- Parent directories are created automatically.
- Relative paths may not escape the workspace via `..`.
- Returns the number of bytes written in the outcome summary.
- **Large files: write them in parts.** One reply can only hold so much,
  and a call cut off mid-body never runs. Write the first part, then add
  each further part with `'append=true'`:

      write-file 'out.txt' '<first 20 lines>' > confirm the first part
      write-file 'out.txt' '<next 20 lines>' 'append=true' > confirm the second part
