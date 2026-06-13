//! The rows widget: tone glyph, primary+secondary, right-aligned meta,
//! full-width selection.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::studio::view_model::StudioRowVm;
use ryeos_client_base::text_surface::TextSurface;

use super::super::primitives::fill_line;
use super::super::text::{display_width, letterspace, truncate};
use super::super::theme::{style_fg, style_muted, style_selected, tone_glyph, tone_style};

pub fn draw_rows(surface: &mut TextSurface, rect: Rect, columns: &[String], rows: &[StudioRowVm]) {
    let width = rect.w as usize;
    let height = rect.h as usize;
    if width == 0 || height == 0 {
        return;
    }
    let mut y = rect.y as usize;
    let bottom = rect.y as usize + height;
    if !columns.is_empty() && y < bottom {
        let header = letterspace(&columns.join(" · "));
        surface.draw_text(rect.x as usize, y, &truncate(&header, width), style_muted());
        y += 1;
    }
    if rows.is_empty() && y < bottom {
        surface.draw_text(rect.x as usize, y, "no rows loaded", style_muted());
        return;
    }
    for row in rows.iter().take(bottom.saturating_sub(y)) {
        let style = if row.selected {
            style_selected()
        } else {
            style_fg()
        };
        fill_line(surface, rect.x as usize, y, width, style);
        let glyph_style = if row.selected {
            style
        } else {
            tone_style(row.tone)
        };
        surface.draw_text(rect.x as usize, y, tone_glyph(row.tone), glyph_style);

        let meta = row.meta.as_deref().unwrap_or_default();
        let meta_width = if meta.is_empty() {
            0
        } else {
            display_width(meta).min(width / 3)
        };
        let primary_width = width.saturating_sub(3 + meta_width + usize::from(meta_width > 0));
        let mut primary = row.primary.clone();
        if let Some(secondary) = &row.secondary {
            primary.push_str("  ");
            primary.push_str(secondary);
        }
        surface.draw_text(
            rect.x as usize + 2,
            y,
            &truncate(&primary, primary_width),
            style,
        );
        if meta_width > 0 {
            let meta_text = truncate(meta, meta_width);
            let meta_x = rect.x as usize + width.saturating_sub(display_width(&meta_text));
            let meta_style = if row.selected { style } else { style_muted() };
            surface.draw_text(meta_x, y, &meta_text, meta_style);
        }
        y += 1;
    }
}
