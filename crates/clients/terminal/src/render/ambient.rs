//! The ambient mark: a faceted shard with orbiting particles — the
//! static seed of the home screen's ambient layer (style.md: ambient
//! sits behind content, never carries information, fades under load).
//!
//! Line-art, not solid fills: the shard is accent-stroked facets, the
//! particles are scattered orbit dots in quieter tones. Animation
//! (twinkle, slow orbit) comes later; the composition is the point.

use ryeos_client_base::text_surface::{Style, TextSurface};

use super::theme::{ACCENT, BG, FG_SOFT, GOOD, MUTED, WARN};

/// The mark, row by row. Glyph classes map to tones in `glyph_style`:
/// facet strokes are accent, the table line is warm, particles are
/// muted/soft/good by size.
const MARK: &[&str] = &[
    r"                ╱╲              ",
    r"       ·       ╱  ╲         ∙   ",
    r"              ╱ ╱╲ ╲            ",
    r"   ∙         ╱ ╱  ╲ ╲      ·    ",
    r"            ╱ ╱    ╲ ╲          ",
    r"     ·     ╱_╱______╲_╲       ∙ ",
    r"           ╲ ╲      ╱ ╱         ",
    r"    ∘       ╲ ╲    ╱ ╱     ·    ",
    r"             ╲ ╲  ╱ ╱           ",
    r"      ·       ╲ ╲╱ ╱        ∘   ",
    r"               ╲  ╱             ",
    r"        ∙       ╲╱       ·      ",
];

pub const MARK_WIDTH: usize = 32;
pub const MARK_HEIGHT: usize = 12;

fn glyph_style(ch: char) -> Style {
    match ch {
        '╱' | '╲' => Style::new().fg(ACCENT).bg(BG),
        '_' => Style::new().fg(WARN).bg(BG),
        '∙' => Style::new().fg(FG_SOFT).bg(BG),
        '∘' => Style::new().fg(GOOD).bg(BG),
        _ => Style::new().fg(MUTED).bg(BG),
    }
}

pub fn draw_mark(surface: &mut TextSurface, x: usize, y: usize) {
    for (row, line) in MARK.iter().enumerate() {
        for (col, ch) in line.chars().enumerate() {
            if ch != ' ' {
                surface.draw_char(x + col, y + row, ch, glyph_style(ch));
            }
        }
    }
}
