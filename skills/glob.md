---
name: glob
description: List files matching a glob pattern, most recently modified first.
---
Find files by glob pattern. Honours `.gitignore`.

Invocation (single line):

    glob '<pattern>' > <what you want to find>

Examples:

    glob 'src/**/*.rs' > every Rust source file
    glob '**/*.toml' > all TOML files

Behaviour:
- `**` matches any number of directories; `*` matches within one path
  segment. Patterns are relative to the workspace root.
- Returns at most **100** paths, most recently modified first, one per line.
- Use `grep` to search file *contents*.
