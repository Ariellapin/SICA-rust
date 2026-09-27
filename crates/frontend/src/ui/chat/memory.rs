//! The session's own memory (v32): a header action and the panel it opens.
//!
//! The backend keeps each session's memory — a running summary and the key
//! facts worth keeping — up to date while the model is idle, and puts it
//! back in front of the model after a compaction. This is where a person
//! reads it, corrects it, asks for an update now, or keeps a fact for good
//! (a long-term memory about this project).
//!
//! The panel is `kit::modal`, the design system's one dialog shape. Nothing
//! here is optimistic: every action is a request, and the panel redraws from
//! the `SessionMemoryChanged` / `SessionMemoryUpdate` events that follow.

use egui::{Align, Align2, Layout, Rect, Sense, Vec2};
use protocol::Request;

use crate::app::{App, SettingsTab};
use crate::supervisor::UiCommand;
use crate::ui::icons::{self, Icon};
use crate::ui::kit::{self, Weight};

/// Header action (right side, beside reminders and jobs): `Memory`, with
/// the number of key facts once there are any, and a live dot while the
/// keeper is working on this session.
pub fn action(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    let label = match &app.session_memory {
        Some(m) if !m.facts.is_empty() => format!("Memory · {}", m.facts.len()),
        _ => "Memory".to_string(),
    };
    let font = kit::font(13.0, Weight::Medium);
    let galley = ui.fonts(|f| f.layout_no_wrap(label.clone(), font.clone(), egui::Color32::WHITE));
    let (rect, resp) = ui.allocate_exact_size(galley.size() + Vec2::new(36.0, 12.0), Sense::click());
    if resp.hovered() || app.memory_ui.open {
        ui.painter().rect_filled(
            rect,
            egui::Rounding::same(sica_core::theme::tokens::RADIUS_PILL),
            kit::cola(t.alias.hover),
        );
    }
    let glyph = Rect::from_center_size(egui::pos2(rect.min.x + 14.0, rect.center().y), Vec2::splat(13.0));
    if app.memory_ui.updating {
        kit::paint_state_dot(ui.painter(), glyph.shrink(2.5), kit::DotState::Ongoing, &t, ui.input(|i| i.time) as f32);
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
    } else {
        let tint = if app.session_memory.is_some() { t.alias.label[1] } else { t.alias.label[3] };
        icons::paint(ui.painter(), glyph, Icon::Memory, kit::col(tint));
    }
    ui.painter().text(
        egui::pos2(rect.min.x + 26.0, rect.center().y),
        Align2::LEFT_CENTER,
        &label,
        font,
        kit::col(t.alias.label[1]),
    );
    let hover = if app.memory_ui.updating {
        "Updating this session's memory…"
    } else {
        "This session's memory — what it is about and the key facts to keep"
    };
    if resp.on_hover_text(hover).clicked() {
        app.memory_ui.open = !app.memory_ui.open;
    }
}

/// The panel, when open. Drawn from `ui::draw` so it sits above the whole
/// window like every other modal.
pub fn panel(app: &mut App, ctx: &egui::Context) {
    if !app.memory_ui.open {
        return;
    }
    let out = kit::modal(ctx, egui::Id::new("session_memory_panel"), "Session memory", 620.0, true, |ui| {
        body(app, ui)
    });
    if let Some(req) = out.inner {
        app.send(UiCommand::SendRequest(req));
    }
    if out.dismissed {
        app.memory_ui.open = false;
        app.memory_ui.draft = None;
    }
}

/// The panel's content. Returns the one request the person asked for.
fn body(app: &mut App, ui: &mut egui::Ui) -> Option<Request> {
    let t = app.theme;
    let session_id = app.chat.session_id;
    let mut out = None;

    status_line(app, ui);
    ui.add_space(4.0);

    if let Some((summary, facts)) = app.memory_ui.draft.as_mut() {
        // Edit mode: the person's version replaces the memory whole.
        kit::label(ui, kit::txt("Summary", 13.0, Weight::Medium, kit::col(t.alias.label[0])));
        ui.add(
            egui::TextEdit::multiline(summary)
                .desired_rows(4)
                .desired_width(f32::INFINITY)
                .hint_text("What this session is about and what has been done"),
        );
        kit::label(ui, kit::txt("Key facts — one per line", 13.0, Weight::Medium, kit::col(t.alias.label[0])));
        egui::ScrollArea::vertical().max_height(240.0).id_source("memory_facts_edit").show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(facts)
                    .desired_rows(8)
                    .desired_width(f32::INFINITY)
                    .hint_text("Decisions, file paths, commands that worked, constraints…"),
            );
        });
        let mut save = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            save = kit::button(ui, "Save", kit::Variant::Primary, kit::Size::Sm).clicked();
            cancel = kit::button(ui, "Cancel", kit::Variant::Ghost, kit::Size::Sm).clicked();
        });
        if save {
            let facts: Vec<String> = facts
                .lines()
                .map(|l| l.trim().trim_start_matches(['-', '*', '•']).trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            out = Some(Request::SetSessionMemory { session_id, summary: summary.trim().to_string(), facts });
        }
        if save || cancel {
            app.memory_ui.draft = None;
        }
        return out;
    }

    let memory = app.session_memory.clone();
    egui::ScrollArea::vertical().max_height(420.0).id_source("memory_view").show(ui, |ui| {
        let Some(m) = &memory else {
            ui.add(
                egui::Label::new(kit::txt(
                    "Nothing yet. The session's memory is written in the background once the \
                     session has been idle for a moment — or ask for it now. It is put back in \
                     front of the model whenever the context is compacted, so decisions and \
                     key facts survive long sessions.",
                    13.0,
                    Weight::Regular,
                    kit::col(t.alias.label[2]),
                ))
                .wrap(),
            );
            return;
        };
        if !m.summary.trim().is_empty() {
            kit::label(ui, kit::txt("Summary", 13.0, Weight::Medium, kit::col(t.alias.label[0])));
            ui.add(
                egui::Label::new(kit::txt(m.summary.trim(), 13.0, Weight::Regular, kit::col(t.alias.label[1])))
                    .wrap(),
            );
            ui.add_space(6.0);
        }
        if !m.facts.is_empty() {
            kit::label(ui, kit::txt("Key facts", 13.0, Weight::Medium, kit::col(t.alias.label[0])));
            let folder = app.session_workspace().1;
            for fact in &m.facts {
                ui.horizontal(|ui| {
                    let keep_w = 56.0;
                    ui.allocate_ui_with_layout(
                        Vec2::new(ui.available_width() - keep_w, 0.0),
                        Layout::top_down(Align::Min),
                        |ui| {
                            ui.add(
                                egui::Label::new(kit::txt(
                                    format!("• {fact}"),
                                    13.0,
                                    Weight::Regular,
                                    kit::col(t.alias.label[1]),
                                ))
                                .wrap(),
                            );
                        },
                    );
                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                        let keep = kit::button(ui, "Keep", kit::Variant::Ghost, kit::Size::Sm)
                            .on_hover_text("Keep in long-term memory for this project, so later sessions know it");
                        if keep.clicked() {
                            out = Some(Request::SaveMemory {
                                id:      None,
                                text:    fact.clone(),
                                project: Some(folder.clone()),
                            });
                        }
                    });
                });
            }
        }
    });

    ui.add_space(8.0);
    let llm_ready = app.llm_state.is_ready();
    let mut edit = false;
    let mut open_settings = false;
    ui.horizontal(|ui| {
        let can_update = llm_ready && !app.memory_ui.updating;
        let update = kit::button_enabled(ui, "Update now", kit::Variant::Outline, kit::Size::Sm, can_update)
            .on_hover_text(if llm_ready {
                "Bring the memory up to date now. It waits for any running turn to finish."
            } else {
                "Connect a model first — the memory is written by it"
            });
        if update.clicked() {
            app.memory_ui.error = None;
            out = Some(Request::RefreshSessionMemory { session_id });
        }
        if kit::button(ui, "Edit", kit::Variant::Ghost, kit::Size::Sm).clicked() {
            edit = true;
        }
        if memory.is_some()
            && kit::button(ui, "Clear", kit::Variant::Danger, kit::Size::Sm)
                .on_hover_text("Forget this session's summary and key facts")
                .clicked()
        {
            out = Some(Request::SetSessionMemory { session_id, summary: String::new(), facts: Vec::new() });
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if kit::button(ui, "Long-term memory…", kit::Variant::Ghost, kit::Size::Sm).clicked() {
                open_settings = true;
            }
        });
    });
    if edit {
        let (summary, facts) = memory
            .map(|m| (m.summary, m.facts.join("\n")))
            .unwrap_or_default();
        app.memory_ui.draft = Some((summary, facts));
    }
    if open_settings {
        app.memory_ui.open = false;
        app.settings_open = true;
        app.settings_tab = SettingsTab::Memory;
    }
    out
}

/// One line under the title: who wrote the memory and when, or that the
/// keeper is working on it, or why its last pass produced nothing.
fn status_line(app: &App, ui: &mut egui::Ui) {
    let t = app.theme;
    let (text, color) = if app.memory_ui.updating {
        ("Updating from the conversation…".to_string(), t.alias.label[2])
    } else if let Some(err) = &app.memory_ui.error {
        (format!("The last update produced nothing — {err}"), t.alias.warn)
    } else if let Some(m) = &app.session_memory {
        let who = match m.author.as_str() {
            "auto" => "kept up to date in the background",
            "model" => "last changed by the model",
            "user" => "last edited by you",
            other => other,
        };
        (format!("{who} · {}", relative(m.updated_at)), t.alias.label[2])
    } else {
        return;
    };
    ui.horizontal(|ui| {
        if app.memory_ui.updating {
            kit::state_dot(ui, kit::DotState::Ongoing, 10.0);
        }
        ui.add(egui::Label::new(kit::txt(text, 12.0, Weight::Regular, kit::col(color))).wrap());
    });
}

/// `just now` / `12 min ago` / `3 h ago` / `2 days ago`, from unix ms.
fn relative(ms: i64) -> String {
    let secs = ((chrono::Utc::now().timestamp_millis() - ms) / 1000).max(0);
    if secs < 60 {
        "just now".into()
    } else if secs < 3_600 {
        format!("{} min ago", secs / 60)
    } else if secs < 86_400 {
        format!("{} h ago", secs / 3_600)
    } else {
        format!("{} days ago", secs / 86_400)
    }
}
