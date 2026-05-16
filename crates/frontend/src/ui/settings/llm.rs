use egui::RichText;

use protocol::{LlmState, Request};
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

        let connected  = matches!(app.llm_state.state, LlmState::Ready { .. });
        let connecting = matches!(app.llm_state.state, LlmState::Connecting);

        ui.horizontal(|ui| {
            let connect_enabled = !connected && !connecting && app.ipc_state.connected;
            let connect_resp = ui.add_enabled(
                connect_enabled,
                egui::Button::new(if connecting { "Connecting…" } else { "Connect" }),
            );
            if !connect_enabled {
                let hint = if !app.ipc_state.connected {
                    "BE service must be running first."
                } else if connecting {
                    "Already connecting — wait for the request to finish."
                } else {
                    "Already connected. Click Disconnect first to reconnect, \
                     or click Apply after editing fields."
                };
                connect_resp.clone().on_hover_text(hint);
            }
            if connect_resp.clicked() {
                // Push an immediate log line so the user gets feedback even if
                // the network call takes a beat.
                app.send(UiCommand::SendRequest(Request::ConnectLlm {
                    base_url: app.llm_base_url.clone(),
                    model:    app.llm_model.clone(),
                }));
            }

            let disc_resp = ui.add_enabled(connected, egui::Button::new("Disconnect"));
            if disc_resp.clicked() {
                app.send(UiCommand::SendRequest(Request::DisconnectLlm));
            }
        });

        ui.add_space(8.0);
        let status_color = match &app.llm_state.state {
            LlmState::Ready { .. }    => rgb(t::IDEALIST_GREEN),
            LlmState::Connecting      => rgb(t::IDEALIST_YELLOW),
            LlmState::Error { .. }    => rgb(t::ERROR_FG),
            LlmState::Disconnected    => rgb(t::TEXT_MUTED),
        };
        ui.label(
            RichText::new(format!("Status: {}", app.llm_state.label()))
                .color(status_color),
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
