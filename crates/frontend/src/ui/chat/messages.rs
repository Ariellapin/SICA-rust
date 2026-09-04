//! Scrollable list of turns. Editorial form — no bubbles. User messages
//! right-align in a subtle sunk-surface rect; assistant messages run full-
//! width below an "ASSISTANT" caps label and a hairline rule; reasoning is
//! inset 24px with a left-edge info-blue hairline and italic serif body.

use egui::{
    Align, Color32, Key, Layout, Modifiers, PointerButton, Pos2, Rect, RichText, Rounding,
    Sense, Shape, Stroke, Vec2,
};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

use sica_core::theme::Palette;

use crate::app::{rgb, App, Notice, ToolChip};
use crate::ui::widgets::{
    blade_mark, caps_button, caps_job, caps_label, display_text, hairline, working_sweep,
};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    // The composer sits in its own bottom panel, so everything still
    // available here belongs to the transcript.
    let height = ui.available_height();
    // Follow-the-stream only while the user hasn't taken the viewport
    // (clicked into the transcript / scrolled up). The flag is still drained
    // every frame so a stale snap doesn't fire when auto-follow resumes.
    let force_scroll =
        std::mem::take(&mut app.chat.scroll_to_bottom) && !app.chat.autoscroll_paused;
    // An interrupt has been sent but the backend hasn't confirmed the turn is
    // over yet — the strip says so instead of implying work is progressing.
    let stopping = app.chat.interrupt_requested;
    // Screen rect of each rendered assistant body, for Ctrl+A targeting.
    let mut assistant_rects: Vec<(usize, Rect)> = Vec::new();
    let selected = app.chat.selected_turn;
    let output = egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .stick_to_bottom(!app.chat.autoscroll_paused)
        // Disable drag-to-scroll so a click-drag selects text instead of
        // panning the viewport — otherwise message text can't be highlighted.
        // Wheel and scrollbar scrolling are unaffected.
        .drag_to_scroll(false)
        // We drive the bottom-snap by writing the offset ourselves (see
        // `transcript_input`). Animated targets would fight that write, and
        // an animated snap rubber-bands on every streamed delta anyway.
        .animated(false)
        .max_height(height.max(120.0))
        .show(ui, |ui| {
            if app.chat.turns.is_empty() {
                draw_empty(ui, &palette);
                return;
            }
            for i in 0..app.chat.turns.len() {
                // Marker entries (auto-compaction records) carry no message
                // body — draw the strip and move on.
                if let Some(notice) = app.chat.turns[i].notice.clone() {
                    draw_notice(ui, &notice, &palette);
                    continue;
                }
                let (user, assistant, reasoning_text, finished, collapsed, errored) = {
                    let t = &app.chat.turns[i];
                    (
                        t.user.clone(),
                        t.assistant.clone(),
                        t.reasoning.clone(),
                        t.finished,
                        t.reasoning_collapsed,
                        t.finish_reason.as_deref().is_some_and(|r| r.starts_with("error")),
                    )
                };
                let has_images = !app.chat.turns[i].images.is_empty();
                if !user.is_empty() || has_images {
                    draw_user(ui, &user, &palette);
                }
                // A message the backend queued behind a running turn: say
                // so, or an empty reply that has not started yet reads as a
                // stalled stream.
                if app.chat.turns[i].queued {
                    draw_queued(ui, &palette);
                }
                if has_images {
                    draw_user_images(app, ui, i);
                }
                if !reasoning_text.is_empty() {
                    if finished && collapsed {
                        if reasoning_chip_collapsed(ui, &palette).clicked() {
                            app.chat.turns[i].reasoning_collapsed = false;
                        }
                    } else {
                        if reasoning_header(ui, finished, &palette).clicked() && finished {
                            app.chat.turns[i].reasoning_collapsed = true;
                        }
                        draw_reasoning(ui, &reasoning_text, &palette);
                    }
                }
                // No placeholder for an empty in-flight reply: the activity
                // strip below already says work is happening, and says what.
                if !assistant.is_empty() {
                    let body = ui
                        .scope(|ui| {
                            draw_assistant(ui, &mut app.md_cache, i, &assistant, &palette);
                        })
                        .response
                        .rect;
                    assistant_rects.push((i, body));
                    // Ctrl+A "selection" — a translucent wash over the whole
                    // message so the selected state reads like a text
                    // highlight (Ctrl+C then copies the message source).
                    if selected == Some(i) {
                        ui.painter().rect_filled(
                            body.expand2(egui::vec2(6.0, 4.0)),
                            2.0,
                            rgb(palette.accent).linear_multiply(0.14),
                        );
                    }
                }
                let chips = app.chat.turns[i].tool_chips.clone();
                if !chips.is_empty() {
                    super::tool_chips::draw(ui, &chips, &palette);
                }
                if !finished {
                    draw_working(ui, &chips, stopping, &palette);
                } else if errored {
                    // The request itself failed (after the backend's retries):
                    // say so where the reply would have been, so an empty
                    // bubble never reads as "the model chose silence". The
                    // log panel carries the exact error.
                    ui.add_space(6.0);
                    caps_label(ui, "Request failed · see log", rgb(palette.danger));
                }
                ui.add_space(20.0);
            }
        });
    transcript_input(app, ui, &output, &assistant_rects, force_scroll);
    // The pill is pointless while everything already fits on screen.
    if output.content_size.y > output.inner_rect.height() {
        resume_button(app, ui, visible_viewport(ui, &output));
    }
}

/// The part of the transcript viewport actually on screen.
///
/// With horizontal scrolling off and `auto_shrink` off, egui grows the scroll
/// area's `inner_rect` to whatever width the content demanded, so a single
/// unwrappable line (a long path in a tool chip, say) can hand us a rect far
/// wider than the window. Anything doing hit-testing or positioning has to
/// clamp to the visible clip rect first, or it lands off-screen.
fn visible_viewport(ui: &egui::Ui, output: &egui::scroll_area::ScrollAreaOutput<()>) -> Rect {
    output.inner_rect.intersect(ui.clip_rect())
}

/// Live activity strip under the turn that is still running: the animated
/// sweep plus a caps line naming what is currently executing. The innermost
/// unfinished tool call is the one actually doing work, and a call nested
/// below the main agent (`depth > 0`) is a sub-agent — so it is named as one.
fn draw_working(ui: &mut egui::Ui, chips: &[ToolChip], stopping: bool, p: &Palette) {
    let active = chips
        .iter()
        .filter(|c| !c.finished)
        .max_by_key(|c| c.depth);
    let (label, tint) = if stopping {
        ("Stopping…".to_string(), rgb(p.caution))
    } else {
        match active {
            Some(c) if c.depth > 0 => {
                (format!("Sub-agent · {}", c.name), rgb(p.info))
            }
            Some(c) => (format!("Running · {}", c.name), rgb(p.accent)),
            None => ("Thinking".to_string(), rgb(p.accent)),
        }
    };
    ui.add_space(6.0);
    let resp = ui.horizontal(|ui| {
        working_sweep(ui, p, 13.0);
        ui.add_space(6.0);
        caps_label(ui, &label, tint);
    });
    // Hovering the strip explains what the sub-agent was asked to produce —
    // the same expectation text the chip carries.
    if let Some(c) = active {
        if !c.expectation.is_empty() {
            resp.response
                .on_hover_text(format!("expect: {}", c.expectation));
        }
    }
}

// ---------- markers ----------

/// Out-of-band transcript marker: a tracked-caps line flanked by tinted rules,
/// info-blue when the operation succeeded and danger when it didn't. Hovering
/// reveals `detail` — for a compaction that is the summary the model wrote, so
/// the user can read exactly what replaced their history.
fn draw_notice(ui: &mut egui::Ui, n: &Notice, p: &Palette) {
    let tint = if n.ok { rgb(p.info) } else { rgb(p.danger) };
    ui.add_space(10.0);
    let row = ui.horizontal(|ui| {
        notice_rule(ui, 20.0, tint);
        ui.add_space(6.0);
        ui.add(egui::Label::new(caps_job(&n.label, tint, 9.0)).selectable(false));
        ui.add_space(6.0);
        // Fill whatever is left; `available_width` can be zero on a narrow
        // window, and `allocate_exact_size` dislikes negative extents.
        notice_rule(ui, (ui.available_width() - 4.0).max(0.0), tint);
    });
    if !n.detail.is_empty() {
        let detail = n.detail.clone();
        let muted = rgb(p.muted);
        row.response.on_hover_ui(move |ui| {
            ui.set_max_width(460.0);
            ui.label(RichText::new(&detail).color(muted));
        });
    }
    ui.add_space(14.0);
}

/// Half-strength horizontal rule used either side of a marker label.
fn notice_rule(ui: &mut egui::Ui, width: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 1.0), Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        Stroke::new(1.0, color.linear_multiply(0.45)),
    );
}

// ---------- viewport input: pause/resume, keyboard + middle-click scroll ----------

/// Post-layout input pass over the transcript viewport. Handles:
///   * pausing auto-follow when the user clicks into the transcript or
///     scrolls up (mirrored by the floating resume pill);
///   * ArrowUp/ArrowDown line-scroll and PgUp/PgDn paging when no widget
///     has keyboard focus (so the composer keeps its own key handling);
///   * Windows-style middle-click pan: a middle click drops an anchor and
///     vertical pointer displacement scrolls until any other click / Esc;
///   * Ctrl+A selecting the hovered (else last) assistant message and
///     Ctrl+C copying the selected one;
///   * committing the viewport offset — the keyboard/pan delta, the hard
///     bottom-snap when new content arrived, and the horizontal pin.
fn transcript_input(
    app: &mut App,
    ui: &mut egui::Ui,
    output: &egui::scroll_area::ScrollAreaOutput<()>,
    assistant_rects: &[(usize, Rect)],
    force_scroll: bool,
) {
    let ctx = ui.ctx().clone();
    let rect = visible_viewport(ui, output);
    // Include the scrollbar gutter so grabbing the bar also counts as the
    // user taking control of the viewport.
    let hit_rect = Rect::from_min_max(rect.min, egui::pos2(rect.max.x + 14.0, rect.max.y));
    let pointer = ctx.input(|i| i.pointer.hover_pos());
    let pointer_over = pointer.is_some_and(|p| hit_rect.contains(p));
    let no_focus = ctx.memory(|m| m.focused().is_none());

    // --- pause auto-follow when the user takes the viewport ---
    let clicked_transcript = pointer_over && ctx.input(|i| i.pointer.primary_pressed());
    let wheeled_up = pointer_over && ctx.input(|i| i.raw_scroll_delta.y > 0.0);
    if !app.chat.turns.is_empty() && (clicked_transcript || wheeled_up) {
        app.chat.autoscroll_paused = true;
    }

    // --- keyboard scrolling; only when no widget owns the keyboard ---
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

    // --- middle-click pan: click toggles an anchor, displacement scrolls ---
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
        draw_pan_anchor(&ctx, origin, &app.palette);
        ctx.request_repaint();
    }

    // --- commit the viewport offset ---
    // Every scroll this view performs is vertical. `offset.x` is pinned to
    // zero so a horizontal scroll target leaking in from a nested widget can
    // never shift the transcript sideways and cut the start off every line.
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
    } else if force_scroll {
        // Hard bottom-snap on new content. Covers the cases egui's
        // `stick_to_bottom` heuristic misses — first render after a session
        // switch, or streaming starting while the user sat mid-transcript.
        // A keyboard/pan delta this frame wins: the user is steering.
        if state.offset.y != max_offset {
            state.offset.y = max_offset;
            dirty = true;
        }
    }
    if dirty {
        state.store(&ctx, output.id);
        ctx.request_repaint();
    }

    // --- Ctrl+A selects one output, Ctrl+C copies it, click / Esc clears ---
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
        // Leave Ctrl+C to egui when a drag-selection exists in some label —
        // that copy should win over the whole-message copy.
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

/// Floating "resume auto-scroll" pill, shown while auto-follow is paused.
/// Sits centred just above the composer, over the transcript.
fn resume_button(app: &mut App, ui: &mut egui::Ui, viewport: Rect) {
    if !app.chat.autoscroll_paused {
        return;
    }
    let p = app.palette;
    let ctx = ui.ctx().clone();
    egui::Area::new(egui::Id::new("chat_autoscroll_resume"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(viewport.center().x - 48.0, viewport.max.y - 36.0))
        .show(&ctx, |ui| {
            let resp = egui::Frame::none()
                .fill(rgb(p.accent))
                .rounding(Rounding::same(12.0))
                .inner_margin(egui::Margin::symmetric(12.0, 5.0))
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(caps_job("↓ Follow", rgb(p.page_bg), 11.0))
                            .selectable(false),
                    );
                })
                .response
                .interact(Sense::click());
            if resp.on_hover_text("Resume auto-scroll").clicked() {
                app.chat.autoscroll_paused = false;
                app.chat.scroll_to_bottom = true;
            }
        });
}

/// Windows-style pan anchor: a circle with up/down arrows at the middle-click
/// origin, painted on the foreground layer so it rides above the transcript.
fn draw_pan_anchor(ctx: &egui::Context, origin: Pos2, p: &Palette) {
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("chat_pan_anchor"),
    ));
    painter.circle(origin, 11.0, rgb(p.surface), Stroke::new(1.0, rgb(p.muted)));
    let ink = rgb(p.ink);
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

// ---------- empty state ----------

fn draw_empty(ui: &mut egui::Ui, p: &Palette) {
    let avail = ui.available_size();
    ui.allocate_ui_with_layout(
        avail,
        Layout::centered_and_justified(egui::Direction::TopDown),
        |ui| {
            ui.vertical_centered(|ui| {
                let watermark_size = Vec2::new(180.0, 90.0);
                let (rect, _) = ui.allocate_exact_size(watermark_size, Sense::hover());
                // Watermark — accent at low alpha.
                let mark = rgb(p.accent).linear_multiply(0.18);
                blade_mark(&ui.painter(), rect, mark);
                ui.add_space(12.0);
                ui.label(display_text("Begin.", 24.0).color(rgb(p.muted)));
            });
        },
    );
}

// ---------- user ----------

/// Right-aligned "queued" caption under a message the backend has accepted
/// but not started — it runs as its own turn once the current one ends.
fn draw_queued(ui: &mut egui::Ui, p: &Palette) {
    let avail = ui.available_width();
    ui.allocate_ui_with_layout(
        Vec2::new(avail, 0.0),
        Layout::right_to_left(Align::Min),
        |ui| {
            ui.add(egui::Label::new(
                RichText::new("queued — runs when the current turn ends")
                    .size(11.0)
                    .color(rgb(p.muted)),
            ));
        },
    );
}

fn draw_user(ui: &mut egui::Ui, text: &str, p: &Palette) {
    let avail = ui.available_width();
    let max_w = (avail * 0.78).max(160.0);
    ui.allocate_ui_with_layout(
        Vec2::new(avail, 0.0),
        Layout::right_to_left(Align::Min),
        |ui| {
            egui::Frame::none()
                .fill(rgb(p.surface_sunk))
                .rounding(Rounding::same(2.0))
                .inner_margin(egui::Margin::symmetric(14.0, 10.0))
                .show(ui, |ui| {
                    ui.set_max_width(max_w);
                    // Labels default to "extend" (no wrap) inside a horizontal
                    // layout like `right_to_left`, so a long message would run
                    // off-screen to the left. Force wrapping at `max_w`.
                    ui.add(
                        egui::Label::new(RichText::new(text).color(rgb(p.ink))).wrap(),
                    );
                });
        },
    );
    ui.add_space(8.0);
}

/// Thumbnail strip rendered under a user message that had image attachments.
/// Right-aligned to match `draw_user`. Lazy texture upload — first frame
/// after a session load is the one that pays for image decoding.
fn draw_user_images(app: &mut App, ui: &mut egui::Ui, turn_idx: usize) {
    const HISTORY_THUMB: f32 = 96.0;
    let ctx = ui.ctx().clone();
    let avail = ui.available_width();
    ui.allocate_ui_with_layout(
        Vec2::new(avail, 0.0),
        Layout::right_to_left(Align::Min),
        |ui| {
            // Reverse order: right_to_left places later children further left,
            // so iterate in normal order and items will read left-to-right.
            let count = app.chat.turns[turn_idx].images.len();
            for j in (0..count).rev() {
                let att = &mut app.chat.turns[turn_idx].images[j];
                let tex = super::input_bar::ensure_texture(
                    &ctx,
                    &mut att.texture,
                    &att.mime,
                    &att.data_base64,
                    turn_idx * 1000 + j,
                );
                if let Some(handle) = tex {
                    let natural = handle.size_vec2();
                    let size = if natural.x <= 0.0 || natural.y <= 0.0 {
                        Vec2::new(HISTORY_THUMB, HISTORY_THUMB)
                    } else {
                        let scale = (HISTORY_THUMB / natural.x).min(HISTORY_THUMB / natural.y);
                        Vec2::new(natural.x * scale, natural.y * scale)
                    };
                    ui.image((handle.id(), size));
                }
            }
        },
    );
    ui.add_space(6.0);
}

// ---------- assistant ----------

fn draw_assistant(
    ui: &mut egui::Ui,
    cache: &mut CommonMarkCache,
    turn_idx: usize,
    text: &str,
    p: &Palette,
) {
    caps_label(ui, "ASSISTANT", rgb(p.muted));
    ui.add_space(2.0);
    hairline(ui, p);
    ui.add_space(8.0);
    // Render as CommonMark so headings, bold/italic, lists and code
    // fences come through. The viewer ID has to be unique per turn so
    // egui_commonmark can keep per-document state straight when the
    // stream re-renders many of these blocks on the same frame.
    //
    // The scoped style swap below is load-bearing: egui_commonmark resolves
    // `**bold**`, headings and list bullets through `strong_text_color()`,
    // which reads `widgets.active.fg_stroke.color`. Our theme paints that
    // with the page background so pressed buttons render inverse text — but
    // that also makes every strong glyph invisible against the page. We
    // override it to ink for the duration of the viewer.
    ui.scope(|ui| {
        let ink = rgb(p.ink);
        let v = &mut ui.style_mut().visuals.widgets;
        v.active.fg_stroke.color = ink;
        v.noninteractive.fg_stroke.color = ink;
        let viewer_id = format!("assistant_md_{turn_idx}");
        CommonMarkViewer::new(viewer_id).show(ui, cache, text);
    });
    // Copy-to-clipboard affordance, right-aligned under the message body.
    // CommonMark-rendered text isn't cleanly selectable, so this is the
    // reliable path to grab the whole response. Hidden while the turn is
    // still an empty "…" placeholder.
    if text != "…" {
        ui.add_space(4.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if caps_button(ui, "Copy", rgb(p.muted))
                .on_hover_text("Copy response")
                .clicked()
            {
                ui.output_mut(|o| o.copied_text = text.to_owned());
            }
        });
    }
}

// ---------- reasoning ----------

fn draw_reasoning(ui: &mut egui::Ui, text: &str, p: &Palette) {
    ui.add_space(6.0);
    let resp = ui.horizontal(|ui| {
        ui.add_space(24.0);
        ui.vertical(|ui| {
            ui.set_max_width(ui.available_width());
            ui.label(display_text(text, 14.0).color(rgb(p.muted)));
        });
    });
    // Paint the inset vertical hairline along the full body height.
    let rect = resp.response.rect;
    ui.painter().vline(
        rect.min.x + 11.0,
        rect.y_range(),
        Stroke::new(1.0, rgb(p.info)),
    );
    ui.add_space(8.0);
}

/// Collapsed-state chip: a single tracked caps label that re-expands the
/// hidden reasoning when clicked.
fn reasoning_chip_collapsed(ui: &mut egui::Ui, p: &Palette) -> egui::Response {
    let resp = caps_button(ui, "+ Reasoning", rgb(p.info));
    resp.on_hover_text("Show reasoning")
}

/// Expanded-state header above the inset reasoning body.
fn reasoning_header(ui: &mut egui::Ui, finished: bool, p: &Palette) -> egui::Response {
    let label = if finished { "− Reasoning" } else { "· Reasoning (live)" };
    let resp = caps_button(ui, label, rgb(p.info));
    if finished {
        resp.on_hover_text("Hide reasoning")
    } else {
        resp
    }
}
