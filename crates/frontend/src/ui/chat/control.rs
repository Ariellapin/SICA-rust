//! Wave 3 control-plane UI above the composer: the one-shot approval
//! strip, the todo checklist + plan-mode toggle row, and the app-modal
//! question dialog. All three are driven by pushed backend events
//! (`ApprovalRequested`, `TodosChanged`, `QuestionAsked`); answers travel
//! back as `ResolveApproval` / `AnswerQuestion`.

use protocol::Request;

use crate::app::{rgb, App};
use crate::supervisor::UiCommand;
use crate::ui::widgets::{caps_label, ghost_button};

/// Strips drawn at the top of the composer bottom panel, before the input
/// bar. The approval strip shows only for the active session — a pending
/// prompt from a background session stays reachable by switching back.
pub fn draw_strips(app: &mut App, ui: &mut egui::Ui) {
    draw_approval_strip(app, ui);
    draw_goal_strip(app, ui);
    draw_jobs_strip(app, ui);
    draw_plan_todo_row(app, ui);
}

fn draw_approval_strip(app: &mut App, ui: &mut egui::Ui) {
    let pending = app.pending_approval.as_ref().filter(|a| {
        a.session_id == app.chat.session_id
    }).map(|a| (a.id, a.skill.clone(), a.args_preview.clone(), a.reason.clone()));
    let Some((id, skill, preview, reason)) = pending else { return };
    let p = app.palette;
    ui.horizontal(|ui| {
        caps_label(ui, "APPROVAL", rgb(p.warn));
        ui.label(
            egui::RichText::new(format!("{skill} — {reason}"))
                .color(rgb(p.ink))
                .small(),
        );
        if !preview.is_empty() {
            ui.label(
                egui::RichText::new(truncate(&preview, 80))
                    .color(rgb(p.muted))
                    .small()
                    .monospace(),
            );
        }
    });
    let mut verdict: Option<bool> = None;
    ui.horizontal(|ui| {
        if ghost_button(ui, &p, "Allow once").clicked() {
            verdict = Some(true);
        }
        if ghost_button(ui, &p, "Deny").clicked() {
            verdict = Some(false);
        }
        caps_label(ui, "no answer in 5 min denies automatically", rgb(p.muted));
    });
    if let Some(allow) = verdict {
        app.pending_approval = None;
        app.send(UiCommand::SendRequest(Request::ResolveApproval { id, allow }));
    }
    ui.add_space(4.0);
}

/// The session's durable objective. Shown whenever one exists, because a
/// goal is the one piece of state that makes the app act on its own: the
/// user must be able to see that rounds are running and stop them in one
/// click.
fn draw_goal_strip(app: &mut App, ui: &mut egui::Ui) {
    let Some(goal) = app.goal.clone() else { return };
    let p = app.palette;
    let mut command: Option<&str> = None;
    ui.horizontal_wrapped(|ui| {
        let (word, tint) = match goal.phase {
            protocol::GoalPhase::Active if goal.armed => ("GOAL RUNNING", rgb(p.accent)),
            protocol::GoalPhase::Active => ("GOAL PAUSED", rgb(p.muted)),
            protocol::GoalPhase::Paused => ("GOAL PAUSED", rgb(p.muted)),
            protocol::GoalPhase::Completed => ("GOAL DONE", rgb(p.ok)),
            protocol::GoalPhase::Blocked => ("GOAL BLOCKED", rgb(p.danger)),
        };
        caps_label(ui, word, tint);
        ui.label(
            egui::RichText::new(format!(
                "round {}/{}",
                goal.rounds_started, goal.max_rounds
            ))
            .color(rgb(p.muted))
            .small(),
        );
        ui.label(
            egui::RichText::new(truncate(&goal.objective, 90))
                .color(rgb(p.ink))
                .small(),
        )
        .on_hover_text(&goal.objective);
        if let Some(b) = &goal.blocker {
            ui.label(
                egui::RichText::new(format!("blocked: {}", truncate(b, 60)))
                    .color(rgb(p.danger))
                    .small(),
            );
        }
        if goal.phase == protocol::GoalPhase::Active && goal.armed {
            if ghost_button(ui, &p, "Pause")
                .on_hover_text("Stop opening new rounds. The current turn finishes.")
                .clicked()
            {
                command = Some("pause");
            }
        } else if !goal.phase.is_terminal() && goal.rounds_started < goal.max_rounds {
            if ghost_button(ui, &p, "Continue")
                .on_hover_text("Resume automatic rounds against this objective.")
                .clicked()
            {
                command = Some("continue");
            }
        }
        if !goal.phase.is_terminal()
            && ghost_button(ui, &p, "Complete")
                .on_hover_text("Mark the objective met and stop.")
                .clicked()
        {
            command = Some("complete");
        }
    });
    if let Some(input) = command {
        let session_id = app.chat.session_id;
        app.last_command_session = Some(session_id);
        app.send(UiCommand::SendRequest(Request::RunCommand {
            session_id,
            name: "goal".into(),
            input: input.into(),
        }));
    }
    ui.add_space(4.0);
}

/// Background jobs for the active session. Running ones first, since those
/// are the ones a "Kill" click is for; a finished job stays listed until
/// its session is closed, because its output is still readable.
fn draw_jobs_strip(app: &mut App, ui: &mut egui::Ui) {
    if app.jobs.is_empty() {
        return;
    }
    let p = app.palette;
    let mut kill: Option<String> = None;
    ui.horizontal_wrapped(|ui| {
        caps_label(ui, "JOBS", rgb(p.muted));
        for job in &app.jobs {
            let color = if job.running {
                rgb(p.warn)
            } else if job.status.starts_with("exited 0") {
                rgb(p.ok)
            } else {
                rgb(p.danger)
            };
            ui.label(
                egui::RichText::new(format!("{} [{}]", job.id, job.status))
                    .color(color)
                    .small(),
            )
            .on_hover_text(format!(
                "{}\n{} unread byte(s)",
                job.command, job.unread
            ));
            if job.running && ghost_button(ui, &p, "Kill").clicked() {
                kill = Some(job.id.clone());
            }
        }
    });
    // Killing from the UI goes through the same tool the model would use,
    // so the completion notice reaches the model either way.
    if let Some(id) = kill {
        let session_id = app.chat.session_id;
        app.send(UiCommand::SendRequest(Request::RunCommand {
            session_id,
            name: "job-kill".into(),
            input: id,
        }));
    }
    ui.add_space(4.0);
}

fn draw_plan_todo_row(app: &mut App, ui: &mut egui::Ui) {
    if app.todos.is_empty() && !app.plan_active {
        return;
    }
    let p = app.palette;
    let plan_active = app.plan_active;
    ui.horizontal_wrapped(|ui| {
        let label = if plan_active { "PLAN ON" } else { "PLAN OFF" };
        if ghost_button(ui, &p, label)
            .on_hover_text(if plan_active {
                "Leave plan mode (no review — your direct action)."
            } else {
                "Enter plan mode: explore-only until exit-plan-mode."
            })
            .clicked()
        {
            let id = app.chat.session_id;
            app.send(UiCommand::SendRequest(Request::SetPlanMode {
                session_id: id,
                active: !plan_active,
            }));
        }
        for item in &app.todos {
            let (glyph, color) = match item.status {
                protocol::TodoStatus::Pending => ("○", rgb(p.muted)),
                protocol::TodoStatus::InProgress => ("◐", rgb(p.warn)),
                protocol::TodoStatus::Completed => ("●", rgb(p.ok)),
            };
            ui.label(
                egui::RichText::new(format!("{glyph} {}", item.content))
                    .color(color)
                    .small(),
            );
        }
    });
    ui.add_space(4.0);
}

/// App-modal question dialog (`ask-user` / plan review). No dismiss: the
/// turn blocks until the human answers, so hiding the prompt would strand
/// it. Options answer with one click; the field answers in free text.
pub fn draw_question_modal(app: &mut App, ui: &mut egui::Ui) {
    if app.pending_question.is_none() {
        return;
    }
    let mut answer: Option<String> = None;
    egui::Window::new("❓ Answer needed")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ui.ctx(), |ui| {
            let Some(q) = app.pending_question.as_mut() else { return };
            if q.session_id != app.chat.session_id {
                caps_label(
                    ui,
                    &format!("for session {}", q.session_id),
                    rgb(app.palette.muted),
                );
            }
            ui.label(
                egui::RichText::new(&q.question)
                    .color(rgb(app.palette.ink)),
            );
            ui.add_space(6.0);
            for opt in q.options.clone() {
                if ui.button(opt.clone()).clicked() {
                    answer = Some(opt);
                }
            }
            if !q.options.is_empty() {
                ui.add_space(4.0);
            }
            ui.horizontal(|ui| {
                let resp = ui.text_edit_singleline(&mut q.draft);
                let send = ui.button("Send answer").clicked()
                    || (resp.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                if send && !q.draft.trim().is_empty() {
                    answer = Some(q.draft.trim().to_string());
                }
            });
            caps_label(
                ui,
                "no answer in 10 min fails the asking call",
                rgb(app.palette.muted),
            );
        });
    if let Some(a) = answer {
        if let Some(q) = app.pending_question.take() {
            app.send(UiCommand::SendRequest(Request::AnswerQuestion {
                id: q.id,
                answer: a,
            }));
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}
