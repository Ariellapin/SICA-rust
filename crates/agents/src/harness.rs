//! `sica-settings/harness.toml`: the turn budgets that used to be constants.
//!
//! Two numbers decide how long the harness lets a model work on one human
//! message before it stops and asks: the **tool-hop cap** per turn and the
//! **auto-continue** budget the completion check (`backend::verdict`) may
//! spend reopening a turn the cap cut short. Both were sized for a chat —
//! twelve hops, two continuations — and a two-hour task run against them
//! stops every thirty-odd tool calls to be told to carry on
//! (long-session-plan D1).
//!
//! The file is optional and absent by default, like every other file under
//! `sica-settings/`: no file means the defaults, a malformed file is a
//! warning the backend reports as a `LogLine` and the defaults apply, and a
//! value of `0` is refused (the loop would stop before its first call) and
//! reported the same way. Read once at backend start — a cap that could
//! change under a running turn would make two hops of one turn answer to
//! different budgets.
//!
//! ```toml
//! # sica-settings/harness.toml — every key optional
//! tool_hops_text   = 12   # text tool-calling: one call per model reply
//! tool_hops_native = 32   # native / PTC: a reply may carry a whole batch
//! auto_continues   = 2    # continuation turns per human message
//! spill_max_age_days       = 7     # spill sweeper (E2): 0 turns the rule off
//! spill_max_mib_per_session = 256  # oldest first past this; 0 turns it off
//! ```
//!
//! The two spill keys drive `agents::spill::sweep`, which the backend runs
//! at start and once an hour over `spill/<session>/`. Unlike the turn
//! budgets, `0` is meaningful there: it switches that rule off.
//!
//! The native cap is higher on purpose: a native batch already overlaps
//! its reads, the repeat guard (`agents::guard`) catches loops at 3/5/8,
//! and the completion check still audits every abnormal stop. Text mode
//! keeps the shorter leash because the small models that run it emit one
//! call per reply and drift more easily.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Tool hops per turn in text tool-calling mode.
pub const DEFAULT_TOOL_HOPS_TEXT: u8 = 12;
/// Tool hops per turn in native and PTC mode.
pub const DEFAULT_TOOL_HOPS_NATIVE: u8 = 32;
/// Continuation turns the completion check may open per human message.
pub const DEFAULT_AUTO_CONTINUES: u8 = 2;
/// Spilled tool outputs older than this are swept. Well past any tail a
/// session could still be reading: a spilled file the model may still
/// need is one the current checkpoint names, and no checkpoint reaches
/// back a week.
pub const DEFAULT_SPILL_MAX_AGE_DAYS: u32 = 7;
/// Per-session cap on spilled bytes; past it the oldest files go first.
pub const DEFAULT_SPILL_MAX_MIB_PER_SESSION: u32 = 256;

/// The loaded budgets. `Default` is the pre-D1 constants, so a test that
/// never reads the file sees exactly the behaviour it always did.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessConfig {
    #[serde(default = "default_hops_text")]
    pub tool_hops_text:   u8,
    #[serde(default = "default_hops_native")]
    pub tool_hops_native: u8,
    #[serde(default = "default_auto_continues")]
    pub auto_continues:   u8,
    /// `0` = no age rule.
    #[serde(default = "default_spill_age")]
    pub spill_max_age_days: u32,
    /// `0` = no size rule.
    #[serde(default = "default_spill_mib")]
    pub spill_max_mib_per_session: u32,
}

fn default_hops_text() -> u8 {
    DEFAULT_TOOL_HOPS_TEXT
}
fn default_hops_native() -> u8 {
    DEFAULT_TOOL_HOPS_NATIVE
}
fn default_auto_continues() -> u8 {
    DEFAULT_AUTO_CONTINUES
}
fn default_spill_age() -> u32 {
    DEFAULT_SPILL_MAX_AGE_DAYS
}
fn default_spill_mib() -> u32 {
    DEFAULT_SPILL_MAX_MIB_PER_SESSION
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            tool_hops_text:   DEFAULT_TOOL_HOPS_TEXT,
            tool_hops_native: DEFAULT_TOOL_HOPS_NATIVE,
            auto_continues:   DEFAULT_AUTO_CONTINUES,
            spill_max_age_days: DEFAULT_SPILL_MAX_AGE_DAYS,
            spill_max_mib_per_session: DEFAULT_SPILL_MAX_MIB_PER_SESSION,
        }
    }
}

impl HarnessConfig {
    /// The hop cap for a turn, by the transport it runs on. PTC rides the
    /// native transport and gets the native cap.
    pub fn hop_cap(&self, native_tools: bool) -> u8 {
        if native_tools { self.tool_hops_native } else { self.tool_hops_text }
    }

    /// One line for the startup `LogLine`, so the operator can see which
    /// budgets a session is running under without opening the file.
    pub fn summary(&self) -> String {
        format!(
            "tool hops {} text / {} native, {} auto-continue(s) per message, \
             spill sweep {} / {}",
            self.tool_hops_text,
            self.tool_hops_native,
            self.auto_continues,
            match self.spill_max_age_days {
                0 => "no age limit".to_string(),
                d => format!("{d} day(s)"),
            },
            match self.spill_max_mib_per_session {
                0 => "no size limit".to_string(),
                m => format!("{m} MiB per session"),
            },
        )
    }

    /// The age rule for [`crate::spill::sweep`], `None` when switched off.
    pub fn spill_max_age(&self) -> Option<std::time::Duration> {
        (self.spill_max_age_days > 0)
            .then(|| std::time::Duration::from_secs(u64::from(self.spill_max_age_days) * 86_400))
    }

    /// The size rule for [`crate::spill::sweep`], `None` when switched off.
    pub fn spill_max_bytes(&self) -> Option<u64> {
        (self.spill_max_mib_per_session > 0)
            .then(|| u64::from(self.spill_max_mib_per_session) << 20)
    }

    /// Replace every zero with its default, naming each in `warnings`. A
    /// zero hop cap ends the turn at the model's first call and a zero
    /// continue budget silently disables the completion check's follow-up;
    /// neither is a setting anyone means, so both are treated as typos.
    fn sanitise(mut self, path: &Path, warnings: &mut Vec<String>) -> Self {
        let fields: [(&str, &mut u8, u8); 3] = [
            ("tool_hops_text", &mut self.tool_hops_text, DEFAULT_TOOL_HOPS_TEXT),
            ("tool_hops_native", &mut self.tool_hops_native, DEFAULT_TOOL_HOPS_NATIVE),
            ("auto_continues", &mut self.auto_continues, DEFAULT_AUTO_CONTINUES),
        ];
        for (name, value, default) in fields {
            if *value == 0 {
                warnings.push(format!(
                    "harness: {} sets {name} = 0 — using the default {default}",
                    path.display()
                ));
                *value = default;
            }
        }
        self
    }
}

/// What a load produced: the config to run under and everything the
/// operator should hear about, each line ready for a `LogLine`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaded {
    pub config:   HarnessConfig,
    pub warnings: Vec<String>,
    /// Whether a file was read at all. The startup line says so, because
    /// a budget that silently fell back to the default is exactly what
    /// someone editing the file will go looking for.
    pub present:  bool,
}

/// Where the file lives: beside the other per-file settings.
pub fn config_path() -> PathBuf {
    sica_core::paths::settings_dir().join("harness.toml")
}

/// Load from [`config_path`]. Never fails: a missing file is the normal
/// case and a broken one is reported and ignored.
pub fn load() -> Loaded {
    load_from(&config_path())
}

pub fn load_from(path: &Path) -> Loaded {
    let mut out = Loaded::default();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return out,
        Err(e) => {
            out.warnings.push(format!("harness: {} unreadable: {e} — defaults apply", path.display()));
            return out;
        }
    };
    out.present = true;
    match toml::from_str::<HarnessConfig>(&text) {
        Ok(cfg) => out.config = cfg.sanitise(path, &mut out.warnings),
        Err(e) => out.warnings.push(format!(
            "harness: {} is malformed: {e} — defaults apply",
            path.display()
        )),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str, body: Option<&str>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sica-harness-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("harness.toml");
        let _ = std::fs::remove_file(&path);
        if let Some(b) = body {
            std::fs::write(&path, b).unwrap();
        }
        path
    }

    /// The contract with the replay recordings and every existing test:
    /// no file means the constants the loop always ran under.
    #[test]
    fn defaults_are_the_old_constants() {
        let cfg = HarnessConfig::default();
        assert_eq!(cfg.tool_hops_text, 12);
        assert_eq!(cfg.auto_continues, 2);
        assert_eq!(cfg.hop_cap(false), 12);
        assert_eq!(cfg.hop_cap(true), 32);
        let loaded = load_from(&tmp("absent", None));
        assert_eq!(loaded, Loaded::default());
        assert!(!loaded.present);
    }

    #[test]
    fn a_partial_file_fills_the_rest_from_defaults() {
        let loaded = load_from(&tmp("partial", Some("tool_hops_native = 64\n")));
        assert!(loaded.present);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.config.tool_hops_native, 64);
        assert_eq!(loaded.config.tool_hops_text, DEFAULT_TOOL_HOPS_TEXT);
        assert_eq!(loaded.config.auto_continues, DEFAULT_AUTO_CONTINUES);
        assert_eq!(loaded.config.spill_max_age_days, DEFAULT_SPILL_MAX_AGE_DAYS);
    }

    /// The spill keys are the one place a zero means something: off.
    #[test]
    fn a_zero_spill_rule_is_off_not_a_typo() {
        let loaded = load_from(&tmp("spill", Some("spill_max_age_days = 0\nspill_max_mib_per_session = 1\n")));
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.config.spill_max_age(), None);
        assert_eq!(loaded.config.spill_max_bytes(), Some(1 << 20));
        let d = HarnessConfig::default();
        assert_eq!(d.spill_max_age(), Some(std::time::Duration::from_secs(7 * 86_400)));
        assert_eq!(d.spill_max_bytes(), Some(256 << 20));
        assert!(d.summary().contains("7 day(s)") && d.summary().contains("256 MiB"), "{}", d.summary());
    }

    #[test]
    fn a_malformed_file_warns_and_keeps_the_defaults() {
        let loaded = load_from(&tmp("bad", Some("tool_hops_text = \"lots\"\n")));
        assert!(loaded.present);
        assert_eq!(loaded.config, HarnessConfig::default());
        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
        assert!(loaded.warnings[0].contains("malformed"), "{}", loaded.warnings[0]);
        // A key the loader does not know is a typo of one it does; saying
        // so beats silently running under the default.
        let loaded = load_from(&tmp("unknown", Some("tool_hops = 20\n")));
        assert!(loaded.warnings[0].contains("malformed"), "{}", loaded.warnings[0]);
    }

    #[test]
    fn a_zero_is_refused_per_field_and_named() {
        let loaded =
            load_from(&tmp("zero", Some("tool_hops_text = 0\nauto_continues = 0\ntool_hops_native = 9\n")));
        assert_eq!(loaded.config.tool_hops_text, DEFAULT_TOOL_HOPS_TEXT);
        assert_eq!(loaded.config.auto_continues, DEFAULT_AUTO_CONTINUES);
        assert_eq!(loaded.config.tool_hops_native, 9, "a valid sibling is kept");
        assert_eq!(loaded.warnings.len(), 2, "{:?}", loaded.warnings);
        assert!(loaded.warnings.iter().any(|w| w.contains("tool_hops_text = 0")));
        assert!(loaded.warnings.iter().any(|w| w.contains("auto_continues = 0")));
    }

    #[test]
    fn summary_names_every_budget() {
        let s = HarnessConfig { tool_hops_text: 7, tool_hops_native: 40, auto_continues: 3, ..Default::default() }.summary();
        assert!(s.contains("7 text") && s.contains("40 native") && s.contains("3 auto-continue"), "{s}");
    }
}
