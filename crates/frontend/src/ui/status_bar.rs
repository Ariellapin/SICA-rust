use crate::app::App;

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        let be_dot = if app.be_state.running {
            (egui::Color32::from_rgb(110, 220, 110), "running")
        } else {
            (egui::Color32::from_gray(140), "stopped")
        };
        circle(ui, be_dot.0);
        ui.label(format!(
            "BE: {} {}",
            be_dot.1,
            app.be_state.pid.map(|p| format!("pid={p}")).unwrap_or_default()
        ));
        ui.separator();

        let ipc_dot = if app.ipc_state.connected {
            (egui::Color32::from_rgb(110, 220, 110), "connected")
        } else {
            (egui::Color32::from_rgb(220, 110, 110), "disconnected")
        };
        circle(ui, ipc_dot.0);
        ui.label(format!("IPC: {}", ipc_dot.1));
        ui.separator();

        match (app.build_state.last_ok, app.build_state.last_duration_ms) {
            (Some(true), Some(ms)) => {
                circle(ui, egui::Color32::from_rgb(110, 220, 110));
                ui.label(format!("last build: ok ({:.2}s)", ms as f64 / 1000.0));
            }
            (Some(false), Some(ms)) => {
                circle(ui, egui::Color32::from_rgb(220, 110, 110));
                ui.label(format!("last build: FAILED ({:.2}s)", ms as f64 / 1000.0));
            }
            _ => {
                circle(ui, egui::Color32::from_gray(140));
                ui.label("last build: —");
            }
        }
        if app.build_state.in_flight {
            ui.spinner();
            ui.label("building…");
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(format!("log: {} lines", app.log.len()));
        });
    });
}

fn circle(ui: &mut egui::Ui, color: egui::Color32) {
    let r = 5.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(r * 2.5, r * 2.5), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), r, color);
}
