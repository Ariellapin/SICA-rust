//! Settings › Memory (v32) — what the agent remembers across sessions, and
//! how both memories are kept.
//!
//! The list is the long-term store (`memories/long-term.json`): one card
//! per fact, global ones first, then one group per project folder. Every
//! change is a request, and the list redraws from the `Memories` answer or
//! the `MemoriesChanged` push — the model's `remember` lands here live.
//!
//! The switches are `sica-settings/memory.toml`, written in place (other
//! keys and comments kept). The backend re-reads that file whenever it
//! needs it, so nothing here needs a restart.

use std::path::{Path, PathBuf};

use agents::long_term::MemoryConfig;
use protocol::{MemoryDump, Request};

use crate::app::App;
use crate::supervisor::UiCommand;
use crate::ui::icons::Icon;
use crate::ui::kit::{self, Level, Weight};

use super::{open_path, reveal_path, row, section};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    if app.memories.is_none() && app.be_state.running {
        // First look: load the list. `Some` right away so the next frame
        // does not ask again.
        app.memories = Some(Vec::new());
        app.send(UiCommand::SendRequest(Request::ListMemories));
    }

    section(ui, "How memory works");
    ui.add(
        egui::Label::new(kit::txt(
            "Every session keeps its own memory — a running summary and the key facts — \
             written in the background while it is idle and put back in front of the model \
             after a compaction, so long sessions keep their decisions. Open it from the \
             Memory button in the conversation header.\n\n\
             Long-term memories are single facts that outlive a session. Global ones reach \
             every session; project ones only sessions working in that folder. The model \
             saves them with `remember` and deletes them with `forget`, the background pass \
             keeps durable facts it notices, and you can add, correct or delete them here.",
            12.0,
            Weight::Regular,
            kit::col(t.alias.label[2]),
        ))
        .wrap(),
    );

    switches(app, ui);
    long_term(app, ui);
    ui.add_space(24.0);
}

fn switches(app: &mut App, ui: &mut egui::Ui) {
    section(ui, "Behaviour");
    let path = MemoryConfig::path();
    let cfg = MemoryConfig::current();
    let mut change: Option<(&str, bool)> = None;
    let mut toggle = |ui: &mut egui::Ui, key: &'static str, title: &str, detail: String, on: bool| {
        let mut value = on;
        row(ui, title, &detail, |ui| {
            if ui.checkbox(&mut value, "").changed() {
                change = Some((key, value));
            }
        });
    };
    toggle(
        ui,
        "session_summary",
        "Keep session memories",
        format!(
            "Update a session's summary and key facts after it has been idle for {} s. Uses the \
             connected model only while nothing else does, and stops the moment a turn starts.",
            cfg.idle_seconds
        ),
        cfg.session_summary,
    );
    toggle(
        ui,
        "auto_remember",
        "Learn from sessions",
        "Let that background pass keep up to three durable facts at a time in long-term memory \
         (marked auto below)."
            .into(),
        cfg.auto_remember,
    );
    toggle(
        ui,
        "inject",
        "Use long-term memory",
        format!(
            "Show the memories that apply to a session at the start of each turn — at most {} of \
             them, {} characters.",
            cfg.prompt_items, cfg.prompt_chars
        ),
        cfg.inject,
    );
    if let Some((key, value)) = change {
        match set_toml_bool(&path, key, value) {
            Ok(()) => app.show_toast(Icon::Check, "Saved — applies from the next turn", 2500),
            Err(e) => app.show_toast(Icon::Warning, e, 6000),
        }
    }
    let open = path.clone();
    row(ui, "Configuration file", &path.display().to_string(), move |ui| {
        if kit::button(ui, "Open", kit::Variant::Ghost, kit::Size::Sm).clicked() {
            // The file is optional; reveal the folder when it is not there yet.
            let _ = if open.exists() { open_path(&open) } else { reveal_path(&open) };
        }
    });
}

fn long_term(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    section(ui, "Long-term memories");
    let folder = app.session_workspace().1;

    // Add one.
    let mut add = false;
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let field = egui::TextEdit::singleline(&mut app.memory_ui.new_text)
            .hint_text("One fact, in a sentence")
            .desired_width((ui.available_width() - 250.0).max(160.0));
        let resp = ui.add(field);
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            add = true;
        }
        if kit::pill(ui, "This project", !app.memory_ui.new_global)
            .interact(egui::Sense::click())
            .on_hover_text(folder.display().to_string())
            .clicked()
        {
            app.memory_ui.new_global = false;
        }
        if kit::pill(ui, "Global", app.memory_ui.new_global)
            .interact(egui::Sense::click())
            .on_hover_text("Every session, whatever folder it works in")
            .clicked()
        {
            app.memory_ui.new_global = true;
        }
        let can = !app.memory_ui.new_text.trim().is_empty() && app.be_state.running;
        if kit::button_enabled(ui, "Add", kit::Variant::Outline, kit::Size::Sm, can).clicked() {
            add = true;
        }
    });
    if add && !app.memory_ui.new_text.trim().is_empty() {
        let text = std::mem::take(&mut app.memory_ui.new_text);
        let project = (!app.memory_ui.new_global).then(|| folder.clone());
        app.send(UiCommand::SendRequest(Request::SaveMemory { id: None, text, project }));
    }
    ui.add_space(10.0);

    let memories = app.memories.clone().unwrap_or_default();
    if memories.is_empty() {
        kit::footnote(ui, "Nothing remembered yet.");
    } else {
        // Global first, then one group per folder in the order the backend
        // sorted them; the active session's folder is marked.
        let mut groups: Vec<(Option<PathBuf>, Vec<&MemoryDump>)> = Vec::new();
        for m in &memories {
            match groups.iter_mut().find(|(p, _)| p == &m.project) {
                Some((_, v)) => v.push(m),
                None => groups.push((m.project.clone(), vec![m])),
            }
        }
        for (project, items) in groups {
            let heading = match &project {
                None => "Global — every session".to_string(),
                Some(p) => {
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| p.display().to_string());
                    if same_folder(p, &folder) {
                        format!("{name} — this session's folder")
                    } else {
                        name
                    }
                }
            };
            ui.add_space(6.0);
            let head = kit::label(ui, kit::txt(heading, 13.0, Weight::Medium, kit::col(t.alias.label[1])));
            if let Some(p) = &project {
                head.on_hover_text(p.display().to_string());
            }
            ui.add_space(4.0);
            for m in items {
                if let Some(req) = memory_card(app, ui, m) {
                    app.send(UiCommand::SendRequest(req));
                }
                ui.add_space(6.0);
            }
        }
    }

    ui.add_space(8.0);
    kit::hairline(ui, Level::L1);
    let store = sica_core::paths::long_term_memory_file();
    row(ui, "Memory file", &store.display().to_string(), move |ui| {
        if kit::button(ui, "Show", kit::Variant::Ghost, kit::Size::Sm).clicked() {
            // Nothing remembered yet means no file; show where it will be.
            let _ = if store.exists() {
                reveal_path(&store)
            } else {
                open_path(store.parent().unwrap_or(&store))
            };
        }
    });
}

/// One memory. Returns the change the person asked for, if any.
fn memory_card(app: &mut App, ui: &mut egui::Ui, m: &MemoryDump) -> Option<Request> {
    let t = app.theme;
    let mut out = None;
    kit::card_frame(&t).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let editing = app.memory_ui.editing.as_ref().is_some_and(|(id, _)| id == &m.id);
        if editing {
            let mut save = false;
            let mut cancel = false;
            if let Some((_, draft)) = app.memory_ui.editing.as_mut() {
                ui.add(egui::TextEdit::multiline(draft).desired_rows(2).desired_width(f32::INFINITY));
                ui.horizontal(|ui| {
                    save = kit::button(ui, "Save", kit::Variant::Primary, kit::Size::Sm).clicked();
                    cancel = kit::button(ui, "Cancel", kit::Variant::Ghost, kit::Size::Sm).clicked();
                });
                if save && !draft.trim().is_empty() {
                    out = Some(Request::SaveMemory {
                        id:      Some(m.id.clone()),
                        text:    draft.trim().to_string(),
                        project: m.project.clone(),
                    });
                }
            }
            if save || cancel {
                app.memory_ui.editing = None;
            }
            return;
        }
        ui.add(
            egui::Label::new(kit::txt(&m.text, 13.0, Weight::Regular, kit::col(t.alias.label[0]))).wrap(),
        );
        ui.horizontal(|ui| {
            let source = match m.source.as_str() {
                "model" => "saved by the model",
                "user" => "added by you",
                "auto" => "learned automatically",
                other => other,
            };
            let mut meta = format!("{} · {source}", m.id);
            if let Some(date) = chrono::DateTime::from_timestamp(m.updated_at, 0) {
                meta.push_str(&format!(" · {}", date.with_timezone(&chrono::Local).format("%Y-%m-%d")));
            }
            if let Some(s) = m.session {
                meta.push_str(&format!(" · session {s}"));
            }
            kit::label(ui, kit::txt(meta, 11.0, Weight::Regular, kit::col(t.alias.label[3])));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if kit::button(ui, "Delete", kit::Variant::Danger, kit::Size::Sm).clicked() {
                    out = Some(Request::DeleteMemory { id: m.id.clone() });
                }
                if kit::button(ui, "Edit", kit::Variant::Ghost, kit::Size::Sm).clicked() {
                    app.memory_ui.editing = Some((m.id.clone(), m.text.clone()));
                }
            });
        });
    });
    out
}

/// Whether two folders are the same, the way the backend compares them:
/// separators unified, no trailing separator, case-folded on Windows.
fn same_folder(a: &Path, b: &Path) -> bool {
    let key = |p: &Path| {
        let s = p.to_string_lossy().replace('/', "\\");
        let s = s.trim_end_matches('\\').to_string();
        if cfg!(windows) {
            s.to_lowercase()
        } else {
            s
        }
    };
    key(a) == key(b)
}

/// Set `key = value` in a TOML file, keeping every other line — comments
/// included — as the person wrote it. Creates the file when it is absent.
fn set_toml_bool(path: &Path, key: &str, value: bool) -> Result<(), String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut out = String::with_capacity(text.len() + 32);
    let mut written = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let is_key = trimmed
            .strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='));
        if is_key && !written {
            out.push_str(&format!("{key} = {value}"));
            written = true;
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if !written {
        out.push_str(&format!("{key} = {value}\n"));
    }
    // A write that would leave the file unreadable is refused: the backend
    // would fall back to the defaults and the switch would lie.
    toml::from_str::<MemoryConfig>(&out).map_err(|e| format!("memory.toml would not parse: {e}"))?;
    sica_core::atomic::atomic_write(path, out.as_bytes()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_switch_rewrites_its_line_and_keeps_the_rest() {
        let dir = std::env::temp_dir().join(format!("sica-memory-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memory.toml");

        // Absent file: created with just the key.
        set_toml_bool(&path, "inject", false).unwrap();
        assert!(!MemoryConfig::load_from(&path).0.inject);

        std::fs::write(&path, "# mine\nidle_seconds = 10\ninject = false\n").unwrap();
        set_toml_bool(&path, "inject", true).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# mine") && text.contains("idle_seconds = 10"), "{text}");
        assert_eq!(text.matches("inject").count(), 1, "{text}");
        let cfg = MemoryConfig::load_from(&path).0;
        assert!(cfg.inject && cfg.idle_seconds == 10);

        // A key that merely starts with the name is another key.
        std::fs::write(&path, "session_summary_x = 1\n").unwrap();
        set_toml_bool(&path, "session_summary", false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("session_summary_x = 1") && text.contains("session_summary = false"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folders_compare_like_the_backend_compares_them() {
        if cfg!(windows) {
            assert!(same_folder(Path::new("C:\\Work\\Proj\\"), Path::new("C:/work/proj")));
        }
        assert!(!same_folder(Path::new("C:\\Work\\Proj"), Path::new("C:\\Work\\Other")));
    }
}
