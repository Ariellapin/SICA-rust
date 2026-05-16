//! Composer. Editorial form: no surrounding frame; a hairline rule under
//! the input field; primary "SEND" button to the right. The token meter
//! lives in the bottom status bar — there is no duplicate beneath the
//! composer. When a ticket has been written this turn, the ticket id is
//! shown on a single tracked-caps line under the field.

use egui::Stroke;

use protocol::Request;

use crate::app::{rgb, App};
use crate::supervisor::UiCommand;
use crate::ui::widgets::{caps_label, primary_button_enabled};

const SEND_BUTTON_W: f32 = 84.0;

pub fn draw(app: &mut App, ui: &mut egui::Ui, disabled: bool) {
    let p = app.palette;

    ui.horizontal(|ui| {
        let spacing = ui.spacing().item_spacing.x;
        let input_w = (ui.available_width() - SEND_BUTTON_W - spacing).max(80.0);
        let input = egui::TextEdit::multiline(&mut app.chat.draft)
            .hint_text(if disabled { "(disabled — connect an LLM)" } else { "Type a message…" })
            .desired_width(input_w)
            .desired_rows(2)
            .frame(false);
        let resp = ui.add_enabled(!disabled, input);

        // Hairline directly under the input rect — replaces the egui chrome.
        ui.painter().hline(
            resp.rect.x_range(),
            resp.rect.bottom() + 2.0,
            Stroke::new(1.0, if resp.has_focus() { rgb(p.accent) } else { rgb(p.hairline) }),
        );

        let send_enabled = !disabled && !app.chat.draft.trim().is_empty();
        let send_resp = primary_button_enabled(ui, &p, "Send", send_enabled);
        let send = send_resp.clicked();
        let submit_via_keys = !disabled
            && resp.has_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.ctrl);
        if send || submit_via_keys {
            let text = std::mem::take(&mut app.chat.draft);
            if !text.trim().is_empty() {
                let session_id = app.chat.session_id;
                app.chat.turns.push(crate::app::Turn {
                    session_id,
                    turn_id: 0,
                    user: text.clone(),
                    assistant: String::new(),
                    reasoning: String::new(),
                    finished: false,
                    finish_reason: None,
                    tool_chips: Vec::new(),
                    reasoning_collapsed: false,
                });
                app.chat.scroll_to_bottom = true;
                app.send(UiCommand::SendRequest(Request::SendUserMessage { session_id, text }));
            }
        }
    });

    if let Some(ticket) = app.chat.idealist.last_ticket.clone() {
        ui.add_space(8.0);
        caps_label(ui, &format!("TICKET {ticket}"), rgb(p.muted));
    }
}
