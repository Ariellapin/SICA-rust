//! Left rail. One painter-drawn glyph per view, no emoji. Brand mark at
//! the bottom is the word "sica" stacked vertically in italic serif with a
//! short rust-accent hairline above it.

use egui::{Color32, Layout, Rect, Sense, Stroke, Vec2};

use crate::app::{rgb, App, AppView};
use crate::ui::widgets::{chat_rail_mark, display_text, settings_rail_mark};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let p = app.palette;
    let ink     = rgb(p.ink);
    let accent  = rgb(p.accent);
    let subtle  = rgb(p.accent_subtle);
    let muted   = rgb(p.muted);

    ui.vertical_centered_justified(|ui| {
        ui.add_space(10.0);
        if rail_button(ui, app.view == AppView::Chat, RailMark::Chat, "Chat", ink, accent, subtle).clicked() {
            app.view = AppView::Chat;
        }
        ui.add_space(4.0);
        if rail_button(ui, app.view == AppView::Settings, RailMark::Settings, "Settings", ink, accent, subtle).clicked() {
            app.view = AppView::Settings;
        }

        ui.with_layout(Layout::bottom_up(egui::Align::Center), |ui| {
            ui.add_space(10.0);
            let avail = ui.available_width();
            let bar_w = (avail * 0.45).min(18.0);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(bar_w, 1.5), Sense::hover());
            ui.painter().hline(
                rect.x_range(),
                rect.center().y,
                Stroke::new(1.5, accent),
            );
            ui.add_space(4.0);
            // Stacked single-letter brand, top-to-bottom: s / i / c / a.
            // bottom_up layout means we push in reverse.
            for ch in ['a', 'c', 'i', 's'] {
                ui.label(display_text(&ch.to_string(), 13.0).color(muted));
            }
            ui.add_space(4.0);
        });
    });
}

#[derive(Clone, Copy)]
enum RailMark { Chat, Settings }

fn rail_button(
    ui: &mut egui::Ui,
    active: bool,
    mark: RailMark,
    tooltip: &str,
    ink: Color32,
    accent: Color32,
    subtle: Color32,
) -> egui::Response {
    let size = Vec2::splat(44.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());

    let fill = if active {
        subtle
    } else if resp.hovered() {
        subtle
    } else {
        Color32::TRANSPARENT
    };
    if fill != Color32::TRANSPARENT {
        ui.painter().rect_filled(rect, 2.0, fill);
    }
    if active {
        // Left-edge accent slab — the same idiom we use for active session rows.
        let slab = Rect::from_min_size(rect.min, Vec2::new(2.0, rect.height()));
        ui.painter().rect_filled(slab, 0.0, accent);
    }

    let glyph_color = if active || resp.hovered() { accent } else { ink };
    let glyph_rect = Rect::from_center_size(rect.center(), Vec2::splat(22.0));
    match mark {
        RailMark::Chat     => chat_rail_mark(&ui.painter(), glyph_rect, glyph_color),
        RailMark::Settings => settings_rail_mark(&ui.painter(), glyph_rect, glyph_color),
    }

    resp.on_hover_text(tooltip)
}
