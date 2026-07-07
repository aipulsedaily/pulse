//! Terminal color theme — the app's OWN palette (user mandate: "turn it into
//! our own custom text theme and match the input box but keep it all
//! seamless"). The grid text and the composer's input lane must read as ONE
//! designed surface, so every slot is curated against the app tokens
//! (BG 0x0B0D12 family, ACCENT 0x7C83FF, the TEXT 0xE7E9EF ramp, DANGER
//! 0xFF5C6C, SUCCESS 0x4ADE80).
//!
//! Rules (binding):
//! - PRESENTATIONAL ONLY: cell data is untouched; copy/selection semantics
//!   see the real characters. Only glyph-color resolution changes.
//! - Semantic hues are KEPT (red stays clearly red — mapped onto the DANGER
//!   family; green onto SUCCESS; yellow a muted gold; blue the ACCENT
//!   indigo) so TUIs that color by meaning (claude, git, PSReadLine) stay
//!   legible — tuned values, never flattened to monochrome.
//! - Default foreground == the composer lane's TEXT and background ==
//!   TERM_BG, so the lane boundary has zero palette clash.
//! - 256-color cube and true-color (Spec) output pass through untouched —
//!   apps that pick exact RGB get exact RGB.
//!
//! Structure adapted from egui_term (MIT, Ilya Shvyryalkin).

use alacritty_terminal::vte::ansi::{Color, NamedColor};
use egui::Color32;

pub struct TerminalTheme {
    normal: [Color32; 8],
    bright: [Color32; 8],
    dim: [Color32; 8],
    pub foreground: Color32,
    pub background: Color32,
}

const fn c(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

impl Default for TerminalTheme {
    fn default() -> Self {
        Self {
            normal: [
                c(0x14, 0x16, 0x1d), // black — a hair above TERM_BG so fills read
                c(0xff, 0x5c, 0x6c), // red — DANGER (errors match app chrome)
                c(0x4a, 0xde, 0x80), // green — SUCCESS
                c(0xe5, 0xc0, 0x7b), // yellow — muted gold (PSReadLine cmd echo)
                c(0x7c, 0x83, 0xff), // blue — ACCENT indigo (dirs, links)
                c(0xbd, 0x8a, 0xff), // magenta — violet neighbour of the accent
                c(0x7a, 0xd9, 0xd9), // cyan — soft teal
                c(0xc9, 0xce, 0xdc), // white — just under TEXT so bold pops
            ],
            bright: [
                c(0x6b, 0x71, 0x85), // bright black — TEXT_MUTED (comments/dim UI)
                c(0xff, 0x74, 0x82), // bright red — DANGER_HOVER
                c(0x6f, 0xe8, 0x9c),
                c(0xf0, 0xd3, 0x99),
                c(0x90, 0x96, 0xff), // bright blue — ACCENT_HOVER
                c(0xd2, 0xa8, 0xff),
                c(0x93, 0xe5, 0xe5),
                c(0xf1, 0xf2, 0xf7), // bright white — top of the TEXT ramp
            ],
            dim: [
                c(0x0e, 0x10, 0x15),
                c(0x99, 0x3a, 0x44),
                c(0x2f, 0x85, 0x50),
                c(0x8a, 0x74, 0x4c),
                c(0x4c, 0x50, 0x99),
                c(0x71, 0x54, 0x99),
                c(0x4c, 0x82, 0x82),
                c(0x79, 0x7c, 0x87),
            ],
            foreground: c(0xe7, 0xe9, 0xef), // TEXT — identical to the input lane
            background: c(0x0c, 0x0e, 0x13), // TERM_BG
        }
    }
}

fn ansi256(index: u8) -> Color32 {
    if index >= 232 {
        let v = (index - 232) * 10 + 8;
        return c(v, v, v);
    }
    let i = index - 16;
    let (r, g, b) = (i / 36, (i / 6) % 6, i % 6);
    let ch = |x: u8| if x == 0 { 0 } else { x * 40 + 55 };
    c(ch(r), ch(g), ch(b))
}

impl TerminalTheme {
    pub fn get_color(&self, color: Color) -> Color32 {
        match color {
            Color::Spec(rgb) => c(rgb.r, rgb.g, rgb.b),
            Color::Indexed(i) => match i {
                0..=7 => self.normal[i as usize],
                8..=15 => self.bright[i as usize - 8],
                _ => ansi256(i),
            },
            Color::Named(named) => match named {
                NamedColor::Foreground | NamedColor::BrightForeground => self.foreground,
                NamedColor::Background => self.background,
                NamedColor::Cursor => self.foreground,
                NamedColor::Black => self.normal[0],
                NamedColor::Red => self.normal[1],
                NamedColor::Green => self.normal[2],
                NamedColor::Yellow => self.normal[3],
                NamedColor::Blue => self.normal[4],
                NamedColor::Magenta => self.normal[5],
                NamedColor::Cyan => self.normal[6],
                NamedColor::White => self.normal[7],
                NamedColor::BrightBlack => self.bright[0],
                NamedColor::BrightRed => self.bright[1],
                NamedColor::BrightGreen => self.bright[2],
                NamedColor::BrightYellow => self.bright[3],
                NamedColor::BrightBlue => self.bright[4],
                NamedColor::BrightMagenta => self.bright[5],
                NamedColor::BrightCyan => self.bright[6],
                NamedColor::BrightWhite => self.bright[7],
                NamedColor::DimForeground => c(0x8b, 0x90, 0xa0), // TEXT_SECONDARY dimmed
                NamedColor::DimBlack => self.dim[0],
                NamedColor::DimRed => self.dim[1],
                NamedColor::DimGreen => self.dim[2],
                NamedColor::DimYellow => self.dim[3],
                NamedColor::DimBlue => self.dim[4],
                NamedColor::DimMagenta => self.dim[5],
                NamedColor::DimCyan => self.dim[6],
                NamedColor::DimWhite => self.dim[7],
            },
        }
    }

    /// Resolve a color for the CELL-BACKGROUND role (SGR 40-47/100-107 and
    /// the named slots behind them). The 16 ANSI slots map to muted,
    /// near-BG tones — hue preserved, luminance pulled down — instead of the
    /// vivid fg palette (restored-render fix hardening: the fg remap put
    /// ACCENT indigo #7C83FF behind every SGR-44 cell, turning subtle shell
    /// bg highlights into prominent full-cell slabs; bg fills must never be
    /// louder than the text they sit under). 256-cube and true-color pass
    /// through untouched, exactly like `get_color`. Reverse video is
    /// unaffected: term_view resolves fg/bg first and swaps AFTER, so
    /// INVERSE highlights still use the vivid fg color as their fill.
    pub fn get_bg_color(&self, color: Color) -> Color32 {
        // Muted bg ramps: normal = the dim ramp (already near-BG, hue kept);
        // bright = a hand-lightened step above it (visible highlight, still
        // far under the fg palette's luminance).
        const BG_BRIGHT: [Color32; 8] = [
            c(0x23, 0x26, 0x30), // bright black — a clear step over TERM_BG
            c(0xa8, 0x4a, 0x55),
            c(0x3d, 0x96, 0x60),
            c(0x9a, 0x84, 0x5c),
            c(0x5c, 0x61, 0xaa), // bright blue — muted indigo, never ACCENT
            c(0x81, 0x64, 0xa9),
            c(0x5c, 0x92, 0x92),
            c(0x80, 0x83, 0x8e),
        ];
        match color {
            Color::Indexed(i @ 0..=7) => self.dim[i as usize],
            Color::Indexed(i @ 8..=15) => BG_BRIGHT[i as usize - 8],
            Color::Named(named) => match named {
                NamedColor::Black
                | NamedColor::Red
                | NamedColor::Green
                | NamedColor::Yellow
                | NamedColor::Blue
                | NamedColor::Magenta
                | NamedColor::Cyan
                | NamedColor::White => self.dim[named as usize],
                NamedColor::BrightBlack
                | NamedColor::BrightRed
                | NamedColor::BrightGreen
                | NamedColor::BrightYellow
                | NamedColor::BrightBlue
                | NamedColor::BrightMagenta
                | NamedColor::BrightCyan
                | NamedColor::BrightWhite => {
                    BG_BRIGHT[named as usize - NamedColor::BrightBlack as usize]
                }
                NamedColor::DimBlack
                | NamedColor::DimRed
                | NamedColor::DimGreen
                | NamedColor::DimYellow
                | NamedColor::DimBlue
                | NamedColor::DimMagenta
                | NamedColor::DimCyan
                | NamedColor::DimWhite => {
                    self.dim[named as usize - NamedColor::DimBlack as usize]
                }
                other => self.get_color(Color::Named(other)),
            },
            other => self.get_color(other),
        }
    }

    /// Brighten a normal ANSI color for bold text (classic bold-as-bright).
    pub fn bold_variant(&self, color: Color) -> Color {
        match color {
            Color::Indexed(i @ 0..=7) => Color::Indexed(i + 8),
            Color::Named(named) => {
                let mapped = match named {
                    NamedColor::Black => NamedColor::BrightBlack,
                    NamedColor::Red => NamedColor::BrightRed,
                    NamedColor::Green => NamedColor::BrightGreen,
                    NamedColor::Yellow => NamedColor::BrightYellow,
                    NamedColor::Blue => NamedColor::BrightBlue,
                    NamedColor::Magenta => NamedColor::BrightMagenta,
                    NamedColor::Cyan => NamedColor::BrightCyan,
                    NamedColor::White => NamedColor::BrightWhite,
                    NamedColor::Foreground => NamedColor::BrightForeground,
                    other => other,
                };
                Color::Named(mapped)
            }
            other => other,
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::vte::ansi::Rgb;

    fn lum(c: Color32) -> f32 {
        0.2126 * c.r() as f32 + 0.7152 * c.g() as f32 + 0.0722 * c.b() as f32
    }

    /// Restored-render fix hardening: the 16 ANSI slots in the BACKGROUND
    /// role must resolve far more conservatively than the fg palette —
    /// a full-cell fill at fg vividness (SGR 44 → ACCENT indigo) reads as a
    /// painted-slab artifact over dark content. Every slot: strictly darker
    /// than its fg counterpart and under an absolute luminance ceiling.
    #[test]
    fn bg_slots_map_conservatively() {
        let t = TerminalTheme::default();
        for i in 0u8..=15 {
            let bg = t.get_bg_color(Color::Indexed(i));
            let fg = t.get_color(Color::Indexed(i));
            // The black slots' fg mapping is itself near-BG by design — the
            // relative rule only means something for visibly-bright fg slots.
            if lum(fg) > 100.0 {
                assert!(
                    lum(bg) < lum(fg) - 30.0,
                    "indexed bg slot {i} ({bg:?}) must sit well under its fg mapping ({fg:?})"
                );
            }
            assert!(
                lum(bg) <= 140.0,
                "indexed bg slot {i} ({bg:?}) exceeds the bg luminance ceiling"
            );
        }
        // The named 0-15 slots resolve identically to their indexed twins.
        use NamedColor::*;
        let named = [
            Black, Red, Green, Yellow, Blue, Magenta, Cyan, White, BrightBlack, BrightRed,
            BrightGreen, BrightYellow, BrightBlue, BrightMagenta, BrightCyan, BrightWhite,
        ];
        for (i, n) in named.into_iter().enumerate() {
            assert_eq!(
                t.get_bg_color(Color::Named(n)),
                t.get_bg_color(Color::Indexed(i as u8)),
                "named bg slot {n:?} must match indexed {i}"
            );
        }
        // The field artifact's specific hazard, pinned: a blue bg cell must
        // never fill with the ACCENT indigo the fg palette uses.
        assert_ne!(
            t.get_bg_color(Color::Indexed(4)),
            t.get_color(Color::Indexed(4)),
            "SGR 44 must not paint the vivid fg ACCENT as a cell fill"
        );
    }

    /// Apps that pick exact colors get exact colors, in both roles.
    #[test]
    fn bg_256_cube_and_truecolor_pass_through() {
        let t = TerminalTheme::default();
        for i in [16u8, 21, 128, 196, 232, 255] {
            assert_eq!(
                t.get_bg_color(Color::Indexed(i)),
                t.get_color(Color::Indexed(i)),
                "256-cube index {i} must pass through untouched in the bg role"
            );
        }
        let spec = Color::Spec(Rgb { r: 0x12, g: 0x34, b: 0x56 });
        assert_eq!(t.get_bg_color(spec), t.get_color(spec));
        assert_eq!(
            t.get_bg_color(Color::Named(NamedColor::Background)),
            t.background,
            "default background stays the app TERM_BG"
        );
        assert_eq!(
            t.get_bg_color(Color::Named(NamedColor::Foreground)),
            t.foreground,
            "DECSCNM-style fg-as-bg keeps the fg resolution"
        );
    }
}
