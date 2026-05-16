//! Composer + live token meter. Token usage reads atomics that the backend
//! updates ~10×/sec during streaming.

use std::sync::atomic::Ordering;

use egui::RichText;

use protocol::Request;
use sica_core::theme as t;

use crate::app::{rgb, App};
use crate::supervisor::UiCommand;

pub fn draw(app: &mut App, ui: &mut egui::Ui, disabled: bool) {
    egui::Frame::none()
        .fill(rgb(t::ASSISTANT_BUBBLE_BG))
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(8.0, 6.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let input = egui::TextEdit::multiline(&mut app.chat.draft)
                    .hint_text(if disabled { "(disabled — connect an LLM)" } else { "Type a message…" })
                    .desired_width(f32::INFINITY)
                    .desired_rows(2);
                let resp = ui.add_enabled(!disabled, input);
                let send = ui
                    .add_enabled(!disabled && !app.chat.draft.trim().is_empty(),
                                 egui::Button::new(RichText::new("Send").color(rgb(t::ACCENT))))
                    .clicked();
                let submit_via_keys = !disabled
                    && resp.has_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.ctrl);
                if send || submit_via_keys {
                    let text = std::mem::take(&mut app.chat.draft);
                    if !text.trim().is_empty() {
                        let session_id = app.chat.session_id;
                        // Record the user message immediately in the active turn so
                        // it appears in the bubble list while we wait for the BE to
                        // emit `TurnStarted`.
                        app.chat.turns.push(crate::app::Turn {
                            session_id,
                            turn_id: 0,
                            user: text.clone(),
                            assistant: String::new(),
                            reasoning: String::new(),
                            finished: false,
                            finish_reason: None,
                            tool_chips: Vec::new(),
                        });
                        app.send(UiCommand::SendRequest(Request::SendUserMessage {
                            session_id,
                            text,
                        }));
                    }
                }
            });
            ui.horizontal(|ui| {
                let used = app.tokens.used.load(Ordering::Relaxed);
                let limit = app.tokens.limit.load(Ordering::Relaxed);
                ui.label(
                    RichText::new(format!("{used:>4} / {limit} tokens"))
                        .color(rgb(t::TEXT_MUTED))
                        .monospace(),
                );
                if let Some(ticket) = app.chat.idealist.last_ticket.clone() {
                    ui.separator();
                    ui.label(
                        RichText::new(format!("ticket: {ticket}"))
                            .color(rgb(t::TEXT_MUTED))
                            .small(),
                    );
                }
            });
        });
}
