//! Small shared widgets used across the UI.

use egui::{Color32, Response, RichText, Sense, Ui};

use sica_core::theme as t;

use crate::app::rgb;

/// A 10x10 filled circle that surfaces a hover tooltip. The tooltip body
/// contains the dot's label and, when present, a red exception detail line.
pub fn status_dot(ui: &mut Ui, color: Color32, label: &str, detail: Option<&str>) -> Response {
    let r = 5.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(r * 2.5, r * 2.5), Sense::hover());
    ui.painter().circle_filled(rect.center(), r, color);
    let detail = detail.map(|s| s.to_string());
    resp.on_hover_ui(|ui| {
        ui.label(RichText::new(label).strong());
        if let Some(d) = detail.as_deref() {
            ui.label(RichText::new(d).color(rgb(t::ERROR_FG)));
        }
    })
}
