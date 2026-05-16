//! Settings view with three tabs: General, LLM, Communication.

mod communication;
mod general;
mod llm;

use crate::app::{App, SettingsTab};

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

    match app.settings_tab {
        SettingsTab::General => general::draw(app, ui),
        SettingsTab::Llm => llm::draw(app, ui),
        SettingsTab::Communication => communication::draw(app, ui),
    }
}

fn tab_button(ui: &mut egui::Ui, app: &mut App, kind: SettingsTab, label: &str) {
    let selected = app.settings_tab == kind;
    if ui.selectable_label(selected, label).clicked() {
        app.settings_tab = kind;
    }
}
