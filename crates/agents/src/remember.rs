//! The model's three memory tools: `remember`, `recall`, `forget`.
//!
//! - `remember '<fact>' [scope]` — `project` (the default: true in this
//!   working directory), `global` (true of the user everywhere) or
//!   `session` (a key fact for this session's own memory only).
//! - `recall '<query>'` — search long-term memory and the memories of
//!   earlier sessions: what was done, decided and learned.
//! - `forget '<id>'` — delete a long-term memory by its `[m-…]` id.
//!
//! They are harness controls like `todo-write`: `remember … session`
//! appends to the session log, and `recall` reads every session's, which no
//! `SkillContext` can reach — so the bodies run in `backend::chat` and the
//! `run` impls here are unreachable fallbacks. Parsing, ranking and wording
//! live here so the backend and its tests share one vocabulary.
//!
//! On by default, unlike the reminder tools: memory is the point of the
//! feature, and three short entries are a small price. `skills/memory.md`
//! is seeded *on*; renaming it to `memory.md.off` (Settings › Integrations)
//! takes the tools out of the catalogue at the next backend start. What is
//! already remembered still reaches the prompt either way — that is
//! `memory.toml`'s `inject`, not this switch.

use std::collections::HashSet;
use std::path::Path;

use async_trait::async_trait;
use serde_json::Value;

use crate::long_term::{normalized, Memory};
use crate::skill::{Skill, SkillContext, SkillOutcome};

pub const REMEMBER_NAME: &str = "remember";
pub const RECALL_NAME: &str = "recall";
pub const FORGET_NAME: &str = "forget";

/// `skills/memory.md` turns the three tools on.
pub const MEMORY_DOC_STEM: &str = "memory";

/// The doc seeded as `skills/memory.md`. Kept out of the model's catalogue
/// (`disable-model-invocation`) — the model sees the three tools, a person
/// sees `/memory` as a reference.
pub const MEMORY_SEED_MD: &str = "\
---
name: memory
description: The agent's memory tools — remember, recall, forget (on by default).
disable-model-invocation: true
---
# Memory

While this file is `skills/memory.md` the agent has three tools:

- `remember '<fact>' [scope]` — save one fact. `scope` is `project` (the
  default: true in this working directory), `global` (true of the user in
  every project) or `session` (a key fact for this session only).
- `recall '<query>'` — search long-term memory and the memories of earlier
  sessions.
- `forget '<id>'` — delete a long-term memory by its `[m-…]` id.

Every session also keeps its own memory — a running summary and the key
facts — updated in the background while it is idle, and re-attached after a
compaction. Long-term memories that apply to a session's folder are put in
front of the model at the start of each turn. Both are in the header's
Memory popover and in Settings › Memory; `sica-settings/memory.toml` holds
the knobs.

Rename this file to `memory.md.off` (or use Settings › Integrations) to take
the three tools away; the backend reads it at startup. Memories already
saved still reach the prompt unless `inject = false` in `memory.toml`.
";

/// Seed `skills/memory.md` when neither it nor `memory.md.off` exists. On
/// by default, never overwritten.
pub fn seed_default(dir: &Path) -> std::io::Result<()> {
    let on = dir.join(format!("{MEMORY_DOC_STEM}.md"));
    let off = dir.join(format!("{MEMORY_DOC_STEM}.md.off"));
    if !on.exists() && !off.exists() {
        std::fs::create_dir_all(dir)?;
        std::fs::write(&on, MEMORY_SEED_MD)?;
    }
    Ok(())
}

pub fn is_memory_skill(name: &str) -> bool {
    name == REMEMBER_NAME || name == RECALL_NAME || name == FORGET_NAME
}

/// Where `remember` puts a fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RememberScope {
    Session,
    Project,
    Global,
}

impl RememberScope {
    pub fn label(&self) -> &'static str {
        match self {
            RememberScope::Session => "session",
            RememberScope::Project => "project",
            RememberScope::Global => "global",
        }
    }
}

/// `scope`, defaulting to `project`. Aliases cover what models reach for.
pub fn parse_scope(v: Option<&Value>) -> Result<RememberScope, String> {
    let raw = match v {
        None | Some(Value::Null) => return Ok(RememberScope::Project),
        Some(Value::String(s)) => s.trim().to_ascii_lowercase(),
        Some(other) => other.to_string().to_ascii_lowercase(),
    };
    let raw = raw.strip_prefix("scope=").unwrap_or(&raw).trim().to_string();
    match raw.as_str() {
        "" | "project" | "folder" | "workspace" | "repo" | "local" => Ok(RememberScope::Project),
        "global" | "user" | "everywhere" | "all" => Ok(RememberScope::Global),
        "session" | "this session" | "short-term" | "note" => Ok(RememberScope::Session),
        other => Err(format!(
            "unknown scope `{other}` — use project (this folder), global (every project) or session"
        )),
    }
}

/// The `fact` argument, with the few other names a model might give it.
pub fn fact_arg(args: &Value) -> Result<String, String> {
    ["fact", "text", "memory", "content"]
        .iter()
        .find_map(|k| args.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "missing `fact` — one fact to remember, in a sentence".to_string())
}

fn unreachable(name: &str) -> SkillOutcome {
    SkillOutcome {
        ok: false,
        summary: format!(
            "`{name}` is handled by the harness loop, not by a skill body — \
             this call should never have been dispatched"
        ),
    }
}

pub struct Remember;

#[async_trait]
impl Skill for Remember {
    fn name(&self) -> &str {
        REMEMBER_NAME
    }
    fn description(&self) -> &str {
        "Save one fact to memory. scope: project (default) — about this folder; global — about \
         the user, true in every project; session — a note for this session only."
    }
    fn positional_args(&self) -> Vec<String> {
        vec!["fact".into(), "scope".into()]
    }
    fn parameters_schema(&self) -> Option<Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "fact":  { "type": "string" },
                "scope": { "type": "string", "enum": ["project", "global", "session"] },
            },
            "required": ["fact"],
        }))
    }
    fn prompt_guidance(&self) -> Option<&'static str> {
        Some(
            "When the user states a lasting preference or fact, or asks you to remember \
             something, save it with remember — one fact per call, never task progress.",
        )
    }
    async fn run(&self, _args: Value, _ctx: SkillContext) -> SkillOutcome {
        unreachable(REMEMBER_NAME)
    }
}

pub struct Recall;

#[async_trait]
impl Skill for Recall {
    fn name(&self) -> &str {
        RECALL_NAME
    }
    fn description(&self) -> &str {
        "Search long-term memory and the memories of earlier sessions — what was done, \
         decided and learned."
    }
    fn positional_args(&self) -> Vec<String> {
        vec!["query".into()]
    }
    fn prompt_guidance(&self) -> Option<&'static str> {
        Some("When the user refers to earlier work or a past session, recall before saying you do not know.")
    }
    async fn run(&self, _args: Value, _ctx: SkillContext) -> SkillOutcome {
        unreachable(RECALL_NAME)
    }
}

pub struct Forget;

#[async_trait]
impl Skill for Forget {
    fn name(&self) -> &str {
        FORGET_NAME
    }
    fn description(&self) -> &str {
        "Delete a long-term memory by its [m-…] id — when the user asks, or it is wrong."
    }
    fn positional_args(&self) -> Vec<String> {
        vec!["id".into()]
    }
    async fn run(&self, _args: Value, _ctx: SkillContext) -> SkillOutcome {
        unreachable(FORGET_NAME)
    }
}

/// Which memory `forget` means. An id wins; failing that, a unique memory
/// whose words contain the argument's — models often pass the fact itself.
pub fn resolve_forget<'a>(arg: &str, memories: &'a [Memory]) -> Result<&'a Memory, String> {
    let arg = arg.trim().trim_matches(|c| c == '[' || c == ']' || c == '`');
    if arg.is_empty() {
        return Err("missing `id` — the [m-…] id shown beside the memory".into());
    }
    if let Some(m) = memories.iter().find(|m| m.id == arg) {
        return Ok(m);
    }
    let needle = normalized(arg);
    let hits: Vec<&Memory> = if needle.is_empty() {
        Vec::new()
    } else {
        memories.iter().filter(|m| normalized(&m.text).contains(&needle)).collect()
    };
    match hits.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("no long-term memory `{arg}` — recall to find its id")),
        many => Err(format!(
            "`{arg}` matches {} memories — name one by id:\n{}",
            many.len(),
            many.iter().map(|m| format!("- [{}] {}", m.id, m.text)).collect::<Vec<_>>().join("\n")
        )),
    }
}

/// One earlier session as `recall` sees it.
pub struct SessionCandidate<'a> {
    pub id:      u64,
    pub title:   &'a str,
    pub cwd:     Option<&'a Path>,
    pub memory:  &'a sica_core::project::SessionMemory,
}

/// Words too common to rank on.
const STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "of", "to", "in", "on", "at", "for", "with", "is", "it",
    "was", "we", "i", "you", "my", "our", "what", "did", "do", "does", "how", "about", "that",
    "this", "last", "time", "session", "sessions", "earlier", "before", "previous", "remember",
];

fn terms(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in normalized(query).split(' ') {
        if w.chars().count() < 2 || STOPWORDS.contains(&w) || out.iter().any(|o| o == w) {
            continue;
        }
        out.push(w.to_string());
    }
    out
}

/// How many of `terms` the text contains. A term of four or more letters
/// also matches as a word prefix, so `postgres` finds `PostgreSQL`.
fn score(terms: &[String], text: &str) -> usize {
    let words: HashSet<String> = normalized(text).split(' ').map(str::to_string).collect();
    terms
        .iter()
        .filter(|t| words.iter().any(|w| w == *t || (t.chars().count() >= 4 && w.starts_with(t.as_str()))))
        .count()
}

/// Characters one session contributes to the answer.
const SESSION_CHARS: usize = 600;
/// Characters of the whole answer.
const ANSWER_CAP: usize = 5_000;

/// The `recall` answer: matching long-term memories, then matching earlier
/// sessions (their summary, and the key facts that matched). A query that
/// matches nothing — or says nothing searchable, like "last time" — lists
/// the most recent sessions in this folder instead, which is usually what
/// was meant.
pub fn recall_text(
    query: &str,
    memories: &[Memory],
    sessions: &[SessionCandidate<'_>],
    folder: &Path,
    now_ms: i64,
) -> String {
    let terms = terms(query);
    let in_folder = |s: &SessionCandidate<'_>| s.cwd.is_some_and(|c| crate::long_term::same_folder(c, folder));

    let mut mem_hits: Vec<(usize, &Memory)> = memories
        .iter()
        .filter(|m| m.applies_to(folder))
        .map(|m| (score(&terms, &m.text), m))
        .filter(|(s, _)| *s > 0)
        .collect();
    mem_hits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.updated_at.cmp(&a.1.updated_at)));
    mem_hits.truncate(8);

    let mut session_hits: Vec<(usize, &SessionCandidate<'_>)> = sessions
        .iter()
        .map(|s| {
            let hay = format!("{} {} {}", s.title, s.memory.summary, s.memory.facts.join(" "));
            (score(&terms, &hay), s)
        })
        .filter(|(sc, _)| *sc > 0)
        .collect();
    session_hits.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(in_folder(b.1).cmp(&in_folder(a.1)))
            .then(b.1.memory.ts.cmp(&a.1.memory.ts))
    });
    session_hits.truncate(5);

    let mut out = String::new();
    if !mem_hits.is_empty() {
        out.push_str("Long-term memories:\n");
        for (_, m) in &mem_hits {
            let scope = if m.is_global() { "global" } else { "project" };
            out.push_str(&format!("- [{}] ({scope}) {}\n", m.id, m.text));
        }
    }
    if !session_hits.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("Earlier sessions:\n");
        for (_, s) in &session_hits {
            out.push_str(&session_block(s, &terms, now_ms));
        }
    }
    if out.is_empty() {
        let mut recent: Vec<&SessionCandidate<'_>> = sessions.iter().collect();
        recent.sort_by(|a, b| in_folder(b).cmp(&in_folder(a)).then(b.memory.ts.cmp(&a.memory.ts)));
        recent.truncate(3);
        if recent.is_empty() {
            return if terms.is_empty() {
                "Nothing to recall yet — no long-term memories and no earlier session has a memory."
                    .into()
            } else {
                format!("No memory matches `{}`, and no earlier session has a memory yet.", query.trim())
            };
        }
        out.push_str(&if terms.is_empty() {
            "The most recent earlier sessions:\n".to_string()
        } else {
            format!(
                "No memory matches `{}`. The most recent earlier sessions were:\n",
                query.trim()
            )
        });
        for s in recent {
            out.push_str(&session_block(s, &[], now_ms));
        }
    }
    if out.len() > ANSWER_CAP {
        let mut cut = ANSWER_CAP;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str("\n… (more matched — narrow the query)");
    }
    out.trim_end().to_string()
}

fn session_block(s: &SessionCandidate<'_>, terms: &[String], now_ms: i64) -> String {
    let date = chrono::DateTime::from_timestamp_millis(s.memory.ts)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default();
    let ago = human_ago(now_ms - s.memory.ts);
    let mut summary = s.memory.summary.trim().to_string();
    if summary.len() > SESSION_CHARS {
        let mut cut = SESSION_CHARS;
        while !summary.is_char_boundary(cut) {
            cut -= 1;
        }
        summary.truncate(cut);
        summary.push('…');
    }
    let mut out = format!("- session {} \"{}\" ({date}, {ago}): {summary}\n", s.id, s.title);
    let facts: Vec<&String> = if terms.is_empty() {
        Vec::new()
    } else {
        s.memory.facts.iter().filter(|f| score(terms, f) > 0).take(4).collect()
    };
    for f in facts {
        out.push_str(&format!("    · {f}\n"));
    }
    out
}

fn human_ago(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    if secs < 3_600 {
        format!("{} min ago", (secs / 60).max(1))
    } else if secs < 86_400 {
        format!("{} h ago", secs / 3_600)
    } else {
        format!("{} days ago", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mem(id: &str, text: &str, project: Option<&str>, at: i64) -> Memory {
        Memory {
            id: id.into(),
            text: text.into(),
            project: project.map(PathBuf::from),
            created_at: at,
            updated_at: at,
            source: "model".into(),
            session: None,
        }
    }

    fn smem(summary: &str, facts: &[&str], ts: i64) -> sica_core::project::SessionMemory {
        sica_core::project::SessionMemory {
            seq: 1,
            ts,
            summary: summary.into(),
            facts: facts.iter().map(|f| f.to_string()).collect(),
            through_seq: 1,
            author: "auto".into(),
        }
    }

    #[test]
    fn scope_defaults_to_project_and_takes_aliases() {
        assert_eq!(parse_scope(None).unwrap(), RememberScope::Project);
        assert_eq!(parse_scope(Some(&Value::String("GLOBAL".into()))).unwrap(), RememberScope::Global);
        assert_eq!(parse_scope(Some(&Value::String("scope=session".into()))).unwrap(), RememberScope::Session);
        assert_eq!(parse_scope(Some(&Value::String("user".into()))).unwrap(), RememberScope::Global);
        assert!(parse_scope(Some(&Value::String("forever".into()))).is_err());
    }

    #[test]
    fn the_fact_argument_accepts_the_names_models_use() {
        assert_eq!(fact_arg(&serde_json::json!({"fact": " a "})).unwrap(), "a");
        assert_eq!(fact_arg(&serde_json::json!({"text": "b"})).unwrap(), "b");
        assert!(fact_arg(&serde_json::json!({"fact": "  "})).is_err());
        assert!(fact_arg(&serde_json::json!({})).is_err());
    }

    #[test]
    fn forget_takes_an_id_or_a_unique_phrase() {
        let all = vec![
            mem("m-00000001", "The user prefers PowerShell", None, 1),
            mem("m-00000002", "Build with run.ps1", Some("C:\\p"), 2),
            mem("m-00000003", "The user prefers dark mode", None, 3),
        ];
        assert_eq!(resolve_forget("[m-00000002]", &all).unwrap().id, "m-00000002");
        assert_eq!(resolve_forget("run.ps1", &all).unwrap().id, "m-00000002");
        let many = resolve_forget("the user prefers", &all).unwrap_err();
        assert!(many.contains("matches 2") && many.contains("m-00000003"), "{many}");
        assert!(resolve_forget("m-99999999", &all).is_err());
        assert!(resolve_forget("  ", &all).is_err());
    }

    #[test]
    fn recall_ranks_memories_and_sessions_by_the_words_that_matter() {
        let folder = PathBuf::from("C:\\work\\proj");
        let memories = vec![
            mem("m-00000001", "The project uses PostgreSQL 16 on port 5433", Some("C:\\work\\proj"), 1),
            mem("m-00000002", "The user prefers terse answers", None, 2),
            mem("m-00000003", "Other project uses MySQL", Some("C:\\elsewhere"), 3),
        ];
        let a = smem("Migrated the schema to postgres and fixed the pool size.", &["pool size is 20", "unrelated"], 10);
        let b = smem("Wrote the README.", &["README lives at docs/"], 20);
        let sessions = vec![
            SessionCandidate { id: 7, title: "DB migration", cwd: Some(folder.as_path()), memory: &a },
            SessionCandidate { id: 9, title: "Docs", cwd: Some(folder.as_path()), memory: &b },
        ];
        let text = recall_text("what did we do about the postgres pool?", &memories, &sessions, &folder, 60_000);
        assert!(text.contains("[m-00000001]"), "{text}");
        assert!(!text.contains("m-00000003"), "another folder's memory never shows: {text}");
        assert!(text.contains("session 7 \"DB migration\""), "{text}");
        assert!(text.contains("· pool size is 20"), "matching facts ride along: {text}");
        assert!(!text.contains("unrelated"), "{text}");
        assert!(!text.contains("session 9"), "{text}");
    }

    #[test]
    fn a_query_that_finds_nothing_lists_the_recent_sessions_instead() {
        let folder = PathBuf::from("C:\\work");
        let a = smem("Old work.", &[], 10);
        let b = smem("Newer work.", &[], 20);
        let sessions = vec![
            SessionCandidate { id: 1, title: "old", cwd: Some(folder.as_path()), memory: &a },
            SessionCandidate { id: 2, title: "new", cwd: Some(folder.as_path()), memory: &b },
        ];
        let text = recall_text("last time", &[], &sessions, &folder, 30);
        assert!(text.starts_with("The most recent earlier sessions"), "{text}");
        assert!(text.find("session 2").unwrap() < text.find("session 1").unwrap(), "newest first");
        let text = recall_text("zeppelin", &[], &sessions, &folder, 30);
        assert!(text.starts_with("No memory matches `zeppelin`"), "{text}");
        assert!(recall_text("zeppelin", &[], &[], &folder, 30).contains("no earlier session"));
    }

    #[test]
    fn the_doc_is_seeded_on_once_and_left_alone() {
        let dir = std::env::temp_dir().join(format!("sica-remember-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        seed_default(&dir).unwrap();
        let on = dir.join("memory.md");
        assert!(on.exists());
        std::fs::rename(&on, dir.join("memory.md.off")).unwrap();
        seed_default(&dir).unwrap();
        assert!(!on.exists(), "a switched-off doc is not switched back on");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
