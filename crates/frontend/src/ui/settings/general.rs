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
            if dark != app.theme_dark {
                app.theme_dark = dark;
                // Note: theme is applied at startup; a restart of the app
                // applies a change. (Live re-apply could call `ctx.set_style`
                // again but we keep this scope minimal.)
            }
        });

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
