//! The sidebar — dsh's left column (§2.1, §4), 280 px expanded and 56 px
//! collapsed, on `sidebar_fill`:
//!
//! 1. **Brand row** (h=60): the blade + wordmark, which *is* a New Session
//!    button, and a panel-toggle circle on the right.
//! 2. **New Session** (h=38, r=12, elevated fill, hairline).
//! 3. **Session region** — one row per session: status dot, title, relative
//!    time, and a `⋯` menu on hover.
//! 4. **Foot** — the Settings trigger with the connection indicator inline,
//!    plus sica's own "Rebuild & restart" chip when the source has drifted
//!    (dsh has no equivalent; the hot-reload loop is the app's core feature
//!    and must not hide).
//!
//! The old rail (Chat / Settings) is gone: Settings is a modal now.

use egui::{Align, Align2, Layout, Rect, Sense, Vec2};

use protocol::Request;
use sica_core::theme::tokens::{RADIUS_CARD, RADIUS_INPUT, RADIUS_PILL};

/// Turn rows the outline shows before it starts saying "N older".
/// A long session must not push the session list off the column.
const OUTLINE_ROWS: usize = 12;

use crate::app::App;
use crate::supervisor::UiCommand;
use crate::ui::icons::{self, Icon};
use crate::ui::kit::{self, ConnState, DotState, Level, Weight};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let collapsed = app.layout.sidebar_collapsed;
    folder_error(app, &ui.ctx().clone());
    ui.spacing_mut().item_spacing.y = 6.0;

    brand_row(app, ui, collapsed);
    new_session(app, ui, collapsed);
    ui.add_space(6.0);

    // Foot first (bottom-up), then the session region takes what is left —
    // egui has no `flex: 1`, so the column is laid out from both ends.
    let foot_h = if collapsed { 84.0 } else { 84.0 };
    // Measure against the **clip** rect, not `available_height()`.
    //
    // A panel's content `max_rect` runs past what is actually visible by
    // the frame's own margins — measured here as 6..732 against a 0..720
    // clip — so sizing the session region from `available_height()` put
    // the foot at y=690..732 on a 720 px window: laid out, painted, and
    // entirely below the bottom edge. The Settings button was reachable
    // only by resizing the window taller than the screen. The clip rect
    // is the honest bound, because it is the one painting obeys.
    // The foot is placed at an explicit rect rather than stacked after the
    // session region, and both are measured against the **clip** rect
    // rather than `available_height()`.
    //
    // Two things made the stacked version put the Settings button off
    // screen. A panel's content `max_rect` runs past what is visible by the
    // frame's margins — measured here as 6..732 against a 0..720 clip — and
    // `allocate_ui` does not clip, so the session region overflowed its
    // request by another ~18 px. Between them the foot landed at y=702..744
    // on a 720 px window: laid out, painted, and entirely below the bottom
    // edge, so Settings was unreachable unless the window was dragged
    // taller than the screen. The clip rect is the honest bound because it
    // is the one painting obeys, and an explicit rect cannot be pushed down
    // by whatever the region above it did.
    let top = ui.cursor().top();
    let bottom = ui.clip_rect().max.y;
    let region_h = (bottom - foot_h - top).max(60.0);
    ui.allocate_ui(Vec2::new(ui.available_width(), region_h), |ui| {
        session_region(app, ui, collapsed);
    });
    let foot_rect = Rect::from_min_max(
        egui::pos2(ui.max_rect().min.x, bottom - foot_h),
        egui::pos2(ui.max_rect().max.x, bottom),
    );
    let mut foot_ui = ui.child_ui(foot_rect, Layout::top_down(Align::Min), None);
    foot(app, &mut foot_ui, collapsed);
}

/// "Couldn't open folder" — a `CreateWorkspace` the backend refused, with
/// the host's own reason (a path that is not a directory, or one it cannot
/// read). The only recovery is another folder, so that is what the dialog
/// offers.
fn folder_error(app: &mut App, ctx: &egui::Context) {
    let Some(message) = app.workspaces.error.clone() else { return };
    let t = app.theme;
    let mut again = false;
    let out = kit::modal(
        ctx,
        egui::Id::new("ws_folder_error"),
        "Couldn't open folder",
        420.0,
        true,
        |ui| {
            kit::label(
                ui,
                kit::txt(message, 13.0, Weight::Regular, kit::col(t.alias.label[1])),
            );
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if kit::button(ui, "Choose again", kit::Variant::Primary, kit::Size::Md).clicked() {
                    again = true;
                }
                let _ = kit::button(ui, "Cancel", kit::Variant::Ghost, kit::Size::Md).clicked();
            });
        },
    );
    if out.dismissed || again {
        app.workspaces.error = None;
    }
    if again {
        apply_ws_action(app, ctx, WsAction::Add);
    }
}

// ---------------------------------------------------------------------------
// Brand + New Session
// ---------------------------------------------------------------------------

fn brand_row(app: &mut App, ui: &mut egui::Ui, collapsed: bool) {
    let t = app.theme;
    let h = if collapsed { 36.0 } else { 60.0 };
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::hover());
    let mark_rect = Rect::from_center_size(
        egui::pos2(rect.min.x + 18.0, rect.center().y),
        Vec2::splat(22.0),
    );

    if collapsed {
        // Collapsed: the mark rests here and swaps to the panel glyph on
        // hover — one 36 px hit target for "expand".
        let hit = Rect::from_center_size(rect.center(), Vec2::splat(32.0));
        let resp = ui.interact(hit, ui.id().with("brand_toggle"), Sense::click());
        if resp.hovered() {
            ui.painter()
                .circle_filled(hit.center(), 16.0, kit::cola(t.alias.hover));
            icons::paint(
                ui.painter(),
                Rect::from_center_size(hit.center(), Vec2::splat(16.0)),
                Icon::Panel,
                kit::col(t.alias.label[1]),
            );
        } else {
            icons::mark(
                ui,
                Rect::from_center_size(hit.center(), Vec2::splat(20.0)),
                kit::col(t.alias.label[0]),
            );
        }
        if resp.on_hover_text("Expand sidebar").clicked() {
            app.layout.sidebar_collapsed = false;
            app.layout.narrow_override = Some(false);
        }
        return;
    }

    // Brand = New Session, per dsh.
    let brand_hit = Rect::from_min_max(
        rect.min,
        egui::pos2(rect.max.x - 36.0, rect.max.y),
    );
    let brand = ui.interact(brand_hit, ui.id().with("brand"), Sense::click());
    icons::mark(ui, mark_rect, kit::col(t.alias.label[0]));
    ui.painter().text(
        egui::pos2(mark_rect.max.x + 10.0, rect.center().y),
        Align2::LEFT_CENTER,
        "sica",
        kit::font(18.0, Weight::Semibold),
        kit::col(t.alias.label[0]),
    );
    if brand.on_hover_text("New session").clicked() {
        app.send(UiCommand::SendRequest(Request::NewSession {
            workspace_id: app.workspaces.of_session(app.chat.session_id),
        }));
    }

    // Panel toggle.
    let toggle = Rect::from_center_size(
        egui::pos2(rect.max.x - 18.0, rect.center().y),
        Vec2::splat(28.0),
    );
    let resp = ui.interact(toggle, ui.id().with("panel_toggle"), Sense::click());
    if resp.hovered() {
        ui.painter()
            .circle_filled(toggle.center(), 14.0, kit::cola(t.alias.hover));
    }
    icons::paint(
        ui.painter(),
        Rect::from_center_size(toggle.center(), Vec2::splat(16.0)),
        Icon::Panel,
        kit::col(t.alias.label[1]),
    );
    if resp.on_hover_text("Collapse sidebar").clicked() {
        app.layout.sidebar_collapsed = true;
        app.layout.narrow_override = Some(true);
    }
}

fn new_session(app: &mut App, ui: &mut egui::Ui, collapsed: bool) {
    let t = app.theme;
    if collapsed {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), Sense::click());
        let box_rect = Rect::from_center_size(rect.center(), Vec2::splat(36.0));
        ui.painter().rect(
            box_rect,
            egui::Rounding::same(RADIUS_CARD),
            kit::col(t.alias.elevated_fill),
            egui::Stroke::new(sica_core::theme::tokens::HAIRLINE, Level::L3.color(&t)),
        );
        icons::paint(
            ui.painter(),
            Rect::from_center_size(box_rect.center(), Vec2::splat(14.0)),
            Icon::NewChat,
            kit::col(t.alias.label[0]),
        );
        if resp.on_hover_text("New session").clicked() {
            app.send(UiCommand::SendRequest(Request::NewSession {
            workspace_id: app.workspaces.of_session(app.chat.session_id),
        }));
        }
        return;
    }
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::click());
    let fill = if resp.hovered() {
        kit::over(kit::col(t.alias.elevated_fill), kit::cola(t.alias.hover))
    } else {
        kit::col(t.alias.elevated_fill)
    };
    ui.painter().rect(
        rect,
        egui::Rounding::same(RADIUS_CARD),
        fill,
        egui::Stroke::new(sica_core::theme::tokens::HAIRLINE, Level::L3.color(&t)),
    );
    icons::paint(
        ui.painter(),
        Rect::from_center_size(
            egui::pos2(rect.min.x + 16.0, rect.center().y),
            Vec2::splat(14.0),
        ),
        Icon::NewChat,
        kit::col(t.alias.label[0]),
    );
    ui.painter().text(
        egui::pos2(rect.min.x + 32.0, rect.center().y),
        Align2::LEFT_CENTER,
        "New session",
        kit::font(14.0, Weight::Medium),
        kit::col(t.alias.label[0]),
    );
    if resp.clicked() {
        app.send(UiCommand::SendRequest(Request::NewSession {
            workspace_id: app.workspaces.of_session(app.chat.session_id),
        }));
    }
}

// ---------------------------------------------------------------------------
// Session rows
// ---------------------------------------------------------------------------

enum RowAction {
    Switch(u64),
    Menu(u64, Rect),
    Arm(u64),
    Cancel,
    Confirm(u64),
    /// Open the inline rename field on this row.
    Rename(u64),
    /// Commit the inline rename (empty draft cancels).
    RenameDone(u64),
    /// Leave the rename field without changing the title.
    RenameCancel,
    Fork(u64),
    Archive(u64),
}

fn session_region(app: &mut App, ui: &mut egui::Ui, collapsed: bool) {
    let t = app.theme;
    if collapsed {
        // Collapsed rail: one dot per session, active one ringed.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let rows: Vec<(u64, String)> = app
                    .chat
                    .sessions
                    .iter()
                    .map(|s| (s.id, s.title.clone()))
                    .collect();
                let active = app.chat.session_id;
                let mut switch = None;
                for (id, title) in rows {
                    let (rect, resp) = ui
                        .allocate_exact_size(Vec2::new(ui.available_width(), 28.0), Sense::click());
                    let state = row_dot(app, id);
                    let center = rect.center();
                    if id == active {
                        ui.painter()
                            .circle_filled(center, 12.0, kit::col(t.alias.sidebar_active));
                    } else if resp.hovered() {
                        ui.painter()
                            .circle_filled(center, 12.0, kit::cola(t.alias.hover));
                    }
                    match state {
                        Some(s) => kit::paint_state_dot(
                            ui.painter(),
                            Rect::from_center_size(center, Vec2::splat(10.0)),
                            s,
                            &t,
                            ui.input(|i| i.time) as f32,
                        ),
                        None => {
                            ui.painter()
                                .circle_filled(center, 3.0, kit::col(t.alias.label[3]));
                        }
                    }
                    if resp.on_hover_text(title).clicked() {
                        switch = Some(id);
                    }
                }
                if let Some(id) = switch {
                    app.switch_session(id);
                }
            });
        return;
    }

    // Header row: the list's name + count + the search toggle, which expands
    // into a 30 px field in place of the label (§4.1), plus the workspace
    // affordances (§4.3).
    let mut ws_action: Option<WsAction> = None;
    ui.horizontal(|ui| {
        ui.set_min_height(36.0);
        ui.add_space(8.0);
        if app.chat.search_open {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(4.0);
                if kit::icon_button(ui, Icon::Close, 24.0)
                    .on_hover_text("Close search")
                    .clicked()
                {
                    close_search(app);
                }
                let field = egui::TextEdit::singleline(&mut app.chat.search_query)
                    .hint_text("Search sessions")
                    .desired_width(ui.available_width())
                    .margin(egui::vec2(8.0, 6.0));
                let resp = ui.add(field);
                if resp.changed() {
                    app.chat.search_changed_at = Some(std::time::Instant::now());
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close_search(app);
                }
                // Focus once, when the field opens — not every frame, which
                // would keep stealing it back from whatever else is clicked.
                let focus_id = egui::Id::new("session_search_focused");
                if !ui.ctx().data(|d| d.get_temp::<bool>(focus_id).unwrap_or(false)) {
                    resp.request_focus();
                    ui.ctx().data_mut(|d| d.insert_temp(focus_id, true));
                }
            });
        } else {
            // The label says what the list *is*: grouping is only offered
            // once there is a workspace to group by, so an app that has
            // never registered one still reads "Sessions".
            let grouped = app.workspaces.grouped && !app.workspaces.rows.is_empty();
            kit::label(
                ui,
                kit::txt(
                    if grouped { "Workspaces" } else { "Sessions" },
                    12.0,
                    Weight::Medium,
                    kit::col(t.alias.label[2]),
                ),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(4.0);
                if kit::icon_button(ui, Icon::Search, 24.0)
                    .on_hover_text("Search sessions")
                    .clicked()
                {
                    app.chat.search_open = true;
                    ui.ctx()
                        .data_mut(|d| d.insert_temp(egui::Id::new("session_search_focused"), false));
                }
                let view = kit::icon_button(ui, Icon::Dots, 24.0).on_hover_text("View options");
                if view.clicked() {
                    app.workspaces.view_menu = Some(view.rect);
                }
                if kit::icon_button(ui, Icon::Folder, 24.0)
                    .on_hover_text("Add workspace")
                    .clicked()
                {
                    ws_action = Some(WsAction::Add);
                }
                kit::label(
                    ui,
                    kit::txt(
                        format!("{}", app.chat.sessions.len()),
                        12.0,
                        Weight::Regular,
                        kit::col(t.alias.label[3]),
                    ),
                );
            });
        }
    });
    ui.add_space(2.0);
    // The host content search is debounced behind the immediate title filter,
    // so every keystroke does not become a full scan of every log.
    search_tick(app);

    if app.chat.sessions.is_empty() {
        kit::label(
            ui,
            kit::txt(
                if app.ipc_state.connected {
                    "Loading sessions…"
                } else {
                    "Start the backend to load sessions."
                },
                13.0,
                Weight::Regular,
                kit::col(t.alias.label[2]),
            ),
        );
        return;
    }

    let query = app.chat.search_query.trim().to_lowercase();
    let rows: Vec<(u64, String, i64, String)> = app
        .chat
        .sessions
        .iter()
        .filter(|s| {
            if query.is_empty() {
                return true;
            }
            s.title.to_lowercase().contains(&query)
                || app.chat.search_hits.iter().any(|h| h.id == s.id)
        })
        .map(|s| {
            // A row that only matched on content shows the line it matched
            // on; a title match needs no explaining.
            let snippet = app
                .chat
                .search_hits
                .iter()
                .find(|h| h.id == s.id)
                .map(|h| h.snippet.clone())
                .filter(|_| !query.is_empty() && !s.title.to_lowercase().contains(&query))
                .unwrap_or_default();
            (s.id, s.title.clone(), s.updated_at.max(s.created_at), snippet)
        })
        .collect();
    let active_id = app.chat.session_id;
    let grouped = app.workspaces.grouped && !app.workspaces.rows.is_empty();
    let by_updated = app.workspaces.by_updated;
    let pending = app.chat.pending_delete;
    let allow_delete = rows.len() > 1;
    let mut action: Option<RowAction> = None;

    let scroll_rect = ui.available_rect_before_wrap();
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            if rows.is_empty() {
                kit::label(
                    ui,
                    kit::txt(
                        "No sessions match.",
                        13.0,
                        Weight::Regular,
                        kit::col(t.alias.label[2]),
                    ),
                );
            }
            // One closure draws a session row wherever it lands — flat, or
            // inside a group — so the two modes cannot drift apart.
            let session_row = |app: &mut App,
                                   ui: &mut egui::Ui,
                                   row: &(u64, String, i64, String),
                                   action: &mut Option<RowAction>| {
                let (id, title, updated, snippet) = row;
                let dot = row_dot(app, *id);
                if app.chat.renaming == Some(*id) {
                    rename_row(app, ui, *id, &mut |a| *action = Some(a));
                    return;
                }
                draw_row(
                    app,
                    ui,
                    *id,
                    title,
                    *updated,
                    dot,
                    *id == active_id,
                    allow_delete,
                    pending == Some(*id),
                    &mut |a| *action = Some(a),
                );
                if *id == active_id {
                    turn_outline(app, ui);
                }
                if !snippet.is_empty() {
                    ui.horizontal(|ui| {
                        ui.add_space(26.0);
                        kit::label(
                            ui,
                            kit::txt(
                                kit::one_line(snippet, 60),
                                12.0,
                                Weight::Regular,
                                kit::col(t.alias.label[3]),
                            ),
                        );
                    });
                }
            };

            if grouped {
                // The projection is the backend's, order included; the only
                // thing decided here is what is folded shut.
                let groups: Vec<GroupView> = app
                    .workspaces
                    .rows
                    .iter()
                    .map(|w| GroupView {
                        id:      Some(w.id),
                        title:   w.title.clone(),
                        path:    Some(w.path.clone()),
                        created: w.created_at,
                        missing: w.missing,
                        members: w.sessions.clone(),
                    })
                    .chain(
                        (!app.workspaces.ungrouped.is_empty()).then(|| GroupView {
                            id:      None,
                            title:   "Ungrouped".into(),
                            path:    None,
                            created: 0,
                            missing: false,
                            members: app.workspaces.ungrouped.clone(),
                        }),
                    )
                    .collect();

                for group in &groups {
                    // Only the sessions the search left standing, in the
                    // group's own order — or by recency, when that is what
                    // the user asked for.
                    let mut members: Vec<&(u64, String, i64, String)> = group
                        .members
                        .iter()
                        .filter_map(|sid| rows.iter().find(|r| r.0 == *sid))
                        .collect();
                    if by_updated || group.id.is_none() {
                        members.sort_by_key(|r| std::cmp::Reverse(r.2));
                    }
                    if members.is_empty() && !query.is_empty() {
                        continue;
                    }
                    let open = match group.id {
                        Some(id) => !app.workspaces.collapsed.contains(&id),
                        None => true,
                    };
                    group_row(app, ui, group, open, members.len(), &mut |a| {
                        ws_action = Some(a)
                    });
                    if !open {
                        continue;
                    }
                    // The active session is never folded away: the user is
                    // working in it, and a fold that hides the selection
                    // reads as the session having disappeared.
                    let show_all = group
                        .id
                        .is_none_or(|id| app.workspaces.show_all.contains(&id))
                        || members.iter().skip(GROUP_FOLD).any(|r| r.0 == active_id);
                    let visible = if show_all { members.len() } else { members.len().min(GROUP_FOLD) };
                    ui.scope(|ui| {
                        ui.visuals_mut().indent_has_left_vline = false;
                        ui.spacing_mut().indent = 10.0;
                        ui.indent(("ws_group", group.id), |ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            for row in &members[..visible] {
                                session_row(app, ui, row, &mut action);
                            }
                            if let Some(id) = group.id {
                                if members.len() > visible {
                                    let n = members.len() - visible;
                                    if fold_button(ui, t, id, &format!("Show {n} more sessions")) {
                                        ws_action = Some(WsAction::ShowAll(id));
                                    }
                                } else if show_all && members.len() > GROUP_FOLD
                                    && fold_button(ui, t, id, "Show less")
                                {
                                    ws_action = Some(WsAction::ShowLess(id));
                                }
                            }
                        });
                    });
                }
            } else {
                for row in &rows {
                    session_row(app, ui, row, &mut action);
                }
            }
            ui.add_space(20.0);
        });

    // 24 px bottom fade to `sidebar_fill`, so rows dissolve rather than clip.
    fade(ui, scroll_rect, kit::col(t.alias.sidebar_fill));

    // The row menu is an overlay so it can spill past the column.
    if let Some((id, rect)) = app.chat.row_menu {
        let items = vec![
            kit::MenuItem::new("Open"),
            kit::MenuItem::new("Rename"),
            kit::MenuItem::new("Fork session")
                .detail("Copy the completed turns into a new session"),
            kit::MenuItem::new("Archive session").detail("Hides the row; the log stays on disk"),
            kit::MenuItem::new("Copy title"),
            kit::MenuItem::new("Delete session")
                .danger(true)
                .sep_above(true),
        ];
        let mut open = true;
        let picked = kit::menu(
            ui.ctx(),
            egui::Id::new(("row_menu", id)),
            rect,
            kit::MenuSide::Below,
            180.0,
            &items,
            &mut open,
        );
        match picked {
            Some(0) => action = Some(RowAction::Switch(id)),
            Some(1) => action = Some(RowAction::Rename(id)),
            Some(2) => action = Some(RowAction::Fork(id)),
            Some(3) => action = Some(RowAction::Archive(id)),
            Some(4) => {
                if let Some(s) = app.chat.sessions.iter().find(|s| s.id == id) {
                    let title = s.title.clone();
                    ui.ctx().output_mut(|o| o.copied_text = title);
                }
            }
            Some(5) => action = Some(RowAction::Arm(id)),
            _ => {}
        }
        if !open {
            app.chat.row_menu = None;
        }
    }

    // The workspace ⋯ menu, and the header's View options. Both are
    // overlays for the same reason the row menu is: they spill past the
    // column.
    if let Some((id, rect)) = app.workspaces.menu {
        let missing = app
            .workspaces
            .rows
            .iter()
            .find(|w| w.id == id)
            .is_some_and(|w| w.missing);
        let items = vec![
            kit::MenuItem::new("New session here"),
            kit::MenuItem::new("Rename workspace"),
            kit::MenuItem::new("Copy path"),
            kit::MenuItem::new("Move up").sep_above(true),
            kit::MenuItem::new("Move down"),
            kit::MenuItem::new("Add workspace…").sep_above(true),
            kit::MenuItem::new("Remove workspace")
                .detail("Keeps the folder and every session log")
                .danger(true)
                .sep_above(true),
        ];
        let mut open = true;
        let picked = kit::menu(
            ui.ctx(),
            egui::Id::new(("ws_menu", id)),
            rect,
            kit::MenuSide::Below,
            200.0,
            &items,
            &mut open,
        );
        match picked {
            Some(0) if !missing => ws_action = Some(WsAction::NewSession(id)),
            Some(1) => ws_action = Some(WsAction::Rename(id)),
            Some(2) => ws_action = Some(WsAction::CopyPath(id)),
            Some(3) => ws_action = Some(WsAction::Move(id, true)),
            Some(4) => ws_action = Some(WsAction::Move(id, false)),
            Some(5) => ws_action = Some(WsAction::Add),
            Some(6) => ws_action = Some(WsAction::Arm(id)),
            _ => {}
        }
        if !open {
            app.workspaces.menu = None;
        }
    }
    if let Some(rect) = app.workspaces.view_menu {
        let grouped_now = app.workspaces.grouped;
        let by_updated = app.workspaces.by_updated;
        let items = vec![
            kit::MenuItem::new("Group by workspace").checked(grouped_now),
            kit::MenuItem::new("One flat list").checked(!grouped_now),
            kit::MenuItem::new("Order by last updated")
                .checked(by_updated)
                .sep_above(true),
            kit::MenuItem::new("Order manually")
                .detail("The order Move up / Move down sets, kept by the backend")
                .checked(!by_updated),
            kit::MenuItem::new("Add workspace…").sep_above(true),
        ];
        let mut open = true;
        let picked = kit::menu(
            ui.ctx(),
            egui::Id::new("ws_view_menu"),
            rect,
            kit::MenuSide::Below,
            200.0,
            &items,
            &mut open,
        );
        match picked {
            Some(0) => ws_action = Some(WsAction::Grouped(true)),
            Some(1) => ws_action = Some(WsAction::Grouped(false)),
            Some(2) => ws_action = Some(WsAction::ByUpdated(true)),
            Some(3) => ws_action = Some(WsAction::ByUpdated(false)),
            Some(4) => ws_action = Some(WsAction::Add),
            _ => {}
        }
        if !open {
            app.workspaces.view_menu = None;
        }
    }

    if let Some(a) = ws_action {
        apply_ws_action(app, &ui.ctx().clone(), a);
    }

    match action {
        Some(RowAction::Switch(id)) => {
            app.chat.row_menu = None;
            app.switch_session(id);
        }
        Some(RowAction::Menu(id, rect)) => app.chat.row_menu = Some((id, rect)),
        Some(RowAction::Arm(id)) => {
            app.chat.row_menu = None;
            app.chat.pending_delete = Some(id);
        }
        Some(RowAction::Cancel) => app.chat.pending_delete = None,
        Some(RowAction::Confirm(id)) => app.delete_session(id),
        Some(RowAction::Rename(id)) => {
            app.chat.row_menu = None;
            app.chat.rename_draft = app
                .chat
                .sessions
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.title.clone())
                .unwrap_or_default();
            app.chat.renaming = Some(id);
        }
        Some(RowAction::RenameDone(id)) => {
            let title = app.chat.rename_draft.trim().to_string();
            app.chat.renaming = None;
            if !title.is_empty() {
                if let Some(s) = app.chat.sessions.iter_mut().find(|s| s.id == id) {
                    s.title = title.clone();
                }
                app.send(UiCommand::SendRequest(protocol::Request::RenameSession {
                    session_id: id,
                    title,
                }));
            }
        }
        Some(RowAction::RenameCancel) => app.chat.renaming = None,
        Some(RowAction::Fork(id)) => {
            app.chat.row_menu = None;
            // The BE answers `SessionCreated`, which switches to the fork;
            // the list is refreshed so the row carries its real title.
            app.send(UiCommand::SendRequest(protocol::Request::ForkSession {
                session_id: id,
            }));
            app.send(UiCommand::SendRequest(protocol::Request::ListSessions));
        }
        Some(RowAction::Archive(id)) => {
            app.chat.row_menu = None;
            app.chat.sessions.retain(|s| s.id != id);
            app.send(UiCommand::SendRequest(protocol::Request::ArchiveSession {
                session_id: id,
            }));
            if app.chat.session_id == id {
                if let Some(next) = app.chat.sessions.first().map(|s| s.id) {
                    app.switch_session(next);
                }
            }
        }
        None => {}
    }
}


/// How many sessions a group shows before "Show {n} more sessions".
const GROUP_FOLD: usize = 5;

/// One group as the sidebar draws it. `id: None` is **Ungrouped** — the
/// sessions whose directory belongs to no registered workspace, plus every
/// session written before sessions had a directory at all.
struct GroupView {
    id:      Option<u64>,
    title:   String,
    path:    Option<std::path::PathBuf>,
    created: i64,
    missing: bool,
    members: Vec<u64>,
}

/// What a workspace row asks the sidebar to do. Same shape as `RowAction`:
/// the row draws, the caller mutates, so nothing borrows `App` twice.
#[derive(Clone)]
enum WsAction {
    Toggle(u64),
    Menu(u64, Rect),
    NewSession(u64),
    ShowAll(u64),
    ShowLess(u64),
    Rename(u64),
    RenameDone(u64),
    RenameCancel,
    Arm(u64),
    CancelDelete,
    ConfirmDelete(u64),
    /// `true` moves it one place up the list.
    Move(u64, bool),
    CopyPath(u64),
    Add,
    Grouped(bool),
    /// Order sessions inside a group by recency (`true`) or by the
    /// backend's manual order.
    ByUpdated(bool),
}

/// The 34 px group header: `[chevron] [folder] [title] … [⋯] [+]`.
///
/// A workspace whose folder is gone still draws — the registration outlives
/// the directory (harness §3.9) — but says so in `warn` and refuses to start
/// a session it could not run in.
fn group_row(
    app: &mut App,
    ui: &mut egui::Ui,
    group: &GroupView,
    open: bool,
    count: usize,
    on_action: &mut dyn FnMut(WsAction),
) {
    let t = app.theme;
    if let Some(wid) = group.id {
        if app.workspaces.renaming == Some(wid) {
            ws_rename_row(app, ui, wid, on_action);
            return;
        }
    }
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::click());
    let hovered = ui.rect_contains_pointer(rect);
    if !ui.is_rect_visible(rect) {
        return;
    }
    if hovered {
        ui.painter().rect_filled(
            rect,
            egui::Rounding::same(8.0),
            kit::col(t.alias.sidebar_hover),
        );
    }

    // The fold state is the row's primary meaning, so the chevron leads;
    // Ungrouped has no folder of its own to show.
    let mut x = rect.min.x + 6.0;
    icons::paint(
        ui.painter(),
        Rect::from_center_size(egui::pos2(x + 8.0, rect.center().y), Vec2::splat(14.0)),
        if open { Icon::ChevronDown } else { Icon::ChevronRight },
        kit::col(t.alias.label[3]),
    );
    x += 20.0;
    if group.id.is_some() {
        icons::paint(
            ui.painter(),
            Rect::from_center_size(egui::pos2(x + 8.0, rect.center().y), Vec2::splat(15.0)),
            Icon::Folder,
            kit::col(if group.missing { t.alias.warn } else { t.alias.label[2] }),
        );
        x += 22.0;
    }

    if group.id.is_some() && app.workspaces.pending_delete == group.id {
        // The same two-step the session rows use: the row itself becomes the
        // question, and the retention promise is the tooltip.
        ui.painter().text(
            egui::pos2(x, rect.center().y),
            Align2::LEFT_CENTER,
            "Remove from the list?",
            kit::font(13.0, Weight::Regular),
            kit::col(t.alias.label[0]),
        );
        let keep = word_button(ui, rect.max.x - 8.0, rect.center().y, "Keep", false, &t);
        let del = word_button(ui, keep.0.min.x - 8.0, rect.center().y, "Remove", true, &t);
        ui.interact(del.0, ui.id().with(("ws_del_tip", group.id)), Sense::hover())
            .on_hover_text(
                "The folder and every session log are kept. Its sessions move to Ungrouped.",
            );
        if del.1 {
            if let Some(wid) = group.id {
                on_action(WsAction::ConfirmDelete(wid));
            }
        } else if keep.1 || resp.clicked() {
            on_action(WsAction::CancelDelete);
        }
        return;
    }

    let title_col = if group.missing { t.alias.warn } else { t.alias.label[1] };
    let font = kit::font(14.0, Weight::Medium);
    let trailing = if hovered && group.id.is_some() { 62.0 } else { 34.0 };
    let shown = kit::elide(ui, &group.title, &font, (rect.max.x - trailing - x).max(24.0));
    ui.painter().text(
        egui::pos2(x, rect.center().y),
        Align2::LEFT_CENTER,
        &shown,
        font,
        kit::col(title_col),
    );

    if hovered && group.id.is_some() {
        let wid = group.id.expect("checked");
        // Hover swaps the count for the two actions, the way a session row
        // swaps its timestamp for the ⋯.
        let plus = Rect::from_center_size(
            egui::pos2(rect.max.x - 16.0, rect.center().y),
            Vec2::splat(26.0),
        );
        let dots = Rect::from_center_size(
            egui::pos2(rect.max.x - 44.0, rect.center().y),
            Vec2::splat(26.0),
        );
        for (r, icon, id_tag) in
            [(plus, Icon::Plus, "ws_plus"), (dots, Icon::Dots, "ws_dots")]
        {
            let hit = ui.interact(r, ui.id().with((id_tag, wid)), Sense::click());
            let dimmed = icon == Icon::Plus && group.missing;
            if hit.hovered() && !dimmed {
                ui.painter().circle_filled(r.center(), 13.0, kit::cola(t.alias.hover));
            }
            icons::paint(
                ui.painter(),
                Rect::from_center_size(r.center(), Vec2::splat(14.0)),
                icon,
                kit::col(if dimmed { t.alias.label[3] } else { t.alias.label[1] }),
            );
            let hit = match (icon, dimmed) {
                (Icon::Plus, true) => {
                    hit.on_hover_text("Folder not found — cannot start a session here")
                }
                (Icon::Plus, false) => {
                    hit.on_hover_text(format!("New session in {}", group.title))
                }
                _ => hit.on_hover_text(format!("Workspace actions for {}", group.title)),
            };
            if hit.clicked() {
                match icon {
                    Icon::Plus if !dimmed => on_action(WsAction::NewSession(wid)),
                    Icon::Dots => on_action(WsAction::Menu(wid, r)),
                    _ => {}
                }
                return;
            }
        }
    } else {
        ui.painter().text(
            egui::pos2(rect.max.x - 8.0, rect.center().y),
            Align2::RIGHT_CENTER,
            format!("{count}"),
            kit::font(12.0, Weight::Regular),
            kit::col(t.alias.label[3]),
        );
    }

    // The hover card: where it is, and since when. `~` for the home
    // directory — a sidebar is not the place for a full user path.
    let resp = match (&group.path, group.missing) {
        (Some(path), true) => resp.on_hover_text(format!(
            "{}\nFolder not found — its sessions still open\nCreated {}",
            tilde(path),
            relative_time(group.created)
        )),
        (Some(path), false) => resp.on_hover_text(format!(
            "{}\nCreated {}",
            tilde(path),
            relative_time(group.created)
        )),
        (None, _) => resp.on_hover_text("Sessions whose folder is not a registered workspace"),
    };
    if resp.clicked() {
        if let Some(wid) = group.id {
            on_action(WsAction::Toggle(wid));
        }
    }
}

/// Inline rename, the same shape as the session rename row: the title
/// becomes a field in place, Enter commits, Escape cancels.
fn ws_rename_row(app: &mut App, ui: &mut egui::Ui, id: u64, on_action: &mut dyn FnMut(WsAction)) {
    ui.horizontal(|ui| {
        ui.set_min_height(34.0);
        ui.add_space(26.0);
        let field = egui::TextEdit::singleline(&mut app.workspaces.rename_draft)
            .hint_text("Workspace name")
            .desired_width(ui.available_width() - 8.0)
            .margin(egui::vec2(6.0, 5.0));
        let resp = ui.add(field);
        if !resp.has_focus() && !resp.lost_focus() {
            resp.request_focus();
        }
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            on_action(WsAction::RenameCancel);
        } else if resp.lost_focus() {
            on_action(WsAction::RenameDone(id));
        }
    });
}

/// `~/project` for a path under the home directory.
pub fn tilde(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    match text.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => text,
    }
}

/// A left-aligned text button for the group fold. `word_button` is
/// right-anchored and painter-placed for the confirm rows; this is the plain
/// little link the fold wants.
fn fold_button(ui: &mut egui::Ui, t: sica_core::theme::Theme, id: u64, text: &str) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::click());
    if !ui.is_rect_visible(rect) {
        return false;
    }
    let hovered = resp.hovered();
    ui.painter().text(
        egui::pos2(rect.min.x + 26.0, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        kit::font(12.0, Weight::Medium),
        kit::col(if hovered { t.alias.label[0] } else { t.alias.label[2] }),
    );
    let _ = id;
    resp.clicked()
}

/// Everything a workspace row or menu can ask for.
///
/// Membership and order belong to the backend, so anything durable is a
/// request and the answer (`WorkspacesChanged`) is what updates the view —
/// nothing here edits the projection in place and hopes the backend agrees.
fn apply_ws_action(app: &mut App, ctx: &egui::Context, action: WsAction) {
    use protocol::Request;
    match action {
        WsAction::Toggle(id) => {
            if !app.workspaces.collapsed.remove(&id) {
                app.workspaces.collapsed.insert(id);
            }
        }
        WsAction::Menu(id, rect) => app.workspaces.menu = Some((id, rect)),
        WsAction::NewSession(id) => {
            app.workspaces.menu = None;
            app.send(UiCommand::SendRequest(Request::NewSession {
                workspace_id: Some(id),
            }));
        }
        WsAction::ShowAll(id) => {
            app.workspaces.show_all.insert(id);
        }
        WsAction::ShowLess(id) => {
            app.workspaces.show_all.remove(&id);
        }
        WsAction::Rename(id) => {
            app.workspaces.menu = None;
            app.workspaces.rename_draft = app
                .workspaces
                .rows
                .iter()
                .find(|w| w.id == id)
                .map(|w| w.title.clone())
                .unwrap_or_default();
            app.workspaces.renaming = Some(id);
        }
        WsAction::RenameDone(id) => {
            let title = app.workspaces.rename_draft.trim().to_string();
            app.workspaces.renaming = None;
            if !title.is_empty() {
                app.send(UiCommand::SendRequest(Request::RenameWorkspace { id, title }));
            }
        }
        WsAction::RenameCancel => app.workspaces.renaming = None,
        WsAction::Arm(id) => {
            app.workspaces.menu = None;
            app.workspaces.pending_delete = Some(id);
        }
        WsAction::CancelDelete => app.workspaces.pending_delete = None,
        WsAction::ConfirmDelete(id) => {
            app.workspaces.pending_delete = None;
            app.send(UiCommand::SendRequest(Request::DeleteWorkspace { id }));
        }
        WsAction::Move(id, up) => {
            app.workspaces.menu = None;
            // egui has no list drag-and-drop, so Move up / down is what the
            // reorder verb gets driven by. `before` is the row it should
            // land in front of; moving down means jumping the one after it.
            let order: Vec<u64> = app.workspaces.rows.iter().map(|w| w.id).collect();
            let Some(at) = order.iter().position(|x| *x == id) else { return };
            let before = if up {
                if at == 0 {
                    return;
                }
                Some(order[at - 1])
            } else {
                order.get(at + 2).copied()
            };
            if !up && at + 1 >= order.len() {
                return;
            }
            app.send(UiCommand::SendRequest(Request::MoveWorkspace { id, before }));
        }
        WsAction::CopyPath(id) => {
            app.workspaces.menu = None;
            if let Some(w) = app.workspaces.rows.iter().find(|w| w.id == id) {
                let path = w.path.display().to_string();
                ctx.output_mut(|o| o.copied_text = path);
                app.show_toast(Icon::Check, "Path copied", 2000);
            }
        }
        WsAction::Add => {
            app.workspaces.menu = None;
            app.workspaces.view_menu = None;
            // The native chooser, the same one Settings › General uses. FE
            // and BE always share a machine, so dsh's in-app browse dialog
            // has nothing to decide here.
            let start = sica_core::paths::working_dir();
            if let Some(dir) = rfd::FileDialog::new().set_directory(&start).pick_folder() {
                let path = dir.display().to_string();
                // Held until the answer: a refusal has to name the folder
                // the user actually picked, and the id is the backend's.
                app.workspaces.creating = Some(path.clone());
                app.send(UiCommand::SendRequest(Request::CreateWorkspace {
                    path,
                    title: None,
                }));
            }
        }
        WsAction::Grouped(on) => {
            app.workspaces.view_menu = None;
            app.workspaces.grouped = on;
            app.persist_settings();
        }
        WsAction::ByUpdated(on) => {
            app.workspaces.view_menu = None;
            app.workspaces.by_updated = on;
            app.persist_settings();
        }
    }
}

/// The turn outline under the active session's row (guide §3.3): one line
/// per turn, click jumps the Trajectory ledger to that turn's `TurnStart`.
///
/// Folded from the log, not from `chat.turns` — the transcript holds only
/// the derived surface, so a turn compaction shadowed has no row there and
/// would silently vanish from a list built off it. Collapsed by default:
/// the sidebar's subject is sessions, and this is a drill-down into the one
/// that is open.
fn turn_outline(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    if app.stats.outline.is_empty() {
        return;
    }
    let n = app.stats.outline.len();
    let expanded = app.stats.expanded;

    // Disclosure row: "▸ 7 turns".
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::click());
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, egui::Rounding::same(RADIUS_INPUT), kit::cola(t.alias.hover));
    }
    ui.painter().text(
        egui::pos2(rect.min.x + 26.0, rect.center().y),
        Align2::LEFT_CENTER,
        format!("{} {} turn{}", if expanded { "▾" } else { "▸" }, n, if n == 1 { "" } else { "s" }),
        kit::font(12.0, Weight::Medium),
        kit::col(t.alias.label[2]),
    );
    if resp.clicked() {
        app.stats.expanded = !expanded;
    }
    if !expanded {
        return;
    }

    // Newest first: the turn a user wants to jump back to is almost always
    // a recent one, and the list is capped so a long session cannot push
    // the session rows off the column.
    let rows: Vec<(u64, String, String, bool)> = app
        .stats
        .outline
        .iter()
        .rev()
        .take(OUTLINE_ROWS)
        .map(|r| {
            let label = if r.first_user_line.is_empty() {
                format!("({})", r.source)
            } else {
                r.first_user_line.clone()
            };
            (r.start_seq, label, r.finish_reason.clone(), r.finish_reason.is_empty())
        })
        .collect();
    let hidden = n.saturating_sub(rows.len());
    let mut jump = None;
    for (start_seq, label, finish, running) in rows {
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 22.0), Sense::click());
        if !ui.is_rect_visible(rect) {
            continue;
        }
        if resp.hovered() {
            ui.painter().rect_filled(
                rect,
                egui::Rounding::same(RADIUS_INPUT),
                kit::col(t.alias.sidebar_hover),
            );
        }
        // A turn still running gets the ongoing dot; a finished one a plain
        // tick mark, so the list says which end of it you are looking at.
        let dot = egui::pos2(rect.min.x + 32.0, rect.center().y);
        ui.painter().circle_filled(
            dot,
            3.0,
            if running { kit::col(t.alias.business) } else { kit::col(t.alias.label[3]) },
        );
        let font = kit::font(12.0, Weight::Regular);
        let avail = rect.width() - 48.0;
        let shown = kit::elide(ui, &label, &font, avail.max(40.0));
        ui.painter().text(
            egui::pos2(rect.min.x + 42.0, rect.center().y),
            Align2::LEFT_CENTER,
            shown,
            font,
            kit::col(t.alias.label[if running { 1 } else { 2 }]),
        );
        let tip = if finish.is_empty() {
            format!("{label}\nrunning · jump to the ledger")
        } else {
            format!("{label}\nfinished: {finish} · jump to the ledger")
        };
        if resp.on_hover_text(tip).clicked() {
            jump = Some(start_seq);
        }
    }
    if hidden > 0 {
        ui.horizontal(|ui| {
            ui.add_space(42.0);
            kit::label(
                ui,
                kit::txt(
                    format!("{hidden} older"),
                    11.0,
                    Weight::Regular,
                    kit::col(t.alias.label[3]),
                ),
            );
        });
    }
    if let Some(seq) = jump {
        app.inspect_event(seq);
    }
}

fn close_search(app: &mut App) {
    app.chat.search_open = false;
    app.chat.search_query.clear();
    app.chat.search_hits.clear();
    app.chat.search_sent.clear();
    app.chat.search_changed_at = None;
}

/// dsh's 250 ms debounce: title matches filter the list on every keystroke,
/// the backend's content scan waits for the typing to settle.
fn search_tick(app: &mut App) {
    const DEBOUNCE_MS: u128 = 250;
    let Some(changed) = app.chat.search_changed_at else { return };
    if changed.elapsed().as_millis() < DEBOUNCE_MS {
        return;
    }
    app.chat.search_changed_at = None;
    let query = app.chat.search_query.trim().to_string();
    if query == app.chat.search_sent {
        return;
    }
    app.chat.search_sent = query.clone();
    if query.is_empty() {
        app.chat.search_hits.clear();
        return;
    }
    app.send(UiCommand::SendRequest(protocol::Request::SearchSessions { query }));
}

/// The row while it is being renamed: the title becomes a field in place.
/// Enter commits, Escape and a click elsewhere cancel.
fn rename_row(app: &mut App, ui: &mut egui::Ui, id: u64, on_action: &mut dyn FnMut(RowAction)) {
    ui.horizontal(|ui| {
        ui.set_min_height(32.0);
        ui.add_space(22.0);
        let field = egui::TextEdit::singleline(&mut app.chat.rename_draft)
            .desired_width(ui.available_width() - 8.0)
            .margin(egui::vec2(6.0, 5.0));
        let resp = ui.add(field);
        if !resp.has_focus() && !resp.lost_focus() {
            resp.request_focus();
        }
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            on_action(RowAction::RenameCancel);
        } else if resp.lost_focus() {
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                on_action(RowAction::RenameDone(id));
            } else {
                on_action(RowAction::RenameCancel);
            }
        }
    });
}

/// Status precedence (§4.1): waiting on a human → running → completed while
/// away → nothing at all.
fn row_dot(app: &App, id: u64) -> Option<DotState> {
    if app.chat.waiting_sessions.contains(&id) {
        Some(DotState::Warning)
    } else if app.chat.running_sessions.contains(&id) {
        Some(DotState::Ongoing)
    } else if app.chat.unseen_sessions.contains(&id) {
        Some(DotState::Done)
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_row(
    app: &App,
    ui: &mut egui::Ui,
    id: u64,
    title: &str,
    created_at: i64,
    dot: Option<DotState>,
    is_active: bool,
    allow_delete: bool,
    is_pending: bool,
    on_action: &mut dyn FnMut(RowAction),
) {
    let t = app.theme;
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
    // Hover is tested against the rect: the trailing ⋯ registers its own
    // interact rect on top of the row and would otherwise steal it.
    let hovered = ui.rect_contains_pointer(rect);
    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter();
    if is_active {
        painter.rect_filled(
            rect,
            egui::Rounding::same(RADIUS_INPUT),
            kit::col(t.alias.sidebar_active),
        );
    } else if hovered {
        painter.rect_filled(
            rect,
            egui::Rounding::same(RADIUS_INPUT),
            kit::col(t.alias.sidebar_hover),
        );
    }

    // 16 px status slot.
    if let Some(state) = dot {
        kit::paint_state_dot(
            painter,
            Rect::from_center_size(
                egui::pos2(rect.min.x + 16.0, rect.center().y),
                Vec2::splat(10.0),
            ),
            state,
            &t,
            ui.input(|i| i.time) as f32,
        );
        if state == DotState::Ongoing {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(120));
        }
    }

    if is_pending {
        // Armed delete keeps sica's two-step confirm — the log file is the
        // user's, and dsh's "no delete at all" is a hosted-product decision.
        let mut confirm = None;
        ui.painter().text(
            egui::pos2(rect.min.x + 26.0, rect.center().y),
            Align2::LEFT_CENTER,
            "Delete this session?",
            kit::font(13.0, Weight::Regular),
            kit::col(t.alias.label[0]),
        );
        let keep = word_button(ui, rect.max.x - 8.0, rect.center().y, "Keep", false, &t);
        let del = word_button(ui, keep.0.min.x - 8.0, rect.center().y, "Delete", true, &t);
        if del.1 {
            confirm = Some(RowAction::Confirm(id));
        } else if keep.1 || resp.clicked() {
            confirm = Some(RowAction::Cancel);
        }
        if let Some(a) = confirm {
            on_action(a);
        }
        return;
    }

    // Title + relative time.
    let time_text = relative_time(created_at);
    let time_font = kit::font(12.0, Weight::Regular);
    let time_w = ui
        .fonts(|f| f.layout_no_wrap(time_text.clone(), time_font.clone(), egui::Color32::WHITE))
        .size()
        .x;
    let trailing = if hovered && allow_delete { 28.0 } else { time_w + 10.0 };
    let title_font = kit::font(14.0, Weight::Regular);
    let title_max = (rect.width() - 26.0 - trailing - 8.0).max(20.0);
    let shown = kit::elide(ui, title, &title_font, title_max);
    ui.painter().text(
        egui::pos2(rect.min.x + 26.0, rect.center().y),
        Align2::LEFT_CENTER,
        &shown,
        title_font,
        if is_active {
            kit::col(t.alias.label[0])
        } else {
            kit::col(t.alias.label[1])
        },
    );

    if hovered && allow_delete {
        // Hover swaps the timestamp for the ⋯ menu, exactly like dsh.
        let dots = Rect::from_center_size(
            egui::pos2(rect.max.x - 16.0, rect.center().y),
            Vec2::splat(24.0),
        );
        let dots_resp = ui.interact(dots, ui.id().with(("row_dots", id)), Sense::click());
        if dots_resp.hovered() {
            ui.painter()
                .circle_filled(dots.center(), 12.0, kit::cola(t.alias.hover));
        }
        icons::paint(
            ui.painter(),
            Rect::from_center_size(dots.center(), Vec2::splat(14.0)),
            Icon::Dots,
            kit::col(t.alias.label[1]),
        );
        if dots_resp.clicked() {
            on_action(RowAction::Menu(id, dots));
            return;
        }
    } else {
        ui.painter().text(
            egui::pos2(rect.max.x - 8.0, rect.center().y),
            Align2::RIGHT_CENTER,
            &time_text,
            time_font,
            kit::col(t.alias.label[2]),
        );
    }

    if resp.clicked() {
        on_action(RowAction::Switch(id));
    }
}

fn word_button(
    ui: &mut egui::Ui,
    right_x: f32,
    center_y: f32,
    text: &str,
    danger: bool,
    t: &sica_core::theme::Theme,
) -> (Rect, bool) {
    let font = kit::font(12.0, Weight::Medium);
    let galley = ui.fonts(|f| f.layout_no_wrap(text.to_owned(), font.clone(), egui::Color32::WHITE));
    let size = galley.size() + Vec2::new(16.0, 8.0);
    let rect = Rect::from_min_size(
        egui::pos2(right_x - size.x, center_y - size.y / 2.0),
        size,
    );
    let resp = ui.interact(rect, ui.id().with(("confirm", text, rect.min.x as i32)), Sense::click());
    let (fill, fg) = if danger {
        (kit::col(t.alias.error), egui::Color32::WHITE)
    } else if resp.hovered() {
        (kit::cola(t.alias.hover), kit::col(t.alias.label[0]))
    } else {
        (egui::Color32::TRANSPARENT, kit::col(t.alias.label[1]))
    };
    ui.painter()
        .rect_filled(rect, egui::Rounding::same(RADIUS_PILL), fill);
    ui.painter()
        .text(rect.center(), Align2::CENTER_CENTER, text, font, fg);
    (rect, resp.clicked())
}

/// Relative buckets: `now · {n}min · {n}h · {n}d · {n}mo · {n}y`.
pub fn relative_time(unix_secs: i64) -> String {
    if unix_secs <= 0 {
        return String::new();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let d = (now - unix_secs).max(0);
    match d {
        0..=59 => "now".into(),
        60..=3599 => format!("{}min", d / 60),
        3600..=86_399 => format!("{}h", d / 3600),
        86_400..=2_591_999 => format!("{}d", d / 86_400),
        2_592_000..=31_535_999 => format!("{}mo", d / 2_592_000),
        _ => format!("{}y", d / 31_536_000),
    }
}

/// A 24 px gradient from transparent to `to` along the bottom of `rect`.
fn fade(ui: &mut egui::Ui, rect: Rect, to: egui::Color32) {
    let h = 24.0;
    let top = rect.max.y - h;
    if top <= rect.min.y {
        return;
    }
    let mut mesh = egui::Mesh::default();
    let clear = egui::Color32::from_rgba_unmultiplied(to.r(), to.g(), to.b(), 0);
    mesh.colored_vertex(egui::pos2(rect.min.x, top), clear);
    mesh.colored_vertex(egui::pos2(rect.max.x, top), clear);
    mesh.colored_vertex(egui::pos2(rect.min.x, rect.max.y), to);
    mesh.colored_vertex(egui::pos2(rect.max.x, rect.max.y), to);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 2, 3);
    ui.painter().add(egui::Shape::mesh(mesh));
}

// ---------------------------------------------------------------------------
// Foot
// ---------------------------------------------------------------------------

fn foot(app: &mut App, ui: &mut egui::Ui, collapsed: bool) {
    let t = app.theme;
    ui.add_space(6.0);

    // Settings trigger + connection indicator inline (wide only).
    if collapsed {
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), Sense::click());
        let hit = Rect::from_center_size(rect.center(), Vec2::splat(36.0));
        if resp.hovered() {
            ui.painter()
                .circle_filled(hit.center(), 18.0, kit::cola(t.alias.hover));
        }
        icons::paint(
            ui.painter(),
            Rect::from_center_size(hit.center(), Vec2::splat(16.0)),
            Icon::Gear,
            kit::col(t.alias.label[1]),
        );
        if resp.on_hover_text("Settings").clicked() {
            app.settings_open = true;
        }
    } else {
        ui.horizontal(|ui| {
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(120.0, 42.0), Sense::click());
            if resp.hovered() {
                ui.painter().rect_filled(
                    rect,
                    egui::Rounding::same(RADIUS_CARD),
                    kit::col(t.alias.sidebar_hover),
                );
            }
            icons::paint(
                ui.painter(),
                Rect::from_center_size(
                    egui::pos2(rect.min.x + 16.0, rect.center().y),
                    Vec2::splat(16.0),
                ),
                Icon::Gear,
                kit::col(t.alias.label[1]),
            );
            ui.painter().text(
                egui::pos2(rect.min.x + 32.0, rect.center().y),
                Align2::LEFT_CENTER,
                "Settings",
                kit::font(14.0, Weight::Regular),
                kit::col(t.alias.label[1]),
            );
            if resp.clicked() {
                app.settings_open = true;
            }

            if let Some(resp) = kit::connection_indicator(ui, conn_state(app)) {
                if resp
                    .on_hover_text(connection_detail(app))
                    .clicked()
                {
                    if app.be_state.running {
                        app.send(UiCommand::RebuildAndRestart { release: app.release_profile });
                    } else {
                        app.send(UiCommand::StartBe);
                    }
                }
            }
        });
    }

    // sica's own affordance: source drift / protocol mismatch. Not in dsh —
    // and deliberately kept, because the rebuild loop is the point of the app.
    restart_chip(app, ui, collapsed);
}

fn conn_state(app: &App) -> ConnState {
    if !app.be_state.running && app.be_state.last_exit_code.is_some() {
        ConnState::Disconnected
    } else if !app.ipc_state.connected {
        if app.be_state.running {
            ConnState::Connecting
        } else {
            ConnState::Disconnected
        }
    } else if app.ipc_state.heartbeat_timeout {
        ConnState::Disconnected
    } else if app
        .recovered_at
        .map(|t| t.elapsed() < std::time::Duration::from_secs(2))
        .unwrap_or(false)
    {
        ConnState::Recovered
    } else {
        ConnState::Healthy
    }
}

fn connection_detail(app: &App) -> String {
    let mut lines = vec![format!(
        "backend: {}",
        if app.be_state.running {
            format!("running (pid {})", app.be_state.pid.unwrap_or(0))
        } else {
            "stopped".into()
        }
    )];
    lines.push(format!(
        "ipc: {}",
        if app.ipc_state.connected {
            "connected"
        } else {
            "disconnected"
        }
    ));
    if let Some(e) = &app.ipc_state.last_error {
        lines.push(e.clone());
    }
    lines.push("click to reconnect".into());
    lines.join("\n")
}

fn restart_chip(app: &mut App, ui: &mut egui::Ui, collapsed: bool) {
    let t = app.theme;
    let mismatch = app.be_state.protocol_mismatch;
    let drifted = app.be_state.restart_pending();
    if !drifted && mismatch.is_none() {
        return;
    }
    let busy = app.build_state.in_flight;
    let (fill, fg, text) = if let Some((be, fe)) = mismatch {
        (
            kit::col(t.alias.error).linear_multiply(if t.dark { 0.35 } else { 0.14 }),
            kit::col(t.alias.error),
            format!("Protocol v{be} ≠ v{fe}"),
        )
    } else {
        (
            kit::col(t.alias.warn_tertiary),
            kit::col(t.alias.warn_label),
            if busy {
                "Rebuilding…".to_string()
            } else {
                "Rebuild & restart".to_string()
            },
        )
    };
    let label = if collapsed { "⟳".to_string() } else { text.clone() };
    let resp = kit::tinted_pill(ui, &label, fill, fg, 12.0);
    let tip = format!(
        "Source has changed since the backend was built.\nBE: {}\nSrc: {}",
        app.be_state.running_version.as_deref().unwrap_or("—"),
        app.be_state.source_version.as_deref().unwrap_or("—"),
    );
    if resp.on_hover_text(tip).clicked() && !busy {
        app.send(UiCommand::RebuildAndRestart {
            release: app.release_profile,
        });
    }
    ui.add_space(4.0);
}
