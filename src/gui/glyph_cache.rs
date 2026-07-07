//! Per-glyph galley cache for the terminal grid.
//!
//! Laying out one galley per cell every frame dominates the render cost. Cells
//! are a tiny fixed alphabet (one char × bold × italic), so we lay each variant
//! out once, tint it per draw via `Shape::galley`'s fallback color, and reuse.
//! Owned by `App` (not egui memory) so it survives across frames deterministically.

use std::collections::HashMap;
use std::sync::Arc;

use egui::epaint::text::{FontsView, VariationCoords};
use egui::text::{Galley, LayoutJob, TextFormat};
use egui::{Color32, FontId};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    pub ch: char,
    pub bold: bool,
    pub italic: bool,
}

pub struct GlyphCache {
    font: FontId,
    ppp: f32,
    map: HashMap<GlyphKey, Arc<Galley>>,
}

impl Default for GlyphCache {
    fn default() -> Self {
        Self {
            font: FontId::monospace(0.0),
            ppp: 0.0,
            map: HashMap::new(),
        }
    }
}

impl GlyphCache {
    /// Drop the cache when the font or pixel density changes. Galleys are
    /// tessellated for a specific ppp; reusing stale ones triggers warnings and
    /// blurry glyphs.
    pub fn sync(&mut self, font: FontId, ppp: f32) {
        if self.font != font || self.ppp != ppp {
            self.font = font;
            self.ppp = ppp;
            self.map.clear();
        }
    }

    pub fn get(&mut self, fonts: &mut FontsView<'_>, key: GlyphKey) -> Arc<Galley> {
        if let Some(g) = self.map.get(&key) {
            return g.clone();
        }
        let mut fmt = TextFormat {
            font_id: self.font.clone(),
            // PLACEHOLDER is substituted per-draw by Shape::galley's fallback.
            color: Color32::PLACEHOLDER,
            italics: key.italic,
            ..Default::default()
        };
        if key.bold {
            // Cascadia Mono is a variable font: wght 700 is real bold with the
            // identical advance width, so the grid stays aligned.
            fmt.coords = VariationCoords::new([(b"wght", 700.0f32)]);
        }
        let g = fonts.layout_job(LayoutJob::single_section(key.ch.to_string(), fmt));
        self.map.insert(key, g.clone());
        g
    }
}
