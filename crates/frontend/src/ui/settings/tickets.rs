//! Settings › Diagnostics › Improvement tickets — what the idealist filed,
//! and what the end-of-session investigator concluded about it.
//!
//! One row per ticket (one ticket per *kind* of failure): a state dot for
//! its status, the module that failed, how often, and — once investigated —
//! the category, confidence and lesson. The actions are the decisions only
//! a person makes: resolved, noise, reopen. Investigating is the backend's
//! job; the one button here asks it to do the current session now rather
//! than after the idle timer.

use protocol::{Request, TicketSummary};

use crate::app::App;
use crate::supervisor::UiCommand;
use crate::ui::kit::{self, DotState, Weight};

use super::{row, section};

fn dot(status: &str) -> DotState {
    match status {
        "open" => DotState::Error,
        "investigating" => DotState::Ongoing,
        "diagnosed" | "investigation_failed" => DotState::Warning,
        "resolved" => DotState::Done,
        _ => DotState::Idle,
    }
}

fn closed(status: &str) -> bool {
    matches!(status, "resolved" | "wontfix" | "noise")
}

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    section(ui, "Improvement tickets");
    kit::label(
        ui,
        kit::txt(
            "Every failure the backend sees is filed here, one ticket per kind of \
             failure. When a session ends (idle, archived, or on request) a read-only \
             investigator reads each open ticket with the session log and the source, \
             and writes its root cause back into the ticket.",
            12.0,
            Weight::Regular,
            kit::col(t.alias.label[2]),
        ),
    );

    if app.chat.idealist.tickets.is_none() && app.be_state.running {
        // First look: load the list. `Some` right away so this frame and
        // the next do not each send another request.
        app.chat.idealist.tickets = Some(Vec::new());
        app.send(UiCommand::SendRequest(Request::ListTickets));
    }

    let session_id = app.chat.session_id;
    row(
        ui,
        "Investigate this session",
        "Run the investigator on the open session's tickets now instead of \
         waiting for it to go idle. It waits for any running turn to finish.",
        |ui| {
            if kit::button_enabled(
                ui,
                "Investigate",
                kit::Variant::Outline,
                kit::Size::Sm,
                app.be_state.running && session_id != 0,
            )
            .clicked()
            {
                app.send(UiCommand::SendRequest(Request::InvestigateSession { session_id }));
            }
        },
    );
    row(ui, "Show closed tickets", "Resolved, won't-fix and noise", |ui| {
        if kit::button(ui, "Refresh", kit::Variant::Ghost, kit::Size::Sm).clicked() {
            app.send(UiCommand::SendRequest(Request::ListTickets));
        }
        ui.checkbox(&mut app.chat.idealist.show_closed, "");
    });

    let tickets: Vec<TicketSummary> = app
        .chat
        .idealist
        .tickets
        .as_ref()
        .map(|v| {
            v.iter()
                .filter(|t| app.chat.idealist.show_closed || !closed(&t.status))
                .cloned()
                .collect()
        })
        .unwrap_or_default();

    ui.add_space(10.0);
    if tickets.is_empty() {
        kit::footnote(ui, "No tickets to show.");
        return;
    }
    for ticket in &tickets {
        if let Some(req) = ticket_card(app, ui, ticket) {
            app.send(UiCommand::SendRequest(req));
        }
        ui.add_space(8.0);
    }
}

/// One ticket. Returns the status change the person asked for, if any.
fn ticket_card(app: &App, ui: &mut egui::Ui, tk: &TicketSummary) -> Option<Request> {
    let t = app.theme;
    let mut out = None;
    kit::card_frame(&t).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            kit::state_dot(ui, dot(&tk.status), 10.0);
            kit::label(ui, kit::mono(&tk.id, 12.0, kit::col(t.alias.label[1])));
            kit::label(ui, kit::txt(&tk.module, 13.0, Weight::Medium, kit::col(t.alias.label[0])));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut meta = format!("{} · ×{}", tk.status.replace('_', " "), tk.occurrences);
                if tk.regressions > 0 {
                    meta.push_str(&format!(" · {} regression(s)", tk.regressions));
                }
                kit::label(ui, kit::txt(meta, 12.0, Weight::Regular, kit::col(t.alias.label[2])));
            });
        });
        let font = kit::font(12.0, Weight::Regular);
        let msg = kit::elide(ui, &kit::one_line(&tk.last_message, 400), &font, ui.available_width());
        kit::label(ui, kit::txt(msg, 12.0, Weight::Regular, kit::col(t.alias.label[2])));
        if let (Some(cat), Some(conf)) = (&tk.category, &tk.confidence) {
            kit::label(
                ui,
                kit::txt(
                    format!("Diagnosed: {} · confidence {conf}", cat.replace('_', " ")),
                    12.0,
                    Weight::Medium,
                    kit::col(t.alias.label[1]),
                ),
            );
        }
        if let Some(lesson) = &tk.lesson {
            ui.add(
                egui::Label::new(kit::txt(
                    format!("Lesson: {lesson}"),
                    12.0,
                    Weight::Regular,
                    kit::col(t.alias.label[1]),
                ))
                .wrap(),
            );
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if kit::button(ui, "Open ticket", kit::Variant::Ghost, kit::Size::Sm).clicked() {
                let _ = super::open_path(std::path::Path::new(&tk.path));
            }
            // A harness bug is fixed in code: open a session in the
            // sica-rust checkout with the fix prompt in the composer. It is
            // never sent from here.
            let is_bug = tk.category.as_deref() == Some("harness_bug");
            if (is_bug || tk.fix_session.is_some()) && !closed(&tk.status) {
                let label = if tk.fix_session.is_some() { "Open fix session" } else { "Start fix session" };
                if kit::button(ui, label, kit::Variant::Primary, kit::Size::Sm).clicked() {
                    out = Some(Request::StartFixSession { ticket_id: tk.id.clone() });
                }
            }
            let set = |status: &str| Request::SetTicketStatus {
                ticket_id: tk.id.clone(),
                status:    status.into(),
            };
            if closed(&tk.status) {
                if kit::button(ui, "Reopen", kit::Variant::Outline, kit::Size::Sm).clicked() {
                    out = Some(set("open"));
                }
            } else {
                if kit::button(ui, "Resolved", kit::Variant::Outline, kit::Size::Sm).clicked() {
                    out = Some(set("resolved"));
                }
                if kit::button(ui, "Noise", kit::Variant::Ghost, kit::Size::Sm).clicked() {
                    out = Some(set("noise"));
                }
                if kit::button(ui, "Won't fix", kit::Variant::Ghost, kit::Size::Sm).clicked() {
                    out = Some(set("wontfix"));
                }
            }
        });
    });
    out
}
