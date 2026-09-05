//! Shell layout (§2.2). Three columns on `bg_base` — sidebar, conversation,
//! details — with 0.5 px `border-l3` seams and a 300 ms width slide. The
//! bottom status bar is gone: its facts moved to the composer ring, the
//! session header and the sidebar's connection indicator.
//!
//! Concession order when the window is narrow: shrink details → close
//! details → the sidebar collapses to 56 px only under 1024 px, via a
//! separate override so re-widening restores the layout exactly.

mod chat;
mod controls;
pub mod fonts;
pub mod icons;
pub mod kit;
pub mod log_panel;
mod onboarding;
mod settings;
mod sidebar;

pub use onboarding::wanted as onboarding_wanted;
pub use settings::open_path as open_path_public;

use egui::{Rect, Vec2};
use sica_core::theme::tokens::{
    SIDEBAR_AUTO_COLLAPSE, SIDEBAR_COLLAPSED, SIDEBAR_MAX, SIDEBAR_MIN,
};

use crate::app::App;

pub fn draw(app: &mut App, ctx: &egui::Context) {
    layout_pass(app, ctx);

    let t = app.theme;
    let target = if app.layout.sidebar_collapsed {
        SIDEBAR_COLLAPSED
    } else {
        app.layout.sidebar_w
    };
    let width = ctx.animate_value_with_time(
        egui::Id::new("sidebar_width"),
        target,
        sica_core::theme::tokens::DUR_COLUMN,
    );

    egui::SidePanel::left("sidebar")
        .resizable(false)
        .exact_width(width)
        .show_separator_line(false)
        .frame(
            egui::Frame::none()
                .fill(kit::col(t.alias.sidebar_fill))
                .inner_margin(egui::Margin::symmetric(12.0, 6.0)),
        )
        .show(ctx, |ui| sidebar::draw(app, ui));

    // The details column is mounted only when something asks for it; 0 means
    // closed (§2.1 keeps it mounted, but egui has no cost to skipping it).
    if app.layout.details_w > 0.0 {
        egui::SidePanel::right("details")
            .resizable(false)
            .exact_width(app.layout.details_w)
            .show_separator_line(false)
            .frame(
                egui::Frame::none()
                    .fill(kit::col(t.alias.bg_base))
                    .inner_margin(egui::Margin::same(16.0)),
            )
            .show(ctx, |ui| chat::details::draw(app, ui));
    }

    let central = egui::CentralPanel::default()
        .frame(egui::Frame::none().fill(kit::col(t.alias.bg_base)))
        .show(ctx, |ui| {
            let rect = ui.max_rect();
            chat::draw(app, ui);
            rect
        })
        .inner;

    // Seams + the invisible 8 px drag strip on the sidebar edge.
    let painter = ctx.layer_painter(egui::LayerId::background());
    kit::vseam(
        &painter,
        &t,
        central.min.x - 0.5,
        ctx.screen_rect().y_range(),
        kit::Level::L3,
    );
    if !app.layout.sidebar_collapsed {
        sidebar_drag(app, ctx, central.min.x);
    }

    settings::draw(app, ctx);
    // Last, so it sits over everything the first run has no use for yet.
    onboarding::draw(app, ctx);
    draw_toast(app, ctx, central);
}

/// Auto-collapse rule: under 1024 px the sidebar collapses through a separate
/// override, so widening again restores whatever the user had chosen.
fn layout_pass(app: &mut App, ctx: &egui::Context) {
    let w = ctx.screen_rect().width();
    let narrow = w < SIDEBAR_AUTO_COLLAPSE;
    match (narrow, app.layout.narrow_override) {
        (true, None) => {
            app.layout.narrow_override = Some(app.layout.sidebar_collapsed);
            app.layout.sidebar_collapsed = true;
        }
        (false, Some(prev)) => {
            app.layout.sidebar_collapsed = prev;
            app.layout.narrow_override = None;
        }
        _ => {}
    }
}

fn sidebar_drag(app: &mut App, ctx: &egui::Context, seam_x: f32) {
    // An invisible 8 px strip centred on the seam. Hit-tested against raw
    // pointer state rather than a widget: the panels either side already own
    // their rects, and egui's own resize handle is off so the width — and its
    // clamp — stays ours.
    let strip = Rect::from_min_size(
        egui::pos2(seam_x - 4.0, ctx.screen_rect().min.y),
        Vec2::new(8.0, ctx.screen_rect().height()),
    );
    let hovered = ctx
        .input(|i| i.pointer.hover_pos())
        .map(|p| strip.contains(p))
        .unwrap_or(false);
    let id = egui::Id::new("sidebar_dragging");
    let mut dragging: bool = ctx.data(|d| d.get_temp(id).unwrap_or(false));
    if hovered && ctx.input(|i| i.pointer.primary_pressed()) {
        dragging = true;
    }
    if ctx.input(|i| i.pointer.primary_released()) {
        dragging = false;
    }
    ctx.data_mut(|d| d.insert_temp(id, dragging));

    if hovered || dragging {
        ctx.set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
    if dragging {
        if let Some(p) = ctx.input(|i| i.pointer.hover_pos()) {
            app.layout.sidebar_w = p.x.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        }
    }
}

/// One toast at a time, anchored over the conversation column.
fn draw_toast(app: &mut App, ctx: &egui::Context, column: Rect) {
    let Some(toast) = app.toast.clone() else { return };
    if !kit::toast(ctx, &toast, column) {
        app.toast = None;
    }
}

/// Conversation content width: `clamp(680, column × 0.64, 920)`, minus the
/// gutters, with the user's persisted override winning when it fits.
pub fn content_width(app: &App, avail: f32) -> f32 {
    use sica_core::theme::tokens::{CONTENT_W_FRACTION, CONTENT_W_MAX, CONTENT_W_MIN};
    let base = app
        .layout
        .content_w
        .unwrap_or_else(|| (avail * CONTENT_W_FRACTION).clamp(CONTENT_W_MIN, CONTENT_W_MAX));
    base.min(avail - 48.0).max(320.0)
}

/// Lay out `body` in a centred column `w` points wide.
///
/// A `Frame` with symmetric side margins rather than a padded row or a child
/// `Ui` at a fixed rect: the frame grows to its content's height, which is
/// what the composer's bottom panel measures itself from, and it hands the
/// content the full remaining height, which the transcript's `ScrollArea`
/// needs.
pub fn centered_column<R>(
    ui: &mut egui::Ui,
    w: f32,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let pad = ((ui.available_width() - w) / 2.0).max(0.0);
    egui::Frame::none()
        .inner_margin(egui::Margin {
            left: pad,
            right: pad,
            top: 0.0,
            bottom: 0.0,
        })
        .show(ui, body)
        .inner
}
