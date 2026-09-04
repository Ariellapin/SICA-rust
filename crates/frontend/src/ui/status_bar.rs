//! Bottom status strip. A subsystem-specific icon + tracked-caps label naming
//! the subsystem, middot separators, project folder and active model in the
//! middle, context meter on the right, and the italic-serif brandmark at the
//! far edge. When the on-disk source has drifted away from what the running
//! BE was built from, a pulsing "RESTART" button appears in front of the
//! brandmark; clicking it issues a `RebuildAndRestart`.
//!
//! The strip sits on a near-black band, so labels are painted white rather
//! than in any page-tuned palette tone: at 9pt those tones sat near the band
//! and were effectively unreadable. Only the middot separators are dimmed,
//! since they are chrome rather than content.

use sica_core::theme::tokens::{HAIRLINE, RADIUS_2};

use crate::app::{rgb, App};
use crate::supervisor::UiCommand;
use crate::ui::widgets::{caps_job, display_text, right_aligned, status_icon, StatusKind};

const FOOTER_FONT_PT: f32 = 9.0;

/// Label colour for the strip.
const INK_COLOR: egui::Color32 = egui::Color32::WHITE;
/// Middot separators — chrome, so they sit back from the labels.
const SEP_COLOR: egui::Color32 = egui::Color32::from_rgb(0x80, 0x80, 0x80);

/// Percentage at which the meter turns amber — one step ahead of the
/// backend's compaction trigger, so a full context is visible before it fires.
const WARN_PCT: u32 = 80;

fn tiny_caps(ui: &mut egui::Ui, text: &str, color: egui::Color32) -> egui::Response {
    ui.add(egui::Label::new(caps_job(text, color, FOOTER_FONT_PT)).selectable(false))
}

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let p = app.palette;
    let ok_color    = rgb(p.ok);
    let err_color   = rgb(p.danger);
    let idle_color  = rgb(p.hairline);
    // The strip paints on a near-black band, so its labels are white rather
    // than the page `ink`, and the separators are a dimmed white instead of
    // the palette `muted` — which is tuned for the light page and disappears
    // against the band.
    let muted       = SEP_COLOR;
    let ink         = INK_COLOR;

    let prev_spacing = ui.spacing().item_spacing;
    ui.spacing_mut().item_spacing.y = 0.0;
    ui.spacing_mut().item_spacing.x = 4.0;
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
        tiny_caps(ui, "BE", ink);

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
        tiny_caps(ui, "IPC", ink);

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
        tiny_caps(ui, "LLM", ink);

        sep(ui, muted);

        // FOLDER — project the agent is operating on.
        tiny_caps(ui, &format!("FOLDER  {}", app.workspace_name.to_uppercase()), ink);

        sep(ui, muted);

        // MODEL — currently connected LLM model id, or "—" when not ready.
        let model = match &app.llm_state.state {
            protocol::LlmState::Ready { model, .. } => model.clone(),
            _ => "—".to_string(),
        };
        tiny_caps(ui, &format!("MODEL  {}", model.to_uppercase()), ink);

        sep(ui, muted);
        draw_permission_pill(app, ui, ink);

        // Right edge lays out right-to-left, so items added later sit further
        // left: brandmark, token counts, context percentage, generation speed,
        // compaction mark.
        right_aligned(ui, |ui| {
            ui.label(display_text("sica", 11.0).color(ink));
            sep(ui, muted);
            draw_context_meter(app, ui, ink);
            sep(ui, muted);
            draw_gen_speed(app, ui, ink);

            if app.be_state.restart_pending() {
                sep(ui, muted);
                draw_restart_button(app, ui);
            }
        });
    });
    ui.spacing_mut().item_spacing = prev_spacing;
}

/// Permission-mode pill: the session's current mode, right-click (or long
/// press) for the three-way switch. Danger mode paints red — it should
/// always look alarming.
fn draw_permission_pill(app: &mut App, ui: &mut egui::Ui, ink: egui::Color32) {
    let mode = app.permission_mode;
    let color = match mode {
        protocol::PermissionMode::ReadOnly => rgb(app.palette.info),
        protocol::PermissionMode::WorkspaceWrite => ink,
        protocol::PermissionMode::DangerFullAccess => rgb(app.palette.danger),
    };
    let resp = tiny_caps(ui, &format!("PERM  {}", mode.label().to_uppercase()), color)
        .on_hover_text(format!("{} — right-click to switch", mode.description()));
    resp.context_menu(|ui| {
        for m in [
            protocol::PermissionMode::ReadOnly,
            protocol::PermissionMode::WorkspaceWrite,
            protocol::PermissionMode::DangerFullAccess,
        ] {
            let mut label = format!("{} — {}", m.label(), m.description());
            if m == mode {
                label.push_str("  ✓");
            }
            if ui.button(label).clicked() {
                let id = app.chat.session_id;
                app.send(crate::supervisor::UiCommand::SendRequest(
                    protocol::Request::SetPermissionMode { session_id: id, mode: m },
                ));
                ui.close_menu();
            }
        }
    });
}

/// Context meter: `CTX 42% · 8145 / 19392`, plus a "COMPRESSING" mark while the
/// backend is folding history.
///
/// The percentage is measured against the *prompt budget* (the window minus the
/// reserve held back for the reply), not the raw window — that is the space a
/// prompt can actually occupy, and it is the same denominator the backend's
/// auto-compaction triggers on, so the reading hits 95% exactly when
/// compression fires. The absolute counts show `used / budget` for the same
/// reason; the full window is in the tooltip.
fn draw_context_meter(app: &App, ui: &mut egui::Ui, ink: egui::Color32) {
    use std::sync::atomic::Ordering;

    let p = app.palette;
    let muted = SEP_COLOR;
    let used = app.tokens.used.load(Ordering::Relaxed);
    let limit = app.tokens.limit.load(Ordering::Relaxed);
    let budget = app.tokens.budget.load(Ordering::Relaxed);
    let pct = app.tokens.pct();

    let count_label = if budget > 0 {
        format!("{used} / {budget}")
    } else {
        format!("{used} / {limit}")
    };
    tiny_caps(ui, &count_label, ink);
    sep(ui, muted);

    let (pct_text, pct_color) = match pct {
        Some(v) if v >= protocol::COMPACT_TRIGGER_PCT => (format!("CTX  {v}%"), rgb(p.danger)),
        Some(v) if v >= WARN_PCT => (format!("CTX  {v}%"), rgb(p.warn)),
        Some(v) => (format!("CTX  {v}%"), ink),
        // No turn has run yet, so the reply reserve — and therefore the real
        // denominator — isn't known.
        None => ("CTX  —".to_string(), muted),
    };
    let resp = tiny_caps(ui, &pct_text, pct_color);
    let reserve = limit.saturating_sub(budget);
    resp.on_hover_text(match pct {
        Some(v) => format!(
            "{used} of {budget} prompt tokens ({v}%).\n\
             Model window {limit}; {reserve} reserved for the reply.\n\
             Older history is compressed automatically at {}%.",
            protocol::COMPACT_TRIGGER_PCT,
        ),
        None => format!(
            "Context window {limit} tokens.\nUsage appears once a turn has run."
        ),
    });

    if app.chat.compacting {
        sep(ui, muted);
        // Repaint keeps the caps mark from going stale if nothing else is
        // invalidating the frame while the summarizer round-trip is in flight.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        tiny_caps(ui, "⟳ COMPRESSING", rgb(p.info))
            .on_hover_text("Summarising older history to free context window space.");
    }
}

/// Generation speed: live tok/s while a turn streams, frozen turn average
/// after it lands, dash before the first turn.
///
/// Derived in the FE from successive `TokenUsage.used` deltas (prompt +
/// generated-so-far, emitted ~every 100 ms), so no protocol change is needed:
/// the first reading of each turn is the prompt baseline and only the growth
/// past it counts. Tooltip carries the generated count + wall time behind
/// the rate.
fn draw_gen_speed(app: &App, ui: &mut egui::Ui, ink: egui::Color32) {
    let g = &app.gen_speed;
    if g.streaming {
        // Keep the readout ticking even if `TokenUsage` stalls mid-stream.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
    }
    let (text, color, tip) = if g.streaming {
        let avg = g.avg();
        (
            format!("TOK/S  {:.1}", g.tps),
            if g.tps > 0.0 { rgb(app.palette.ok) } else { ink },
            format!(
                "Generating… {} tokens in {:.1}s (avg {:.1} tok/s).",
                g.completed, g.elapsed_secs, avg,
            ),
        )
    } else if g.tps > 0.0 {
        (
            format!("TOK/S  {:.1}", g.tps),
            ink,
            format!(
                "Last turn: {} generated tokens in {:.1}s ({:.1} tok/s).",
                g.completed, g.elapsed_secs, g.tps,
            ),
        )
    } else {
        (
            "TOK/S  —".to_string(),
            SEP_COLOR,
            "Generation speed appears once a turn streams.".to_string(),
        )
    };
    tiny_caps(ui, &text, color).on_hover_text(tip);
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
        egui::Button::new(caps_job("⟳ RESTART", label_color, 9.0))
            .fill(fill)
            .stroke(egui::Stroke::new(HAIRLINE, accent))
            .rounding(egui::Rounding::same(RADIUS_2))
            .min_size(egui::Vec2::new(0.0, 16.0)),
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
    ui.label(egui::RichText::new(" · ").color(color).size(FOOTER_FONT_PT));
}
