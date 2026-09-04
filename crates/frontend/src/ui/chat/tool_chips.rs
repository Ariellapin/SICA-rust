//! Tool-call chain. Editorial form: a leading caps-label verb (RUN / OK /
//! ERR) coloured by state, followed by the tool name as a tracked caps
//! label. Middot separators between chips.

use egui::{Sense, Stroke};

use sica_core::theme::Palette;

use crate::app::{rgb, ToolChip};
use crate::ui::widgets::{caps_label, caps_job};

/// Display cap for the chip's call text. An args preview can carry a whole
/// script or an unbroken path; caps labels don't wrap, so an uncapped label
/// widens the transcript's content rect far past the window and everything
/// to its right becomes unreachable. The full call text moves to hover.
const LABEL_MAX_CHARS: usize = 72;
/// Same guard for the italic `> expectation` tail.
const EXPECT_MAX_CHARS: usize = 64;

/// Front-truncate-free char-safe ellipsis cap.
fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let head: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

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
                    // Editorial form of the call: `skill 'arg'` followed by
                    // `> expectation` so the chip reads like the source line
                    // the model emitted. We render the natural-language label
                    // when present and fall back to the bare skill name.
                    let full = if chip.args_preview.is_empty() {
                        chip.name.clone()
                    } else {
                        chip.args_preview.clone()
                    };
                    let label = ellipsize(&full, LABEL_MAX_CHARS);
                    let truncated = label != full;
                    let name_resp = ui.add(
                        egui::Label::new(caps_job(&label, rgb(palette.ink), 11.0))
                            .selectable(false)
                            .sense(Sense::hover()),
                    );
                    let rect = name_resp.rect;
                    ui.painter().hline(
                        rect.x_range(),
                        rect.bottom() + 1.0,
                        Stroke::new(1.0, rgb(palette.hairline)),
                    );
                    // Hover reveals whatever the chip elided: the untruncated
                    // call text, the expectation, and the result summary.
                    let mut hover_parts: Vec<String> = Vec::new();
                    if truncated {
                        hover_parts.push(full.clone());
                    }
                    if !chip.expectation.is_empty() {
                        hover_parts.push(format!("expect: {}", chip.expectation));
                    }
                    if !chip.summary.is_empty() {
                        hover_parts.push(chip.summary.clone());
                    }
                    if !hover_parts.is_empty() {
                        name_resp.on_hover_text(hover_parts.join("\n\n"));
                    }
                    if !chip.expectation.is_empty() {
                        ui.label(
                            egui::RichText::new(format!(
                                " > {}",
                                ellipsize(&chip.expectation, EXPECT_MAX_CHARS)
                            ))
                            .color(muted)
                            .size(10.0)
                            .italics(),
                        );
                    }
                    if i + 1 < chips.len() {
                        ui.label(egui::RichText::new(" · ").color(muted));
                    }
                }
            });
        });
}
