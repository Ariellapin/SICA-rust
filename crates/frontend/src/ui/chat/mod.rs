//! The conversation column (§2.2, §3.7): a header with the workspace /
//! session breadcrumb and the header actions, the transcript in a centred
//! `clamp(680, column × 0.64, 920)` content column, and the composer seat at
//! the bottom (`content + 32` wide).
//!
//! In the **hero phase** — a session with nothing in it — the header is
//! hidden and the whole stack centres vertically: mark, wordmark, the build
//! badge, the workspace chip, and the composer, which is *not* remounted
//! between phases (the `TextEdit` keeps its id, so focus and draft survive).

pub mod at_menu;
mod composer;
mod control;
pub mod details;
mod dock;
mod messages;
mod meter;
mod slash_menu;
mod tool_row;
pub mod trajectory;
mod user_text;

use egui::{Align, Align2, Layout, Rect, Sense, Vec2};

use crate::app::{App, ChatView};
use crate::supervisor::UiCommand;
use crate::ui::icons::{self, Icon};
use crate::ui::kit::{self, Weight};
use crate::ui::{centered_column, content_width};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    if !app.ipc_state.connected {
        draw_no_be(app, ui);
        return;
    }
    let disabled = !app.llm_state.is_ready();
    let hero = app.chat.turns.is_empty();
    let content_w = content_width(app, ui.available_width());

    if hero {
        hero_view(app, ui, disabled, content_w);
    } else if app.view == ChatView::Trajectory {
        // The ledger is full-bleed: no content column, no composer seat.
        // Sending a message from here would be sending it into a view that
        // cannot show the reply.
        header(app, ui);
        trajectory::draw(app, ui);
    } else {
        header(app, ui);
        egui::TopBottomPanel::bottom(egui::Id::new("composer_seat"))
            .show_separator_line(false)
            .frame(egui::Frame::none().inner_margin(egui::Margin {
                left: 0.0,
                right: 0.0,
                top: 6.0,
                bottom: 10.0,
            }))
            .show_inside(ui, |ui| {
                centered_column(ui, content_w + 32.0, |ui| {
                    composer::draw(app, ui, disabled);
                });
            });
        centered_column(ui, content_w, |ui| messages::draw(app, ui));
    }
    composer::drop_overlay(app, ui);
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

fn header(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    let title = app
        .chat
        .sessions
        .iter()
        .find(|s| s.id == app.chat.session_id)
        .map(|s| s.title.clone())
        .unwrap_or_else(|| format!("Session {}", app.chat.session_id));

    egui::Frame::none()
        .inner_margin(egui::Margin {
            left: 20.0,
            right: 28.0,
            top: 12.0,
            bottom: 8.0,
        })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.set_min_height(32.0);
                // Workspace crumb — one root, so it is the first crumb and
                // its tooltip is the full path (click copies it).
                let ws = crumb(ui, &app.workspace_name, false, &t);
                if ws
                    .on_hover_text(
                        sica_core::paths::working_dir()
                            .display()
                            .to_string(),
                    )
                    .clicked()
                {
                    let path = sica_core::paths::working_dir().display().to_string();
                    ui.ctx().output_mut(|o| o.copied_text = path);
                }
                kit::label(
                    ui,
                    kit::txt("/", 14.0, Weight::Regular, kit::col(t.alias.label[3])),
                );
                crumb(ui, &title, true, &t);

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    jobs_action(app, ui);
                });
            });
            stats_line(app, ui);
            tab_strip(app, ui);
        });
    let rect = ui.min_rect();
    ui.painter().hline(
        rect.x_range(),
        ui.min_rect().max.y,
        egui::Stroke::new(
            sica_core::theme::tokens::HAIRLINE,
            kit::Level::L3.color(&t),
        ),
    );
}

/// The projection line under the session crumb (guide §3.3): turns,
/// messages, tool calls, retries, elapsed.
///
/// Folded by the backend from the log, so it counts what *happened* rather
/// than what the model can still see — a compaction shrinks the context,
/// not the session's history. Absent until the first fold answers, and
/// silent on a session that has nothing in it yet.
fn stats_line(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    let Some(s) = app.stats.stats else { return };
    if s.turns == 0 {
        return;
    }
    let mut parts = vec![
        format!("{} turn{}", s.turns, plural(s.turns)),
        format!("{} message{}", s.user_msgs + s.assistant_msgs, plural(s.user_msgs + s.assistant_msgs)),
    ];
    if s.tool_calls > 0 {
        let mut tools = format!("{} tool call{}", s.tool_calls, plural(s.tool_calls));
        if s.tool_failures > 0 {
            tools.push_str(&format!(" ({} failed)", s.tool_failures));
        }
        parts.push(tools);
    }
    if s.retries > 0 {
        parts.push(format!("{} retr{}", s.retries, if s.retries == 1 { "y" } else { "ies" }));
    }
    if s.wall_ms >= 1_000 {
        parts.push(elapsed(s.wall_ms));
    }
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        kit::label(
            ui,
            kit::txt(parts.join(" · "), 12.0, Weight::Regular, kit::col(t.alias.label[3])),
        );
    });
}

fn plural(n: u32) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Wall time as the coarsest unit that still reads: `42s`, `7m 12s`, `2h 5m`.
fn elapsed(ms: i64) -> String {
    let secs = ms / 1_000;
    if secs < 60 {
        return format!("{secs}s");
    }
    let (m, s) = (secs / 60, secs % 60);
    if m < 60 {
        return format!("{m}m {s}s");
    }
    format!("{}h {}m", m / 60, m % 60)
}

/// `Chat · Trajectory` (§2.1): gap 36, 13/16 500, `label[2]`, the active tab
/// in `business` text over a 2 px r=2 underline.
///
/// dsh draws the strip only when more than one view is registered; there are
/// exactly two here, and the second is the only door onto the event log, so
/// it is always drawn on a session that has content.
fn tab_strip(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        for (view, label) in [
            (ChatView::Chat, "Chat"),
            (ChatView::Trajectory, "Trajectory"),
        ] {
            let active = app.view == view;
            let font = kit::font(13.0, Weight::Medium);
            let galley =
                ui.fonts(|f| f.layout_no_wrap(label.to_string(), font.clone(), egui::Color32::WHITE));
            let (rect, resp) = ui.allocate_exact_size(
                Vec2::new(galley.size().x, 22.0),
                Sense::click(),
            );
            let color = if active {
                kit::col(t.alias.business)
            } else if resp.hovered() {
                kit::col(t.alias.label[1])
            } else {
                kit::col(t.alias.label[2])
            };
            ui.painter()
                .text(rect.left_top(), Align2::LEFT_TOP, label, font, color);
            if active {
                ui.painter().rect_filled(
                    Rect::from_min_size(
                        egui::pos2(rect.min.x, rect.max.y - 2.0),
                        Vec2::new(rect.width(), 2.0),
                    ),
                    egui::Rounding::same(2.0),
                    color,
                );
            }
            if resp.clicked() {
                app.view = view;
                if view == ChatView::Trajectory {
                    // Opening the tab is the request: a turn may have run
                    // since the last look, so this reloads rather than
                    // showing a page that stops mid-session.
                    app.load_trajectory(true);
                }
            }
            ui.add_space(36.0 - ui.spacing().item_spacing.x);
        }
    });
}

fn crumb(
    ui: &mut egui::Ui,
    text: &str,
    current: bool,
    t: &sica_core::theme::Theme,
) -> egui::Response {
    let font = kit::font(
        14.0,
        if current { Weight::Medium } else { Weight::Regular },
    );
    let shown = kit::elide(ui, text, &font, 220.0);
    let galley = ui.fonts(|f| f.layout_no_wrap(shown.clone(), font.clone(), egui::Color32::WHITE));
    let (rect, resp) =
        ui.allocate_exact_size(galley.size() + Vec2::new(16.0, 8.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(
            rect,
            egui::Rounding::same(sica_core::theme::tokens::RADIUS_CARD),
            kit::cola(t.alias.hover),
        );
    }
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        shown,
        font,
        if current {
            kit::col(t.alias.label[0])
        } else {
            kit::col(t.alias.label[2])
        },
    );
    resp
}

/// Header action (§6.9): rendered only when the session has jobs. Read-mostly
/// — but Kill stays, because a human door onto a runaway process matters more
/// than matching dsh exactly here.
fn jobs_action(app: &mut App, ui: &mut egui::Ui) {
    if app.jobs.is_empty() {
        return;
    }
    let t = app.theme;
    let live = app.jobs.iter().filter(|j| j.running).count();
    let label = if live > 0 {
        format!("{live} background job{}", if live == 1 { "" } else { "s" })
    } else {
        format!("{} job{}", app.jobs.len(), if app.jobs.len() == 1 { "" } else { "s" })
    };
    let font = kit::font(13.0, Weight::Medium);
    let galley = ui.fonts(|f| f.layout_no_wrap(label.clone(), font.clone(), egui::Color32::WHITE));
    let (rect, resp) =
        ui.allocate_exact_size(galley.size() + Vec2::new(44.0, 12.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(
            rect,
            egui::Rounding::same(sica_core::theme::tokens::RADIUS_PILL),
            kit::cola(t.alias.hover),
        );
    }
    if live > 0 {
        kit::paint_state_dot(
            ui.painter(),
            Rect::from_center_size(
                egui::pos2(rect.min.x + 14.0, rect.center().y),
                Vec2::splat(8.0),
            ),
            kit::DotState::Ongoing,
            &t,
            ui.input(|i| i.time) as f32,
        );
    }
    ui.painter().text(
        egui::pos2(rect.min.x + 24.0, rect.center().y),
        Align2::LEFT_CENTER,
        &label,
        font,
        kit::col(t.alias.label[1]),
    );
    icons::paint(
        ui.painter(),
        Rect::from_center_size(
            egui::pos2(rect.max.x - 12.0, rect.center().y),
            Vec2::splat(12.0),
        ),
        Icon::ChevronDown,
        kit::col(t.alias.label[2]),
    );
    if resp.clicked() {
        app.menu_open.jobs = !app.menu_open.jobs;
    }

    let jobs = app.jobs.clone();
    let items: Vec<kit::MenuItem> = jobs
        .iter()
        .map(|j| {
            kit::MenuItem::new(format!("{} · {}", j.id, j.status))
                .detail(kit::one_line(&j.command, 60))
        })
        .chain(std::iter::once(
            kit::MenuItem::new("Kill the running jobs")
                .danger(true)
                .sep_above(true),
        ))
        .collect();
    let mut open = app.menu_open.jobs;
    let picked = kit::menu(
        ui.ctx(),
        egui::Id::new("jobs_menu"),
        rect,
        kit::MenuSide::Below,
        320.0,
        &items,
        &mut open,
    );
    app.menu_open.jobs = open;
    if let Some(i) = picked {
        let session_id = app.chat.session_id;
        if i < jobs.len() {
            // Show output: the same tool the model would call, so the result
            // reaches the transcript rather than a UI-only pane.
            app.last_command_session = Some(session_id);
            app.send(UiCommand::SendRequest(protocol::Request::RunCommand {
                session_id,
                name: "job-output".into(),
                input: jobs[i].id.clone(),
            }));
        } else {
            for j in jobs.iter().filter(|j| j.running) {
                app.send(UiCommand::SendRequest(protocol::Request::RunCommand {
                    session_id,
                    name: "job-kill".into(),
                    input: j.id.clone(),
                }));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Hero + empty states
// ---------------------------------------------------------------------------

fn hero_view(app: &mut App, ui: &mut egui::Ui, disabled: bool, content_w: f32) {
    let t = app.theme;
    let avail = ui.available_height();
    // Centre the stack with a 32 px bottom bias, per dsh.
    ui.add_space(((avail - 260.0) / 2.0 - 32.0).max(12.0));
    centered_column(ui, content_w + 32.0, |ui| {
        ui.vertical_centered(|ui| {
            ui.horizontal(|ui| {
                let w = ui.available_width();
                ui.add_space((w / 2.0 - 86.0).max(0.0));
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(38.0), Sense::hover());
                icons::mark(ui, rect, kit::col(t.alias.label[0]));
                ui.add_space(8.0);
                kit::label(
                    ui,
                    kit::txt("sica", 26.0, Weight::Medium, kit::col(t.alias.label[0])),
                );
                ui.add_space(6.0);
                let badge = if app.release_profile { "Release" } else { "Preview" };
                kit::tinted_pill(
                    ui,
                    badge,
                    kit::col(t.alias.business_tertiary),
                    kit::col(t.alias.business),
                    11.0,
                );
            });
            ui.add_space(10.0);
            // Workspace chip — one root, so it names it rather than picking.
            let resp = kit::tinted_pill(
                ui,
                &app.workspace_name,
                kit::col(t.alias.tip),
                kit::col(t.alias.label[1]),
                12.0,
            );
            let path = sica_core::paths::working_dir().display().to_string();
            if resp.on_hover_text(&path).clicked() {
                ui.ctx().output_mut(|o| o.copied_text = path);
            }
            ui.add_space(18.0);
        });
        composer::draw(app, ui, disabled);
    });
}

fn draw_no_be(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    let avail = ui.available_size();
    ui.allocate_ui_with_layout(
        avail,
        Layout::centered_and_justified(egui::Direction::TopDown),
        |ui| {
            ui.vertical_centered(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(96.0), Sense::hover());
                icons::mark(
                    ui,
                    rect,
                    kit::col(t.alias.label[0]).linear_multiply(0.25),
                );
                ui.add_space(12.0);
                kit::label(
                    ui,
                    kit::txt(
                        "No backend running",
                        20.0,
                        Weight::Medium,
                        kit::col(t.alias.label[0]),
                    ),
                );
                ui.add_space(4.0);
                kit::label(
                    ui,
                    kit::txt(
                        "The daemon is not connected. Start it to begin.",
                        14.0,
                        Weight::Regular,
                        kit::col(t.alias.label[2]),
                    ),
                );
                ui.add_space(16.0);
                if kit::button(ui, "Start backend", kit::Variant::Primary, kit::Size::Md).clicked()
                {
                    app.send(UiCommand::StartBe);
                }
            });
        },
    );
}
