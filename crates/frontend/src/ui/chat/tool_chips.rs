//! Renders the tool-call chain.
//!
//! All chips for a turn — including chips emitted by nested sub-agents — appear
//! on **one** horizontal line, separated by `>`. The verb flips from `Use` to
//! `Used` when the chip's `finished` flag is set.

use egui::RichText;

use sica_core::theme as t;

use crate::app::{rgb, ToolChip};

pub fn draw(ui: &mut egui::Ui, chips: &[ToolChip]) {
    if chips.is_empty() {
        return;
    }
    egui::Frame::none()
        .inner_margin(egui::Margin::symmetric(6.0, 4.0))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for chip in chips {
                    let (verb, color) = if !chip.finished {
                        ("Use", rgb(t::ACCENT))
                    } else if chip.ok {
                        ("Used", rgb(t::TEXT_MUTED))
                    } else {
                        ("Used", rgb(t::ERROR_FG))
                    };
                    if !chip.finished {
                        // Subtle running indicator before the label.
                        ui.add(egui::Spinner::new().size(10.0));
                    }
                    let text = format!("{verb} {}", chip.name);
                    let resp = ui.label(
                        RichText::new(text)
                            .color(color)
                            .monospace(),
                    );
                    if !chip.summary.is_empty() {
                        resp.on_hover_text(&chip.summary);
                    }
                    ui.label(RichText::new(">").color(rgb(t::TEXT_MUTED)).monospace());
                }
            });
        });
}
