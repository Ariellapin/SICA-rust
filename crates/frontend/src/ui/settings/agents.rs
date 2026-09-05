//! Settings › Agents (§7.2) — the roster of `agents/*.md` presets.
//!
//! Every preset here is **a file the user owns**. dsh ships read-only
//! presets and a Creator-mode add card; neither has a counterpart, because
//! there is nothing in this app that a user may not edit. So the actions are
//! the ones that make sense for files: show where it is, copy it, delete it,
//! and pick which one a new session starts with.
//!
//! A file that fails to parse still gets a card. It is on disk, the user put
//! it there, and hiding it would leave them with a preset that does not
//! appear anywhere and does not work — the card says why, and only *Show
//! location* and *Delete* are live on it.
//!
//! Selection itself is not here: a session's preset is fixed once the
//! session has produced anything (harness §5.2), so this section sets the
//! **default for the next session** and reports which preset the current one
//! is running.

use std::path::PathBuf;

use sica_core::paths::agents_dir;

use crate::app::App;
use crate::ui::kit::{self, Level, Weight};

use super::{open_path, row, section};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    let dir = agents_dir();
    let (presets, errors) = agents::preset::load_dir(&dir);

    ui.add_space(10.0);
    kit::label(
        ui,
        kit::txt(
            "One markdown file per preset: the body becomes the persona section \
             of the system prompt, and a `skills:` list narrows what the agent \
             may call. A session's preset is fixed once it has produced \
             anything, so this picks what the *next* session starts with.",
            12.0,
            Weight::Regular,
            kit::col(t.alias.label[2]),
        ),
    );

    // Transient UI state lives in egui's temp store: none of it is worth a
    // field on `App`, and all of it should die with the modal.
    let arm_id = ui.id().with("agent_delete_arm");
    let dup_id = ui.id().with("agent_duplicate");
    let ident_id = ui.id().with("agent_duplicate_ident");
    let mut armed: Option<String> = ui.ctx().data(|d| d.get_temp(arm_id).unwrap_or(None));
    let duplicating: Option<String> = ui.ctx().data(|d| d.get_temp(dup_id).unwrap_or(None));

    let mut set_default: Option<Option<String>> = None;
    let mut delete: Option<PathBuf> = None;
    let mut duplicate: Option<String> = None;

    section(ui, "Presets");
    if presets.is_empty() && errors.is_empty() {
        ui.add_space(8.0);
        kit::label(
            ui,
            kit::txt(
                format!("No presets yet. Files go in {}", dir.display()),
                12.0,
                Weight::Regular,
                kit::col(t.alias.label[3]),
            ),
        );
    }

    for p in &presets {
        let in_use = app.session_agent.as_deref() == Some(p.name.as_str());
        let is_default = app.default_agent.as_deref() == Some(p.name.as_str());
        let skills = if p.skills.is_empty() {
            "every skill".to_string()
        } else {
            format!("{} skill{}", p.skills.len(), if p.skills.len() == 1 { "" } else { "s" })
        };
        let detail = format!(
            "{}{} · {skills}",
            p.description,
            if in_use { " · in use by this session" } else { "" }
        );
        let name = p.name.clone();
        let path = p.source_path.clone();
        let is_armed = armed.as_deref() == Some(name.as_str());
        row(ui, &p.name, &detail, |ui| {
            if is_armed {
                if kit::button(ui, "Keep", kit::Variant::Ghost, kit::Size::Sm).clicked() {
                    armed = Some(String::new());
                }
                if kit::button(ui, "Delete", kit::Variant::Danger, kit::Size::Sm)
                    .on_hover_text("Deletes the file. Sessions already running it keep their prompt.")
                    .clicked()
                {
                    delete = Some(path.clone());
                }
                return;
            }
            if kit::button(ui, "Delete", kit::Variant::Ghost, kit::Size::Sm).clicked() {
                armed = Some(name.clone());
            }
            if kit::button(ui, "Duplicate", kit::Variant::Ghost, kit::Size::Sm).clicked() {
                duplicate = Some(name.clone());
            }
            if kit::button(ui, "Show location", kit::Variant::Ghost, kit::Size::Sm).clicked() {
                let _ = open_path(path.parent().unwrap_or(&path));
            }
            let label = if is_default { "Default" } else { "Set as default" };
            let variant = if is_default { kit::Variant::Primary } else { kit::Variant::Outline };
            if kit::button(ui, label, variant, kit::Size::Sm).clicked() {
                set_default = Some(if is_default { None } else { Some(name.clone()) });
            }
        });
    }

    for (path, reason) in &errors {
        let file = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let p = path.clone();
        let is_armed = armed.as_deref() == Some(file.as_str());
        row(ui, &format!("{file} — cannot be read"), reason, |ui| {
            if is_armed {
                if kit::button(ui, "Keep", kit::Variant::Ghost, kit::Size::Sm).clicked() {
                    armed = Some(String::new());
                }
                if kit::button(ui, "Delete", kit::Variant::Danger, kit::Size::Sm).clicked() {
                    delete = Some(p.clone());
                }
                return;
            }
            if kit::button(ui, "Delete", kit::Variant::Ghost, kit::Size::Sm).clicked() {
                armed = Some(file.clone());
            }
            if kit::button(ui, "Show location", kit::Variant::Ghost, kit::Size::Sm).clicked() {
                let _ = open_path(p.parent().unwrap_or(&p));
            }
        });
    }

    ui.add_space(10.0);
    kit::hairline(ui, Level::L1);
    section(ui, "Defaults");
    let has_default = app.default_agent.is_some();
    row(
        ui,
        "Default preset for new sessions",
        &match &app.default_agent {
            Some(n) => format!("New sessions start as {n}"),
            None => "New sessions start with no persona".to_string(),
        },
        |ui| {
            if has_default && kit::button(ui, "Clear", kit::Variant::Outline, kit::Size::Sm).clicked()
            {
                set_default = Some(None);
            }
        },
    );
    row(
        ui,
        "Folder",
        &dir.display().to_string(),
        |ui| {
            if kit::button(ui, "Open", kit::Variant::Ghost, kit::Size::Sm).clicked() {
                let _ = std::fs::create_dir_all(&dir);
                let _ = open_path(&dir);
            }
        },
    );

    // ----------------------------------------------------------- effects
    if let Some(choice) = set_default {
        app.default_agent = choice;
        app.persist_settings();
    }
    if let Some(path) = delete {
        armed = Some(String::new());
        match std::fs::remove_file(&path) {
            Ok(()) => {
                // A preset the *current* session is running stays in force
                // until that session ends — the prompt prefix is already
                // written into its transcript.
                if app.default_agent.as_deref()
                    == path.file_stem().and_then(|s| s.to_str())
                {
                    app.default_agent = None;
                    app.persist_settings();
                }
                app.show_toast(crate::ui::icons::Icon::Check, "Preset deleted", 2500);
            }
            Err(e) => app.show_toast(crate::ui::icons::Icon::Warning, e.to_string(), 6000),
        }
    }
    if let Some(name) = duplicate {
        ui.ctx().data_mut(|d| {
            d.insert_temp(dup_id, Some(name.clone()));
            d.insert_temp(ident_id, format!("{name}-copy"));
        });
    }
    ui.ctx().data_mut(|d| d.insert_temp(arm_id, armed));

    if let Some(source) = duplicating {
        duplicate_dialog(app, ui, &source, dup_id, ident_id, &presets);
    }
    ui.add_space(24.0);
}

/// "Copy preset" — the identifier *is* the filename, so it is validated
/// before anything is written: required, filename-safe, and not already
/// taken. dsh's three messages, for the three ways it can be wrong.
fn duplicate_dialog(
    app: &mut App,
    ui: &mut egui::Ui,
    source: &str,
    dup_id: egui::Id,
    ident_id: egui::Id,
    presets: &[agents::preset::AgentPreset],
) {
    let t = app.theme;
    let mut ident: String = ui.ctx().data(|d| d.get_temp(ident_id).unwrap_or_default());
    let mut create = false;
    let mut cancel = false;
    let problem = ident_problem(&ident, presets);

    let out = kit::modal(
        ui.ctx(),
        egui::Id::new("agent_duplicate_modal"),
        "Copy preset",
        420.0,
        true,
        |ui| {
            kit::label(
                ui,
                kit::txt(
                    format!("Copied from {source}"),
                    12.0,
                    Weight::Regular,
                    kit::col(t.alias.label[2]),
                ),
            );
            ui.add_space(10.0);
            kit::label(
                ui,
                kit::txt("Identifier", 12.0, Weight::Medium, kit::col(t.alias.label[1])),
            );
            let field = egui::TextEdit::singleline(&mut ident)
                .hint_text("my-agent")
                .desired_width(ui.available_width());
            if ui.add(field).changed() {
                ui.ctx().data_mut(|d| d.insert_temp(ident_id, ident.clone()));
            }
            ui.add_space(4.0);
            kit::label(
                ui,
                kit::txt(
                    problem.clone().unwrap_or_else(|| format!("Saves as {ident}.md")),
                    11.0,
                    Weight::Regular,
                    kit::col(if problem.is_some() { t.alias.error } else { t.alias.label[3] }),
                ),
            );
            ui.add_space(14.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let ok = kit::button_enabled(
                    ui,
                    "Create",
                    kit::Variant::Primary,
                    kit::Size::Md,
                    problem.is_none(),
                );
                if ok.clicked() {
                    create = true;
                }
                if kit::button(ui, "Cancel", kit::Variant::Ghost, kit::Size::Md).clicked() {
                    cancel = true;
                }
            });
        },
    );

    if create {
        let dir = agents_dir();
        let from = dir.join(format!("{source}.md"));
        let to = dir.join(format!("{ident}.md"));
        match copy_preset(&from, &to, &ident) {
            Ok(()) => app.show_toast(
                crate::ui::icons::Icon::Check,
                format!("Created {ident}.md"),
                2500,
            ),
            Err(e) => app.show_toast(crate::ui::icons::Icon::Warning, e, 6000),
        }
    }
    if create || cancel || out.dismissed {
        ui.ctx().data_mut(|d| {
            d.insert_temp::<Option<String>>(dup_id, None);
            d.insert_temp(ident_id, String::new());
        });
    }
}

/// Copy the file, rewriting the `name:` line so the copy is addressable by
/// its own identifier — the frontmatter name is what `/name` resolves and
/// what the roster shows, so a copy that kept the original's name would be
/// two files claiming to be the same preset.
fn copy_preset(from: &std::path::Path, to: &std::path::Path, ident: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(from).map_err(|e| e.to_string())?;
    let mut out = String::with_capacity(text.len() + 16);
    let mut renamed = false;
    for line in text.lines() {
        if !renamed && line.trim_start().starts_with("name:") {
            out.push_str(&format!("name: {ident}"));
            renamed = true;
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    sica_core::atomic::atomic_write(to, out.as_bytes()).map_err(|e| e.to_string())
}

/// Why this identifier cannot be used, or `None` when it can.
fn ident_problem(ident: &str, presets: &[agents::preset::AgentPreset]) -> Option<String> {
    let trimmed = ident.trim();
    if trimmed.is_empty() {
        return Some("Identifier is required".into());
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Some("Identifier is invalid — letters, digits, - and _ only".into());
    }
    if presets.iter().any(|p| p.name == trimmed)
        || agents_dir().join(format!("{trimmed}.md")).exists()
    {
        return Some("Identifier is taken".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identifier_must_be_present_safe_and_free() {
        let presets: Vec<agents::preset::AgentPreset> = Vec::new();
        assert_eq!(ident_problem("", &presets).as_deref(), Some("Identifier is required"));
        assert_eq!(ident_problem("   ", &presets).as_deref(), Some("Identifier is required"));
        // A filename is what this becomes, so a separator in it is not a
        // naming preference — it is a path.
        assert!(ident_problem("../evil", &presets).is_some());
        assert!(ident_problem("a/b", &presets).is_some());
        assert!(ident_problem("with space", &presets).is_some());
        assert!(ident_problem("my-agent_2", &presets).is_none());
    }

    #[test]
    fn a_copy_is_addressable_by_its_own_name() {
        let dir = std::env::temp_dir().join(format!("sica-agents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let from = dir.join("reviewer.md");
        std::fs::write(
            &from,
            "---\nname: reviewer\ndescription: reads code\n---\nBody stays.\n",
        )
        .unwrap();
        let to = dir.join("auditor.md");
        copy_preset(&from, &to, "auditor").unwrap();
        let text = std::fs::read_to_string(&to).unwrap();
        assert!(text.contains("name: auditor"), "{text}");
        assert!(!text.contains("name: reviewer"));
        assert!(text.contains("description: reads code"));
        assert!(text.contains("Body stays."));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
