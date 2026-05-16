//! Per-provider LLM panel configs. One TOML file per provider lives under
//! `sica-settings/llm-providers/`. The filename stem becomes the provider `id`.

use std::fs;
use std::io;

use serde::{Deserialize, Serialize};

use sica_core::paths::llm_providers_dir;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(skip)]
    pub id: String,
    pub title: String,
    pub description: String,
    pub icon: String,
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: String,
}

/// Scan the providers directory and parse every `*.toml` file. Files that
/// fail to parse are skipped silently; the UI just won't show a panel for
/// them. Returns providers sorted by title for a stable on-screen order.
pub fn load_all() -> Vec<ProviderConfig> {
    let dir = llm_providers_dir();
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        match toml::from_str::<ProviderConfig>(&text) {
            Ok(mut cfg) => {
                cfg.id = stem.to_string();
                out.push(cfg);
            }
            Err(_) => continue,
        }
    }
    out.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    out
}

pub fn save(cfg: &ProviderConfig) -> io::Result<()> {
    let dir = llm_providers_dir();
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.toml", cfg.id));
    let text = toml::to_string_pretty(cfg)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    fs::write(path, text)
}

/// If the providers directory is empty (or missing), write three starter
/// files so the UI is non-empty on first launch.
pub fn seed_defaults_if_empty() -> io::Result<()> {
    let dir = llm_providers_dir();
    if dir.is_dir() {
        let has_any = fs::read_dir(&dir)?
            .flatten()
            .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("toml"));
        if has_any {
            return Ok(());
        }
    }
    fs::create_dir_all(&dir)?;
    for cfg in defaults() {
        save(&cfg)?;
    }
    Ok(())
}

fn defaults() -> Vec<ProviderConfig> {
    vec![
        ProviderConfig {
            id: "local".into(),
            title: "Local (llama.cpp)".into(),
            description: "Local OpenAI-compatible server (llama.cpp, ollama, vLLM).".into(),
            icon: "🖥".into(),
            base_url: "http://localhost:8080".into(),
            model: "local".into(),
            api_key: String::new(),
        },
        ProviderConfig {
            id: "openai".into(),
            title: "OpenAI".into(),
            description: "OpenAI GPT models via api.openai.com.".into(),
            icon: "🟢".into(),
            base_url: "https://api.openai.com".into(),
            model: "gpt-4o-mini".into(),
            api_key: String::new(),
        },
        ProviderConfig {
            id: "anthropic".into(),
            title: "Anthropic".into(),
            description: "Claude models via Anthropic's OpenAI-compatible endpoint.".into(),
            icon: "🟣".into(),
            base_url: "https://api.anthropic.com/v1".into(),
            model: "claude-sonnet-4-6".into(),
            api_key: String::new(),
        },
    ]
}
