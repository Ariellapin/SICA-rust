//! Credential *references* (guide §14.6).
//!
//! A config file may carry `api_key = "${DEEPSEEK_API_KEY}"` instead of the
//! key itself. Three things follow from that, and they are the whole point:
//!
//! - The file can be shared, diffed and (if the user insists) committed,
//!   because it holds the *name* of a secret and never the secret.
//! - The value is resolved **per operation**, not once at startup, so a
//!   rotated key reaches the next request without restarting anything.
//! - [`describe`] can answer "is this configured, and from where?" without
//!   returning the value, which is what lets a settings card show
//!   `configured · env` and stay write-only.
//!
//! Two layers, in order: the process environment, then
//! `sica-settings/.env`. Deliberately **not** the working directory's
//! `.env` — that file arrives with `git clone`, and a repository must not be
//! able to hand the app a credential by existing.
//!
//! An empty value is absent everywhere: an unset variable, a variable set to
//! `""` and a missing file are one state, because they mean the same thing
//! to the caller.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Where a resolved value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A literal in the config file.
    Literal,
    /// The process environment.
    Env(String),
    /// `sica-settings/.env`.
    File(String),
}

/// What a config value is, without saying what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Nothing there at all.
    Absent,
    Configured(Source),
    /// A reference whose variable is set nowhere. The name is safe to show —
    /// it is what the user has to go and set.
    Unresolved(String),
}

/// `true` when the value is a reference rather than a secret.
pub fn is_reference(value: &str) -> bool {
    value.contains("${")
}

/// Expand every `${NAME}` in `value`.
///
/// A reference that resolves to nothing expands to the empty string, so a
/// caller that only checks "is this empty" behaves the same for a missing
/// key and a missing reference. Text that is not a reference comes back
/// unchanged — including a literal key, which stays supported.
pub fn resolve(value: &str) -> String {
    if !is_reference(value) {
        return value.to_string();
    }
    let file = dotenv();
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(open) = rest.find("${") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find('}') else {
            // An unterminated `${` is text, not a broken reference.
            out.push_str(&rest[open..]);
            return out;
        };
        let name = &after[..close];
        out.push_str(&lookup(name, &file).unwrap_or_default());
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Answer whether a value is usable, and from where, **without returning
/// it**. This is the half of a credential that can safely cross a wire or
/// reach a UI.
pub fn describe(value: &str) -> Status {
    if value.trim().is_empty() {
        return Status::Absent;
    }
    if !is_reference(value) {
        return Status::Configured(Source::Literal);
    }
    let file = dotenv();
    // The first reference in the value decides what to report; a config
    // value with two references in it is not a shape this app writes.
    let Some(name) = first_name(value) else { return Status::Absent };
    match lookup(&name, &file) {
        Some(_) if std::env::var(&name).is_ok_and(|v| !v.is_empty()) => {
            Status::Configured(Source::Env(name))
        }
        Some(_) => Status::Configured(Source::File(name)),
        None => Status::Unresolved(name),
    }
}

fn first_name(value: &str) -> Option<String> {
    let open = value.find("${")?;
    let after = &value[open + 2..];
    let close = after.find('}')?;
    Some(after[..close].to_string())
}

/// Process environment first, then the file. An empty value counts as unset
/// in both layers.
fn lookup(name: &str, file: &BTreeMap<String, String>) -> Option<String> {
    if let Ok(v) = std::env::var(name) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    file.get(name).filter(|v| !v.is_empty()).cloned()
}

/// `sica-settings/.env`, parsed fresh on every resolution — that is what
/// makes a rotated key take effect without a restart. The file is a few
/// lines; re-reading it costs less than the request it is about to
/// authenticate.
pub fn env_file() -> PathBuf {
    crate::paths::workspace_root().join("sica-settings").join(".env")
}

fn dotenv() -> BTreeMap<String, String> {
    parse_env(&std::fs::read_to_string(env_file()).unwrap_or_default())
}

/// `KEY=value` per line. `#` comments and blank lines are skipped, an
/// `export ` prefix is tolerated, and one layer of matching quotes is
/// stripped. Anything else is ignored rather than fatal — this file is
/// hand-edited, and a typo in it must not take the app down.
pub fn parse_env(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        out.insert(key.to_string(), value.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_literal_is_returned_unchanged() {
        assert!(!is_reference("sk-abc"));
        assert_eq!(resolve("sk-abc"), "sk-abc");
        assert_eq!(describe("sk-abc"), Status::Configured(Source::Literal));
        assert_eq!(describe("   "), Status::Absent);
    }

    #[test]
    fn a_reference_reads_the_process_environment() {
        let name = format!("SICA_TEST_KEY_{}", std::process::id());
        std::env::set_var(&name, "from-env");
        let value = format!("${{{name}}}");
        assert!(is_reference(&value));
        assert_eq!(resolve(&value), "from-env");
        assert_eq!(describe(&value), Status::Configured(Source::Env(name.clone())));
        // An empty variable is the same state as an unset one.
        std::env::set_var(&name, "");
        assert_eq!(resolve(&value), "");
        assert_eq!(describe(&value), Status::Unresolved(name.clone()));
        std::env::remove_var(&name);
    }

    #[test]
    fn a_reference_can_be_embedded_in_surrounding_text() {
        let name = format!("SICA_TEST_TOK_{}", std::process::id());
        std::env::set_var(&name, "xyz");
        assert_eq!(resolve(&format!("Bearer ${{{name}}}!")), "Bearer xyz!");
        // An unterminated reference is text, not an error.
        assert_eq!(resolve("${oops"), "${oops");
        std::env::remove_var(&name);
    }

    #[test]
    fn the_env_file_parser_takes_the_shapes_people_write() {
        let parsed = parse_env(
            "# comment\n\
             \n\
             PLAIN=value\n\
             export EXPORTED = spaced \n\
             QUOTED=\"with spaces\"\n\
             SINGLE='single'\n\
             EMPTY=\n\
             nonsense-line\n",
        );
        assert_eq!(parsed.get("PLAIN").unwrap(), "value");
        assert_eq!(parsed.get("EXPORTED").unwrap(), "spaced");
        assert_eq!(parsed.get("QUOTED").unwrap(), "with spaces");
        assert_eq!(parsed.get("SINGLE").unwrap(), "single");
        assert_eq!(parsed.get("EMPTY").unwrap(), "");
        assert!(parsed.get("nonsense-line").is_none());
        // An empty value is absent, not configured.
        assert!(lookup("EMPTY", &parsed).is_none());
    }
}
