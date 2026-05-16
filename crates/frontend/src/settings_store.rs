//! Disk-backed user settings. Loaded at startup, written on Apply.

use std::fs;

use serde::{Deserialize, Serialize};

use sica_core::paths::settings_file;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub theme_dark:             bool,
    pub log_raw_llm:            bool,
    pub idealist_auto_apply_be: bool,
    pub llm_base_url:           String,
    pub llm_model:              String,
    pub auto_start_be:          bool,
    pub auto_connect_llm:       bool,
    pub autoscroll:             bool,
    pub release_profile:        bool,
    pub auto_watch:             bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme_dark:             true,
            log_raw_llm:            false,
            idealist_auto_apply_be: false,
            llm_base_url:           "http://localhost:8080".into(),
            llm_model:              "local".into(),
            auto_start_be:          true,
            auto_connect_llm:       true,
            autoscroll:             true,
            release_profile:        false,
            auto_watch:             false,
        }
    }
}

pub fn load() -> Settings {
    let path = settings_file();
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

pub fn save(s: &Settings) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(s)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    fs::write(settings_file(), text)
}
