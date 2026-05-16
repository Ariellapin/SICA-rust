//! Top-level UI layout. The status bar (dots only) sits at the bottom, a thin
//! vertical sidebar on the left switches between `Chat` and `Settings`, and the
//! central panel routes to whichever view is active. In the Chat view a second
//! left panel lists chat sessions.

mod chat;
mod controls;
pub mod fonts;
mod log_panel;
mod sessions_panel;
mod settings;
mod sidebar;
mod status_bar;
pub mod widgets;

use crate::app::{App, AppView};

pub fn draw(app: &mut App, ctx: &egui::Context) {
    egui::TopBottomPanel::bottom("status")
        .show_separator_line(false)
        .show(ctx, |ui| status_bar::draw(app, ui));

    egui::SidePanel::left("sidebar")
        .resizable(false)
        .exact_width(56.0)
        .show_separator_line(false)
        .show(ctx, |ui| sidebar::draw(app, ui));

    if matches!(app.view, AppView::Chat) {
        egui::SidePanel::left("sessions")
            .resizable(true)
            .default_width(220.0)
            .min_width(160.0)
            .show(ctx, |ui| sessions_panel::draw(app, ui));
    }

    egui::CentralPanel::default().show(ctx, |ui| match app.view {
        AppView::Chat => chat::draw(app, ui),
        AppView::Settings => settings::draw(app, ui),
    });
}
