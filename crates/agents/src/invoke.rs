//! `/name` — user-typed skill invocation.
//!
//! The chat palette (`slash_menu.rs`) rewrites a pick into `/name args…`,
//! and a user can type the token by hand. Either way the sent text starts
//! with a whitespace-bounded `/name`. [`expand`] resolves that name to a
//! markdown file and returns the framed content the backend injects as
//! `ContextInjected { source: SkillInvocation }` *before* the user's
//! message, so the model reads the instructions and then the request.
//!
//! Resolution order, first hit wins:
//!
//! 1. `commands/<name>.md` — a canned prompt; `{{args}}` in the body is
//!    replaced with the rest of the line.
//! 2. `agents/<name>.md` — a persona.
//! 3. `skills/<name>.md` — a skill contract (user-authored or the seeded
//!    doc of a Rust built-in, so `/run-cli …` loads the `run-cli` contract).
//!
//! Nothing here executes a skill — that stays the model's decision after it
//! has read the contract. Names are restricted to `[A-Za-z0-9_-]` so a
//! typed token can never reach outside the three directories.

use std::path::{Path, PathBuf};

use crate::md_skill::{self, MarkdownSkill};

/// Where the three families live. Built from `sica_core::paths` in
/// production; tests point it at a temp dir.
#[derive(Debug, Clone)]
pub struct Roots {
    pub commands: PathBuf,
    pub agents: PathBuf,
    pub skills: PathBuf,
}

impl Roots {
    pub fn from_workspace() -> Self {
        Self {
            commands: sica_core::paths::commands_dir(),
            agents: sica_core::paths::agents_dir(),
            skills: sica_core::paths::skills_dir(),
        }
    }
}

/// Which family answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Command,
    Agent,
    Skill,
}

/// The result of resolving a `/name` token.
#[derive(Debug, Clone)]
pub struct Expansion {
    pub name: String,
    pub family: Family,
    /// The `<skill_content>` frame to inject.
    pub content: String,
    /// Everything after the token, trimmed.
    pub args: String,
}

/// The leading `/name` token of `text`, with the rest of the line, or
/// `None` when the text does not start with one. A bare `/` or a name with
/// characters outside `[A-Za-z0-9_-]` is not a token — `/etc/passwd` and
/// `/../x` stay ordinary text.
pub fn parse_token(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix('/')?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..end];
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return None;
    }
    Some((name, rest[end..].trim()))
}

/// Resolve the leading `/name` in `text` against `roots`. `None` when there
/// is no token or nothing on disk answers to it — the text is then sent as
/// typed, which is also what happens for the frontend's own app commands
/// (`/new`, `/stop`…) should one ever reach the backend.
pub fn expand(text: &str, roots: &Roots) -> Option<Expansion> {
    let (name, args) = parse_token(text)?;
    let candidates = [
        (Family::Command, roots.commands.join(format!("{name}.md"))),
        (Family::Agent, roots.agents.join(format!("{name}.md"))),
        (Family::Skill, roots.skills.join(format!("{name}.md"))),
    ];
    for (family, path) in candidates {
        let Ok(skill) = load(&path) else { continue };
        let content = match family {
            Family::Command => {
                let body = skill.body.replace("{{args}}", args);
                MarkdownSkill { body, ..skill }.render_skill_content()
            }
            _ => skill.render_skill_content(),
        };
        return Some(Expansion { name: name.to_string(), family, content, args: args.to_string() });
    }
    None
}

fn load(path: &Path) -> Result<MarkdownSkill, String> {
    if !path.is_file() {
        return Err("missing".into());
    }
    md_skill::load_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> (PathBuf, Roots) {
        let tmp = std::env::temp_dir().join(format!(
            "sica-invoke-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let r = Roots {
            commands: tmp.join("commands"),
            agents: tmp.join("agents"),
            skills: tmp.join("skills"),
        };
        for d in [&r.commands, &r.agents, &r.skills] {
            std::fs::create_dir_all(d).unwrap();
        }
        (tmp, r)
    }

    #[test]
    fn token_is_whitespace_bounded_and_name_restricted() {
        assert_eq!(parse_token("/standup since monday"), Some(("standup", "since monday")));
        assert_eq!(parse_token("/standup"), Some(("standup", "")));
        assert_eq!(parse_token("/read-file\nfoo"), Some(("read-file", "foo")));
        assert_eq!(parse_token("hello /x"), None);
        assert_eq!(parse_token("/"), None);
        assert_eq!(parse_token("/etc/passwd"), None);
        assert_eq!(parse_token("/../x"), None);
        assert_eq!(parse_token("/a.b"), None);
    }

    #[test]
    fn commands_win_and_substitute_args() {
        let (tmp, r) = roots();
        std::fs::write(r.commands.join("standup.md"), "---\nname: standup\n---\nWrite a standup since {{args}}.\n").unwrap();
        std::fs::write(r.skills.join("standup.md"), "---\nname: standup\n---\nSKILL\n").unwrap();
        let e = expand("/standup monday", &r).unwrap();
        assert_eq!(e.family, Family::Command);
        assert_eq!(e.args, "monday");
        assert!(e.content.contains("Write a standup since monday."), "{}", e.content);
        assert!(e.content.starts_with("<skill_content name=\"standup\">"));
        assert!(e.content.contains(&format!("Base directory for this skill: {}", r.commands.display())));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn falls_through_agents_to_skills_and_unknown_is_none() {
        let (tmp, r) = roots();
        std::fs::write(r.agents.join("reviewer.md"), "---\nname: reviewer\n---\nYou review.\n").unwrap();
        std::fs::write(r.skills.join("run-cli.md"), "---\nname: run-cli\n---\nContract.\n").unwrap();
        assert_eq!(expand("/reviewer", &r).unwrap().family, Family::Agent);
        let s = expand("/run-cli cargo build", &r).unwrap();
        assert_eq!(s.family, Family::Skill);
        assert_eq!(s.args, "cargo build");
        assert!(s.content.contains("Contract."));
        assert!(expand("/nope", &r).is_none());
        assert!(expand("plain text", &r).is_none());
        // A malformed file is skipped like a missing one.
        std::fs::write(r.skills.join("bad.md"), "no frontmatter").unwrap();
        assert!(expand("/bad", &r).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
