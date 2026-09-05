//! Settings › General (§7.1). Rows in dsh's order, and **every row applies
//! live** — there is no Apply button here; each change writes through to
//! `sica-settings.json` and, for the appearance rows, straight into the
//! egui style.

use egui::{Align2, Rect, Rounding, Sense, Vec2};

use sica_core::theme::tokens::{CONTENT_MAX_PX, CONTENT_MIN_PX};

use crate::app::{App, BusyEnter, ThemeMode};
use crate::ui::icons::{self, Icon};
use crate::ui::kit::{self, Level, Weight};

use super::{row, section, segmented};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let mut dirty = false;

    // Permission — the default mode for new sessions. Full access applies
    // like any other mode; the segment's description carries the warning.
    let modes = ["Read Only", "Workspace Write", "Full access"];
    let ids = ["read-only", "workspace-write", "danger-full-access"];
    let current = ids
        .iter()
        .position(|id| *id == app.default_permission_mode)
        .unwrap_or(1);
    row(
        ui,
        "Permission",
        "Default permission mode for new sessions. Full access stops asking \
         before destructive commands and writes outside the working directory.",
        |ui| {
            if let Some(i) = segmented(ui, &modes, current) {
                app.default_permission_mode = ids[i].to_string();
                dirty = true;
            }
        },
    );

    // Working directory — the folder the agent reads, writes and runs
    // commands in. Separate from the app's own root, which keeps holding
    // settings, sessions and skills.
    if working_dir_row(app, ui, &ctx) {
        dirty = false; // `set_working_dir` already persisted everything.
    }

    // Appearance — three cubes.
    row(ui, "Appearance", "Light, dark, or follow the system", |ui| {
        if let Some(mode) = appearance_cubes(app, ui) {
            app.theme_mode = mode;
            dirty = true;
        }
    });

    // Font size — a stepper pill, 12..=17.
    row(
        ui,
        "Font size",
        "Only affects conversation content",
        |ui| {
            if let Some(v) = stepper(app, ui) {
                app.content_px = v;
                dirty = true;
            }
        },
    );

    row(
        ui,
        "Conversation display",
        "Controls process content in completed turns",
        |ui| {
            let sel = usize::from(app.transcript_compact);
            if let Some(i) = segmented(ui, &["Normal", "Compact"], sel) {
                app.transcript_compact = i == 1;
                dirty = true;
            }
        },
    );

    row(
        ui,
        "Enter behavior while busy",
        "Busy only; Ctrl+Enter uses the other behavior",
        |ui| {
            let sel = usize::from(app.busy_enter == BusyEnter::Steer);
            if let Some(i) = segmented(ui, &["Queue", "Steer"], sel) {
                app.busy_enter = if i == 1 { BusyEnter::Steer } else { BusyEnter::Queue };
                dirty = true;
            }
        },
    );

    row(
        ui,
        "Reduce motion",
        "Freeze the shimmer, sweep and status animations",
        |ui| {
            if ui.checkbox(&mut app.reduce_motion, "").changed() {
                dirty = true;
            }
        },
    );

    section(ui, "Startup");
    row(ui, "Start the backend automatically", "", |ui| {
        if ui.checkbox(&mut app.auto_start_be, "").changed() {
            dirty = true;
        }
    });
    row(
        ui,
        "Connect the model automatically",
        "Reconnects the last provider once the backend is up",
        |ui| {
            if ui.checkbox(&mut app.auto_connect_llm, "").changed() {
                dirty = true;
            }
        },
    );
    row(
        ui,
        "Rebuild the backend on source changes",
        "Watches crates/ and rebuilds `backend` after a 1 s debounce",
        |ui| {
            if ui.checkbox(&mut app.auto_watch, "").changed() {
                app.send(crate::supervisor::UiCommand::SetAutoWatch(app.auto_watch));
                dirty = true;
            }
        },
    );

    section(ui, "Logging");
    row(
        ui,
        "Log raw model responses",
        "Writes every completion under logs/model/",
        |ui| {
            if ui.checkbox(&mut app.log_raw_llm, "").changed() {
                dirty = true;
            }
        },
    );
    row(
        ui,
        "Idealist auto-apply",
        "When off, idealist only writes Improvement-BE-*.md tickets. \
         Frontend issues always get a ticket and are never auto-patched.",
        |ui| {
            if ui.checkbox(&mut app.idealist_auto_apply_be, "").changed() {
                dirty = true;
            }
        },
    );

    if dirty {
        app.save_general(&ctx);
    }
    ui.add_space(24.0);
}

/// The Working directory row: the live path as the description, a picker
/// button on the right. The menu lists the app's own folder, the directories
/// the user picked before, and a native folder chooser.
///
/// Returns `true` when the directory changed — the caller then skips its own
/// save, because switching folders persists and restarts the BE itself.
fn working_dir_row(app: &mut App, ui: &mut egui::Ui, ctx: &egui::Context) -> bool {
    let root = sica_core::paths::workspace_root();
    let current = sica_core::paths::working_dir();
    let label = current
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| current.display().to_string());

    // Menu targets, in the order the items are drawn. `None` = the app root.
    let mut targets: Vec<Option<std::path::PathBuf>> = vec![None];
    let mut items = vec![kit::MenuItem::new("App folder")
        .detail(root.display().to_string())
        .checked(app.working_dir.is_none())];
    for dir in app.recent_working_dirs.clone() {
        if dir == root {
            continue;
        }
        items.push(
            kit::MenuItem::new(
                dir.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| dir.display().to_string()),
            )
            .detail(dir.display().to_string())
            .checked(app.working_dir.as_deref() == Some(dir.as_path()))
            .sep_above(targets.len() == 1),
        );
        targets.push(Some(dir));
    }
    items.push(kit::MenuItem::new("Choose folder…").sep_above(true));

    let mut chosen: Option<Option<std::path::PathBuf>> = None;
    row(
        ui,
        "Working directory",
        &current.display().to_string(),
        |ui| {
            let resp = kit::button(ui, &label, kit::Variant::Outline, kit::Size::Sm);
            let anchor = resp.rect;
            let tip = "Where the agent reads, writes and runs commands. \
                       Changing it restarts the backend.";
            if resp.on_hover_text(tip).clicked() {
                app.menu_open.working_dir = !app.menu_open.working_dir;
            }
            let mut open = app.menu_open.working_dir;
            let picked = kit::menu(
                ui.ctx(),
                egui::Id::new("working_dir_menu"),
                anchor,
                kit::MenuSide::Below,
                300.0,
                &items,
                &mut open,
            );
            app.menu_open.working_dir = open;
            match picked.map(|i| targets.get(i)) {
                Some(Some(t)) => chosen = Some(t.clone()),
                // The last item is the native chooser; a cancelled dialog
                // decides nothing, which is not the same as "app folder".
                Some(None) => {
                    if let Some(dir) =
                        rfd::FileDialog::new().set_directory(&current).pick_folder()
                    {
                        chosen = Some(Some(dir));
                    }
                }
                None => {}
            }
        },
    );

    match chosen {
        Some(dir) => {
            let before = app.working_dir.clone();
            app.set_working_dir(dir, ctx);
            app.working_dir != before
        }
        None => false,
    }
}

/// Three "cubes" — icon over label, `flex 1 1 180px`, r=20, selected gets a
/// `bluish-400` border and the module fill.
fn appearance_cubes(app: &App, ui: &mut egui::Ui) -> Option<ThemeMode> {
    let t = app.theme;
    let mut picked = None;
    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
        for (mode, label, icon) in [
            (ThemeMode::Light, "Light", Icon::Sun),
            (ThemeMode::Dark, "Dark", Icon::Moon),
            (ThemeMode::System, "System", Icon::Monitor),
        ] {
            let active = app.theme_mode == mode;
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(96.0, 64.0), Sense::click());
            ui.painter().rect(
                rect,
                Rounding::same(20.0),
                if active {
                    kit::cola(t.alias.hover)
                } else if resp.hovered() {
                    kit::cola(t.alias.hover).linear_multiply(0.5)
                } else {
                    egui::Color32::TRANSPARENT
                },
                egui::Stroke::new(
                    if active { 1.0 } else { sica_core::theme::tokens::HAIRLINE },
                    if active {
                        kit::col(t.statics.bluish[8])
                    } else {
                        Level::L4.color(&t)
                    },
                ),
            );
            icons::paint(
                ui.painter(),
                Rect::from_center_size(
                    egui::pos2(rect.center().x, rect.min.y + 22.0),
                    Vec2::splat(18.0),
                ),
                icon,
                kit::col(t.alias.label[1]),
            );
            ui.painter().text(
                egui::pos2(rect.center().x, rect.max.y - 16.0),
                Align2::CENTER_CENTER,
                label,
                kit::font(13.0, Weight::Regular),
                kit::col(t.alias.label[1]),
            );
            if resp.clicked() {
                picked = Some(mode);
            }
        }
    });
    picked
}

/// h=36 r=18 pill with −/+ either side of the current value.
fn stepper(app: &App, ui: &mut egui::Ui) -> Option<u8> {
    let t = app.theme;
    let mut out = None;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(112.0, 36.0), Sense::hover());
    ui.painter().rect(
        rect,
        Rounding::same(18.0),
        egui::Color32::TRANSPARENT,
        egui::Stroke::new(sica_core::theme::tokens::HAIRLINE, Level::L3.color(&t)),
    );
    let minus = Rect::from_center_size(
        egui::pos2(rect.min.x + 18.0, rect.center().y),
        Vec2::splat(28.0),
    );
    let plus = Rect::from_center_size(
        egui::pos2(rect.max.x - 18.0, rect.center().y),
        Vec2::splat(28.0),
    );
    let m = ui.interact(minus, ui.id().with("font_minus"), Sense::click());
    let p = ui.interact(plus, ui.id().with("font_plus"), Sense::click());
    for (r, hovered) in [(minus, m.hovered()), (plus, p.hovered())] {
        if hovered {
            ui.painter()
                .circle_filled(r.center(), 14.0, kit::cola(t.alias.hover));
        }
    }
    ui.painter().text(
        minus.center(),
        Align2::CENTER_CENTER,
        "−",
        kit::font(15.0, Weight::Medium),
        kit::col(t.alias.label[1]),
    );
    ui.painter().text(
        plus.center(),
        Align2::CENTER_CENTER,
        "+",
        kit::font(15.0, Weight::Medium),
        kit::col(t.alias.label[1]),
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        format!("{}", app.content_px),
        kit::font(14.0, Weight::Medium),
        kit::col(t.alias.label[0]),
    );
    if m.clicked() && app.content_px > CONTENT_MIN_PX {
        out = Some(app.content_px - 1);
    }
    if p.clicked() && app.content_px < CONTENT_MAX_PX {
        out = Some(app.content_px + 1);
    }
    out
}
