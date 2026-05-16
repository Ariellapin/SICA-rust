//! Scrollable list of turns. Each turn renders the user message, assistant
//! message + reasoning (if any), and a single horizontal tool-chip row.

use egui::RichText;

use sica_core::theme as t;

use crate::app::{rgb, App};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let height = ui.available_height() - 110.0;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .max_height(height.max(120.0))
        .show(ui, |ui| {
            if app.chat.turns.is_empty() {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new("Start the conversation below.")
                            .color(rgb(t::TEXT_MUTED)),
                    );
                });
                return;
            }
            for turn in &app.chat.turns {
                if !turn.user.is_empty() {
                    bubble(ui, true, &turn.user);
                }
                if !turn.reasoning.is_empty() {
                    reasoning(ui, &turn.reasoning);
                }
                if !turn.assistant.is_empty() || !turn.finished {
                    bubble(ui, false, if turn.assistant.is_empty() { "…" } else { &turn.assistant });
                }
                if !turn.tool_chips.is_empty() {
                    super::tool_chips::draw(ui, &turn.tool_chips);
                }
                ui.add_space(8.0);
            }
        });
}

fn bubble(ui: &mut egui::Ui, is_user: bool, text: &str) {
    let fill = if is_user { rgb(t::USER_BUBBLE_BG) } else { rgb(t::ASSISTANT_BUBBLE_BG) };
    egui::Frame::none()
        .fill(fill)
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(rgb(t::TEXT_PRIMARY)));
        });
}

fn reasoning(ui: &mut egui::Ui, text: &str) {
    egui::Frame::none()
        .fill(rgb(t::ASSISTANT_BUBBLE_BG))
        .stroke(egui::Stroke::new(1.0, rgb(t::REASONING_BLUE)))
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
        .show(ui, |ui| {
            ui.label(
                RichText::new(text)
                    .color(rgb(t::REASONING_BLUE))
                    .italics(),
            );
        });
}
