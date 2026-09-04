//! Settings › Diagnostics — the old Communication tab, rehoused (§7.2):
//! the backend/IPC connection card with the build controls, the demo request
//! row `smoke` also exercises, and the log panel, which now carries the
//! backend's own level instead of flattening everything to INF.

use crate::app::App;
use crate::ui::kit::{self, Weight};
use crate::ui::{controls, log_panel};

use super::{row, section};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;

    section(ui, "Connection");
    row(
        ui,
        "Backend",
        &format!(
            "{} · protocol v{} · build {}",
            if app.be_state.running {
                format!("running (pid {})", app.be_state.pid.unwrap_or(0))
            } else {
                "stopped".to_string()
            },
            protocol::PROTOCOL_VERSION,
            app.be_state.running_version.as_deref().unwrap_or("—"),
        ),
        |ui| {
            if app.be_state.running {
                if kit::button(ui, "Stop", kit::Variant::Outline, kit::Size::Sm).clicked() {
                    app.send(crate::supervisor::UiCommand::StopBe);
                }
            } else if kit::button(ui, "Start", kit::Variant::Primary, kit::Size::Sm).clicked() {
                app.send(crate::supervisor::UiCommand::StartBe);
            }
        },
    );
    row(
        ui,
        "Rebuild",
        "cargo build -p backend, then respawn the child and reconnect the pipe",
        |ui| {
            let busy = app.build_state.in_flight;
            if kit::button_enabled(
                ui,
                if busy { "Building…" } else { "Rebuild & restart" },
                kit::Variant::Primary,
                kit::Size::Sm,
                !busy,
            )
            .clicked()
            {
                app.send(crate::supervisor::UiCommand::RebuildAndRestart {
                    release: app.release_profile,
                });
            }
        },
    );
    row(ui, "Release profile", "Build the backend with --release", |ui| {
        ui.checkbox(&mut app.release_profile, "");
    });

    section(ui, "Demo requests");
    kit::label(
        ui,
        kit::txt(
            "The legacy request set the smoke test drives — a live check that \
             the pipe, the framing and the dispatcher are all healthy.",
            12.0,
            Weight::Regular,
            kit::col(t.alias.label[2]),
        ),
    );
    ui.add_space(6.0);
    controls::draw_request(app, ui);

    section(ui, "Log");
    log_panel::draw(app, ui);
}
