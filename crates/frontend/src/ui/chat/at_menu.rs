//! The "@" file picker (§6.3). Typing `@` at the start of the draft or after
//! whitespace opens a list of workspace paths; accepting one writes the
//! relative path into the draft.
//!
//! Two things separate it from the `/` palette next door:
//!
//!   * **The list is the frontend's own.** `@` names a path in
//!     [`sica_core::paths::workspace_root`], which the frontend resolves for
//!     itself, so there is no reason to send a keystroke through the
//!     dispatcher loop and wait behind whatever turn is running. The walk
//!     honours `.gitignore` through the `ignore` crate — the same crate the
//!     backend's `glob` skill uses, so the two agree on what is in the tree.
//!   * **It opens anywhere in the draft**, not just at the head, so the token
//!     under the *caret* is what the query comes from.
//!
//! Accepting a directory inserts `@dir/` and leaves the picker open on the
//! narrowed query — dsh's "browse folder" — while accepting a file inserts
//! `@path ` and the trailing space closes it.
//!
//! The path is plain text: nothing resolves it, and the model reads it as
//! written. That is what `@` is for — pointing, precisely and cheaply, at a
//! file the agent should open itself.

use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use egui::{Align, Align2, Color32, Rounding, Sense, Vec2};

use sica_core::theme::tokens::RADIUS_ITEM;

use crate::app::{App, FileEntry};
use crate::ui::icons::Icon;
use crate::ui::kit::{self, Weight};

/// Id of the composer's `TextEdit` — the caret this picker reads.
const INPUT_ID: &str = "chat_input_field";

/// Tallest the list gets before it scrolls internally.
const MAX_LIST_HEIGHT: f32 = 320.0;
/// Rows kept after ranking. The menu scrolls, but painting a whole workspace
/// every frame would not be free.
const MAX_ROWS: usize = 60;
/// Entries the walk collects before it stops. A tree larger than this is one
/// where `@` browsing has stopped being the right tool anyway.
const MAX_ENTRIES: usize = 20_000;
/// How deep the walk goes. Deep trees are almost always vendored.
const MAX_DEPTH: usize = 12;
/// How long an index is served before the next open re-walks the tree.
pub const INDEX_TTL: Duration = Duration::from_secs(30);

/// What the composer needs to know after the picker has had its turn.
pub struct Outcome {
    pub open: bool,
}

/// Draw the picker (when the caret sits in an `@` token) and handle its keys.
/// Runs after [`super::slash_menu::draw`] and only when that one is closed:
/// the two never want the same keystroke, because a slash query ends at the
/// first whitespace and an `@` token starts after one.
pub fn draw(app: &mut App, ui: &mut egui::Ui, input_focused: bool) -> Outcome {
    let caret = caret_char_index(ui.ctx(), &app.chat.draft);
    let Some(token) = token_at(&app.chat.draft, caret) else {
        app.chat.at.selected = 0;
        app.chat.at.last_query.clear();
        app.chat.at.dismissed = false;
        return Outcome { open: false };
    };

    if token.query != app.chat.at.last_query {
        app.chat.at.last_query = token.query.clone();
        app.chat.at.selected = 0;
        app.chat.at.dismissed = false;
    }
    if app.chat.at.dismissed {
        return Outcome { open: false };
    }

    poll_index(app);
    if app.chat.at.entries.is_empty() {
        // Nothing to pick from yet. Say which of the two reasons it is, and
        // hold the keys either way so Enter cannot send a half-typed `@`.
        let indexing = app.chat.at.scanned_at.is_none();
        return draw_placeholder(app, ui, indexing, &token.query);
    }

    let rows = candidates(&app.chat.at.entries, &token.query);
    if rows.is_empty() {
        return draw_placeholder(app, ui, false, &token.query);
    }

    let keys = if input_focused { read_keys(ui) } else { Keys::default() };
    let nav = navigate(
        &mut app.chat.at.selected,
        &mut app.chat.at.dismissed,
        rows.len(),
        &keys,
    );
    if matches!(nav, Nav::Dismissed) {
        return Outcome { open: false };
    }
    let selected = app.chat.at.selected;
    let list = draw_list(app, ui, &rows, selected, matches!(nav, Nav::Moved));
    if list.outside_press {
        // Same rule as the `/` palette: a pointerdown anywhere else closes
        // the picker and leaves the half-typed token alone.
        app.chat.at.dismissed = true;
        return Outcome { open: false };
    }
    let accepted = list.inner.or(match nav {
        Nav::Accept(i) => Some(i),
        _ => None,
    });

    if let Some(i) = accepted {
        let (path, is_dir) = (rows[i].path.clone(), rows[i].is_dir);
        accept(app, ui, &token, &path, is_dir);
    }
    Outcome { open: true }
}

// ---------------------------------------------------------------------------
// The token under the caret
// ---------------------------------------------------------------------------

/// An `@` token in the draft: character positions of the `@` itself and of
/// the caret that ends it, plus the text between them.
#[derive(Debug, PartialEq, Eq)]
pub struct Token {
    /// Character index of the `@`.
    pub start: usize,
    /// Character index one past the last character of the query (the caret).
    pub end:   usize,
    pub query: String,
}

/// The `@` token the caret sits in, or `None`.
///
/// The `@` must open the token — start-of-draft or after whitespace — so
/// `user@host` and an email address are not pickers. Whitespace ends it,
/// which is why the query never spans a space: a path that needs one is
/// typed by hand, and dsh's quoted form is not worth the parser.
pub fn token_at(draft: &str, caret: usize) -> Option<Token> {
    let chars: Vec<char> = draft.chars().collect();
    let caret = caret.min(chars.len());
    let mut i = caret;
    while i > 0 {
        let c = chars[i - 1];
        if c == '@' {
            if i >= 2 && !chars[i - 2].is_whitespace() {
                return None;
            }
            return Some(Token {
                start: i - 1,
                end:   caret,
                query: chars[i..caret].iter().collect(),
            });
        }
        if c.is_whitespace() {
            return None;
        }
        i -= 1;
    }
    None
}

/// Caret position in characters. `TextEditState` is the only place egui keeps
/// it; before the field has been focused once there is none, and the end of
/// the draft is the honest guess.
fn caret_char_index(ctx: &egui::Context, draft: &str) -> usize {
    let id = egui::Id::new(INPUT_ID);
    egui::widgets::text_edit::TextEditState::load(ctx, id)
        .and_then(|s| s.cursor.char_range())
        .map(|r| r.primary.index)
        .unwrap_or_else(|| draft.chars().count())
}

/// Replace `token` with `insert`, returning the caret position that follows it.
pub fn splice(draft: &str, token: &Token, insert: &str) -> (String, usize) {
    let head: String = draft.chars().take(token.start).collect();
    let tail: String = draft.chars().skip(token.end).collect();
    let caret = token.start + insert.chars().count();
    (format!("{head}{insert}{tail}"), caret)
}

/// Commit a row: a file closes the picker with a trailing space, a directory
/// stays open on the narrowed query so the next keystroke walks into it.
fn accept(app: &mut App, ui: &mut egui::Ui, token: &Token, path: &str, is_dir: bool) {
    let insert = if is_dir {
        format!("@{path}")
    } else {
        format!("@{path} ")
    };
    let (draft, caret) = splice(&app.chat.draft, token, &insert);
    app.chat.draft = draft;
    set_caret(ui, caret);
    app.chat.at.selected = 0;
    app.chat.at.dismissed = false;
}

/// Park the composer's caret at `char_index` and keep focus on the field.
fn set_caret(ui: &mut egui::Ui, char_index: usize) {
    let id = egui::Id::new(INPUT_ID);
    let ctx = ui.ctx();
    let mut state = egui::widgets::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
    let at = egui::text::CCursor::new(char_index);
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::one(at)));
    state.store(ctx, id);
    ui.memory_mut(|m| m.request_focus(id));
}

// ---------------------------------------------------------------------------
// The index
// ---------------------------------------------------------------------------

/// Take a finished scan, and start one when the index is missing or stale.
/// The old list keeps serving across a refresh — a picker that emptied itself
/// every 30 s would be worse than one that is a few seconds behind.
fn poll_index(app: &mut App) {
    if let Some(rx) = &app.chat.at.scan {
        if let Ok(entries) = rx.try_recv() {
            app.chat.at.entries = entries;
            app.chat.at.scanned_at = Some(Instant::now());
            app.chat.at.scan = None;
        }
    }
    if app.chat.at.scan.is_some() {
        return;
    }
    let stale = match app.chat.at.scanned_at {
        Some(at) => at.elapsed() > INDEX_TTL,
        None => true,
    };
    if stale {
        app.chat.at.scan = Some(spawn_scan());
    }
}

/// Walk the workspace on its own thread and send the result back once.
fn spawn_scan() -> Receiver<Vec<FileEntry>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(scan(&sica_core::paths::working_dir()));
    });
    rx
}

/// Collect relative, `/`-separated paths under `root`, directories included
/// and marked with a trailing slash. Ignored and hidden files are skipped by
/// the walker, which is what keeps `target/` out of the list.
fn scan(root: &std::path::Path) -> Vec<FileEntry> {
    let mut out = Vec::new();
    let walk = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(false)
        .parents(false)
        .max_depth(Some(MAX_DEPTH))
        .build();
    for entry in walk.flatten() {
        if out.len() >= MAX_ENTRIES {
            break;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let is_dir = entry.file_type().map(|f| f.is_dir()).unwrap_or(false);
        let mut path = rel.to_string_lossy().replace('\\', "/");
        if is_dir {
            path.push('/');
        }
        out.push(FileEntry { path, is_dir });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

// ---------------------------------------------------------------------------
// Ranking
// ---------------------------------------------------------------------------

/// One ranked row.
struct Row {
    path:   String,
    is_dir: bool,
}

/// Filter and order the index for `query`, capped at [`MAX_ROWS`].
fn candidates(entries: &[FileEntry], query: &str) -> Vec<Row> {
    let q = query.to_lowercase();
    let mut scored: Vec<(u32, &FileEntry)> = entries
        .iter()
        .filter_map(|e| rank(e, &q).map(|k| (k, e)))
        .collect();
    // Shorter paths first among equals, and a file ahead of a directory that
    // scored the same — dsh lists files first.
    scored.sort_by(|(ka, a), (kb, b)| {
        ka.cmp(kb)
            .then(a.is_dir.cmp(&b.is_dir))
            .then(a.path.len().cmp(&b.path.len()))
            .then(a.path.cmp(&b.path))
    });
    scored
        .into_iter()
        .take(MAX_ROWS)
        .map(|(_, e)| Row {
            path:   e.path.clone(),
            is_dir: e.is_dir,
        })
        .collect()
}

/// Match key for one entry — lower is better, `None` when it doesn't match.
///
/// The file *name* is what a person types, so a name hit always outranks a
/// hit that only landed because the query's characters were scattered down
/// the path.
fn rank(entry: &FileEntry, query_lower: &str) -> Option<u32> {
    if query_lower.is_empty() {
        return Some(0);
    }
    let path = entry.path.to_lowercase();
    let base = path.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string();
    let name = super::slash_menu::fuzzy_score(&base, query_lower);
    let full = super::slash_menu::fuzzy_score(&path, query_lower);
    let score = match (name, full) {
        (Some(n), Some(f)) => n.max(f - PATH_ONLY_PENALTY),
        (Some(n), None) => n,
        (None, Some(f)) => f - PATH_ONLY_PENALTY,
        (None, None) => return None,
    };
    let best = (query_lower.chars().count() as i32) * 12;
    Some((best - score).max(0).min(4_000) as u32)
}

/// How much a path-only hit gives up to a hit on the file name.
const PATH_ONLY_PENALTY: i32 = 8;

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Keys {
    up:      bool,
    down:    bool,
    accept:  bool,
    dismiss: bool,
}

/// What one frame of key input asked the picker to do.
#[derive(Debug, PartialEq, Eq)]
enum Nav {
    Dismissed,
    Moved,
    Accept(usize),
    Idle,
}

/// Claim the navigation keys before the `TextEdit` sees them — otherwise
/// Enter also sends the message and the arrows also move the caret.
fn read_keys(ui: &mut egui::Ui) -> Keys {
    ui.input_mut(|i| {
        let none = egui::Modifiers::NONE;
        Keys {
            down:    i.consume_key(none, egui::Key::ArrowDown),
            up:      i.consume_key(none, egui::Key::ArrowUp),
            accept:  super::consume_enter(i, false)
                | i.consume_key(none, egui::Key::Tab),
            dismiss: i.consume_key(none, egui::Key::Escape),
        }
    })
}

/// Fold one frame of key input into the picker state. `len` must be non-zero;
/// the selection is clamped first, because the list shrinks under the
/// highlight as the query narrows.
fn navigate(selected: &mut usize, dismissed: &mut bool, len: usize, keys: &Keys) -> Nav {
    debug_assert!(len > 0);
    *selected = (*selected).min(len - 1);
    if keys.dismiss {
        *dismissed = true;
        return Nav::Dismissed;
    }
    let mut moved = false;
    if keys.down {
        *selected = (*selected + 1) % len;
        moved = true;
    } else if keys.up {
        *selected = (*selected + len - 1) % len;
        moved = true;
    }
    if keys.accept {
        return Nav::Accept(*selected);
    }
    if moved {
        Nav::Moved
    } else {
        Nav::Idle
    }
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

/// Paint the list. Returns the index of a row the pointer clicked.
fn draw_list(
    app: &App,
    ui: &mut egui::Ui,
    rows: &[Row],
    selected: usize,
    scroll_to_selected: bool,
) -> super::slash_menu::Overlay<Option<usize>> {
    let t = app.theme;
    let mut clicked = None;
    let id = super::slash_menu::overlay_id();
    let out = super::slash_menu::overlay(ui, &t, app.chat.composer_rect, id, |ui| {
        ui.set_width(ui.available_width());
        kit::label(
            ui,
            kit::txt(
                "Files & folders",
                12.0,
                Weight::Medium,
                kit::col(t.alias.label[2]),
            ),
        );
        ui.add_space(2.0);
        egui::ScrollArea::vertical()
            .id_source("at_menu_scroll")
            .max_height(MAX_LIST_HEIGHT)
            .show(ui, |ui| {
                for (i, row) in rows.iter().enumerate() {
                    if draw_row(app, ui, row, i == selected, scroll_to_selected) {
                        clicked = Some(i);
                    }
                }
            });
        ui.add_space(6.0);
        kit::label(
            ui,
            kit::txt(
                "\u{2191}\u{2193} move \u{b7} enter insert \u{b7} esc dismiss",
                11.0,
                Weight::Regular,
                kit::col(t.alias.label[3]),
            ),
        );
    });
    super::slash_menu::Overlay { inner: clicked, outside_press: out.outside_press }
}

/// One row: min-h 40, r=10, `[kind icon] [name] [parent directory]`.
fn draw_row(
    app: &App,
    ui: &mut egui::Ui,
    row: &Row,
    selected: bool,
    scroll_to_selected: bool,
) -> bool {
    let t = app.theme;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 40.0), Sense::click());
    let fill = if selected {
        kit::cola(t.alias.active)
    } else if resp.hovered() {
        kit::cola(t.alias.hover)
    } else {
        Color32::TRANSPARENT
    };
    let painter = ui.painter();
    painter.rect_filled(rect, Rounding::same(RADIUS_ITEM), fill);
    crate::ui::icons::paint(
        painter,
        egui::Rect::from_center_size(
            egui::pos2(rect.min.x + 16.0, rect.center().y),
            Vec2::splat(14.0),
        ),
        if row.is_dir { Icon::Folder } else { Icon::Read },
        kit::col(t.alias.label[2]),
    );
    let (parent, name) = split_path(&row.path);
    let name_font = kit::mono_font(13.0);
    let name_w = painter
        .layout_no_wrap(name.to_string(), name_font.clone(), Color32::WHITE)
        .size()
        .x;
    painter.text(
        egui::pos2(rect.min.x + 30.0, rect.center().y),
        Align2::LEFT_CENTER,
        name,
        name_font,
        kit::col(t.alias.label[0]),
    );
    if !parent.is_empty() {
        let x = rect.min.x + 30.0 + name_w + 8.0;
        let f = kit::font(12.0, Weight::Regular);
        let avail = (rect.max.x - 12.0 - x).max(0.0);
        let shown = kit::elide(ui, parent, &f, avail);
        ui.painter().text(
            egui::pos2(x, rect.center().y),
            Align2::LEFT_CENTER,
            shown,
            f,
            kit::col(t.alias.label[3]),
        );
    }
    if selected && scroll_to_selected {
        resp.scroll_to_me(Some(Align::Center));
    }
    resp.on_hover_text(&row.path).clicked()
}

/// `("crates/frontend/src", "at_menu.rs")` — the parent directory and the
/// last component, with a directory's trailing slash kept on the name.
pub fn split_path(path: &str) -> (&str, &str) {
    let body = path.strip_suffix('/').unwrap_or(path);
    match body.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

/// The list has nothing to show: still "open", because Enter on a half-typed
/// `@` must not send the message.
fn draw_placeholder(app: &mut App, ui: &mut egui::Ui, indexing: bool, query: &str) -> Outcome {
    let t = app.theme;
    let dismiss = ui.input_mut(|i| {
        let none = egui::Modifiers::NONE;
        let _ = super::consume_enter(i, false) | i.consume_key(none, egui::Key::Tab);
        i.consume_key(none, egui::Key::Escape)
    });
    if dismiss {
        app.chat.at.dismissed = true;
        return Outcome { open: false };
    }
    let text = if indexing {
        "indexing the workspace\u{2026}".to_string()
    } else {
        format!("no file matching @{query}")
    };
    let id = super::slash_menu::overlay_id();
    let out = super::slash_menu::overlay(ui, &t, app.chat.composer_rect, id, |ui| {
        ui.set_width(ui.available_width());
        kit::label(
            ui,
            kit::txt(text, 13.0, Weight::Regular, kit::col(t.alias.label[2])),
        );
    });
    if out.outside_press {
        app.chat.at.dismissed = true;
        return Outcome { open: false };
    }
    if indexing {
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }
    Outcome { open: true }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> FileEntry {
        FileEntry {
            is_dir: path.ends_with('/'),
            path:   path.to_string(),
        }
    }

    #[test]
    fn a_token_opens_at_the_start_or_after_whitespace() {
        assert_eq!(
            token_at("@src", 4),
            Some(Token { start: 0, end: 4, query: "src".into() })
        );
        assert_eq!(
            token_at("open @src/main", 14),
            Some(Token { start: 5, end: 14, query: "src/main".into() })
        );
        // An address is not a picker, and neither is a path fragment.
        assert_eq!(token_at("me@example.com", 14), None);
        assert_eq!(token_at("a@b", 3), None);
    }

    #[test]
    fn whitespace_ends_the_token_and_the_caret_bounds_it() {
        assert_eq!(token_at("@src/main.rs and more", 21), None);
        // The caret sits mid-token: only what is behind it is the query.
        assert_eq!(
            token_at("@src/main.rs", 5).map(|t| t.query),
            Some("src/".into())
        );
        assert_eq!(token_at("no token here", 13), None);
    }

    #[test]
    fn a_bare_at_is_a_token_with_an_empty_query() {
        assert_eq!(
            token_at("look at @", 9),
            Some(Token { start: 8, end: 9, query: String::new() })
        );
    }

    #[test]
    fn splice_replaces_only_the_token_and_reports_the_caret() {
        let token = token_at("read @mai and stop", 9).unwrap();
        let (draft, caret) = splice("read @mai and stop", &token, "@src/main.rs ");
        assert_eq!(draft, "read @src/main.rs  and stop");
        assert_eq!(caret, 5 + "@src/main.rs ".chars().count());
    }

    #[test]
    fn a_name_hit_outranks_a_scattered_path_hit() {
        let entries = vec![
            entry("crates/agents/src/memory.rs"),
            entry("docs/harness-ui-guide.md"),
            entry("crates/frontend/src/ui/mod.rs"),
        ];
        let rows = candidates(&entries, "memory");
        assert_eq!(rows[0].path, "crates/agents/src/memory.rs");
    }

    #[test]
    fn an_empty_query_lists_everything_in_path_order() {
        let entries = vec![entry("b.rs"), entry("a.rs")];
        let rows = candidates(&entries, "");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].path, "a.rs");
    }

    #[test]
    fn a_query_that_matches_nothing_yields_nothing() {
        let entries = vec![entry("crates/frontend/src/app.rs")];
        assert!(candidates(&entries, "zqx").is_empty());
    }

    #[test]
    fn a_path_query_finds_the_file_under_that_directory() {
        let entries = vec![
            entry("crates/frontend/src/app.rs"),
            entry("crates/backend/src/app.rs"),
        ];
        let rows = candidates(&entries, "backend/app");
        assert_eq!(rows[0].path, "crates/backend/src/app.rs");
    }

    #[test]
    fn directories_keep_their_slash_and_lose_ties_to_files() {
        let entries = vec![entry("build/"), entry("build")];
        let rows = candidates(&entries, "build");
        assert!(!rows[0].is_dir, "a file wins a tie with a directory");
        assert_eq!(rows[1].path, "build/");
    }

    #[test]
    fn split_path_names_the_parent_and_the_leaf() {
        assert_eq!(split_path("crates/frontend/src/app.rs"), ("crates/frontend/src", "app.rs"));
        assert_eq!(split_path("README.md"), ("", "README.md"));
        assert_eq!(split_path("crates/agents/"), ("crates", "agents/"));
    }

    fn keys(up: bool, down: bool, accept: bool, dismiss: bool) -> Keys {
        Keys { up, down, accept, dismiss }
    }

    #[test]
    fn arrows_wrap_and_esc_dismisses() {
        let (mut sel, mut dis) = (0usize, false);
        assert_eq!(navigate(&mut sel, &mut dis, 2, &keys(true, false, false, false)), Nav::Moved);
        assert_eq!(sel, 1, "up from the first row wraps to the last");
        assert_eq!(navigate(&mut sel, &mut dis, 2, &keys(false, false, false, true)), Nav::Dismissed);
        assert!(dis);
    }

    #[test]
    fn the_selection_is_clamped_when_the_list_shrinks() {
        let (mut sel, mut dis) = (9usize, false);
        assert_eq!(navigate(&mut sel, &mut dis, 3, &keys(false, false, true, false)), Nav::Accept(2));
    }
}
