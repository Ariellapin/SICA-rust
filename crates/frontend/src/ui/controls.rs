//! The legacy demo-request row, kept because `smoke` drives the same
//! requests and because a live round-trip through the pipe is the quickest
//! proof the dispatcher is healthy. Rehoused in Settings > Diagnostics.

use crate::app::{App, RequestKind};
use crate::supervisor::UiCommand;

pub fn draw_request(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        ui.label("Request:");
        ui.selectable_value(&mut app.request_draft.kind, RequestKind::GetCounter, "GetCounter");
        ui.selectable_value(&mut app.request_draft.kind, RequestKind::Increment, "Increment");
        ui.selectable_value(&mut app.request_draft.kind, RequestKind::Reset, "Reset");
        ui.selectable_value(&mut app.request_draft.kind, RequestKind::Fib, "Fib");
        ui.selectable_value(&mut app.request_draft.kind, RequestKind::Echo, "Echo");

        ui.separator();
        match app.request_draft.kind {
            RequestKind::Increment => {
                ui.label("by:");
                ui.add(egui::DragValue::new(&mut app.request_draft.inc_by).speed(1));
            }
            RequestKind::Fib => {
                ui.label("n:");
                ui.add(egui::DragValue::new(&mut app.request_draft.fib_n).range(0..=186));
            }
            RequestKind::Echo => {
                ui.label("text:");
                ui.add(
                    egui::TextEdit::singleline(&mut app.request_draft.echo_text)
                        .desired_width(220.0),
                );
            }
            _ => {}
        }

        ui.separator();
        let can_send = app.ipc_state.connected;
        if ui
            .add_enabled(can_send, egui::Button::new("➤ Send"))
            .clicked()
        {
            app.send(UiCommand::SendRequest(app.request_draft.to_request()));
        }
    });
}
