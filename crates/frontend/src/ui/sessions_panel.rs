//! Sessions list — second column in Chat view. Editorial flat rows: title
//! in mono, hairline between rows, a 2px accent slab on the left edge marks
//! the active row.

use egui::{Rect, Sense, Vec2};

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
    let allow_delete = rows.len() > 1;
    let mut clicked_switch: Option<u64> = None;
    let mut clicked_delete: Option<u64> = None;

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (i, (id, title)) in rows.iter().enumerate() {
                let is_active = *id == active_id;
                draw_row(
                    ui,
                    &p,
                    title,
                    is_active,
                    allow_delete,
                    &mut |action| match action {
                        RowAction::Switch => clicked_switch = Some(*id),
                        RowAction::Delete => clicked_delete = Some(*id),
                    },
                );
                if i + 1 < rows.len() {
                    hairline(ui, &p);
                }
            }
        });

    if let Some(id) = clicked_delete {
        app.delete_session(id);
    } else if let Some(id) = clicked_switch {
        app.switch_session(id);
    }
}

enum RowAction { Switch, Delete }

fn draw_row(
    ui: &mut egui::Ui,
    p: &sica_core::theme::Palette,
    title: &str,
    is_active: bool,
    allow_delete: bool,
    on_action: &mut dyn FnMut(RowAction),
) {
    let avail = ui.available_width();
    let row_h = 32.0;
    let (rect, row_resp) = ui.allocate_exact_size(Vec2::new(avail, row_h), Sense::click());

    // Active = 2px accent slab on the left edge.
    if is_active {
        let slab = Rect::from_min_size(rect.min, Vec2::new(2.0, rect.height()));
        ui.painter().rect_filled(slab, 0.0, rgb(p.accent));
    } else if row_resp.hovered() {
        // Hover = subtle wash across the row.
        ui.painter().rect_filled(rect, 0.0, rgb(p.accent_subtle));
    }

    if row_resp.clicked() {
        on_action(RowAction::Switch);
    }

    // Title — left-anchored mono, ink for active, muted otherwise.
    let title_color = if is_active { rgb(p.ink) } else { rgb(p.muted) };
    let title_pos = egui::pos2(rect.min.x + 12.0, rect.center().y);
    let painter = ui.painter().clone();
    painter.text(
        title_pos,
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::new(13.0, egui::FontFamily::Monospace),
        title_color,
    );

    // Trailing × — only show on hover or when active, and only if allow_delete.
    if allow_delete && (row_resp.hovered() || is_active) {
        let x_size = 18.0;
        let x_rect = Rect::from_min_size(
            egui::pos2(rect.max.x - x_size - 8.0, rect.center().y - x_size / 2.0),
            Vec2::splat(x_size),
        );
        let x_resp = ui.interact(x_rect, ui.id().with(("del", title)), Sense::click());
        let x_color = if x_resp.hovered() { rgb(p.accent) } else { rgb(p.muted) };
        ui.painter().text(
            x_rect.center(),
            egui::Align2::CENTER_CENTER,
            "×",
            egui::FontId::new(14.0, egui::FontFamily::Monospace),
            x_color,
        );
        if x_resp.on_hover_text("Delete session").clicked() {
            on_action(RowAction::Delete);
        }
    }
}
