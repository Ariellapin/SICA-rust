//! Scrollable list of turns. Editorial form — no bubbles. User messages
//! right-align in a subtle sunk-surface rect; assistant messages run full-
//! width below an "ASSISTANT" caps label and a hairline rule; reasoning is
//! inset 24px with a left-edge info-blue hairline and italic serif body.

use egui::{Align, Layout, RichText, Rounding, Sense, Stroke, Vec2};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

use sica_core::theme::Palette;

use crate::app::{rgb, App};
use crate::ui::widgets::{
    blade_mark, caps_button, caps_label, display_text, hairline,
};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let height = ui.available_height() - 110.0;
    let force_scroll = std::mem::take(&mut app.chat.scroll_to_bottom);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .max_height(height.max(120.0))
        .show(ui, |ui| {
            if app.chat.turns.is_empty() {
                draw_empty(ui, &palette);
                return;
            }
            for i in 0..app.chat.turns.len() {
                let (user, assistant, reasoning_text, finished, collapsed) = {
                    let t = &app.chat.turns[i];
                    (
                        t.user.clone(),
                        t.assistant.clone(),
                        t.reasoning.clone(),
                        t.finished,
                        t.reasoning_collapsed,
                    )
                };
                if !user.is_empty() {
                    draw_user(ui, &user, &palette);
                }
                if !reasoning_text.is_empty() {
                    if finished && collapsed {
                        if reasoning_chip_collapsed(ui, &palette).clicked() {
                            app.chat.turns[i].reasoning_collapsed = false;
                        }
                    } else {
                        if reasoning_header(ui, finished, &palette).clicked() && finished {
                            app.chat.turns[i].reasoning_collapsed = true;
                        }
                        draw_reasoning(ui, &reasoning_text, &palette);
                    }
                }
                if !assistant.is_empty() || !finished {
                    draw_assistant(
                        ui,
                        &mut app.md_cache,
                        i,
                        if assistant.is_empty() { "…" } else { &assistant },
                        &palette,
                    );
                }
                let chips = app.chat.turns[i].tool_chips.clone();
                if !chips.is_empty() {
                    super::tool_chips::draw(ui, &chips, &palette);
                }
                ui.add_space(20.0);
            }
            // Bottom anchor: when new content arrived this frame, hard-snap
            // the viewport here. This guarantees autoscroll even when egui's
            // `stick_to_bottom` heuristic disengages (e.g. on first render
            // after a session switch, or when streaming starts while the
            // user was not yet at the bottom).
            if force_scroll {
                let anchor = ui.allocate_response(Vec2::ZERO, Sense::hover());
                anchor.scroll_to_me(Some(Align::BOTTOM));
            }
        });
}

// ---------- empty state ----------

fn draw_empty(ui: &mut egui::Ui, p: &Palette) {
    let avail = ui.available_size();
    ui.allocate_ui_with_layout(
        avail,
        Layout::centered_and_justified(egui::Direction::TopDown),
        |ui| {
            ui.vertical_centered(|ui| {
                let watermark_size = Vec2::new(180.0, 90.0);
                let (rect, _) = ui.allocate_exact_size(watermark_size, Sense::hover());
                // Watermark — accent at low alpha.
                let mark = rgb(p.accent).linear_multiply(0.18);
                blade_mark(&ui.painter(), rect, mark);
                ui.add_space(12.0);
                ui.label(display_text("Begin.", 24.0).color(rgb(p.muted)));
            });
        },
    );
}

// ---------- user ----------

fn draw_user(ui: &mut egui::Ui, text: &str, p: &Palette) {
    let avail = ui.available_width();
    let max_w = (avail * 0.78).max(160.0);
    ui.allocate_ui_with_layout(
        Vec2::new(avail, 0.0),
        Layout::right_to_left(Align::Min),
        |ui| {
            egui::Frame::none()
                .fill(rgb(p.surface_sunk))
                .rounding(Rounding::same(2.0))
                .inner_margin(egui::Margin::symmetric(14.0, 10.0))
                .show(ui, |ui| {
                    ui.set_max_width(max_w);
                    ui.label(RichText::new(text).color(rgb(p.ink)));
                });
        },
    );
    ui.add_space(8.0);
}

// ---------- assistant ----------

fn draw_assistant(
    ui: &mut egui::Ui,
    cache: &mut CommonMarkCache,
    turn_idx: usize,
    text: &str,
    p: &Palette,
) {
    caps_label(ui, "ASSISTANT", rgb(p.muted));
    ui.add_space(2.0);
    hairline(ui, p);
    ui.add_space(8.0);
    // Render as CommonMark so headings, bold/italic, lists and code
    // fences come through. The viewer ID has to be unique per turn so
    // egui_commonmark can keep per-document state straight when the
    // stream re-renders many of these blocks on the same frame.
    //
    // The scoped style swap below is load-bearing: egui_commonmark resolves
    // `**bold**`, headings and list bullets through `strong_text_color()`,
    // which reads `widgets.active.fg_stroke.color`. Our theme paints that
    // with the page background so pressed buttons render inverse text — but
    // that also makes every strong glyph invisible against the page. We
    // override it to ink for the duration of the viewer.
    ui.scope(|ui| {
        let ink = rgb(p.ink);
        let v = &mut ui.style_mut().visuals.widgets;
        v.active.fg_stroke.color = ink;
        v.noninteractive.fg_stroke.color = ink;
        let viewer_id = format!("assistant_md_{turn_idx}");
        CommonMarkViewer::new(viewer_id).show(ui, cache, text);
    });
}

// ---------- reasoning ----------

fn draw_reasoning(ui: &mut egui::Ui, text: &str, p: &Palette) {
    ui.add_space(6.0);
    let resp = ui.horizontal(|ui| {
        ui.add_space(24.0);
        ui.vertical(|ui| {
            ui.set_max_width(ui.available_width());
            ui.label(display_text(text, 14.0).color(rgb(p.muted)));
        });
    });
    // Paint the inset vertical hairline along the full body height.
    let rect = resp.response.rect;
    ui.painter().vline(
        rect.min.x + 11.0,
        rect.y_range(),
        Stroke::new(1.0, rgb(p.info)),
    );
    ui.add_space(8.0);
}

/// Collapsed-state chip: a single tracked caps label that re-expands the
/// hidden reasoning when clicked.
fn reasoning_chip_collapsed(ui: &mut egui::Ui, p: &Palette) -> egui::Response {
    let resp = caps_button(ui, "+ Reasoning", rgb(p.info));
    resp.on_hover_text("Show reasoning")
}

/// Expanded-state header above the inset reasoning body.
fn reasoning_header(ui: &mut egui::Ui, finished: bool, p: &Palette) -> egui::Response {
    let label = if finished { "− Reasoning" } else { "· Reasoning (live)" };
    let resp = caps_button(ui, label, rgb(p.info));
    if finished {
        resp.on_hover_text("Hide reasoning")
    } else {
        resp
    }
}
