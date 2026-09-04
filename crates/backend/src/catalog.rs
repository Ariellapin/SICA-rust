//! Builds the catalogue the frontend's "/" palette lists.
//!
//! Three families, one flat list (the FE groups them by [`CatalogKind`]):
//!
//! * **Skills**   — every entry in the live `SkillRegistry`: the Rust
//!   built-ins plus each `skills/*.md` loaded at startup. This is the
//!   authoritative list, which is why it comes from the registry rather than
//!   from a second directory scan — a markdown file whose body documents a
//!   built-in must not appear twice.
//! * **Agents**   — `agents/*.md`, parsed with the same frontmatter reader.
//! * **Commands** — `commands/*.md`, likewise.
//!
//! Each family is name-sorted so the palette order is stable between calls.
//! Missing directories are not an error: they simply contribute nothing.

use std::path::Path;

use agents::SkillRegistry;
use protocol::{CatalogEntry, CatalogKind};
use sica_core::paths::{agents_dir, commands_dir, skills_dir};

/// Catalogue for the running workspace. Resolves the three directories from
/// `sica_core::paths` and delegates to [`build`].
pub fn build_from_workspace(skills: &SkillRegistry) -> Vec<CatalogEntry> {
    build(skills, &skills_dir(), &agents_dir(), &commands_dir())
}

pub fn build(skills: &SkillRegistry, skills_dir: &Path, agents_dir: &Path, commands_dir: &Path) -> Vec<CatalogEntry> {
    let mut out = Vec::new();

    let mut names: Vec<&str> = skills.by_name.keys().map(String::as_str).collect();
    names.sort_unstable();
    for name in names {
        let Some(skill) = skills.by_name.get(name) else { continue };
        // The registry hands out `Arc<dyn Skill>`, which carries no source
        // path, so the contract file is looked up by convention. Built-ins
        // seed one at startup; a user-authored skill *is* one.
        let doc = skills_dir.join(format!("{name}.md"));
        out.push(CatalogEntry {
            kind:        CatalogKind::Skill,
            name:        skill.name().to_string(),
            description: skill.description().to_string(),
            args:        skill.positional_args(),
            source:      doc.exists().then(|| doc.display().to_string()),
        });
    }

    out.extend(from_dir(agents_dir, CatalogKind::Agent));
    out.extend(from_dir(commands_dir, CatalogKind::Command));
    out
}

/// Parse one markdown directory into name-sorted catalogue entries. Files with
/// malformed frontmatter are skipped — `load_dir` already reports them, and the
/// startup scan surfaces the same errors as `LogLine` events.
fn from_dir(dir: &Path, kind: CatalogKind) -> Vec<CatalogEntry> {
    let mut loaded = agents::md_skill::load_dir(dir).loaded;
    loaded.sort_by(|a, b| a.name.cmp(&b.name));
    loaded
        .into_iter()
        .map(|s| CatalogEntry {
            kind,
            name:        s.name,
            description: s.description,
            args:        s.positionals,
            source:      Some(s.source_path.display().to_string()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn skills_come_from_the_registry_and_are_sorted() {
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(agents::RunPwsh));
        reg.register(Arc::new(agents::RunCli));
        let missing = Path::new("no-such-dir");
        let entries = build(&reg, missing, missing, missing);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["run-cli", "run-pwsh"]);
        assert!(entries.iter().all(|e| e.kind == CatalogKind::Skill));
        // No `skills/<name>.md` next to a bogus dir — source stays empty.
        assert!(entries.iter().all(|e| e.source.is_none()));
    }

    #[test]
    fn markdown_dirs_contribute_agents_and_commands() {
        let tmp = std::env::temp_dir().join(format!("sica-catalog-{}", std::process::id()));
        let agents_dir = tmp.join("agents");
        let commands_dir = tmp.join("commands");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::create_dir_all(&commands_dir).unwrap();
        std::fs::write(
            agents_dir.join("reviewer.md"),
            "---\nname: reviewer\ndescription: reviews code\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            commands_dir.join("standup.md"),
            "---\nname: standup\ndescription: writes a standup\npositional: since\n---\nbody\n",
        )
        .unwrap();

        let reg = SkillRegistry::new();
        let entries = build(&reg, Path::new("no-such-dir"), &agents_dir, &commands_dir);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, CatalogKind::Agent);
        assert_eq!(entries[0].name, "reviewer");
        assert_eq!(entries[1].kind, CatalogKind::Command);
        assert_eq!(entries[1].args, vec!["since".to_string()]);
        assert!(entries[1].source.as_deref().unwrap().ends_with("standup.md"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
