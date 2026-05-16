use std::fs;
use std::path::PathBuf;

use anyhow::Result;

use sica_core::paths::idealist_workspace;

use crate::analyzer::analyze;
use crate::trigger_bus::Trigger;

/// Writes `Improvement-FE-<iso>.md`. **Never** edits source — FE issues always
/// get a human-readable ticket so a developer can review before patching the UI.
pub fn write_fe_ticket(t: &Trigger) -> Result<PathBuf> {
    let dir = idealist_workspace();
    fs::create_dir_all(&dir)?;

    let analysis = analyze(t);
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let path = dir.join(format!("Improvement-FE-{stamp}.md"));

    let body = format!(
        "---\nkind: FeBug\nmodule: {module}\nseverity: {sev}\ncategory: {cat}\ncreated: {stamp}\n---\n\n# Frontend bug ticket: {summary}\n\n**Module:** `{module}`\n**Trigger kind:** `{kind}`\n\nFE issues are never auto-patched. Open this ticket and review the failing render path before making changes.\n\n## Message\n\n```\n{message}\n```\n\n## Traceback\n\n```\n{tb}\n```\n\n## Proposed investigation\n\n{fix}\n",
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
