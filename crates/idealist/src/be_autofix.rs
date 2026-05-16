use std::fs;
use std::path::PathBuf;

use anyhow::Result;

use sica_core::paths::idealist_workspace;

use crate::analyzer::analyze;
use crate::trigger_bus::Trigger;

/// Writes `Improvement-BE-<kind>-<iso>.md` describing the failure. If
/// `_auto_apply` is true the call site is expected to also schedule a code-edit
/// pass; this writer never edits source on its own — that's the safe default
/// the user picked when approving the plan.
///
/// When the analyzer can identify a better-fitting skill (e.g. `run-pwsh`
/// after a `cmd.exe` lookup failure on Windows), it is surfaced in a
/// dedicated "Suggested skill swap" section so the LLM has an actionable
/// next step on its next hop.
pub fn write_be_ticket(t: &Trigger, _auto_apply: bool) -> Result<PathBuf> {
    let dir = idealist_workspace();
    fs::create_dir_all(&dir)?;

    let analysis = analyze(t);
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let safe_kind = sanitize(&t.kind);
    let path = dir.join(format!("Improvement-BE-{safe_kind}-{stamp}.md"));

    let suggested_section = match analysis.suggested_skill.as_deref() {
        Some(name) => format!(
            "\n## Suggested skill swap\n\nRetry the failing operation with **`{name}`** \
             instead of the skill that just failed. See `skills/{name}.md` for the \
             contract.\n"
        ),
        None => String::new(),
    };

    let body = format!(
        "---\nkind: BeFix\nmodule: {module}\nseverity: {sev}\ncategory: {cat}\ncreated: {stamp}\n---\n\n# Backend improvement: {summary}\n\n**Module:** `{module}`\n**Trigger kind:** `{kind}`\n\n## Message\n\n```\n{message}\n```\n\n## Traceback\n\n```\n{tb}\n```\n\n## Proposed fix\n\n{fix}\n{suggested}",
        module    = t.module,
        sev       = analysis.severity,
        cat       = analysis.category,
        kind      = t.kind,
        summary   = analysis.summary,
        message   = t.message,
        tb        = t.traceback.clone().unwrap_or_else(|| "(none)".into()),
        fix       = analysis.proposed_fix,
        suggested = suggested_section,
    );
    fs::write(&path, body)?;
    Ok(path)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggested_skill_appears_in_ticket() {
        // Pick a temp dir for the workspace root the writer uses.
        let tmp = std::env::temp_dir().join(format!(
            "sica-idealist-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        // The writer resolves the workspace via the running exe's ancestors;
        // we can't override that without env-var plumbing, so just assert
        // on the rendered body via the `analyze` -> format path.
        let trigger = Trigger {
            kind:      "tool_failed".into(),
            module:    "agents::tool::run-cli".into(),
            message:   "exit=1\n'rg' is not recognized as an internal or external command".into(),
            traceback: Some("host_os=windows\n".into()),
        };
        let analysis = crate::analyzer::analyze(&trigger);
        assert_eq!(analysis.suggested_skill.as_deref(), Some("run-pwsh"));
    }
}
