//! Design tokens, v2 — the `dsh` design platform ported whole
//! ([docs/harness-ui-guide.md](../../../docs/harness-ui-guide.md) §1).
//!
//! Three layers, and the discipline is the point:
//!
//! 1. [`Statics`] — the ramps. Byte-identical in light and dark; nothing
//!    reads them directly except the alias maps and the two or three places
//!    that need a literal ramp step (the pixel-chase dot, the diff tints).
//! 2. [`Aliases`] — the semantic map. Every widget consumes *these*, never a
//!    literal colour and never `if dark { … }`. Two maps exist ([`Aliases::light`]
//!    / [`Aliases::dark`]) and a theme flip is one field swap.
//! 3. [`tokens`] — radii by role, the hairline width, the row height, motion
//!    durations. Named for the role, not the value, so "cards are r=12" is
//!    stated once.
//!
//! Two rules carried over verbatim from the CSS: **primary is ink, not
//! blue** (`primary_fill` resolves to `#0F1115` / `#F9FAFB`), and there is no
//! `info` alias — info *is* [`Aliases::business`].
//!
//! The legacy `Palette` (paper/iron) is gone; `Theme::light()` / `Theme::dark()`
//! replace it.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb(r, g, b)
    }
}

/// A colour with an explicit alpha channel. Borders and the interactive
/// washes are alpha colours in dsh — they are drawn *over* whatever surface
/// is beneath, so the same token works on every layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Rgba(r, g, b, a)
    }
    /// Alpha from a 0..1 fraction, rounded — lets the tables below read the
    /// way the CSS does (`black @ .04`).
    pub const fn black(a_pct: u8) -> Self {
        Rgba(0, 0, 0, a_pct)
    }
    pub const fn white(a_pct: u8) -> Self {
        Rgba(255, 255, 255, a_pct)
    }
}

// ---------------------------------------------------------------------------
// Layer 1 — the static scale
// ---------------------------------------------------------------------------

/// The static ramps. Identical in both themes; the alias maps below pick
/// different steps out of them.
#[derive(Debug, Clone, Copy)]
pub struct Statics {
    /// `neutral-bluish` 00…1000 — the UI ramp (19 steps).
    pub bluish: [Rgb; 19],
    /// The accent ramp (`deepseek` in dsh): 50, 100, 200, 400, 450, 500, 800.
    pub accent: [Rgb; 7],
    /// green 500 / 100 / 900.
    pub green: [Rgb; 3],
    /// red 600 / 400.
    pub red: [Rgb; 2],
    /// amber 500 / 600 / 100 / 900.
    pub amber: [Rgb; 4],
}

/// Index names for [`Statics::bluish`] — the ramp is addressed by step
/// everywhere the CSS names one.
pub mod bluish {
    pub const S00: usize = 0;
    pub const S50: usize = 1;
    pub const S60: usize = 2;
    pub const S75: usize = 3;
    pub const S100: usize = 4;
    pub const S150: usize = 5;
    pub const S200: usize = 6;
    pub const S300: usize = 7;
    pub const S400: usize = 8;
    pub const S500: usize = 9;
    pub const S600: usize = 10;
    pub const S700: usize = 11;
    pub const S750: usize = 12;
    pub const S800: usize = 13;
    pub const S850: usize = 14;
    pub const S875: usize = 15;
    pub const S900: usize = 16;
    pub const S950: usize = 17;
    pub const S1000: usize = 18;
}

/// Index names for [`Statics::accent`].
pub mod accent {
    pub const A50: usize = 0;
    pub const A100: usize = 1;
    pub const A200: usize = 2;
    pub const A400: usize = 3;
    pub const A450: usize = 4;
    pub const A500: usize = 5;
    pub const A800: usize = 6;
}

impl Statics {
    pub const fn new() -> Self {
        Self {
            bluish: [
                Rgb::new(0xFF, 0xFF, 0xFF), // 00
                Rgb::new(0xF9, 0xFA, 0xFB), // 50
                Rgb::new(0xF5, 0xF6, 0xF7), // 60
                Rgb::new(0xF1, 0xF3, 0xF5), // 75
                Rgb::new(0xEB, 0xEE, 0xF2), // 100
                Rgb::new(0xE9, 0xEC, 0xF2), // 150
                Rgb::new(0xE1, 0xE5, 0xEE), // 200
                Rgb::new(0xCF, 0xD3, 0xD6), // 300
                Rgb::new(0xAD, 0xB2, 0xB8), // 400
                Rgb::new(0x97, 0x9D, 0xA6), // 500
                Rgb::new(0x81, 0x85, 0x8C), // 600
                Rgb::new(0x61, 0x66, 0x6B), // 700
                Rgb::new(0x43, 0x45, 0x4A), // 750
                Rgb::new(0x35, 0x36, 0x38), // 800
                Rgb::new(0x2C, 0x2C, 0x2E), // 850
                Rgb::new(0x23, 0x23, 0x24), // 875
                Rgb::new(0x1B, 0x1B, 0x1C), // 900
                Rgb::new(0x15, 0x15, 0x17), // 950
                Rgb::new(0x0F, 0x11, 0x15), // 1000
            ],
            accent: [
                Rgb::new(237, 243, 254), // 50
                Rgb::new(228, 237, 253), // 100
                Rgb::new(211, 226, 255), // 200
                Rgb::new(0x67, 0x9E, 0xFE), // 400
                Rgb::new(0x56, 0x86, 0xFE), // 450
                Rgb::new(0x41, 0x76, 0xE6), // 500
                Rgb::new(52, 65, 91),    // 800
            ],
            green: [
                Rgb::new(34, 197, 94),   // 500
                Rgb::new(230, 250, 237), // 100
                Rgb::new(35, 60, 44),    // 900
            ],
            red: [
                Rgb::new(236, 19, 19), // 600
                Rgb::new(242, 90, 90), // 400
            ],
            amber: [
                Rgb::new(245, 158, 11),  // 500
                Rgb::new(221, 134, 41),  // 600
                Rgb::new(254, 245, 231), // 100
                Rgb::new(39, 36, 31),    // 900
            ],
        }
    }
}

impl Default for Statics {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Layer 2 — the semantic alias map
// ---------------------------------------------------------------------------

/// The semantic layer. One instance per theme; widgets read only this.
#[derive(Debug, Clone, Copy)]
pub struct Aliases {
    // Surfaces.
    pub bg_base: Rgb,
    /// layer 1 / 2 / 3 — cards, modals, menus. Light has no layering: all
    /// three are white and separation comes from hairlines.
    pub bg_layer: [Rgb; 3],
    pub sidebar_fill: Rgb,
    pub sidebar_hover: Rgb,
    pub sidebar_active: Rgb,
    /// Dock cards (todo, queue).
    pub tip: Rgb,
    /// User bubble.
    pub bubble: Rgb,
    /// Composer card.
    pub input_major: Rgb,
    /// Menus, popovers.
    pub menu: Rgb,
    pub code_block: Rgb,
    pub inline_code: Rgb,

    // Text.
    /// primary, secondary, tertiary, caption.
    pub label: [Rgb; 4],
    pub label_dimmed: Rgb,
    pub label_inverted: Rgb,

    // Lines and washes.
    /// l1..l4 — every hairline, drawn as an alpha colour over its surface.
    pub border: [Rgba; 4],
    pub hover: Rgba,
    pub active: Rgba,
    pub hover_danger: Rgba,

    // Buttons.
    pub primary_fill: Rgb,
    pub primary_hover: Rgb,
    pub info_fill: Rgb,
    pub info_hover: Rgb,
    pub elevated_fill: Rgb,
    pub floating_fill: Rgb,

    // States.
    pub business: Rgb,
    pub business_tertiary: Rgb,
    pub success: Rgb,
    pub success_tertiary: Rgb,
    pub warn: Rgb,
    pub warn_label: Rgb,
    pub warn_tertiary: Rgb,
    pub error: Rgb,

    // Chrome.
    pub tooltip_bg: Rgb,
    pub toast_bg: Rgb,
    pub scrollbar: Rgb,
    pub scrollbar_hover: Rgb,
}

impl Aliases {
    pub const fn light() -> Self {
        let s = Statics::new();
        Self {
            bg_base: s.bluish[bluish::S00],
            bg_layer: [
                s.bluish[bluish::S00],
                s.bluish[bluish::S00],
                s.bluish[bluish::S00],
            ],
            sidebar_fill: s.bluish[bluish::S50],
            sidebar_hover: s.bluish[bluish::S75],
            sidebar_active: s.bluish[bluish::S100],
            tip: s.bluish[bluish::S60],
            bubble: s.accent[accent::A50],
            input_major: s.bluish[bluish::S00],
            menu: s.bluish[bluish::S00],
            code_block: s.bluish[bluish::S50],
            inline_code: s.bluish[bluish::S100],

            label: [
                s.bluish[bluish::S1000],
                s.bluish[bluish::S700],
                s.bluish[bluish::S600],
                s.bluish[bluish::S400],
            ],
            label_dimmed: s.bluish[bluish::S950],
            label_inverted: s.bluish[bluish::S00],

            border: [
                Rgba::black(10),  // l1 — .04
                Rgba::black(26),  // l2 — .10
                Rgba::black(31),  // l3 — .12
                Rgba::black(41),  // l4 — .16
            ],
            hover: Rgba::new(38, 49, 72, 15),        // .06
            active: Rgba::new(38, 49, 72, 26),       // .10
            hover_danger: Rgba::new(236, 19, 19, 13), // .05

            primary_fill: s.bluish[bluish::S1000],
            primary_hover: s.bluish[bluish::S750],
            info_fill: s.accent[accent::A500],
            info_hover: s.accent[accent::A400],
            elevated_fill: s.bluish[bluish::S00],
            floating_fill: s.bluish[bluish::S00],

            business: s.accent[accent::A500],
            business_tertiary: s.accent[accent::A100],
            success: s.green[0],
            success_tertiary: s.green[1],
            warn: s.amber[0],
            warn_label: s.amber[1],
            warn_tertiary: s.amber[2],
            error: s.red[0],

            tooltip_bg: s.bluish[bluish::S850],
            toast_bg: s.bluish[bluish::S800],
            scrollbar: Rgb::new(229, 229, 229),
            scrollbar_hover: Rgb::new(212, 212, 212),
        }
    }

    pub const fn dark() -> Self {
        let s = Statics::new();
        Self {
            bg_base: s.bluish[bluish::S950],
            bg_layer: [
                s.bluish[bluish::S875],
                s.bluish[bluish::S850],
                s.bluish[bluish::S800],
            ],
            sidebar_fill: s.bluish[bluish::S900],
            sidebar_hover: s.bluish[bluish::S850],
            sidebar_active: s.bluish[bluish::S750],
            tip: s.bluish[bluish::S800],
            bubble: s.bluish[bluish::S850],
            input_major: s.bluish[bluish::S850],
            menu: s.bluish[bluish::S800],
            code_block: s.bluish[bluish::S900],
            inline_code: s.bluish[bluish::S850],

            label: [
                s.bluish[bluish::S50],
                s.bluish[bluish::S300],
                s.bluish[bluish::S400],
                s.bluish[bluish::S600],
            ],
            label_dimmed: s.bluish[bluish::S100],
            label_inverted: s.bluish[bluish::S800],

            border: [
                Rgba::white(15), // l1 — .06
                Rgba::white(31), // l2 — .12
                Rgba::white(41), // l3 — .16
                Rgba::white(51), // l4 — .20
            ],
            hover: Rgba::white(20),                   // .08
            active: Rgba::white(36),                  // .14
            hover_danger: Rgba::new(242, 90, 90, 38), // .15

            primary_fill: s.bluish[bluish::S50],
            primary_hover: s.bluish[bluish::S100],
            info_fill: s.accent[accent::A400],
            info_hover: s.accent[accent::A500],
            elevated_fill: s.bluish[bluish::S750],
            floating_fill: s.bluish[bluish::S850],

            business: s.accent[accent::A400],
            business_tertiary: s.accent[accent::A800],
            success: s.green[0],
            success_tertiary: s.green[2],
            warn: s.amber[0],
            warn_label: s.amber[1],
            warn_tertiary: s.amber[3],
            error: s.red[1],

            tooltip_bg: s.bluish[bluish::S750],
            toast_bg: s.bluish[bluish::S750],
            scrollbar: Rgb::new(60, 60, 61),
            scrollbar_hover: Rgb::new(84, 85, 87),
        }
    }
}

// ---------------------------------------------------------------------------
// The theme
// ---------------------------------------------------------------------------

/// Everything a frame needs to paint itself: the two colour layers plus the
/// user's content-size preference.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub statics: Statics,
    pub alias: Aliases,
    pub dark: bool,
    /// Conversation content size, 12..=17 px (default 14). Only the
    /// transcript ladder follows it — chrome type is fixed.
    pub content_px: u8,
}

impl Theme {
    pub const fn light() -> Self {
        Self {
            statics: Statics::new(),
            alias: Aliases::light(),
            dark: false,
            content_px: tokens::CONTENT_DEFAULT_PX,
        }
    }

    pub const fn dark() -> Self {
        Self {
            statics: Statics::new(),
            alias: Aliases::dark(),
            dark: true,
            content_px: tokens::CONTENT_DEFAULT_PX,
        }
    }

    pub const fn of(dark: bool) -> Self {
        if dark {
            Self::dark()
        } else {
            Self::light()
        }
    }

    /// Font-size delta of the content ladder against the 14 px default —
    /// dsh's `δ`, which almost every transcript metric is expressed in.
    pub fn delta(&self) -> f32 {
        self.content_px as f32 - tokens::CONTENT_DEFAULT_PX as f32
    }

    /// Secondary content tier: `min(size − 1, max(13, size − 2))` — 13 at the
    /// default. Row titles use it.
    pub fn content_secondary_px(&self) -> f32 {
        let size = self.content_px as f32;
        (size - 1.0).min((size - 2.0).max(13.0))
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

/// Role-named geometry, motion and type tokens. Values are dsh's.
pub mod tokens {
    // Radii by role (§1.1 "Geometry").
    /// inline code, tool row, stopped tag
    pub const RADIUS_ROW: f32 = 6.0;
    /// inputs, list rows, tooltip, notice
    pub const RADIUS_INPUT: f32 = 8.0;
    /// menu items
    pub const RADIUS_ITEM: f32 = 10.0;
    /// cards, code/diff blocks, settings nav cell, New Session, dock cards
    pub const RADIUS_CARD: f32 = 12.0;
    /// toast, small button
    pub const RADIUS_BTN_SM: f32 = 14.0;
    /// medium button (capsule)
    pub const RADIUS_BTN: f32 = 18.0;
    /// menus
    pub const RADIUS_MENU: f32 = 20.0;
    /// user bubble, composer card
    pub const RADIUS_BUBBLE: f32 = 22.0;
    /// modal
    pub const RADIUS_MODAL: f32 = 24.0;
    /// settings panel
    pub const RADIUS_SETTINGS: f32 = 32.0;
    /// pills and circles
    pub const RADIUS_PILL: f32 = 999.0;

    /// Neutral borders draw at one device pixel. egui feathers sub-pixel
    /// strokes, which is exactly the intent — do not round this to 1.
    pub const HAIRLINE: f32 = 0.5;

    /// Base transcript row height; almost every row is `ROW_H + δ`.
    pub const ROW_H: f32 = 24.0;

    /// Content ladder bounds (Settings › General stepper).
    pub const CONTENT_MIN_PX: u8 = 12;
    pub const CONTENT_MAX_PX: u8 = 17;
    pub const CONTENT_DEFAULT_PX: u8 = 14;

    // Shell metrics (§2.1).
    pub const SIDEBAR_DEFAULT: f32 = 280.0;
    pub const SIDEBAR_MIN: f32 = 264.0;
    pub const SIDEBAR_MAX: f32 = 420.0;
    pub const SIDEBAR_COLLAPSED: f32 = 56.0;
    /// Viewport width under which the sidebar auto-collapses.
    pub const SIDEBAR_AUTO_COLLAPSE: f32 = 1024.0;
    pub const CENTER_MIN: f32 = 640.0;
    pub const DETAILS_DEFAULT: f32 = 360.0;
    pub const DETAILS_MIN: f32 = 300.0;
    pub const DETAILS_MAX: f32 = 520.0;

    /// Conversation content column: `clamp(680, column × 0.64, 920)`.
    pub const CONTENT_W_MIN: f32 = 680.0;
    pub const CONTENT_W_MAX: f32 = 920.0;
    pub const CONTENT_W_FRACTION: f32 = 0.64;

    // Motion (seconds). Every ambient loop freezes under `reduce_motion`.
    pub const DUR_HOVER: f32 = 0.100;
    pub const DUR_CHEVRON: f32 = 0.120;
    pub const DUR_FADE: f32 = 0.150;
    pub const DUR_TOAST_IN: f32 = 0.160;
    pub const DUR_COLUMN: f32 = 0.300;
    /// Turn-status text shimmer period.
    pub const SHIMMER_PERIOD: f32 = 1.8;
    /// Running tool / reasoning row glare sweep period.
    pub const SWEEP_PERIOD: f32 = 2.6;
    /// State-dot pixel-chase period.
    pub const CHASE_PERIOD: f32 = 1.0;
    pub const TOAST_HOLD_MS: u64 = 3000;
    pub const TOAST_FADE_MS: u64 = 1000;

    // Type roles (px / line-height). Chrome type never follows `content_px`.
    pub const FS_XL: f32 = 24.0;
    pub const FS_L: f32 = 20.0;
    pub const FS_M: f32 = 18.0;
    pub const FS_BASE: f32 = 16.0;
    pub const FS_S: f32 = 14.0;
    pub const FS_XS: f32 = 13.0;
    pub const FS_XXS: f32 = 12.0;
    pub const FS_XXXS: f32 = 11.0;

    /// Font family keys registered by the frontend's `fonts::install`.
    pub const FAMILY_MEDIUM: &str = "ui_medium";
    pub const FAMILY_SEMIBOLD: &str = "ui_semibold";
    pub const FAMILY_BOLD: &str = "ui_bold";
    pub const FAMILY_MONO: &str = "mono";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_is_ink_not_blue() {
        // The rule the CSS repeats: `brand-primary` resolves to ink, so a
        // primary button is near-black in light and near-white in dark.
        assert_eq!(Aliases::light().primary_fill, Rgb::new(0x0F, 0x11, 0x15));
        assert_eq!(Aliases::dark().primary_fill, Rgb::new(0xF9, 0xFA, 0xFB));
    }

    #[test]
    fn light_has_no_surface_layering() {
        let l = Aliases::light();
        assert_eq!(l.bg_layer[0], l.bg_layer[1]);
        assert_eq!(l.bg_layer[1], l.bg_layer[2]);
        // Dark layers do step up.
        let d = Aliases::dark();
        assert_ne!(d.bg_layer[0], d.bg_layer[2]);
    }

    #[test]
    fn content_ladder_secondary_tier_is_13_at_default() {
        let t = Theme::dark();
        assert_eq!(t.content_px, 14);
        assert_eq!(t.delta(), 0.0);
        assert_eq!(t.content_secondary_px(), 13.0);
        // …and tracks the preference above the default.
        let big = Theme { content_px: 17, ..Theme::dark() };
        assert_eq!(big.delta(), 3.0);
        assert_eq!(big.content_secondary_px(), 15.0);
    }
}
