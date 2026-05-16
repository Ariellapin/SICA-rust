//! Settings view with three tabs: General, LLM, Communication.
//! A shared Apply button at the bottom persists changes to sica-settings.json
//! and re-applies runtime state (theme, LLM reconnect, auto-watch).

mod communication;
mod general;
mod llm;

use std::time::Duration;

use egui::RichText;

use sica_core::theme as t;

use crate::app::{rgb, App, SettingsTab};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.heading("Settings");
    });
    ui.separator();

    ui.horizontal(|ui| {
        tab_button(ui, app, SettingsTab::General, "General");
        tab_button(ui, app, SettingsTab::Llm, "LLM");
        tab_button(ui, app, SettingsTab::Communication, "Communication");
    });
    ui.separator();
    ui.add_space(8.0);

    // Body — leave room at the bottom for the Apply bar.
    let body_height = (ui.available_height() - 56.0).max(120.0);
    ui.allocate_ui(egui::vec2(ui.available_width(), body_height), |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| match app.settings_tab {
                SettingsTab::General => general::draw(app, ui),
                SettingsTab::Llm => llm::draw(app, ui),
                SettingsTab::Communication => communication::draw(app, ui),
            });
    });

    ui.add_space(6.0);
    ui.separator();
    draw_apply_bar(app, ui);
}

fn tab_button(ui: &mut egui::Ui, app: &mut App, kind: SettingsTab, label: &str) {
    let selected = app.settings_tab == kind;
    if ui.selectable_label(selected, label).clicked() {
        app.settings_tab = kind;
    }
}

fn draw_apply_bar(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        if ui
            .add(egui::Button::new(RichText::new("Apply").color(rgb(t::ACCENT))))
            .clicked()
        {
            let ctx = ui.ctx().clone();
            app.apply_and_save_settings(&ctx);
        }
        if let Some((ts, result)) = &app.last_settings_status {
            if ts.elapsed() < Duration::from_secs(4) {
                match result {
                    Ok(()) => ui.label(
                        RichText::new("Saved.").color(rgb(t::IDEALIST_GREEN)).small(),
                    ),
                    Err(e) => ui.label(
                        RichText::new(format!("Save failed: {e}"))
                            .color(rgb(t::ERROR_FG))
                            .small(),
                    ),
                };
            }
        }
    });
}
