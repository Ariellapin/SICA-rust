//! `idealist_workspace/lessons.md` — one line per diagnosed ticket whose
//! investigator wrote a lesson the model should know next time.
//!
//! Only `model_mistake` and `environment` findings become lessons: those
//! are the ones the *model* can act on. A harness bug is fixed in code, not
//! by telling the model about it.
//!
//! Keyed by ticket id, so a re-investigation replaces its line instead of
//! adding a second one. The prompt only ever sees the newest few
//! ([`newest`]), and only when `lessons_in_prompt` is on.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;

static WRITE: Mutex<()> = Mutex::new(());

/// Lessons the prompt carries at most.
pub const PROMPT_LESSONS: usize = 10;
/// Characters one lesson keeps.
const LESSON_CAP: usize = 240;

pub fn path() -> PathBuf {
    sica_core::paths::idealist_workspace().join("lessons.md")
}

/// Categories whose lesson is about the model's behaviour or the machine
/// it runs on, as opposed to a bug in the harness.
pub fn category_teaches(category: &str) -> bool {
    matches!(category, "model_mistake" | "environment")
}

const HEADER: &str = "# Lessons from investigated tickets\n\n\
One line per ticket, newest last. Written by the idealist investigator; \
delete a line to retire it.\n\n";

fn line_id(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("- [")?;
    Some(&rest[..rest.find(']')?])
}

fn clean(lesson: &str) -> String {
    // The prompt interpolates `{{name}}` strictly; a lesson quoting one
    // must not be able to break prompt assembly for every later turn.
    let flat: String = lesson
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("{{", "{ {")
        .replace("}}", "} }");
    if flat.len() <= LESSON_CAP {
        return flat;
    }
    let mut end = LESSON_CAP;
    while !flat.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &flat[..end])
}

/// Add or replace the lesson for `ticket_id`.
pub fn record(file: &Path, ticket_id: &str, lesson: &str) -> Result<()> {
    let lesson = clean(lesson);
    if lesson.is_empty() {
        return Ok(());
    }
    let _g = WRITE.lock().unwrap_or_else(|p| p.into_inner());
    let existing = fs::read_to_string(file).unwrap_or_default();
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| l.starts_with("- ["))
        .filter(|l| line_id(l) != Some(ticket_id))
        .map(str::to_string)
        .collect();
    lines.push(format!("- [{ticket_id}] {lesson}"));
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(file, format!("{HEADER}{}\n", lines.join("\n")))?;
    Ok(())
}

/// The newest `n` lessons, oldest of them first, without their ids.
pub fn newest(file: &Path, n: usize) -> Vec<String> {
    let text = fs::read_to_string(file).unwrap_or_default();
    let all: Vec<String> = text
        .lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("- [")?;
            Some(rest[rest.find(']')? + 1..].trim().to_string())
        })
        .filter(|l| !l.is_empty())
        .collect();
    let skip = all.len().saturating_sub(n);
    all.into_iter().skip(skip).collect()
}

/// The prompt section, or `None` when there is nothing to say.
pub fn prompt_section(file: &Path) -> Option<String> {
    let items = newest(file, PROMPT_LESSONS);
    if items.is_empty() {
        return None;
    }
    let mut out = String::from(
        "## Lessons from earlier sessions\n\
         These were learned by investigating failures in past sessions on this machine. \
         Apply them when relevant.\n",
    );
    for l in items {
        out.push_str("- ");
        out.push_str(&l);
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_replaces_by_ticket_and_keeps_order() {
        let f = crate::ticket::tests::scratch("lessons").join("lessons.md");
        record(&f, "aaa", "first").unwrap();
        record(&f, "bbb", "second").unwrap();
        record(&f, "aaa", "first, revised").unwrap();
        assert_eq!(newest(&f, 10), vec!["second".to_string(), "first, revised".to_string()]);
        assert_eq!(newest(&f, 1), vec!["first, revised".to_string()]);
        let s = prompt_section(&f).unwrap();
        assert!(s.contains("- second\n"));
        record(&f, "ccc", "never write {{cwd}} literally").unwrap();
        assert!(!prompt_section(&f).unwrap().contains("{{"));
    }

    #[test]
    fn empty_file_has_no_section() {
        let f = crate::ticket::tests::scratch("lessons-empty").join("lessons.md");
        assert!(prompt_section(&f).is_none());
        assert!(category_teaches("environment"));
        assert!(!category_teaches("harness_bug"));
    }
}
