//! All visual identity lives here. Direction: Hairline Minimal Mono, retuned to
//! jet-black + white with a single retro-orange accent; every *other* color that
//! ever appears (status dots, syntax, signals) draws from the glowy-retro set
//! below so the whole surface reads like warm phosphor on black.
//!
//! Note: a terminal cell grid can't do real glow/bloom — "glowy" here means
//! saturated brights on true black, not a blur effect. Font is the host
//! terminal's, not ours to set — and neither is the background. The app paints
//! only foreground + accents and lets the terminal own the background, so the
//! embedded shell (which always renders on the terminal bg) stays seamless. Set
//! the terminal profile to `BG` below for the intended jet-black look.

use ratatui::style::{Color, Modifier, Style};

pub struct Theme;

// Palette is defined ahead of use: SELECT_BG and the unused GLOW_* land in
// Phase 2b/2c (selection fill, status dots, syntax). Intentional, not dead.
#[allow(dead_code)]
impl Theme {
    // --- Core: jet black + white ---
    pub const BG: Color = Color::Rgb(0x0a, 0x0a, 0x0a); // recommended terminal bg (not painted)
    pub const FG: Color = Color::Rgb(0xed, 0xed, 0xed); // white ink
    pub const DIM: Color = Color::Rgb(0x6b, 0x6b, 0x6b); // labels, inactive
    pub const HAIR: Color = Color::Rgb(0x2a, 0x26, 0x22); // warm-tinted 1px hairline

    // --- The one accent: retro orange ---
    pub const ACCENT: Color = Color::Rgb(0xff, 0x7a, 0x1a); // the one accent: selection + focus only
    pub const SELECT_BG: Color = Color::Rgb(0x2a, 0x17, 0x0a); // faint orange-glow row fill

    // --- Glowy-retro secondaries (everything that isn't the accent) ---
    pub const GLOW_GREEN: Color = Color::Rgb(0x43, 0xe0, 0x7a); // healthy / live
    pub const GLOW_CYAN: Color = Color::Rgb(0x2f, 0xd6, 0xd6); // info
    pub const GLOW_AMBER: Color = Color::Rgb(0xff, 0xb0, 0x2e); // warning / dirty
    pub const GLOW_MAGENTA: Color = Color::Rgb(0xff, 0x5f, 0xa0); // special / accent-2

    /// Tiny uppercase region label for an unfocused pane — barely-there.
    pub fn label() -> Style {
        Style::default()
            .fg(Self::DIM)
            .add_modifier(Modifier::DIM | Modifier::BOLD)
    }

    /// Region label for the focused pane. Borderless: the label *is* the focus
    /// indicator, so it carries the one accent color instead of a drawn box.
    pub fn label_focused() -> Style {
        Style::default()
            .fg(Self::ACCENT)
            .add_modifier(Modifier::BOLD)
    }
}
