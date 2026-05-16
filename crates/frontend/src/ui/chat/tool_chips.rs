//! Tool-call chain. Editorial form: a leading caps-label verb (RUN / OK /
//! ERR) coloured by state, followed by the tool name as a tracked caps
//! label. Middot separators between chips.

use egui::{Sense, Stroke};

use sica_core::theme::Palette;

use crate::app::{rgb, ToolChip};
use crate::ui::widgets::{caps_label, caps_job};

pub fn draw(ui: &mut egui::Ui, chips: &[ToolChip], palette: &Palette) {
    if chips.is_empty() {
        return;
    }
    ui.add_space(4.0);
    egui::Frame::none()
        .inner_margin(egui::Margin::symmetric(6.0, 4.0))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let muted = rgb(palette.muted);
                for (i, chip) in chips.iter().enumerate() {
                    let (verb, color) = if !chip.finished {
                        ("RUN", rgb(palette.accent))
                    } else if chip.ok {
                        ("OK", muted)
                    } else {
                        ("ERR", rgb(palette.danger))
                    };
                    caps_label(ui, verb, color);
                    // Tool name as a tracked caps label with hairline underline.
                    let name_resp = ui.add(
                        egui::Label::new(caps_job(&chip.name, rgb(palette.ink), 11.0))
                            .selectable(false)
                            .sense(Sense::hover()),
                    );
                    let rect = name_resp.rect;
                    ui.painter().hline(
                        rect.x_range(),
                        rect.bottom() + 1.0,
                        Stroke::new(1.0, rgb(palette.hairline)),
                    );
                    if !chip.summary.is_empty() {
                        name_resp.on_hover_text(&chip.summary);
                    }
                    if i + 1 < chips.len() {
                        ui.label(egui::RichText::new(" · ").color(muted));
                    }
                }
            });
        });
}
