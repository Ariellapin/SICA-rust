//! Application icon. Procedurally draws a 128×128 RGBA buffer matching
//! `assets/icon.svg` — a yellow chat-bubble silhouette with three dark dots
//! on a deep-navy rounded-square background. Doing this in Rust avoids
//! shipping a binary PNG and a PNG decoder dependency.

use egui::IconData;

const SIZE: u32 = 128;

const BG:     [u8; 3] = [0x0E, 0x12, 0x18]; // page navy
const BUBBLE: [u8; 3] = [0xFF, 0xD2, 0x4A]; // accent yellow
const DOT:    [u8; 3] = [0x0E, 0x12, 0x18]; // dots reuse navy

pub fn generate() -> IconData {
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];

    let s = SIZE as f32;
    // Geometry (all coordinates in icon-local 0..SIZE space).
    let bg_radius   = s * (40.0 / 256.0);
    let bubble_x    = s * (56.0 / 256.0);
    let bubble_y    = s * (64.0 / 256.0);
    let bubble_w    = s * ((220.0 - 36.0) / 256.0);
    let bubble_h    = s * ((180.0 - 64.0) / 256.0);
    let bubble_rad  = s * (20.0 / 256.0);
    // Tail triangle, points: (88, 180), (96, 180), (88, 212).
    let tail = [
        (s * 88.0 / 256.0, s * 180.0 / 256.0),
        (s * 96.0 / 256.0, s * 180.0 / 256.0),
        (s * 88.0 / 256.0, s * 212.0 / 256.0),
    ];
    let dot_r = s * (11.0 / 256.0);
    let dot_y = s * (122.0 / 256.0);
    let dot_xs = [
        s *  92.0 / 256.0,
        s * 128.0 / 256.0,
        s * 164.0 / 256.0,
    ];

    for y in 0..SIZE {
        for x in 0..SIZE {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;

            // 1) Rounded-square background. Anything outside is transparent.
            let bg_cov = rounded_rect_coverage(px, py, 0.0, 0.0, s, s, bg_radius);
            if bg_cov <= 0.0 {
                continue;
            }
            let mut color = BG;
            let mut alpha = bg_cov;

            // 2) Bubble body (rounded rect) + tail triangle, both yellow.
            let bubble_cov = rounded_rect_coverage(
                px, py, bubble_x, bubble_y, bubble_w, bubble_h, bubble_rad,
            )
            .max(triangle_coverage(px, py, &tail));
            if bubble_cov > 0.0 {
                color = blend(color, BUBBLE, bubble_cov);
            }

            // 3) Three dark dots inside the bubble.
            let mut dot_cov: f32 = 0.0;
            for &dx in &dot_xs {
                dot_cov = dot_cov.max(circle_coverage(px, py, dx, dot_y, dot_r));
            }
            if dot_cov > 0.0 {
                color = blend(color, DOT, dot_cov);
            }

            // Clamp alpha and stamp.
            alpha = alpha.clamp(0.0, 1.0);
            let i = ((y * SIZE + x) * 4) as usize;
            rgba[i]     = color[0];
            rgba[i + 1] = color[1];
            rgba[i + 2] = color[2];
            rgba[i + 3] = (alpha * 255.0).round() as u8;
        }
    }

    IconData { rgba, width: SIZE, height: SIZE }
}

/// Fractional coverage of pixel `(px, py)` by a rounded rectangle. Returns
/// a value in `[0, 1]` — 1 deep inside, 0 outside, fractional on the edge.
fn rounded_rect_coverage(px: f32, py: f32, x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    // Signed distance from point to the rectangle's outer surface.
    let cx = x + w * 0.5;
    let cy = y + h * 0.5;
    let qx = (px - cx).abs() - (w * 0.5 - r);
    let qy = (py - cy).abs() - (h * 0.5 - r);
    let dx = qx.max(0.0);
    let dy = qy.max(0.0);
    let outside = (dx * dx + dy * dy).sqrt();
    let inside  = qx.max(qy).min(0.0);
    let signed_d = outside + inside - r;
    aa_coverage(signed_d)
}

fn circle_coverage(px: f32, py: f32, cx: f32, cy: f32, r: f32) -> f32 {
    let dx = px - cx;
    let dy = py - cy;
    let signed_d = (dx * dx + dy * dy).sqrt() - r;
    aa_coverage(signed_d)
}

fn triangle_coverage(px: f32, py: f32, tri: &[(f32, f32); 3]) -> f32 {
    // Barycentric containment test on a 2×2 supersample for cheap AA.
    let mut hits = 0;
    let offsets = [(-0.25, -0.25), (0.25, -0.25), (-0.25, 0.25), (0.25, 0.25)];
    for (ox, oy) in offsets {
        if point_in_tri(px + ox, py + oy, tri) {
            hits += 1;
        }
    }
    hits as f32 / 4.0
}

fn point_in_tri(px: f32, py: f32, tri: &[(f32, f32); 3]) -> bool {
    let (ax, ay) = tri[0];
    let (bx, by) = tri[1];
    let (cx, cy) = tri[2];
    let d1 = sign(px, py, ax, ay, bx, by);
    let d2 = sign(px, py, bx, by, cx, cy);
    let d3 = sign(px, py, cx, cy, ax, ay);
    let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_neg && has_pos)
}

fn sign(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    (px - bx) * (ay - by) - (ax - bx) * (py - by)
}

/// Convert a signed distance (in pixels) to an alpha coverage value in
/// `[0, 1]`. ~1px of edge softening yields acceptable antialiasing for icon
/// sizes typical of a window titlebar (16, 32, 48, 64 px).
fn aa_coverage(signed_d: f32) -> f32 {
    (0.5 - signed_d).clamp(0.0, 1.0)
}

fn blend(base: [u8; 3], over: [u8; 3], cov: f32) -> [u8; 3] {
    let cov = cov.clamp(0.0, 1.0);
    let lerp = |a: u8, b: u8| ((a as f32) * (1.0 - cov) + (b as f32) * cov).round() as u8;
    [lerp(base[0], over[0]), lerp(base[1], over[1]), lerp(base[2], over[2])]
}
