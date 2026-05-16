use egui::RichText;

use sica_core::theme as t;

use crate::app::{rgb, App};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    ui.vertical(|ui| {
        ui.label(RichText::new("Appearance").strong());
        ui.horizontal(|ui| {
            let mut dark = app.theme_dark;
            ui.radio_value(&mut dark, true,  "Dark");
            ui.radio_value(&mut dark, false, "Light");
            app.theme_dark = dark;
        });
        ui.label(
            RichText::new("Theme changes apply when you click Apply.")
                .color(rgb(t::TEXT_MUTED))
                .small(),
        );

        ui.add_space(12.0);
        ui.label(RichText::new("Startup").strong());
        ui.checkbox(&mut app.auto_start_be, "Start BE service automatically");
        ui.checkbox(&mut app.auto_connect_llm, "Connect to LLM automatically once BE is up");

        ui.add_space(12.0);
        ui.label(RichText::new("Logging").strong());
        ui.checkbox(&mut app.log_raw_llm, "Log raw LLM responses to logs/model/");

        ui.add_space(12.0);
        ui.label(RichText::new("Idealist").strong());
        ui.checkbox(
            &mut app.idealist_auto_apply_be,
            "Auto-apply BE patches (off by default)",
        );
        ui.label(
            RichText::new(
                "When off, idealist only writes Improvement-BE-*.md tickets. \
                 Frontend issues always get a ticket and are never auto-patched."
            )
            .color(rgb(t::TEXT_MUTED))
            .small(),
        );
    });
}
