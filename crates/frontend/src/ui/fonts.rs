//! Typography. dsh's UI face is the *system sans* (`-apple-system,
//! 'Segoe UI', …`) and its code face is a grotesque mono (`'SF Mono',
//! 'JetBrains Mono', Consolas, …`), so the port loads the platform's own
//! sans at runtime and keeps IBM Plex Mono — already vendored — as the code
//! family.
//!
//! egui has no weight axis: each weight is its own family. Four are
//! registered, matching the 400/500/600/700 dsh uses and nothing else:
//!
//!   * `FontFamily::Proportional`        → system sans Regular
//!   * `FontFamily::Name("ui_medium")`   → system sans Medium/Semibold
//!   * `FontFamily::Name("ui_semibold")` → system sans Semibold
//!   * `FontFamily::Name("ui_bold")`     → system sans Bold
//!   * `FontFamily::Monospace` / `Name("mono")` → IBM Plex Mono
//!
//! egui's bundled faces stay at the end of every chain so emoji and CJK
//! glyphs render rather than tofu, and a machine without the system faces
//! (or a non-Windows build) degrades to those defaults instead of failing.

use sica_core::theme::tokens::{FAMILY_BOLD, FAMILY_MEDIUM, FAMILY_MONO, FAMILY_SEMIBOLD};

const PLEX_REGULAR: &[u8] = include_bytes!("../../assets/fonts/IBMPlexMono-Regular.ttf");
const PLEX_BOLD: &[u8] = include_bytes!("../../assets/fonts/IBMPlexMono-Bold.ttf");

/// Candidate files per weight, most-wanted first. Windows ships Segoe UI in
/// four usable weights; the macOS / Linux entries are best-effort and fall
/// through to egui's own faces when absent.
#[cfg(target_os = "windows")]
const SANS_CANDIDATES: [(&str, &[&str]); 4] = [
    ("ui_regular", &["C:/Windows/Fonts/segoeui.ttf"]),
    ("ui_medium", &["C:/Windows/Fonts/seguisb.ttf", "C:/Windows/Fonts/segoeui.ttf"]),
    ("ui_semibold", &["C:/Windows/Fonts/seguisb.ttf", "C:/Windows/Fonts/segoeuib.ttf"]),
    ("ui_bold", &["C:/Windows/Fonts/segoeuib.ttf"]),
];

#[cfg(target_os = "macos")]
const SANS_CANDIDATES: [(&str, &[&str]); 4] = [
    ("ui_regular", &["/System/Library/Fonts/SFNS.ttf", "/Library/Fonts/Arial.ttf"]),
    ("ui_medium", &["/System/Library/Fonts/SFNS.ttf"]),
    ("ui_semibold", &["/System/Library/Fonts/SFNS.ttf"]),
    ("ui_bold", &["/System/Library/Fonts/SFNSDisplay-Bold.otf"]),
];

#[cfg(all(unix, not(target_os = "macos")))]
const SANS_CANDIDATES: [(&str, &[&str]); 4] = [
    (
        "ui_regular",
        &[
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
        ],
    ),
    ("ui_medium", &["/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"]),
    ("ui_semibold", &["/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"]),
    ("ui_bold", &["/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"]),
];

/// Register the UI and code families. Call once at startup, before
/// `apply_visuals`.
pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    fonts
        .font_data
        .insert("mono_regular".to_owned(), egui::FontData::from_static(PLEX_REGULAR));
    fonts
        .font_data
        .insert("mono_bold".to_owned(), egui::FontData::from_static(PLEX_BOLD));

    // Load whatever system faces exist; a missing file just means that
    // weight resolves through the fallback chain below.
    let mut loaded: Vec<&'static str> = Vec::new();
    for (key, paths) in SANS_CANDIDATES {
        for path in paths {
            if let Ok(bytes) = std::fs::read(path) {
                fonts
                    .font_data
                    .insert(key.to_owned(), egui::FontData::from_owned(bytes));
                loaded.push(key);
                break;
            }
        }
    }
    let has = |key: &str| loaded.contains(&key);

    // Proportional: system sans first, then egui's bundled faces.
    if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        if has("ui_regular") {
            list.insert(0, "ui_regular".to_owned());
        }
    }
    // Monospace: Plex Mono ahead of egui's Hack.
    if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
        list.insert(0, "mono_regular".to_owned());
    }

    let proportional = fonts
        .families
        .get(&egui::FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    // Each weight is a family whose tail is the proportional chain, so a
    // glyph the weighted face lacks still resolves.
    let mut weighted = |name: &str, key: &str| {
        let mut chain = Vec::new();
        if has(key) {
            chain.push(key.to_owned());
        }
        chain.extend(proportional.iter().cloned());
        fonts
            .families
            .insert(egui::FontFamily::Name(name.into()), chain);
    };
    weighted(FAMILY_MEDIUM, "ui_medium");
    weighted(FAMILY_SEMIBOLD, "ui_semibold");
    weighted(FAMILY_BOLD, "ui_bold");

    fonts.families.insert(
        egui::FontFamily::Name(FAMILY_MONO.into()),
        vec!["mono_regular".to_owned(), "mono_bold".to_owned()],
    );

    ctx.set_fonts(fonts);
}
