mod controls;
mod log_panel;
mod status_bar;

use crate::app::App;

pub fn draw(app: &mut App, ctx: &egui::Context) {
    egui::TopBottomPanel::top("top").show(ctx, |ui| controls::draw_top(app, ui));
    egui::TopBottomPanel::top("request").show(ctx, |ui| controls::draw_request(app, ui));
    egui::TopBottomPanel::bottom("status").show(ctx, |ui| status_bar::draw(app, ui));
    egui::CentralPanel::default().show(ctx, |ui| log_panel::draw(app, ui));
}
