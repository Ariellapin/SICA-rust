//! The composer dock (§5.1 "Dock stack", §6.4, §6.6): full-width cards
//! *above* the composer card, 6 px apart, in dsh's order — **To-dos** (0),
//! **Goal** (10), **Queue** (20) — plus the stats line under the card.
//!
//! The queue dock is tucked 3 px under the composer so the two read as one
//! surface. Its rows are the backend's own inbox (`Event::QueueChanged`), so
//! Edit / Remove / Steer address the message the loop will actually run —
//! and a row this frontend has sent but not yet heard back about renders
//! beside them as an inert local echo.

use egui::{Align, Align2, Layout, Rect, Rounding, Sense, Stroke, Vec2};

use protocol::Request;
use sica_core::theme::tokens::{HAIRLINE, RADIUS_CARD};

use crate::app::App;
use crate::supervisor::UiCommand;
use crate::ui::icons::{self, Icon};
use crate::ui::kit::{self, Level, Weight};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    todo_card(app, ui);
    goal_bar(app, ui);
    queue_dock(app, ui);
}

// ---------------------------------------------------------------------------
// To-dos
// ---------------------------------------------------------------------------

fn todo_card(app: &mut App, ui: &mut egui::Ui) {
    if app.todos.is_empty() {
        return;
    }
    let t = app.theme;
    let id = ui.id().with("todo_open");
    // Collapsed by default, like dsh.
    let mut open: bool = ui.ctx().data(|d| d.get_temp(id).unwrap_or(false));
    let done = app
        .todos
        .iter()
        .filter(|i| i.status == protocol::TodoStatus::Completed)
        .count();
    let active = app
        .todos
        .iter()
        .filter(|i| i.status == protocol::TodoStatus::InProgress)
        .count();
    let pending = app.todos.len() - done - active;

    egui::Frame::none()
        .fill(kit::col(t.alias.tip))
        .stroke(Stroke::new(HAIRLINE, Level::L1.color(&t)))
        .rounding(Rounding::same(RADIUS_CARD))
        .inner_margin(egui::Margin::symmetric(12.0, 6.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let (rect, resp) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), Sense::click());
            ui.painter().text(
                rect.left_center(),
                Align2::LEFT_CENTER,
                "To-dos",
                kit::font(13.0, Weight::Medium),
                kit::col(t.alias.label[0]),
            );
            ui.painter().text(
                egui::pos2(rect.min.x + 60.0, rect.center().y),
                Align2::LEFT_CENTER,
                format!("{done} completed · {active} in progress · {pending} pending"),
                kit::font(12.0, Weight::Regular),
                kit::col(t.alias.label[2]),
            );
            icons::paint(
                ui.painter(),
                Rect::from_center_size(
                    egui::pos2(rect.max.x - 10.0, rect.center().y),
                    Vec2::splat(12.0),
                ),
                if open { Icon::ChevronDown } else { Icon::ChevronRight },
                kit::col(t.alias.label[2]),
            );
            if resp.clicked() {
                open = !open;
                ui.ctx().data_mut(|d| d.insert_temp(id, open));
            }
            if open {
                for item in &app.todos {
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                        todo_glyph(ui, r, item.status, &t);
                        ui.add_space(6.0);
                        ui.add(egui::Label::new(kit::txt(
                            &item.content,
                            13.0,
                            Weight::Regular,
                            match item.status {
                                protocol::TodoStatus::Completed => kit::col(t.alias.label[2]),
                                _ => kit::col(t.alias.label[0]),
                            },
                        )));
                    });
                }
                ui.add_space(2.0);
            }
        });
    ui.add_space(6.0);
}

/// pending = dashed ring · in-progress = a spinning blue arc · completed =
/// solid ring + check.
fn todo_glyph(
    ui: &mut egui::Ui,
    rect: Rect,
    status: protocol::TodoStatus,
    t: &sica_core::theme::Theme,
) {
    let painter = ui.painter();
    let c = rect.center();
    let r = rect.width() * 0.42;
    match status {
        protocol::TodoStatus::Pending => {
            let color = kit::col(t.alias.label[3]);
            for i in 0..8 {
                let a0 = std::f32::consts::TAU * (i as f32) / 8.0;
                let a1 = a0 + std::f32::consts::TAU / 16.0;
                painter.line_segment(
                    [
                        egui::pos2(c.x + r * a0.cos(), c.y + r * a0.sin()),
                        egui::pos2(c.x + r * a1.cos(), c.y + r * a1.sin()),
                    ],
                    Stroke::new(1.2, color),
                );
            }
        }
        protocol::TodoStatus::InProgress => {
            let color = kit::col(t.alias.business);
            painter.circle_stroke(c, r, Stroke::new(1.2, color.linear_multiply(0.25)));
            let time = ui.input(|i| i.time) as f32;
            let start = (time * 3.0) % std::f32::consts::TAU;
            let pts = (0..=12)
                .map(|i| {
                    let a = start + std::f32::consts::TAU * 0.35 * (i as f32 / 12.0);
                    egui::pos2(c.x + r * a.cos(), c.y + r * a.sin())
                })
                .collect::<Vec<_>>();
            painter.add(egui::Shape::line(pts, Stroke::new(1.6, color)));
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(33));
        }
        protocol::TodoStatus::Completed => {
            let color = kit::col(t.alias.success);
            painter.circle_filled(c, r, color);
            icons::paint(
                painter,
                Rect::from_center_size(c, Vec2::splat(rect.width() * 0.7)),
                Icon::Check,
                egui::Color32::WHITE,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Goal
// ---------------------------------------------------------------------------

fn goal_bar(app: &mut App, ui: &mut egui::Ui) {
    let Some(goal) = app.goal.clone() else { return };
    if goal.phase == protocol::GoalPhase::Completed {
        return;
    }
    let t = app.theme;
    let mut command: Option<&str> = None;
    /// One-shot "the objective field has taken focus" flag.
    const GOAL_FOCUS: &str = "goal_edit_focused";
    /// Width of the inline objective field. The bar is one row, so the field
    /// takes a fixed seat rather than eating the controls to its right.
    const EDIT_FIELD_W: f32 = 360.0;
    let mut open_edit = false;
    let mut edit: Option<String> = None;
    // "Paused" covers active-but-disarmed: pressing Stop disarms the driver,
    // and the label must not claim rounds are still opening.
    let (phase, tint) = match goal.phase {
        protocol::GoalPhase::Active if goal.armed => ("Ongoing Goal", kit::col(t.alias.business)),
        protocol::GoalPhase::Active | protocol::GoalPhase::Paused => {
            ("Paused Goal", kit::col(t.alias.label[2]))
        }
        protocol::GoalPhase::Blocked => ("Blocked Goal", kit::col(t.alias.error)),
        protocol::GoalPhase::Completed => ("Goal", kit::col(t.alias.success)),
    };

    egui::Frame::none()
        .fill(kit::col(t.alias.tip))
        .stroke(Stroke::new(HAIRLINE, Level::L1.color(&t)))
        .rounding(Rounding::same(RADIUS_CARD))
        .inner_margin(egui::Margin::symmetric(12.0, 6.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                icons::show(ui, Icon::Goal, 14.0, tint);
                ui.add_space(6.0);
                kit::label(ui, kit::txt(phase, 13.0, Weight::Medium, tint))
                    .on_hover_text(format!(
                        "round {}/{}",
                        goal.rounds_started, goal.max_rounds
                    ));
                ui.add_space(8.0);
                if let Some(mut draft) = app.goal_edit.clone() {
                    let field = egui::TextEdit::singleline(&mut draft)
                        .desired_width(EDIT_FIELD_W)
                        .margin(egui::vec2(6.0, 4.0));
                    let resp = ui.add(field);
                    let mut open = true;
                    if resp.lost_focus() {
                        // Enter commits; Escape — and any other way of losing
                        // focus — abandons, rather than rewording the
                        // objective because the user clicked elsewhere.
                        if ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                            edit = Some(draft.clone());
                        }
                        open = false;
                    }
                    let focus = egui::Id::new(GOAL_FOCUS);
                    if !ui.ctx().data(|d| d.get_temp::<bool>(focus).unwrap_or(false)) {
                        resp.request_focus();
                        ui.ctx().data_mut(|d| d.insert_temp(focus, true));
                    }
                    app.goal_edit = open.then_some(draft);
                } else {
                    let objective = kit::one_line(&goal.objective, 90);
                    kit::label(
                        ui,
                        kit::txt(objective, 13.0, Weight::Regular, kit::col(t.alias.label[1])),
                    )
                    .on_hover_text(&goal.objective);
                }
                if let Some(b) = &goal.blocker {
                    ui.add_space(6.0);
                    kit::label(
                        ui,
                        kit::txt(
                            format!("blocked: {}", kit::one_line(b, 60)),
                            12.0,
                            Weight::Regular,
                            kit::col(t.alias.error),
                        ),
                    );
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Complete / block live under a ⋯, not on the bar.
                    let dots = kit::icon_button(ui, Icon::Dots, 26.0);
                    if dots.clicked() {
                        app.menu_open.goal = !app.menu_open.goal;
                    }
                    let items = vec![
                        kit::MenuItem::new("Mark complete")
                            .detail("Stop rounds; the objective is met"),
                        kit::MenuItem::new("Block")
                            .detail("Record that it cannot proceed")
                            .danger(true),
                    ];
                    let mut open = app.menu_open.goal;
                    let picked = kit::menu(
                        ui.ctx(),
                        egui::Id::new("goal_menu"),
                        dots.rect,
                        kit::MenuSide::Above,
                        240.0,
                        &items,
                        &mut open,
                    );
                    app.menu_open.goal = open;
                    match picked {
                        Some(0) => command = Some("complete"),
                        Some(1) => command = Some("block blocked from the UI"),
                        _ => {}
                    }
                    if app.goal_edit.is_none()
                        && !goal.phase.is_terminal()
                        && kit::icon_button(ui, Icon::Edit, 26.0)
                            .on_hover_text("Reword the objective")
                            .clicked()
                    {
                        open_edit = true;
                    }
                    if goal.phase == protocol::GoalPhase::Active && goal.armed {
                        if kit::button(ui, "Pause", kit::Variant::Ghost, kit::Size::Sm)
                            .on_hover_text("Stop opening new rounds. The current turn finishes.")
                            .clicked()
                        {
                            command = Some("pause");
                        }
                    } else if !goal.phase.is_terminal() && goal.rounds_started < goal.max_rounds {
                        if kit::button(ui, "Resume", kit::Variant::Ghost, kit::Size::Sm)
                            .on_hover_text("Resume automatic rounds against this objective.")
                            .clicked()
                        {
                            command = Some("continue");
                        }
                    }
                });
            });
        });
    ui.add_space(6.0);

    if open_edit {
        app.goal_edit = Some(goal.objective.clone());
        ui.ctx().data_mut(|d| d.insert_temp(egui::Id::new(GOAL_FOCUS), false));
    }
    // An emptied field is not a deletion — a goal is ended with Complete or
    // Block — so it simply abandons the edit.
    let edit = edit.map(|text| text.trim().to_string()).filter(|text| {
        !text.is_empty() && *text != goal.objective
    });
    let input = match (&edit, command) {
        (Some(text), _) => Some(format!("edit {text}")),
        (None, Some(word)) => Some(word.to_string()),
        (None, None) => None,
    };
    if let Some(input) = input {
        let session_id = app.chat.session_id;
        app.last_command_session = Some(session_id);
        app.send(UiCommand::SendRequest(Request::RunCommand {
            session_id,
            name: "goal".into(),
            input,
        }));
    }
}

// ---------------------------------------------------------------------------
// Queue
// ---------------------------------------------------------------------------

/// What a row's controls asked for, applied after the frame so the row loop
/// never mutates the list it is iterating.
enum QueueAct {
    Open(u64),
    Save(u64),
    Cancel,
    Remove(u64),
    Steer(u64),
}

fn queue_dock(app: &mut App, ui: &mut egui::Ui) {
    if app.chat.queued.is_empty() {
        return;
    }
    let t = app.theme;
    let rows = app.chat.queued.clone();
    let running = app.chat.running_sessions.contains(&app.chat.session_id);
    let mut act: Option<QueueAct> = None;
    let id = ui.id().with("queue_open");
    let mut open: bool = ui.ctx().data(|d| d.get_temp(id).unwrap_or(rows.len() == 1));
    egui::Frame::none()
        .fill(kit::col(t.alias.tip))
        .stroke(Stroke::new(HAIRLINE, Level::L1.color(&t)))
        .rounding(Rounding {
            nw: RADIUS_CARD,
            ne: RADIUS_CARD,
            sw: 0.0,
            se: 0.0,
        })
        .inner_margin(egui::Margin {
            left: 12.0,
            right: 12.0,
            top: 6.0,
            bottom: 9.0,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if rows.len() > 1 {
                let (rect, resp) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 22.0), Sense::click());
                icons::paint(
                    ui.painter(),
                    Rect::from_center_size(
                        egui::pos2(rect.min.x + 7.0, rect.center().y),
                        Vec2::splat(13.0),
                    ),
                    Icon::Queue,
                    kit::col(t.alias.label[2]),
                );
                ui.painter().text(
                    egui::pos2(rect.min.x + 20.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    format!("{} queued messages", rows.len()),
                    kit::font(13.0, Weight::Medium),
                    kit::col(t.alias.label[1]),
                );
                icons::paint(
                    ui.painter(),
                    Rect::from_center_size(
                        egui::pos2(rect.max.x - 8.0, rect.center().y),
                        Vec2::splat(12.0),
                    ),
                    if open { Icon::ChevronDown } else { Icon::ChevronRight },
                    kit::col(t.alias.label[2]),
                );
                if resp.clicked() {
                    open = !open;
                    ui.ctx().data_mut(|d| d.insert_temp(id, open));
                }
            }
            if open || rows.len() == 1 {
                for row in &rows {
                    queue_row(app, ui, row, running, &mut act);
                }
            }
        });
    // Tuck the dock under the composer card so they read as one surface.
    ui.add_space(-3.0);

    let session_id = app.chat.session_id;
    match act {
        Some(QueueAct::Open(id)) => {
            app.chat.queue_edit_draft = rows
                .iter()
                .find(|r| r.id == Some(id))
                .map(|r| r.text.clone())
                .unwrap_or_default();
            app.chat.queue_edit = Some(id);
            // Re-arm the one-shot focus claim for this row's field.
            ui.ctx()
                .data_mut(|d| d.insert_temp(queue_focus_id(id), false));
        }
        Some(QueueAct::Cancel) => {
            app.chat.queue_edit = None;
            app.chat.queue_edit_draft.clear();
        }
        Some(QueueAct::Save(id)) => {
            let text = app.chat.queue_edit_draft.trim().to_string();
            app.chat.queue_edit = None;
            app.chat.queue_edit_draft.clear();
            // An emptied row is a removal in disguise; the backend refuses
            // it, so say what was meant instead of showing an error.
            if text.is_empty() {
                app.send(UiCommand::SendRequest(Request::RemoveQueued { session_id, id }));
            } else if rows.iter().any(|r| r.id == Some(id) && r.text != text) {
                app.send(UiCommand::SendRequest(Request::EditQueued { session_id, id, text }));
            }
        }
        Some(QueueAct::Remove(id)) => {
            app.send(UiCommand::SendRequest(Request::RemoveQueued { session_id, id }));
        }
        Some(QueueAct::Steer(id)) => {
            app.send(UiCommand::SendRequest(Request::SteerQueued { session_id, id }));
        }
        None => {}
    }
}

/// The one-shot "this field has taken focus" flag for a row's editor.
fn queue_focus_id(id: u64) -> egui::Id {
    egui::Id::new("queue_edit_focused").with(id)
}

/// One queued message: its preview and, on hover, Steer · Edit · Remove.
///
/// A local echo (no id yet) draws dimmed with no actions — there is nothing
/// to address until the backend answers, and offering a button that would
/// silently do nothing is worse than not offering it.
fn queue_row(
    app: &mut App,
    ui: &mut egui::Ui,
    row: &crate::app::QueuedRow,
    running: bool,
    act: &mut Option<QueueAct>,
) {
    let t = app.theme;
    let Some(id) = row.id else {
        ui.horizontal(|ui| {
            ui.add(egui::Label::new(kit::txt(
                kit::one_line(&row.text, 90),
                13.0,
                Weight::Regular,
                kit::col(t.alias.label[3]),
            )))
            .on_hover_text("sending…");
        });
        return;
    };

    if app.chat.queue_edit == Some(id) {
        ui.horizontal(|ui| {
            let field = egui::TextEdit::singleline(&mut app.chat.queue_edit_draft)
                .desired_width(ui.available_width())
                .margin(egui::vec2(6.0, 4.0));
            let resp = ui.add(field);
            if resp.lost_focus() {
                // Enter saves, Escape abandons, and losing focus any other
                // way (a click elsewhere) abandons too rather than committing
                // an edit the user may not have meant to finish.
                if ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                    *act = Some(QueueAct::Save(id));
                } else {
                    *act = Some(QueueAct::Cancel);
                }
            }
            let focus = queue_focus_id(id);
            if !ui.ctx().data(|d| d.get_temp::<bool>(focus).unwrap_or(false)) {
                resp.request_focus();
                ui.ctx().data_mut(|d| d.insert_temp(focus, true));
            }
        });
        return;
    }

    ui.horizontal(|ui| {
        let hot = ui.rect_contains_pointer(ui.max_rect());
        ui.add(egui::Label::new(kit::txt(
            kit::one_line(&row.text, 90),
            13.0,
            Weight::Regular,
            kit::col(t.alias.label[1]),
        )));
        if row.images > 0 {
            kit::label(
                ui,
                kit::txt(
                    format!("· {} image{}", row.images, if row.images == 1 { "" } else { "s" }),
                    12.0,
                    Weight::Regular,
                    kit::col(t.alias.label[2]),
                ),
            );
        }
        if !hot {
            return;
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if kit::icon_button(ui, Icon::Close, 22.0)
                .on_hover_text("Remove from the queue")
                .clicked()
            {
                *act = Some(QueueAct::Remove(id));
            }
            if kit::icon_button(ui, Icon::Edit, 22.0)
                .on_hover_text("Edit this message")
                .clicked()
            {
                *act = Some(QueueAct::Open(id));
            }
            // Steering means joining the turn that is running now — with
            // nothing running there is no turn to join, and a message
            // carrying images cannot become a steer at all.
            let can_steer = running && row.images == 0;
            let steer = kit::icon_button_tinted(
                ui,
                Icon::Inject,
                22.0,
                (!can_steer).then(|| kit::col(t.alias.label[3])),
            );
            let steer = if can_steer {
                steer.on_hover_text("Steer: join the running turn at its next step")
            } else if row.images > 0 {
                steer.on_hover_text("A message with images cannot be steered")
            } else {
                steer.on_hover_text("Nothing is running to steer")
            };
            if can_steer && steer.clicked() {
                *act = Some(QueueAct::Steer(id));
            }
        });
    });
}

// ---------------------------------------------------------------------------
// Stats line
// ---------------------------------------------------------------------------

/// One centred line under the card, groups joined by ` | ` (§5.1). The
/// numbers are folded FE-side from what the wire already carries; per-turn
/// usage (TTFT, cache hits) needs `TurnUsage` (§11).
pub fn stats_line(app: &App, ui: &mut egui::Ui) {
    use std::sync::atomic::Ordering;
    let t = app.theme;
    let turns = app
        .chat
        .turns
        .iter()
        .filter(|t| t.notice.is_none() && !t.user.is_empty())
        .count();
    if turns == 0 {
        return;
    }
    let steps: usize = app.chat.turns.iter().map(|t| t.tool_chips.len()).sum();
    let mut groups: Vec<String> = vec![format!(
        "{turns} turn{} · {steps} step{}",
        if turns == 1 { "" } else { "s" },
        if steps == 1 { "" } else { "s" }
    )];
    if app.gen_speed.tps > 0.0 {
        groups.push(format!("{:.0} tok/s", app.gen_speed.tps));
    }
    let used = app.tokens.used.load(Ordering::Relaxed);
    let budget = app.tokens.budget.load(Ordering::Relaxed);
    if budget > 0 {
        groups.push(format!("{used} / {budget} tok"));
    }
    let line = groups.join("  |  ");
    ui.add_space(4.0);
    ui.vertical_centered(|ui| {
        kit::label(
            ui,
            kit::txt(line, 12.0, Weight::Regular, kit::col(t.alias.label[2])),
        );
    });
}
