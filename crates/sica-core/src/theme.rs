//! Dark-theme palette mirroring the original Flet `src/sica/ui/theme.py`.

#[derive(Debug, Clone, Copy)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb(r, g, b)
    }
}

pub const PAGE_BG:             Rgb = Rgb::new(0x0E, 0x12, 0x18);
pub const SIDEBAR_BG:          Rgb = Rgb::new(0x10, 0x14, 0x1A);
pub const SIDEBAR_ACTIVE_BG:   Rgb = Rgb::new(0x1F, 0x2A, 0x38);
pub const USER_BUBBLE_BG:      Rgb = Rgb::new(0x1E, 0x2A, 0x3A);
pub const ASSISTANT_BUBBLE_BG: Rgb = Rgb::new(0x1A, 0x1F, 0x26);
pub const SYSTEM_BUBBLE_BG:    Rgb = Rgb::new(0x2A, 0x2A, 0x2A);
pub const STATUS_BAR_BG:       Rgb = Rgb::new(0x0C, 0x0F, 0x14);
pub const TEXT_PRIMARY:        Rgb = Rgb::new(0xE0, 0xE0, 0xE0);
pub const TEXT_MUTED:          Rgb = Rgb::new(0x88, 0x98, 0xA8);
pub const DIVIDER_FG:          Rgb = Rgb::new(0x5A, 0x60, 0x68);
pub const ACCENT:              Rgb = Rgb::new(0xFF, 0xD2, 0x4A);
pub const REASONING_BLUE:      Rgb = Rgb::new(0x00, 0xBB, 0xFF);
pub const ERROR_FG:            Rgb = Rgb::new(0xFF, 0x6B, 0x6B);
pub const IDEALIST_GREEN:      Rgb = Rgb::new(0x33, 0xCC, 0x66);
pub const IDEALIST_YELLOW:     Rgb = Rgb::new(0xE0, 0xC0, 0x40);
pub const IDEALIST_ORANGE:     Rgb = Rgb::new(0xFF, 0xAA, 0x00);
pub const IDEALIST_RED:        Rgb = Rgb::new(0xFF, 0x6B, 0x6B);
pub const INPUT_BORDER:        Rgb = Rgb::new(0x2A, 0x33, 0x40);
