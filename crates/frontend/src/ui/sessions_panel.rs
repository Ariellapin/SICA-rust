//! Sessions list — second column in Chat view. Editorial flat rows: title
//! in mono, hairline between rows, a 2px accent slab on the left edge marks
//! the active row. Hovering a row reveals a `×` that *arms* a delete; the row
//! then shows an inline "Delete / Keep" confirm so a stray click can't drop a
//! session. Hit areas are keyed by session id (not title) so duplicate titles
//! never target the wrong row.

use egui::{Align2, FontFamily, FontId, Rect, Sense, Vec2};

use protocol::Request;

use crate::app::{rgb, App};
use crate::supervisor::UiCommand;
use crate::ui::widgets::{
    caps_label, ghost_button, hairline, italic_text, right_aligned, section_heading,
};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let p = app.palette;

    // Header — section heading + ghost "+ NEW" button on the right.
    section_heading(ui, &p, "Sessions");
    ui.horizontal(|ui| {
        caps_label(ui, "ACTIVE", rgb(p.muted));
        right_aligned(ui, |ui| {
            if ghost_button(ui, &p, "+ New")
                .on_hover_text("Create a new chat session")
                .clicked()
            {
                app.send(UiCommand::SendRequest(Request::NewSession));
            }
        });
    });
    ui.add_space(8.0);

    if app.chat.sessions.is_empty() {
        ui.label(
            italic_text(
                if app.ipc_state.connected {
                    "Loading sessions…"
                } else {
                    "Start the BE to load sessions."
                },
                13.0,
            )
            .color(rgb(p.muted)),
        );
        return;
    }

    let rows: Vec<(u64, String)> = app
        .chat
        .sessions
        .iter()
        .map(|s| (s.id, s.title.clone()))
        .collect();
    let active_id = app.chat.session_id;
    let pending = app.chat.pending_delete;
    let allow_delete = rows.len() > 1;

    let mut action: Option<RowAction> = None;

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (i, (id, title)) in rows.iter().enumerate() {
                let is_active = *id == active_id;
                let is_pending = pending == Some(*id);
                draw_row(
                    ui,
                    &p,
                    *id,
                    title,
                    is_active,
                    allow_delete,
                    is_pending,
                    &mut |a| action = Some(a),
                );
                if i + 1 < rows.len() {
                    hairline(ui, &p);
                }
            }
        });

    match action {
        Some(RowAction::Switch(id)) => app.switch_session(id),
        Some(RowAction::Arm(id)) => app.chat.pending_delete = Some(id),
        Some(RowAction::Cancel) => app.chat.pending_delete = None,
        Some(RowAction::Confirm(id)) => app.delete_session(id),
        None => {}
    }
}

enum RowAction {
    Switch(u64),
    Arm(u64),
    Cancel,
    Confirm(u64),
}

#[allow(clippy::too_many_arguments)]
fn draw_row(
    ui: &mut egui::Ui,
    p: &sica_core::theme::Palette,
    id: u64,
    title: &str,
    is_active: bool,
    allow_delete: bool,
    is_pending: bool,
    on_action: &mut dyn FnMut(RowAction),
) {
    let avail = ui.available_width();
    let row_h = 32.0;
    let (rect, row_resp) = ui.allocate_exact_size(Vec2::new(avail, row_h), Sense::click());

    // Active = 2px accent slab on the left edge. Hover = subtle wash (but not
    // while the row is armed — the confirm state owns the row's chrome).
    if is_active {
        let slab = Rect::from_min_size(rect.min, Vec2::new(2.0, rect.height()));
        ui.painter().rect_filled(slab, 0.0, rgb(p.accent));
    } else if row_resp.hovered() && !is_pending {
        ui.painter().rect_filled(rect, 0.0, rgb(p.accent_subtle));
    }

    // Title — left-anchored mono, ink for active, muted otherwise.
    let title_color = if is_active { rgb(p.ink) } else { rgb(p.muted) };
    let title_pos = egui::pos2(rect.min.x + 12.0, rect.center().y);
    ui.painter().text(
        title_pos,
        Align2::LEFT_CENTER,
        title,
        FontId::new(13.0, FontFamily::Monospace),
        title_color,
    );

    if is_pending {
        // Inline confirm: "Delete" (accent) then "Keep" (muted), right-aligned.
        // Each gets its own id-keyed hit rect so clicks never bleed.
        let font = FontId::new(12.0, FontFamily::Proportional);
        let cy = rect.center().y;

        let keep = confirm_word(ui, p, rect.max.x - 10.0, cy, &font, "Keep", false);
        let del = confirm_word(ui, p, keep.rect.min.x - 12.0, cy, &font, "Delete", true);

        // Priority: confirm beats cancel beats a stray row-body click (which
        // also cancels, giving an easy escape hatch). else-if avoids one click
        // emitting two actions.
        if del.clicked {
            on_action(RowAction::Confirm(id));
        } else if keep.clicked || row_resp.clicked() {
            on_action(RowAction::Cancel);
        }
        return;
    }

    // Trailing × — arms the delete. Only on hover or when active, and only if
    // there's more than one session to keep.
    if allow_delete && (row_resp.hovered() || is_active) {
        let x_size = 18.0;
        let x_rect = Rect::from_min_size(
            egui::pos2(rect.max.x - x_size - 8.0, rect.center().y - x_size / 2.0),
            Vec2::splat(x_size),
        );
        let x_resp = ui.interact(x_rect, ui.id().with(("del", id)), Sense::click());
        // Filled backing so the control is visible against any row state:
        // sunk surface at rest, accent fill (with page-colour glyph) on hover.
        let (x_fill, x_stroke, x_color) = if x_resp.hovered() {
            (rgb(p.accent), egui::Stroke::new(1.0, rgb(p.accent)), rgb(p.page_bg))
        } else {
            (rgb(p.surface_sunk), egui::Stroke::new(1.0, rgb(p.hairline)), rgb(p.muted))
        };
        ui.painter().rect(x_rect, 2.0, x_fill, x_stroke);
        ui.painter().text(
            x_rect.center(),
            Align2::CENTER_CENTER,
            "×",
            FontId::new(14.0, FontFamily::Monospace),
            x_color,
        );
        if x_resp.on_hover_text("Delete session").clicked() {
            on_action(RowAction::Arm(id));
            return;
        }
        // Clicking the row body (anywhere but the ×) switches sessions.
        if row_resp.clicked() {
            on_action(RowAction::Switch(id));
        }
    } else if row_resp.clicked() {
        on_action(RowAction::Switch(id));
    }
}

struct WordHit {
    rect: Rect,
    clicked: bool,
}

/// Paint a right-anchored clickable pill (its right edge at `right_x`) and
/// return its rect + click state. Both choices get a filled background so
/// they read as buttons against any row state: `danger` is a solid accent
/// fill with page-colour text; the safe choice is a sunk-surface fill with
/// a hairline border, brightening on hover.
fn confirm_word(
    ui: &mut egui::Ui,
    p: &sica_core::theme::Palette,
    right_x: f32,
    center_y: f32,
    font: &FontId,
    text: &str,
    danger: bool,
) -> WordHit {
    let pad = egui::vec2(8.0, 3.0);
    let galley = ui
        .fonts(|f| f.layout_no_wrap(text.to_owned(), font.clone(), rgb(p.muted)));
    let size = galley.size() + pad * 2.0;
    let rect = Rect::from_min_size(
        egui::pos2(right_x - size.x, center_y - size.y / 2.0),
        size,
    );
    let resp = ui.interact(rect, ui.id().with(("confirm", text, rect.min.x as i32)), Sense::click());
    let (fill, stroke, text_color) = if danger {
        let fill = if resp.hovered() { rgb(p.accent_hover) } else { rgb(p.accent) };
        (fill, egui::Stroke::new(1.0, fill), rgb(p.page_bg))
    } else {
        let fill = if resp.hovered() { rgb(p.surface) } else { rgb(p.surface_sunk) };
        let ink = if resp.hovered() { rgb(p.ink) } else { rgb(p.muted) };
        (fill, egui::Stroke::new(1.0, rgb(p.hairline)), ink)
    };
    ui.painter().rect(rect, 2.0, fill, stroke);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        font.clone(),
        text_color,
    );
    WordHit { rect, clicked: resp.clicked() }
}
