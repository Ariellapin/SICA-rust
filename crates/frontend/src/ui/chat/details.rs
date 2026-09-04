//! The details column (§2.1). dsh hosts one slot here,
//! `conversation.details.tool`: the selected tool call's full payload. The
//! column is closed (`details_w == 0`) until a row asks for it.
//!
//! Empty state, verbatim from dsh: "Click a tool row in the message flow to
//! view its details."

use crate::app::App;
use crate::ui::kit::{self, Weight};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    ui.horizontal(|ui| {
        kit::label(
            ui,
            kit::txt("Tool details", 14.0, Weight::Medium, kit::col(t.alias.label[0])),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if kit::icon_button(ui, crate::ui::icons::Icon::Close, 28.0).clicked() {
                app.layout.details_w = 0.0;
                app.details_call = None;
            }
        });
    });
    ui.add_space(8.0);

    let selected = app.details_call.and_then(|id| {
        app.chat
            .turns
            .iter()
            .flat_map(|t| t.tool_chips.iter())
            .find(|c| c.id == id)
            .cloned()
    });
    let Some(chip) = selected else {
        kit::label(
            ui,
            kit::txt(
                "Click a tool row in the message flow to view its details.",
                13.0,
                Weight::Regular,
                kit::col(t.alias.label[2]),
            ),
        );
        return;
    };

    kit::label(
        ui,
        kit::txt(
            super::tool_row::title_of(&chip.name),
            13.0,
            Weight::Medium,
            kit::col(t.alias.label[1]),
        ),
    );
    kit::footnote(ui, &chip.name);
    ui.add_space(8.0);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            kit::io_card(ui, &chip.args_preview, &chip.summary, !chip.ok);
            if !chip.expectation.is_empty() {
                ui.add_space(6.0);
                kit::footnote(ui, &format!("expected: {}", chip.expectation));
            }
        });
}
