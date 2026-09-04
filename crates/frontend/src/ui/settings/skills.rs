//! Settings › Skills (§7.2 — replaces dsh's Plugins section).
//!
//! Tab **Catalogue**: what the `/` palette lists, grouped Commands / Skills /
//! Agents, with the source path of each entry and a filter. Tab **Folders**:
//! the three directories those entries come from, plus the skill-creator
//! template. The harness constants (hop limit, timeouts, spill caps) are
//! read-only here until they become settings.

use egui::{Layout, Sense, Vec2};

use protocol::CatalogKind;
use sica_core::paths::{agents_dir, commands_dir, skills_dir};

use crate::app::App;
use crate::ui::kit::{self, Level, Weight};

use super::{open_path, row, section};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    let tab_id = ui.id().with("skills_tab");
    let mut tab: usize = ui.ctx().data(|d| d.get_temp(tab_id).unwrap_or(0));
    ui.add_space(12.0);
    if let Some(i) = super::segmented(ui, &["Catalogue", "Folders", "Harness"], tab) {
        tab = i;
        ui.ctx().data_mut(|d| d.insert_temp(tab_id, i));
    }
    ui.add_space(8.0);

    match tab {
        0 => catalogue(app, ui),
        1 => folders(app, ui),
        _ => harness(app, ui),
    }
    let _ = t;
    ui.add_space(24.0);
}

fn catalogue(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    let filter_id = ui.id().with("skills_filter");
    let mut filter: String = ui.ctx().data(|d| d.get_temp(filter_id).unwrap_or_default());
    let resp = ui.add(
        egui::TextEdit::singleline(&mut filter)
            .hint_text("Filter by name or description")
            .desired_width(ui.available_width()),
    );
    if resp.changed() {
        ui.ctx()
            .data_mut(|d| d.insert_temp(filter_id, filter.clone()));
    }
    let q = filter.to_lowercase();

    if app.chat.slash.entries.is_empty() {
        ui.add_space(12.0);
        kit::label(
            ui,
            kit::txt(
                "The catalogue arrives with the backend connection.",
                13.0,
                Weight::Regular,
                kit::col(t.alias.label[2]),
            ),
        );
        return;
    }

    for (kind, title) in [
        (CatalogKind::Command, "Commands"),
        (CatalogKind::Skill, "Skills"),
        (CatalogKind::Agent, "Agents"),
    ] {
        let entries: Vec<_> = app
            .chat
            .slash
            .entries
            .iter()
            .filter(|e| e.kind == kind)
            .filter(|e| {
                q.is_empty()
                    || e.name.to_lowercase().contains(&q)
                    || e.description.to_lowercase().contains(&q)
            })
            .cloned()
            .collect();
        if entries.is_empty() {
            continue;
        }
        section(ui, &format!("{title} · {}", entries.len()));
        for e in entries {
            ui.add_space(6.0);
            ui.horizontal_top(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(160.0, 20.0), Sense::hover());
                ui.painter().text(
                    rect.left_center(),
                    egui::Align2::LEFT_CENTER,
                    format!("/{}", e.name),
                    kit::mono_font(13.0),
                    kit::col(t.alias.label[0]),
                );
                ui.vertical(|ui| {
                    ui.add(egui::Label::new(kit::txt(
                        &e.description,
                        13.0,
                        Weight::Regular,
                        kit::col(t.alias.label[1]),
                    )));
                    if let Some(src) = &e.source {
                        let src = src.clone();
                        let resp = kit::label(
                            ui,
                            kit::txt(&src, 11.0, Weight::Regular, kit::col(t.alias.label[3])),
                        )
                        .interact(Sense::click());
                        if resp.on_hover_text("Open in the file browser").clicked() {
                            let _ = open_path(std::path::Path::new(&src));
                        }
                    }
                });
            });
            kit::hairline(ui, Level::L1);
        }
    }
}

fn folders(app: &mut App, ui: &mut egui::Ui) {
    for (label, dir, hint) in [
        (
            "Skills",
            skills_dir(),
            "*.md files here register as skills the agent can call. Each needs \
             YAML frontmatter with `name:` and `description:`. Scanned at \
             backend start — adding one needs a restart.",
        ),
        (
            "Commands",
            commands_dir(),
            "Typed as /name in the composer; {{args}} is substituted. Read per \
             message, so edits need no restart.",
        ),
        (
            "Agents",
            agents_dir(),
            "Markdown agents the / palette lists and a typed /name injects.",
        ),
    ] {
        let path = dir.clone();
        row(ui, label, hint, move |ui| {
            if kit::button(ui, "Open", kit::Variant::Outline, kit::Size::Sm).clicked() {
                let _ = std::fs::create_dir_all(&path);
                let _ = open_path(&path);
            }
        });
    }
    let dir = skills_dir();
    row(
        ui,
        "skill-creator template",
        "Seeds skills/skill-creator.md and opens it",
        |ui| {
            if kit::button(ui, "Reveal", kit::Variant::Outline, kit::Size::Sm).clicked() {
                let _ = agents::skill_creator::seed_default(&dir);
                let _ = open_path(&dir.join("skill-creator.md"));
            }
        },
    );
    let _ = app;
}

fn harness(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    ui.add_space(8.0);
    kit::label(
        ui,
        kit::txt(
            "These are compile-time constants today; the section lists them so \
             the numbers behind a stalled or truncated call are visible.",
            12.0,
            Weight::Regular,
            kit::col(t.alias.label[2]),
        ),
    );
    for (name, value) in [
        ("Tool hops per turn", "12"),
        ("Default tool timeout", "120 s"),
        ("Shell foreground cap", "30 s · 32 KiB per stream"),
        ("Background jobs", "10 per session · 256 KiB retained"),
        ("Spill threshold", "48 KB → spill/<session>/"),
        ("Tool-result pruner", "8 KiB → 4 KiB head + 1 KiB tail"),
        ("Sub-agent depth", "4"),
        ("Parallel tool pool", "4 (native mode)"),
    ] {
        row(ui, name, "", |ui| {
            kit::label(
                ui,
                kit::mono(value, 12.0, kit::col(t.alias.label[1])),
            );
        });
    }
    ui.with_layout(Layout::left_to_right(egui::Align::Center), |ui| {
        ui.add_space(0.0);
    });
}
