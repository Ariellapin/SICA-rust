//! Shared editorial widgets. Every panel composes from this kit so spacing,
//! type and chrome stay coherent across the surface.
//!
//! Categories:
//!   * Typography  — [`section_heading`], [`caps_label`], [`display_text`],
//!                   [`italic_text`], [`muted_italic`]
//!   * Buttons     — [`ghost_button`], [`primary_button`]
//!   * Containers  — [`card`]
//!   * Rules       — [`hairline`]
//!   * Marks       — [`blade_mark`], [`chat_rail_mark`], [`settings_rail_mark`]
//!   * Status      — [`status_icon`], [`status_pill`]

use egui::{
    text::LayoutJob, Align, Color32, FontFamily, FontId, Layout, Painter, Pos2, Rect,
    Response, RichText, Rounding, Sense, Shape, Stroke, TextFormat, TextStyle, Ui, Vec2,
};

use sica_core::theme::{
    tokens::{FAMILY_ITALIC, HAIRLINE, RADIUS_2, SPACE_1, SPACE_2},
    Palette,
};

use crate::app::rgb;

// ----------------------------------------------------------------------------
// Typography
// ----------------------------------------------------------------------------

/// Italic serif section heading at 22pt, followed by a full-width hairline
/// rule. Use for the title of every panel section ("Sessions", "Appearance",
/// "Startup", per-provider title in the LLM tab, etc).
pub fn section_heading(ui: &mut Ui, palette: &Palette, text: &str) {
    ui.add_space(SPACE_1);
    ui.label(display_text(text, 22.0).color(rgb(palette.ink)));
    ui.add_space(2.0);
    hairline(ui, palette);
    ui.add_space(SPACE_1);
}

/// Tracked uppercase mono label at 11pt — the small-cap label motif. Falls
/// back to a regular `RichText` label if `LayoutJob` letter-spacing is
/// unavailable (it isn't in egui 0.28, so this always renders tracked).
pub fn caps_label(ui: &mut Ui, text: &str, color: Color32) -> Response {
    ui.add(egui::Label::new(caps_job(text, color, 11.0)).selectable(false))
}

/// Same as [`caps_label`] but returns a click-sensitive response — useful for
/// settings tabs and tappable chips.
pub fn caps_button(ui: &mut Ui, text: &str, color: Color32) -> Response {
    ui.add(
        egui::Label::new(caps_job(text, color, 11.0))
            .selectable(false)
            .sense(Sense::click()),
    )
}

/// Build a `LayoutJob` for tracked caps text. Public so callers can compose
/// it into a horizontal row alongside other widgets without re-laying out.
pub fn caps_job(text: &str, color: Color32, size: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.append(
        &text.to_uppercase(),
        0.0,
        TextFormat {
            font_id: FontId::new(size, FontFamily::Monospace),
            color,
            extra_letter_spacing: 1.4,
            ..Default::default()
        },
    );
    job
}

/// Italic serif display `RichText`. Use for empty-state titles, decorative
/// numerals, and any time the editorial voice should ring through.
pub fn display_text(text: &str, size: f32) -> RichText {
    RichText::new(text)
        .family(FontFamily::Name(FAMILY_ITALIC.into()))
        .size(size)
}

/// Regular serif inline text — useful for prose subtitles where italic would
/// over-emote.
pub fn italic_text(text: &str, size: f32) -> RichText {
    RichText::new(text)
        .family(FontFamily::Name(FAMILY_ITALIC.into()))
        .size(size)
}

/// Muted italic serif "why" voice — for the explanatory subtitles under
/// section headings in Settings.
pub fn muted_italic(palette: &Palette, text: &str) -> RichText {
    display_text(text, 12.0).color(rgb(palette.muted))
}

// ----------------------------------------------------------------------------
// Buttons
// ----------------------------------------------------------------------------

/// Ghost button — hairline border + ink label by default, accent border +
/// accent label on hover, accent fill + on-accent label when pressed.
/// Inherits all chrome from `apply_visuals`; this is just a styled label
/// wrapped in an interactive widget.
pub fn ghost_button(ui: &mut Ui, palette: &Palette, label: &str) -> Response {
    ghost_button_enabled(ui, palette, label, true)
}

/// Ghost button with an explicit enabled flag — matches `ui.add_enabled`
/// usage for guarded actions.
pub fn ghost_button_enabled(
    ui: &mut Ui,
    palette: &Palette,
    label: &str,
    enabled: bool,
) -> Response {
    let color = if enabled { rgb(palette.ink) } else { rgb(palette.muted) };
    ui.add_enabled(
        enabled,
        egui::Button::new(caps_job(label, color, 11.0))
            .min_size(Vec2::new(0.0, 24.0)),
    )
}

/// Primary button — solid accent fill + on-accent (page colour) label.
/// Reserve for the single most important action on a surface.
pub fn primary_button(ui: &mut Ui, palette: &Palette, label: &str) -> Response {
    primary_button_enabled(ui, palette, label, true)
}

pub fn primary_button_enabled(
    ui: &mut Ui,
    palette: &Palette,
    label: &str,
    enabled: bool,
) -> Response {
    let label_color = if enabled { rgb(palette.page_bg) } else { rgb(palette.muted) };
    let fill = if enabled { rgb(palette.accent) } else { rgb(palette.surface_sunk) };
    ui.add_enabled(
        enabled,
        egui::Button::new(caps_job(label, label_color, 11.0))
            .fill(fill)
            .stroke(Stroke::new(HAIRLINE, if enabled { rgb(palette.accent) } else { rgb(palette.hairline) }))
            .min_size(Vec2::new(0.0, 26.0)),
    )
}

// ----------------------------------------------------------------------------
// Containers
// ----------------------------------------------------------------------------

/// Card frame — surface fill, 2px rounding, hairline border. The standard
/// box for a self-contained config panel (per-provider in the LLM tab) or
/// any grouped form.
pub fn card<R>(ui: &mut Ui, palette: &Palette, body: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::none()
        .fill(rgb(palette.surface))
        .stroke(Stroke::new(HAIRLINE, rgb(palette.hairline)))
        .rounding(Rounding::same(RADIUS_2))
        .inner_margin(egui::Margin::symmetric(SPACE_2 * 1.5, SPACE_2 * 1.5))
        .show(ui, body)
        .inner
}

// ----------------------------------------------------------------------------
// Rules
// ----------------------------------------------------------------------------

/// Full-width 1px horizontal hairline rule. Used everywhere a section breaks
/// or a row needs separating.
pub fn hairline(ui: &mut Ui, palette: &Palette) {
    let avail = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(avail, HAIRLINE), Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        Stroke::new(HAIRLINE, rgb(palette.hairline)),
    );
}

// ----------------------------------------------------------------------------
// Marks
// ----------------------------------------------------------------------------

/// The blade silhouette — a stylised *sica*. Used both as the sidebar brand
/// (small, ink) and as the empty-chat watermark (large, low-opacity accent).
///
/// The shape: an angled slab with a sharpened tip on the right and a small
/// circular pommel at the back. Drawn with a 4-point convex polygon plus a
/// stroke; no font, no SVG.
pub fn blade_mark(painter: &Painter, rect: Rect, color: Color32) {
    let cx = rect.center().x;
    let cy = rect.center().y;
    let w  = rect.width();
    let h  = rect.height();

    // Blade body — a 4-point polygon with a subtle curve hinted by the
    // bottom edge being slightly higher than the top at the tip.
    let tip   = Pos2::new(cx + w * 0.46, cy + h * 0.02);
    let crown = Pos2::new(cx - w * 0.10, cy - h * 0.28);
    let back  = Pos2::new(cx - w * 0.40, cy - h * 0.08);
    let belly = Pos2::new(cx + w * 0.05, cy + h * 0.18);

    painter.add(Shape::convex_polygon(
        vec![back, crown, tip, belly],
        color,
        Stroke::NONE,
    ));

    // Pommel — a small filled disc at the back of the blade.
    let pommel = Pos2::new(cx - w * 0.46, cy - h * 0.03);
    painter.circle_filled(pommel, (h * 0.10).max(2.0), color);

    // Grip — a thin line from pommel to back of blade.
    painter.line_segment(
        [pommel, back],
        Stroke::new((h * 0.05).max(1.0), color),
    );
}

/// Sidebar mark for the Chat view — three descending horizontal lines
/// suggesting a hung paragraph.
pub fn chat_rail_mark(painter: &Painter, rect: Rect, color: Color32) {
    let cx = rect.center().x;
    let cy = rect.center().y;
    let stroke = Stroke::new(1.5, color);
    let widths = [10.0, 8.0, 6.0];
    let dy = 5.0;
    for (i, &w) in widths.iter().enumerate() {
        let y = cy + (i as f32 - 1.0) * dy;
        painter.hline(cx - w..=cx + w, y, stroke);
    }
}

/// Sidebar mark for the Settings view — a circle with a small notch at top
/// (a single gear tooth, abstracted).
pub fn settings_rail_mark(painter: &Painter, rect: Rect, color: Color32) {
    let center = rect.center();
    let r = 6.0;
    painter.circle_stroke(center, r, Stroke::new(1.5, color));
    // Crown notch — a short vertical bar at the top.
    painter.line_segment(
        [Pos2::new(center.x, center.y - r - 3.0), Pos2::new(center.x, center.y - r)],
        Stroke::new(1.5, color),
    );
    // Center dot.
    painter.circle_filled(center, 1.5, color);
}

// ----------------------------------------------------------------------------
// Status
// ----------------------------------------------------------------------------

/// Identifies which subsystem a status icon represents — drives the icon
/// glyph chosen by [`status_icon`].
#[derive(Clone, Copy)]
pub enum StatusKind {
    /// Backend daemon — drawn as a small rounded "process" box.
    Be,
    /// Named-pipe link — drawn as two nodes joined by a connector that
    /// breaks open when disconnected.
    Ipc,
    /// LLM HTTP client — drawn as a 4-point diamond / spark.
    Llm,
}

/// A 12x12 vector icon (per [`StatusKind`]) that surfaces a hover tooltip.
/// `connected` selects the filled (live) vs outlined (idle/broken) variant.
/// The tooltip label is tinted with `color` so it reads at a glance — green
/// when healthy, red when erroring, faint when idle — instead of always
/// inheriting the default ink colour.
pub fn status_icon(
    ui: &mut Ui,
    kind: StatusKind,
    connected: bool,
    color: Color32,
    label: &str,
    detail: Option<&str>,
    error_color: Color32,
) -> Response {
    let size = 12.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let painter = ui.painter();
    match kind {
        StatusKind::Be  => draw_be_icon(painter, rect, color, connected),
        StatusKind::Ipc => draw_ipc_icon(painter, rect, color, connected),
        StatusKind::Llm => draw_llm_icon(painter, rect, color, connected),
    }
    let label = label.to_string();
    let detail = detail.map(|s| s.to_string());
    resp.on_hover_ui(move |ui| {
        ui.label(RichText::new(&label).color(color).strong());
        if let Some(d) = detail.as_deref() {
            ui.label(RichText::new(d).color(error_color));
        }
    })
}

/// BE icon — a rounded "process" box. Filled when the backend is running,
/// stroked-only when stopped. Suggests a daemon container.
fn draw_be_icon(p: &Painter, rect: Rect, color: Color32, connected: bool) {
    let body = rect.shrink2(Vec2::new(1.0, 2.0));
    if connected {
        p.rect_filled(body, Rounding::same(1.5), color);
    } else {
        p.rect_stroke(body, Rounding::same(1.5), Stroke::new(1.2, color));
    }
}

/// IPC icon — two nodes joined by a connector. When connected, the bar is
/// solid; when disconnected, the bar breaks with a visible gap, telegraphing
/// a severed pipe.
fn draw_ipc_icon(p: &Painter, rect: Rect, color: Color32, connected: bool) {
    let cy = rect.center().y;
    let r = 2.0;
    let lx = rect.min.x + 1.5 + r;
    let rx = rect.max.x - 1.5 - r;
    if connected {
        p.circle_filled(Pos2::new(lx, cy), r, color);
        p.circle_filled(Pos2::new(rx, cy), r, color);
        p.line_segment(
            [Pos2::new(lx + r, cy), Pos2::new(rx - r, cy)],
            Stroke::new(1.5, color),
        );
    } else {
        let stroke = Stroke::new(1.0, color);
        p.circle_stroke(Pos2::new(lx, cy), r, stroke);
        p.circle_stroke(Pos2::new(rx, cy), r, stroke);
        // Short stubs that leave an explicit gap in the middle.
        let stub = 1.5;
        p.line_segment(
            [Pos2::new(lx + r, cy), Pos2::new(lx + r + stub, cy)],
            stroke,
        );
        p.line_segment(
            [Pos2::new(rx - r - stub, cy), Pos2::new(rx - r, cy)],
            stroke,
        );
    }
}

/// LLM icon — a 4-point diamond / spark. Filled when the model is ready,
/// outlined otherwise. Suggests transmission / intelligence.
fn draw_llm_icon(p: &Painter, rect: Rect, color: Color32, connected: bool) {
    let c = rect.center();
    let r = 4.5;
    let top   = Pos2::new(c.x,       c.y - r);
    let right = Pos2::new(c.x + r,   c.y);
    let bot   = Pos2::new(c.x,       c.y + r);
    let left  = Pos2::new(c.x - r,   c.y);
    if connected {
        p.add(Shape::convex_polygon(
            vec![top, right, bot, left],
            color,
            Stroke::NONE,
        ));
    } else {
        let stroke = Stroke::new(1.2, color);
        p.line_segment([top,   right], stroke);
        p.line_segment([right, bot],   stroke);
        p.line_segment([bot,   left],  stroke);
        p.line_segment([left,  top],   stroke);
    }
}

/// Caps-labelled status pill — a tracked caps label inside a hairline box
/// tinted by the status colour. Used in the per-provider Connect row in the
/// LLM settings panel.
pub fn status_pill(ui: &mut Ui, palette: &Palette, label: &str, color: Color32) {
    egui::Frame::none()
        .stroke(Stroke::new(HAIRLINE, color))
        .rounding(Rounding::same(RADIUS_2))
        .inner_margin(egui::Margin::symmetric(SPACE_2, 3.0))
        .show(ui, |ui| {
            ui.add(egui::Label::new(caps_job(label, color, 10.0)).selectable(false));
            let _ = palette; // reserved for future tinted-fill variant.
        });
}

// ----------------------------------------------------------------------------
// Layout helpers
// ----------------------------------------------------------------------------

/// Right-align the body content within a horizontal strip — used for the
/// "+ NEW" button next to the "Sessions" heading and for the right edge of
/// the status bar.
pub fn right_aligned<R>(ui: &mut Ui, body: impl FnOnce(&mut Ui) -> R) -> R {
    ui.with_layout(Layout::right_to_left(Align::Center), body).inner
}

/// Re-export of TextStyle::Name("Caps") for callers that want the registered
/// style without typing the full path.
#[allow(dead_code)]
pub fn caps_style() -> TextStyle {
    TextStyle::Name("Caps".into())
}
