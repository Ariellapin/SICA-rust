//! Agent presets — `agents/*.md` as a session's persona (guide §5.2).
//!
//! `agents/` has always backed one of the three families of the frontend's
//! "/" palette, but picking a row only ever injected the file's body once,
//! as a `SkillInvocation` context block (`crate::invoke`). A preset gives
//! the same file *session-level* meaning:
//!
//! * its **body** becomes the [`order::PERSONA`](crate::prompt::order::PERSONA)
//!   section of the system prompt — ahead of `memory.md`, so "who is
//!   answering" frames the workspace's standing instructions rather than
//!   trailing them;
//! * its frontmatter **`skills:`** list restricts the registry view the
//!   session dispatches against ([`view`]), so a reviewer preset can be
//!   given read-only tools without changing the permission mode.
//!
//! ```text
//! ---
//! name: reviewer
//! description: Reads code and reports findings; never edits.
//! skills: [read-file, glob, grep]
//! ---
//! You are a code reviewer…
//! ```
//!
//! Selection is per session and durable (`EventKind::AgentPreset`), and dsh
//! fixes it once a session has produced anything — a prompt prefix that
//! changes mid-session throws away the provider's cache and leaves half the
//! transcript answering to rules that are no longer in force. The backend
//! enforces that; this module only loads and applies.

use std::path::{Path, PathBuf};

use crate::md_skill::{self, MarkdownSkill};
use crate::registry::SkillRegistry;

/// One parsed `agents/*.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPreset {
    pub name:        String,
    pub description: String,
    /// The file body — the `PERSONA` prompt section, verbatim. Interpolated
    /// at render time like every other section, so `{{cwd}}` works here too.
    pub persona:     String,
    /// Skill names from `skills:`. Empty means "every skill stays visible".
    pub skills:      Vec<String>,
    pub source_path: PathBuf,
}

impl From<MarkdownSkill> for AgentPreset {
    fn from(s: MarkdownSkill) -> Self {
        Self {
            name:        s.name,
            description: s.description,
            persona:     s.body,
            skills:      s.skills,
            source_path: s.source_path,
        }
    }
}

/// Load every preset in `dir`, name-sorted, plus the per-file parse errors
/// the caller should surface. A missing directory contributes nothing —
/// `agents/` is created empty at startup and stays that way until a user
/// writes a file into it.
pub fn load_dir(dir: &Path) -> (Vec<AgentPreset>, Vec<(PathBuf, String)>) {
    let report = md_skill::load_dir(dir);
    let mut loaded: Vec<AgentPreset> = report.loaded.into_iter().map(AgentPreset::from).collect();
    loaded.sort_by(|a, b| a.name.cmp(&b.name));
    (loaded, report.errors)
}

/// Load the preset named `name` from `dir`. `Err` names what went wrong so
/// the backend can refuse the selection with a reason the user can act on
/// (the file is gone, the frontmatter is broken) rather than silently
/// running with no persona.
///
/// The name is restricted to `[A-Za-z0-9_-]`, exactly as a typed `/name`
/// token is: a preset name arrives over the wire and is joined onto a path,
/// so `../` must never round-trip into one.
pub fn load(dir: &Path, name: &str) -> Result<AgentPreset, String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(format!("`{name}` is not a valid agent name"));
    }
    let path = dir.join(format!("{name}.md"));
    if !path.is_file() {
        return Err(format!("no agent `{name}` in {}", dir.display()));
    }
    let skill = md_skill::load_file(&path)?;
    // The frontmatter `name:` is what the palette and `/name` address, so a
    // file whose name disagrees with its own frontmatter is ambiguous.
    if skill.name != name {
        return Err(format!(
            "{}: frontmatter name `{}` does not match the file name",
            path.display(),
            skill.name
        ));
    }
    Ok(AgentPreset::from(skill))
}

/// Skills a preset can never hide. These are the harness's own control
/// plane, not capabilities the persona is choosing between: without
/// `exit-plan-mode` a planning session cannot leave plan mode, without
/// `ask-user` the approval and question brokers have no route back to the
/// human, and without the goal skills a running goal cannot be updated.
/// A preset that lists none of them still gets them.
fn always_visible(name: &str) -> bool {
    crate::control::is_control_skill(name) || name == crate::control::ASK_USER_NAME
}

/// The registry view a session running `preset` dispatches against: the
/// listed skills plus [`always_visible`] ones. An empty `skills:` list is
/// no restriction at all, so the registry is returned unchanged.
///
/// Names that match nothing are *not* an error here — [`unknown_skills`]
/// reports them so the backend can log them once at selection time instead
/// of failing every turn.
pub fn view(registry: &SkillRegistry, preset: &AgentPreset) -> SkillRegistry {
    if preset.skills.is_empty() {
        return registry.clone();
    }
    let keep: Vec<&str> = preset
        .skills
        .iter()
        .map(String::as_str)
        .chain(registry.by_name.keys().map(String::as_str).filter(|n| always_visible(n)))
        .collect();
    registry.restricted_to(&keep)
}

/// Names in `preset.skills` that no skill answers to — a typo in the
/// frontmatter, or a tool from another machine's configuration.
pub fn unknown_skills(registry: &SkillRegistry, preset: &AgentPreset) -> Vec<String> {
    preset
        .skills
        .iter()
        .filter(|n| !registry.by_name.contains_key(*n))
        .cloned()
        .collect()
}

/// The preset seeded into `agents/` on first run, so the family the palette
/// lists is not empty and the frontmatter contract is discoverable from a
/// working example. Seeded once, never overwritten — a user edit of this
/// file survives every restart.
const REVIEWER_MD: &str = "\
---
name: reviewer
description: Reads the code and reports findings; never edits or runs anything.
skills: [read-file, glob, grep]
---
You are a code reviewer working in {{cwd}}.

Read the code before judging it, and base every finding on something you \
actually read — quote the file and line. You have no tools for editing \
files or running commands, so propose changes as diffs in your reply \
rather than applying them.

Report findings most-severe first. Say plainly when you found nothing \
worth reporting.
";

/// Write `agents/reviewer.md` if it is absent. Same contract as the skill
/// and plan-mode seeds: create once, never clobber.
pub fn seed_defaults(agents_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(agents_dir)?;
    let path = agents_dir.join("reviewer.md");
    if path.exists() {
        return Ok(());
    }
    std::fs::write(path, REVIEWER_MD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::{Skill, SkillContext, SkillOutcome};
    use async_trait::async_trait;
    use serde_json::Value;
    use std::sync::Arc;

    struct Named(&'static str);
    #[async_trait]
    impl Skill for Named {
        fn name(&self) -> &str {
            self.0
        }
        async fn run(&self, _a: Value, _c: SkillContext) -> SkillOutcome {
            SkillOutcome { ok: true, summary: String::new() }
        }
    }

    fn registry() -> SkillRegistry {
        let mut r = SkillRegistry::new();
        for n in ["read-file", "write-file", "grep", "run-cli", "ask-user", "exit-plan-mode"] {
            r.register(Arc::new(Named(n)));
        }
        r
    }

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sica-preset-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_reads_body_and_skill_list() {
        let dir = tmp("load");
        std::fs::write(
            dir.join("reviewer.md"),
            "---\nname: reviewer\ndescription: d\nskills: [read-file, grep]\n---\nBODY\n",
        )
        .unwrap();
        let p = load(&dir, "reviewer").unwrap();
        assert_eq!(p.persona, "BODY");
        assert_eq!(p.skills, vec!["read-file".to_string(), "grep".into()]);
        assert_eq!(p.description, "d");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_refuses_traversal_and_missing_files() {
        let dir = tmp("refuse");
        assert!(load(&dir, "../secrets").unwrap_err().contains("not a valid"));
        assert!(load(&dir, "").unwrap_err().contains("not a valid"));
        assert!(load(&dir, "nope").unwrap_err().contains("no agent"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_refuses_a_frontmatter_name_mismatch() {
        let dir = tmp("mismatch");
        std::fs::write(dir.join("a.md"), "---\nname: b\n---\nbody\n").unwrap();
        assert!(load(&dir, "a").unwrap_err().contains("does not match"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn view_keeps_listed_skills_plus_the_control_plane() {
        let reg = registry();
        let p = AgentPreset {
            name:        "reviewer".into(),
            description: String::new(),
            persona:     "P".into(),
            skills:      vec!["read-file".into(), "grep".into()],
            source_path: PathBuf::new(),
        };
        let v = view(&reg, &p);
        let mut names: Vec<&str> = v.by_name.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["ask-user", "exit-plan-mode", "grep", "read-file"]);
        assert!(unknown_skills(&reg, &p).is_empty());
    }

    #[test]
    fn an_empty_list_restricts_nothing() {
        let reg = registry();
        let p = AgentPreset {
            name:        "plain".into(),
            description: String::new(),
            persona:     "P".into(),
            skills:      Vec::new(),
            source_path: PathBuf::new(),
        };
        assert_eq!(view(&reg, &p).by_name.len(), reg.by_name.len());
    }

    #[test]
    fn unknown_names_are_reported_not_fatal() {
        let reg = registry();
        let p = AgentPreset {
            name:        "typo".into(),
            description: String::new(),
            persona:     "P".into(),
            skills:      vec!["read-file".into(), "raed-file".into()],
            source_path: PathBuf::new(),
        };
        assert_eq!(unknown_skills(&reg, &p), vec!["raed-file".to_string()]);
        assert!(view(&reg, &p).by_name.contains_key("read-file"));
    }

    #[test]
    fn seed_writes_once_and_never_clobbers() {
        let dir = tmp("seed");
        seed_defaults(&dir).unwrap();
        let path = dir.join("reviewer.md");
        std::fs::write(&path, "---\nname: reviewer\n---\nMINE\n").unwrap();
        seed_defaults(&dir).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("MINE"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_seeded_preset_parses_and_names_real_skills() {
        let dir = tmp("seed-parse");
        seed_defaults(&dir).unwrap();
        let p = load(&dir, "reviewer").unwrap();
        assert_eq!(p.skills, vec!["read-file".to_string(), "glob".into(), "grep".into()]);
        assert!(p.persona.contains("{{cwd}}"), "personas interpolate");
        let (all, errors) = load_dir(&dir);
        assert!(errors.is_empty());
        assert_eq!(all.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
