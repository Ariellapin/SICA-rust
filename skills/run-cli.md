---
name: run-cli
description: Execute a shell command on the host (cmd.exe on Windows, /bin/sh elsewhere).
---
Run a shell command on the host. stdout and stderr are captured and returned
to the agent in the outcome `summary`.

Args (JSON):

```
{
  "command": "git status",   // required, the shell command line
  "cwd":     "."             // optional, working dir (relative to workspace root or absolute)
}
```

Behaviour:
- Windows: invokes `cmd /C <command>`. Other OSes: `/bin/sh -c <command>`.
- Stdout and stderr are each capped to **32 KiB** before being returned.
- A timeout of **30 seconds** kills the child and reports an error outcome.
- The outcome `ok` mirrors the child exit code (0 = ok).

Use this for build tools, git, package managers, or one-shot scripts.
