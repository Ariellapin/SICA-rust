//! Settings as a **modal** (§7): a full-window mask, an 800 × min(800, h−48)
//! panel at r=32, a 188 px nav rail on the left and a scrolling content
//! column on the right with a 54 px header.
//!
//! Sections: **General** (applies live — dsh has no Apply button there),
//! **Models** (one card per provider, Connect / Apply per card), **Skills**
//! (the catalogue the `/` palette lists, plus the folders behind it) and
//! **Diagnostics** (the old Communication tab: connection, demo requests and
//! the log panel, which now keeps its level).

mod diagnostics;
mod general;
mod llm;
mod skills;

use egui::{Align, Align2, Layout, Rect, Rounding, Sense, Vec2};

use sica_core::theme::tokens::{RADIUS_CARD, RADIUS_SETTINGS};

use crate::app::{App, SettingsTab};
use crate::ui::icons::{self, Icon};
use crate::ui::kit::{self, Elevation, Level, Weight};

const NAV_W: f32 = 188.0;
const PANEL_W: f32 = 800.0;

const SECTIONS: [(SettingsTab, &str, Icon); 4] = [
    (SettingsTab::General, "General", Icon::Gear),
    (SettingsTab::Models, "Models", Icon::Model),
    (SettingsTab::Skills, "Skills", Icon::Sparkle),
    (SettingsTab::Diagnostics, "Diagnostics", Icon::Job),
];

pub fn draw(app: &mut App, ctx: &egui::Context) {
    if !app.settings_open {
        return;
    }
    let t = app.theme;
    let screen = ctx.screen_rect();
    let w = PANEL_W.min(screen.width() - 48.0);
    let h = 800.0_f32.min(screen.height() - 48.0);

    // Mask: painted straight onto a layer above the panels. An `Area` would
    // shrink to its content and (being movable) fight the pointer; the dialog
    // below owns the clicks it needs, and everything outside it closes.
    kit::mask(ctx, egui::Id::new("settings_mask"));
    let mut close = ctx.input(|i| i.key_pressed(egui::Key::Escape));

    egui::Area::new(egui::Id::new("settings_panel"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(
            screen.center().x - w / 2.0,
            screen.center().y - h / 2.0,
        ))
        .show(ctx, |ui| {
            ui.set_width(w);
            ui.set_height(h);
            kit::elevated_frame(
                &t,
                Elevation::Prominent,
                Level::L1,
                kit::col(t.alias.bg_layer[1]),
                RADIUS_SETTINGS,
            )
            .inner_margin(egui::Margin::ZERO)
            .show(ui, |ui| {
                ui.set_width(w);
                ui.set_height(h);
                ui.horizontal_top(|ui| {
                    // Nav rail.
                    ui.allocate_ui_with_layout(
                        Vec2::new(NAV_W, h),
                        Layout::top_down(Align::Min),
                        |ui| {
                            ui.add_space(22.0);
                            ui.horizontal(|ui| {
                                ui.add_space(12.0);
                                kit::label(
                                    ui,
                                    kit::txt(
                                        "Settings",
                                        16.0,
                                        Weight::Medium,
                                        kit::col(t.alias.label[0]),
                                    ),
                                );
                            });
                            ui.add_space(12.0);
                            for (tab, label, icon) in SECTIONS {
                                nav_cell(app, ui, tab, label, icon);
                            }
                        },
                    );
                    // Content.
                    ui.allocate_ui_with_layout(
                        Vec2::new(w - NAV_W, h),
                        Layout::top_down(Align::Min),
                        |ui| {
                            ui.set_width(w - NAV_W);
                            header(app, ui, &mut close);
                            egui::Frame::none()
                                .inner_margin(egui::Margin {
                                    left: 24.0,
                                    right: 24.0,
                                    top: 0.0,
                                    bottom: 24.0,
                                })
                                .show(ui, |ui| {
                                    egui::ScrollArea::vertical()
                                        .auto_shrink([false, false])
                                        .show(ui, |ui| match app.settings_tab {
                                            SettingsTab::General => general::draw(app, ui),
                                            SettingsTab::Models => llm::draw(app, ui),
                                            SettingsTab::Skills => skills::draw(app, ui),
                                            SettingsTab::Diagnostics => diagnostics::draw(app, ui),
                                        });
                                });
                        },
                    );
                });
            });
        });

    // A press anywhere off the panel closes, which is the mask's whole job.
    let panel_rect = ctx
        .memory(|m| m.area_rect(egui::Id::new("settings_panel")))
        .unwrap_or(screen);
    if ctx.input(|i| {
        i.pointer.any_pressed()
            && i.pointer
                .interact_pos()
                .map(|p| !panel_rect.contains(p))
                .unwrap_or(false)
    }) {
        close = true;
    }

    if close {
        app.settings_open = false;
        app.risk_gate_open = false;
    }
}

fn header(app: &mut App, ui: &mut egui::Ui, close: &mut bool) {
    let t = app.theme;
    let title = SECTIONS
        .iter()
        .find(|(tab, _, _)| *tab == app.settings_tab)
        .map(|(_, l, _)| *l)
        .unwrap_or("Settings");
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), 54.0),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.add_space(24.0);
            kit::label(
                ui,
                kit::txt(title, 16.0, Weight::Medium, kit::col(t.alias.label[0])),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(12.0);
                if kit::icon_button(ui, Icon::Close, 28.0).clicked() {
                    *close = true;
                }
                if kit::button(
                    ui,
                    "Open configuration file",
                    kit::Variant::Outline,
                    kit::Size::Sm,
                )
                .clicked()
                {
                    let _ = open_path(&sica_core::paths::settings_file());
                }
            });
        },
    );
    kit::hairline(ui, Level::L2);
}

fn nav_cell(app: &mut App, ui: &mut egui::Ui, tab: SettingsTab, label: &str, icon: Icon) {
    let t = app.theme;
    let active = app.settings_tab == tab;
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(NAV_W - 24.0, 40.0), Sense::click());
        if active {
            ui.painter().rect_filled(
                rect,
                Rounding::same(RADIUS_CARD),
                kit::col(t.alias.sidebar_active),
            );
        } else if resp.hovered() {
            ui.painter().rect_filled(
                rect,
                Rounding::same(RADIUS_CARD),
                kit::col(t.alias.sidebar_hover),
            );
        }
        icons::paint(
            ui.painter(),
            Rect::from_center_size(
                egui::pos2(rect.min.x + 14.0, rect.center().y),
                Vec2::splat(15.0),
            ),
            icon,
            kit::col(t.alias.label[1]),
        );
        ui.painter().text(
            egui::pos2(rect.min.x + 32.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            kit::font(14.0, Weight::Regular),
            kit::col(if active {
                t.alias.label[0]
            } else {
                t.alias.label[1]
            }),
        );
        if resp.clicked() {
            app.settings_tab = tab;
        }
    });
}

// ---------------------------------------------------------------------------
// Shared row chrome — every settings row is `pad 16 0`, a 0.5 px bottom rule,
// title 14/22, description 12/18, control on the right.
// ---------------------------------------------------------------------------

pub fn row<R>(
    ui: &mut egui::Ui,
    title: &str,
    description: &str,
    control: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let t = kit::theme(ui);
    ui.add_space(16.0);
    let out = ui
        .horizontal(|ui| {
            let text_w = (ui.available_width() * 0.52).max(180.0);
            let inner = ui
                .allocate_ui_with_layout(
                    Vec2::new(text_w, 0.0),
                    Layout::top_down(Align::Min),
                    |ui| {
                        ui.set_max_width(text_w);
                        kit::label(
                            ui,
                            kit::txt(title, 14.0, Weight::Medium, kit::col(t.alias.label[0])),
                        );
                        if !description.is_empty() {
                            ui.add(
                                egui::Label::new(kit::txt(
                                    description,
                                    12.0,
                                    Weight::Regular,
                                    kit::col(t.alias.label[2]),
                                ))
                                .wrap(),
                            );
                        }
                    },
                )
                .response;
            let _ = inner;
            ui.with_layout(Layout::right_to_left(Align::Center), control).inner
        })
        .inner;
    ui.add_space(12.0);
    kit::hairline(ui, Level::L2);
    out
}

/// A section title inside a content column.
pub fn section(ui: &mut egui::Ui, title: &str) {
    let t = kit::theme(ui);
    ui.add_space(18.0);
    kit::label(
        ui,
        kit::txt(title, 14.0, Weight::Semibold, kit::col(t.alias.label[0])),
    );
}

/// Segmented control — the shape dsh uses for Normal/Compact, Queue/Steer.
pub fn segmented(ui: &mut egui::Ui, options: &[&str], selected: usize) -> Option<usize> {
    let t = kit::theme(ui);
    let mut picked = None;
    // `ui.horizontal` inherits the parent's right-to-left preference, and the
    // control slot of a settings row *is* right-to-left — so the direction is
    // stated explicitly here or the options render reversed.
    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
        for (i, label) in options.iter().enumerate() {
            let font = kit::font(13.0, Weight::Medium);
            let galley =
                ui.fonts(|f| f.layout_no_wrap((*label).to_owned(), font.clone(), egui::Color32::WHITE));
            let (rect, resp) =
                ui.allocate_exact_size(galley.size() + Vec2::new(24.0, 14.0), Sense::click());
            let active = i == selected;
            ui.painter().rect(
                rect,
                Rounding::same(sica_core::theme::tokens::RADIUS_PILL),
                if active {
                    kit::cola(t.alias.active)
                } else if resp.hovered() {
                    kit::cola(t.alias.hover)
                } else {
                    egui::Color32::TRANSPARENT
                },
                egui::Stroke::new(
                    sica_core::theme::tokens::HAIRLINE,
                    if active {
                        kit::col(t.alias.business)
                    } else {
                        Level::L3.color(&t)
                    },
                ),
            );
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                *label,
                font,
                kit::col(if active {
                    t.alias.label[0]
                } else {
                    t.alias.label[2]
                }),
            );
            if resp.clicked() {
                picked = Some(i);
            }
        }
    });
    picked
}

/// Open a path in the OS file browser.
pub fn open_path(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";

    std::process::Command::new(program)
        .arg(path.as_os_str())
        .spawn()
        .map(|_| ())
}
