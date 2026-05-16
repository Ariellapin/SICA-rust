use egui::RichText;

use protocol::Request;
use sica_core::theme as t;

use crate::app::{rgb, App};
use crate::supervisor::UiCommand;

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    ui.vertical(|ui| {
        ui.label(RichText::new("LLM endpoint").strong());
        ui.horizontal(|ui| {
            ui.label("Base URL:");
            ui.add(egui::TextEdit::singleline(&mut app.llm_base_url).desired_width(360.0));
        });
        ui.horizontal(|ui| {
            ui.label("Model:");
            ui.add(egui::TextEdit::singleline(&mut app.llm_model).desired_width(220.0));
        });
        ui.add_space(8.0);

        let connected = matches!(app.llm_state.state, protocol::LlmState::Ready { .. });
        let connecting = matches!(app.llm_state.state, protocol::LlmState::Connecting);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !connected && !connecting && app.ipc_state.connected,
                    egui::Button::new("Connect"),
                )
                .clicked()
            {
                app.send(UiCommand::SendRequest(Request::ConnectLlm {
                    base_url: app.llm_base_url.clone(),
                    model: app.llm_model.clone(),
                }));
            }
            if ui
                .add_enabled(connected, egui::Button::new("Disconnect"))
                .clicked()
            {
                app.send(UiCommand::SendRequest(Request::DisconnectLlm));
            }
        });

        ui.add_space(8.0);
        ui.label(
            RichText::new(format!("Status: {}", app.llm_state.label()))
                .color(rgb(t::TEXT_MUTED)),
        );
        if !app.ipc_state.connected {
            ui.add_space(8.0);
            ui.label(
                RichText::new("BE service must be running to manage the LLM connection.")
                    .color(rgb(t::ERROR_FG)),
            );
        }
    });
}
