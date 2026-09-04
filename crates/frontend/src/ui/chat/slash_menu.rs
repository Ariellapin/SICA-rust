//! The "/" palette. Typing a leading slash in the composer opens a picker
//! listing everything the workspace can offer:
//!
//!   * **COMMANDS** — the app actions below ([`AppCommand`]), plus any
//!     `commands/*.md` the backend found.
//!   * **SKILLS**   — the live skill registry (Rust built-ins + `skills/*.md`).
//!   * **AGENTS**   — `agents/*.md`.
//!
//! The catalogue arrives once per IPC connection via `Request::ListCatalog`
//! and lives on [`crate::app::SlashState`]; the frontend's own app commands are
//! appended here and never travel over the wire.
//!
//! Keys, all consumed *before* the composer's TextEdit or the send/Esc handlers
//! see them (which is why [`draw`] runs first in `input_bar::draw`):
//!
//! | key          | effect                                              |
//! |--------------|-----------------------------------------------------|
//! | `↑` / `↓`    | move the highlight (wraps at both ends)             |
//! | `Enter`/`Tab`| accept the highlighted row                          |
//! | `Esc`        | dismiss the list, keeping the draft text            |
//!
//! Accepting an **app command** runs it immediately and clears the draft;
//! accepting anything else rewrites the draft to `/<name> ` and parks the caret
//! at the end so the user can type arguments. The trailing space is what closes
//! the palette — a query containing whitespace is no longer a palette query.

use egui::{Align, Color32, Rounding, Sense, Stroke, Vec2};

use protocol::{CatalogEntry, CatalogKind, Request};
use sica_core::theme::tokens::{HAIRLINE, RADIUS_2};

use crate::app::{rgb, App, AppView, SettingsTab};
use crate::supervisor::UiCommand;
use crate::ui::widgets::caps_label;

/// Tallest the list gets before it scrolls internally.
const MAX_LIST_HEIGHT: f32 = 260.0;

/// An app action the frontend performs itself — no LLM turn involved. These
/// mirror controls that already exist elsewhere in the UI, so the palette is a
/// keyboard route to them rather than a new capability.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AppCommand {
    NewSession,
    StopTurn,
    CompactNow,
    PlanToggle,
    PermissionHint,
    GoalHint,
    ClearDraft,
    OpenSettings,
    OpenLlmSettings,
    OpenSkillSettings,
    RebuildBackend,
}

/// `(name, description, action)` for every app command, in display order.
const APP_COMMANDS: &[(&str, &str, AppCommand)] = &[
    ("new", "Start a fresh chat session.", AppCommand::NewSession),
    ("stop", "Interrupt the turn that is streaming.", AppCommand::StopTurn),
    ("compact", "Fold older history into a summary now.", AppCommand::CompactNow),
    ("plan", "Toggle plan mode (explore-only until exit-plan-mode).", AppCommand::PlanToggle),
    ("permission", "Switch permission mode: /permission <read-only|workspace-write|danger-full-access>.", AppCommand::PermissionHint),
    ("goal", "Show or change this session's objective: /goal [continue|pause|complete|block <why>].", AppCommand::GoalHint),
    ("clear", "Empty the composer and drop attachments.", AppCommand::ClearDraft),
    ("settings", "Open the Settings view.", AppCommand::OpenSettings),
    ("llm", "Open Settings → LLM to pick a provider.", AppCommand::OpenLlmSettings),
    ("skills", "Open Settings → General, where the skills folder lives.", AppCommand::OpenSkillSettings),
    ("rebuild", "Rebuild the backend and restart it.", AppCommand::RebuildBackend),
];

/// One candidate row: either a catalogue entry from the BE or an app command.
struct Row {
    kind:        CatalogKind,
    name:        String,
    description: String,
    args:        Vec<String>,
    source:      Option<String>,
    action:      Option<AppCommand>,
}

/// What the composer needs to know after the palette has had its turn at the
/// input. While `open`, Enter belongs to the palette and must not send.
pub struct Outcome {
    pub open: bool,
}

/// Draw the palette (when the draft is a slash query) and handle its keys.
/// Called at the top of `input_bar::draw`, so the list renders above the
/// composer and grows the bottom panel upwards.
pub fn draw(app: &mut App, ui: &mut egui::Ui, input_focused: bool) -> Outcome {
    let Some(query) = query_of(&app.chat.draft).map(str::to_owned) else {
        // Not a slash query any more — forget the picker state so the next `/`
        // opens on the first row rather than wherever the last one left off.
        app.chat.slash.selected = 0;
        app.chat.slash.last_query.clear();
        app.chat.slash.dismissed = false;
        return Outcome { open: false };
    };

    if query != app.chat.slash.last_query {
        app.chat.slash.last_query = query.clone();
        app.chat.slash.selected = 0;
        // Typing after an Esc is a fresh intent — bring the list back.
        app.chat.slash.dismissed = false;
        // A bottom panel is only as tall as the content it measured, so the
        // frame that opens (or resizes) the list needs a follow-up frame to
        // settle at the new height instead of clipping it.
        ui.ctx().request_repaint();
    }
    if app.chat.slash.dismissed {
        return Outcome { open: false };
    }

    let rows = candidates(&app.chat.slash.entries, &query);
    if rows.is_empty() {
        draw_empty(app, ui, &query);
        // Still "open": Enter on a no-match query would otherwise send `/nope`
        // as a chat message, which is never what the slash was for.
        return Outcome { open: true };
    }

    let keys = if input_focused { read_keys(ui) } else { Keys::default() };
    let nav = navigate(&mut app.chat.slash, rows.len(), &keys);
    if matches!(nav, Nav::Dismissed) {
        return Outcome { open: false };
    }
    let selected = app.chat.slash.selected;

    let clicked = draw_list(app, ui, &rows, selected, matches!(nav, Nav::Moved));
    let accepted = clicked.or(match nav {
        Nav::Accept(i) => Some(i),
        _ => None,
    });

    if let Some(i) = accepted {
        accept(app, ui, &rows[i]);
    }
    Outcome { open: true }
}

/// The palette query for a draft, or `None` when the draft isn't one. A query
/// runs from a leading `/` up to the first whitespace: once the user types a
/// space they are writing arguments, and the picker gets out of the way.
fn query_of(draft: &str) -> Option<&str> {
    let rest = draft.strip_prefix('/')?;
    if rest.contains(char::is_whitespace) {
        return None;
    }
    Some(rest)
}

/// Filter + order the catalogue for `query`. Groups stay intact (commands,
/// then skills, then agents) so each heading appears at most once; within a
/// group, name-prefix matches come before name-substring matches, which come
/// before description-only matches. An empty query keeps everything.
fn candidates(entries: &[CatalogEntry], query: &str) -> Vec<Row> {
    let q = query.to_lowercase();
    let mut rows: Vec<(u8, Row)> = APP_COMMANDS
        .iter()
        .map(|(name, description, action)| Row {
            kind:        CatalogKind::Command,
            name:        (*name).to_string(),
            description: (*description).to_string(),
            args:        Vec::new(),
            source:      None,
            action:      Some(*action),
        })
        .chain(entries.iter().map(|e| Row {
            kind:        e.kind,
            name:        e.name.clone(),
            description: e.description.clone(),
            args:        e.args.clone(),
            source:      e.source.clone(),
            action:      None,
        }))
        .filter_map(|row| rank(&row, &q).map(|r| (r, row)))
        .collect();

    // Stable sort: equal keys keep APP_COMMANDS order and the BE's own
    // name-sorted order inside each group.
    rows.sort_by_key(|(rank, row)| (group_order(row.kind), *rank));
    rows.into_iter().map(|(_, row)| row).collect()
}

/// Match rank, or `None` when the row doesn't match at all.
fn rank(row: &Row, query_lower: &str) -> Option<u8> {
    if query_lower.is_empty() {
        return Some(0);
    }
    let name = row.name.to_lowercase();
    if name.starts_with(query_lower) {
        Some(0)
    } else if name.contains(query_lower) {
        Some(1)
    } else if row.description.to_lowercase().contains(query_lower) {
        Some(2)
    } else {
        None
    }
}

fn group_order(kind: CatalogKind) -> u8 {
    match kind {
        CatalogKind::Command => 0,
        CatalogKind::Skill   => 1,
        CatalogKind::Agent   => 2,
    }
}

fn group_label(kind: CatalogKind) -> &'static str {
    match kind {
        CatalogKind::Command => "Commands",
        CatalogKind::Skill   => "Skills",
        CatalogKind::Agent   => "Agents",
    }
}

/// What one frame of key input asked the palette to do.
#[derive(Debug, PartialEq, Eq)]
enum Nav {
    /// Esc: the list closes but the draft survives.
    Dismissed,
    /// The highlight moved this frame, so the list should scroll it into view.
    Moved,
    /// Commit the row at this index.
    Accept(usize),
    Idle,
}

/// Fold one frame of key input into the picker state. `len` is the number of
/// filtered rows and must be non-zero; the selection is clamped to it first,
/// because the list shrinks under the highlight as the query narrows.
fn navigate(state: &mut crate::app::SlashState, len: usize, keys: &Keys) -> Nav {
    debug_assert!(len > 0);
    state.selected = state.selected.min(len - 1);
    if keys.dismiss {
        state.dismissed = true;
        return Nav::Dismissed;
    }
    let mut moved = false;
    if keys.down {
        state.selected = (state.selected + 1) % len;
        moved = true;
    } else if keys.up {
        // `+ len - 1` rather than `- 1`: the highlight wraps to the bottom
        // instead of underflowing at the first row.
        state.selected = (state.selected + len - 1) % len;
        moved = true;
    }
    if keys.accept {
        return Nav::Accept(state.selected);
    }
    if moved {
        Nav::Moved
    } else {
        Nav::Idle
    }
}

#[derive(Default)]
struct Keys {
    up:      bool,
    down:    bool,
    accept:  bool,
    dismiss: bool,
}

/// Claim the navigation keys before any other consumer sees them. `consume_key`
/// removes the event, which is what stops Enter from also sending the message,
/// Esc from also interrupting the turn, and the arrows from also moving the
/// text caret.
fn read_keys(ui: &mut egui::Ui) -> Keys {
    ui.input_mut(|i| {
        let none = egui::Modifiers::NONE;
        Keys {
            down:    i.consume_key(none, egui::Key::ArrowDown),
            up:      i.consume_key(none, egui::Key::ArrowUp),
            accept:  i.consume_key(none, egui::Key::Enter)
                | i.consume_key(none, egui::Key::Tab),
            dismiss: i.consume_key(none, egui::Key::Escape),
        }
    })
}

/// Paint the grouped list. Returns the index of a row the pointer clicked.
fn draw_list(
    app: &App,
    ui: &mut egui::Ui,
    rows: &[Row],
    selected: usize,
    scroll_to_selected: bool,
) -> Option<usize> {
    let p = app.palette;
    let mut clicked = None;

    frame(&p).show(ui, |ui| {
        egui::ScrollArea::vertical()
            .id_source("slash_menu_scroll")
            .max_height(MAX_LIST_HEIGHT)
            .show(ui, |ui| {
                let mut last_kind: Option<CatalogKind> = None;
                for (i, row) in rows.iter().enumerate() {
                    if last_kind != Some(row.kind) {
                        if last_kind.is_some() {
                            ui.add_space(6.0);
                        }
                        caps_label(ui, group_label(row.kind), rgb(p.muted));
                        ui.add_space(2.0);
                        last_kind = Some(row.kind);
                    }
                    if draw_row(app, ui, row, i, i == selected, scroll_to_selected) {
                        clicked = Some(i);
                    }
                }
            });
        ui.add_space(6.0);
        caps_label(
            ui,
            "↑↓ move · enter accept · esc dismiss",
            rgb(p.muted),
        );
    });
    ui.add_space(6.0);
    clicked
}

/// One row. Returns `true` when it was clicked.
fn draw_row(
    app: &App,
    ui: &mut egui::Ui,
    row: &Row,
    index: usize,
    selected: bool,
    scroll_to_selected: bool,
) -> bool {
    let p = app.palette;
    // The background depends on hover, which isn't known until the row has
    // been laid out — so reserve a shape slot now and fill it in after.
    let bg_slot = ui.painter().add(egui::Shape::Noop);

    let row_h = 22.0;
    let inner = ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), row_h),
        egui::Layout::left_to_right(Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.add_space(8.0);
            let name_color = if selected { rgb(p.accent) } else { rgb(p.ink) };
            ui.label(
                egui::RichText::new(format!("/{}", row.name))
                    .monospace()
                    .color(name_color),
            );
            if !row.args.is_empty() {
                let hint = row
                    .args
                    .iter()
                    .map(|a| format!("<{a}>"))
                    .collect::<Vec<_>>()
                    .join(" ");
                ui.label(egui::RichText::new(hint).monospace().small().color(rgb(p.muted)));
            }
            if !row.description.is_empty() {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&row.description).small().color(rgb(p.muted)),
                    )
                    .truncate(),
                );
            }
        },
    );

    let rect = inner.response.rect;
    let resp = ui.interact(rect, ui.id().with(("slash_row", index)), Sense::click());
    let fill = if selected {
        rgb(p.accent_subtle)
    } else if resp.hovered() {
        rgb(p.surface_sunk)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().set(
        bg_slot,
        egui::epaint::RectShape::filled(rect, Rounding::same(RADIUS_2), fill),
    );
    if selected {
        // 2px accent slab on the left edge — the same "you are here" mark the
        // protocol banner and sidebar use.
        let slab = egui::Rect::from_min_size(rect.min, Vec2::new(2.0, rect.height()));
        ui.painter().rect_filled(slab, 0.0, rgb(p.accent));
        if scroll_to_selected {
            resp.scroll_to_me(Some(Align::Center));
        }
    }
    if let Some(src) = &row.source {
        return resp.on_hover_text(src).clicked();
    }
    resp.clicked()
}

/// Nothing matched: say so rather than leaving a bare frame, and remind the
/// user how to get out of the query.
fn draw_empty(app: &mut App, ui: &mut egui::Ui, query: &str) {
    let p = app.palette;
    // Esc has to be claimed here too — otherwise the only way out of a
    // no-match query is deleting it by hand. Enter/Tab are swallowed with it
    // so a typo can't leak through to the composer as a sent message.
    let dismiss = ui.input_mut(|i| {
        let none = egui::Modifiers::NONE;
        let _ = i.consume_key(none, egui::Key::Enter) | i.consume_key(none, egui::Key::Tab);
        i.consume_key(none, egui::Key::Escape)
    });
    if dismiss {
        app.chat.slash.dismissed = true;
        return;
    }
    frame(&p).show(ui, |ui| {
        caps_label(ui, &format!("no match for /{query}"), rgb(p.muted));
    });
    ui.add_space(6.0);
}

fn frame(p: &sica_core::theme::Palette) -> egui::Frame {
    egui::Frame::none()
        .fill(rgb(p.surface))
        .stroke(Stroke::new(HAIRLINE, rgb(p.hairline)))
        .rounding(Rounding::same(RADIUS_2))
        .inner_margin(egui::Margin::symmetric(8.0, 8.0))
}

/// Commit the highlighted row: run app commands, insert everything else.
fn accept(app: &mut App, ui: &mut egui::Ui, row: &Row) {
    match row.action {
        // `/permission` needs an argument and `/goal` takes an optional
        // one — complete the prefix in the draft like a catalogue entry
        // instead of firing immediately.
        Some(AppCommand::PermissionHint) | Some(AppCommand::GoalHint) | None => {
            // Trailing space: it separates the name from its arguments *and*
            // closes the palette (whitespace ends a slash query).
            app.chat.draft = format!("/{} ", row.name);
            move_caret_to_end(ui, &app.chat.draft);
        }
        Some(cmd) => {
            app.chat.draft.clear();
            run_app_command(app, cmd);
        }
    }
    app.chat.slash.selected = 0;
    app.chat.slash.dismissed = false;
}

fn run_app_command(app: &mut App, cmd: AppCommand) {
    match cmd {
        AppCommand::NewSession => app.send(UiCommand::SendRequest(Request::NewSession)),
        AppCommand::StopTurn => {
            let streaming = app.chat.turns.last().map(|t| !t.finished).unwrap_or(false);
            if streaming && !app.chat.interrupt_requested {
                app.interrupt_turn();
            }
        }
        AppCommand::CompactNow => {
            let id = app.chat.session_id;
            app.last_command_session = Some(id);
            app.send(UiCommand::SendRequest(Request::RunCommand {
                session_id: id,
                name: "compact".into(),
                input: String::new(),
            }));
        }
        AppCommand::PlanToggle => {
            let id = app.chat.session_id;
            app.last_command_session = Some(id);
            app.send(UiCommand::SendRequest(Request::RunCommand {
                session_id: id,
                name: "plan".into(),
                input: String::new(),
            }));
        }
        AppCommand::PermissionHint => {
            // Reached only programmatically — the palette completes the
            // prefix instead (see `accept`).
            app.chat.draft = "/permission ".to_string();
        }
        AppCommand::GoalHint => {
            app.chat.draft = "/goal ".to_string();
        }
        AppCommand::ClearDraft => {
            app.chat.draft.clear();
            app.chat.pending_images.clear();
        }
        AppCommand::OpenSettings => app.view = AppView::Settings,
        AppCommand::OpenLlmSettings => {
            app.view = AppView::Settings;
            app.settings_tab = SettingsTab::Llm;
        }
        AppCommand::OpenSkillSettings => {
            app.view = AppView::Settings;
            app.settings_tab = SettingsTab::General;
        }
        AppCommand::RebuildBackend => {
            app.send(UiCommand::RebuildAndRestart { release: app.release_profile });
        }
    }
}

/// Park the composer's caret after the text we just wrote. Without this the
/// TextEdit keeps the cursor where it was when the user typed `/`, and the next
/// keystroke lands in the middle of the inserted name.
fn move_caret_to_end(ui: &mut egui::Ui, text: &str) {
    let id = egui::Id::new("chat_input_field");
    let ctx = ui.ctx();
    let mut state = egui::widgets::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
    let end = egui::text::CCursor::new(text.chars().count());
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::one(end)));
    state.store(ctx, id);
    ui.memory_mut(|m| m.request_focus(id));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: CatalogKind, name: &str, description: &str) -> CatalogEntry {
        CatalogEntry {
            kind,
            name: name.into(),
            description: description.into(),
            args: Vec::new(),
            source: None,
        }
    }

    #[test]
    fn query_starts_at_slash_and_ends_at_whitespace() {
        assert_eq!(query_of("/"), Some(""));
        assert_eq!(query_of("/read"), Some("read"));
        // Once an argument is being typed the palette is done.
        assert_eq!(query_of("/read-file foo.md"), None);
        assert_eq!(query_of("hello"), None);
        assert_eq!(query_of("what about /read"), None);
        assert_eq!(query_of("/multi\nline"), None);
    }

    #[test]
    fn empty_query_lists_app_commands_and_catalogue() {
        let entries = vec![entry(CatalogKind::Skill, "read-file", "read a file")];
        let rows = candidates(&entries, "");
        assert_eq!(rows.len(), APP_COMMANDS.len() + 1);
        // Commands group first, catalogue skills after.
        assert_eq!(rows[0].name, "new");
        assert_eq!(rows.last().unwrap().name, "read-file");
    }

    #[test]
    fn groups_stay_contiguous_and_ordered() {
        let entries = vec![
            entry(CatalogKind::Agent, "reviewer", ""),
            entry(CatalogKind::Skill, "run-cli", ""),
            entry(CatalogKind::Command, "standup", ""),
        ];
        let rows = candidates(&entries, "");
        let kinds: Vec<u8> = rows.iter().map(|r| group_order(r.kind)).collect();
        assert!(kinds.windows(2).all(|w| w[0] <= w[1]), "kinds not grouped: {kinds:?}");
    }

    #[test]
    fn prefix_matches_outrank_substring_and_description_matches() {
        let entries = vec![
            entry(CatalogKind::Skill, "write-file", "no keyword here"),
            entry(CatalogKind::Skill, "file-read", "reads bytes"),
            entry(CatalogKind::Skill, "unrelated", "handy for a file"),
        ];
        let rows = candidates(&entries, "file");
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["file-read", "write-file", "unrelated"]);
    }

    #[test]
    fn non_matching_query_yields_nothing() {
        let entries = vec![entry(CatalogKind::Skill, "run-cli", "shell")];
        assert!(candidates(&entries, "zzz").is_empty());
    }

    fn keys(up: bool, down: bool, accept: bool, dismiss: bool) -> Keys {
        Keys { up, down, accept, dismiss }
    }

    #[test]
    fn arrows_wrap_at_both_ends() {
        let mut state = crate::app::SlashState::default();
        assert_eq!(navigate(&mut state, 3, &keys(false, true, false, false)), Nav::Moved);
        assert_eq!(state.selected, 1);
        navigate(&mut state, 3, &keys(false, true, false, false));
        navigate(&mut state, 3, &keys(false, true, false, false));
        assert_eq!(state.selected, 0, "down past the last row wraps to the first");
        navigate(&mut state, 3, &keys(true, false, false, false));
        assert_eq!(state.selected, 2, "up from the first row wraps to the last");
    }

    #[test]
    fn selection_is_clamped_when_the_list_shrinks() {
        let mut state = crate::app::SlashState::default();
        state.selected = 7;
        assert_eq!(navigate(&mut state, 2, &keys(false, false, true, false)), Nav::Accept(1));
    }

    #[test]
    fn esc_dismisses_without_accepting() {
        let mut state = crate::app::SlashState::default();
        assert_eq!(navigate(&mut state, 3, &keys(false, false, true, true)), Nav::Dismissed);
        assert!(state.dismissed);
    }

    #[test]
    fn idle_frame_changes_nothing() {
        let mut state = crate::app::SlashState::default();
        state.selected = 1;
        assert_eq!(navigate(&mut state, 3, &keys(false, false, false, false)), Nav::Idle);
        assert_eq!(state.selected, 1);
        assert!(!state.dismissed);
    }

    #[test]
    fn app_commands_carry_an_action_and_catalogue_rows_do_not() {
        let entries = vec![entry(CatalogKind::Command, "standup", "md command")];
        let rows = candidates(&entries, "");
        let new = rows.iter().find(|r| r.name == "new").unwrap();
        let md = rows.iter().find(|r| r.name == "standup").unwrap();
        assert!(new.action.is_some());
        assert!(md.action.is_none());
    }
}
