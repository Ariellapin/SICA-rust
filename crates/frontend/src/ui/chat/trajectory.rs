//! The Trajectory view (§10) — the second tab over the conversation column.
//!
//! The transcript shows the *derived surface*: what the model sees, after the
//! fold has shadowed everything a compaction or a rewind took out. This shows
//! the log itself. Every line `sessions/<id>.jsonl` holds gets a row, and the
//! ones the fold dropped are still here, struck through and dimmed — seeing
//! what left the model's view, and what a request actually cost, is the whole
//! reason the view exists.
//!
//! Full-bleed on `bg_layer[0]`, a 32 px toolbar, the timeline strip, then the
//! ledger: a fixed-layout table, `# | kind | text | In | Out | Time`.
//!
//! Deviations from dsh, both deliberate:
//!
//! - **No Think column.** dsh prices reasoning per node; nothing durable here
//!   carries reasoning tokens (`Event::TurnUsage` does, but it is live-only
//!   and never logged). Rather than fill a token column with a different
//!   unit, the reasoning body is shown in the inspector's Result tab.
//! - **Turn headers are not sticky.** egui has no sticky row inside a
//!   `ScrollArea`; they scroll with the ledger and stay legible because they
//!   are full-width and ruled.

use egui::{Align, Align2, Layout, Rect, Sense, Vec2};
use protocol::{EventDump, EventTag};
use sica_core::theme::{tokens, Theme};

use crate::app::{App, TrajectoryState};
use crate::ui::icons::Icon;
use crate::ui::kit::{self, Weight};

/// Ledger geometry, in points. dsh's 12 px type on 30 px rows.
const ROW_H: f32 = 30.0;
const TURN_H: f32 = 44.0;
const COL_SEQ: f32 = 56.0;
const COL_TAG: f32 = 88.0;
const COL_NUM: f32 = 62.0;
const COL_TIME: f32 = 74.0;
const TEXT_SIZE: f32 = 12.0;

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    // A view that has never been asked for is asked for now: opening the tab
    // is the request.
    if app.trajectory.rows.is_empty()
        && !app.trajectory.loading
        && app.trajectory.session_id != app.chat.session_id
    {
        app.load_trajectory(true);
    }

    egui::Frame::none()
        .fill(kit::col(t.alias.bg_layer[0]))
        .inner_margin(egui::Margin::symmetric(20.0, 0.0))
        .show(ui, |ui| {
            toolbar(app, ui);
            kit::hairline(ui, kit::Level::L3);
            timeline(app, ui);
            kit::hairline(ui, kit::Level::L3);
            ledger(app, ui);
        });
}

// ---------------------------------------------------------------------------
// Toolbar
// ---------------------------------------------------------------------------

fn toolbar(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    ui.horizontal(|ui| {
        ui.set_min_height(38.0);

        // Live search. Non-matching rows dim rather than vanish, so the seq
        // numbering and the turn structure stay readable around a hit.
        let field = egui::TextEdit::singleline(&mut app.trajectory.search)
            .hint_text("Search the log")
            .desired_width(200.0)
            .frame(true);
        ui.add(field);
        if !app.trajectory.search.is_empty()
            && kit::icon_button(ui, Icon::Close, 22.0).clicked()
        {
            app.trajectory.search.clear();
        }

        ui.add_space(10.0);
        let all_turns: Vec<u64> = turn_ids(&app.trajectory.rows);
        let all_collapsed =
            !all_turns.is_empty() && all_turns.iter().all(|id| app.trajectory.collapsed.contains(id));
        if kit::pill(ui, if all_collapsed { "Expand turns" } else { "Collapse turns" }, all_collapsed)
            .clicked()
        {
            if all_collapsed {
                app.trajectory.collapsed.clear();
            } else {
                app.trajectory.collapsed.extend(all_turns);
            }
        }
        let dur = app.trajectory.actual_duration;
        if kit::pill(ui, if dur { "Actual duration" } else { "Equal width" }, dur).clicked() {
            app.trajectory.actual_duration = !dur;
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if kit::icon_button(ui, Icon::Refresh, 26.0)
                .on_hover_text("Reload the log")
                .clicked()
            {
                app.load_trajectory(true);
            }
            let shown = app.trajectory.rows.len();
            let total = app.trajectory.total.max(shown as u32);
            kit::label(
                ui,
                kit::txt(
                    format!("{shown} of {total} events"),
                    12.0,
                    Weight::Regular,
                    kit::col(t.alias.label[2]),
                ),
            );
        });
    });
}

// ---------------------------------------------------------------------------
// Timeline strip
// ---------------------------------------------------------------------------

/// `Total {d} · Started {t} · {n} requests`, then one segment per turn.
///
/// dsh drags a range here to filter; a click on a segment scrolls the ledger
/// to that turn instead — the ledger is short enough to scan, and a range
/// filter would hide the request boundaries that give the strip its numbers.
fn timeline(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    let rows = &app.trajectory.rows;
    if rows.is_empty() {
        ui.add_space(8.0);
        return;
    }
    let start = rows.first().map(|r| r.ts).unwrap_or(0);
    let end = rows.last().map(|r| r.ts).unwrap_or(start);
    let requests = rows
        .iter()
        .filter(|r| matches!(r.tag, EventTag::Usage | EventTag::Retry))
        .count();

    egui::Frame::none()
        .inner_margin(egui::Margin::symmetric(0.0, 8.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                for (k, v) in [
                    ("Total", duration(end - start)),
                    ("Started", clock(start)),
                    ("Requests", requests.to_string()),
                ] {
                    kit::label(
                        ui,
                        kit::txt(k, 11.0, Weight::Regular, kit::col(t.alias.label[3])),
                    );
                    kit::label(
                        ui,
                        kit::txt(v, 12.0, Weight::Medium, kit::col(t.alias.label[1])),
                    );
                    ui.add_space(10.0);
                }
            });
            ui.add_space(6.0);
            segments(app, ui, start, end);
        });
}

fn segments(app: &mut App, ui: &mut egui::Ui, start: i64, end: i64) {
    let t = app.theme;
    // One segment per turn, in order, plus the events outside any turn.
    let spans = turn_spans(&app.trajectory.rows);
    if spans.is_empty() {
        return;
    }
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 10.0), Sense::hover());
    let total = (end - start).max(1) as f32;
    let gap = 2.0;
    let equal_w = (rect.width() - gap * (spans.len() as f32 - 1.0)) / spans.len() as f32;

    let mut x = rect.min.x;
    for (i, span) in spans.iter().enumerate() {
        let w = if app.trajectory.actual_duration {
            let frac = ((span.end_ts - span.start_ts).max(1) as f32 / total).clamp(0.01, 1.0);
            (rect.width() - gap * (spans.len() as f32 - 1.0)) * frac
        } else {
            equal_w
        };
        let seg =
            Rect::from_min_size(egui::pos2(x, rect.min.y), Vec2::new(w.max(3.0), rect.height()));
        let resp = ui.interact(seg, ui.id().with(("traj_seg", i)), Sense::click());
        let fill = if span.failed {
            kit::col(t.alias.error)
        } else if resp.hovered() {
            kit::col(t.alias.business)
        } else {
            kit::col(t.alias.business_tertiary)
        };
        ui.painter().rect_filled(seg, egui::Rounding::same(3.0), fill);
        let hint = match span.turn_id {
            Some(id) => format!(
                "Turn {id} · {} · {} events",
                duration(span.end_ts - span.start_ts),
                span.count
            ),
            None => format!("{} events outside a turn", span.count),
        };
        if resp.on_hover_text(hint).clicked() {
            app.trajectory.scroll_to = Some(span.first_seq);
        }
        x = seg.max.x + gap;
    }
}

struct TurnSpan {
    turn_id: Option<u64>,
    first_seq: u64,
    start_ts: i64,
    end_ts: i64,
    count: usize,
    failed: bool,
}

/// Contiguous runs of rows sharing a `turn_id`. Rows outside any turn form
/// their own run, so the strip covers the whole log rather than only the
/// parts a turn happened to own.
fn turn_spans(rows: &[EventDump]) -> Vec<TurnSpan> {
    let mut out: Vec<TurnSpan> = Vec::new();
    for r in rows {
        match out.last_mut() {
            Some(last) if last.turn_id == r.turn_id => {
                last.end_ts = r.ts;
                last.count += 1;
                last.failed |= r.tag == EventTag::Retry || r.ok == Some(false);
            }
            _ => out.push(TurnSpan {
                turn_id: r.turn_id,
                first_seq: r.seq,
                start_ts: r.ts,
                end_ts: r.ts,
                count: 1,
                failed: r.tag == EventTag::Retry,
            }),
        }
    }
    out
}

fn turn_ids(rows: &[EventDump]) -> Vec<u64> {
    let mut out: Vec<u64> = Vec::new();
    for r in rows {
        if let Some(id) = r.turn_id {
            if out.last() != Some(&id) && !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Ledger
// ---------------------------------------------------------------------------

fn ledger(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    if app.trajectory.rows.is_empty() {
        ui.add_space(28.0);
        ui.vertical_centered(|ui| {
            kit::label(
                ui,
                kit::txt(
                    if app.trajectory.loading {
                        "Loading the event log…"
                    } else {
                        "This session has no events yet."
                    },
                    13.0,
                    Weight::Regular,
                    kit::col(t.alias.label[2]),
                ),
            );
        });
        return;
    }

    header_row(ui, &t);
    let start_ts = app.trajectory.rows.first().map(|r| r.ts).unwrap_or(0);
    let query = app.trajectory.search.to_lowercase();
    let rows = app.trajectory.rows.clone();
    let scroll_to = app.trajectory.scroll_to.take();
    // Request boundaries are numbered in log order, whether they succeeded
    // (a `TokenUsage`) or failed (an `LlmRetry`) — a failed attempt persists
    // no usage, so the retry row *is* the boundary.
    let mut request_n = 0usize;
    let mut cumulative = (0u32, 0u32);

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .id_source("trajectory_ledger")
        .show(ui, |ui| {
            let mut i = 0usize;
            while i < rows.len() {
                let r = &rows[i];
                // A `TurnStart` opens a header; the rows under it fold away
                // when the turn is collapsed. Recognised structurally — it is
                // the first row carrying its turn id — rather than by reading
                // the backend's row text, which is prose and free to change.
                // `ok` separates the two `Turn` rows: an end records the
                // finish reason, a start has no outcome to record.
                if let (EventTag::Turn, Some(id), None) = (r.tag, r.turn_id, r.ok) {
                    if i == 0 || rows[i - 1].turn_id != Some(id) {
                        let collapsed = app.trajectory.collapsed.contains(&id);
                        if turn_header(ui, &t, r, collapsed).clicked() {
                            if collapsed {
                                app.trajectory.collapsed.remove(&id);
                            } else {
                                app.trajectory.collapsed.insert(id);
                            }
                        }
                        if collapsed {
                            let folded = rows[i..]
                                .iter()
                                .take_while(|x| x.turn_id == Some(id))
                                .count();
                            i += folded;
                            continue;
                        }
                        i += 1;
                        continue;
                    }
                }
                if matches!(r.tag, EventTag::Usage | EventTag::Retry) {
                    request_n += 1;
                    cumulative.0 += r.tokens_in;
                    cumulative.1 += r.tokens_out;
                    boundary_row(ui, &t, r, request_n, cumulative);
                    i += 1;
                    continue;
                }
                let dim = !query.is_empty() && !matches(r, &query);
                let resp = data_row(ui, &t, r, start_ts, app.trajectory.selected == Some(r.seq), dim);
                if scroll_to == Some(r.seq) {
                    resp.scroll_to_me(Some(Align::Center));
                }
                if resp.clicked() {
                    app.trajectory.selected = Some(r.seq);
                    app.details_call = None;
                    if app.layout.details_w <= 0.0 {
                        app.layout.details_w = tokens::DETAILS_DEFAULT;
                    }
                }
                i += 1;
            }

            // Paging: dsh loads 50 nodes at a time; the backend's page is
            // larger, so this is a button rather than an infinite scroll —
            // an accidental fetch of a long log is worse than one click.
            if app.trajectory.next_seq.is_some() {
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    let label = if app.trajectory.loading { "Loading…" } else { "Load more" };
                    if kit::button(ui, label, kit::Variant::Outline, kit::Size::Sm).clicked() {
                        app.load_trajectory(false);
                    }
                });
                ui.add_space(8.0);
            }
        });
}

fn header_row(ui: &mut egui::Ui, t: &Theme) {
    ui.horizontal(|ui| {
        ui.set_min_height(24.0);
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), 20.0),
            Sense::hover(),
        );
        let cols = columns(rect);
        let font = kit::font(10.0, Weight::Medium);
        let c = kit::col(t.alias.label[3]);
        let p = ui.painter();
        p.text(cols.seq.left_center(), Align2::LEFT_CENTER, "#", font.clone(), c);
        p.text(cols.tag.left_center(), Align2::LEFT_CENTER, "KIND", font.clone(), c);
        p.text(cols.text.left_center(), Align2::LEFT_CENTER, "EVENT", font.clone(), c);
        p.text(cols.tin.right_center(), Align2::RIGHT_CENTER, "IN", font.clone(), c);
        p.text(cols.tout.right_center(), Align2::RIGHT_CENTER, "OUT", font.clone(), c);
        p.text(cols.time.right_center(), Align2::RIGHT_CENTER, "TIME", font, c);
    });
    kit::hairline(ui, kit::Level::L2);
}

struct Columns {
    seq: Rect,
    tag: Rect,
    text: Rect,
    tin: Rect,
    tout: Rect,
    time: Rect,
}

/// Fixed layout: the numeric columns keep their width so the digits line up
/// down the page, and the text column takes what is left.
fn columns(row: Rect) -> Columns {
    let text_w =
        (row.width() - COL_SEQ - COL_TAG - COL_NUM * 2.0 - COL_TIME - 8.0).max(80.0);
    let widths = [COL_SEQ, COL_TAG, text_w, COL_NUM, COL_NUM, COL_TIME];
    let mut rects = Vec::with_capacity(widths.len());
    let mut x = row.min.x;
    for w in widths {
        rects.push(Rect::from_min_size(
            egui::pos2(x, row.min.y),
            Vec2::new(w, row.height()),
        ));
        x += w;
    }
    Columns {
        seq: rects[0],
        tag: rects[1],
        text: rects[2],
        tin: rects[3],
        tout: rects[4],
        time: rects[5],
    }
}

fn data_row(
    ui: &mut egui::Ui,
    t: &Theme,
    r: &EventDump,
    start_ts: i64,
    selected: bool,
    dim: bool,
) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
    let cols = columns(rect);
    let fade = |c: egui::Color32| if dim { c.gamma_multiply(0.35) } else { c };
    {
        let p = ui.painter();
        if selected {
            p.rect_filled(rect, egui::Rounding::same(4.0), kit::cola(t.alias.active));
        } else if resp.hovered() {
            p.rect_filled(rect, egui::Rounding::same(4.0), kit::cola(t.alias.hover));
        }
        p.text(
            cols.seq.left_center(),
            Align2::LEFT_CENTER,
            format!("{}", r.seq),
            kit::mono_font(11.0),
            fade(kit::col(t.alias.label[3])),
        );
    }
    tag_chip(ui, cols.tag, r.tag, dim, t);

    // A shadowed row is dimmed and struck through: it is still in the log,
    // but the fold has taken it out of the model's view.
    let text_col = if r.shadowed {
        kit::col(t.alias.label[3])
    } else if r.ok == Some(false) {
        kit::col(t.alias.error)
    } else {
        kit::col(t.alias.label[1])
    };
    let font = kit::font(TEXT_SIZE, Weight::Regular);
    let shown = kit::elide(ui, &r.text, &font, cols.text.width() - 8.0);
    let p = ui.painter();
    let painted = p.text(
        cols.text.left_center(),
        Align2::LEFT_CENTER,
        &shown,
        font,
        fade(text_col),
    );
    // Struck through where its own text sits, not across the whole column.
    if r.shadowed {
        p.hline(
            painted.x_range(),
            painted.center().y,
            egui::Stroke::new(1.0, fade(kit::col(t.alias.label[3]))),
        );
    }
    for (v, at) in [(r.tokens_in, cols.tin), (r.tokens_out, cols.tout)] {
        if v > 0 {
            p.text(
                at.right_center(),
                Align2::RIGHT_CENTER,
                thousands(v),
                kit::mono_font(11.0),
                fade(kit::col(t.alias.label[2])),
            );
        }
    }
    p.text(
        cols.time.right_center(),
        Align2::RIGHT_CENTER,
        offset(r.ts - start_ts),
        kit::mono_font(11.0),
        fade(kit::col(t.alias.label[3])),
    );
    resp
}

/// `Turn n · {source}` — a full-width band with a chevron that folds the
/// turn's rows away.
fn turn_header(ui: &mut egui::Ui, t: &Theme, r: &EventDump, collapsed: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), TURN_H),
        Sense::click(),
    );
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, egui::Rounding::same(4.0), kit::cola(t.alias.hover));
    }
    p.hline(
        rect.x_range(),
        rect.min.y + 0.5,
        egui::Stroke::new(tokens::HAIRLINE, kit::Level::L2.color(t)),
    );
    let chev = Rect::from_center_size(
        egui::pos2(rect.min.x + 10.0, rect.center().y),
        Vec2::splat(12.0),
    );
    crate::ui::icons::paint(
        p,
        chev,
        if collapsed { Icon::ChevronRight } else { Icon::ChevronDown },
        kit::col(t.alias.label[2]),
    );
    p.text(
        egui::pos2(rect.min.x + COL_SEQ, rect.center().y),
        Align2::LEFT_CENTER,
        &r.text,
        kit::font(13.0, Weight::Medium),
        kit::col(t.alias.label[0]),
    );
    resp
}

/// `Request #n` with its own usage and the running cumulative — red when the
/// request failed, which is what an `LlmRetry` row records.
fn boundary_row(
    ui: &mut egui::Ui,
    t: &Theme,
    r: &EventDump,
    n: usize,
    cumulative: (u32, u32),
) {
    let failed = r.tag == EventTag::Retry;
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), ROW_H),
        Sense::hover(),
    );
    let color = if failed {
        kit::col(t.alias.error)
    } else {
        kit::col(t.alias.label[2])
    };
    let label = format!("Request #{n}{}", if failed { " — failed" } else { "" });
    let font = kit::font(11.0, Weight::Medium);
    let label_w = ui
        .fonts(|f| f.layout_no_wrap(label.clone(), font.clone(), color))
        .size()
        .x;
    let p = ui.painter();
    p.text(rect.left_center(), Align2::LEFT_CENTER, &label, font, color);
    let line_x = egui::Rangef::new(rect.min.x + label_w + 8.0, rect.max.x - 260.0);
    if line_x.span() > 8.0 {
        p.hline(
            line_x,
            rect.center().y,
            egui::Stroke::new(tokens::HAIRLINE, kit::Level::L2.color(t)),
        );
    }
    let detail = if failed {
        r.text.clone()
    } else {
        format!(
            "in {} · out {} · Σ {} / {}",
            thousands(r.tokens_in),
            thousands(r.tokens_out),
            thousands(cumulative.0),
            thousands(cumulative.1)
        )
    };
    p.text(
        rect.right_center(),
        Align2::RIGHT_CENTER,
        detail,
        kit::mono_font(11.0),
        color,
    );
}

/// r=6, 22 px, tinted per kind (§10). The palette is the alias map's, so it
/// tracks the theme rather than naming colours here.
fn tag_chip(ui: &mut egui::Ui, at: Rect, tag: EventTag, dim: bool, t: &Theme) {
    let (fill, fg) = tag_colors(tag, t);
    let (fill, fg) = if dim {
        (fill.gamma_multiply(0.35), fg.gamma_multiply(0.4))
    } else {
        (fill, fg)
    };
    let font = kit::font(10.0, Weight::Medium);
    let text = tag.label();
    let w = ui
        .fonts(|f| f.layout_no_wrap(text.to_string(), font.clone(), fg))
        .size()
        .x;
    let chip = Rect::from_min_size(
        egui::pos2(at.min.x, at.center().y - 11.0),
        Vec2::new((w + 14.0).min(at.width() - 4.0), 22.0),
    );
    let p = ui.painter();
    p.rect_filled(chip, egui::Rounding::same(6.0), fill);
    p.text(chip.center(), Align2::CENTER_CENTER, text, font, fg);
}

fn tag_colors(tag: EventTag, t: &Theme) -> (egui::Color32, egui::Color32) {
    let a = &t.alias;
    match tag {
        EventTag::User => (kit::col(a.success_tertiary), kit::col(a.success)),
        EventTag::Context => (
            kit::col(a.success_tertiary).gamma_multiply(0.6),
            kit::col(a.success),
        ),
        EventTag::Assistant => (kit::col(a.business_tertiary), kit::col(a.business)),
        EventTag::Tool => (kit::col(a.warn_tertiary), kit::col(a.warn_label)),
        EventTag::ToolResult => (
            kit::col(a.warn_tertiary).gamma_multiply(0.6),
            kit::col(a.warn_label),
        ),
        EventTag::Retry => (kit::col(a.warn_tertiary), kit::col(a.error)),
        EventTag::Turn | EventTag::Usage => (kit::col(a.tip), kit::col(a.label[1])),
        _ => (kit::col(a.tip), kit::col(a.label[2])),
    }
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

fn matches(r: &EventDump, query: &str) -> bool {
    r.text.to_lowercase().contains(query)
        || r.payload.to_lowercase().contains(query)
        || r.result.to_lowercase().contains(query)
        || r.tag.label().to_lowercase().contains(query)
}

/// `1,204` — the ledger's numbers are read at a glance, and four unspaced
/// digits are not.
pub fn thousands(v: u32) -> String {
    let s = v.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A span, in the coarsest unit that still says something: `840 ms`, `12.4 s`,
/// `3 m 04 s`.
pub fn duration(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 1000 {
        return format!("{ms} ms");
    }
    if ms < 60_000 {
        return format!("{:.1} s", ms as f64 / 1000.0);
    }
    format!("{} m {:02} s", ms / 60_000, (ms % 60_000) / 1000)
}

/// Time since the first row, as the ledger's Time column: `+0:04.120`.
pub fn offset(ms: i64) -> String {
    let ms = ms.max(0);
    format!("+{}:{:02}.{:03}", ms / 60_000, (ms % 60_000) / 1000, ms % 1000)
}

/// Wall-clock time of a unix-millisecond stamp, in the user's own zone.
pub fn clock(ts_ms: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(ts_ms).single() {
        Some(dt) => dt.format("%H:%M:%S").to_string(),
        None => "—".into(),
    }
}

/// Full date and time, for the inspector's Timing tab.
pub fn stamp(ts_ms: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(ts_ms).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
        None => "—".into(),
    }
}

/// Rows of the loaded page, in log order — what the inspector pages through.
pub fn selected_row(state: &TrajectoryState) -> Option<&EventDump> {
    let seq = state.selected?;
    state.rows.iter().find(|r| r.seq == seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(seq: u64, ts: i64, tag: EventTag, turn: Option<u64>) -> EventDump {
        EventDump {
            seq,
            ts,
            tag,
            text: String::new(),
            payload: String::new(),
            result: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            ok: None,
            shadowed: false,
            shadows: None,
            call_seq: None,
            turn_id: turn,
            raw: String::new(),
        }
    }

    #[test]
    fn spans_break_on_the_turn_id() {
        let rows = vec![
            ev(1, 0, EventTag::System, None),
            ev(2, 10, EventTag::Turn, Some(1)),
            ev(3, 20, EventTag::User, Some(1)),
            ev(4, 30, EventTag::Turn, Some(2)),
        ];
        let spans = turn_spans(&rows);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].turn_id, None);
        assert_eq!(spans[1].turn_id, Some(1));
        assert_eq!(spans[1].count, 2);
        assert_eq!(spans[1].end_ts, 20);
        assert_eq!(spans[2].first_seq, 4);
    }

    #[test]
    fn a_retrying_span_is_marked_failed() {
        let rows = vec![ev(1, 0, EventTag::Turn, Some(1)), ev(2, 5, EventTag::Retry, Some(1))];
        assert!(turn_spans(&rows)[0].failed);
    }

    #[test]
    fn turn_ids_are_listed_once_in_order() {
        let rows = vec![
            ev(1, 0, EventTag::Turn, Some(4)),
            ev(2, 0, EventTag::User, Some(4)),
            ev(3, 0, EventTag::Turn, Some(9)),
            ev(4, 0, EventTag::System, None),
        ];
        assert_eq!(turn_ids(&rows), vec![4, 9]);
    }

    #[test]
    fn search_reads_the_bodies_not_only_the_row_text() {
        let mut r = ev(1, 0, EventTag::Tool, None);
        r.payload = "README.md".into();
        assert!(matches(&r, "readme"));
        assert!(matches(&r, "tool"), "the kind tag is searchable too");
        assert!(!matches(&r, "cargo"));
    }

    #[test]
    fn numbers_and_spans_read_at_a_glance() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1204), "1,204");
        assert_eq!(thousands(1_204_000), "1,204,000");
        assert_eq!(duration(840), "840 ms");
        assert_eq!(duration(12_400), "12.4 s");
        assert_eq!(duration(184_000), "3 m 04 s");
        assert_eq!(duration(-5), "0 ms", "a clock that went backwards is not negative time");
        assert_eq!(offset(4_120), "+0:04.120");
    }

    #[test]
    fn columns_keep_the_numeric_widths_and_give_the_rest_to_text() {
        let row = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(900.0, ROW_H));
        let c = columns(row);
        assert_eq!(c.seq.width(), COL_SEQ);
        assert_eq!(c.tin.width(), COL_NUM);
        assert_eq!(c.time.width(), COL_TIME);
        assert!(c.text.width() > 400.0);
        assert!(c.time.max.x <= row.max.x + 0.01);
    }

    #[test]
    fn a_narrow_column_still_leaves_the_text_a_floor() {
        let row = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(240.0, ROW_H));
        assert!(columns(row).text.width() >= 80.0);
    }
}
