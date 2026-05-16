//! `skill-creator` — a built-in skill the agent can call to author a new
//! markdown skill. The skill writes `<skills_dir>/<name>.md` with a YAML
//! frontmatter (`name`, `description`) and a body, then returns a short
//! confirmation. Errors (missing args, write failure, name collision)
//! are reported as a non-ok outcome — the runtime keeps running.

use std::fs;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::Value;

use crate::skill::{Skill, SkillContext, SkillOutcome};

pub const NAME: &str = "skill-creator";

pub const DESCRIPTION: &str =
    "Author a new markdown skill. Args: { name, description, body, overwrite? }. \
     Writes <skills_dir>/<name>.md and registers it on next backend start.";

/// Default body the seeded `skill-creator.md` carries. Kept in the same
/// file so the FE-visible "open skills folder" surface always describes
/// the same contract the loader enforces.
pub const SEED_MD: &str = r#"---
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
"#;

pub struct SkillCreator {
    pub skills_dir: PathBuf,
}

impl SkillCreator {
    pub fn new(skills_dir: PathBuf) -> Self {
        Self { skills_dir }
    }
}

#[async_trait]
impl Skill for SkillCreator {
    fn name(&self) -> &str { NAME }

    async fn run(&self, args: Value, _ctx: SkillContext) -> SkillOutcome {
        match write_skill(&self.skills_dir, &args) {
            Ok(path) => SkillOutcome {
                ok:      true,
                summary: format!("wrote {}", path.display()),
            },
            Err(e) => SkillOutcome { ok: false, summary: e },
        }
    }
}

fn write_skill(dir: &Path, args: &Value) -> Result<PathBuf, String> {
    let name = args.get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing `name` arg".to_string())?
        .trim()
        .to_string();
    if name.is_empty() {
        return Err("`name` must not be empty".into());
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(format!(
            "`name` must be ascii alphanumeric / `-` / `_`, got {name:?}"
        ));
    }
    let description = args.get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .replace('\n', " ");
    let body = args.get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let overwrite = args.get("overwrite")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    fs::create_dir_all(dir).map_err(|e| format!("create skills dir: {e}"))?;
    let path = dir.join(format!("{name}.md"));
    if path.exists() && !overwrite {
        return Err(format!(
            "{} already exists — pass `overwrite: true` to replace it",
            path.display()
        ));
    }

    let text = render(&name, &description, &body);
    fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

fn render(name: &str, description: &str, body: &str) -> String {
    let mut out = String::with_capacity(64 + body.len());
    out.push_str("---\n");
    out.push_str(&format!("name: {name}\n"));
    out.push_str(&format!("description: {description}\n"));
    out.push_str("---\n");
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Write `skills/skill-creator.md` if no file with that name exists yet.
/// Lets the user discover the contract by inspecting the folder on first
/// run; never clobbers a user-edited copy.
pub fn seed_default(skills_dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(skills_dir)?;
    let path = skills_dir.join("skill-creator.md");
    if !path.exists() {
        fs::write(&path, SEED_MD)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "skill-creator-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn writes_valid_md() {
        let dir = tempdir();
        let args = json!({
            "name": "alpha",
            "description": "first skill",
            "body": "do the thing\nnext line",
        });
        let path = write_skill(&dir, &args).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("---\nname: alpha\n"));
        assert!(text.contains("description: first skill"));
        assert!(text.contains("do the thing"));
    }

    #[test]
    fn rejects_overwrite_without_flag() {
        let dir = tempdir();
        let args = json!({ "name": "beta", "description": "", "body": "x" });
        write_skill(&dir, &args).unwrap();
        let err = write_skill(&dir, &args).unwrap_err();
        assert!(err.contains("already exists"));
    }

    #[test]
    fn rejects_bad_name() {
        let dir = tempdir();
        let args = json!({ "name": "../escape", "description": "", "body": "x" });
        let err = write_skill(&dir, &args).unwrap_err();
        assert!(err.contains("ascii"));
    }

    #[test]
    fn seed_creates_file_once() {
        let dir = tempdir();
        seed_default(&dir).unwrap();
        let path = dir.join("skill-creator.md");
        assert!(path.exists());
        // Tamper with the file; second call must NOT overwrite it.
        std::fs::write(&path, "user edits").unwrap();
        seed_default(&dir).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "user edits");
    }
}
