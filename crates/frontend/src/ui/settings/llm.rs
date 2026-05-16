//! LLM tab — one card per provider config (TOML file under
//! `sica-settings/llm-providers/`). A single backend `LlmClient` is shared,
//! so only one panel can be active at a time; the others show "idle".
//!
//! Panels render in a two-column grid; the active / last-connected provider
//! is pinned to the first slot so the user's working config is always
//! visible without scrolling.

use egui::{RichText, Vec2};

use protocol::{LlmState, Request};

use sica_core::theme::Palette;

use crate::app::{rgb, App};
use crate::supervisor::UiCommand;
use crate::ui::widgets::{
    caps_label, card, ghost_button, ghost_button_enabled, muted_italic, primary_button_enabled,
    section_heading, status_pill,
};

const GRID_COLS: usize = 2;
const GRID_GUTTER: f32 = 12.0;

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let p = app.palette;

    if !app.ipc_state.connected {
        ui.label(
            RichText::new("BE service must be running to manage the LLM connection.")
                .color(rgb(p.danger)),
        );
        ui.add_space(6.0);
    }

    if app.providers.is_empty() {
        ui.label(muted_italic(
            &p,
            "No provider configs found. Add TOML files under \
             `sica-settings/llm-providers/` and restart.",
        ));
        return;
    }

    // Draw order: active / last-connected provider first, the rest in their
    // natural (alphabetical) order. Pins the working panel to the top-left
    // so it greets the user without scrolling.
    let active_id = app.active_provider_id.clone();
    let mut order: Vec<usize> = (0..app.providers.len()).collect();
    if let Some(id) = active_id.as_deref() {
        if let Some(pos) = app.providers.iter().position(|cfg| cfg.id == id) {
            let chosen = order.remove(pos);
            order.insert(0, chosen);
        }
    }

    let mut clicked_connect: Option<String> = None;
    let mut clicked_disconnect = false;

    let llm_state = app.llm_state.state.clone();
    let ipc_connected = app.ipc_state.connected;

    for row in order.chunks(GRID_COLS) {
        ui.columns(GRID_COLS, |ui_cols| {
            for (slot, &idx) in row.iter().enumerate() {
                let ui = &mut ui_cols[slot];
                let cfg = &mut app.providers[idx];
                let is_active = active_id.as_deref() == Some(cfg.id.as_str());
                let panel_state = if is_active { Some(&llm_state) } else { None };

                card(ui, &p, |ui| {
                    section_heading(ui, &p, &cfg.title);
                    ui.label(muted_italic(&p, &cfg.description));
                    ui.add_space(10.0);

                    field_row(ui, &p, "Base URL", &mut cfg.base_url, false);
                    field_row(ui, &p, "Model",    &mut cfg.model,    false);
                    field_row(ui, &p, "API key",  &mut cfg.api_key,  true);

                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        let panel_connecting = matches!(panel_state, Some(LlmState::Connecting));
                        let panel_ready      = matches!(panel_state, Some(LlmState::Ready { .. }));
                        let connect_enabled  = ipc_connected && !panel_connecting && !panel_ready;
                        let connect_label    = if panel_connecting { "Connecting" } else { "Connect" };

                        let connect_resp = primary_button_enabled(ui, &p, connect_label, connect_enabled);
                        if !connect_enabled {
                            let hint = if !ipc_connected {
                                "BE service must be running first."
                            } else if panel_connecting {
                                "Already connecting — wait for the request to finish."
                            } else {
                                "Already connected. Click Disconnect first to reconnect."
                            };
                            connect_resp.clone().on_hover_text(hint);
                        }
                        if connect_resp.clicked() {
                            clicked_connect = Some(cfg.id.clone());
                        }

                        let disc_enabled = is_active
                            && matches!(panel_state, Some(LlmState::Ready { .. } | LlmState::Connecting));
                        if ghost_button_enabled(ui, &p, "Disconnect", disc_enabled).clicked() {
                            clicked_disconnect = true;
                        }

                        ui.add_space(12.0);
                        draw_status(ui, &p, panel_state);
                    });
                });
            }
        });
        ui.add_space(GRID_GUTTER);
    }

    if let Some(id) = clicked_connect {
        app.connect_provider(&id);
    } else if clicked_disconnect {
        app.send(UiCommand::SendRequest(Request::DisconnectLlm));
        app.active_provider_id = None;
        app.persist_settings();
    }

    let _ = ghost_button; // exposed for future "Add provider" affordance.
}

fn field_row(
    ui: &mut egui::Ui,
    p: &Palette,
    label: &str,
    value: &mut String,
    password: bool,
) {
    ui.horizontal(|ui| {
        ui.allocate_ui(Vec2::new(72.0, 22.0), |ui| {
            caps_label(ui, label, rgb(p.muted));
        });
        // Fill the remaining card width so the input scales with the grid
        // cell rather than being clipped by fixed widths from the old
        // single-column layout.
        let w = (ui.available_width() - 4.0).max(80.0);
        let mut edit = egui::TextEdit::singleline(value).desired_width(w);
        if password {
            edit = edit.password(true);
        }
        ui.add(edit);
    });
    ui.add_space(4.0);
}

fn draw_status(ui: &mut egui::Ui, p: &Palette, state: Option<&LlmState>) {
    let (text, color) = match state {
        Some(LlmState::Connecting) => ("Connecting", rgb(p.warn)),
        Some(LlmState::Ready { model, .. }) => {
            // Render the model name as a tracked caps label after the OK pill.
            status_pill(ui, p, "Ready", rgb(p.ok));
            ui.add_space(6.0);
            caps_label(ui, model, rgb(p.muted));
            return;
        }
        Some(LlmState::Error { message }) => {
            status_pill(ui, p, "Error", rgb(p.danger));
            ui.add_space(6.0);
            ui.label(RichText::new(message).color(rgb(p.danger)).small());
            return;
        }
        Some(LlmState::Disconnected) | None => ("Idle", rgb(p.muted)),
    };
    status_pill(ui, p, text, color);
}
