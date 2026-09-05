//! `/name` — user-typed invocation of the three markdown families.
//!
//! The chat palette (`slash_menu.rs`) rewrites a pick into `/name args…`,
//! and a user can type the token by hand. Either way the sent text starts
//! with a whitespace-bounded `/name`. [`resolve`] maps that name to a file
//! and says what the backend should *do* with it.
//!
//! Resolution order, first hit wins:
//!
//! 1. `commands/<name>.md` — a canned prompt. Injected as
//!    `ContextInjected { source: SkillInvocation }` before the user's
//!    message, with `{{args}}` replaced by the rest of the line.
//! 2. `agents/<name>.md` — a **persona**, and a persona is session state,
//!    not one-shot context: this resolves to [`Invocation::Agent`], which
//!    the backend answers by *selecting* the preset (guide §5.2), exactly
//!    as the palette's AGENTS row and `/agent <name>` do. One file family,
//!    one meaning — otherwise the same file would mean two things
//!    depending on which route reached it.
//! 3. `skills/<name>.md` — a skill contract (user-authored or the seeded
//!    doc of a Rust built-in, so `/run-cli …` loads the `run-cli`
//!    contract). Injected like a command, minus the `{{args}}` rewrite.
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

/// Which *injecting* family answered. Agents are not here: they select
/// rather than inject (see [`Invocation::Agent`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Command,
    Skill,
}

/// What the backend should do about a typed `/name`.
#[derive(Debug, Clone)]
pub enum Invocation {
    /// Inject this body as context ahead of the user's message.
    Context(Expansion),
    /// Select this agent preset for the session. The user's message still
    /// goes out as typed; only the persona and the registry view change.
    Agent { name: String },
}

/// The result of resolving a `/name` token to an injecting family.
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
pub fn resolve(text: &str, roots: &Roots) -> Option<Invocation> {
    let (name, args) = parse_token(text)?;
    // Commands still shadow an `agents/<name>.md` of the same name: the
    // precedence has not changed, only what an agent hit *means*.
    if let Ok(skill) = load(&roots.commands.join(format!("{name}.md"))) {
        let body = skill.body.replace("{{args}}", args);
        return Some(Invocation::Context(Expansion {
            name:    name.to_string(),
            family:  Family::Command,
            content: MarkdownSkill { body, ..skill }.render_skill_content(),
            args:    args.to_string(),
        }));
    }
    // A *readable* agent file is a selection. A malformed one falls through
    // to `skills/` exactly as a malformed command does, rather than becoming
    // a selection the backend would only fail to load a moment later.
    if load(&roots.agents.join(format!("{name}.md"))).is_ok() {
        return Some(Invocation::Agent { name: name.to_string() });
    }
    if let Ok(skill) = load(&roots.skills.join(format!("{name}.md"))) {
        return Some(Invocation::Context(Expansion {
            name:    name.to_string(),
            family:  Family::Skill,
            content: skill.render_skill_content(),
            args:    args.to_string(),
        }));
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
        let Some(Invocation::Context(e)) = resolve("/standup monday", &r) else {
            panic!("a command injects")
        };
        assert_eq!(e.family, Family::Command);
        assert_eq!(e.args, "monday");
        assert!(e.content.contains("Write a standup since monday."), "{}", e.content);
        assert!(e.content.starts_with("<skill_content name=\"standup\">"));
        assert!(e.content.contains(&format!("Base directory for this skill: {}", r.commands.display())));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// An agent token *selects*; it never injects. Same file, same effect
    /// as the palette's AGENTS row and as `/agent <name>` (guide §5.2).
    #[test]
    fn an_agent_token_selects_rather_than_injecting() {
        let (tmp, r) = roots();
        std::fs::write(r.agents.join("reviewer.md"), "---\nname: reviewer\n---\nYou review.\n").unwrap();
        match resolve("/reviewer look at chat.rs", &r) {
            Some(Invocation::Agent { name }) => assert_eq!(name, "reviewer"),
            other => panic!("expected a selection, got {other:?}"),
        }
        // A command of the same name still wins, and still injects.
        std::fs::write(r.commands.join("reviewer.md"), "---\nname: reviewer\n---\nCMD\n").unwrap();
        let Some(Invocation::Context(e)) = resolve("/reviewer", &r) else {
            panic!("a command shadows the agent")
        };
        assert_eq!(e.family, Family::Command);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn falls_through_to_skills_and_unknown_is_none() {
        let (tmp, r) = roots();
        std::fs::write(r.skills.join("run-cli.md"), "---\nname: run-cli\n---\nContract.\n").unwrap();
        let Some(Invocation::Context(s)) = resolve("/run-cli cargo build", &r) else {
            panic!("a skill injects")
        };
        assert_eq!(s.family, Family::Skill);
        assert_eq!(s.args, "cargo build");
        assert!(s.content.contains("Contract."));
        assert!(resolve("/nope", &r).is_none());
        assert!(resolve("plain text", &r).is_none());
        // A malformed file is skipped like a missing one — including a
        // malformed *agent*, which must not become a selection.
        std::fs::write(r.skills.join("bad.md"), "no frontmatter").unwrap();
        std::fs::write(r.agents.join("broken.md"), "no frontmatter").unwrap();
        assert!(resolve("/bad", &r).is_none());
        assert!(resolve("/broken", &r).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
