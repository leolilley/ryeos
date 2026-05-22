//! Space browser view — browse items across project/user/system.

use crate::model::AppModel;
use crate::text_surface::Style;
use crate::text_surface::TextSurface;
use crate::theme;
use crate::workspace::ViewLocalState;

pub fn build(model: &AppModel, w: usize, h: usize) -> TextSurface {
    let mut surface = TextSurface::new(w, h);
    surface.fill(Style::new().bg(theme::BG));

    if h == 0 || w == 0 {
        return surface;
    }

    let header_style = Style::new().fg(theme::FG).bg(theme::BG).bold();
    let signed_style = Style::new().fg(theme::GREEN).bg(theme::BG);
    let unsigned_style = Style::new().fg(theme::FG_MUTED).bg(theme::BG);
    let item_style = Style::new().fg(theme::FG_DIM).bg(theme::BG);
    let cat_style = Style::new().fg(theme::ACCENT).bg(theme::BG);
    let dim_style = Style::new().fg(theme::FG_MUTED).bg(theme::BG);

    // Get filter state
    let (filter, cursor) = model
        .workspace
        .tiles
        .get(&model.workspace.focused_tile)
        .and_then(|t| match &t.local {
            ViewLocalState::SpaceBrowser {
                query,
                cursor,
                scroll: _,
            } => Some((query.clone(), *cursor)),
            _ => None,
        })
        .unwrap_or_default();

    // Header
    surface.draw_text(0, 0, "Items", header_style);

    if !filter.is_empty() && w > 10 {
        let filter_text = format!("filter: {}", filter);
        surface.draw_text(8, 0, &filter_text, dim_style);
    }

    // Collect and sort items by name
    let mut items: Vec<_> = model.store.items.values().collect();
    items.sort_by(|a, b| a.name.cmp(&b.name));

    let mut row = 2;
    let mut visible_idx: usize = 0;

    for item in &items {
        // Apply filter
        if !filter.is_empty() {
            let filter_lower = filter.to_lowercase();
            let matches = item.kind.to_lowercase().contains(&filter_lower)
                || item.name.to_lowercase().contains(&filter_lower)
                || item
                    .description
                    .as_ref()
                    .map_or(false, |d| d.to_lowercase().contains(&filter_lower));
            if !matches {
                continue;
            }
        }

        if row >= h {
            break;
        }

        let is_selected = visible_idx == cursor;

        // Cursor marker
        let cursor_ch = if is_selected { '▸' } else { ' ' };
        surface.draw_char(
            0,
            row,
            cursor_ch,
            Style::new().fg(theme::ACCENT).bg(theme::BG),
        );

        // Signed icon
        let (sign_ch, sign_style) = if item.signed {
            ('✓', signed_style)
        } else {
            ('○', unsigned_style)
        };
        surface.draw_char(2, row, sign_ch, sign_style);

        // Kind badge
        let kind_display = match item.kind.as_str() {
            "directive" => "dir",
            "tool" => "tool",
            "knowledge" => "know",
            "config" => "cfg",
            _ => &item.kind,
        };
        surface.draw_text(4, row, kind_display, cat_style);

        // Item name
        let max_name_len = w.saturating_sub(20);
        let name_display = if item.name.len() > max_name_len {
            &item.name[..max_name_len]
        } else {
            &item.name
        };
        surface.draw_text(10, row, name_display, item_style);

        // Description (right side if space)
        if let Some(desc) = &item.description {
            let desc_start = 10 + name_display.len() + 2;
            if desc_start + 10 < w {
                let max_desc = w.saturating_sub(desc_start + 2);
                let desc_display = if desc.len() > max_desc {
                    &desc[..max_desc]
                } else {
                    desc.as_str()
                };
                surface.draw_text(desc_start, row, desc_display, dim_style);
            }
        }

        row += 1;
        visible_idx += 1;
    }

    // Empty state
    if items.is_empty() && h > 3 {
        let msg = "No items found";
        let x = w.saturating_sub(msg.len()) / 2;
        surface.draw_text(x, h / 2, msg, dim_style);
    }

    surface
}
