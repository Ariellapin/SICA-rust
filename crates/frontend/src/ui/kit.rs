//! `ui::kit` — the primitives every surface composes from, one function per
//! dsh primitive and named the same so the guide and the code line up
//! ([docs/harness-ui-guide.md](../../../../docs/harness-ui-guide.md) §1.3).
//!
//! Two conventions hold everywhere:
//!
//! * **Nothing reads a literal colour.** Widgets call [`theme`] and consume
//!   the semantic aliases; a theme flip is one field swap and no
//!   `if dark { … }` exists below this module.
//! * **The theme travels in `egui::Context` memory**, installed once per
//!   frame by `App::apply_visuals`. That keeps every call site to
//!   `kit::button(ui, "Send", Variant::Primary, Size::Md)` instead of
//!   threading a palette through twenty signatures.
//!
//! One deliberate simplification against the guide: dsh's elevation stacks
//! two blur layers behind a hairline; egui's `Frame` carries a single
//! `Shadow`, so [`elevated_frame`] draws the dominant blur as the frame's
//! shadow and the hairline as a real 0.5 px stroke. At these radii the
//! second layer is not visible on an opaque desktop window.

use std::time::Instant;

use egui::{
    text::LayoutJob, Align, Align2, Color32, FontFamily, FontId, Layout, Pos2, Rect, Response,
    RichText, Rounding, Sense, Shape, Stroke, TextFormat, Ui, Vec2,
};

use sica_core::theme::{
    tokens::{
        FAMILY_BOLD, FAMILY_MEDIUM, FAMILY_MONO, FAMILY_SEMIBOLD, HAIRLINE, RADIUS_BTN,
        RADIUS_BTN_SM, RADIUS_CARD, RADIUS_INPUT, RADIUS_ITEM, RADIUS_MENU, RADIUS_PILL,
        SWEEP_PERIOD,
    },
    Rgb, Rgba, Theme,
};

use super::icons::{self, Icon};

// ---------------------------------------------------------------------------
// Theme access + colour conversion
// ---------------------------------------------------------------------------

const THEME_KEY: &str = "sica_theme";

pub fn set_theme(ctx: &egui::Context, theme: Theme) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::new(THEME_KEY), theme));
}

/// The live theme. Falls back to dark if `apply_visuals` has not run yet —
/// which only happens on the very first frame.
pub fn theme(ui: &Ui) -> Theme {
    theme_ctx(ui.ctx())
}

pub fn theme_ctx(ctx: &egui::Context) -> Theme {
    ctx.data(|d| d.get_temp::<Theme>(egui::Id::new(THEME_KEY)))
        .unwrap_or_else(Theme::dark)
}

pub fn col(c: Rgb) -> Color32 {
    Color32::from_rgb(c.0, c.1, c.2)
}

pub fn cola(c: Rgba) -> Color32 {
    Color32::from_rgba_unmultiplied(c.0, c.1, c.2, c.3)
}

/// Composite an alpha wash onto an opaque `base` — needed wherever a wash has
/// to be baked into a fill rather than painted over one.
///
/// The blend runs through `egui::Rgba` (premultiplied *linear* floats, which
/// is how egui itself composites), so a baked fill matches what the painter
/// would have produced by drawing the wash on top.
pub fn over(base: Color32, wash: Color32) -> Color32 {
    let b = egui::Rgba::from(base);
    let w = egui::Rgba::from(wash);
    let inv = 1.0 - w.a();
    egui::Rgba::from_rgba_premultiplied(
        w.r() + b.r() * inv,
        w.g() + b.g() * inv,
        w.b() + b.b() * inv,
        1.0,
    )
    .into()
}

// ---------------------------------------------------------------------------
// Type
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Weight {
    Regular,
    Medium,
    Semibold,
    Bold,
}

impl Weight {
    pub fn family(self) -> FontFamily {
        match self {
            Weight::Regular => FontFamily::Proportional,
            Weight::Medium => FontFamily::Name(FAMILY_MEDIUM.into()),
            Weight::Semibold => FontFamily::Name(FAMILY_SEMIBOLD.into()),
            Weight::Bold => FontFamily::Name(FAMILY_BOLD.into()),
        }
    }
}

pub fn font(size: f32, weight: Weight) -> FontId {
    FontId::new(size, weight.family())
}

pub fn mono_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(FAMILY_MONO.into()))
}

pub fn txt(text: impl Into<String>, size: f32, weight: Weight, color: Color32) -> RichText {
    RichText::new(text).family(weight.family()).size(size).color(color)
}

pub fn mono(text: impl Into<String>, size: f32, color: Color32) -> RichText {
    RichText::new(text)
        .family(FontFamily::Name(FAMILY_MONO.into()))
        .size(size)
        .color(color)
}

/// Non-selectable label — the default for chrome. Body copy uses egui labels
/// directly so it stays selectable.
pub fn label(ui: &mut Ui, text: RichText) -> Response {
    ui.add(egui::Label::new(text).selectable(false))
}

/// Shorten `text` with a trailing ellipsis so it fits `max_w`.
pub fn elide(ui: &Ui, text: &str, font: &FontId, max_w: f32) -> String {
    let width = |s: String| ui.fonts(|f| f.layout_no_wrap(s, font.clone(), Color32::WHITE).size().x);
    if max_w <= 0.0 || width(text.to_owned()) <= max_w {
        return text.to_owned();
    }
    let mut out = String::new();
    for c in text.chars() {
        let mut candidate = out.clone();
        candidate.push(c);
        candidate.push('…');
        if width(candidate) > max_w {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

/// One-line, whitespace-collapsed, length-capped preview.
pub fn one_line(text: &str, cap: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(cap) {
        Some((i, _)) => format!("{}…", &flat[..i]),
        None => flat,
    }
}

// ---------------------------------------------------------------------------
// Rules and elevation
// ---------------------------------------------------------------------------

/// Border tier, 1-indexed like the CSS (`border-l1` … `border-l4`).
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub enum Level {
    L1,
    L2,
    L3,
    L4,
}

impl Level {
    pub fn color(self, t: &Theme) -> Color32 {
        cola(t.alias.border[self as usize])
    }
    fn stroke(self, t: &Theme) -> Stroke {
        Stroke::new(HAIRLINE, self.color(t))
    }
}

/// Full-width 0.5 px rule.
pub fn hairline(ui: &mut Ui, level: Level) {
    let t = theme(ui);
    let avail = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(avail, 1.0), Sense::hover());
    ui.painter()
        .hline(rect.x_range(), rect.center().y, level.stroke(&t));
}

/// Vertical 0.5 px seam at `x` spanning `y_range` — column separators.
pub fn vseam(painter: &egui::Painter, t: &Theme, x: f32, y_range: egui::Rangef, level: Level) {
    painter.vline(x, y_range, level.stroke(t));
}

#[derive(Clone, Copy)]
#[allow(dead_code)]
pub enum Elevation {
    /// Hairline only.
    Stroke,
    /// Cards.
    Panel,
    /// Menus, modals, popovers.
    Prominent,
    /// The composer card.
    Soft,
}

impl Elevation {
    fn shadow(self, dark: bool) -> egui::epaint::Shadow {
        // Shadows are barely visible on a dark ground; dsh leans on the
        // hairline there, so the blur is halved rather than dropped.
        let k = if dark { 0.5 } else { 1.0 };
        let a = |x: f32| (x * k * 255.0) as u8;
        match self {
            Elevation::Stroke => egui::epaint::Shadow::NONE,
            Elevation::Panel => egui::epaint::Shadow {
                offset: Vec2::new(0.0, 3.0),
                blur: 8.0,
                spread: 0.0,
                color: Color32::from_black_alpha(a(0.05)),
            },
            Elevation::Prominent => egui::epaint::Shadow {
                offset: Vec2::new(0.0, 3.0),
                blur: 12.0,
                spread: 0.0,
                color: Color32::from_black_alpha(a(0.09)),
            },
            Elevation::Soft => egui::epaint::Shadow {
                offset: Vec2::new(0.0, 4.0),
                blur: 16.0,
                spread: 0.0,
                color: Color32::from_black_alpha(a(0.06)),
            },
        }
    }
}

/// An elevated surface: fill + 0.5 px hairline + the elevation's shadow.
pub fn elevated_frame(
    t: &Theme,
    elevation: Elevation,
    stroke: Level,
    fill: Color32,
    rounding: f32,
) -> egui::Frame {
    egui::Frame::none()
        .fill(fill)
        .stroke(stroke.stroke(t))
        .rounding(Rounding::same(rounding))
        .shadow(elevation.shadow(t.dark))
}

/// Plain card: `bg_layer[0]`, hairline, r=12.
pub fn card_frame(t: &Theme) -> egui::Frame {
    elevated_frame(
        t,
        Elevation::Stroke,
        Level::L1,
        col(t.alias.bg_layer[0]),
        RADIUS_CARD,
    )
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Variant {
    /// Ink fill, inverted label — the single most important action.
    Primary,
    /// Transparent, hover wash.
    Ghost,
    /// 0.5 px `border-l3`.
    Outline,
    /// Outline that hovers red.
    Danger,
    /// The blue send circle's flat sibling.
    Info,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Size {
    /// h=36, r=18, 14/22, pad 0 14.
    Md,
    /// h=28, r=14, 12/18, pad 0 10.
    Sm,
}

impl Size {
    fn metrics(self) -> (f32, f32, f32, f32) {
        match self {
            // height, rounding, font size, horizontal padding
            Size::Md => (36.0, RADIUS_BTN, 14.0, 14.0),
            Size::Sm => (28.0, RADIUS_BTN_SM, 12.0, 10.0),
        }
    }
}

pub fn button(ui: &mut Ui, label: &str, variant: Variant, size: Size) -> Response {
    button_enabled(ui, label, variant, size, true)
}

pub fn button_enabled(
    ui: &mut Ui,
    label: &str,
    variant: Variant,
    size: Size,
    enabled: bool,
) -> Response {
    let t = theme(ui);
    let a = &t.alias;
    let (h, r, fs, pad) = size.metrics();
    let font = font(fs, Weight::Medium);
    let galley = ui.fonts(|f| f.layout_no_wrap(label.to_owned(), font, Color32::WHITE));
    let w = galley.size().x + pad * 2.0;
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(w, h),
        if enabled { Sense::click() } else { Sense::hover() },
    );
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let hovered = enabled && resp.hovered();
    let pressed = enabled && resp.is_pointer_button_down_on();
    let (fill, stroke, text_color) = match variant {
        Variant::Primary => (
            if pressed || hovered {
                col(a.primary_hover)
            } else {
                col(a.primary_fill)
            },
            Stroke::NONE,
            col(a.label_inverted),
        ),
        Variant::Info => (
            if pressed || hovered {
                col(a.info_hover)
            } else {
                col(a.info_fill)
            },
            Stroke::NONE,
            Color32::WHITE,
        ),
        Variant::Ghost => (
            if pressed {
                cola(a.active)
            } else if hovered {
                cola(a.hover)
            } else {
                Color32::TRANSPARENT
            },
            Stroke::NONE,
            col(a.label[1]),
        ),
        Variant::Outline => (
            if pressed {
                cola(a.active)
            } else if hovered {
                cola(a.hover)
            } else {
                Color32::TRANSPARENT
            },
            Level::L3.stroke(&t),
            col(a.label[0]),
        ),
        Variant::Danger => (
            if hovered { cola(a.hover_danger) } else { Color32::TRANSPARENT },
            Level::L3.stroke(&t),
            if hovered { col(a.error) } else { col(a.label[0]) },
        ),
    };
    let alpha = if enabled { 1.0 } else { 0.4 };
    let painter = ui.painter();
    painter.rect(
        rect,
        Rounding::same(r),
        fill.linear_multiply(alpha),
        Stroke::new(stroke.width, stroke.color.linear_multiply(alpha)),
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        font_for(fs, Weight::Medium),
        text_color.linear_multiply(alpha),
    );
    resp
}

fn font_for(size: f32, weight: Weight) -> FontId {
    font(size, weight)
}

/// A round icon button — transparent, hover wash, glyph in `label[1]`.
pub fn icon_button(ui: &mut Ui, icon: Icon, px: f32) -> Response {
    icon_button_tinted(ui, icon, px, None)
}

pub fn icon_button_tinted(ui: &mut Ui, icon: Icon, px: f32, tint: Option<Color32>) -> Response {
    let t = theme(ui);
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(px), Sense::click());
    if ui.is_rect_visible(rect) {
        if resp.hovered() {
            ui.painter()
                .circle_filled(rect.center(), px / 2.0, cola(t.alias.hover));
        }
        let glyph = Rect::from_center_size(rect.center(), Vec2::splat(px * 0.56));
        icons::paint(
            ui.painter(),
            glyph,
            icon,
            tint.unwrap_or_else(|| col(t.alias.label[1])),
        );
    }
    resp
}

/// Pill: h=24, r=12, 12/18, `label[1]` on `bg_layer[1]`; active adds an inset
/// ring and promotes the text to `label[0]`.
pub fn pill(ui: &mut Ui, text: &str, active: bool) -> Response {
    let t = theme(ui);
    let font = font(12.0, Weight::Regular);
    let galley = ui.fonts(|f| f.layout_no_wrap(text.to_owned(), font.clone(), Color32::WHITE));
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(galley.size().x + 16.0, 24.0), Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect(
            rect,
            Rounding::same(12.0),
            col(t.alias.bg_layer[1]),
            if active {
                Stroke::new(1.0, col(t.statics.bluish[9]))
            } else {
                Level::L1.stroke(&t)
            },
        );
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            text,
            font,
            if active {
                col(t.alias.label[0])
            } else {
                col(t.alias.label[1])
            },
        );
    }
    resp
}

/// A tinted status pill (approval strip, plan chip, job status).
pub fn tinted_pill(ui: &mut Ui, text: &str, fill: Color32, fg: Color32, size: f32) -> Response {
    let font = font(size, Weight::Medium);
    let galley = ui.fonts(|f| f.layout_no_wrap(text.to_owned(), font.clone(), Color32::WHITE));
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(galley.size().x + 16.0, size + 10.0),
        Sense::click(),
    );
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, Rounding::same(RADIUS_PILL), fill);
        ui.painter()
            .text(rect.center(), Align2::CENTER_CENTER, text, font, fg);
    }
    resp
}

// ---------------------------------------------------------------------------
// State dot
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum DotState {
    Done,
    Warning,
    Error,
    /// 2×2 pixel chase, 1 s, per-cell −125 ms.
    Ongoing,
    Idle,
}

/// 10 px state dot: a 10 % halo with the core inset 20 %.
pub fn state_dot(ui: &mut Ui, state: DotState, px: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(px), Sense::hover());
    if ui.is_rect_visible(rect) {
        paint_state_dot(ui.painter(), rect, state, &theme(ui), ui.input(|i| i.time) as f32);
        if state == DotState::Ongoing {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
        }
    }
    resp
}

pub fn paint_state_dot(
    painter: &egui::Painter,
    rect: Rect,
    state: DotState,
    t: &Theme,
    time: f32,
) {
    let a = &t.alias;
    let color = match state {
        DotState::Done => col(a.success),
        DotState::Warning => col(a.warn),
        DotState::Error => col(a.error),
        DotState::Ongoing => col(t.statics.accent[4]),
        DotState::Idle => col(a.label[3]),
    };
    let px = rect.width().min(rect.height());
    if state == DotState::Ongoing {
        // Pixel chase: a 2×2 grid, each cell lit on its own 125 ms offset.
        let cell = px * 0.42;
        let gap = px * 0.10;
        let origin = rect.center() - Vec2::splat(cell + gap * 0.5);
        let order = [(0, 0), (1, 0), (1, 1), (0, 1)];
        let phase = (time / 1.0).fract() * 4.0;
        for (i, (cx, cy)) in order.iter().enumerate() {
            let d = (phase - i as f32).rem_euclid(4.0);
            let lit = (1.0 - d / 2.0).clamp(0.15, 1.0);
            let r = Rect::from_min_size(
                origin + Vec2::new(*cx as f32 * (cell + gap), *cy as f32 * (cell + gap)),
                Vec2::splat(cell),
            );
            painter.rect_filled(r, Rounding::same(1.0), color.linear_multiply(lit));
        }
        return;
    }
    painter.circle_filled(rect.center(), px * 0.5, color.linear_multiply(0.18));
    painter.circle_filled(rect.center(), px * 0.30, color);
}

// ---------------------------------------------------------------------------
// Disclosure row — the transcript's workhorse
// ---------------------------------------------------------------------------

/// What decorates the leading 16 px box of a [`disclosure_row`].
#[derive(Clone, Copy)]
pub enum Leading {
    Icon(Icon),
    Dot(DotState),
}

pub struct RowOutput {
    pub response: Response,
    pub clicked: bool,
}

/// `[16px leading] 6 [title 13] 8 [2×2 dot] 8 [summary, ellipsized] [suffix]`,
/// h = `ROW_H + δ`. The leading box crossfades to a chevron on hover, which
/// is how dsh tells you a row opens without spending a column on a caret.
/// `running` paints the 2.6 s sweep glare over the row.
pub fn disclosure_row(
    ui: &mut Ui,
    leading: Leading,
    title: &str,
    summary: &str,
    open: bool,
    running: bool,
    suffix: Option<(&str, Color32)>,
) -> RowOutput {
    let t = theme(ui);
    let a = &t.alias;
    let h = (sica_core::theme::tokens::ROW_H + t.delta()).max(20.0);
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, h), Sense::click());
    if !ui.is_rect_visible(rect) {
        return RowOutput { clicked: resp.clicked(), response: resp };
    }
    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(rect.expand2(Vec2::new(4.0, 0.0)), Rounding::same(RADIUS_ITEM), cola(a.hover));
    }

    let title_size = t.content_secondary_px();
    let mut x = rect.min.x;
    // Leading box.
    let lead = Rect::from_center_size(Pos2::new(x + 8.0, rect.center().y), Vec2::splat(14.0));
    if resp.hovered() {
        icons::paint(
            painter,
            lead,
            if open { Icon::ChevronDown } else { Icon::ChevronRight },
            col(a.label[2]),
        );
    } else {
        match leading {
            Leading::Icon(icon) => icons::paint(painter, lead, icon, col(a.label[2])),
            Leading::Dot(state) => paint_state_dot(
                painter,
                Rect::from_center_size(lead.center(), Vec2::splat(10.0)),
                state,
                &t,
                ui.input(|i| i.time) as f32,
            ),
        }
    }
    x += 16.0 + 6.0;

    // Title.
    let title_font = font(title_size, Weight::Medium);
    let title_galley = painter.layout_no_wrap(title.to_owned(), title_font, col(a.label[1]));
    painter.galley(
        Pos2::new(x, rect.center().y - title_galley.size().y / 2.0),
        title_galley.clone(),
        col(a.label[1]),
    );
    x += title_galley.size().x;

    // Suffix reserve (diff stat, count).
    let mut right = rect.max.x;
    if let Some((text, color)) = suffix {
        let f = mono_font(11.0);
        let g = painter.layout_no_wrap(text.to_owned(), f, color);
        painter.galley(
            Pos2::new(right - g.size().x, rect.center().y - g.size().y / 2.0),
            g.clone(),
            color,
        );
        right -= g.size().x + 8.0;
    }

    // Separator dot + summary.
    if !summary.is_empty() {
        x += 8.0;
        painter.circle_filled(Pos2::new(x + 1.0, rect.center().y), 1.0, col(a.label[3]));
        x += 2.0 + 8.0;
        let avail = (right - x).max(0.0);
        let f = font(title_size, Weight::Regular);
        let shown = elide(ui, summary, &f, avail);
        ui.painter().text(
            Pos2::new(x, rect.center().y),
            Align2::LEFT_CENTER,
            shown,
            f,
            col(a.label[2]),
        );
    }

    if running {
        sweep_glare(ui, rect);
    }
    RowOutput { clicked: resp.clicked(), response: resp }
}

/// The 2.6 s glare that rides a running row: a 300 px band of `bg_base` at
/// 60 % gliding left→right.
pub fn sweep_glare(ui: &mut Ui, rect: Rect) {
    let t = theme(ui);
    let time = ui.input(|i| i.time) as f32;
    let band = 300.0_f32.min(rect.width().max(80.0));
    let travel = rect.width() + band;
    let x = (time / SWEEP_PERIOD).fract() * travel - band;
    let painter = ui.painter().with_clip_rect(rect);
    let base = col(t.alias.bg_base);
    let mut mesh = egui::Mesh::default();
    let steps = 8;
    for i in 0..=steps {
        let f = i as f32 / steps as f32;
        // Triangular falloff — brightest in the middle of the band.
        let alpha = (1.0 - (f - 0.5).abs() * 2.0) * 0.6;
        let cx = rect.min.x + x + band * f;
        let color = base.linear_multiply(alpha);
        mesh.colored_vertex(Pos2::new(cx, rect.min.y), color);
        mesh.colored_vertex(Pos2::new(cx, rect.max.y), color);
        if i < steps {
            let base_i = (i * 2) as u32;
            mesh.add_triangle(base_i, base_i + 1, base_i + 2);
            mesh.add_triangle(base_i + 1, base_i + 2, base_i + 3);
        }
    }
    painter.add(Shape::mesh(mesh));
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(16));
}

/// Shimmering gradient text — the turn-status line. Per-glyph colour sampled
/// from a moving accent gradient, 1.8 s linear.
pub fn shimmer_text(ui: &mut Ui, text: &str, size: f32) -> Response {
    let t = theme(ui);
    let time = ui.input(|i| i.time) as f32;
    let phase = (time / sica_core::theme::tokens::SHIMMER_PERIOD).fract();
    let hi = col(t.statics.accent[2]);
    let lo = col(t.statics.accent[5]);
    let mut job = LayoutJob::default();
    let n = text.chars().count().max(1);
    for (i, ch) in text.chars().enumerate() {
        let f = i as f32 / n as f32;
        // A 2.5-wide window sliding across the string.
        let d = ((f - phase * 1.5 + 0.25).rem_euclid(1.0) - 0.5).abs() * 2.0;
        let mix = 1.0 - d;
        let color = Color32::from_rgb(
            lerp_u8(lo.r(), hi.r(), mix),
            lerp_u8(lo.g(), hi.g(), mix),
            lerp_u8(lo.b(), hi.b(), mix),
        );
        job.append(
            &ch.to_string(),
            0.0,
            TextFormat {
                font_id: font(size, Weight::Medium),
                color,
                ..Default::default()
            },
        );
    }
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
    ui.add(egui::Label::new(job).selectable(false))
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t.clamp(0.0, 1.0)).round() as u8
}

// ---------------------------------------------------------------------------
// Menus
// ---------------------------------------------------------------------------

pub struct MenuItem {
    pub label: String,
    pub detail: Option<String>,
    pub checked: bool,
    pub danger: bool,
    pub sep_above: bool,
}

impl MenuItem {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: None,
            checked: false,
            danger: false,
            sep_above: false,
        }
    }
    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }
    pub fn checked(mut self, c: bool) -> Self {
        self.checked = c;
        self
    }
    pub fn danger(mut self, d: bool) -> Self {
        self.danger = d;
        self
    }
    pub fn sep_above(mut self, s: bool) -> Self {
        self.sep_above = s;
        self
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MenuSide {
    Above,
    Below,
}

/// An open menu anchored to `anchor`. The caller owns `open`; this closes it
/// on pick, outside press or Esc. **Selection shows as a trailing check,
/// never a fill** — dsh is emphatic about that.
pub fn menu(
    ctx: &egui::Context,
    id: egui::Id,
    anchor: Rect,
    side: MenuSide,
    min_width: f32,
    items: &[MenuItem],
    open: &mut bool,
) -> Option<usize> {
    if !*open {
        return None;
    }
    let t = theme_ctx(ctx);
    let a = &t.alias;
    let mut picked = None;

    let item_h = 40.0;
    let sep_count = items.iter().filter(|i| i.sep_above).count() as f32;
    let height = items.len() as f32 * item_h + sep_count * 5.0 + 8.0;
    let pos = match side {
        MenuSide::Above => Pos2::new(anchor.min.x, anchor.min.y - height - 6.0),
        MenuSide::Below => Pos2::new(anchor.min.x, anchor.max.y + 6.0),
    };
    // Keep the card on screen.
    let screen = ctx.screen_rect();
    let width = min_width.max(anchor.width());
    let pos = Pos2::new(
        pos.x.min(screen.max.x - width - 8.0).max(screen.min.x + 8.0),
        pos.y.min(screen.max.y - height - 8.0).max(screen.min.y + 8.0),
    );

    let area = egui::Area::new(id)
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            ui.set_width(width);
            elevated_frame(&t, Elevation::Prominent, Level::L1, col(a.menu), RADIUS_MENU)
                .inner_margin(egui::Margin::same(4.0))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for (i, item) in items.iter().enumerate() {
                        if item.sep_above {
                            ui.add_space(2.0);
                            let (r, _) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width() - 8.0, 1.0),
                                Sense::hover(),
                            );
                            ui.painter().hline(
                                r.x_range(),
                                r.center().y,
                                Level::L1.stroke(&t),
                            );
                            ui.add_space(2.0);
                        }
                        let (rect, resp) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), item_h),
                            Sense::click(),
                        );
                        let fg = if item.danger {
                            col(a.error)
                        } else {
                            col(a.label[0])
                        };
                        if resp.hovered() {
                            ui.painter().rect_filled(
                                rect,
                                Rounding::same(RADIUS_ITEM),
                                if item.danger { cola(a.hover_danger) } else { cola(a.hover) },
                            );
                        }
                        let text_x = rect.min.x + 10.0;
                        let has_detail = item.detail.is_some();
                        let label_y = if has_detail {
                            rect.center().y - 8.0
                        } else {
                            rect.center().y
                        };
                        ui.painter().text(
                            Pos2::new(text_x, label_y),
                            Align2::LEFT_CENTER,
                            &item.label,
                            font(14.0, Weight::Regular),
                            fg,
                        );
                        if let Some(detail) = &item.detail {
                            ui.painter().text(
                                Pos2::new(text_x, rect.center().y + 8.0),
                                Align2::LEFT_CENTER,
                                elide(
                                    ui,
                                    detail,
                                    &font(12.0, Weight::Regular),
                                    rect.width() - 40.0,
                                ),
                                font(12.0, Weight::Regular),
                                col(a.label[2]),
                            );
                        }
                        if item.checked {
                            icons::paint(
                                ui.painter(),
                                Rect::from_center_size(
                                    Pos2::new(rect.max.x - 16.0, rect.center().y),
                                    Vec2::splat(14.0),
                                ),
                                Icon::Check,
                                col(a.business),
                            );
                        }
                        if resp.clicked() {
                            picked = Some(i);
                        }
                    }
                });
        });

    let area_rect = area.response.rect;
    let pointer_outside = ctx.input(|i| {
        i.pointer.any_pressed()
            && i.pointer
                .interact_pos()
                .map(|p| !area_rect.contains(p) && !anchor.contains(p))
                .unwrap_or(false)
    });
    if picked.is_some() || pointer_outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        *open = false;
    }
    picked
}

// ---------------------------------------------------------------------------
// Modal
// ---------------------------------------------------------------------------

pub struct ModalOutput<R> {
    pub inner: R,
    /// The mask was clicked, Esc was pressed, or the × was hit.
    pub dismissed: bool,
}

/// Full-window mask + a centred dialog. `width`/`max_height` in points;
/// pass `None` for the close button to make the modal non-dismissible.
pub fn modal<R>(
    ctx: &egui::Context,
    id: egui::Id,
    title: &str,
    width: f32,
    dismissible: bool,
    body: impl FnOnce(&mut Ui) -> R,
) -> ModalOutput<R> {
    let t = theme_ctx(ctx);
    let a = &t.alias;
    let screen = ctx.screen_rect();

    mask(ctx, id.with("mask"));

    let w = width.min(screen.width() - 48.0);
    let max_h = (screen.height() - 48.0).max(240.0);
    let mut closed = false;
    let inner = egui::Area::new(id)
        .order(egui::Order::Foreground)
        .fixed_pos(Pos2::new(
            screen.center().x - w / 2.0,
            (screen.center().y - max_h / 2.0).max(screen.min.y + 24.0),
        ))
        .show(ctx, |ui| {
            ui.set_width(w);
            ui.set_max_height(max_h);
            elevated_frame(
                &t,
                Elevation::Prominent,
                Level::L1,
                col(a.bg_layer[1]),
                sica_core::theme::tokens::RADIUS_MODAL,
            )
            .inner_margin(egui::Margin {
                left: 24.0,
                right: 14.0,
                top: 18.0,
                bottom: 20.0,
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    label(ui, txt(title, 16.0, Weight::Medium, col(a.label[0])));
                    if dismissible {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if icon_button(ui, Icon::Close, 28.0).clicked() {
                                closed = true;
                            }
                        });
                    }
                });
                ui.add_space(10.0);
                ui.scope(|ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    body(ui)
                })
                .inner
            })
            .inner
        })
        .inner;

    let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    let dialog_rect = ctx.memory(|m| m.area_rect(id)).unwrap_or(screen);
    let pressed_outside = ctx.input(|i| {
        i.pointer.any_pressed()
            && i.pointer
                .interact_pos()
                .map(|p| !dialog_rect.contains(p))
                .unwrap_or(false)
    });
    ModalOutput {
        inner,
        dismissed: dismissible && (closed || esc || pressed_outside),
    }
}

/// The full-window scrim behind a modal: `rgba(0,0,0,.24)` painted on a layer
/// above the panels. Painted rather than allocated, so it never competes for
/// the pointer with the dialog it sits behind.
pub fn mask(ctx: &egui::Context, id: egui::Id) {
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Middle, id));
    painter.rect_filled(
        ctx.screen_rect(),
        Rounding::ZERO,
        Color32::from_black_alpha(61),
    );
}

// ---------------------------------------------------------------------------
// Toast
// ---------------------------------------------------------------------------

/// One toast. dsh has no severity kinds — the caller picks the icon.
#[derive(Clone)]
pub struct Toast {
    pub seq: u64,
    pub icon: Icon,
    pub text: String,
    pub created: Instant,
    pub hold_ms: u64,
}

impl Toast {
    pub fn new(seq: u64, icon: Icon, text: impl Into<String>, hold_ms: u64) -> Self {
        Self {
            seq,
            icon,
            text: text.into(),
            created: Instant::now(),
            hold_ms,
        }
    }
    /// 0 while held, ramping to 1 across the 1 s fade; `>= 1` means retire.
    pub fn fade(&self) -> f32 {
        let ms = self.created.elapsed().as_millis() as u64;
        if ms <= self.hold_ms {
            0.0
        } else {
            (ms - self.hold_ms) as f32 / sica_core::theme::tokens::TOAST_FADE_MS as f32
        }
    }
}

/// Top-centre of the conversation column, 120 px down. Returns `false` once
/// the toast has fully faded, so the caller can drop it.
pub fn toast(ctx: &egui::Context, toast: &Toast, column: Rect) -> bool {
    let fade = toast.fade();
    if fade >= 1.0 {
        return false;
    }
    let t = theme_ctx(ctx);
    let alpha = 1.0 - fade;
    // 160 ms slide-in.
    let age = toast.created.elapsed().as_millis() as f32;
    let slide = (1.0 - (age / 160.0).min(1.0)) * 12.0;
    egui::Area::new(egui::Id::new(("toast", toast.seq)))
        .order(egui::Order::Tooltip)
        .fixed_pos(Pos2::new(column.center().x - 160.0, column.min.y + 120.0 - slide))
        .interactable(false)
        .show(ctx, |ui| {
            ui.set_max_width(320.0);
            egui::Frame::none()
                .fill(col(t.alias.toast_bg).linear_multiply(alpha))
                .rounding(Rounding::same(RADIUS_BTN_SM))
                .inner_margin(egui::Margin::symmetric(14.0, 10.0))
                .shadow(Elevation::Prominent.shadow(t.dark))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        icons::show(ui, toast.icon, 14.0, Color32::WHITE.linear_multiply(alpha));
                        ui.add_space(8.0);
                        ui.add(
                            egui::Label::new(txt(
                                &toast.text,
                                14.0,
                                Weight::Regular,
                                Color32::WHITE.linear_multiply(alpha),
                            ))
                            .selectable(false)
                            .wrap(),
                        );
                    });
                });
        });
    ctx.request_repaint_after(std::time::Duration::from_millis(50));
    true
}

// ---------------------------------------------------------------------------
// Connection indicator
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Healthy,
    Connecting,
    Disconnected,
    Recovered,
}

/// h=32, r=8, 12/18 500. **Hidden when healthy** — the whole point is that a
/// working connection says nothing.
pub fn connection_indicator(ui: &mut Ui, state: ConnState) -> Option<Response> {
    let t = theme(ui);
    let a = &t.alias;
    let (text, fill, fg) = match state {
        ConnState::Healthy => return None,
        ConnState::Connecting => {
            // Dots advance every 500 ms.
            let n = ((ui.input(|i| i.time) * 2.0) as usize) % 4;
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
            (
                format!("Connecting{}", ".".repeat(n)),
                col(a.warn_tertiary),
                col(a.warn_label),
            )
        }
        ConnState::Disconnected => (
            "Disconnected".to_string(),
            col(a.warn_tertiary),
            col(a.warn_label),
        ),
        ConnState::Recovered => (
            "Connected".to_string(),
            col(a.success_tertiary),
            col(a.success),
        ),
    };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(112.0, 32.0), Sense::click());
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, Rounding::same(RADIUS_INPUT), fill);
        let shown = if resp.hovered() && state != ConnState::Recovered {
            "Reconnect now".to_string()
        } else {
            text
        };
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            shown,
            font(12.0, Weight::Medium),
            fg,
        );
    }
    Some(resp)
}

// ---------------------------------------------------------------------------
// Code / IN-OUT / terminal blocks
// ---------------------------------------------------------------------------

/// r=12 card, sticky banner with the infostring and a Copy → "Copied"
/// (1000 ms) button, body 11/19 mono.
pub fn code_block(ui: &mut Ui, lang: &str, text: &str) {
    let t = theme(ui);
    let a = &t.alias;
    let id = ui.id().with(("copied", text.len(), lang));
    egui::Frame::none()
        .fill(col(a.code_block))
        .stroke(Level::L1.stroke(&t))
        .rounding(Rounding::same(RADIUS_CARD))
        .inner_margin(egui::Margin::ZERO)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Banner.
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                ui.add_space(0.0);
                label(
                    ui,
                    mono(if lang.is_empty() { "text" } else { lang }, 11.0, col(a.label[3])),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(8.0);
                    let copied_at: Option<f64> = ui.ctx().data(|d| d.get_temp(id));
                    let now = ui.input(|i| i.time);
                    let just_copied = copied_at.map(|t0| now - t0 < 1.0).unwrap_or(false);
                    if just_copied {
                        label(ui, txt("Copied", 11.0, Weight::Medium, col(a.success)));
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
                    } else if label(
                        ui,
                        txt("Copy", 11.0, Weight::Medium, col(a.label[2])),
                    )
                    .interact(Sense::click())
                    .clicked()
                    {
                        ui.output_mut(|o| o.copied_text = text.to_owned());
                        ui.ctx().data_mut(|d| d.insert_temp(id, now));
                    }
                });
            });
            ui.add_space(2.0);
            hairline(ui, Level::L1);
            egui::Frame::none()
                .inner_margin(egui::Margin::symmetric(14.0, 10.0))
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(mono(text, 11.0, col(a.label[0])))
                            .wrap(),
                    );
                });
        });
}

/// The generic tool body: two sections, `IN` and `OUT`, each independently
/// scrollable to 150 px, 11/16 mono, gutter labels in `label[3]`.
pub fn io_card(ui: &mut Ui, input: &str, output: &str, failed: bool) {
    let t = theme(ui);
    let a = &t.alias;
    egui::Frame::none()
        .fill(col(a.code_block))
        .stroke(Level::L1.stroke(&t))
        .rounding(Rounding::same(RADIUS_CARD))
        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            io_section(ui, "IN", input, col(a.label[0]), &t, 0);
            ui.add_space(6.0);
            let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
            ui.painter().hline(r.x_range(), r.center().y, Level::L2.stroke(&t));
            ui.add_space(6.0);
            io_section(
                ui,
                "OUT",
                if output.is_empty() { "(no output)" } else { output },
                if failed { col(a.error) } else { col(a.label[0]) },
                &t,
                1,
            );
        });
}

fn io_section(ui: &mut Ui, gutter: &str, text: &str, color: Color32, t: &Theme, nonce: usize) {
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(30.0, 16.0), Sense::hover());
        ui.painter().text(
            rect.left_top(),
            Align2::LEFT_TOP,
            gutter,
            mono_font(11.0),
            col(t.alias.label[3]),
        );
        egui::ScrollArea::vertical()
            .id_source(ui.id().with(("io", nonce)))
            .max_height(150.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.add(egui::Label::new(mono(text, 11.0, color)).wrap());
            });
    });
}

/// A read-only line of key/value chrome under a body — the "N matches ·
/// M files" footers.
pub fn footnote(ui: &mut Ui, text: &str) {
    let t = theme(ui);
    label(ui, txt(text, 11.0, Weight::Regular, col(t.alias.label[2])));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn over_composites_like_the_painter_would() {
        let base = Color32::from_rgb(20, 30, 40);
        // A transparent wash changes nothing…
        assert_eq!(over(base, Color32::TRANSPARENT), base);
        // …an opaque one replaces the base entirely…
        let opaque = Color32::from_rgb(200, 100, 50);
        assert_eq!(over(base, opaque), opaque);
        // …and a partial wash lands strictly between the two.
        let half = over(base, Color32::from_rgba_unmultiplied(255, 255, 255, 128));
        assert!(half.r() > base.r() && half.r() < 255, "{half:?}");
    }

    #[test]
    fn one_line_collapses_and_caps() {
        assert_eq!(one_line("a\n  b\tc", 80), "a b c");
        assert_eq!(one_line("abcdef", 3), "abc…");
    }

    #[test]
    fn toast_retires_after_hold_plus_fade() {
        let t = Toast::new(1, Icon::Warning, "x", 0);
        // Hold of zero means the fade starts immediately and completes in 1 s.
        assert!(t.fade() < 1.0);
    }
}
