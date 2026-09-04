//! Control-plane **takeovers** (§6.1, §6.2). An approval request and a
//! user question do not stack above the composer — they *replace* it, with
//! the transcript still scrollable above. That is the whole interaction
//! model: when the turn is blocked on you, the thing you type into is the
//! thing that unblocks it.
//!
//! Approval: a `warn`-bordered card, a "Waiting for approval" strip, the
//! reason as the headline, the command in mono, and exactly two actions —
//! **Reject** and **Allow once**. No countdown is rendered (the backend's
//! 5-minute deadline is real, so it is the strip's tooltip, not a ticking
//! number).
//!
//! Questions: header, markdown-ish detail, numbered options that answer on
//! click, and a free-text field that is always available. A plan review is
//! the same takeover with **Chat about it · Refuse · Approve**.

use egui::{Align, Align2, Layout, Rounding, Sense, Stroke, Vec2};

use protocol::Request;
use sica_core::theme::tokens::{RADIUS_MENU, RADIUS_PILL};

use crate::app::App;
use crate::supervisor::UiCommand;
use crate::ui::kit::{self, Elevation, Level, Weight};

/// Draw whichever takeover is pending for the active session. Returns `true`
/// when it replaced the composer.
pub fn takeover(app: &mut App, ui: &mut egui::Ui) -> bool {
    if approval(app, ui) {
        return true;
    }
    question(app, ui)
}

// ---------------------------------------------------------------------------
// Approval
// ---------------------------------------------------------------------------

fn approval(app: &mut App, ui: &mut egui::Ui) -> bool {
    let pending = app
        .pending_approval
        .as_ref()
        .filter(|a| a.session_id == app.chat.session_id)
        .map(|a| (a.id, a.skill.clone(), a.args_preview.clone(), a.reason.clone()));
    let Some((id, skill, preview, reason)) = pending else {
        return false;
    };
    let t = app.theme;
    let mut verdict: Option<bool> = None;

    egui::Frame::none()
        .fill(kit::col(t.alias.bg_layer[1]))
        // State borders stay 1 px — only neutral hairlines are 0.5.
        .stroke(Stroke::new(1.0, kit::col(t.alias.warn)))
        .rounding(Rounding::same(RADIUS_MENU))
        .shadow(egui::epaint::Shadow {
            offset: Vec2::new(0.0, 3.0),
            blur: 12.0,
            spread: 0.0,
            color: egui::Color32::from_black_alpha(if t.dark { 40 } else { 24 }),
        })
        .inner_margin(egui::Margin::ZERO)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Strip.
            egui::Frame::none()
                .fill(kit::col(t.alias.warn_tertiary))
                .rounding(Rounding {
                    nw: RADIUS_MENU,
                    ne: RADIUS_MENU,
                    sw: 0.0,
                    se: 0.0,
                })
                .inner_margin(egui::Margin::symmetric(14.0, 8.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        kit::state_dot(ui, kit::DotState::Warning, 8.0);
                        ui.add_space(6.0);
                        kit::label(
                            ui,
                            kit::txt(
                                "Waiting for approval",
                                13.0,
                                Weight::Medium,
                                kit::col(t.alias.warn_label),
                            ),
                        )
                        .on_hover_text("No answer within 5 minutes denies the call.");
                    });
                });
            egui::Frame::none()
                .inner_margin(egui::Margin::symmetric(16.0, 12.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let headline = if reason.trim().is_empty() {
                        format!("Tool {skill} requests privileged execution")
                    } else {
                        reason.clone()
                    };
                    ui.add(egui::Label::new(kit::txt(
                        headline,
                        15.0,
                        Weight::Medium,
                        kit::col(t.alias.label[0]),
                    )));
                    if !preview.is_empty() {
                        ui.add_space(6.0);
                        ui.add(
                            egui::Label::new(kit::mono(&preview, 13.0, kit::col(t.alias.label[2])))
                                .wrap(),
                        );
                    }
                    ui.add_space(10.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if kit::button(ui, "Allow once", kit::Variant::Primary, kit::Size::Md)
                            .clicked()
                        {
                            verdict = Some(true);
                        }
                        ui.add_space(8.0);
                        if kit::button(ui, "Reject", kit::Variant::Danger, kit::Size::Md).clicked()
                        {
                            verdict = Some(false);
                        }
                    });
                });
        });

    if let Some(allow) = verdict {
        let sid = app.chat.session_id;
        app.chat.waiting_sessions.remove(&sid);
        app.pending_approval = None;
        app.send(UiCommand::SendRequest(Request::ResolveApproval { id, allow }));
    }
    true
}

// ---------------------------------------------------------------------------
// Questions and plan review
// ---------------------------------------------------------------------------

fn question(app: &mut App, ui: &mut egui::Ui) -> bool {
    let Some(q) = app.pending_question.as_ref() else {
        return false;
    };
    if q.session_id != app.chat.session_id {
        return false;
    }
    let t = app.theme;
    let plan = q.plan_review;
    let question = q.question.clone();
    let options = q.options.clone();
    let mut answer: Option<String> = None;

    kit::elevated_frame(
        &t,
        Elevation::Prominent,
        Level::L2,
        kit::col(t.alias.bg_layer[1]),
        RADIUS_MENU,
    )
    .inner_margin(egui::Margin::ZERO)
    .show(ui, |ui| {
        ui.set_width(ui.available_width());
        // Eyebrow strip.
        egui::Frame::none()
            .fill(if plan {
                kit::col(t.alias.business_tertiary)
            } else {
                kit::cola(t.alias.hover)
            })
            .rounding(Rounding {
                nw: RADIUS_MENU,
                ne: RADIUS_MENU,
                sw: 0.0,
                se: 0.0,
            })
            .inner_margin(egui::Margin::symmetric(14.0, 8.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                kit::label(
                    ui,
                    kit::txt(
                        if plan { "Plan review" } else { "Waiting for your answer" },
                        13.0,
                        Weight::Medium,
                        if plan {
                            kit::col(t.alias.business)
                        } else {
                            kit::col(t.alias.label[1])
                        },
                    ),
                )
                .on_hover_text("No answer within 10 minutes fails the asking call.");
            });

        egui::Frame::none()
            .inner_margin(egui::Margin::symmetric(16.0, 12.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical()
                    .id_source("question_body")
                    .max_height(336.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(kit::txt(
                            &question,
                            if plan { 14.0 } else { 15.0 },
                            if plan { Weight::Regular } else { Weight::Medium },
                            kit::col(t.alias.label[0]),
                        )));
                    });
                ui.add_space(10.0);

                if plan {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let approve = options
                            .iter()
                            .find(|o| o.to_lowercase().contains("approve"))
                            .cloned()
                            .unwrap_or_else(|| "approve".into());
                        let refuse = options
                            .iter()
                            .find(|o| {
                                let l = o.to_lowercase();
                                l.contains("refuse") || l.contains("reject") || l.contains("no")
                            })
                            .cloned()
                            .unwrap_or_else(|| "refuse".into());
                        if kit::button(ui, "Approve", kit::Variant::Primary, kit::Size::Md).clicked()
                        {
                            answer = Some(approve);
                        }
                        ui.add_space(8.0);
                        if kit::button(ui, "Refuse", kit::Variant::Outline, kit::Size::Md).clicked()
                        {
                            answer = Some(refuse.clone());
                        }
                        ui.add_space(8.0);
                        if kit::button(ui, "Chat about it", kit::Variant::Ghost, kit::Size::Md)
                            .on_hover_text("Answer in your own words instead")
                            .clicked()
                        {
                            answer = Some("Let's discuss the plan before acting.".into());
                        }
                    });
                } else {
                    // Numbered options: a 20 px badge, the label, and a
                    // "Recommended" pill where the model suffixed one — the
                    // original label is what gets sent back.
                    for (i, opt) in options.iter().enumerate() {
                        let (label, recommended) = strip_recommended(opt);
                        let (rect, resp) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), 34.0),
                            Sense::click(),
                        );
                        if resp.hovered() {
                            ui.painter().rect_filled(
                                rect,
                                Rounding::same(sica_core::theme::tokens::RADIUS_INPUT),
                                kit::cola(t.alias.hover),
                            );
                        }
                        let badge = egui::pos2(rect.min.x + 14.0, rect.center().y);
                        ui.painter()
                            .circle_filled(badge, 10.0, kit::cola(t.alias.hover));
                        ui.painter().text(
                            badge,
                            Align2::CENTER_CENTER,
                            format!("{}", i + 1),
                            kit::font(12.0, Weight::Medium),
                            kit::col(t.alias.label[1]),
                        );
                        ui.painter().text(
                            egui::pos2(rect.min.x + 32.0, rect.center().y),
                            Align2::LEFT_CENTER,
                            &label,
                            kit::font(14.0, Weight::Regular),
                            kit::col(t.alias.label[0]),
                        );
                        if recommended {
                            let pill = egui::Rect::from_center_size(
                                egui::pos2(rect.max.x - 56.0, rect.center().y),
                                Vec2::new(92.0, 20.0),
                            );
                            ui.painter().rect_filled(
                                pill,
                                Rounding::same(RADIUS_PILL),
                                kit::col(t.alias.business_tertiary),
                            );
                            ui.painter().text(
                                pill.center(),
                                Align2::CENTER_CENTER,
                                "Recommended",
                                kit::font(11.0, Weight::Medium),
                                kit::col(t.alias.business),
                            );
                        }
                        if resp.clicked() {
                            answer = Some(opt.clone());
                        }
                    }
                    if !options.is_empty() {
                        ui.add_space(6.0);
                    }
                    let mut draft = app
                        .pending_question
                        .as_ref()
                        .map(|q| q.draft.clone())
                        .unwrap_or_default();
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut draft)
                            .hint_text("Type your answer")
                            .desired_width(ui.available_width())
                            .frame(true),
                    );
                    if let Some(q) = app.pending_question.as_mut() {
                        q.draft = draft.clone();
                    }
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.add_space(8.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let send = kit::button_enabled(
                            ui,
                            "Submit",
                            kit::Variant::Primary,
                            kit::Size::Md,
                            !draft.trim().is_empty(),
                        )
                        .clicked();
                        if (send || enter) && !draft.trim().is_empty() {
                            answer = Some(draft.trim().to_string());
                        }
                    });
                }
            });
    });

    if let Some(a) = answer {
        if let Some(q) = app.pending_question.take() {
            let sid = app.chat.session_id;
            app.chat.waiting_sessions.remove(&sid);
            app.send(UiCommand::SendRequest(Request::AnswerQuestion {
                id: q.id,
                answer: a,
            }));
        }
    }
    true
}

/// `"Rebuild now (recommended)"` → `("Rebuild now", true)`. The badge is
/// cosmetic: the original string is what travels back, because the model
/// asked with it.
fn strip_recommended(label: &str) -> (String, bool) {
    let lower = label.to_lowercase();
    for marker in ["(recommended)", "[recommended]", "— recommended", "- recommended"] {
        if let Some(idx) = lower.rfind(marker) {
            return (label[..idx].trim().to_string(), true);
        }
    }
    (label.to_string(), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommended_suffix_becomes_a_badge_not_part_of_the_answer() {
        assert_eq!(
            strip_recommended("Rebuild now (recommended)"),
            ("Rebuild now".to_string(), true)
        );
        assert_eq!(
            strip_recommended("Leave it"),
            ("Leave it".to_string(), false)
        );
    }
}
