//! Disk-backed user settings. Loaded at startup, written on Apply.

use std::fs;

use serde::{Deserialize, Serialize};

use sica_core::paths::settings_file;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Legacy light/dark flag, still written so an older build reads the
    /// same preference. `theme_mode` is the authority.
    pub theme_dark:             bool,
    /// `light | dark | system` — the Appearance cubes (§7.1).
    #[serde(default = "default_theme_mode")]
    pub theme_mode:             String,
    /// Conversation content size, 12..=17. Chrome type never follows it.
    #[serde(default = "default_content_px")]
    pub content_px:             u8,
    /// Conversation display: `false` = Normal (every row), `true` = Compact
    /// (closed turns fold their process rows behind one button).
    #[serde(default)]
    pub transcript_compact:     bool,
    /// What plain Enter does while a turn runs: `queue | steer`.
    #[serde(default = "default_busy_enter")]
    pub busy_enter:             String,
    /// Freeze the ambient animations (shimmer, sweep, dot chase).
    #[serde(default)]
    pub reduce_motion:          bool,
    /// User override of the conversation content width (680..=920).
    #[serde(default)]
    pub chat_content_width:     Option<f32>,
    pub log_raw_llm:            bool,
    pub idealist_auto_apply_be: bool,
    pub auto_start_be:          bool,
    pub auto_connect_llm:       bool,
    pub autoscroll:             bool,
    pub release_profile:        bool,
    pub auto_watch:             bool,
    /// `id` (filename stem) of the provider panel that should auto-connect
    /// on IPC ready and that "Apply" should reconnect. `None` means "no
    /// provider was last active" — the app starts disconnected.
    #[serde(default)]
    pub last_active_provider:   Option<String>,
    /// Permission mode applied to every freshly minted session
    /// (`read-only | workspace-write | danger-full-access`). Hand-editable;
    /// the backend default matches when this fails to parse.
    #[serde(default = "default_permission_mode")]
    pub default_permission_mode: String,
    /// Folder the agent reads, writes and runs commands in. `None` means the
    /// app's own root (`paths::workspace_root`), which is what the app did
    /// before this setting existed.
    #[serde(default)]
    pub working_dir:            Option<String>,
    /// Previously chosen working directories, most recent first. The picker
    /// offers them so switching between projects is one click.
    #[serde(default)]
    pub recent_working_dirs:    Vec<String>,
    /// Sidebar grouping (§4.3): `workspace` groups sessions under the
    /// directory they work in, `flat` is the one list this app had before
    /// workspaces existed. Only the grouping is a preference — the order
    /// inside a workspace is the backend's, because it is durable there.
    #[serde(default = "default_sidebar_group")]
    pub sidebar_group:          String,
    /// Agent preset applied to every freshly minted session (§7.2). `None`
    /// is the persona-less prompt, which is what the app did before presets
    /// existed.
    #[serde(default)]
    pub default_agent:          Option<String>,
}

fn default_sidebar_group() -> String {
    "workspace".into()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme_dark:             true,
            theme_mode:             default_theme_mode(),
            content_px:             default_content_px(),
            transcript_compact:     false,
            busy_enter:             default_busy_enter(),
            reduce_motion:          false,
            chat_content_width:     None,
            log_raw_llm:            false,
            idealist_auto_apply_be: false,
            auto_start_be:          true,
            auto_connect_llm:       true,
            autoscroll:             true,
            release_profile:        false,
            auto_watch:             false,
            last_active_provider:   None,
            default_permission_mode: default_permission_mode(),
            working_dir:            None,
            recent_working_dirs:    Vec::new(),
            sidebar_group:          default_sidebar_group(),
            default_agent:          None,
        }
    }
}

fn default_theme_mode() -> String {
    "dark".into()
}

fn default_content_px() -> u8 {
    sica_core::theme::tokens::CONTENT_DEFAULT_PX
}

fn default_busy_enter() -> String {
    "queue".into()
}

fn default_permission_mode() -> String {
    "workspace-write".into()
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
