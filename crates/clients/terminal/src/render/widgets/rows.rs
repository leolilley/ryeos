//! The rows widget: tone glyph, primary+secondary, right-aligned meta,
//! full-width selection.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::text_surface::TextSurface;
use ryeos_client_base::ui::view_model::{RyeOsRowDetailVm, RyeOsRowVm};

use super::super::primitives::fill_line;
use super::super::text::{display_width, letterspace, truncate};
use super::super::theme::{
    active_pulse_style, shimmer_style, style_fg, style_muted, style_selected, tone_glyph,
    tone_style,
};

pub fn draw_rows(
    surface: &mut TextSurface,
    rect: Rect,
    columns: &[String],
    rows: &[RyeOsRowVm],
    now_ms: u64,
) {
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
    let selected_idx = rows.iter().position(|row| row.selected).unwrap_or(0);
    let visible_rows = bottom.saturating_sub(y);
    let max_offset = rows.len().saturating_sub(visible_rows);
    let offset = selected_idx
        .saturating_sub(visible_rows / 2)
        .min(max_offset);

    for row in rows.iter().skip(offset) {
        if y >= bottom {
            break;
        }
        let mut style = if row.selected {
            style_selected()
        } else {
            style_fg()
        };
        style = active_pulse_style(style, row.tone, now_ms);
        style = shimmer_style(style, row.changed_at_ms, now_ms);
        fill_line(surface, rect.x as usize, y, width, style);
        let glyph_style = if row.selected {
            style
        } else {
            tone_style(row.tone)
        };
        let glyph = if row.expandable {
            if row.expanded {
                "▾"
            } else {
                "▸"
            }
        } else {
            tone_glyph(row.tone)
        };
        surface.draw_text(rect.x as usize, y, glyph, glyph_style);

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
        for detail in &row.detail {
            if y >= bottom {
                break;
            }
            draw_detail(surface, rect.x as usize, y, width, detail);
            y += 1;
        }
    }
}

/// One `field: value` detail line under a row, shared by the rows and table
/// widgets so the label-width math cannot fork between them again.
pub(super) fn draw_detail(
    surface: &mut TextSurface,
    left: usize,
    y: usize,
    width: usize,
    detail: &RyeOsRowDetailVm,
) {
    fill_line(surface, left, y, width, style_fg());
    let label = format!("  {}: ", detail.field);
    surface.draw_text(left, y, &truncate(&label, width), style_muted());
    let x = left + display_width(&label).min(width);
    if x < left + width {
        let style = detail.tone.map(tone_style).unwrap_or_else(style_fg);
        surface.draw_text(x, y, &truncate(&detail.value, left + width - x), style);
    }
}
