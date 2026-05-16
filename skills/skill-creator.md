---
name: skill-creator
description: Author a new markdown skill in the skills/ folder.
---
You are the **skill-creator** tool. When the agent calls you, it must pass
JSON with the following shape:

```
{
  "name":        "my-skill",       // required, becomes the filename stem
  "description": "one-line summary",
  "body":        "multi-line skill instructions",
  "overwrite":   false              // optional, default false
}
```

You write `skills/<name>.md` with a YAML frontmatter block (`name`,
`description`) followed by the supplied body. The file becomes a live
skill on the next backend restart. Refuse to overwrite an existing file
unless `overwrite: true` is set.

To author further skills by hand, drop another `*.md` file with the same
frontmatter shape into this folder.
