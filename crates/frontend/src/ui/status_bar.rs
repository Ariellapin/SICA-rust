//! Bottom status strip. A subsystem-specific icon + tracked-caps label naming
//! the subsystem, middot separators, project folder and active model in the
//! middle, token meter on the right, and the italic-serif brandmark at the
//! far edge. When the on-disk source has drifted away from what the running
//! BE was built from, a pulsing "RESTART" button appears in front of the
//! brandmark; clicking it issues a `RebuildAndRestart`.

use sica_core::theme::tokens::{HAIRLINE, RADIUS_2};

use crate::app::{rgb, App};
use crate::supervisor::UiCommand;
use crate::ui::widgets::{caps_job, caps_label, display_text, right_aligned, status_icon, StatusKind};

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

            if app.be_state.restart_pending() {
                ui.label(egui::RichText::new(" · ").color(muted));
                draw_restart_button(app, ui);
            }
        });
    });
    ui.add_space(2.0);
}

/// Pulsing "RESTART" pill shown when the on-disk source has drifted from the
/// running BE. Click → `RebuildAndRestart`. Disabled while a build is in
/// flight so repeated clicks can't stack respawns.
fn draw_restart_button(app: &mut App, ui: &mut egui::Ui) {
    let p = app.palette;
    let busy = app.build_state.in_flight;

    // Pulse — sine wave on a 1.2s period. egui needs a repaint scheduled to
    // keep the animation moving even when nothing else is invalidating the
    // frame.
    let t = ui.input(|i| i.time);
    let phase = (t as f32 * std::f32::consts::TAU / 1.2).sin() * 0.5 + 0.5;
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));

    let accent = rgb(p.accent);
    let subtle = rgb(p.accent_subtle);
    let on_accent = rgb(p.page_bg);
    let fill = lerp_color(subtle, accent, phase);
    let label_color = if busy { rgb(p.muted) } else { on_accent };

    let resp = ui.add_enabled(
        !busy,
        egui::Button::new(caps_job("⟳ RESTART", label_color, 11.0))
            .fill(fill)
            .stroke(egui::Stroke::new(HAIRLINE, accent))
            .rounding(egui::Rounding::same(RADIUS_2))
            .min_size(egui::Vec2::new(0.0, 22.0)),
    );
    let resp = resp.on_hover_text(format!(
        "Source has changed since the BE was built.\nBE: {}\nSrc: {}",
        app.be_state.running_version.as_deref().unwrap_or("—"),
        app.be_state.source_version.as_deref().unwrap_or("—"),
    ));
    if resp.clicked() {
        app.send(UiCommand::RebuildAndRestart { release: app.release_profile });
    }
}

fn lerp_color(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| -> u8 {
        let xf = x as f32;
        let yf = y as f32;
        (xf + (yf - xf) * t).round().clamp(0.0, 255.0) as u8
    };
    egui::Color32::from_rgba_unmultiplied(
        lerp(a.r(), b.r()),
        lerp(a.g(), b.g()),
        lerp(a.b(), b.b()),
        lerp(a.a(), b.a()),
    )
}

fn sep(ui: &mut egui::Ui, color: egui::Color32) {
    ui.label(egui::RichText::new(" · ").color(color));
}
