//! Chat view: empty-state screens + message list + input bar.

mod input_bar;
mod messages;
mod tool_chips;

use sica_core::theme as t;

use crate::app::{rgb, App, AppView, SettingsTab};
use crate::supervisor::UiCommand;

/// Empty-state precedence: BE missing trumps LLM missing (you can't reach an
/// LLM without the BE proxying the requests).
pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    if let Some((be_v, fe_v)) = app.be_state.protocol_mismatch {
        draw_protocol_banner(app, ui, be_v, fe_v);
    }
    if !app.ipc_state.connected {
        draw_no_be(app, ui);
        return;
    }
    if !app.llm_state.is_ready() {
        draw_no_llm(app, ui);
        // Still show the input bar but disabled, so the affordance is visible.
        ui.add_space(8.0);
        input_bar::draw(app, ui, true);
        return;
    }
    messages::draw(app, ui);
    ui.add_space(6.0);
    input_bar::draw(app, ui, false);
}

fn draw_protocol_banner(app: &mut App, ui: &mut egui::Ui, be_v: u32, fe_v: u32) {
    egui::Frame::none()
        .fill(rgb(t::SYSTEM_BUBBLE_BG))
        .stroke(egui::Stroke::new(1.0, rgb(t::ERROR_FG)))
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "Backend protocol mismatch (BE v{be_v} vs FE v{fe_v})"
                        ))
                        .color(rgb(t::ERROR_FG))
                        .strong(),
                    );
                    ui.label(
                        egui::RichText::new(
                            "The BE binary on disk was built against an older protocol. \
                             Rebuild and restart it before chatting."
                        )
                        .color(rgb(t::TEXT_MUTED))
                        .small(),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(egui::RichText::new("Rebuild & Restart").color(rgb(t::ACCENT)))
                        .clicked()
                    {
                        app.send(UiCommand::RebuildAndRestart { release: app.release_profile });
                    }
                });
            });
        });
    ui.add_space(8.0);
}

fn draw_no_be(app: &mut App, ui: &mut egui::Ui) {
    ui.allocate_ui_with_layout(
        ui.available_size(),
        egui::Layout::centered_and_justified(egui::Direction::TopDown),
        |ui| {
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("No BE service running")
                        .color(rgb(t::TEXT_PRIMARY))
                        .heading(),
                );
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("The backend isn't connected. Start it to begin a chat.")
                        .color(rgb(t::TEXT_MUTED)),
                );
                ui.add_space(16.0);
                if ui
                    .add(egui::Button::new(egui::RichText::new("▶ Start BE").color(rgb(t::ACCENT))))
                    .clicked()
                {
                    app.send(UiCommand::StartBe);
                }
            });
        },
    );
}

fn draw_no_llm(app: &mut App, ui: &mut egui::Ui) {
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), 220.0),
        egui::Layout::centered_and_justified(egui::Direction::TopDown),
        |ui| {
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("No LLM connected")
                        .color(rgb(t::TEXT_PRIMARY))
                        .heading(),
                );
                ui.add_space(6.0);
                let detail = app
                    .llm_state
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "Open Settings → LLM to connect.".into());
                ui.label(egui::RichText::new(detail).color(rgb(t::TEXT_MUTED)));
                ui.add_space(12.0);
                if ui.button("Open LLM settings").clicked() {
                    app.view = AppView::Settings;
                    app.settings_tab = SettingsTab::Llm;
                }
            });
        },
    );
}
