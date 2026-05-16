//! Bottom status strip. A subsystem-specific icon + tracked-caps label naming
//! the subsystem, middot separators, project folder and active model in the
//! middle, token meter on the right, and the italic-serif brandmark at the
//! far edge.

use crate::app::{rgb, App};
use crate::ui::widgets::{caps_label, display_text, right_aligned, status_icon, StatusKind};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let p = app.palette;
    let ok_color    = rgb(p.ok);
    let err_color   = rgb(p.danger);
    let idle_color  = rgb(p.hairline);
    let muted       = rgb(p.muted);

    ui.add_space(2.0);
    ui.horizontal(|ui| {
        // BE
        let be_connected = app.be_state.running;
        let (be_color, be_label, be_detail) = if be_connected {
            (ok_color, format!("BE  RUNNING (pid {})", app.be_state.pid.unwrap_or(0)), None)
        } else if let Some(err) = app.be_state.last_error.clone() {
            (err_color, "BE  STOPPED".to_string(), Some(err))
        } else {
            (idle_color, "BE  STOPPED".to_string(), None)
        };
        status_icon(ui, StatusKind::Be, be_connected, be_color, &be_label, be_detail.as_deref(), err_color);
        caps_label(ui, "BE", muted);

        sep(ui, muted);

        // IPC
        let ipc_connected = app.ipc_state.connected && !app.ipc_state.heartbeat_timeout;
        let (ipc_color, ipc_label, ipc_detail) = if ipc_connected {
            (ok_color, "IPC  CONNECTED".to_string(), None)
        } else if app.ipc_state.connected && app.ipc_state.heartbeat_timeout {
            (err_color, "IPC  HEARTBEAT TIMEOUT".to_string(), Some("no heartbeat for >5s".to_string()))
        } else if let Some(err) = app.ipc_state.last_error.clone() {
            (err_color, "IPC  DISCONNECTED".to_string(), Some(err))
        } else {
            (idle_color, "IPC  DISCONNECTED".to_string(), None)
        };
        status_icon(ui, StatusKind::Ipc, ipc_connected, ipc_color, &ipc_label, ipc_detail.as_deref(), err_color);
        caps_label(ui, "IPC", muted);

        sep(ui, muted);

        // LLM
        let llm_label = format!("LLM  {}", app.llm_state.label().to_uppercase());
        let llm_connected = matches!(app.llm_state.state, protocol::LlmState::Ready { .. });
        let (llm_color, llm_detail) = match &app.llm_state.state {
            protocol::LlmState::Ready { .. }   => (ok_color, None),
            protocol::LlmState::Connecting     => (rgb(p.warn), None),
            protocol::LlmState::Error { message } => (err_color, Some(message.clone())),
            protocol::LlmState::Disconnected   => (idle_color, None),
        };
        status_icon(ui, StatusKind::Llm, llm_connected, llm_color, &llm_label, llm_detail.as_deref(), err_color);
        caps_label(ui, "LLM", muted);

        sep(ui, muted);

        // FOLDER — project the agent is operating on.
        caps_label(ui, &format!("FOLDER  {}", app.workspace_name.to_uppercase()), muted);

        sep(ui, muted);

        // MODEL — currently connected LLM model id, or "—" when not ready.
        let model = match &app.llm_state.state {
            protocol::LlmState::Ready { model, .. } => model.clone(),
            _ => "—".to_string(),
        };
        caps_label(ui, &format!("MODEL  {}", model.to_uppercase()), muted);

        right_aligned(ui, |ui| {
            ui.label(display_text("sica", 14.0).color(muted));
            ui.label(egui::RichText::new(" · ").color(muted));
            let used = app.tokens.used.load(std::sync::atomic::Ordering::Relaxed);
            let limit = app.tokens.limit.load(std::sync::atomic::Ordering::Relaxed);
            caps_label(ui, &format!("{used} / {limit}"), muted);
        });
    });
    ui.add_space(2.0);
}

fn sep(ui: &mut egui::Ui, color: egui::Color32) {
    ui.label(egui::RichText::new(" · ").color(color));
}
