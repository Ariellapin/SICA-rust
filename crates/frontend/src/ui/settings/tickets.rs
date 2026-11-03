//! Settings › Issues — what the idealist filed, in words a person can act
//! on, and the actions that fix it.
//!
//! One card per ticket (one ticket per *kind* of failure). Each card
//! answers three questions: **what happened** (a plain headline and the
//! last message), **why** (the investigator's root cause, or that nobody
//! has looked yet) and **how to fix it** (the proposed fix or the
//! heuristic hint, the files it names, and what to do for its category).
//!
//! The actions: *Fix with agent* opens a session in the sica-rust checkout
//! with the fix prompt in its composer — never sent from here; *Investigate*
//! asks for the investigator now; *Go to session*, *Copy report*, *Full
//! ticket* (the markdown body, read with `GetTicket`) and *Open file*; and
//! the decisions only a person makes: resolved, noise, won't fix, reopen.

use egui::{Align, Layout};
use egui_commonmark::CommonMarkViewer;
use protocol::{Request, TicketSummary};

use crate::app::App;
use crate::supervisor::UiCommand;
use crate::ui::icons::Icon;
use crate::ui::issues::{self, advice, ago, category_label, headline, is_active, is_closed, status_label};
use crate::ui::kit::{self, Weight};

use super::{row, section};

/// What a card asked for, applied once the list is drawn.
enum Action {
    Send(Request),
    Expand(String),
    Collapse(String),
    GoTo(u64),
    Copy(String),
}

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    section(ui, "Issues");
    ui.add(
        egui::Label::new(kit::txt(
            "Every failure the app sees is logged here, one issue per kind of failure — a \
             tool that failed, a turn that ended in an error, a crash. When a session ends \
             (idle, archived, or on request) a read-only investigator reads each new issue \
             with the session log and the source and writes down why it happened. Each card \
             says what happened, why, and how to fix it.",
            12.0,
            Weight::Regular,
            kit::col(t.alias.label[2]),
        ))
        .wrap(),
    );

    if app.chat.idealist.tickets.is_none() && app.be_state.running {
        // Not loaded yet (the connect asks, but this may be the first frame
        // after it). `Some` right away so the next frame does not ask again.
        app.chat.idealist.tickets = Some(Vec::new());
        app.send(UiCommand::SendRequest(Request::ListTickets));
    }

    let all: Vec<TicketSummary> = app.chat.idealist.tickets.clone().unwrap_or_default();
    let open = all.iter().filter(|t| is_active(&t.status)).count();
    let new = all.iter().filter(|t| is_active(&t.status) && issues::needs_attention(t)).count();
    let closed = all.iter().filter(|t| is_closed(&t.status)).count();
    ui.add_space(6.0);
    kit::footnote(
        ui,
        &format!("{open} open · {new} need a look · {} diagnosed · {closed} closed", open - new),
    );

    let session_id = app.chat.session_id;
    row(
        ui,
        "Investigate this session",
        "Run the investigator on the open session's new issues now instead of \
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
    row(
        ui,
        "Show",
        "Only the open session's issues, and closed ones (resolved, won't fix, noise) too",
        |ui| {
            if kit::button(ui, "Refresh", kit::Variant::Ghost, kit::Size::Sm).clicked() {
                app.send(UiCommand::SendRequest(Request::ListTickets));
            }
            ui.checkbox(&mut app.chat.idealist.show_closed, "Closed");
            ui.checkbox(&mut app.chat.idealist.this_session_only, "This session only");
        },
    );

    let shown: Vec<TicketSummary> = all
        .into_iter()
        .filter(|t| app.chat.idealist.show_closed || !is_closed(&t.status))
        .filter(|t| !app.chat.idealist.this_session_only || t.sessions.contains(&session_id))
        .collect();

    ui.add_space(10.0);
    if shown.is_empty() {
        kit::footnote(
            ui,
            if app.chat.idealist.tickets.as_ref().is_some_and(|v| v.is_empty()) {
                "No issues logged. Failures the app sees will appear here."
            } else {
                "No issues match these filters."
            },
        );
        return;
    }
    let mut actions = Vec::new();
    for ticket in &shown {
        actions.extend(card(app, ui, ticket));
        ui.add_space(8.0);
    }
    for a in actions {
        match a {
            Action::Send(req) => app.send(UiCommand::SendRequest(req)),
            Action::Expand(ticket_id) => {
                if !app.chat.idealist.bodies.contains_key(&ticket_id) {
                    app.send(UiCommand::SendRequest(Request::GetTicket { ticket_id: ticket_id.clone() }));
                }
                app.chat.idealist.expanded.insert(ticket_id);
            }
            Action::Collapse(ticket_id) => {
                app.chat.idealist.expanded.remove(&ticket_id);
            }
            Action::GoTo(sid) => {
                app.switch_session(sid);
                app.settings_open = false;
            }
            Action::Copy(text) => {
                ui.ctx().output_mut(|o| o.copied_text = text);
                app.show_toast(Icon::Copy, "Issue report copied", 1800);
            }
        }
    }
}

/// The status pill's fill and ink.
fn status_tint(t: &sica_core::theme::Theme, status: &str) -> (egui::Color32, egui::Color32) {
    let a = &t.alias;
    match status {
        "open" => (
            kit::col(a.error).linear_multiply(if t.dark { 0.35 } else { 0.14 }),
            kit::col(a.error),
        ),
        "diagnosed" | "investigation_failed" => (kit::col(a.warn_tertiary), kit::col(a.warn_label)),
        "resolved" => (kit::col(a.success_tertiary), kit::col(a.success)),
        "investigating" => (kit::cola(a.hover), kit::col(a.label[1])),
        _ => (kit::cola(a.hover), kit::col(a.label[2])),
    }
}

/// A small heading and its wrapped text.
fn field(ui: &mut egui::Ui, heading: &str, text: &str, strong: bool) {
    let t = kit::theme(ui);
    ui.add_space(6.0);
    kit::label(ui, kit::txt(heading, 11.0, Weight::Medium, kit::col(t.alias.label[2])));
    ui.add(
        egui::Label::new(kit::txt(
            text.trim(),
            12.0,
            Weight::Regular,
            kit::col(if strong { t.alias.label[0] } else { t.alias.label[1] }),
        ))
        .wrap(),
    );
}

/// One ticket. Returns what the person asked for.
fn card(app: &mut App, ui: &mut egui::Ui, tk: &TicketSummary) -> Vec<Action> {
    let t = app.theme;
    let mut out = Vec::new();
    let running = app.be_state.running;
    let known_session = tk
        .sessions
        .iter()
        .rev()
        .copied()
        .find(|sid| app.chat.sessions.iter().any(|s| s.id == *sid));
    let expanded = app.chat.idealist.expanded.contains(&tk.id);
    let body = app.chat.idealist.bodies.get(&tk.id).cloned();

    kit::card_frame(&t).show(ui, |ui| {
        ui.set_width(ui.available_width());

        // What failed, its state, how often.
        ui.horizontal(|ui| {
            kit::state_dot(ui, issues::dot(&tk.status), 10.0);
            let font = kit::font(14.0, Weight::Medium);
            let head = kit::elide(ui, &headline(tk), &font, (ui.available_width() - 150.0).max(120.0));
            kit::label(ui, kit::txt(head, 14.0, Weight::Medium, kit::col(t.alias.label[0])))
                .on_hover_text(headline(tk));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (fill, fg) = status_tint(&t, &tk.status);
                kit::tinted_pill(ui, status_label(&tk.status), fill, fg, 11.0);
                if tk.occurrences > 1 {
                    kit::label(
                        ui,
                        kit::txt(format!("×{}", tk.occurrences), 12.0, Weight::Regular, kit::col(t.alias.label[2])),
                    )
                    .on_hover_text(format!("Happened {} times", tk.occurrences));
                }
            });
        });
        let mut meta = vec![tk.module.clone(), tk.id.clone(), format!("last {}", ago(&tk.last_seen))];
        if tk.first_seen != tk.last_seen {
            meta.push(format!("first {}", ago(&tk.first_seen)));
        }
        match tk.sessions.len() {
            0 => meta.push("outside any session".into()),
            1 => meta.push(format!("session {}", tk.sessions[0])),
            n => meta.push(format!("{n} sessions")),
        }
        ui.add(egui::Label::new(kit::mono(meta.join(" · "), 11.0, kit::col(t.alias.label[2]))).wrap());
        if tk.regressions > 0 {
            kit::label(
                ui,
                kit::txt(
                    format!(
                        "Came back {} time(s) after being marked resolved",
                        tk.regressions
                    ),
                    12.0,
                    Weight::Medium,
                    kit::col(t.alias.warn_label),
                ),
            );
        }

        // What happened.
        field(ui, "What happened", &kit::one_line(&tk.last_message, 600), true);

        // Why.
        match (&tk.root_cause, &tk.category) {
            (Some(cause), cat) => {
                let verdict = match (cat, &tk.confidence) {
                    (Some(c), Some(conf)) => format!("Why — {} · {conf} confidence", category_label(c)),
                    (Some(c), None) => format!("Why — {}", category_label(c)),
                    _ => "Why".into(),
                };
                field(ui, &verdict, cause, true);
            }
            (None, Some(c)) => field(
                ui,
                &format!("Why — {}", category_label(c)),
                "The investigator's notes are in the full ticket.",
                false,
            ),
            (None, None) => {
                let text = match tk.status.as_str() {
                    "investigating" => "Being investigated now…",
                    "investigation_failed" => {
                        "The investigator ran but could not pin the cause down. Its notes are in \
                         the full ticket; Fix with agent can pick it up from there."
                    }
                    _ => "Not investigated yet.",
                };
                field(ui, "Why", text, false);
            }
        }

        // How to fix.
        let fix = tk.fix.as_deref().or((!tk.hint.trim().is_empty()).then_some(tk.hint.as_str()));
        let heading = if tk.fix.is_some() { "How to fix" } else { "How to fix — first guess" };
        if let Some(fix) = fix {
            field(ui, heading, fix, true);
        } else {
            ui.add_space(6.0);
            kit::label(ui, kit::txt("How to fix", 11.0, Weight::Medium, kit::col(t.alias.label[2])));
        }
        if is_active(&tk.status) {
            ui.add(
                egui::Label::new(kit::txt(
                    advice(tk.category.as_deref()),
                    12.0,
                    Weight::Regular,
                    kit::col(t.alias.label[2]),
                ))
                .wrap(),
            );
        }
        if !tk.files.is_empty() {
            ui.add_space(2.0);
            ui.add(
                egui::Label::new(kit::mono(
                    format!("Files: {}", tk.files.join(", ")),
                    11.0,
                    kit::col(t.alias.label[1]),
                ))
                .wrap(),
            );
        }
        if let Some(lesson) = &tk.lesson {
            field(ui, "Lesson the model now gets", lesson, false);
        }

        // Fix and understand.
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            if is_active(&tk.status) {
                let label = if tk.fix_session.is_some() { "Open fix session" } else { "Fix with agent" };
                if kit::button_enabled(ui, label, kit::Variant::Primary, kit::Size::Sm, running)
                    .on_hover_text(
                        "Opens a session in the sica-rust checkout with a fix prompt built from \
                         this issue in the composer. Nothing is sent until you send it.",
                    )
                    .clicked()
                {
                    out.push(Action::Send(Request::StartFixSession { ticket_id: tk.id.clone() }));
                }
                let investigable = matches!(tk.status.as_str(), "open" | "investigation_failed");
                if let Some(&sid) = tk.sessions.last().filter(|_| investigable) {
                    if kit::button_enabled(ui, "Investigate", kit::Variant::Outline, kit::Size::Sm, running)
                        .on_hover_text(format!(
                            "Have the read-only investigator find the root cause from session \
                             {sid}'s log and the source."
                        ))
                        .clicked()
                    {
                        out.push(Action::Send(Request::InvestigateSession { session_id: sid }));
                    }
                }
            }
            if let Some(sid) = known_session {
                if kit::button(ui, "Go to session", kit::Variant::Ghost, kit::Size::Sm)
                    .on_hover_text(format!("Open session {sid}, where this last happened"))
                    .clicked()
                {
                    out.push(Action::GoTo(sid));
                }
            }
            if kit::button(ui, "Copy report", kit::Variant::Ghost, kit::Size::Sm)
                .on_hover_text("Copy a markdown report of this issue — paste it into a bug or another agent")
                .clicked()
            {
                out.push(Action::Copy(issues::report(tk, body.as_deref())));
            }
            let toggle = if expanded { "Hide full ticket" } else { "Full ticket" };
            if kit::button(ui, toggle, kit::Variant::Ghost, kit::Size::Sm).clicked() {
                out.push(if expanded {
                    Action::Collapse(tk.id.clone())
                } else {
                    Action::Expand(tk.id.clone())
                });
            }
            if kit::button(ui, "Open file", kit::Variant::Ghost, kit::Size::Sm)
                .on_hover_text(tk.path.as_str())
                .clicked()
            {
                let _ = super::open_path(std::path::Path::new(&tk.path));
            }
        });

        // The person's decision. `horizontal` centres the footnote on the
        // buttons once the row is button-high from the start — egui places
        // the label before the buttons have made the row any taller.
        ui.horizontal(|ui| {
            ui.set_min_height(28.0);
            kit::footnote(ui, "Mark as");
            let set = |status: &str| {
                Action::Send(Request::SetTicketStatus {
                    ticket_id: tk.id.clone(),
                    status:    status.into(),
                })
            };
            if is_closed(&tk.status) {
                if kit::button(ui, "Reopen", kit::Variant::Outline, kit::Size::Sm).clicked() {
                    out.push(set("open"));
                }
            } else {
                if kit::button(ui, "Resolved", kit::Variant::Outline, kit::Size::Sm)
                    .on_hover_text("Fixed. If it happens again it reopens as a regression.")
                    .clicked()
                {
                    out.push(set("resolved"));
                }
                if kit::button(ui, "Noise", kit::Variant::Ghost, kit::Size::Sm)
                    .on_hover_text("Not a real problem. Repeats are counted but stay closed.")
                    .clicked()
                {
                    out.push(set("noise"));
                }
                if kit::button(ui, "Won't fix", kit::Variant::Ghost, kit::Size::Sm)
                    .on_hover_text("A real problem you have decided to live with.")
                    .clicked()
                {
                    out.push(set("wontfix"));
                }
            }
        });

        if expanded {
            ui.add_space(6.0);
            kit::hairline(ui, kit::Level::L2);
            ui.add_space(6.0);
            match &body {
                Some(body) => full_ticket(app, ui, &tk.id, body),
                None => kit::footnote(ui, "Loading the ticket…"),
            }
        }
    });
    out
}

/// Lines of one code block the inline ticket shows; a panic backtrace runs
/// to hundreds, and the file and Copy report have all of it.
const CODE_LINES: usize = 40;

/// The ticket's markdown body: prose through the viewer, code blocks (the
/// message, the traceback) through the kit's card so they stay copyable.
/// Inline rather than in a scroll area of its own: a scroller inside the
/// settings scroller takes the wheel from it, and came out a sliver tall.
fn full_ticket(app: &mut App, ui: &mut egui::Ui, id: &str, body: &str) {
    let t = app.theme;
    ui.scope(|ui| {
        // The viewer draws `**bold**` and headings with the active stroke
        // colour; point it at the primary label, as the transcript does.
        let ink = kit::col(t.alias.label[0]);
        let v = &mut ui.style_mut().visuals.widgets;
        v.active.fg_stroke.color = ink;
        v.noninteractive.fg_stroke.color = ink;
        ui.style_mut().visuals.extreme_bg_color = kit::col(t.alias.code_block);
        for (i, block) in crate::ui::chat::md_blocks::split(body).into_iter().enumerate() {
            use crate::ui::chat::md_blocks::Block;
            let md_id = format!("ticket_md_{id}_{i}");
            match block {
                Block::Code { lang, body } => {
                    let lines = body.lines().count();
                    let shown = if lines > CODE_LINES {
                        body.lines().take(CODE_LINES).collect::<Vec<_>>().join("\n")
                    } else {
                        body
                    };
                    kit::code_block(ui, if lang.is_empty() { "text" } else { &lang }, &shown);
                    if lines > CODE_LINES {
                        kit::footnote(
                            ui,
                            &format!(
                                "… {} more lines — Open file or Copy report for all of it",
                                lines - CODE_LINES
                            ),
                        );
                    }
                }
                Block::Math(s) => kit::code_block(ui, "math", &s),
                Block::Prose(s) | Block::Table(s) => {
                    CommonMarkViewer::new(md_id).show(ui, &mut app.md_cache, &s);
                }
            }
        }
    });
}
