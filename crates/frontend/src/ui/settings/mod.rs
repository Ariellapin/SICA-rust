//! Settings view with three tabs. Tabs render as tracked caps labels with
//! a 1px accent underline under the active one; the Apply bar at the bottom
//! is the only primary-button surface in the settings area.

mod communication;
mod general;
mod llm;

use std::time::Duration;

use egui::{Sense, Stroke, Vec2};

use crate::app::{rgb, App, SettingsTab};
use crate::ui::widgets::{
    caps_label, hairline, italic_text, primary_button, section_heading,
};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    section_heading(ui, &app.palette, "Settings");

    ui.horizontal(|ui| {
        tab_button(ui, app, SettingsTab::General, "General");
        ui.add_space(20.0);
        tab_button(ui, app, SettingsTab::Llm, "LLM");
        ui.add_space(20.0);
        tab_button(ui, app, SettingsTab::Communication, "Communication");
    });
    ui.add_space(8.0);
    hairline(ui, &app.palette);
    ui.add_space(12.0);

    // Body — leave room at the bottom for the Apply bar.
    let body_height = (ui.available_height() - 64.0).max(120.0);
    ui.allocate_ui(Vec2::new(ui.available_width(), body_height), |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| match app.settings_tab {
                SettingsTab::General       => general::draw(app, ui),
                SettingsTab::Llm           => llm::draw(app, ui),
                SettingsTab::Communication => communication::draw(app, ui),
            });
    });

    ui.add_space(10.0);
    hairline(ui, &app.palette);
    ui.add_space(8.0);
    draw_apply_bar(app, ui);
}

fn tab_button(ui: &mut egui::Ui, app: &mut App, kind: SettingsTab, label: &str) {
    let p = app.palette;
    let selected = app.settings_tab == kind;
    let color = if selected { rgb(p.accent) } else { rgb(p.muted) };

    let label_resp = ui.add(
        egui::Label::new(crate::ui::widgets::caps_job(label, color, 11.0))
            .selectable(false)
            .sense(Sense::click()),
    );

    // Active underline — 1px accent line spanning the label width.
    if selected {
        let rect = label_resp.rect;
        let y = rect.bottom() + 3.0;
        ui.painter().hline(
            rect.x_range(),
            y,
            Stroke::new(1.5, rgb(p.accent)),
        );
        ui.add_space(2.0);
    } else if label_resp.hovered() {
        let rect = label_resp.rect;
        let y = rect.bottom() + 3.0;
        ui.painter().hline(
            rect.x_range(),
            y,
            Stroke::new(1.0, rgb(p.muted)),
        );
        ui.add_space(2.0);
    }

    if label_resp.clicked() {
        app.settings_tab = kind;
    }
}

fn draw_apply_bar(app: &mut App, ui: &mut egui::Ui) {
    let p = app.palette;
    ui.horizontal(|ui| {
        if primary_button(ui, &p, "Apply").clicked() {
            let ctx = ui.ctx().clone();
            app.apply_and_save_settings(&ctx);
        }
        ui.add_space(12.0);
        if let Some((ts, result)) = &app.last_settings_status {
            if ts.elapsed() < Duration::from_secs(4) {
                match result {
                    Ok(()) => caps_label(ui, "Saved", rgb(p.ok)),
                    Err(e) => {
                        ui.label(
                            italic_text(&format!("Save failed: {e}"), 12.0)
                                .color(rgb(p.danger)),
                        )
                    }
                };
            }
        }
    });
}
