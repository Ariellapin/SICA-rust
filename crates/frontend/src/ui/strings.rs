//! The user-facing strings that the UI guide names (§13): one language, so
//! a `pub const` per string rather than a runtime registry. A surface that
//! wants to say one of these reads it from here, which is what keeps the
//! wording in one place when it changes.

/// The assistant's status while a turn is running and nothing has streamed
/// yet (UI §3.2). sica's wording, not dsh's "Thinking…".
pub const TURN_STATUS: &str = "Working…";

/// The composer placeholder while no model is connected (UI §6.8).
pub const NO_MODEL: &str = "No model connected — select one to continue";

/// The composer placeholder on an empty session (UI §3.7).
pub const HERO_PLACEHOLDER: &str = "Describe what you want to build… / commands, @ files or sessions";

/// What the turn error row says (UI §3.5).
pub const TURN_FAILED: &str = "This turn failed";
