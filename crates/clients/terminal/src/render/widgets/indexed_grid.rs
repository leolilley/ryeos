//! Compact terminal realization of the generic indexed-grid preview.

use std::collections::{BTreeMap, BTreeSet};

use ryeos_client_base::layout::Rect;
use ryeos_client_base::text_surface::TextSurface;
use ryeos_client_base::ui::field::RyeOsIndexedGridVm;

use super::super::text::truncate;
use super::super::theme::{style_fg, style_muted, style_selected};

pub fn draw_indexed_grid(
    surface: &mut TextSurface,
    rect: Rect,
    title: &str,
    grid: &RyeOsIndexedGridVm,
) {
    if rect.w == 0 || rect.h == 0 {
        return;
    }
    let x = rect.x as usize;
    let y = rect.y as usize;
    let width = rect.w as usize;
    let height = rect.h as usize;
    let cell_width = if grid.width as usize <= width / 2 {
        2
    } else {
        1
    };
    let visible_columns = (width / cell_width).min(grid.width as usize);
    let visible_rows = height.saturating_sub(1).min(grid.height as usize);
    let extent = if visible_columns < grid.width as usize || visible_rows < grid.height as usize {
        format!(
            "{title} · {visible_columns}x{visible_rows} of {}x{}",
            grid.width, grid.height
        )
    } else {
        title.to_string()
    };
    surface.draw_text(x, y, &truncate(&extent, width), style_muted());
    if height <= 1 {
        return;
    }
    let palette = grid
        .palette
        .iter()
        .map(|entry| (entry.index, entry.glyph.as_str()))
        .collect::<BTreeMap<_, _>>();
    let changed = grid.changed.iter().copied().collect::<BTreeSet<_>>();
    for row in 0..visible_rows {
        for column in 0..visible_columns {
            let index = row * grid.width as usize + column;
            let glyph = palette
                .get(grid.cells.get(index).unwrap_or(&u16::MAX))
                .and_then(|glyph| glyph.chars().next())
                .unwrap_or('?')
                .to_string();
            let text = if cell_width == 2 {
                format!("{glyph} ")
            } else {
                glyph
            };
            surface.draw_text(
                x + column * cell_width,
                y + 1 + row,
                &text,
                if changed.contains(&(index as u32)) {
                    style_selected()
                } else {
                    style_fg()
                },
            );
        }
    }
}
