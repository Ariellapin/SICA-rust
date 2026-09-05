//! Settings › Integrations (§7.2) — the optional, opt-in files.
//!
//! Every integration this app has is **a file that is absent by default**:
//! an MCP server per TOML, `.sica/hooks.json`, `web.toml`, and the two
//! skills that only register when their markdown exists. That is the whole
//! organising idea of the section, and dsh's "Overridden / Reset to default"
//! pair has no meaning here — a card *is* its file, so the card reads it,
//! writes it, and can open it.
//!
//! Status is derived rather than asked for. The frontend and backend always
//! share a machine, so the file half is read straight from disk; the *live*
//! half comes from the catalogue the backend already sends — an MCP server
//! that started has its tools in it, under `mcp__<server>__<tool>`. That
//! saves a request, and it cannot disagree with what the model can actually
//! call, because it is the same list.
//!
//! Almost everything here needs the backend restarted to take effect: the
//! registry is built once at startup. Rows say so rather than pretending
//! otherwise (dsh's `applies: restart` badge).

use std::path::{Path, PathBuf};

use egui::Layout;

use sica_core::creds;
use sica_core::paths::{skills_dir, workspace_root};

use crate::app::App;
use crate::ui::kit::{self, Level, Weight};

use super::{open_path, row, section};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    mcp_servers(app, ui);
    hooks(app, ui);
    web_search(app, ui);
    optional_skills(app, ui);
    ui.add_space(24.0);
}

/// A short "restart the backend" note, for the rows where it is true.
fn restart_note(ui: &mut egui::Ui, t: &sica_core::theme::Theme) {
    kit::label(
        ui,
        kit::txt(
            "Takes effect when the backend restarts",
            11.0,
            Weight::Regular,
            kit::col(t.alias.label[3]),
        ),
    );
}

// ---------------------------------------------------------------------------
// MCP
// ---------------------------------------------------------------------------

struct McpRow {
    name:    String,
    path:    PathBuf,
    command: String,
    enabled: bool,
    /// Tools this server contributed to the catalogue. `0` with `enabled`
    /// means it did not start, or started with nothing to offer.
    tools:   usize,
    /// The file could not be parsed; the reason is the card's whole story.
    error:   Option<String>,
}

fn mcp_rows(app: &App) -> Vec<McpRow> {
    let dir = agents::mcp::config_dir();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else { return out };
    let mut paths: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("toml"))
        .collect();
    paths.sort();
    for path in paths {
        let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        let prefix = format!("mcp__{name}__");
        let tools = app
            .chat
            .slash
            .entries
            .iter()
            .filter(|e| e.name.starts_with(&prefix))
            .count();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        match toml::from_str::<agents::mcp::ServerConfig>(&text) {
            Ok(cfg) => out.push(McpRow {
                name,
                path,
                command: format!("{} {}", cfg.command, cfg.args.join(" ")).trim().to_string(),
                enabled: cfg.enabled,
                tools,
                error: None,
            }),
            Err(e) => out.push(McpRow {
                name,
                path,
                command: String::new(),
                enabled: false,
                tools,
                error: Some(e.to_string()),
            }),
        }
    }
    out
}

fn mcp_servers(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    section(ui, "MCP servers");
    let dir = agents::mcp::config_dir();
    let rows = mcp_rows(app);

    if rows.is_empty() {
        ui.add_space(8.0);
        kit::label(
            ui,
            kit::txt(
                format!(
                    "None configured. One TOML per server under {} — `command`, \
                     `args`, `env`, `cwd`, `enabled` — each bridged as \
                     mcp__<server>__<tool>.",
                    dir.display()
                ),
                12.0,
                Weight::Regular,
                kit::col(t.alias.label[2]),
            ),
        );
    }

    let mut toggled: Option<(PathBuf, bool)> = None;
    for r in &rows {
        let detail = match &r.error {
            Some(e) => format!("unreadable — {e}"),
            None if !r.enabled => format!("{} · disabled", r.command),
            None if r.tools > 0 => format!(
                "{} · started, {} tool{}",
                r.command,
                r.tools,
                if r.tools == 1 { "" } else { "s" }
            ),
            None => format!("{} · no tools registered — see the log for why", r.command),
        };
        let mut enabled = r.enabled;
        let path = r.path.clone();
        let broken = r.error.is_some();
        row(ui, &r.name, &detail, |ui| {
            if kit::button(ui, "Open", kit::Variant::Ghost, kit::Size::Sm).clicked() {
                let _ = open_path(&path);
            }
            if !broken && ui.checkbox(&mut enabled, "").changed() {
                toggled = Some((path.clone(), enabled));
            }
        });
    }
    if let Some((path, enabled)) = toggled {
        match set_toml_enabled(&path, enabled) {
            // The file is the source of truth, so the toggle rewrites it
            // rather than keeping a second copy of the answer in the UI.
            Ok(()) => app.show_toast(
                crate::ui::icons::Icon::Check,
                "Saved — restart the backend to apply",
                3000,
            ),
            Err(e) => app.show_toast(crate::ui::icons::Icon::Warning, e, 6000),
        }
    }
    if !rows.is_empty() {
        ui.add_space(6.0);
        restart_note(ui, &t);
    }
    ui.add_space(10.0);
    kit::hairline(ui, Level::L1);
}

/// Flip `enabled` in a server's TOML, preserving everything else in it.
///
/// The rewrite is line-based on purpose: re-serialising the parsed struct
/// would drop the user's comments and their key order, and this file is
/// hand-written.
fn set_toml_enabled(path: &Path, enabled: bool) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut out = String::with_capacity(text.len() + 16);
    let mut written = false;
    for line in text.lines() {
        if line.trim_start().starts_with("enabled") && line.contains('=') {
            out.push_str(&format!("enabled = {enabled}"));
            written = true;
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if !written {
        out.push_str(&format!("enabled = {enabled}\n"));
    }
    sica_core::atomic::atomic_write(path, out.as_bytes()).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

fn hooks(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    section(ui, "Hooks");
    // Discovered under the *working* directory, so a project brings its own.
    let path = sica_core::paths::working_dir().join(".sica").join("hooks.json");
    let summary = match std::fs::read_to_string(&path) {
        Ok(body) => hook_summary(&body),
        Err(_) => format!("No file at {}", path.display()),
    };
    let open_path_buf = path.clone();
    row(
        ui,
        ".sica/hooks.json",
        &summary,
        |ui| {
            if kit::button(ui, "Open", kit::Variant::Outline, kit::Size::Sm).clicked() {
                let _ = open_path(&open_path_buf);
            }
        },
    );
    ui.add_space(6.0);
    kit::label(
        ui,
        kit::txt(
            "Read once at backend start, in Claude Code's schema. A hook that \
             fails to spawn, times out or writes non-JSON abstains — a broken \
             script must not become a permission decision.",
            11.0,
            Weight::Regular,
            kit::col(t.alias.label[3]),
        ),
    );
    ui.add_space(10.0);
    kit::hairline(ui, Level::L1);
    let _ = app;
}

// ---------------------------------------------------------------------------
// Web search
// ---------------------------------------------------------------------------

const PROVIDERS: [&str; 3] = ["brave", "exa", "tavily"];

fn web_search(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    section(ui, "Web search");
    let path = agents::web::config_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let cfg: toml::Value = toml::from_str(&text).unwrap_or(toml::Value::Table(Default::default()));
    let provider = cfg
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("brave")
        .to_string();
    let stored_key = cfg.get("api_key").and_then(|v| v.as_str()).unwrap_or("");

    // The key is **write-only** (§14.6): the card says whether one is
    // configured and where it came from, never what it is. A value like
    // `${BRAVE_API_KEY}` is a reference resolved per request, so "not set"
    // here means the variable is unset, not that the file is empty.
    let status = key_status(stored_key);

    let mut picked: Option<&str> = None;
    let current = provider.clone();
    row(ui, "Provider", &status, |ui| {
        ui.with_layout(Layout::left_to_right(egui::Align::Center), |ui| {
            for p in PROVIDERS {
                if kit::pill(ui, p, p == current).clicked() {
                    picked = Some(p);
                }
            }
        });
    });

    let draft_id = ui.id().with("web_key_draft");
    let mut draft: String = ui.ctx().data(|d| d.get_temp(draft_id).unwrap_or_default());
    let mut save = false;
    row(
        ui,
        "API key",
        "Stored in sica-settings/web.toml. Enter ${VAR} to keep the secret in \
         the environment instead — it is re-read on every search, so rotating \
         it needs no restart.",
        |ui| {
            if kit::button(ui, "Save", kit::Variant::Outline, kit::Size::Sm).clicked() {
                save = true;
            }
            // A `${VAR}` reference is a name, not a secret, so it stays
            // readable while it is typed.
            let masked = !draft.starts_with("${");
            let field = egui::TextEdit::singleline(&mut draft)
                .password(masked)
                .hint_text("key or ${VAR}")
                .desired_width(200.0);
            if ui.add(field).changed() {
                ui.ctx().data_mut(|d| d.insert_temp(draft_id, draft.clone()));
            }
        },
    );

    if picked.is_some() || save {
        let provider = picked.unwrap_or(&provider).to_string();
        let key = if save && !draft.trim().is_empty() {
            draft.trim().to_string()
        } else {
            stored_key.to_string()
        };
        let endpoint = cfg.get("endpoint").and_then(|v| v.as_str()).unwrap_or("");
        let mut body = format!("provider = {provider:?}\napi_key  = {key:?}\n");
        if !endpoint.is_empty() {
            body.push_str(&format!("endpoint = {endpoint:?}\n"));
        }
        match sica_core::atomic::atomic_write(&path, body.as_bytes()) {
            Ok(()) => {
                if save {
                    ui.ctx().data_mut(|d| d.insert_temp(draft_id, String::new()));
                }
                app.show_toast(crate::ui::icons::Icon::Check, "web.toml saved", 2500);
            }
            Err(e) => app.show_toast(crate::ui::icons::Icon::Warning, e.to_string(), 6000),
        }
    }

    let open = path.clone();
    row(ui, "Configuration file", &path.display().to_string(), move |ui| {
        if kit::button(ui, "Open", kit::Variant::Ghost, kit::Size::Sm).clicked() {
            let _ = open_path(&open);
        }
    });
    ui.add_space(10.0);
    kit::hairline(ui, Level::L1);
    let _ = t;
}

// ---------------------------------------------------------------------------
// Opt-in skills
// ---------------------------------------------------------------------------

/// The two skills that are off until a doc turns them on. Both spend many
/// LLM conversations per call, and `workflow` also adds its scripting
/// reference to every system prompt while it is on — so the switch is the
/// presence of the file, and turning it off renames rather than deletes.
const OPTIONAL: [(&str, &str); 2] = [
    (
        "workflow",
        "Model-written orchestration scripts (harness §12.5). One call can \
         spend 32 child conversations, and the scripting reference costs \
         ~575 tokens on every request while it is on.",
    ),
    (
        "agent-team",
        "A named team of children answering one brief (harness §12.7). Each \
         call is several full conversations.",
    ),
];

fn optional_skills(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    section(ui, "Optional skills");
    let dir = skills_dir();
    let mut change: Option<(PathBuf, PathBuf)> = None;
    for (name, detail) in OPTIONAL {
        let on_path = dir.join(format!("{name}.md"));
        let off_path = dir.join(format!("{name}.md.off"));
        let present = on_path.exists();
        let available = present || off_path.exists();
        let mut enabled = present;
        let detail = if available {
            detail.to_string()
        } else {
            format!("{detail} No skills/{name}.md in this workspace.")
        };
        row(ui, name, &detail, |ui| {
            if !available {
                kit::label(
                    ui,
                    kit::txt("not installed", 12.0, Weight::Regular, kit::col(t.alias.label[3])),
                );
                return;
            }
            if ui.checkbox(&mut enabled, "").changed() {
                change = Some(if enabled {
                    (off_path.clone(), on_path.clone())
                } else {
                    (on_path.clone(), off_path.clone())
                });
            }
        });
    }
    if let Some((from, to)) = change {
        match std::fs::rename(&from, &to) {
            Ok(()) => app.show_toast(
                crate::ui::icons::Icon::Check,
                "Saved — restart the backend to apply",
                3000,
            ),
            Err(e) => app.show_toast(crate::ui::icons::Icon::Warning, e.to_string(), 6000),
        }
    }
    ui.add_space(6.0);
    restart_note(ui, &t);
    ui.add_space(4.0);
    kit::label(
        ui,
        kit::txt(
            format!("Files live in {}", workspace_root().join("skills").display()),
            11.0,
            Weight::Regular,
            kit::col(t.alias.label[3]),
        ),
    );
}

/// One line describing what a hooks file holds: the events it matches and
/// how many commands each has. Never the commands themselves — this card is
/// a status, and the file is one click away.
fn hook_summary(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return "unreadable — not valid JSON".to_string();
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(map) = v.get("hooks").and_then(|h| h.as_object()) {
        for (event, entries) in map {
            let n = entries
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|m| {
                            m.get("hooks").and_then(|h| h.as_array()).map(Vec::len).unwrap_or(1)
                        })
                        .sum::<usize>()
                })
                .unwrap_or(0);
            parts.push(format!("{event} ({n})"));
        }
    }
    if parts.is_empty() {
        "The file has no hooks in it".to_string()
    } else {
        parts.join(" · ")
    }
}

/// What the card says about a configured key — **never the key** (§14.6).
/// A `${VAR}` reference reports where the value came from, or names the
/// variable that still has to be set.
fn key_status(stored: &str) -> String {
    match creds::describe(stored) {
        creds::Status::Absent => "Not set".to_string(),
        creds::Status::Configured(creds::Source::Literal) => "Configured · in file".to_string(),
        creds::Status::Configured(creds::Source::Env(name)) => {
            format!("Configured · environment ({name})")
        }
        creds::Status::Configured(creds::Source::File(name)) => {
            format!("Configured · sica-settings/.env ({name})")
        }
        creds::Status::Unresolved(name) => format!("{name} is not set"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sica-int-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The card writes the file the user hand-wrote, so the toggle has to
    /// leave everything it did not change alone — comments included.
    #[test]
    fn toggling_a_server_preserves_the_rest_of_its_file() {
        let dir = temp_dir("toggle");
        let path = dir.join("fs.toml");
        let original = "# my notes
command = \"npx\"
args = [\"-y\", \"pkg\"]
enabled = true
";
        std::fs::write(&path, original).unwrap();

        set_toml_enabled(&path, false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my notes"), "a comment was lost: {text}");
        assert!(text.contains("args = [\"-y\", \"pkg\"]"));
        assert!(text.contains("enabled = false"));
        assert_eq!(text.matches("enabled").count(), 1, "no duplicate key");

        // And back, which is what the checkbox does.
        set_toml_enabled(&path, true).unwrap();
        let cfg: agents::mcp::ServerConfig =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.command, "npx");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file that never mentioned `enabled` relied on the default; writing
    /// the key is the only way to say "off".
    #[test]
    fn toggling_a_file_without_the_key_appends_it() {
        let dir = temp_dir("append");
        let path = dir.join("fs.toml");
        std::fs::write(&path, "command = \"npx\"
").unwrap();
        set_toml_enabled(&path, false).unwrap();
        let cfg: agents::mcp::ServerConfig =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(!cfg.enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_hooks_card_counts_commands_per_event() {
        let body = r#"{"hooks":{"PreToolUse":[{"matcher":"write-file","hooks":[
            {"type":"command","command":"a"},{"type":"command","command":"b"}]}]}}"#;
        assert_eq!(hook_summary(body), "PreToolUse (2)");
        assert_eq!(hook_summary("{}"), "The file has no hooks in it");
        assert!(hook_summary("{not json").starts_with("unreadable"));
        // The commands themselves are never in the summary — the file is one
        // click away and this is a status line.
        assert!(!hook_summary(body).contains('a'));
    }

    /// The card must never be able to print the key. Every branch answers
    /// with a state, and the reference branch names only the variable.
    #[test]
    fn the_key_status_never_reveals_the_key() {
        assert_eq!(key_status(""), "Not set");
        assert_eq!(key_status("   "), "Not set");
        let secret = "sk-do-not-print-me";
        assert_eq!(key_status(secret), "Configured · in file");
        assert!(!key_status(secret).contains(secret));

        let name = format!("SICA_INT_TEST_{}", std::process::id());
        assert_eq!(key_status(&format!("${{{name}}}")), format!("{name} is not set"));
        std::env::set_var(&name, "value");
        let status = key_status(&format!("${{{name}}}"));
        assert_eq!(status, format!("Configured · environment ({name})"));
        assert!(!status.contains("value"));
        std::env::remove_var(&name);
    }
}
