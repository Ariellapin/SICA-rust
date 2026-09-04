//! The log panel (§9). Rows are coloured by *level* — `error` / `warn` /
//! `label[1]` / `label[2]` — instead of the old hard-coded RGBs, and the
//! backend's own `LogLine.level` survives the wire-to-UI hop now, so a WARN
//! from the tool-call parser reads as a WARN here (and raises a toast).

use crate::app::{App, LogKind, LogKind2};
use crate::ui::kit;

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    ui.horizontal(|ui| {
        for level in [
            LogKind2::All,
            LogKind2::InfoUp,
            LogKind2::WarnUp,
            LogKind2::ErrorOnly,
        ] {
            let active = app.log_filter == level;
            if kit::pill(ui, level.label(), active)
                .interact(egui::Sense::click())
                .clicked()
            {
                app.log_filter = level;
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if kit::button(ui, "Clear", kit::Variant::Ghost, kit::Size::Sm).clicked() {
                app.log.clear();
            }
        });
    });
    ui.add_space(6.0);

    let min_rank = app.log_filter.min_rank();
    let rows: Vec<_> = app
        .log
        .iter()
        .filter(|e| e.kind.rank() >= min_rank)
        .cloned()
        .collect();
    let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .max_height(320.0)
        .stick_to_bottom(app.autoscroll)
        .show_rows(ui, row_height, rows.len(), |ui, range| {
            for idx in range {
                let Some(entry) = rows.get(idx) else { continue };
                let color = match entry.kind {
                    LogKind::Error => kit::col(t.alias.error),
                    LogKind::Warn => kit::col(t.alias.warn_label),
                    LogKind::Debug => kit::col(t.alias.label[3]),
                    LogKind::Build | LogKind::Be | LogKind::Ipc | LogKind::Event => {
                        kit::col(t.alias.label[2])
                    }
                    LogKind::Info => kit::col(t.alias.label[1]),
                };
                ui.horizontal(|ui| {
                    kit::label(ui, kit::mono(entry.kind.tag(), 11.0, color));
                    ui.add(
                        egui::Label::new(kit::mono(&entry.text, 11.0, color))
                            .wrap(),
                    );
                });
            }
        });
}
