//! Per-model recommended settings.
//!
//! Different model families want different harness settings: a Qwen3 local
//! build honours the `enable_thinking` template flag, a DeepSeek-R1 distill
//! reasons no matter what the flag says, small Llama/Mistral/Gemma builds
//! need a low temperature to keep the text-protocol tool-call syntax exact,
//! and OpenAI/Anthropic endpoints expect native tool calling. Getting these
//! wrong looks like "the model is bad at tools" when it is really a config
//! mismatch, so each provider card offers a one-click preset matched on the
//! model name.
//!
//! Matching is a case-insensitive substring scan, specific families first,
//! so `qwen3-8b-instruct` hits Qwen3 before the generic-local fallback. The
//! returned values use the same 0-means-auto convention as the provider TOML
//! (`max_tokens`/`context_window`/compact knobs stay 0 unless the family
//! genuinely needs a non-default).

/// Recommended harness settings for one model family.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPreset {
    /// Stable id, also used in tests.
    pub id: &'static str,
    /// Short label shown on the provider card (`Recommended: Qwen3 (local)`).
    pub label: &'static str,
    /// Sampling temperature for agentic turns (tool calls, summaries).
    pub temperature: f32,
    /// Whether the model should emit `<think>` reasoning. When false the
    /// backend sends `chat_template_kwargs: {"enable_thinking": false}`.
    pub thinking: bool,
    /// Whether to use the OpenAI-native `tools` / `tool_calls` API.
    /// Local servers need explicit tool support (e.g. vLLM
    /// `--enable-auto-tool-choice`); plain llama.cpp builds do not have it.
    pub native_tools: bool,
    /// Per-response completion cap. 0 = server default.
    pub max_tokens: u32,
    /// Prompt-window budget override. 0 = auto-detect from the server.
    pub context_window: u32,
    /// Compaction trigger, percent of budget. 0 = default (80).
    pub compact_threshold_pct: u32,
    /// Verbatim tail kept when compacting, percent. 0 = default (16).
    pub compact_retain_pct: u32,
    /// Completion cap for the compaction summary. 0 = default (8192).
    pub compact_max_tokens: u32,
    /// One-line why, shown as hover text on the provider card.
    pub note: &'static str,
}

const AUTO: ModelPreset = ModelPreset {
    id: "generic-local",
    label: "Generic local model",
    temperature: 0.2,
    thinking: true,
    native_tools: false,
    max_tokens: 0,
    context_window: 0,
    compact_threshold_pct: 0,
    compact_retain_pct: 0,
    compact_max_tokens: 0,
    note: "Text-protocol tool calling at low temperature. Enable native \
           tools only if the server was started with tool support \
           (vLLM --enable-auto-tool-choice).",
};

/// Best-effort preset for a model name as typed in the provider card.
/// Never fails — unknown names fall back to [`AUTO`]-equivalent values
/// (with the label adjusted for remote APIs, see [`preset_for_provider`]).
pub fn preset_for_model(model: &str) -> ModelPreset {
    let m = model.to_lowercase();
    let has = |subs: &[&str]| subs.iter().any(|s| m.contains(s));

    // Reasoning-first families: the thinking flag matters most here.
    if has(&["qwen3", "qwq"]) {
        return ModelPreset {
            id: "qwen3",
            label: "Qwen3 (local)",
            note: "Qwen3 honours the thinking toggle via its chat template; \
                   keep it on for tool work, off for faster terse replies.",
            ..AUTO
        };
    }
    if has(&["deepseek-r1", "deepseek-reasoner", "r1-distill", "reasoner"]) {
        return ModelPreset {
            id: "deepseek-r1",
            label: "DeepSeek R1 / distill",
            temperature: 0.3,
            thinking: true,
            note: "R1 reasons regardless of the thinking toggle — expect \
                   long <think> blocks. Slightly higher temperature than \
                   the default keeps it from stalling on tool syntax.",
            ..AUTO
        };
    }
    // Small / strict local builds: low temperature keeps the
    // `skill 'args' > expectation` line exact.
    if has(&["llama", "mistral", "mixtral", "gemma", "phi-", "phi3", "phi-3",
             "qwen2", "qwen-2", "ministral", "smollm", "stablelm"])
    {
        return ModelPreset {
            id: "small-local",
            label: "Small local model",
            temperature: 0.15,
            thinking: false,
            note: "Small builds rarely support <think> and drift off the \
                   tool-call syntax above ~0.3 — low temperature, thinking \
                   off, text protocol.",
            ..AUTO
        };
    }
    // Remote APIs with first-class tool support.
    if has(&["gpt-4o", "gpt-4", "gpt-5", "gpt-3.5", "o1", "o3", "o4"]) {
        return ModelPreset {
            id: "openai",
            label: "OpenAI GPT",
            temperature: 0.2,
            thinking: true,
            native_tools: true,
            note: "Native tool calling. Reasoning (o-series) models ignore \
                   temperature and the thinking toggle — both are sent \
                   harmlessly.",
            ..AUTO
        };
    }
    if has(&["claude", "sonnet", "opus", "haiku"]) {
        return ModelPreset {
            id: "anthropic",
            label: "Anthropic Claude",
            temperature: 0.3,
            thinking: true,
            native_tools: true,
            note: "Native tool calling. Extended thinking is a separate API \
                   flag — the harness toggle is ignored, harmlessly.",
            ..AUTO
        };
    }
    if has(&["deepseek-chat", "deepseek-v"]) {
        return ModelPreset {
            id: "deepseek-chat",
            label: "DeepSeek chat (API)",
            temperature: 0.2,
            thinking: true,
            native_tools: true,
            note: "DeepSeek's API supports native tools; fall back to the \
                   text protocol if the server errors on the tools array.",
            ..AUTO
        };
    }
    // Generic Qwen (non-3) and other mid-size locals: thinking-capable
    // templates exist, so leave the toggle on.
    if has(&["qwen", "yi-", "solar", "starling", "openchat"]) {
        return ModelPreset {
            id: "mid-local",
            label: "Mid-size local model",
            temperature: 0.2,
            thinking: true,
            note: "Text protocol at low temperature; thinking left on since \
                   these templates usually honour the toggle.",
            ..AUTO
        };
    }
    AUTO
}

/// Same as [`preset_for_model`], but flips the fallback to native tools when
/// the base URL looks like a hosted API rather than localhost. A model name
/// the matcher does not recognise on a remote endpoint is more likely an
/// OpenAI-compatible API (which usually accepts the tools array) than a
/// bare llama.cpp build.
pub fn preset_for_provider(base_url: &str, model: &str) -> ModelPreset {
    let p = preset_for_model(model);
    if p.id != AUTO.id {
        return p;
    }
    let u = base_url.to_lowercase();
    let local = u.contains("localhost")
        || u.contains("127.0.0.1")
        || u.contains("[::1]")
        || u.is_empty();
    if local {
        return p;
    }
    ModelPreset {
        id: "generic-remote",
        label: "Generic remote API",
        native_tools: true,
        note: "Remote OpenAI-compatible endpoint: native tools assumed. \
               Turn it off if the server rejects the tools array.",
        ..p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specific_families_win_over_generic() {
        assert_eq!(preset_for_model("qwen3-8b-instruct").id, "qwen3");
        assert_eq!(preset_for_model("Qwen3-32B-GGUF").id, "qwen3");
        assert_eq!(preset_for_model("deepseek-r1-distill-qwen-7b").id, "deepseek-r1");
        // r1-distill contains "qwen" too — the reasoning match must come first.
        assert_eq!(preset_for_model("R1-Distill-Llama-8B").id, "deepseek-r1");
        assert_eq!(preset_for_model("llama-3.1-8b-instruct").id, "small-local");
        assert_eq!(preset_for_model("Meta-Llama-3-70B").id, "small-local");
        assert_eq!(preset_for_model("gpt-4o-mini").id, "openai");
        assert_eq!(preset_for_model("o3-mini").id, "openai");
        assert_eq!(preset_for_model("claude-sonnet-4-6").id, "anthropic");
        assert_eq!(preset_for_model("deepseek-chat").id, "deepseek-chat");
        assert_eq!(preset_for_model("qwen2.5-7b-instruct").id, "small-local");
        assert_eq!(preset_for_model("something-entirely-new").id, "generic-local");
        assert_eq!(preset_for_model("").id, "generic-local");
    }

    #[test]
    fn presets_carry_sane_tool_modes() {
        assert!(preset_for_model("gpt-4o").native_tools);
        assert!(preset_for_model("claude-sonnet-4-6").native_tools);
        assert!(!preset_for_model("llama-3.1-8b").native_tools);
        assert!(!preset_for_model("qwen3-8b").native_tools);
        // Thinking: small locals off, reasoning models on.
        assert!(!preset_for_model("mistral-7b-instruct").thinking);
        assert!(preset_for_model("deepseek-r1-distill-qwen-7b").thinking);
    }

    #[test]
    fn remote_fallback_assumes_native_tools() {
        let p = preset_for_provider("https://api.example.com", "my-custom-model");
        assert_eq!(p.id, "generic-remote");
        assert!(p.native_tools);
        let p = preset_for_provider("http://localhost:8080", "my-custom-model");
        assert_eq!(p.id, "generic-local");
        assert!(!p.native_tools);
        // A recognised family is never overridden by the URL heuristic.
        let p = preset_for_provider("http://localhost:8080", "gpt-4o-mini");
        assert_eq!(p.id, "openai");
    }

    #[test]
    fn auto_fields_keep_zero_means_auto() {
        for m in ["qwen3-8b", "llama-3.1-8b", "gpt-4o", "claude-sonnet-4-6", "xyz"] {
            let p = preset_for_model(m);
            assert_eq!(p.max_tokens, 0, "{m}");
            assert_eq!(p.context_window, 0, "{m}");
            assert_eq!(p.compact_threshold_pct, 0, "{m}");
        }
    }
}
