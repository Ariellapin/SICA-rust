---
name: grep
description: Regex search across files; returns path:line: text rows.
---
Search file contents with a regular expression. Honours `.gitignore`.

Invocation (single line):

    grep '<regex>' '<path>' > <what you want to find>

Examples:

    grep 'fn main' '.' > where is main defined
    grep 'TODO|FIXME' 'src' > outstanding work markers

Behaviour:
- `path` is a file or a directory (searched recursively).
- Output rows are `path:line: text`; at most **250** matches are returned.
- Non-UTF-8 (binary) files are skipped silently.
- Uses Rust `regex` syntax — no backreferences, no look-around.
