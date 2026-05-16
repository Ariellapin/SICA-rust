use crate::app::{App, LogKind};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Log").strong());
        if ui.small_button("Clear").clicked() {
            app.log.clear();
        }
    });
    ui.separator();

    let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
    let total_rows = app.log.len();

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .stick_to_bottom(app.autoscroll)
        .show_rows(ui, row_height, total_rows, |ui, row_range| {
            for idx in row_range {
                if let Some(entry) = app.log.get(idx) {
                    let color = match entry.kind {
                        LogKind::Info  => egui::Color32::from_gray(210),
                        LogKind::Build => egui::Color32::from_rgb(180, 200, 255),
                        LogKind::Be    => egui::Color32::from_rgb(180, 255, 180),
                        LogKind::Ipc   => egui::Color32::from_rgb(255, 220, 140),
                        LogKind::Event => egui::Color32::from_rgb(220, 180, 255),
                        LogKind::Error => egui::Color32::from_rgb(255, 130, 130),
                    };
                    let tag = match entry.kind {
                        LogKind::Info  => "INF",
                        LogKind::Build => "BLD",
                        LogKind::Be    => "BE ",
                        LogKind::Ipc   => "IPC",
                        LogKind::Event => "EVT",
                        LogKind::Error => "ERR",
                    };
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(tag)
                                .monospace()
                                .color(color)
                                .strong(),
                        );
                        ui.label(egui::RichText::new(&entry.text).monospace().color(color));
                    });
                }
            }
        });
}
