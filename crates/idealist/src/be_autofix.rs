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
pub fn write_be_ticket(t: &Trigger, _auto_apply: bool) -> Result<PathBuf> {
    let dir = idealist_workspace();
    fs::create_dir_all(&dir)?;

    let analysis = analyze(t);
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let safe_kind = sanitize(&t.kind);
    let path = dir.join(format!("Improvement-BE-{safe_kind}-{stamp}.md"));

    let body = format!(
        "---\nkind: BeFix\nmodule: {module}\nseverity: {sev}\ncategory: {cat}\ncreated: {stamp}\n---\n\n# Backend improvement: {summary}\n\n**Module:** `{module}`\n**Trigger kind:** `{kind}`\n\n## Message\n\n```\n{message}\n```\n\n## Traceback\n\n```\n{tb}\n```\n\n## Proposed fix\n\n{fix}\n",
        module  = t.module,
        sev     = analysis.severity,
        cat     = analysis.category,
        kind    = t.kind,
        summary = analysis.summary,
        message = t.message,
        tb      = t.traceback.clone().unwrap_or_else(|| "(none)".into()),
        fix     = analysis.proposed_fix,
    );
    fs::write(&path, body)?;
    Ok(path)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}
