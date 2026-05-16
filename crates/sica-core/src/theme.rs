//! UI palette + design tokens. Two palette presets — `paper()` (light) and
//! `iron()` (dark) — both built from the same semantic token set so a theme
//! toggle is a single field swap rather than a global recompile.
//!
//! The palette deliberately leans editorial: cream paper + ink for light,
//! oxidized iron + bone for dark, with a single rust accent. There are no
//! component-named fields (no `user_bubble_bg`); every token is named for the
//! role it plays so widgets compose freely.

#[derive(Debug, Clone, Copy)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb(r, g, b)
    }
}

/// Semantic colour tokens. Three surface tiers, three text tiers, three
/// accent tiers, five semantic state colours.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    // Surfaces (page → card → sunk).
    pub page_bg:       Rgb,
    pub surface:       Rgb,
    pub surface_sunk:  Rgb,

    // Text + rules.
    pub ink:           Rgb,
    pub muted:         Rgb,
    pub hairline:      Rgb,

    // Accent (rest → hover → subtle wash for selection / hover fills).
    pub accent:        Rgb,
    pub accent_hover:  Rgb,
    pub accent_subtle: Rgb,

    // Semantic states.
    pub ok:            Rgb,
    pub warn:          Rgb,
    pub caution:       Rgb,
    pub danger:        Rgb,
    pub info:          Rgb,
}

impl Palette {
    /// Light "paper" — cream stock, ink, deep rust accent.
    pub const fn paper() -> Self {
        Self {
            page_bg:       Rgb::new(0xF4, 0xEF, 0xE6),
            surface:       Rgb::new(0xEF, 0xE8, 0xDB),
            surface_sunk:  Rgb::new(0xE8, 0xE0, 0xD0),

            ink:           Rgb::new(0x1C, 0x18, 0x14),
            muted:         Rgb::new(0x7A, 0x6F, 0x62),
            hairline:      Rgb::new(0xD9, 0xCF, 0xC1),

            accent:        Rgb::new(0xB1, 0x4A, 0x1F),
            accent_hover:  Rgb::new(0xC2, 0x57, 0x29),
            accent_subtle: Rgb::new(0xF0, 0xD8, 0xC6),

            ok:            Rgb::new(0x4F, 0x7B, 0x3F),
            warn:          Rgb::new(0xB5, 0x9A, 0x2C),
            caution:       Rgb::new(0xC2, 0x6A, 0x1A),
            danger:        Rgb::new(0x9C, 0x2D, 0x1F),
            info:          Rgb::new(0x2F, 0x5D, 0x7A),
        }
    }

    /// Dark "iron" — oxidised iron, bone text, glow rust accent.
    pub const fn iron() -> Self {
        Self {
            page_bg:       Rgb::new(0x16, 0x11, 0x0D),
            surface:       Rgb::new(0x1E, 0x18, 0x13),
            surface_sunk:  Rgb::new(0x24, 0x1D, 0x17),

            ink:           Rgb::new(0xE8, 0xDF, 0xD3),
            muted:         Rgb::new(0x8B, 0x7E, 0x6F),
            hairline:      Rgb::new(0x2A, 0x20, 0x18),

            accent:        Rgb::new(0xD8, 0x6A, 0x2A),
            accent_hover:  Rgb::new(0xE8, 0x7A, 0x3A),
            accent_subtle: Rgb::new(0x3A, 0x24, 0x18),

            ok:            Rgb::new(0x6B, 0xA8, 0x55),
            warn:          Rgb::new(0xD4, 0xB6, 0x42),
            caution:       Rgb::new(0xDF, 0x82, 0x32),
            danger:        Rgb::new(0xC4, 0x44, 0x38),
            info:          Rgb::new(0x4D, 0x89, 0xA8),
        }
    }

    /// Compatibility alias — `dark()` returns `iron()`.
    pub const fn dark() -> Self { Self::iron() }

    /// Compatibility alias — `light()` returns `paper()`.
    pub const fn light() -> Self { Self::paper() }
}

/// Design tokens shared by every widget. Spacing follows a 4px grid; rounding
/// is restrained (0–4px); fonts are addressed by family name registered in the
/// frontend's `fonts::install`.
pub mod tokens {
    // Spacing scale — 4px grid.
    pub const SPACE_1: f32 = 4.0;
    pub const SPACE_2: f32 = 8.0;
    pub const SPACE_3: f32 = 12.0;
    pub const SPACE_4: f32 = 16.0;
    pub const SPACE_5: f32 = 24.0;
    pub const SPACE_6: f32 = 32.0;

    // Radius scale — precise, not pillowy.
    pub const RADIUS_0: f32 = 0.0;
    pub const RADIUS_2: f32 = 2.0;
    pub const RADIUS_4: f32 = 4.0;

    // Hairline width — single source of truth.
    pub const HAIRLINE: f32 = 1.0;

    // Font family names — register matching keys in `fonts::install`.
    pub const FAMILY_SERIF:  &str = "display";
    pub const FAMILY_ITALIC: &str = "display_italic";
    pub const FAMILY_MONO:   &str = "mono";
}
