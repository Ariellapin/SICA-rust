//! Custom typography: Newsreader (serif display, regular + italic) and
//! IBM Plex Mono (UI/body, regular + bold). Both are vendored under
//! `crates/frontend/assets/fonts/` and embedded into the binary via
//! `include_bytes!`.
//!
//! Family map:
//!   * `FontFamily::Proportional` → IBM Plex Mono Regular (default body)
//!   * `FontFamily::Monospace`    → IBM Plex Mono Regular (same — we use one
//!                                  family for everything mono-leaning)
//!   * `FontFamily::Name("display")`        → Newsreader Regular
//!   * `FontFamily::Name("display_italic")` → Newsreader Italic
//!
//! egui's bundled defaults are kept in the fallback chain so emoji + CJK
//! glyphs that don't exist in our two faces still render rather than tofu.

use sica_core::theme::tokens::{FAMILY_ITALIC, FAMILY_MONO, FAMILY_SERIF};

const PLEX_REGULAR: &[u8] =
    include_bytes!("../../assets/fonts/IBMPlexMono-Regular.ttf");
const PLEX_BOLD: &[u8] =
    include_bytes!("../../assets/fonts/IBMPlexMono-Bold.ttf");
const NEWSREADER_REGULAR: &[u8] =
    include_bytes!("../../assets/fonts/Newsreader-Regular.ttf");
const NEWSREADER_ITALIC: &[u8] =
    include_bytes!("../../assets/fonts/Newsreader-Italic.ttf");

/// Register the four custom faces on the given context. Call once at
/// startup, before `apply_visuals`.
pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    fonts.font_data.insert(
        "plex_mono".to_owned(),
        egui::FontData::from_static(PLEX_REGULAR),
    );
    fonts.font_data.insert(
        "plex_mono_bold".to_owned(),
        egui::FontData::from_static(PLEX_BOLD),
    );
    fonts.font_data.insert(
        "newsreader".to_owned(),
        egui::FontData::from_static(NEWSREADER_REGULAR),
    );
    fonts.font_data.insert(
        "newsreader_italic".to_owned(),
        egui::FontData::from_static(NEWSREADER_ITALIC),
    );

    // Prepend Plex Mono so it wins ahead of egui's bundled Ubuntu/Hack.
    if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        list.insert(0, "plex_mono".to_owned());
    }
    if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
        list.insert(0, "plex_mono".to_owned());
    }

    // Custom families addressed by name. Both fall back to the proportional
    // chain (which now starts with Plex Mono) so missing glyphs degrade
    // sensibly rather than disappearing.
    fonts.families.insert(
        egui::FontFamily::Name(FAMILY_SERIF.into()),
        vec![
            "newsreader".to_owned(),
            "plex_mono".to_owned(),
        ],
    );
    fonts.families.insert(
        egui::FontFamily::Name(FAMILY_ITALIC.into()),
        vec![
            "newsreader_italic".to_owned(),
            "newsreader".to_owned(),
            "plex_mono".to_owned(),
        ],
    );
    // Explicit alias of the mono family for parity with the serif keys.
    fonts.families.insert(
        egui::FontFamily::Name(FAMILY_MONO.into()),
        vec![
            "plex_mono".to_owned(),
            "plex_mono_bold".to_owned(),
        ],
    );

    ctx.set_fonts(fonts);
}
