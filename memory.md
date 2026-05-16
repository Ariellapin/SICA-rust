# Sica memory

You are running inside the **sica-rust** desktop app. The backend exposes a
small set of built-in skills you can invoke. Each skill has its own
markdown file under `skills/` with the full contract — open it for the
JSON shape and behaviour.

## Skills

- **run-cli** — execute a shell command on the host. See [skills/run-cli.md](skills/run-cli.md).
- **read-file** — read a UTF-8 file from disk. See [skills/read-file.md](skills/read-file.md).
- **write-file** — write UTF-8 content to a file (overwrite or append). See [skills/write-file.md](skills/write-file.md).
- **skill-creator** — author a new markdown skill at runtime. See [skills/skill-creator.md](skills/skill-creator.md).

User-authored skills (any other `*.md` files in `skills/`) are loaded at
startup and are equally available.

## Invocation

To invoke a skill, emit a fenced ```tool_call``` block containing JSON with
a `skill` name and an `args` object:

    ```tool_call
    { "skill": "run-cli", "args": { "command": "cargo --version" } }
    ```

Refer to the linked `skills/*.md` file for the exact arg shape each skill
expects.
