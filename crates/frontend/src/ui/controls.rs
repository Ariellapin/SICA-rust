use crate::app::{App, RequestKind};
use crate::supervisor::UiCommand;

pub fn draw_top(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        ui.heading("sica-rust");
        ui.separator();

        let be_running = app.be_state.running;
        let build_busy = app.build_state.in_flight;

        if ui
            .add_enabled(!be_running && !build_busy, egui::Button::new("▶ Start BE"))
            .clicked()
        {
            app.send(UiCommand::StartBe);
        }
        if ui
            .add_enabled(be_running, egui::Button::new("■ Stop BE"))
            .clicked()
        {
            app.send(UiCommand::StopBe);
        }
        ui.separator();
        if ui
            .add_enabled(!build_busy, egui::Button::new("⟳ Rebuild"))
            .clicked()
        {
            app.send(UiCommand::Rebuild { release: app.release_profile });
        }
        if ui
            .add_enabled(!build_busy, egui::Button::new("⟳ Rebuild & Restart"))
            .clicked()
        {
            app.send(UiCommand::RebuildAndRestart { release: app.release_profile });
        }

        ui.separator();
        if ui
            .checkbox(&mut app.auto_watch, "Auto-watch")
            .changed()
        {
            app.send(UiCommand::SetAutoWatch(app.auto_watch));
        }
        ui.checkbox(&mut app.release_profile, "Release profile");

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.checkbox(&mut app.autoscroll, "Autoscroll");
        });
    });
}

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
