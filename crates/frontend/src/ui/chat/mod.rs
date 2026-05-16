//! Chat view: empty-state screens + message list + input bar.

mod input_bar;
mod messages;
mod tool_chips;

use egui::{Rect, Sense, Vec2};

use crate::app::{rgb, App, AppView, SettingsTab};
use crate::supervisor::UiCommand;
use crate::ui::widgets::{
    blade_mark, caps_label, display_text, ghost_button, primary_button,
};

/// Empty-state precedence: BE missing trumps LLM missing (you can't reach an
/// LLM without the BE proxying the requests).
pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    if let Some((be_v, fe_v)) = app.be_state.protocol_mismatch {
        draw_protocol_banner(app, ui, be_v, fe_v);
    }
    if !app.ipc_state.connected {
        draw_no_be(app, ui);
        return;
    }
    if !app.llm_state.is_ready() {
        draw_no_llm(app, ui);
        ui.add_space(8.0);
        input_bar::draw(app, ui, true);
        return;
    }
    messages::draw(app, ui);
    ui.add_space(10.0);
    input_bar::draw(app, ui, false);
}

fn draw_protocol_banner(app: &mut App, ui: &mut egui::Ui, be_v: u32, fe_v: u32) {
    let p = app.palette;
    let danger = rgb(p.danger);

    // 2px danger slab on the left edge — the warning bar idiom.
    egui::Frame::none()
        .fill(rgb(p.surface_sunk))
        .inner_margin(egui::Margin {
            left: 14.0, right: 12.0, top: 10.0, bottom: 10.0,
        })
        .show(ui, |ui| {
            let outer_rect = ui.max_rect();
            let slab = Rect::from_min_size(
                egui::pos2(outer_rect.min.x - 14.0, outer_rect.min.y - 10.0),
                Vec2::new(2.0, outer_rect.height() + 20.0),
            );
            ui.painter().rect_filled(slab, 0.0, danger);

            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    caps_label(ui, "Protocol mismatch", danger);
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "Backend reports v{be_v}; frontend expects v{fe_v}. The BE binary on disk was built against an older protocol — rebuild and restart it before chatting."
                        ))
                        .color(rgb(p.muted)),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if primary_button(ui, &p, "Rebuild & Restart").clicked() {
                        app.send(UiCommand::RebuildAndRestart { release: app.release_profile });
                    }
                });
            });
        });
    ui.add_space(10.0);
}

fn draw_no_be(app: &mut App, ui: &mut egui::Ui) {
    let p = app.palette;
    let avail = ui.available_size();
    ui.allocate_ui_with_layout(
        avail,
        egui::Layout::centered_and_justified(egui::Direction::TopDown),
        |ui| {
            ui.vertical_centered(|ui| {
                let mark_size = Vec2::new(200.0, 96.0);
                let (rect, _) = ui.allocate_exact_size(mark_size, Sense::hover());
                blade_mark(&ui.painter(), rect, rgb(p.accent).linear_multiply(0.18));
                ui.add_space(14.0);
                ui.label(display_text("No backend running.", 26.0).color(rgb(p.ink)));
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new("The daemon is not connected. Start it to begin.")
                        .color(rgb(p.muted)),
                );
                ui.add_space(18.0);
                if primary_button(ui, &p, "Start BE").clicked() {
                    app.send(UiCommand::StartBe);
                }
            });
        },
    );
}

fn draw_no_llm(app: &mut App, ui: &mut egui::Ui) {
    let p = app.palette;
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), 220.0),
        egui::Layout::centered_and_justified(egui::Direction::TopDown),
        |ui| {
            ui.vertical_centered(|ui| {
                let mark_size = Vec2::new(160.0, 76.0);
                let (rect, _) = ui.allocate_exact_size(mark_size, Sense::hover());
                blade_mark(&ui.painter(), rect, rgb(p.accent).linear_multiply(0.14));
                ui.add_space(10.0);
                ui.label(display_text("No language model.", 22.0).color(rgb(p.ink)));
                ui.add_space(6.0);
                let detail = app
                    .llm_state
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "Open Settings → LLM to connect a provider.".into());
                ui.label(egui::RichText::new(detail).color(rgb(p.muted)));
                ui.add_space(14.0);
                if ghost_button(ui, &p, "Open LLM Settings").clicked() {
                    app.view = AppView::Settings;
                    app.settings_tab = SettingsTab::Llm;
                }
            });
        },
    );
}
