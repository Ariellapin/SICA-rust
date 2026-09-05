//! The composer (§5): an r=22 card on `input_major` with elevation-soft, a
//! toolbar row under the text, and the dock stack above it. Focus shows only
//! through the caret — the card itself never changes fill or stroke.
//!
//! Toolbar: left `[+ opens the / menu] [permission chip] [plan chip]`, right
//! `[model select] [context ring] [send/stop]`. There is no attach button:
//! images enter by paste, by drop, and through the `+` menu's picker.
//!
//! Keymap (§5.1): Enter submits (queueing behind a running turn, per the
//! busy-Enter preference); Ctrl+Enter is the *accelerated* submit — the
//! opposite of that preference, so it steers when Enter queues; Shift+Enter
//! is a newline unconditionally; Esc closes the `/` menu first and only then
//! interrupts.

use std::path::Path;

use base64::Engine as _;
use egui::{Align, Align2, Layout, Rect, Rounding, Sense, Stroke, Vec2};

use protocol::Request;
use sica_core::theme::tokens::{HAIRLINE, RADIUS_BUBBLE, RADIUS_PILL};

use crate::app::{App, BusyEnter, LogKind, PendingAttachment};
use crate::supervisor::UiCommand;
use crate::ui::icons::{self, Icon};
use crate::ui::kit::{self, DotState, Elevation, Level, Weight};

const MIN_INPUT_ROWS: usize = 1;
/// 14 lines, after which the field scrolls internally (336 px at 24 px).
const MAX_INPUT_ROWS: usize = 14;
const THUMB_SIZE: f32 = 56.0;
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const SEND_CIRCLE: f32 = 34.0;

pub fn draw(app: &mut App, ui: &mut egui::Ui, disabled: bool) {
    handle_dropped_files(app, ui);
    handle_paste(app, ui);

    // Approval and questions **replace** the composer in place; the
    // transcript above stays scrollable (§6.1, §6.2).
    if super::control::takeover(app, ui) {
        return;
    }

    super::dock::draw(app, ui);
    card(app, ui, disabled);
    super::dock::stats_line(app, ui);
}

fn card(app: &mut App, ui: &mut egui::Ui, disabled: bool) {
    let t = app.theme;
    let input_id = egui::Id::new("chat_input_field");
    let input_focused = ui.memory(|m| m.has_focus(input_id));

    // The `/` palette owns ↑↓/Enter/Tab/Esc while open, so it runs first.
    // The `@` picker takes the same keys, and the two are mutually exclusive
    // — a slash query ends at the first whitespace and an `@` token opens
    // after one — so it only gets a look once the palette is closed.
    let slash = super::slash_menu::draw(app, ui, input_focused);
    let at = if slash.open {
        super::at_menu::Outcome { open: false }
    } else {
        super::at_menu::draw(app, ui, input_focused)
    };
    let picker_open = slash.open || at.open;
    if !picker_open {
        handle_escape(app, ui);
    }

    let turn_in_flight = last_turn_in_flight(app);
    let keys = read_submit_keys(ui, input_focused && !disabled && !picker_open);

    // Resolved out here: the card's closure borrows the draft mutably for
    // the text field, and the hint is read off the same draft.
    let ghost = ghost_hint(app);

    let card = kit::elevated_frame(
        &t,
        Elevation::Soft,
        Level::L2,
        kit::col(t.alias.input_major),
        RADIUS_BUBBLE,
    )
    .inner_margin(egui::Margin {
        left: 14.0,
        right: 8.0,
        top: 10.0,
        bottom: 6.0,
    })
    .show(ui, |ui| {
        if !app.chat.pending_images.is_empty() {
            draw_pending_strip(app, ui);
            ui.add_space(6.0);
        }

        let hint = placeholder(app, disabled, turn_in_flight);
        let font_id = egui::TextStyle::Body.resolve(ui.style());
        let row_h = ui.fonts(|f| f.row_height(&font_id));
        let text_w = (ui.available_width() - 8.0).max(80.0);
        let rows = wrapped_rows(ui, &app.chat.draft, &font_id, text_w)
            .clamp(MIN_INPUT_ROWS, MAX_INPUT_ROWS);
        let field_h = row_h * rows as f32 + 4.0;
        ui.allocate_ui(Vec2::new(ui.available_width(), field_h), |ui| {
            egui::ScrollArea::vertical()
                .id_source("chat_input_scroll")
                .show(ui, |ui| {
                    let input = egui::TextEdit::multiline(&mut app.chat.draft)
                        .id(input_id)
                        .hint_text(kit::txt(
                            hint,
                            t.content_px as f32,
                            Weight::Regular,
                            kit::col(t.alias.label[3]),
                        ))
                        .desired_width(ui.available_width())
                        .desired_rows(rows)
                        .return_key(Some(egui::KeyboardShortcut::new(
                            egui::Modifiers::SHIFT,
                            egui::Key::Enter,
                        )))
                        .frame(false);
                    let out = ui.add_enabled_ui(!disabled, |ui| input.show(ui)).inner;
                    if let Some(hint) = &ghost {
                        paint_ghost(ui, &out, hint, &font_id, &t);
                    }
                    out.response
                });
        });

        ui.add_space(8.0);
        toolbar(app, ui, disabled, turn_in_flight, input_id);
    });
    // What the `/` and `@` menus anchor to next frame (§6.3).
    app.chat.composer_rect = Some(card.response.rect);

    // Submission. `busy_enter` decides plain Enter while a turn runs;
    // Ctrl+Enter always does the other thing.
    let can_submit =
        !disabled && (!app.chat.draft.trim().is_empty() || !app.chat.pending_images.is_empty());
    if !can_submit {
        // …except the empty-draft accelerator, which steers the whole queue.
        if keys.accelerated && turn_in_flight && !app.chat.queued.is_empty() {
            steer_queue(app);
        }
        return;
    }
    let steer = if !turn_in_flight {
        false
    } else {
        match (app.busy_enter, keys.accelerated) {
            (BusyEnter::Queue, false) => false,
            (BusyEnter::Queue, true) => true,
            (BusyEnter::Steer, false) => true,
            (BusyEnter::Steer, true) => false,
        }
    };
    if keys.submit || keys.accelerated {
        send_message(app, steer);
        ui.memory_mut(|m| m.request_focus(input_id));
    }
}

fn placeholder(app: &App, disabled: bool, busy: bool) -> &'static str {
    if disabled {
        "No model connected — select one to continue"
    } else if busy {
        match app.busy_enter {
            BusyEnter::Queue => "Enter queues · Ctrl+Enter steers this turn",
            BusyEnter::Steer => "Enter steers this turn · Ctrl+Enter queues",
        }
    } else if app.plan_active {
        "describe your task to generate plan"
    } else if app.chat.turns.is_empty() {
        "Describe what you want to build... / commands, @ files or sessions"
    } else {
        "Message or run a task... / commands, @ files or sessions"
    }
}

// ---------------------------------------------------------------------------
// Toolbar
// ---------------------------------------------------------------------------

fn toolbar(
    app: &mut App,
    ui: &mut egui::Ui,
    disabled: bool,
    turn_in_flight: bool,
    input_id: egui::Id,
) {
    ui.horizontal(|ui| {
        // `+` opens the / menu with an empty query — the palette already
        // handles `""` — and long-press-free: the picker is the same one.
        if kit::icon_button(ui, Icon::Plus, 28.0)
            .on_hover_text("Commands, skills and files")
            .clicked()
        {
            if !app.chat.draft.starts_with('/') {
                app.chat.draft.insert(0, '/');
            }
            app.chat.slash.dismissed = false;
            ui.memory_mut(|m| m.request_focus(input_id));
        }
        permission_chip(app, ui);
        if app.plan_active {
            plan_chip(app, ui);
        }
        if app.session_agent.is_some() {
            agent_chip(app, ui);
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            send_button(app, ui, disabled, turn_in_flight);
            ui.add_space(4.0);
            super::meter::context_ring(app, ui);
            ui.add_space(4.0);
            model_select(app, ui);
        });
    });
}

fn send_button(app: &mut App, ui: &mut egui::Ui, disabled: bool, turn_in_flight: bool) {
    let t = app.theme;
    let draft_empty = app.chat.draft.trim().is_empty() && app.chat.pending_images.is_empty();
    let stop = turn_in_flight && draft_empty;
    let stopping = app.chat.interrupt_requested;
    let enabled = !disabled && (stop || !draft_empty) && !(stop && stopping);

    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(SEND_CIRCLE), Sense::click());
    let alpha = if enabled { 1.0 } else { 0.4 };
    let fill = if resp.hovered() && enabled {
        kit::col(t.alias.info_hover)
    } else {
        kit::col(t.alias.info_fill)
    };
    ui.painter()
        .circle_filled(rect.center(), SEND_CIRCLE / 2.0, fill.linear_multiply(alpha));
    icons::paint(
        ui.painter(),
        Rect::from_center_size(rect.center(), Vec2::splat(15.0)),
        if stop { Icon::Stop } else { Icon::ArrowUp },
        egui::Color32::WHITE.linear_multiply(alpha),
    );
    let tip = if stop {
        if stopping { "Stopping…" } else { "Stop generating" }
    } else if turn_in_flight {
        match app.busy_enter {
            BusyEnter::Queue => "Queue this message",
            BusyEnter::Steer => "Steer this turn",
        }
    } else {
        "Send"
    };
    if resp.on_hover_text(tip).clicked() && enabled {
        if stop {
            app.interrupt_turn();
        } else {
            let steer = turn_in_flight && app.busy_enter == BusyEnter::Steer;
            send_message(app, steer);
        }
    }
}

/// The nudge painted after a *claimed* command (§6.3). Picking a row that
/// takes arguments leaves `/name ` in the draft, and a non-empty draft never
/// shows the placeholder — so the hint is painted after the token instead,
/// in the dimmest label colour.
///
/// `None` unless the draft is exactly that one claimed token: once an
/// argument is being typed the nudge has done its job. dsh also hints
/// `/plan`, which here has nothing to hint — sica's `/plan` toggles on accept
/// rather than claiming the token.
fn ghost_hint(app: &App) -> Option<String> {
    let name = claimed_command(&app.chat.draft)?;
    match name {
        "goal" => Some(
            match &app.goal {
                // A terminal goal is not one you can pause or resume, so it
                // reads as no goal at all here.
                Some(g) if g.phase != protocol::GoalPhase::Completed => {
                    "goal active — edit / pause / resume / clear"
                }
                _ => "describe the objective for a long-running task",
            }
            .to_string(),
        ),
        "permission" => Some("read-only · workspace-write · danger-full-access".to_string()),
        "agent" => Some(match &app.session_agent {
            Some(n) => format!("running {n} — name another, or `off` to clear"),
            None => "name an agents/*.md persona, or `off`".to_string(),
        }),
        // Everything else is a catalogue row: its declared argument names are
        // the most useful thing to say, and a row without any says nothing.
        _ => {
            let entry = app.chat.slash.entries.iter().find(|e| e.name == name)?;
            if entry.args.is_empty() {
                return None;
            }
            Some(
                entry
                    .args
                    .iter()
                    .map(|a| format!("<{a}>"))
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        }
    }
}

/// The command name of a draft that is exactly one claimed token — `/name `
/// with the trailing space the palette leaves and nothing after it.
fn claimed_command(draft: &str) -> Option<&str> {
    let name = draft.strip_prefix('/')?.strip_suffix(' ')?;
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    Some(name)
}

/// Paint `hint` where the caret sits, elided to the field's own clip rect so
/// a long hint never spills past the card.
fn paint_ghost(
    ui: &egui::Ui,
    out: &egui::text_edit::TextEditOutput,
    hint: &str,
    font: &egui::FontId,
    t: &sica_core::theme::Theme,
) {
    let end = egui::text::CCursor::new(out.galley.text().chars().count());
    let caret = out.galley.pos_from_cursor(&out.galley.from_ccursor(end));
    let pos = out.galley_pos + caret.min.to_vec2();
    let avail = out.text_clip_rect.max.x - pos.x - 4.0;
    if avail < 48.0 {
        return;
    }
    ui.painter().text(
        pos,
        Align2::LEFT_TOP,
        kit::elide(ui, hint, font, avail),
        font.clone(),
        kit::col(t.alias.label[3]),
    );
}

/// `[shield] [label] [chevron]`, h=28 r=24; the menu switches modes. Full
/// access applies straight away — its consequences are spelled out in the
/// menu item's own description rather than behind a confirmation.
fn permission_chip(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    let mode = app.permission_mode;
    let compact = ui.available_width() < 460.0;
    let label = match mode {
        protocol::PermissionMode::ReadOnly => "Read Only",
        protocol::PermissionMode::WorkspaceWrite => "Workspace Write",
        protocol::PermissionMode::DangerFullAccess => "Full access",
    };
    let rect = chip(
        ui,
        Icon::Shield,
        if compact { "" } else { label },
        true,
        kit::col(t.alias.label[1]),
    );
    let resp = ui.interact(rect, ui.id().with("perm_chip"), Sense::click());
    if resp.on_hover_text(mode.description()).clicked() {
        app.menu_open.permission = !app.menu_open.permission;
    }
    let items: Vec<kit::MenuItem> = [
        protocol::PermissionMode::ReadOnly,
        protocol::PermissionMode::WorkspaceWrite,
        protocol::PermissionMode::DangerFullAccess,
    ]
    .iter()
    .map(|m| {
        kit::MenuItem::new(match m {
            protocol::PermissionMode::ReadOnly => "Read Only",
            protocol::PermissionMode::WorkspaceWrite => "Workspace Write",
            protocol::PermissionMode::DangerFullAccess => "Full access",
        })
        .detail(m.description())
        .checked(*m == mode)
    })
    .collect();
    let mut open = app.menu_open.permission;
    let picked = kit::menu(
        ui.ctx(),
        egui::Id::new("perm_menu"),
        rect,
        kit::MenuSide::Above,
        280.0,
        &items,
        &mut open,
    );
    app.menu_open.permission = open;
    if let Some(i) = picked {
        let m = [
            protocol::PermissionMode::ReadOnly,
            protocol::PermissionMode::WorkspaceWrite,
            protocol::PermissionMode::DangerFullAccess,
        ][i];
        set_permission(app, m);
    }
}

pub fn set_permission(app: &mut App, mode: protocol::PermissionMode) {
    let session_id = app.chat.session_id;
    app.permission_mode = mode;
    app.send(UiCommand::SendRequest(Request::SetPermissionMode {
        session_id,
        mode,
    }));
}

/// Shown only while plan mode is on; clicking it turns plan mode off.
fn plan_chip(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    let resp = kit::tinted_pill(
        ui,
        "Plan ✕",
        kit::col(t.alias.warn_tertiary),
        kit::col(t.alias.warn_label),
        13.0,
    );
    if resp
        .on_hover_text("Plan mode on — click to turn off (/plan)")
        .clicked()
    {
        let session_id = app.chat.session_id;
        app.send(UiCommand::SendRequest(Request::SetPlanMode {
            session_id,
            active: false,
        }));
    }
}

/// Shown only while an agent preset is selected; clicking it clears the
/// selection. The backend refuses a change once the session has produced a
/// reply, and answers with a `LogLine` saying so — the chip stays put in
/// that case, which is the honest rendering of "this is fixed now".
fn agent_chip(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    let Some(name) = app.session_agent.clone() else { return };
    let resp = kit::tinted_pill(
        ui,
        &format!("{name} ✕"),
        kit::col(t.alias.business_tertiary),
        kit::col(t.alias.business),
        13.0,
    );
    if resp
        .on_hover_text(format!(
            "Agent `{name}` — its persona leads the prompt and its `skills:` list              restricts the tools. Click to clear (/agent off)"
        ))
        .clicked()
    {
        let session_id = app.chat.session_id;
        app.send(UiCommand::SendRequest(Request::SetSessionAgent {
            session_id,
            name: None,
        }));
    }
}

/// `[dot] [model name] [chevron]` — the connection state that used to live in
/// the status bar is this chip's leading dot (§6.8).
fn model_select(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    let (dot, name) = match &app.llm_state.state {
        protocol::LlmState::Ready { model, .. } => (None, model.clone()),
        protocol::LlmState::Connecting => (Some(DotState::Ongoing), "Connecting…".to_string()),
        protocol::LlmState::Error { .. } => (Some(DotState::Error), "Model error".to_string()),
        protocol::LlmState::Disconnected => (Some(DotState::Idle), "Select model".to_string()),
    };
    let short = short_model(&name);
    let rect = chip_with_dot(ui, dot, &short, kit::col(t.alias.label[1]), &t);
    let resp = ui.interact(rect, ui.id().with("model_chip"), Sense::click());
    let tip = app
        .llm_state
        .last_error
        .clone()
        .unwrap_or_else(|| app.llm_state.label());
    if resp.on_hover_text(tip).clicked() {
        app.menu_open.model = !app.menu_open.model;
    }

    let providers: Vec<(String, String, String)> = app
        .providers
        .iter()
        .map(|p| (p.id.clone(), p.title.clone(), p.model.clone()))
        .collect();
    let active = app.active_provider_id.clone();
    let mut items: Vec<kit::MenuItem> = providers
        .iter()
        .map(|(id, title, model)| {
            kit::MenuItem::new(title.clone())
                .detail(model.clone())
                .checked(active.as_deref() == Some(id.as_str()))
        })
        .collect();
    items.push(
        kit::MenuItem::new("Model settings…")
            .detail("Add or edit a provider")
            .sep_above(true),
    );
    let mut open = app.menu_open.model;
    let picked = kit::menu(
        ui.ctx(),
        egui::Id::new("model_menu"),
        rect,
        kit::MenuSide::Above,
        260.0,
        &items,
        &mut open,
    );
    app.menu_open.model = open;
    if let Some(i) = picked {
        if i < providers.len() {
            let id = providers[i].0.clone();
            app.connect_provider(&id);
        } else {
            app.settings_open = true;
            app.settings_tab = crate::app::SettingsTab::Models;
        }
    }
}

fn short_model(name: &str) -> String {
    let tail = name.rsplit('/').next().unwrap_or(name);
    if tail.chars().count() > 22 {
        format!("{}…", tail.chars().take(21).collect::<String>())
    } else {
        tail.to_string()
    }
}

/// A toolbar chip: h=28, r=24, optional leading glyph, trailing chevron.
fn chip(ui: &mut egui::Ui, icon: Icon, label: &str, chevron: bool, fg: egui::Color32) -> Rect {
    chip_inner(ui, Some(icon), None, label, chevron, fg, None)
}

fn chip_with_dot(
    ui: &mut egui::Ui,
    dot: Option<DotState>,
    label: &str,
    fg: egui::Color32,
    t: &sica_core::theme::Theme,
) -> Rect {
    chip_inner(ui, None, dot, label, true, fg, Some(*t))
}

fn chip_inner(
    ui: &mut egui::Ui,
    icon: Option<Icon>,
    dot: Option<DotState>,
    label: &str,
    chevron: bool,
    fg: egui::Color32,
    theme: Option<sica_core::theme::Theme>,
) -> Rect {
    let t = theme.unwrap_or_else(|| kit::theme(ui));
    let font = kit::font(13.0, Weight::Medium);
    let text_w = if label.is_empty() {
        0.0
    } else {
        ui.fonts(|f| f.layout_no_wrap(label.to_owned(), font.clone(), egui::Color32::WHITE))
            .size()
            .x
            + 6.0
    };
    let lead_w = if icon.is_some() || dot.is_some() { 18.0 } else { 0.0 };
    let chev_w = if chevron { 14.0 } else { 0.0 };
    let w = 10.0 + lead_w + text_w + chev_w + 8.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 28.0), Sense::hover());
    let hovered = ui.rect_contains_pointer(rect);
    if hovered {
        ui.painter()
            .rect_filled(rect, Rounding::same(24.0), kit::cola(t.alias.hover));
    }
    let mut x = rect.min.x + 10.0;
    if let Some(icon) = icon {
        icons::paint(
            ui.painter(),
            Rect::from_center_size(egui::pos2(x + 7.0, rect.center().y), Vec2::splat(15.0)),
            icon,
            fg,
        );
        x += lead_w;
    } else if let Some(state) = dot {
        kit::paint_state_dot(
            ui.painter(),
            Rect::from_center_size(egui::pos2(x + 7.0, rect.center().y), Vec2::splat(10.0)),
            state,
            &t,
            ui.input(|i| i.time) as f32,
        );
        x += lead_w;
    }
    if !label.is_empty() {
        ui.painter().text(
            egui::pos2(x, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            font,
            fg,
        );
        x += text_w;
    }
    if chevron {
        icons::paint(
            ui.painter(),
            Rect::from_center_size(egui::pos2(x + 7.0, rect.center().y), Vec2::splat(12.0)),
            Icon::ChevronUp,
            kit::col(t.alias.label[2]),
        );
    }
    rect
}

// ---------------------------------------------------------------------------
// Submission
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SubmitKeys {
    submit: bool,
    accelerated: bool,
}

/// Enter / Ctrl+Enter, consumed before the multiline editor sees them.
/// Held Enter (`repeat`) is swallowed, as dsh does.
fn read_submit_keys(ui: &mut egui::Ui, active: bool) -> SubmitKeys {
    if !active {
        return SubmitKeys::default();
    }
    let repeat = ui.input(|i| {
        i.events.iter().any(|e| {
            matches!(
                e,
                egui::Event::Key {
                    key: egui::Key::Enter,
                    repeat: true,
                    pressed: true,
                    ..
                }
            )
        })
    });
    if repeat {
        return SubmitKeys::default();
    }
    ui.input_mut(|i| SubmitKeys {
        submit: i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
        accelerated: i.consume_key(egui::Modifiers::CTRL, egui::Key::Enter)
            || i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter),
    })
}

/// Ctrl+Enter on an empty draft with rows waiting: every queued message
/// joins the running turn at its next hop instead of waiting for one of its
/// own (§5.1).
///
/// The rows are not cleared here — the backend answers each promotion with a
/// `QueueChanged`, and that is what empties the dock. A row it refuses (one
/// carrying images, which a steer cannot take) stays, which is the point.
fn steer_queue(app: &mut App) {
    let session_id = app.chat.session_id;
    let ids: Vec<u64> = app.chat.queued.iter().filter_map(|r| r.id).collect();
    for id in ids {
        app.send(UiCommand::SendRequest(Request::SteerQueued { session_id, id }));
    }
}

/// `/compact`, `/plan …`, `/permission …`, `/goal …` run as harness commands
/// instead of model turns.
fn parse_harness_command(text: &str) -> Option<(String, String)> {
    let head = text.split_whitespace().next()?;
    let name = match head {
        "/compact" => "compact",
        "/plan" => "plan",
        "/permission" => "permission",
        "/agent" => "agent",
        "/goal" => "goal",
        _ => return None,
    };
    let input = text[head.len()..].trim().to_string();
    Some((name.to_string(), input))
}

fn send_message(app: &mut App, steer: bool) {
    let text = std::mem::take(&mut app.chat.draft);
    let attachments = std::mem::take(&mut app.chat.pending_images);
    let images = attachments
        .iter()
        .map(PendingAttachment::to_user_image)
        .collect::<Vec<_>>();
    let history_images = attachments
        .iter()
        .map(|a| crate::app::Attachment {
            mime: a.mime.clone(),
            data_base64: a.data_base64.clone(),
            texture: None,
        })
        .collect::<Vec<_>>();

    if text.trim().is_empty() && images.is_empty() {
        return;
    }

    // A steer joins the turn already on screen instead of opening one of its
    // own, so it lands as a marker row.
    if steer && images.is_empty() {
        let session_id = app.chat.session_id;
        app.chat.turns.push(crate::app::Turn::marker(
            session_id,
            crate::app::Notice::new(
                crate::app::NoticeKind::Steer,
                kit::one_line(&text, 80),
                text.clone(),
                true,
            ),
        ));
        app.chat.scroll_to_bottom = true;
        app.send(UiCommand::SendRequest(Request::SteerTurn { session_id, text }));
        return;
    }

    if images.is_empty() {
        if let Some((name, input)) = parse_harness_command(&text) {
            let session_id = app.chat.session_id;
            app.last_command_session = Some(session_id);
            app.send(UiCommand::SendRequest(Request::RunCommand {
                session_id,
                name,
                input,
            }));
            return;
        }
    }

    let session_id = app.chat.session_id;
    if let Some(pos) = app.chat.sessions.iter().position(|s| s.id == session_id) {
        if pos > 0 {
            let meta = app.chat.sessions.remove(pos);
            app.chat.sessions.insert(0, meta);
        }
    }
    if last_turn_in_flight(app) {
        // Optimistic local echo, inert until the backend's `QueueChanged`
        // replaces it with the addressable row.
        app.chat.queued.push(crate::app::QueuedRow {
            id:     None,
            text:   text.clone(),
            images: images.len() as u32,
        });
    }
    app.chat.turns.push(crate::app::Turn {
        user: text.clone(),
        images: history_images,
        ..crate::app::Turn::new(session_id, 0)
    });
    app.chat.scroll_to_bottom = true;
    app.chat.interrupt_requested = false;
    app.send(UiCommand::SendRequest(Request::SendUserMessage {
        session_id,
        text,
        images,
    }));
}

pub fn last_turn_in_flight(app: &App) -> bool {
    app.chat.turns.last().map(|t| !t.finished).unwrap_or(false)
}

/// Visual rows `text` occupies when wrapped at `width`; a trailing newline
/// gets a row of its own so Shift+Enter opens the field immediately.
fn wrapped_rows(ui: &egui::Ui, text: &str, font_id: &egui::FontId, width: f32) -> usize {
    if text.is_empty() {
        return 1;
    }
    let job = egui::text::LayoutJob::simple(
        text.to_owned(),
        font_id.clone(),
        egui::Color32::WHITE,
        width,
    );
    let galley = ui.fonts(|f| f.layout_job(job));
    galley
        .rows
        .len()
        .saturating_add(usize::from(text.ends_with('\n')))
        .max(1)
}

fn handle_escape(app: &mut App, ui: &mut egui::Ui) {
    if !ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        return;
    }
    if last_turn_in_flight(app) && !app.chat.interrupt_requested {
        app.interrupt_turn();
    }
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

fn draw_pending_strip(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    let ctx = ui.ctx().clone();
    ui.horizontal_wrapped(|ui| {
        let mut remove_idx: Option<usize> = None;
        for (i, att) in app.chat.pending_images.iter_mut().enumerate() {
            let tex = ensure_texture(&ctx, &mut att.texture, &att.mime, &att.data_base64, i);
            let (rect, resp) = ui.allocate_exact_size(Vec2::splat(THUMB_SIZE), Sense::hover());
            match tex {
                Some(handle) => {
                    let natural = handle.size_vec2();
                    let size = if natural.x <= 0.0 || natural.y <= 0.0 {
                        Vec2::splat(THUMB_SIZE)
                    } else {
                        let scale = (THUMB_SIZE / natural.x).min(THUMB_SIZE / natural.y);
                        natural * scale
                    };
                    let draw = Rect::from_center_size(rect.center(), size);
                    ui.painter().image(
                        handle.id(),
                        draw,
                        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                }
                None => {
                    ui.painter().rect_filled(
                        rect,
                        Rounding::same(16.0),
                        kit::col(t.alias.code_block),
                    );
                }
            }
            ui.painter().rect_stroke(
                rect,
                Rounding::same(16.0),
                Stroke::new(HAIRLINE, Level::L2.color(&t)),
            );
            // Remove ×, top-right of the tile.
            let x_rect = Rect::from_center_size(
                egui::pos2(rect.max.x - 8.0, rect.min.y + 8.0),
                Vec2::splat(16.0),
            );
            let x_resp = ui.interact(x_rect, ui.id().with(("rm_img", i)), Sense::click());
            ui.painter()
                .circle_filled(x_rect.center(), 8.0, kit::col(t.alias.toast_bg));
            icons::paint(
                ui.painter(),
                Rect::from_center_size(x_rect.center(), Vec2::splat(9.0)),
                Icon::Close,
                egui::Color32::WHITE,
            );
            if x_resp.on_hover_text("Remove").clicked() {
                remove_idx = Some(i);
            }
            let _ = resp;
            ui.add_space(6.0);
        }
        if let Some(i) = remove_idx {
            app.chat.pending_images.remove(i);
        }
    });
}

pub fn ensure_texture(
    ctx: &egui::Context,
    slot: &mut Option<egui::TextureHandle>,
    mime: &str,
    data_base64: &str,
    nonce: usize,
) -> Option<egui::TextureHandle> {
    if let Some(h) = slot {
        return Some(h.clone());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_base64)
        .ok()?;
    let img = image::load_from_memory(&bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let color = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
    let handle = ctx.load_texture(
        format!("attach-{mime}-{nonce}-{}", data_base64.len()),
        color,
        Default::default(),
    );
    *slot = Some(handle.clone());
    Some(handle)
}

pub fn pick_file_and_attach(app: &mut App) {
    let picked = rfd::FileDialog::new()
        .add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif", "bmp"])
        .pick_file();
    let Some(path) = picked else { return };
    if let Err(e) = attach_from_path(app, &path) {
        app.push_log(LogKind::Error, format!("attach failed: {e}"));
    }
}

fn attach_from_path(app: &mut App, path: &Path) -> std::io::Result<()> {
    let bytes = std::fs::read(path)?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("file too large ({} bytes); cap is {}", bytes.len(), MAX_IMAGE_BYTES),
        ));
    }
    let mime = mime_from_path(path)
        .unwrap_or("application/octet-stream")
        .to_string();
    if !mime.starts_with("image/") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("not an image: {}", path.display()),
        ));
    }
    let data_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let filename = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".into());
    app.chat.pending_images.push(PendingAttachment {
        mime,
        data_base64,
        filename,
        size_bytes: bytes.len(),
        texture: None,
    });
    Ok(())
}

fn mime_from_path(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
    {
        Some(ext) => Some(match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            "bmp" => "image/bmp",
            _ => return None,
        }),
        None => None,
    }
}

fn handle_dropped_files(app: &mut App, ui: &mut egui::Ui) {
    let dropped = ui.ctx().input(|i| i.raw.dropped_files.clone());
    for f in dropped {
        if let Some(path) = f.path.as_ref() {
            if let Err(e) = attach_from_path(app, path) {
                app.push_log(LogKind::Error, format!("drop attach failed: {e}"));
            }
        }
    }
}

fn handle_paste(app: &mut App, ui: &mut egui::Ui) {
    let pressed = ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::V));
    if !pressed {
        return;
    }
    let mut clip = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(_) => return,
    };
    let img = match clip.get_image() {
        Ok(i) => i,
        Err(_) => return,
    };
    let w = img.width as u32;
    let h = img.height as u32;
    let raw = img.bytes.into_owned();
    let buf = match image::RgbaImage::from_raw(w, h, raw) {
        Some(b) => b,
        None => return,
    };
    let mut png: Vec<u8> = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut png);
    if image::DynamicImage::ImageRgba8(buf)
        .write_to(&mut cursor, image::ImageFormat::Png)
        .is_err()
    {
        return;
    }
    let data_base64 = base64::engine::general_purpose::STANDARD.encode(&png);
    app.chat.pending_images.push(PendingAttachment {
        mime: "image/png".into(),
        data_base64,
        filename: "clipboard.png".into(),
        size_bytes: png.len(),
        texture: None,
    });
}

/// Full-window overlay while files hover the window (§5.1).
pub fn drop_overlay(app: &App, ui: &mut egui::Ui) {
    let hovering = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
    if !hovering {
        return;
    }
    let t = app.theme;
    let rect = ui.max_rect();
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        Rounding::same(RADIUS_PILL.min(24.0)),
        kit::col(t.alias.bg_base).linear_multiply(0.85),
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        "Drop images here to add them",
        kit::font(16.0, Weight::Medium),
        kit::col(t.alias.label[0]),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_commands_are_whitespace_bounded() {
        assert_eq!(
            parse_harness_command("/plan build a thing"),
            Some(("plan".into(), "build a thing".into()))
        );
        assert_eq!(parse_harness_command("/compact"), Some(("compact".into(), String::new())));
        // Not a harness command: it is a markdown command the BE resolves.
        assert_eq!(parse_harness_command("/planner do x"), None);
        assert_eq!(parse_harness_command("hello /plan"), None);
    }

    #[test]
    fn model_names_shorten_from_the_tail() {
        assert_eq!(short_model("openai/gpt-4o-mini"), "gpt-4o-mini");
        assert!(short_model(&"x".repeat(40)).ends_with('…'));
    }

    #[test]
    fn only_a_bare_claimed_token_carries_a_ghost_hint() {
        // What the palette leaves behind after accepting a row.
        assert_eq!(claimed_command("/goal "), Some("goal"));
        // Still being typed, or already carrying an argument.
        assert_eq!(claimed_command("/goal"), None);
        assert_eq!(claimed_command("/goal ship it"), None);
        // A stray second space is an argument the user has started, not a
        // claim — the hint would sit in the middle of what they are writing.
        assert_eq!(claimed_command("/goal  "), None);
        assert_eq!(claimed_command("/ "), None);
        assert_eq!(claimed_command("hello "), None);
    }
}
