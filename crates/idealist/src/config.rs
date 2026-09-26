//! `sica-settings/idealist.toml` — the investigator's knobs.
//!
//! Optional, like every other integration file: absent means the defaults
//! below, and a file that does not parse is reported and ignored rather than
//! failing startup.
//!
//! ```toml
//! investigate       = true   # run the end-of-session investigator at all
//! idle_minutes      = 10     # quiet time before a session counts as ended
//! max_per_session   = 5      # tickets investigated per session end
//! max_hops          = 12     # tool calls per investigation
//! timeout_secs      = 300    # wall clock per investigation
//! lessons_in_prompt = false  # add diagnosed lessons to the system prompt
//! ```

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct IdealistConfig {
    pub investigate:       bool,
    pub idle_minutes:      u64,
    pub max_per_session:   usize,
    pub max_hops:          u8,
    pub timeout_secs:      u64,
    /// Off by default: every lesson costs prompt tokens on every turn, and
    /// the rule for anything with a standing prompt cost is opt-in.
    pub lessons_in_prompt: bool,
}

impl Default for IdealistConfig {
    fn default() -> Self {
        Self {
            investigate:       true,
            idle_minutes:      10,
            max_per_session:   5,
            max_hops:          12,
            timeout_secs:      300,
            lessons_in_prompt: false,
        }
    }
}

pub fn path() -> PathBuf {
    sica_core::paths::settings_dir().join("idealist.toml")
}

/// The config, and a warning when the file exists but could not be used.
pub fn load() -> (IdealistConfig, Option<String>) {
    let p = path();
    let Ok(text) = std::fs::read_to_string(&p) else {
        return (IdealistConfig::default(), None);
    };
    match toml::from_str::<IdealistConfig>(&text) {
        Ok(mut c) => {
            c.idle_minutes = c.idle_minutes.max(1);
            c.max_hops = c.max_hops.max(1);
            c.timeout_secs = c.timeout_secs.max(10);
            (c, None)
        }
        Err(e) => (
            IdealistConfig::default(),
            Some(format!("{} ignored (defaults in use): {e}", p.display())),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_file_keeps_other_defaults() {
        let c: IdealistConfig = toml::from_str("idle_minutes = 3\n").unwrap();
        assert_eq!(c.idle_minutes, 3);
        assert!(c.investigate);
        assert!(!c.lessons_in_prompt);
    }
}
