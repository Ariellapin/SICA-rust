//! Brand artwork. Both marks are SVG — `assets/icon.svg` (the full-colour
//! application icon) and `assets/mark.svg` (the monochrome blade-and-loop
//! silhouette the shell paints) — compiled in with `include_str!` and
//! rasterised with resvg at the size the caller asks for.
//!
//! This replaces a hand-written scanline rasteriser that had to be kept in
//! step with the SVG by eye. There is now one source of truth per mark, and
//! curves, masks and gradients come out right at any resolution.

use egui::IconData;

const ICON_SVG: &str = include_str!("../assets/icon.svg");
const MARK_SVG: &str = include_str!("../assets/mark.svg");

/// Edge of the rasterised window icon. Windows picks from this down to 16 px,
/// so it is rendered generously and left for the compositor to filter.
const ICON_PX: u32 = 256;

/// Edge of the mark texture. The shell's largest use is the 44 px hero mark,
/// which at 2x DPI wants 88 px; 256 keeps headroom for higher scale factors.
pub const MARK_PX: u32 = 256;

/// The window / taskbar icon.
///
/// Falls back to a fully transparent buffer if the SVG ever fails to parse —
/// a missing icon is a cosmetic defect and must not stop the GUI from opening.
pub fn generate() -> IconData {
    let rgba = render(ICON_SVG, ICON_PX)
        .unwrap_or_else(|| vec![0u8; (ICON_PX * ICON_PX * 4) as usize]);
    IconData { rgba, width: ICON_PX, height: ICON_PX }
}

/// The monochrome mark as premultiplied-free straight RGBA, `MARK_PX` square.
///
/// Every pixel is white with a varying alpha, so egui can tint the resulting
/// texture to any theme colour by multiplication.
pub fn mark_rgba() -> Vec<u8> {
    render(MARK_SVG, MARK_PX)
        .unwrap_or_else(|| vec![0u8; (MARK_PX * MARK_PX * 4) as usize])
}

/// Rasterise `svg` into a `size`x`size` straight-alpha RGBA buffer.
///
/// The source viewBox is square in both assets, so a uniform fit is exact and
/// no letterboxing is needed.
fn render(svg: &str, size: u32) -> Option<Vec<u8>> {
    let opt = resvg::usvg::Options::default();
    let tree = match resvg::usvg::Tree::from_str(svg, &opt) {
        Ok(tree) => tree,
        Err(e) => {
            tracing::warn!("icon svg failed to parse: {e}");
            return None;
        }
    };

    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)?;
    let scale = size as f32 / tree.size().width().max(1.0);
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // tiny-skia hands back premultiplied alpha; egui textures and
    // `IconData` both want straight alpha.
    Some(
        pixmap
            .pixels()
            .iter()
            .flat_map(|px| {
                let a = px.alpha();
                if a == 0 {
                    [0, 0, 0, 0]
                } else {
                    let un = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
                    [un(px.red()), un(px.green()), un(px.blue()), a]
                }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_icon_rasterises() {
        let icon = generate();
        assert_eq!(icon.width, ICON_PX);
        assert_eq!(icon.rgba.len(), (ICON_PX * ICON_PX * 4) as usize);
        // The tile is opaque edge to edge apart from the rounded corners, so
        // the centre pixel must be solid — a blank buffer means the SVG did
        // not parse and `generate` fell through to its fallback.
        let mid = ((ICON_PX / 2 * ICON_PX + ICON_PX / 2) * 4) as usize;
        assert_eq!(icon.rgba[mid + 3], 255, "icon centre is transparent");
    }

    #[test]
    fn mark_is_white_with_shaped_alpha() {
        let rgba = mark_rgba();
        assert_eq!(rgba.len(), (MARK_PX * MARK_PX * 4) as usize);
        // Corners sit outside the ring; the blade covers the middle band.
        assert_eq!(rgba[3], 0, "top-left corner should be transparent");
        let opaque = rgba.chunks_exact(4).filter(|p| p[3] > 200).count();
        assert!(opaque > 1_000, "mark looks empty ({opaque} opaque px)");
        for px in rgba.chunks_exact(4).filter(|p| p[3] > 200) {
            assert_eq!(
                [px[0], px[1], px[2]],
                [255, 255, 255],
                "mark must be pure white so it can be tinted",
            );
        }
    }
}
