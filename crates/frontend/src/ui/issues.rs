//! Improvement tickets as a person reads them: the issue count under the
//! sidebar's Settings trigger, its hover summary, and the plain-language
//! wording Settings › Issues shares with it.
//!
//! A ticket is written for the investigator — a module path, an origin
//! tag, a category enum. Everything here turns those into "what failed",
//! "why" and "what to do about it" without losing the precise names, which
//! stay one line below in mono.

use egui::{Align2, Rect, Sense, Vec2};
use protocol::TicketSummary;

use crate::app::{App, SettingsTab};
use crate::ui::icons::{self, Icon};
use crate::ui::kit::{self, DotState, Weight};

/// Still waiting on someone: the investigator or a person.
pub fn is_active(status: &str) -> bool {
    matches!(status, "open" | "investigating" | "diagnosed" | "investigation_failed")
}

/// A person closed it.
pub fn is_closed(status: &str) -> bool {
    matches!(status, "resolved" | "wontfix" | "noise")
}

/// Nobody has found the cause yet, or it came back after being resolved.
pub fn needs_attention(t: &TicketSummary) -> bool {
    matches!(t.status.as_str(), "open" | "investigation_failed") || t.regressions > 0
}

pub fn active(app: &App) -> Vec<&TicketSummary> {
    app.chat
        .idealist
        .tickets
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|t| is_active(&t.status))
        .collect()
}

pub fn dot(status: &str) -> DotState {
    match status {
        "open" => DotState::Error,
        "investigating" => DotState::Ongoing,
        "diagnosed" | "investigation_failed" => DotState::Warning,
        "resolved" => DotState::Done,
        _ => DotState::Idle,
    }
}

pub fn status_label(status: &str) -> &'static str {
    match status {
        "open" => "New",
        "investigating" => "Investigating…",
        "diagnosed" => "Diagnosed",
        "investigation_failed" => "Cause not found",
        "resolved" => "Resolved",
        "wontfix" => "Won't fix",
        "noise" => "Noise",
        _ => "Unknown",
    }
}

/// The investigator's category, as a person would say it.
pub fn category_label(category: &str) -> &'static str {
    match category {
        "harness_bug" => "Bug in sica-rust's own code",
        "model_mistake" => "The model used a tool wrongly",
        "environment" => "Something on this machine",
        "config" => "A configuration file is wrong",
        "external" => "Outside this machine",
        _ => "Other",
    }
}

/// What to do next, by category; `None` is a ticket nobody has
/// investigated yet.
pub fn advice(category: Option<&str>) -> &'static str {
    match category {
        Some("harness_bug") => {
            "Fix with agent opens a session in the sica-rust checkout with the diagnosis \
             in the composer; review the prompt, send it, then mark this resolved once the \
             fix is in."
        }
        Some("model_mistake") => {
            "The harness behaved correctly. The lesson (if any) is added to the model's \
             prompt so it does better next time. If it keeps happening, Fix with agent can \
             sharpen the tool's description or error message."
        }
        Some("environment") => {
            "Fix the machine — install or start what is missing, check the shell or the \
             permissions — then mark this resolved. Fix with agent can make the error \
             message say so next time."
        }
        Some("config") => {
            "Correct the file the cause names (a provider TOML, an agent preset, hooks or \
             an MCP file), then mark this resolved."
        }
        Some("external") => {
            "The model server or a remote service failed. Check it is up; a one-off is \
             noise."
        }
        Some(_) => "Read the full ticket, then decide: fix it, or close it.",
        None => {
            "Nobody has looked into this yet. Investigate has a read-only agent find the \
             root cause from the session log and the source; Fix with agent goes straight \
             to a fix session."
        }
    }
}

/// What failed, in one plain sentence.
pub fn headline(t: &TicketSummary) -> String {
    let m = t.module.as_str();
    let tail = |prefix: &str| m.strip_prefix(prefix).unwrap_or(m).to_string();
    let file = |prefix: &str| {
        let f = tail(prefix);
        f.rsplit('/').next().unwrap_or(&f).to_string()
    };
    match t.origin.as_str() {
        "tool_call" => format!("The `{}` tool failed", tail("agents::tool::")),
        "turn_error" => match tail("backend::turn::").as_str() {
            "llm_request" => "A reply failed: the model request did not go through".into(),
            "context_overflow" => "A turn stopped: the conversation outgrew the context window".into(),
            "empty_reply" | "empty" => "The model sent an empty reply".into(),
            "bad_call" => "The model kept sending tool calls that could not be read".into(),
            "checkpoint" => "A tool call could not be recorded before it ran".into(),
            "prompt" => "The prompt could not be put together".into(),
            other => format!("A turn ended with an error ({other})"),
        },
        "llm_connect" => "Could not connect to the model server".into(),
        "panic" => format!("The backend crashed in {}", file("backend::panic::")),
        "invariant" => format!(
            "A session-log consistency check failed ({})",
            tail("backend::invariants::").replace('_', " ")
        ),
        "frontend" if m.starts_with("frontend::panic") => {
            format!("The app window crashed in {}", file("frontend::panic::"))
        }
        "frontend" if m == "frontend::supervisor::start_be" => "The backend would not start".into(),
        "frontend" if m == "frontend::supervisor::respawn" => "The backend would not restart".into(),
        "frontend" if m == "frontend::supervisor::be_exit" => "The backend stopped unexpectedly".into(),
        "frontend" => "The app window hit an error".into(),
        "config" => format!("A configuration file could not be loaded ({m})"),
        _ if !t.title.trim().is_empty() => kit::one_line(&t.title, 90),
        _ => m.to_string(),
    }
}

/// `2h ago` from a ticket's RFC 3339 stamp; empty when it does not parse.
pub fn ago(stamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(stamp)
        .map(|d| {
            let r = crate::ui::relative_time(d.timestamp());
            if r == "now" { "just now".to_string() } else { format!("{r} ago") }
        })
        .unwrap_or_default()
}

/// A self-contained markdown report for the clipboard — enough to paste
/// into an issue or another agent without the app open.
pub fn report(t: &TicketSummary, body: Option<&str>) -> String {
    let mut out = format!(
        "## {} — ticket `{}`\n\n\
         - **Module:** `{}` · **Origin:** `{}`\n\
         - **Status:** {} · **Occurrences:** {} · **Regressions:** {}\n\
         - **First seen:** {} · **Last seen:** {}\n\
         - **Sessions:** {}\n\
         - **File:** {}\n\n\
         ### What happened\n\n```\n{}\n```\n",
        headline(t),
        t.id,
        t.module,
        t.origin,
        status_label(&t.status),
        t.occurrences,
        t.regressions,
        t.first_seen,
        t.last_seen,
        if t.sessions.is_empty() {
            "none".to_string()
        } else {
            t.sessions.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(", ")
        },
        t.path,
        t.last_message.trim(),
    );
    if let Some(cause) = &t.root_cause {
        let verdict = match (&t.category, &t.confidence) {
            (Some(c), Some(conf)) => format!(" ({}, {conf} confidence)", category_label(c)),
            _ => String::new(),
        };
        out.push_str(&format!("\n### Why{verdict}\n\n{}\n", cause.trim()));
    }
    let fix = t.fix.as_deref().or((!t.hint.trim().is_empty()).then_some(t.hint.as_str()));
    if let Some(fix) = fix {
        out.push_str(&format!("\n### How to fix\n\n{}\n", fix.trim()));
    }
    if !t.files.is_empty() {
        out.push_str("\n### Files to touch\n\n");
        for f in &t.files {
            out.push_str(&format!("- `{f}`\n"));
        }
    }
    if let Some(l) = &t.lesson {
        out.push_str(&format!("\n### Lesson\n\n{}\n", l.trim()));
    }
    if let Some(body) = body {
        out.push_str(&format!("\n---\n\n### Full ticket\n\n{}\n", body.trim()));
    }
    out
}

/// Fill and ink for an issue count: red while something waits for a first
/// look, amber once everything open has been diagnosed.
fn tint(app: &App, urgent: bool) -> (egui::Color32, egui::Color32) {
    let t = app.theme;
    if urgent {
        (
            kit::col(t.alias.error).linear_multiply(if t.dark { 0.35 } else { 0.14 }),
            kit::col(t.alias.error),
        )
    } else {
        (kit::col(t.alias.warn_tertiary), kit::col(t.alias.warn_label))
    }
}

/// The issue count under the Settings trigger. Nothing while there are no
/// open tickets; a click opens Settings › Issues, the hover lists them.
pub fn sidebar_chip(app: &mut App, ui: &mut egui::Ui, collapsed: bool) {
    let open = active(app);
    if open.is_empty() {
        return;
    }
    let n = open.len();
    let urgent = open.iter().any(|t| needs_attention(t));
    let (fill, fg) = tint(app, urgent);
    let hover = app.theme.alias.hover;
    let text = if collapsed {
        n.to_string()
    } else if n == 1 {
        "1 open issue".to_string()
    } else {
        format!("{n} open issues")
    };

    let font = kit::font(12.0, Weight::Medium);
    let galley = ui.fonts(|f| f.layout_no_wrap(text.clone(), font.clone(), fg));
    let h = 24.0;
    let w = galley.size().x + 14.0 + 6.0 + 18.0;
    let resp = ui
        .horizontal(|ui| {
            if collapsed {
                ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
            } else {
                ui.add_space(8.0);
            }
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());
            if ui.is_rect_visible(rect) {
                ui.painter().rect_filled(rect, egui::Rounding::same(h / 2.0), fill);
                // Hover is a second wash painted on top — the tint is
                // translucent, so compositing it by hand would darken it,
                // and egui asserts a `linear_multiply` factor is at most 1.
                if resp.hovered() {
                    ui.painter().rect_filled(rect, egui::Rounding::same(h / 2.0), kit::cola(hover));
                }
                icons::paint(
                    ui.painter(),
                    Rect::from_center_size(egui::pos2(rect.min.x + 9.0 + 7.0, rect.center().y), Vec2::splat(13.0)),
                    Icon::Warning,
                    fg,
                );
                ui.painter().text(
                    egui::pos2(rect.min.x + 9.0 + 14.0 + 6.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    text,
                    font,
                    fg,
                );
            }
            resp
        })
        .inner;
    let resp = resp.on_hover_ui(|ui| hover_summary(app, ui));
    if resp.clicked() {
        app.settings_tab = SettingsTab::Issues;
        app.settings_open = true;
    }
    ui.add_space(4.0);
}

/// How many tickets the hover lists before "+N more".
const HOVER_ROWS: usize = 6;

/// The chip's tooltip: one block per open ticket — what failed, the last
/// message, and what is known about why.
fn hover_summary(app: &App, ui: &mut egui::Ui) {
    let t = app.theme;
    ui.set_max_width(380.0);
    let open = active(app);
    let new = open.iter().filter(|t| needs_attention(t)).count();
    let here = open.iter().filter(|t| t.sessions.contains(&app.chat.session_id)).count();
    kit::label(
        ui,
        kit::txt(
            if open.len() == 1 { "1 open issue".to_string() } else { format!("{} open issues", open.len()) },
            13.0,
            Weight::Medium,
            kit::col(t.alias.label[0]),
        ),
    );
    let mut sub = vec![format!("{new} need a look"), format!("{} diagnosed", open.len() - new)];
    if here > 0 {
        sub.push(format!("{here} from this session"));
    }
    kit::label(ui, kit::txt(sub.join(" · "), 11.0, Weight::Regular, kit::col(t.alias.label[2])));
    for tk in open.iter().take(HOVER_ROWS) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            kit::state_dot(ui, dot(&tk.status), 9.0);
            kit::label(ui, kit::txt(headline(tk), 12.0, Weight::Medium, kit::col(t.alias.label[0])));
            if tk.occurrences > 1 {
                kit::label(
                    ui,
                    kit::txt(format!("×{}", tk.occurrences), 11.0, Weight::Regular, kit::col(t.alias.label[2])),
                );
            }
        });
        ui.horizontal(|ui| {
            ui.add_space(15.0);
            ui.vertical(|ui| {
                kit::label(
                    ui,
                    kit::txt(kit::one_line(&tk.last_message, 110), 11.0, Weight::Regular, kit::col(t.alias.label[2])),
                );
                // Only what is known: "not investigated yet" on every row
                // repeats what the header's count already says.
                let why = match (&tk.category, &tk.root_cause) {
                    (Some(c), Some(cause)) => {
                        Some(format!("{}: {}", category_label(c), kit::one_line(cause, 100)))
                    }
                    (Some(c), None) => Some(category_label(c).to_string()),
                    _ if tk.status == "investigating" => Some("Being investigated now".to_string()),
                    _ if tk.status == "investigation_failed" => {
                        Some("Investigated — cause not found".to_string())
                    }
                    _ => None,
                };
                if let Some(why) = why {
                    kit::label(ui, kit::txt(why, 11.0, Weight::Medium, kit::col(t.alias.label[1])));
                }
            });
        });
    }
    if open.len() > HOVER_ROWS {
        ui.add_space(4.0);
        kit::footnote(ui, &format!("+{} more", open.len() - HOVER_ROWS));
    }
    ui.add_space(6.0);
    kit::footnote(ui, "Click to see why each one happened and fix it.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tk(origin: &str, module: &str) -> TicketSummary {
        TicketSummary {
            id:           "abcdef012345".into(),
            title:        "the analyzer's title".into(),
            hint:         String::new(),
            root_cause:   None,
            fix:          None,
            files:        Vec::new(),
            status:       "open".into(),
            origin:       origin.into(),
            module:       module.into(),
            severity:     "Error".into(),
            occurrences:  1,
            regressions:  0,
            first_seen:   "2026-11-03T10:00:00Z".into(),
            last_seen:    "2026-11-03T11:00:00Z".into(),
            sessions:     vec![7],
            last_message: "exit=1 'rg' is not recognized".into(),
            category:     None,
            confidence:   None,
            lesson:       None,
            fix_session:  None,
            path:         "idealist_workspace/tickets/abcdef012345.md".into(),
        }
    }

    #[test]
    fn headlines_say_what_failed() {
        assert_eq!(headline(&tk("tool_call", "agents::tool::run-cli")), "The `run-cli` tool failed");
        assert!(headline(&tk("turn_error", "backend::turn::context_overflow")).contains("context window"));
        assert!(headline(&tk("turn_error", "backend::turn::something_new")).contains("something_new"));
        assert_eq!(
            headline(&tk("panic", "backend::panic::crates/backend/src/chat.rs")),
            "The backend crashed in chat.rs"
        );
        assert_eq!(
            headline(&tk("frontend", "frontend::panic::crates/frontend/src/app.rs")),
            "The app window crashed in app.rs"
        );
        assert_eq!(headline(&tk("frontend", "frontend::supervisor::start_be")), "The backend would not start");
        assert_eq!(headline(&tk("frontend", "frontend::supervisor::be_exit")), "The backend stopped unexpectedly");
        assert!(headline(&tk("invariant", "backend::invariants::turn_error_ticketed")).contains("turn error ticketed"));
        assert_eq!(headline(&tk("other", "x::y")), "the analyzer's title");
    }

    #[test]
    fn active_and_closed_partition_the_statuses() {
        for s in ["open", "investigating", "diagnosed", "investigation_failed"] {
            assert!(is_active(s) && !is_closed(s), "{s}");
        }
        for s in ["resolved", "wontfix", "noise"] {
            assert!(is_closed(s) && !is_active(s), "{s}");
        }
    }

    #[test]
    fn attention_means_undiagnosed_or_regressed() {
        let mut t = tk("tool_call", "agents::tool::glob");
        assert!(needs_attention(&t));
        t.status = "diagnosed".into();
        assert!(!needs_attention(&t));
        t.regressions = 1;
        assert!(needs_attention(&t));
    }

    #[test]
    fn report_carries_cause_fix_and_body() {
        let mut t = tk("tool_call", "agents::tool::run-cli");
        t.hint = "Retry with run-pwsh.".into();
        let fresh = report(&t, None);
        assert!(fresh.contains("The `run-cli` tool failed") && fresh.contains("`abcdef012345`"));
        assert!(fresh.contains("### How to fix\n\nRetry with run-pwsh."), "{fresh}");
        assert!(!fresh.contains("### Why"));

        t.category = Some("environment".into());
        t.confidence = Some("high".into());
        t.root_cause = Some("rg is not on PATH".into());
        t.fix = Some("Install ripgrep.".into());
        t.files = vec!["skills/run-cli.md".into()];
        let full = report(&t, Some("# body"));
        assert!(full.contains("### Why (Something on this machine, high confidence)"));
        assert!(full.contains("Install ripgrep.") && !full.contains("Retry with run-pwsh."));
        assert!(full.contains("- `skills/run-cli.md`") && full.contains("# body"));
    }

    #[test]
    fn every_category_has_advice() {
        for c in ["harness_bug", "model_mistake", "environment", "config", "external"] {
            assert_ne!(category_label(c), "Other");
            assert!(!advice(Some(c)).is_empty());
        }
        assert!(advice(None).contains("Investigate"));
    }

    #[test]
    fn ago_reads_rfc3339_and_tolerates_junk() {
        assert!(ago("2020-01-01T00:00:00Z").ends_with(" ago"));
        assert_eq!(ago("not a date"), "");
    }
}
