//! "Communication" settings tab — the original IPC demo (BE start/stop, build,
//! synthetic Counter/Fib/Echo requests, and the raw log panel), relocated here
//! so the chat view stays focused. Behavior is unchanged from the legacy UI.

use crate::app::App;
use crate::ui::controls;
use crate::ui::log_panel;

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    controls::draw_top(app, ui);
    ui.separator();
    controls::draw_request(app, ui);
    ui.separator();
    log_panel::draw(app, ui);
}
