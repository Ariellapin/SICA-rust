//! The transcript (§3): one column of flow items, 16 px apart, no avatars
//! and no role labels anywhere.
//!
//! * **User** — right-aligned bubble, `specific-bubble`, r=22, pad 10 16,
//!   max 70 % of the column; `/name` and `@path` runs decorated.
//! * **Assistant** — flat, full column width, markdown, no streaming caret.
//!   While the turn runs the tail shows the shimmering `TurnStatus` line with
//!   a mono clock after 15 s; an interrupted reply ends with a "Stopped" tag.
//! * **Reasoning** — a disclosure row titled "Think" whose summary is the
//!   latest line while running and the first line once settled.
//! * **Tool calls** — [`super::tool_row`].
//! * **Markers** — compaction / injection / steer rows, also disclosures.
//! * **Turn tail** — copy + timestamp, revealed on the newest turn always and
//!   on hover for older ones.
//!
//! Scroll behaviour keeps sica-rust's own strengths that dsh lacks (middle-
//! click pan, Ctrl+A/Ctrl+C message copy) and adopts dsh's 24 px "at bottom"
//! threshold and floating back-to-bottom circle.

use egui::{
    Align, Key, Layout, Modifiers, PointerButton, Pos2, Rect, Rounding, Sense, Shape, Stroke,
    Vec2,
};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

use sica_core::theme::{
    tokens::{HAIRLINE, RADIUS_BUBBLE, RADIUS_ROW},
    Theme,
};

use crate::app::{App, LogKind, Notice, NoticeKind};
use crate::supervisor::UiCommand;
use crate::ui::icons::Icon;
use crate::ui::kit::{self, DotState, Leading, Weight};

/// dsh's "at bottom" tolerance.
const BOTTOM_EPS: f32 = 24.0;
/// Horizontal padding inside the user bubble (§3.1: pad 10 16).
const BUBBLE_PAD_X: f32 = 16.0;
/// The turn-status clock appears after this long.
const CLOCK_AFTER_SECS: f32 = 15.0;

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    let height = ui.available_height();
    let force_scroll =
        std::mem::take(&mut app.chat.scroll_to_bottom) && !app.chat.autoscroll_paused;
    let mut assistant_rects: Vec<(usize, Rect)> = Vec::new();
    let selected = app.chat.selected_turn;

    let output = egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .stick_to_bottom(!app.chat.autoscroll_paused)
        // Click-drag selects text instead of panning; wheel and scrollbar are
        // unaffected.
        .drag_to_scroll(false)
        .animated(false)
        .max_height(height.max(120.0))
        .show(ui, |ui| {
            // Flow items carry their own 16 px separation (§3); egui's default
            // inter-widget spacing would double it.
            ui.spacing_mut().item_spacing.y = 2.0;
            for i in 0..app.chat.turns.len() {
                if let Some(notice) = app.chat.turns[i].notice.clone() {
                    draw_marker(app, ui, i, &notice);
                    continue;
                }
                draw_turn(app, ui, i, &mut assistant_rects, selected, &t);
            }
            ui.add_space(24.0);
        });

    transcript_input(app, ui, &output, &assistant_rects, force_scroll);
    // A floating control has no business painting over an open modal.
    if output.content_size.y > output.inner_rect.height() && !app.settings_open {
        back_to_bottom(app, ui, visible_viewport(ui, &output));
    }
    // A "Run again" is applied here, after every reader of `turns` has run:
    // `edit_user_message` truncates the vector, and both the loop above and
    // the selection `transcript_input` just set index into it.
    if let Some((idx, text)) = app.chat.pending_edit.take() {
        app.edit_user_message(idx, text);
    }
}

fn draw_turn(
    app: &mut App,
    ui: &mut egui::Ui,
    i: usize,
    assistant_rects: &mut Vec<(usize, Rect)>,
    selected: Option<usize>,
    t: &Theme,
) {
    let (user, assistant, reasoning, finished, collapsed, finish_reason, queued) = {
        let turn = &app.chat.turns[i];
        (
            turn.user.clone(),
            turn.assistant.clone(),
            turn.reasoning.clone(),
            turn.finished,
            turn.reasoning_collapsed,
            turn.finish_reason.clone(),
            turn.queued,
        )
    };
    let has_images = !app.chat.turns[i].images.is_empty();
    if !user.is_empty() || has_images {
        draw_user(app, ui, i, &user, t);
    }
    if has_images {
        draw_user_images(app, ui, i);
    }
    if queued {
        // A queued message has no turn yet — it waits in the composer dock,
        // and the bubble above is the only trace until the BE admits it.
        ui.add_space(2.0);
        kit::footnote(ui, "queued — runs when the current turn ends");
    }

    // Process rows. In Compact display a *closed* turn folds them all behind
    // one button; Normal shows every row.
    let process_count = app.chat.turns[i].tool_chips.len()
        + usize::from(!reasoning.is_empty());
    let fold_id = ui.id().with(("fold", i));
    let folded: bool = if app.transcript_compact && finished && process_count > 0 {
        ui.ctx().data(|d| d.get_temp(fold_id).unwrap_or(true))
    } else {
        false
    };
    if app.transcript_compact && finished && process_count > 0 {
        if process_fold(ui, app, i, folded) {
            ui.ctx().data_mut(|d| d.insert_temp(fold_id, !folded));
        }
    }
    if !folded {
        if !reasoning.is_empty() {
            let summary = if finished {
                reasoning.lines().find(|l| !l.trim().is_empty()).unwrap_or("")
            } else {
                reasoning.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("")
            }
            .to_string();
            let out = kit::disclosure_row(
                ui,
                Leading::Icon(Icon::Think),
                "Think",
                &kit::one_line(&summary, 200),
                !collapsed,
                !finished,
                None,
            );
            if out.clicked {
                app.chat.turns[i].reasoning_collapsed = !collapsed;
            }
            if !collapsed {
                reasoning_body(ui, &reasoning, t);
            }
        }
        super::tool_row::draw(app, ui, i);
    }

    retry_chain(app, ui, i, finished, t);

    if !assistant.is_empty() {
        ui.add_space(8.0);
        // Image targets resolve against the session's folder, not the
        // process's (§3.8).
        let cwd = app.session_workspace().1;
        let body = ui
            .scope(|ui| draw_assistant(ui, &mut app.md_cache, i, &assistant, t, &cwd))
            .response
            .rect;
        assistant_rects.push((i, body));
        if selected == Some(i) {
            ui.painter().rect_filled(
                body.expand2(egui::vec2(6.0, 4.0)),
                RADIUS_ROW,
                kit::col(t.alias.business).linear_multiply(0.12),
            );
        }
    }

    if !finished && is_tail_turn(app, i) {
        turn_status(app, ui, i, t);
    } else if finished {
        match finish_reason.as_deref() {
            Some(r) if r.starts_with("error") => turn_error(ui, t),
            Some("interrupted") => stopped_tag(ui, t),
            Some("max_tokens") | Some("length") => max_tokens_row(ui, t),
            _ => {}
        }
        turn_tail(app, ui, i, &assistant, t);
    }
    ui.add_space(16.0);
}

// ---------------------------------------------------------------------------
// User
// ---------------------------------------------------------------------------

fn draw_user(app: &mut App, ui: &mut egui::Ui, i: usize, text: &str, t: &Theme) {
    if app.chat.editing_turn == Some(i) {
        draw_user_editor(app, ui, i, t);
        return;
    }
    // A prompt can be rewritten once the backend has told us where it landed
    // (`user_seq`) and while nothing is running — a rewind mid-turn would
    // race the loop's own appends, and the backend refuses it anyway.
    let editable = app.chat.turns[i].user_seq.is_some()
        && !app.chat.running_sessions.contains(&app.chat.session_id)
        && app.pending_approval.is_none()
        && app.pending_question.is_none();
    // The reveal area is last frame's bubble, widened to cover the button
    // that appears beside it — otherwise moving onto the button would take
    // the pointer out of the hover rect and the button would blink away.
    let hover_id = ui.id().with(("user_row", i));
    let hovered = ui
        .ctx()
        .data(|d| d.get_temp::<Rect>(hover_id))
        .is_some_and(|r| ui.rect_contains_pointer(r));

    let avail = ui.available_width();
    let max_w = (avail * 0.70).max(180.0);
    // The frame's own horizontal margin is not available to the text.
    let text_w = (max_w - 2.0 * BUBBLE_PAD_X).max(80.0);
    let mut bubble = Rect::NOTHING;
    let mut open_editor = false;
    ui.allocate_ui_with_layout(
        Vec2::new(avail, 0.0),
        Layout::right_to_left(Align::Min),
        |ui| {
            let framed = egui::Frame::none()
                .fill(kit::col(t.alias.bubble))
                .rounding(Rounding::same(RADIUS_BUBBLE))
                .inner_margin(egui::Margin::symmetric(BUBBLE_PAD_X, 10.0))
                .show(ui, |ui| {
                    ui.set_max_width(text_w);
                    let mut job = super::user_text::job(
                        text,
                        kit::col(t.alias.label[0]),
                        kit::col(t.alias.business),
                        kit::font(t.content_px as f32, Weight::Regular),
                    );
                    job.wrap.max_width = text_w;
                    // Lay the job out here instead of handing egui the job:
                    // `Label` replaces `job.wrap` with the wrap mode it derives
                    // from the ui's layout, and this right-to-left row resolves
                    // to `Extend` — which is how a long message became one
                    // endless line running off the right edge. A pre-laid
                    // galley is used verbatim.
                    let galley = ui.fonts(|f| f.layout_job(job));
                    if galley.size().x <= text_w {
                        ui.add(egui::Label::new(galley));
                    } else {
                        // Nothing to break on (one long path, URL or token):
                        // scroll it rather than let it spill over the
                        // transcript. The fixed-width box is what puts a
                        // left-to-right scroll area inside a right-aligned row.
                        ui.allocate_ui_with_layout(
                            Vec2::new(text_w, 0.0),
                            Layout::top_down(Align::Min),
                            |ui| {
                                egui::ScrollArea::horizontal()
                                    .id_source(("user_msg", i))
                                    .show(ui, |ui| ui.add(egui::Label::new(galley)));
                            },
                        );
                    }
                });
            bubble = framed.response.rect;
            // Right-to-left: the bubble is placed first and keeps the right
            // edge, so showing this only on hover never shifts it.
            if editable && hovered {
                ui.add_space(4.0);
                open_editor = kit::icon_button(ui, Icon::Edit, 28.0)
                    .on_hover_text("Edit this prompt and run again")
                    .clicked();
            }
        },
    );
    if bubble.is_positive() {
        let hit = Rect::from_min_max(
            Pos2::new(bubble.min.x - 40.0, bubble.min.y),
            bubble.max,
        );
        ui.ctx().data_mut(|d| d.insert_temp(hover_id, hit));
    }
    if open_editor {
        app.chat.editing_turn = Some(i);
        app.chat.edit_draft = text.to_owned();
        // Re-arm the one-shot focus claim for this turn's field.
        let focus_id = ui.id().with(("edit_prompt", i)).with("focused");
        ui.ctx().data_mut(|d| d.insert_temp(focus_id, false));
    }
    ui.add_space(8.0);
}

/// The user bubble with its text open for rewriting: Enter (or "Run again")
/// re-runs the conversation from here, Esc (or "Cancel") leaves it alone.
fn draw_user_editor(app: &mut App, ui: &mut egui::Ui, i: usize, t: &Theme) {
    let avail = ui.available_width();
    let box_w = (avail * 0.70).max(240.0);
    let field_id = ui.id().with(("edit_prompt", i));
    // Consumed before the editor sees them: Shift+Enter is the newline (the
    // composer's own split), so plain Enter is free to mean "run". Only
    // while the field itself holds focus — the composer stays usable with an
    // editor open, and its Enter must still be its own.
    let submit = ui.memory(|m| m.has_focus(field_id))
        && ui.input_mut(|inp| inp.consume_key(Modifiers::NONE, Key::Enter));
    // Escape closes the editor wherever focus sits. Nothing else wants it
    // here: the composer's Escape interrupts a running turn, and a running
    // turn is exactly when the editor cannot be open.
    let cancel = ui.input_mut(|inp| inp.consume_key(Modifiers::NONE, Key::Escape));
    let mut run = false;
    ui.allocate_ui_with_layout(
        Vec2::new(avail, 0.0),
        Layout::right_to_left(Align::Min),
        |ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(box_w, 0.0),
                Layout::top_down(Align::Min),
                |ui| {
                    // Focus is claimed once, on the frame the editor opens —
                    // re-requesting it every frame would tear it back from
                    // anything else the user clicked.
                    let focus_id = field_id.with("focused");
                    let first = !ui.ctx().data(|d| d.get_temp(focus_id).unwrap_or(false));
                    egui::Frame::none()
                        .fill(kit::col(t.alias.bubble))
                        .rounding(Rounding::same(RADIUS_BUBBLE))
                        .inner_margin(egui::Margin::symmetric(BUBBLE_PAD_X, 10.0))
                        .show(ui, |ui| {
                            let field = egui::TextEdit::multiline(&mut app.chat.edit_draft)
                                .id(field_id)
                                .desired_width(ui.available_width())
                                .font(kit::font(t.content_px as f32, Weight::Regular))
                                .return_key(Some(egui::KeyboardShortcut::new(
                                    Modifiers::SHIFT,
                                    Key::Enter,
                                )))
                                .frame(false);
                            let resp = ui.add(field);
                            if first {
                                resp.request_focus();
                                ui.ctx().data_mut(|d| d.insert_temp(focus_id, true));
                            }
                        });
                    ui.add_space(6.0);
                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                        run = kit::button(ui, "Run again", kit::Variant::Primary, kit::Size::Sm)
                            .clicked();
                        if kit::button(ui, "Cancel", kit::Variant::Ghost, kit::Size::Sm).clicked() {
                            app.chat.editing_turn = None;
                        }
                    });
                    kit::footnote(ui, "the reply and everything after it is replaced");
                },
            );
        },
    );
    if cancel {
        app.chat.editing_turn = None;
        app.chat.edit_draft.clear();
        return;
    }
    if run || submit {
        // Deferred, not applied: the transcript loop is iterating `turns`
        // and this rewrites it. `draw` picks it up once the loop is done.
        app.chat.pending_edit = Some((i, app.chat.edit_draft.clone()));
    }
    ui.add_space(8.0);
}

fn draw_user_images(app: &mut App, ui: &mut egui::Ui, turn_idx: usize) {
    let lone = app.chat.turns[turn_idx].images.len() == 1;
    let mut open_lightbox: Option<(usize, usize)> = None;
    let ctx = ui.ctx().clone();
    let avail = ui.available_width();
    ui.allocate_ui_with_layout(
        Vec2::new(avail, 0.0),
        Layout::right_to_left(Align::Min),
        |ui| {
            let count = app.chat.turns[turn_idx].images.len();
            for j in (0..count).rev() {
                let att = &mut app.chat.turns[turn_idx].images[j];
                let tex = super::composer::ensure_texture(
                    &ctx,
                    &mut att.texture,
                    &att.mime,
                    &att.data_base64,
                    turn_idx * 1000 + j,
                );
                if let Some(handle) = tex {
                    let size = history_image_size(handle.size_vec2(), lone);
                    let resp = ui
                        .add(egui::Image::new((handle.id(), size)).sense(Sense::click()))
                        .on_hover_cursor(egui::CursorIcon::ZoomIn);
                    if resp.clicked() {
                        // The thumbnail is a way in, not the picture. dsh
                        // opens the original; this opens it in place.
                        open_lightbox = Some((turn_idx, j));
                    }
                }
            }
        },
    );
    if let Some(which) = open_lightbox {
        app.lightbox = Some(which);
    }
    ui.add_space(6.0);
}

/// The full-size view (§5.3): the image at `min(viewport − 64, natural)`,
/// closed by Esc, the mask or ×.
///
/// It is a *document-level* surface, not part of the row that opened it —
/// the transcript scrolls, and a viewer that scrolled with it would be a
/// picture that runs away from the reader.
pub fn lightbox(app: &mut App, ctx: &egui::Context) {
    let Some((turn, idx)) = app.lightbox else { return };
    let Some(att) = app.chat.turns.get_mut(turn).and_then(|t| t.images.get_mut(idx)) else {
        app.lightbox = None;
        return;
    };
    // `Attachment` carries no filename — the transcript stores what the
    // model was sent, not what the file was called on disk.
    let name = format!("Attachment {}", idx + 1);
    let mime = att.mime.clone();
    let data = att.data_base64.clone();
    let tex = super::composer::ensure_texture(ctx, &mut att.texture, &mime, &data, turn * 1000 + idx);
    let mut copy = false;
    let out = kit::modal(ctx, egui::Id::new("image_lightbox"), &name, 960.0, true, |ui| {
        match &tex {
            Some(handle) => {
                let natural = handle.size_vec2();
                let room = ui.ctx().screen_rect().size() - Vec2::splat(64.0);
                let scale = (room.x / natural.x).min(room.y / natural.y).min(1.0);
                ui.add(egui::Image::new((handle.id(), natural * scale)));
            }
            None => {
                let t = kit::theme(ui);
                kit::label(
                    ui,
                    kit::txt(
                        "This image could not be decoded.",
                        13.0,
                        Weight::Regular,
                        kit::col(t.alias.label[2]),
                    ),
                );
            }
        }
        ui.add_space(10.0);
        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
            if kit::button(ui, "Copy", kit::Variant::Outline, kit::Size::Sm).clicked() {
                copy = true;
            }
        });
    });
    if copy {
        // The bytes are base64 in the log; the useful thing to put on the
        // clipboard is the data URI, which pastes into a browser.
        ctx.output_mut(|o| o.copied_text = format!("data:{mime};base64,{data}"));
    }
    if out.dismissed || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.lightbox = None;
    }
}

// ---------------------------------------------------------------------------
// Assistant
// ---------------------------------------------------------------------------

fn draw_assistant(
    ui: &mut egui::Ui,
    cache: &mut CommonMarkCache,
    turn_idx: usize,
    text: &str,
    t: &Theme,
    cwd: &std::path::Path,
) {
    // egui_commonmark resolves `**bold**`, headings and bullets through
    // `strong_text_color()` (which reads `widgets.active.fg_stroke.color`) —
    // point it at the primary label so strong glyphs stay legible.
    ui.scope(|ui| {
        let ink = kit::col(t.alias.label[0]);
        let v = &mut ui.style_mut().visuals.widgets;
        v.active.fg_stroke.color = ink;
        v.noninteractive.fg_stroke.color = ink;
        ui.style_mut().visuals.extreme_bg_color = kit::col(t.alias.code_block);
        // Math and wide tables are split out before the viewer sees them
        // (§3.8): the parser would mangle TeX, and a wide table left to the
        // viewer widens the whole conversation column instead of itself.
        for (i, block) in super::md_blocks::split(text).into_iter().enumerate() {
            let id = format!("assistant_md_{turn_idx}_{i}");
            match block {
                super::md_blocks::Block::Prose(body) => {
                    let body = super::md_blocks::inline_math_to_code(&body);
                    let body = super::md_blocks::rewrite_image_uris(&body, cwd);
                    let body = super::md_blocks::linkify_file_paths(&body, cwd);
                    CommonMarkViewer::new(id).show(ui, cache, &body);
                }
                super::md_blocks::Block::Math(src) => {
                    // No KaTeX for egui: the source, legible and copyable,
                    // beats a formula the parser has eaten.
                    kit::code_block(ui, "math", &src);
                }
                super::md_blocks::Block::Table(src) => {
                    egui::ScrollArea::horizontal()
                        .id_source(("md_table", &id))
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            CommonMarkViewer::new(id.clone()).show(ui, cache, &src);
                        });
                }
            }
        }
    });
}

/// How large an image is drawn in the transcript (§5.3).
///
/// A lone image is the message's subject, so it gets **240 px on its long
/// edge** — but it is never *upscaled*: a 32 px icon blown up to 240 is a
/// blurry lie about what was attached. Several images are a set rather than
/// a subject, so each becomes a 64 px square. An aspect beyond dsh's
/// `[0.25, 4]` is clamped, which is what keeps a 20 000 × 40 panorama from
/// becoming a hairline that cannot be clicked.
fn history_image_size(natural: Vec2, lone: bool) -> Vec2 {
    if natural.x <= 0.0 || natural.y <= 0.0 {
        return Vec2::splat(if lone { 240.0 } else { 64.0 });
    }
    if !lone {
        return Vec2::splat(64.0);
    }
    let aspect = (natural.x / natural.y).clamp(0.25, 4.0);
    let (w, h) = if aspect >= 1.0 {
        (240.0, 240.0 / aspect)
    } else {
        (240.0 * aspect, 240.0)
    };
    // Never bigger than it really is.
    let scale = (natural.x / w).min(natural.y / h).min(1.0);
    Vec2::new(w * scale, h * scale)
}

/// Is `i` the last non-marker row — the one the loop is working on?
///
/// Only that row carries the status line. An older unfinished row (a bubble
/// the backend opened its own turn for instead of claiming) would otherwise
/// shimmer "Working…" for the rest of the session.
fn is_tail_turn(app: &App, i: usize) -> bool {
    app.chat.turns.iter().rposition(|t| t.notice.is_none()) == Some(i)
}

/// The live tail of a running turn: dsh's shimmering status line, plus a
/// mono clock once the turn has been going for 15 s.
fn turn_status(app: &mut App, ui: &mut egui::Ui, i: usize, t: &Theme) {
    let chips = &app.chat.turns[i].tool_chips;
    let active = chips.iter().filter(|c| !c.finished).max_by_key(|c| c.depth);
    let text = if app.chat.interrupt_requested {
        "Stopping…".to_string()
    } else {
        match active {
            Some(c) if c.depth > 0 => format!("Sub-agent · {}", c.name),
            Some(c) => format!("Running · {}", super::tool_row::title_of(&c.name).to_lowercase()),
            None => "Working…".to_string(),
        }
    };
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if app.reduce_motion {
            kit::label(
                ui,
                kit::txt(&text, 14.0, Weight::Medium, kit::col(t.alias.business)),
            );
        } else {
            kit::shimmer_text(ui, &text, 14.0);
        }
        let elapsed = app.gen_speed.elapsed_secs;
        if elapsed >= CLOCK_AFTER_SECS {
            ui.add_space(8.0);
            let secs = elapsed as u32;
            let clock = if secs >= 60 {
                format!("{}m {}s", secs / 60, secs % 60)
            } else {
                format!("{secs}s")
            };
            kit::label(ui, kit::mono(clock, 12.0, kit::col(t.alias.label[3])));
        }
    });
}

/// `Stopped` tag: pad 0 6, r=6, hover fill, `label[2]`, fixed 11/18.
fn stopped_tag(ui: &mut egui::Ui, t: &Theme) {
    ui.add_space(4.0);
    let font = kit::font(11.0, Weight::Regular);
    let galley = ui.fonts(|f| f.layout_no_wrap("Stopped".into(), font.clone(), egui::Color32::WHITE));
    let (rect, _) =
        ui.allocate_exact_size(galley.size() + Vec2::new(12.0, 4.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, Rounding::same(RADIUS_ROW), kit::cola(t.alias.hover));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "Stopped",
        font,
        kit::col(t.alias.label[2]),
    );
}

/// `[10px dot] "This turn failed" [message]` — grid 10px 1fr auto (§3.5).
fn turn_error(ui: &mut egui::Ui, t: &Theme) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        kit::state_dot(ui, DotState::Error, 10.0);
        ui.add_space(8.0);
        kit::label(
            ui,
            kit::txt("This turn failed", 14.0, Weight::Semibold, kit::col(t.alias.error)),
        );
        ui.add_space(8.0);
        kit::label(
            ui,
            kit::txt(
                "the request did not complete — see Diagnostics for the error",
                13.0,
                Weight::Regular,
                kit::col(t.alias.label[1]),
            ),
        );
    });
}

fn max_tokens_row(ui: &mut egui::Ui, t: &Theme) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        kit::state_dot(ui, DotState::Warning, 10.0);
        ui.add_space(8.0);
        kit::label(
            ui,
            kit::txt(
                "Output token limit reached",
                14.0,
                Weight::Semibold,
                kit::col(t.alias.warn_label),
            ),
        );
    });
    kit::footnote(
        ui,
        "The reply was cut off; earlier output is preserved in the conversation. \
         Send \"continue\" to let the model resume.",
    );
}

/// The retry chain (§3.5): one `<details>` row per step-level LLM retry the
/// backend performed inside this turn. The newest row of a still-running turn
/// is the *pending* one — it counts down and shimmers; every earlier row is
/// settled and reads "Retried model request".
fn retry_chain(app: &mut App, ui: &mut egui::Ui, i: usize, finished: bool, t: &Theme) {
    let rows = app.chat.turns[i].retries.clone();
    if rows.is_empty() {
        return;
    }
    let last = rows.len() - 1;
    for (n, r) in rows.iter().enumerate() {
        let remaining = (r.delay_ms as f32 / 1000.0) - r.at.elapsed().as_secs_f32();
        let pending = !finished && n == last && remaining > 0.0;
        let (title, secs) = if pending {
            ("Waiting to retry model request", remaining)
        } else {
            ("Retried model request", r.delay_ms as f32 / 1000.0)
        };
        let summary = format!("({}/{}) · {:.1}s", r.attempt, r.max, secs.max(0.0));
        let id = ui.id().with(("retry", i, n));
        let open: bool = ui.ctx().data(|d| d.get_temp(id).unwrap_or(false));
        let out = kit::disclosure_row(
            ui,
            Leading::Icon(Icon::Refresh),
            title,
            &summary,
            open,
            pending,
            None,
        );
        if out.clicked {
            ui.ctx().data_mut(|d| d.insert_temp(id, !open));
        }
        if open {
            ui.horizontal(|ui| {
                ui.add_space(22.0 + t.delta());
                ui.vertical(|ui| {
                    kit::footnote(ui, &format!("Retry delay: {} ms", r.delay_ms));
                    kit::footnote(ui, &format!("Failure reason: {}", r.reason));
                });
            });
        }
        // The countdown is only live while it is counting.
        if pending {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }
    }
}

/// Compact token counts the way dsh does: `12.2K`, `980`, `1.4M`.
fn fmt_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f32 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f32 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn fmt_duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f32 / 1000.0)
    } else {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// Produced-file chips, then copy · branch · usage pill · time pill ·
/// generation speed — revealed on the newest turn always and on hover
/// otherwise (§3.5).
fn turn_tail(app: &mut App, ui: &mut egui::Ui, i: usize, assistant: &str, t: &Theme) {
    if assistant.is_empty() {
        return;
    }
    let newest = i + 1 == app.chat.turns.len();
    let produced = produced_files(&app.chat.turns[i]);
    // Hover is tested against the seat the tail is about to occupy rather
    // than against the rest of the column, so pointing anywhere below an old
    // turn does not reveal its tail.
    let seat = ui.available_rect_before_wrap();
    let tail_h = if produced.is_empty() {
        TAIL_ROW_H
    } else {
        TAIL_ROW_H + CHIP_H + 4.0
    };
    let hovered = ui.rect_contains_pointer(egui::Rect::from_min_size(
        seat.min,
        Vec2::new(seat.width(), tail_h),
    ));
    if !(newest || hovered) {
        // The seat is held at its full height, chips included, so revealing
        // a tail does not shove the rest of the transcript downwards.
        ui.add_space(tail_h);
        return;
    }
    if !produced.is_empty() {
        produced_row(app, ui, &produced, t);
    }
    let row = ui.horizontal(|ui| {
        if kit::icon_button(ui, Icon::Copy, 28.0)
            .on_hover_text("Copy response")
            .clicked()
        {
            ui.output_mut(|o| o.copied_text = assistant.to_owned());
        }
        // Branching forks the session at its last completed turn, which is
        // this turn only while it is the newest finished one. On any earlier
        // turn the button would fork somewhere else than where it sits, so
        // it is not offered at all.
        let branchable = newest && app.chat.turns[i].finished;
        if branchable
            && kit::icon_button(ui, Icon::Branch, 28.0)
                .on_hover_text("Branch into a new conversation")
                .clicked()
        {
            let session_id = app.chat.session_id;
            app.send(UiCommand::SendRequest(protocol::Request::ForkSession {
                session_id,
            }));
            app.send(UiCommand::SendRequest(protocol::Request::ListSessions));
        }
        if let Some(u) = app.chat.turns[i].usage {
            let total = u.prompt.saturating_add(u.completion);
            // A provider that sent no `usage` trailer leaves zeroes; a pill
            // reading "Usage 0" would be a lie, so it simply does not appear.
            if total > 0 {
                let mut detail = format!(
                    "Input {}\nOutput {}\nTotal {}",
                    u.prompt, u.completion, total
                );
                if u.reasoning > 0 {
                    detail.push_str(&format!("\n(+{} chars of reasoning)", u.reasoning));
                }
                kit::pill(ui, &format!("Usage {}", fmt_tokens(total)), false)
                    .on_hover_text(detail);
            }
            if u.duration_ms > 0 {
                let mut detail = format!("Total {}", fmt_duration(u.duration_ms));
                if u.ttft_ms > 0 {
                    detail.push_str(&format!("\nTime to first token {}", fmt_duration(u.ttft_ms)));
                }
                if u.completion > 0 {
                    detail.push_str(&format!(
                        "\n{:.0} tok/s",
                        u.completion as f32 / (u.duration_ms as f32 / 1000.0).max(0.001)
                    ));
                }
                kit::pill(ui, &format!("Ran for {}", fmt_duration(u.duration_ms)), false)
                    .on_hover_text(detail);
            }
        } else if app.gen_speed.completed > 0 && newest {
            // No `TurnUsage` yet (an older backend, or a turn still settling):
            // the live meter is what there is.
            kit::label(
                ui,
                kit::txt(
                    format!(
                        "{} tok · {:.0} tok/s",
                        app.gen_speed.completed, app.gen_speed.tps
                    ),
                    12.0,
                    Weight::Regular,
                    kit::col(t.alias.label[3]),
                ),
            );
        }
    });
    let _ = row;
}

/// Height of the tail's icon row — also the space an unrevealed tail holds
/// open, so hovering one does not shift the column under the pointer.
const TAIL_ROW_H: f32 = 28.0;
/// Height of a produced-file chip (§3.5: r=6 chips).
const CHIP_H: f32 = 22.0;
/// Chips shown before the overflow count takes over.
const MAX_CHIPS: usize = 6;

/// Files this turn wrote, in call order and deduplicated.
///
/// The turn's own successful `write-file` / `edit-file` calls are the record
/// — the same rows the transcript already renders — so nothing has to be
/// collected backend-side for the chips to be true. A failed call produced
/// nothing and is left out.
fn produced_files(turn: &crate::app::Turn) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for chip in &turn.tool_chips {
        if !(chip.finished && chip.ok) {
            continue;
        }
        if !matches!(chip.name.as_str(), "write-file" | "edit-file") {
            continue;
        }
        let path = serde_json::from_str::<serde_json::Value>(&chip.args_json)
            .ok()
            .and_then(|v| {
                v.get("path")
                    .and_then(|p| p.as_str())
                    .map(|p| p.trim().to_owned())
            })
            .unwrap_or_default();
        if path.is_empty() || out.iter().any(|p| p == &path) {
            continue;
        }
        out.push(path);
    }
    out
}

/// `Produced [chip] [chip] + 2 files`. A chip shows the file name and reveals
/// the file in the OS browser when clicked; the full path is its tooltip.
fn produced_row(app: &mut App, ui: &mut egui::Ui, files: &[String], t: &Theme) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        kit::label(
            ui,
            kit::txt("Produced", 12.0, Weight::Regular, kit::col(t.alias.label[3])),
        );
        for path in files.iter().take(MAX_CHIPS) {
            if file_chip(ui, path, t).on_hover_text(path).clicked() {
                let full = sica_core::paths::working_dir().join(path);
                if let Err(e) = crate::ui::settings::reveal_path(&full) {
                    app.push_log(LogKind::Warn, format!("could not open {path}: {e}"));
                }
            }
        }
        if files.len() > MAX_CHIPS {
            let extra = files.len() - MAX_CHIPS;
            kit::label(
                ui,
                kit::txt(
                    format!("+ {extra} file{}", if extra == 1 { "" } else { "s" }),
                    12.0,
                    Weight::Regular,
                    kit::col(t.alias.label[3]),
                ),
            );
        }
    });
    ui.add_space(4.0);
}

/// One r=6 chip: the file's own name, on `bg_layer[1]` behind a hairline.
fn file_chip(ui: &mut egui::Ui, path: &str, t: &Theme) -> egui::Response {
    let name = super::at_menu::split_path(path).1;
    let font = kit::font(12.0, Weight::Regular);
    let galley = ui.fonts(|f| f.layout_no_wrap(name.to_owned(), font.clone(), kit::col(t.alias.label[1])));
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(galley.size().x + 16.0, CHIP_H),
        Sense::click(),
    );
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect(
            rect,
            Rounding::same(6.0),
            if resp.hovered() {
                kit::cola(t.alias.hover)
            } else {
                kit::col(t.alias.bg_layer[1])
            },
            Stroke::new(HAIRLINE, kit::Level::L1.color(t)),
        );
        painter.galley(
            egui::pos2(rect.min.x + 8.0, rect.center().y - galley.size().y / 2.0),
            galley,
            kit::col(t.alias.label[1]),
        );
    }
    resp
}

/// Compact display: one full-width 33 px button with a bottom rule
/// summarising everything folded behind it.
fn process_fold(ui: &mut egui::Ui, app: &App, i: usize, folded: bool) -> bool {
    let t = app.theme;
    let turn = &app.chat.turns[i];
    let tools = turn.tool_chips.len();
    let text = if tools == 0 {
        "Thought for a while".to_string()
    } else if turn.reasoning.is_empty() {
        format!("{tools} tool call{}", if tools == 1 { "" } else { "s" })
    } else {
        format!(
            "{tools} tool call{} · reasoning",
            if tools == 1 { "" } else { "s" }
        )
    };
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 33.0), Sense::click());
    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(rect, Rounding::same(RADIUS_ROW), kit::cola(t.alias.hover));
    }
    painter.text(
        egui::pos2(rect.min.x + 22.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        kit::font(t.content_secondary_px(), Weight::Medium),
        kit::col(t.alias.label[1]),
    );
    crate::ui::icons::paint(
        painter,
        Rect::from_center_size(
            egui::pos2(rect.min.x + 8.0, rect.center().y),
            Vec2::splat(12.0),
        ),
        if folded { Icon::ChevronRight } else { Icon::ChevronDown },
        kit::col(t.alias.label[2]),
    );
    painter.hline(
        rect.x_range(),
        rect.max.y,
        Stroke::new(sica_core::theme::tokens::HAIRLINE, kit::Level::L2.color(&t)),
    );
    resp.clicked()
}

fn reasoning_body(ui: &mut egui::Ui, text: &str, t: &Theme) {
    ui.horizontal(|ui| {
        ui.add_space(22.0 + t.delta());
        ui.vertical(|ui| {
            ui.set_max_width((ui.available_width() - 8.0).max(120.0));
            ui.add(egui::Label::new(kit::txt(
                text,
                t.content_secondary_px(),
                Weight::Regular,
                kit::col(t.alias.label[2]),
            )));
        });
    });
    ui.add_space(4.0);
}

// ---------------------------------------------------------------------------
// Markers
// ---------------------------------------------------------------------------

/// Compaction / injection / steer rows. A compaction marker is quiet
/// (`label_dimmed`, no icon tint) and does **not** hide the rows it shadows.
fn draw_marker(app: &mut App, ui: &mut egui::Ui, i: usize, n: &Notice) {
    let t = app.theme;
    let (icon, title) = match (n.kind, n.ok) {
        // A failed compaction is not a quiet marker: the history it was
        // meant to fold is about to be trimmed instead.
        (NoticeKind::Compaction, false) => (Icon::Warning, "Context compaction failed"),
        (NoticeKind::Compaction, true) => (Icon::Compact, "Context compacted"),
        (NoticeKind::Injection, _) => (Icon::Inject, "Context injection"),
        (NoticeKind::Steer, _) => (Icon::ArrowUp, "Steered"),
    };
    ui.add_space(4.0);
    let out = kit::disclosure_row(
        ui,
        Leading::Icon(icon),
        title,
        &kit::one_line(&n.label, 160),
        n.open,
        false,
        None,
    );
    if out.clicked {
        if let Some(notice) = app.chat.turns[i].notice.as_mut() {
            notice.open = !notice.open;
        }
    }
    if n.open && !n.detail.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(22.0);
            ui.vertical(|ui| {
                ui.set_max_width((ui.available_width() - 8.0).max(120.0));
                egui::Frame::none()
                    .fill(kit::col(t.alias.code_block))
                    .rounding(Rounding::same(sica_core::theme::tokens::RADIUS_INPUT))
                    .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .id_source(ui.id().with(("marker", i)))
                            .max_height(141.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(kit::mono(
                                        &n.detail,
                                        11.0,
                                        kit::col(t.alias.label[1]),
                                    ))
                                    .wrap(),
                                );
                            });
                    });
            });
        });
    }
    ui.add_space(6.0);
}

// ---------------------------------------------------------------------------
// Viewport
// ---------------------------------------------------------------------------

fn visible_viewport(ui: &egui::Ui, output: &egui::scroll_area::ScrollAreaOutput<()>) -> Rect {
    output.inner_rect.intersect(ui.clip_rect())
}

/// Post-layout input pass: auto-follow pause, keyboard scrolling, middle-click
/// pan, Ctrl+A/Ctrl+C message copy, and the offset commit.
fn transcript_input(
    app: &mut App,
    ui: &mut egui::Ui,
    output: &egui::scroll_area::ScrollAreaOutput<()>,
    assistant_rects: &[(usize, Rect)],
    force_scroll: bool,
) {
    let ctx = ui.ctx().clone();
    let rect = visible_viewport(ui, output);
    let hit_rect = Rect::from_min_max(rect.min, egui::pos2(rect.max.x + 14.0, rect.max.y));
    let pointer = ctx.input(|i| i.pointer.hover_pos());
    let pointer_over = pointer.is_some_and(|p| hit_rect.contains(p));
    let no_focus = ctx.memory(|m| m.focused().is_none());

    let clicked_transcript = pointer_over && ctx.input(|i| i.pointer.primary_pressed());
    let wheeled_up = pointer_over && ctx.input(|i| i.raw_scroll_delta.y > 0.0);
    if !app.chat.turns.is_empty() && (clicked_transcript || wheeled_up) {
        app.chat.autoscroll_paused = true;
    }

    let mut delta = 0.0f32;
    if no_focus {
        let line = 48.0;
        let page = (rect.height() - 24.0).max(48.0);
        ctx.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                delta -= line;
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                delta += line;
            }
            if i.consume_key(Modifiers::NONE, Key::PageUp) {
                delta -= page;
            }
            if i.consume_key(Modifiers::NONE, Key::PageDown) {
                delta += page;
            }
        });
    }

    if ctx.input(|i| i.pointer.button_pressed(PointerButton::Middle)) {
        app.chat.middle_scroll_origin = match app.chat.middle_scroll_origin {
            Some(_) => None,
            None if pointer_over => pointer,
            None => None,
        };
    }
    if ctx.input(|i| {
        i.pointer.primary_pressed() || i.pointer.secondary_pressed() || i.key_pressed(Key::Escape)
    }) {
        app.chat.middle_scroll_origin = None;
    }
    if let Some(origin) = app.chat.middle_scroll_origin {
        if let Some(pos) = pointer {
            let dy = pos.y - origin.y;
            const DEAD_ZONE: f32 = 8.0;
            if dy.abs() > DEAD_ZONE {
                let dt = ctx.input(|i| i.stable_dt).min(0.1);
                delta += (dy - DEAD_ZONE * dy.signum()) * 6.0 * dt;
            }
        }
        draw_pan_anchor(&ctx, origin, &app.theme);
        ctx.request_repaint();
    }

    let max_offset = (output.content_size.y - rect.height()).max(0.0);
    let mut state = output.state;
    let mut dirty = state.offset.x != 0.0;
    state.offset.x = 0.0;
    if delta != 0.0 {
        if delta < 0.0 {
            app.chat.autoscroll_paused = true;
        }
        state.offset.y = (state.offset.y + delta).clamp(0.0, max_offset);
        dirty = true;
    } else if force_scroll && (max_offset - state.offset.y).abs() > BOTTOM_EPS {
        state.offset.y = max_offset;
        dirty = true;
    }
    if dirty {
        state.store(&ctx, output.id);
        ctx.request_repaint();
    }

    if clicked_transcript || ctx.input(|i| i.key_pressed(Key::Escape)) {
        app.chat.selected_turn = None;
    }
    if no_focus && ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::A)) {
        app.chat.selected_turn = pointer
            .and_then(|pos| {
                assistant_rects
                    .iter()
                    .find(|(_, r)| r.expand2(egui::vec2(6.0, 4.0)).contains(pos))
                    .map(|(i, _)| *i)
            })
            .or_else(|| assistant_rects.last().map(|(i, _)| *i));
    }
    if let Some(sel) = app.chat.selected_turn {
        let label_selection = egui::text_selection::LabelSelectionState::load(&ctx).has_selection();
        if no_focus
            && !label_selection
            && ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::C))
        {
            if let Some(turn) = app.chat.turns.get(sel) {
                ctx.output_mut(|o| o.copied_text = turn.assistant.clone());
            }
        }
    }
}

/// 34 px circle in a zero-height slot 16 px above the composer.
fn back_to_bottom(app: &mut App, ui: &mut egui::Ui, viewport: Rect) {
    if !app.chat.autoscroll_paused {
        return;
    }
    let t = app.theme;
    let ctx = ui.ctx().clone();
    egui::Area::new(egui::Id::new("back_to_bottom"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(
            viewport.center().x - 17.0,
            viewport.max.y - 34.0 - 16.0,
        ))
        .show(&ctx, |ui| {
            let (rect, resp) = ui.allocate_exact_size(Vec2::splat(34.0), Sense::click());
            let painter = ui.painter();
            painter.circle(
                rect.center(),
                17.0,
                kit::col(t.alias.floating_fill),
                Stroke::new(sica_core::theme::tokens::HAIRLINE, kit::Level::L3.color(&t)),
            );
            crate::ui::icons::paint(
                painter,
                Rect::from_center_size(rect.center(), Vec2::splat(16.0)),
                Icon::ArrowDown,
                kit::col(t.alias.label[1]),
            );
            if resp.on_hover_text("Back to bottom").clicked() {
                app.chat.autoscroll_paused = false;
                app.chat.scroll_to_bottom = true;
            }
        });
}

fn draw_pan_anchor(ctx: &egui::Context, origin: Pos2, t: &Theme) {
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("chat_pan_anchor"),
    ));
    painter.circle(
        origin,
        11.0,
        kit::col(t.alias.bg_layer[1]),
        Stroke::new(1.0, kit::col(t.alias.label[3])),
    );
    let ink = kit::col(t.alias.label[0]);
    for dir in [-1.0f32, 1.0] {
        let tip = Pos2::new(origin.x, origin.y + dir * 7.0);
        let base_y = origin.y + dir * 3.0;
        painter.add(Shape::convex_polygon(
            vec![
                tip,
                Pos2::new(origin.x - 3.5, base_y),
                Pos2::new(origin.x + 3.5, base_y),
            ],
            ink,
            Stroke::NONE,
        ));
    }
    painter.circle_filled(origin, 1.5, ink);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lone image is the message's subject and gets 240 px on its long
    /// edge — but is never upscaled, because a 32 px icon blown up to 240 is
    /// a blurry lie about what was attached.
    #[test]
    fn a_lone_image_fills_240_on_its_long_edge_but_is_never_upscaled() {
        let wide = history_image_size(Vec2::new(1200.0, 600.0), true);
        assert_eq!(wide, Vec2::new(240.0, 120.0));
        let tall = history_image_size(Vec2::new(600.0, 1200.0), true);
        assert_eq!(tall, Vec2::new(120.0, 240.0));

        let tiny = history_image_size(Vec2::new(32.0, 32.0), true);
        assert_eq!(tiny, Vec2::new(32.0, 32.0), "a small image stays small");
    }

    /// An extreme aspect is clamped, which is what stops a panorama from
    /// becoming a hairline nobody can click.
    #[test]
    fn an_extreme_aspect_is_clamped() {
        let panorama = history_image_size(Vec2::new(20_000.0, 40.0), true);
        assert_eq!(panorama.x / panorama.y, 4.0);
        let column = history_image_size(Vec2::new(40.0, 20_000.0), true);
        assert!((column.y / column.x - 4.0).abs() < 0.001);
    }

    /// Several images are a set rather than a subject.
    #[test]
    fn images_in_a_group_are_uniform_squares() {
        for natural in [Vec2::new(1200.0, 600.0), Vec2::new(20.0, 900.0)] {
            assert_eq!(history_image_size(natural, false), Vec2::splat(64.0));
        }
        // A texture that decoded to nothing still takes room, so the row
        // does not silently collapse.
        assert_eq!(history_image_size(Vec2::ZERO, true), Vec2::splat(240.0));
        assert_eq!(history_image_size(Vec2::ZERO, false), Vec2::splat(64.0));
    }
}
