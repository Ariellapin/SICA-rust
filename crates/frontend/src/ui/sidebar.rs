//! Thin left rail. One icon per top-level view. Active item highlights.

use egui::RichText;

use sica_core::theme as t;

use crate::app::{rgb, App, AppView};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    ui.vertical_centered_justified(|ui| {
        ui.add_space(8.0);
        if rail_button(ui, app.view == AppView::Chat, "💬", "Chat").clicked() {
            app.view = AppView::Chat;
        }
        ui.add_space(4.0);
        if rail_button(ui, app.view == AppView::Settings, "⚙", "Settings").clicked() {
            app.view = AppView::Settings;
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
            ui.add_space(8.0);
            ui.label(
                RichText::new("sica-rust")
                    .color(rgb(t::TEXT_MUTED))
                    .small()
                    .monospace(),
            );
        });
    });
}

fn rail_button(ui: &mut egui::Ui, active: bool, glyph: &str, tooltip: &str) -> egui::Response {
    let bg = if active { Some(rgb(t::SIDEBAR_ACTIVE_BG)) } else { None };
    let text = RichText::new(glyph).size(20.0);
    let mut button = egui::Button::new(text).min_size(egui::vec2(44.0, 44.0));
    if let Some(c) = bg {
        button = button.fill(c);
    }
    ui.add(button).on_hover_text(tooltip)
}
