//! Bottom status strip. Dots-only — labels and detail appear in hover tooltips.
//! Three dots: **BE**, **IPC**, **LLM**.

use egui::Color32;

use sica_core::theme as t;

use crate::app::{rgb, App};
use crate::ui::widgets::status_dot;

const COLOR_OK: Color32 = Color32::from_rgb(0x33, 0xCC, 0x66);  // IDEALIST_GREEN
const COLOR_ERR: Color32 = Color32::from_rgb(0xFF, 0x6B, 0x6B); // ERROR_FG
const COLOR_IDLE: Color32 = Color32::from_rgb(0x5A, 0x60, 0x68); // DIVIDER_FG

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        // BE dot
        let (be_color, be_label, be_detail) = if app.be_state.running {
            (COLOR_OK, format!("BE: running (pid={})", app.be_state.pid.unwrap_or(0)), None)
        } else if let Some(err) = app.be_state.last_error.clone() {
            (COLOR_ERR, "BE: stopped".to_string(), Some(err))
        } else {
            (COLOR_IDLE, "BE: stopped".to_string(), None)
        };
        status_dot(ui, be_color, &be_label, be_detail.as_deref());
        ui.add_space(8.0);

        // IPC dot
        let (ipc_color, ipc_label, ipc_detail) = if app.ipc_state.connected && !app.ipc_state.heartbeat_timeout {
            (COLOR_OK, "IPC: connected".to_string(), None)
        } else if app.ipc_state.connected && app.ipc_state.heartbeat_timeout {
            (COLOR_ERR, "IPC: heartbeat timeout".to_string(), Some("no heartbeat for >5s".to_string()))
        } else if let Some(err) = app.ipc_state.last_error.clone() {
            (COLOR_ERR, "IPC: disconnected".to_string(), Some(err))
        } else {
            (COLOR_IDLE, "IPC: disconnected".to_string(), None)
        };
        status_dot(ui, ipc_color, &ipc_label, ipc_detail.as_deref());
        ui.add_space(8.0);

        // LLM dot
        let llm_label = format!("LLM: {}", app.llm_state.label());
        let (llm_color, llm_detail) = match &app.llm_state.state {
            protocol::LlmState::Ready { .. } => (COLOR_OK, None),
            protocol::LlmState::Connecting => (rgb(t::IDEALIST_YELLOW), None),
            protocol::LlmState::Error { message } => (COLOR_ERR, Some(message.clone())),
            protocol::LlmState::Disconnected => (COLOR_IDLE, None),
        };
        status_dot(ui, llm_color, &llm_label, llm_detail.as_deref());

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let used = app.tokens.used.load(std::sync::atomic::Ordering::Relaxed);
            let limit = app.tokens.limit.load(std::sync::atomic::Ordering::Relaxed);
            ui.label(
                egui::RichText::new(format!("{used} / {limit} tokens"))
                    .color(rgb(t::TEXT_MUTED))
                    .monospace(),
            );
        });
    });
}
