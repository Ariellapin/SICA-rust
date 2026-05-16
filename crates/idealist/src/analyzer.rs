//! Best-effort categorization of a `Trigger` into a short kind tag. The Python
//! version routed this through an LLM; here we keep it pattern-based so the
//! crate stays cheap to build and avoids a circular dep on `llm`.

use crate::trigger_bus::Trigger;

#[derive(Debug, Clone)]
pub struct Analysis {
    pub summary:        String,
    pub category:       String,
    pub severity:       String,
    pub proposed_fix:   String,
    pub files_to_touch: Vec<String>,
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
    Analysis {
        summary:        t.message.clone(),
        category:       category.into(),
        severity:       severity.into(),
        proposed_fix:   String::from(
            "Investigate the failing path; see traceback for the exact frame.",
        ),
        files_to_touch: Vec::new(),
    }
}
