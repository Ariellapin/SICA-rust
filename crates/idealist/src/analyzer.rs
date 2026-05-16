//! Best-effort categorization of a `Trigger` into a short kind tag. The Python
//! version routed this through an LLM; here we keep it pattern-based so the
//! crate stays cheap to build and avoids a circular dep on `llm`.
//!
//! For triggers raised by failed sub-agent tool calls (see `classify` →
//! `SubAgentTool`), the analyzer also detects environment mismatches:
//! e.g. when `run-cli` fails on Windows because `cmd.exe` cannot resolve a
//! command that PowerShell would. In that case `suggested_skill` is set so
//! `be_autofix` can record an actionable suggestion in the improvement
//! ticket.

use crate::trigger_bus::Trigger;

#[derive(Debug, Clone)]
pub struct Analysis {
    pub summary:         String,
    pub category:        String,
    pub severity:        String,
    pub proposed_fix:    String,
    pub files_to_touch:  Vec<String>,
    /// Optional: the name of a skill that the LLM should use *instead of*
    /// the one that failed. Populated when the analyzer can map the
    /// observed failure to a known-better alternative on this host.
    pub suggested_skill: Option<String>,
}

pub fn analyze(t: &Trigger) -> Analysis {
    let lower = t.message.to_lowercase();
    let category = if lower.contains("permission") {
        "Permissions"
    } else if lower.contains("not found") || lower.contains("no such") {
        "Filesystem"
    } else if lower.contains("connection") || lower.contains("connect") {
        "Network"
    } else if lower.contains("panic") {
        "Panic"
    } else {
        "Logic"
    };
    let severity = if t.traceback.is_some() { "Error" } else { "Warning" };

    let (proposed_fix, suggested_skill) = propose_fix(t, &lower);

    Analysis {
        summary:         t.message.clone(),
        category:        category.into(),
        severity:        severity.into(),
        proposed_fix,
        files_to_touch:  Vec::new(),
        suggested_skill,
    }
}

/// Returns `(human-readable fix, optional replacement skill name)`.
///
/// The replacement-skill suggestion is the actionable bit: when present, the
/// improvement ticket explicitly names which skill the LLM should pick next
/// time. The auto-apply path uses it to surface the swap in the ticket
/// summary as well.
fn propose_fix(t: &Trigger, lower: &str) -> (String, Option<String>) {
    // Sub-agent tool-call failures use the `agents::tool::<skill-name>` module
    // convention populated by `subagent::run`.
    if let Some(skill) = t.module.strip_prefix("agents::tool::") {
        return propose_fix_for_tool(skill, t, lower);
    }
    (
        String::from("Investigate the failing path; see traceback for the exact frame."),
        None,
    )
}

fn propose_fix_for_tool(
    skill: &str,
    t: &Trigger,
    lower: &str,
) -> (String, Option<String>) {
    let host_os = parse_host_os(&t.traceback);

    match skill {
        // cmd.exe on Windows uses a *different* PATH resolution and command
        // lookup than PowerShell — the classic symptom is "is not recognized
        // as an internal or external command". PowerShell finds these via
        // its own resolver and reports the same situation as
        // "is not recognized as the name of a cmdlet".
        "run-cli" if host_os == Some("windows") && looks_like_cmd_lookup_failure(lower) => (
            format!(
                "On Windows, `cmd.exe` couldn't resolve the command. \
                 Retry with the `run-pwsh` skill — PowerShell uses a \
                 different PATH/alias resolver and is the wrapper-script \
                 shell the project targets (see CLAUDE.md). \
                 Failing summary: {}",
                short(&t.message)
            ),
            Some("run-pwsh".into()),
        ),
        "run-cli" if looks_like_timeout(lower) => (
            "The command exceeded the 30 s timeout. Split it into smaller \
             steps, run it in the background outside the sub-agent, or \
             raise `CLI_TIMEOUT` in `agents::builtins`."
                .into(),
            None,
        ),
        "read-file" if lower.contains("escapes workspace") => (
            "The path tried to walk outside the workspace via `..`. Pass \
             a path relative to the workspace root, or an absolute one."
                .into(),
            None,
        ),
        "read-file" if lower.contains("too large") => (
            "File exceeded the 1 MiB read cap. Read a slice via `run-pwsh` \
             (`Get-Content -TotalCount N`) or split the file."
                .into(),
            None,
        ),
        "read-file" if lower.contains("stat ") || lower.contains("no such") => (
            "The file does not exist at the resolved path. Check the path \
             is relative to the workspace root, or list the directory \
             first with `run-pwsh`."
                .into(),
            None,
        ),
        "write-file" if lower.contains("escapes workspace") => (
            "The path tried to walk outside the workspace via `..`. Pass \
             a path relative to the workspace root, or an absolute one."
                .into(),
            None,
        ),
        "write-file" if lower.contains("permission") => (
            "Write was denied by the OS. Check the destination is not \
             read-only or held by another process; on Windows this often \
             means the file is open in another editor."
                .into(),
            None,
        ),
        _ => (
            format!(
                "Sub-agent tool `{skill}` returned an error: {}. \
                 Investigate the args and retry; consider whether a \
                 different skill is a better fit for this environment.",
                short(&t.message)
            ),
            None,
        ),
    }
}

fn looks_like_cmd_lookup_failure(lower: &str) -> bool {
    // cmd.exe phrasing.
    if lower.contains("is not recognized as an internal or external command") {
        return true;
    }
    // PowerShell's variant (covered too in case the user is already on pwsh
    // and still hitting it).
    if lower.contains("is not recognized as the name of a cmdlet") {
        return true;
    }
    // Fallback: anything mentioning "command not found".
    lower.contains("command not found")
}

fn looks_like_timeout(lower: &str) -> bool {
    lower.contains("timeout after") || lower.contains("timed out")
}

/// The trigger's traceback field carries the host OS line written by the
/// backend (`host_os=windows`). Pull it out so the analyzer doesn't have to
/// guess from `cfg!(windows)` — the daemon may be running on a developer's
/// box different from the failing one in a future deployment.
fn parse_host_os(traceback: &Option<String>) -> Option<&'static str> {
    let tb = traceback.as_deref()?;
    for line in tb.lines() {
        if let Some(rest) = line.trim().strip_prefix("host_os=") {
            return match rest.trim() {
                "windows" => Some("windows"),
                "linux"   => Some("linux"),
                "macos"   => Some("macos"),
                _         => None,
            };
        }
    }
    None
}

fn short(s: &str) -> String {
    const CAP: usize = 240;
    if s.len() <= CAP {
        return s.to_string();
    }
    let mut end = CAP;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("…");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(module: &str, msg: &str, tb: Option<&str>) -> Trigger {
        Trigger {
            kind:      "tool_failed".into(),
            module:    module.into(),
            message:   msg.into(),
            traceback: tb.map(String::from),
        }
    }

    #[test]
    fn windows_cmd_lookup_failure_suggests_pwsh() {
        let trig = t(
            "agents::tool::run-cli",
            "exit=1\n--- stderr ---\n'rg' is not recognized as an internal or external command,\noperable program or batch file.",
            Some("host_os=windows\nhost_family=windows\n"),
        );
        let a = analyze(&trig);
        assert_eq!(a.suggested_skill.as_deref(), Some("run-pwsh"));
        assert!(a.proposed_fix.contains("run-pwsh"));
    }

    #[test]
    fn run_cli_timeout_explains_the_cap() {
        let trig = t(
            "agents::tool::run-cli",
            "timeout after 30s",
            Some("host_os=linux\n"),
        );
        let a = analyze(&trig);
        assert!(a.suggested_skill.is_none());
        assert!(a.proposed_fix.contains("30 s"));
    }

    #[test]
    fn read_file_traversal_is_called_out() {
        let trig = t(
            "agents::tool::read-file",
            "path \"../../../etc/passwd\" escapes workspace via `..`",
            None,
        );
        let a = analyze(&trig);
        assert!(a.proposed_fix.contains("workspace"));
    }

    #[test]
    fn linux_cmd_failure_does_not_suggest_pwsh() {
        // The classic "is not recognized" string is Windows-only; on Linux
        // a missing binary surfaces as "command not found", which we also
        // detect, but we never push someone toward PowerShell on Linux.
        let trig = t(
            "agents::tool::run-cli",
            "/bin/sh: 1: foo: not found",
            Some("host_os=linux\n"),
        );
        let a = analyze(&trig);
        assert!(a.suggested_skill.is_none());
    }

    #[test]
    fn unrelated_module_uses_generic_fix() {
        let trig = t("backend::dispatcher", "some panic", Some("trace"));
        let a = analyze(&trig);
        assert!(a.suggested_skill.is_none());
        assert!(a.proposed_fix.contains("Investigate"));
    }
}
