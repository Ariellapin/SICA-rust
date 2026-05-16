//! Decides whether an error originated in the frontend, the backend, the LLM
//! plumbing, or somewhere we can't pin down. Drives ticket type + auto-fix
//! eligibility.

use crate::trigger_bus::Trigger;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerSource {
    Frontend,
    Backend,
    Llm,
    Unknown,
}

pub fn classify(t: &Trigger) -> TriggerSource {
    let m = t.module.as_str();
    if m.starts_with("frontend::") || m.starts_with("crate::ui") || m.starts_with("ui::") {
        return TriggerSource::Frontend;
    }
    if m.starts_with("llm::") {
        return TriggerSource::Llm;
    }
    if m.starts_with("backend::")
        || m.starts_with("agents::")
        || m.starts_with("idealist::")
        || m.starts_with("protocol::")
    {
        return TriggerSource::Backend;
    }
    TriggerSource::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(module: &str) -> Trigger {
        Trigger {
            kind: "error".into(),
            module: module.into(),
            message: "x".into(),
            traceback: None,
        }
    }

    #[test]
    fn frontend_module_is_frontend() {
        assert_eq!(classify(&t("frontend::ui::chat")), TriggerSource::Frontend);
        assert_eq!(classify(&t("ui::status_bar")), TriggerSource::Frontend);
    }

    #[test]
    fn backend_module_is_backend() {
        assert_eq!(classify(&t("backend::dispatcher")), TriggerSource::Backend);
        assert_eq!(classify(&t("agents::turn")), TriggerSource::Backend);
        assert_eq!(classify(&t("idealist::lib")), TriggerSource::Backend);
    }

    #[test]
    fn llm_module_is_llm() {
        assert_eq!(classify(&t("llm::client")), TriggerSource::Llm);
    }

    #[test]
    fn unknown_module_falls_back() {
        assert_eq!(classify(&t("???")), TriggerSource::Unknown);
        assert_eq!(classify(&t("")), TriggerSource::Unknown);
    }
}
