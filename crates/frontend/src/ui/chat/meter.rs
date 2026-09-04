//! The context ring (§5.1 "Context ring"). A 14 px ring — track
//! `border-l3`, fill `label[2]`, **monochrome at every percentage**, because
//! a threshold colour tells the user to act on something the harness already
//! handles itself. Clicking opens a 264 px panel with `~used / window` and a
//! 4 px segmented bar: System prompt / Tools / Messages — the first surface
//! ever to draw `TokenBreakdown`, which has been on the wire since v12.
//!
//! While the backend is compacting, the ring spins and says so; the old
//! `⟳ COMPRESSING` status-bar text is gone with the status bar.

use std::sync::atomic::Ordering;

use egui::{Pos2, Rect, Rounding, Sense, Stroke, Vec2};

use crate::app::App;
use crate::ui::kit::{self, Elevation, Level, Weight};

const RING_PX: f32 = 22.0;

pub fn context_ring(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    let used = app.tokens.used.load(Ordering::Relaxed);
    let limit = app.tokens.limit.load(Ordering::Relaxed);
    let budget = app.tokens.budget.load(Ordering::Relaxed);
    // Renders nothing until capacity is known — a 0 % ring is a lie.
    let Some(pct) = app.tokens.pct() else { return };

    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(RING_PX), Sense::click());
    let center = rect.center();
    let radius = 5.5;
    let painter = ui.painter();
    painter.circle_stroke(center, radius, Stroke::new(2.0, Level::L3.color(&t)));

    let sweep = if app.chat.compacting {
        // Compaction: a rotating arc rather than a fill.
        let time = ui.input(|i| i.time) as f32;
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(33));
        ((time * 2.0) % 1.0, 0.3)
    } else {
        (-0.25, pct as f32 / 100.0)
    };
    let (start_turns, frac) = sweep;
    let pts = (0..=24)
        .map(|i| {
            let a = (start_turns + frac * (i as f32 / 24.0)) * std::f32::consts::TAU;
            Pos2::new(center.x + radius * a.cos(), center.y + radius * a.sin())
        })
        .collect::<Vec<_>>();
    painter.add(egui::Shape::line(
        pts,
        Stroke::new(2.0, kit::col(t.alias.label[2])),
    ));

    let tip = if app.chat.compacting {
        "Compacting context…".to_string()
    } else {
        format!("{pct}% of context used")
    };
    if resp.on_hover_text(tip).clicked() {
        app.menu_open.context = !app.menu_open.context;
    }
    if app.menu_open.context {
        panel(app, ui, rect, used, limit, budget, pct);
    }
}

#[allow(clippy::too_many_arguments)]
fn panel(
    app: &mut App,
    ui: &mut egui::Ui,
    anchor: Rect,
    used: u32,
    limit: u32,
    budget: u32,
    pct: u32,
) {
    let t = app.theme;
    let ctx = ui.ctx().clone();
    let width = 264.0;
    let pos = Pos2::new(
        (anchor.center().x - width / 2.0).max(ctx.screen_rect().min.x + 8.0),
        anchor.min.y - 128.0,
    );
    let area = egui::Area::new(egui::Id::new("context_panel"))
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .show(&ctx, |ui| {
            ui.set_width(width);
            kit::elevated_frame(
                &t,
                Elevation::Prominent,
                Level::L1,
                kit::col(t.alias.menu),
                sica_core::theme::tokens::RADIUS_CARD,
            )
            .inner_margin(egui::Margin::symmetric(14.0, 12.0))
            .show(ui, |ui| {
                kit::label(
                    ui,
                    kit::txt(
                        format!("~{used} / {limit}"),
                        14.0,
                        Weight::Medium,
                        kit::col(t.alias.label[0]),
                    ),
                );
                kit::footnote(
                    ui,
                    &format!(
                        "{pct}% of the {budget}-token prompt budget · compaction fires at {}%",
                        protocol::COMPACT_TRIGGER_PCT
                    ),
                );
                ui.add_space(8.0);

                // 4 px segmented bar.
                let b = app.token_breakdown;
                let (system, tools, history) = match b {
                    Some(b) => (b.system, b.tools, b.history),
                    None => (0, 0, used),
                };
                let total = (system + tools + history).max(1) as f32;
                let (bar, _) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 4.0), Sense::hover());
                let seg_colors = [
                    kit::col(t.statics.bluish[8]),
                    egui::Color32::from_rgb(167, 139, 250),
                    kit::col(t.statics.accent[4]),
                ];
                let mut x = bar.min.x;
                for (v, color) in [system, tools, history].iter().zip(seg_colors) {
                    let w = bar.width() * (*v as f32 / total);
                    ui.painter().rect_filled(
                        Rect::from_min_size(egui::pos2(x, bar.min.y), Vec2::new(w, 4.0)),
                        Rounding::same(2.0),
                        color,
                    );
                    x += w;
                }
                ui.add_space(8.0);
                for (label, v, color) in [
                    ("System prompt", system, seg_colors[0]),
                    ("Tools", tools, seg_colors[1]),
                    ("Messages", history, seg_colors[2]),
                ] {
                    ui.horizontal(|ui| {
                        let (dot, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                        ui.painter()
                            .circle_filled(dot.center(), 4.0, color);
                        ui.add_space(4.0);
                        kit::label(
                            ui,
                            kit::txt(label, 12.0, Weight::Regular, kit::col(t.alias.label[1])),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                kit::label(
                                    ui,
                                    kit::txt(
                                        format!("~{v}"),
                                        12.0,
                                        Weight::Regular,
                                        kit::col(t.alias.label[2]),
                                    ),
                                );
                            },
                        );
                    });
                }
                if app.token_breakdown.is_none() {
                    kit::footnote(ui, "composition appears once a turn has run");
                }
            });
        });

    let rect = area.response.rect;
    let outside = ctx.input(|i| {
        i.pointer.any_pressed()
            && i.pointer
                .interact_pos()
                .map(|p| !rect.contains(p) && !anchor.contains(p))
                .unwrap_or(false)
    });
    if outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.menu_open.context = false;
    }
}
