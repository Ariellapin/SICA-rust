---
name: edit-file
description: Replace an exact block of text in a file. The block must match exactly once.
---
Replace one exact stretch of text in an existing file.

Invocation (JSON-fenced form preferred — the arguments contain newlines):

    ```tool_call
    { "skill": "edit-file", "args": { "path": "src/main.rs", "old": "let x = 1;", "new": "let x = 2;" }, "expectation": "confirm the replacement" }
    ```

Behaviour:
- `old` must match the file **exactly once**, byte for byte (indentation and
  newlines included). `read-file` first — its line-numbered output tells you
  the exact text.
- 0 matches or more than 1 match is an error; include more surrounding lines
  in `old` to make it unique.
- The result shows the changed region with line numbers so you can verify it.
