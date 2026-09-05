//! The icon set. dsh ships 74 `currentColor` SVGs; the ~30 glyphs the shell
//! actually needs are hand-painted with `Painter` instead. Every glyph is
//! described in a unit square and scaled into the rect it is given, so one
//! enum serves the 8/12/14/16/20 px sizes dsh uses — and a 1.4 px stroke laid
//! down by the painter stays crisp at 12 px where a downscaled raster would
//! not.
//!
//! Strokes are single-weight (1.4 px at 16 px, scaled with the box) and every
//! glyph is drawn in one colour, so an icon inherits the row's label tier
//! exactly like `currentColor` does.
//!
//! [`mark`] is the exception: the brand mark has curves, a mask and (in the
//! app-icon variant) a gradient, so it stays an SVG and is rasterised by
//! resvg — see [`crate::icon`].

use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, Vec2};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)]
pub enum Icon {
    // Tool-row variants (§3.4).
    Search,
    Read,
    Bash,
    Write,
    Edit,
    Code,
    Sparkle,
    Think,
    // Structure.
    ChevronDown,
    ChevronRight,
    ChevronUp,
    Check,
    Close,
    Plus,
    Dots,
    ArrowUp,
    ArrowDown,
    Stop,
    Copy,
    Refresh,
    Branch,
    // Shell.
    Gear,
    Panel,
    Folder,
    Trash,
    NewChat,
    // Control plane.
    Shield,
    Goal,
    Queue,
    Job,
    Todo,
    Inject,
    Compact,
    Warning,
    Model,
    // Appearance cubes.
    Sun,
    Moon,
    Monitor,
}

impl Icon {
    /// Variant icon for a skill name — the mapping dsh applies to tool calls.
    pub fn for_skill(skill: &str) -> Self {
        match skill {
            "glob" | "grep" => Icon::Search,
            "read-file" => Icon::Read,
            "run-cli" | "run-pwsh" | "job-output" | "job-list" | "job-kill" => Icon::Bash,
            "write-file" => Icon::Write,
            "edit-file" => Icon::Edit,
            "skill-creator" | "model-eval" => Icon::Code,
            "todo-write" => Icon::Todo,
            "create-goal" | "get-goal" | "update-goal" => Icon::Goal,
            _ => Icon::Sparkle,
        }
    }
}

/// Paint `icon` inside `rect` in `color`. Stroke weight scales with the box.
pub fn paint(painter: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let w = rect.width().min(rect.height());
    let sw = (w / 16.0 * 1.4).max(1.0);
    let s = Stroke::new(sw, color);
    // Unit-square → rect.
    let p = |x: f32, y: f32| -> Pos2 {
        Pos2::new(
            rect.center().x + (x - 0.5) * w,
            rect.center().y + (y - 0.5) * w,
        )
    };
    let line = |a: (f32, f32), b: (f32, f32)| {
        painter.line_segment([p(a.0, a.1), p(b.0, b.1)], s);
    };
    let poly = |pts: &[(f32, f32)]| {
        painter.add(Shape::line(
            pts.iter().map(|&(x, y)| p(x, y)).collect::<Vec<_>>(),
            s,
        ));
    };
    let rect_stroke = |x0: f32, y0: f32, x1: f32, y1: f32, r: f32| {
        painter.rect_stroke(
            Rect::from_min_max(p(x0, y0), p(x1, y1)),
            egui::Rounding::same(r * w),
            s,
        );
    };

    match icon {
        Icon::Search => {
            painter.circle_stroke(p(0.44, 0.44), 0.26 * w, s);
            line((0.63, 0.63), (0.84, 0.84));
        }
        Icon::Read => {
            // A page with a folded corner.
            poly(&[(0.24, 0.12), (0.62, 0.12), (0.78, 0.30), (0.78, 0.88), (0.24, 0.88), (0.24, 0.12)]);
            poly(&[(0.62, 0.12), (0.62, 0.30), (0.78, 0.30)]);
            line((0.34, 0.52), (0.68, 0.52));
            line((0.34, 0.68), (0.60, 0.68));
        }
        Icon::Bash => {
            // Terminal: prompt chevron + caret line.
            rect_stroke(0.12, 0.18, 0.88, 0.82, 0.12);
            poly(&[(0.28, 0.38), (0.42, 0.50), (0.28, 0.62)]);
            line((0.50, 0.64), (0.72, 0.64));
        }
        Icon::Write | Icon::Edit => {
            // Pencil on a baseline.
            poly(&[(0.20, 0.74), (0.68, 0.26), (0.80, 0.38), (0.32, 0.86), (0.18, 0.88), (0.20, 0.74)]);
            line((0.58, 0.36), (0.70, 0.48));
        }
        Icon::Code => {
            poly(&[(0.36, 0.28), (0.16, 0.50), (0.36, 0.72)]);
            poly(&[(0.64, 0.28), (0.84, 0.50), (0.64, 0.72)]);
        }
        Icon::Sparkle => {
            poly(&[(0.50, 0.10), (0.60, 0.40), (0.90, 0.50), (0.60, 0.60), (0.50, 0.90), (0.40, 0.60), (0.10, 0.50), (0.40, 0.40), (0.50, 0.10)]);
        }
        Icon::Think => {
            // A thought: rounded cloud arc plus two trailing dots.
            painter.circle_stroke(p(0.48, 0.42), 0.30 * w, s);
            painter.circle_filled(p(0.30, 0.80), 0.06 * w, color);
            painter.circle_filled(p(0.18, 0.92), 0.04 * w, color);
        }
        Icon::ChevronDown => poly(&[(0.26, 0.40), (0.50, 0.64), (0.74, 0.40)]),
        Icon::ChevronUp => poly(&[(0.26, 0.62), (0.50, 0.38), (0.74, 0.62)]),
        Icon::ChevronRight => poly(&[(0.40, 0.26), (0.64, 0.50), (0.40, 0.74)]),
        Icon::Check => poly(&[(0.22, 0.52), (0.42, 0.72), (0.78, 0.30)]),
        Icon::Close => {
            line((0.26, 0.26), (0.74, 0.74));
            line((0.74, 0.26), (0.26, 0.74));
        }
        Icon::Plus => {
            line((0.50, 0.22), (0.50, 0.78));
            line((0.22, 0.50), (0.78, 0.50));
        }
        Icon::Dots => {
            for x in [0.24, 0.50, 0.76] {
                painter.circle_filled(p(x, 0.50), 0.07 * w, color);
            }
        }
        Icon::ArrowUp => {
            line((0.50, 0.80), (0.50, 0.22));
            poly(&[(0.28, 0.44), (0.50, 0.22), (0.72, 0.44)]);
        }
        Icon::ArrowDown => {
            line((0.50, 0.20), (0.50, 0.78));
            poly(&[(0.28, 0.56), (0.50, 0.78), (0.72, 0.56)]);
        }
        Icon::Stop => {
            painter.rect_filled(
                Rect::from_min_max(p(0.30, 0.30), p(0.70, 0.70)),
                egui::Rounding::same(0.08 * w),
                color,
            );
        }
        Icon::Copy => {
            rect_stroke(0.30, 0.14, 0.86, 0.70, 0.10);
            poly(&[(0.70, 0.86), (0.14, 0.86), (0.14, 0.30)]);
        }
        Icon::Branch => {
            // Git's fork mark: a trunk with two nodes and an arm reaching a
            // third — "carry this history into a conversation of its own".
            painter.circle_stroke(p(0.30, 0.22), 0.10 * w, s);
            painter.circle_stroke(p(0.30, 0.80), 0.10 * w, s);
            painter.circle_stroke(p(0.74, 0.30), 0.10 * w, s);
            line((0.30, 0.32), (0.30, 0.70));
            poly(&[(0.74, 0.40), (0.74, 0.50), (0.30, 0.58)]);
        }
        Icon::Refresh => {
            // Three-quarter ring with an arrow head.
            arc(painter, p(0.50, 0.50), 0.32 * w, -2.2, 1.9, s);
            poly(&[(0.62, 0.10), (0.84, 0.22), (0.66, 0.36)]);
        }
        Icon::Gear => {
            painter.circle_stroke(p(0.50, 0.50), 0.18 * w, s);
            for i in 0..6 {
                let a = std::f32::consts::TAU * (i as f32) / 6.0;
                let (sx, sy) = (a.cos(), a.sin());
                painter.line_segment(
                    [
                        Pos2::new(rect.center().x + sx * 0.26 * w, rect.center().y + sy * 0.26 * w),
                        Pos2::new(rect.center().x + sx * 0.42 * w, rect.center().y + sy * 0.42 * w),
                    ],
                    s,
                );
            }
        }
        Icon::Panel => {
            rect_stroke(0.12, 0.20, 0.88, 0.80, 0.10);
            line((0.40, 0.20), (0.40, 0.80));
        }
        Icon::Folder => {
            poly(&[(0.12, 0.76), (0.12, 0.24), (0.42, 0.24), (0.52, 0.36), (0.88, 0.36), (0.88, 0.76), (0.12, 0.76)]);
        }
        Icon::Trash => {
            line((0.16, 0.28), (0.84, 0.28));
            poly(&[(0.26, 0.28), (0.32, 0.86), (0.68, 0.86), (0.74, 0.28)]);
            line((0.40, 0.18), (0.60, 0.18));
        }
        Icon::NewChat => {
            poly(&[(0.14, 0.72), (0.14, 0.18), (0.86, 0.18), (0.86, 0.62), (0.42, 0.62), (0.24, 0.84), (0.24, 0.62), (0.14, 0.62)]);
            line((0.36, 0.40), (0.64, 0.40));
        }
        Icon::Shield => {
            poly(&[(0.50, 0.12), (0.82, 0.26), (0.82, 0.52), (0.50, 0.88), (0.18, 0.52), (0.18, 0.26), (0.50, 0.12)]);
        }
        Icon::Goal => {
            painter.circle_stroke(p(0.50, 0.50), 0.36 * w, s);
            painter.circle_stroke(p(0.50, 0.50), 0.16 * w, s);
            painter.circle_filled(p(0.50, 0.50), 0.05 * w, color);
        }
        Icon::Queue => {
            for y in [0.28, 0.50, 0.72] {
                line((0.18, y), (0.82, y));
            }
        }
        Icon::Job => {
            painter.circle_stroke(p(0.50, 0.50), 0.34 * w, s);
            line((0.50, 0.50), (0.50, 0.30));
            line((0.50, 0.50), (0.66, 0.58));
        }
        Icon::Todo => {
            rect_stroke(0.14, 0.14, 0.86, 0.86, 0.14);
            poly(&[(0.30, 0.52), (0.44, 0.66), (0.72, 0.34)]);
        }
        Icon::Inject => {
            line((0.50, 0.14), (0.50, 0.58));
            poly(&[(0.32, 0.42), (0.50, 0.60), (0.68, 0.42)]);
            poly(&[(0.16, 0.66), (0.16, 0.86), (0.84, 0.86), (0.84, 0.66)]);
        }
        Icon::Compact => {
            poly(&[(0.30, 0.20), (0.50, 0.40), (0.70, 0.20)]);
            poly(&[(0.30, 0.80), (0.50, 0.60), (0.70, 0.80)]);
            line((0.18, 0.50), (0.82, 0.50));
        }
        Icon::Warning => {
            poly(&[(0.50, 0.14), (0.90, 0.84), (0.10, 0.84), (0.50, 0.14)]);
            line((0.50, 0.40), (0.50, 0.62));
            painter.circle_filled(p(0.50, 0.74), 0.05 * w, color);
        }
        Icon::Model => {
            // A cube: the "model" mark.
            poly(&[(0.50, 0.12), (0.86, 0.32), (0.86, 0.70), (0.50, 0.90), (0.14, 0.70), (0.14, 0.32), (0.50, 0.12)]);
            poly(&[(0.14, 0.32), (0.50, 0.52), (0.86, 0.32)]);
            line((0.50, 0.52), (0.50, 0.90));
        }
        Icon::Sun => {
            painter.circle_stroke(p(0.50, 0.50), 0.22 * w, s);
            for i in 0..8 {
                let a = std::f32::consts::TAU * (i as f32) / 8.0;
                let (sx, sy) = (a.cos(), a.sin());
                painter.line_segment(
                    [
                        Pos2::new(rect.center().x + sx * 0.32 * w, rect.center().y + sy * 0.32 * w),
                        Pos2::new(rect.center().x + sx * 0.44 * w, rect.center().y + sy * 0.44 * w),
                    ],
                    s,
                );
            }
        }
        Icon::Moon => {
            // Crescent: a filled disc with a bite taken by the background is
            // not available, so draw the arc pair.
            arc(painter, p(0.54, 0.50), 0.34 * w, 0.6, 5.0, s);
            arc(painter, p(0.34, 0.50), 0.40 * w, -0.9, 0.9, s);
        }
        Icon::Monitor => {
            rect_stroke(0.12, 0.20, 0.88, 0.66, 0.08);
            line((0.34, 0.84), (0.66, 0.84));
            line((0.50, 0.66), (0.50, 0.84));
        }
    }
}

/// Stroke an arc from `a0` to `a1` radians. epaint has no arc primitive; 20
/// segments is smooth at icon sizes.
fn arc(painter: &Painter, center: Pos2, radius: f32, a0: f32, a1: f32, stroke: Stroke) {
    const SEGMENTS: usize = 20;
    let pts = (0..=SEGMENTS)
        .map(|i| {
            let a = a0 + (a1 - a0) * (i as f32 / SEGMENTS as f32);
            Pos2::new(center.x + radius * a.cos(), center.y + radius * a.sin())
        })
        .collect::<Vec<_>>();
    painter.add(Shape::line(pts, stroke));
}

/// Allocate a `size`-square and paint `icon` into it.
pub fn show(ui: &mut egui::Ui, icon: Icon, size: f32, color: Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        paint(ui.painter(), rect, icon, color);
    }
    resp
}

/// The sica mark — the blade-and-loop logo, rasterised from
/// `assets/mark.svg`.
///
/// The asset is pure white with a shaped alpha, so one texture is uploaded
/// per process and tinted at paint time: the mark inherits a theme colour the
/// same way a `currentColor` SVG would. The source viewBox is square, so a
/// square is centred inside whatever `rect` the caller allocates.
pub fn mark(ui: &egui::Ui, rect: Rect, color: Color32) {
    let side = rect.width().min(rect.height());
    let square = Rect::from_center_size(rect.center(), Vec2::splat(side));
    let tex = mark_texture(ui.ctx());
    egui::Image::new(egui::load::SizedTexture::from_handle(&tex))
        .tint(color)
        .paint_at(ui, square);
}

/// Upload-once cache for the mark. Lives in `Context` memory rather than a
/// `static` because a `TextureHandle` is owned by the context that made it.
fn mark_texture(ctx: &egui::Context) -> egui::TextureHandle {
    let id = egui::Id::new("sica_mark_texture");
    if let Some(tex) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return tex;
    }
    let px = crate::icon::MARK_PX as usize;
    let image = egui::ColorImage::from_rgba_unmultiplied([px, px], &crate::icon::mark_rgba());
    let tex = ctx.load_texture("sica_mark", image, egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, tex.clone()));
    tex
}
