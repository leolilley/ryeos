//! Item inspector — read-only raw/effective item details.

use crate::model::AppModel;
use crate::text_surface::{Style, TextSurface};
use crate::theme;

pub fn build(model: &AppModel, w: usize, h: usize) -> TextSurface {
    let mut surface = TextSurface::new(w, h);
    surface.fill(Style::new().bg(theme::BG));

    if h == 0 || w == 0 {
        return surface;
    }

    let header = Style::new().fg(theme::FG).bg(theme::BG).bold();
    let accent = Style::new().fg(theme::ACCENT).bg(theme::BG).bold();
    let dim = Style::new().fg(theme::FG_DIM).bg(theme::BG);
    let muted = Style::new().fg(theme::FG_MUTED).bg(theme::BG);

    surface.draw_text(0, 0, "Item Inspector", header);

    let Some(item) = &model.store.item_inspection else {
        let msg = "Select an item and press Enter";
        surface.draw_text(w.saturating_sub(msg.len()) / 2, h / 2, msg, muted);
        return surface;
    };

    let mut row = 2;
    draw_kv(&mut surface, w, row, "ref", &item.canonical_ref, accent);
    row += 1;
    draw_kv(&mut surface, w, row, "kind", &item.item_kind, dim);
    row += 1;
    draw_kv(&mut surface, w, row, "space", &item.space, dim);
    row += 1;
    draw_kv(&mut surface, w, row, "source", &item.source_path, muted);
    row += 2;

    if row < h {
        let raw_title = if item.raw_truncated {
            "Raw (truncated)"
        } else {
            "Raw"
        };
        surface.draw_text(0, row, raw_title, accent);
        row += 1;
    }

    if let Some(raw) = &item.raw_content {
        for line in raw.lines().take(h.saturating_sub(row + 2)) {
            if row >= h {
                break;
            }
            surface.draw_text(1, row, &truncate(line, w.saturating_sub(2)), dim);
            row += 1;
        }
    } else if row < h {
        surface.draw_text(1, row, "raw content not loaded", muted);
        row += 1;
    }

    if row + 2 < h {
        row += 1;
        surface.draw_text(0, row, "Effective", accent);
        row += 1;
        let effective = item
            .effective
            .as_ref()
            .map(|value| serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()))
            .unwrap_or_else(|| "not loaded".into());
        for line in effective.lines().take(h.saturating_sub(row)) {
            if row >= h {
                break;
            }
            surface.draw_text(1, row, &truncate(line, w.saturating_sub(2)), muted);
            row += 1;
        }
    }

    surface
}

fn draw_kv(surface: &mut TextSurface, w: usize, row: usize, key: &str, value: &str, style: Style) {
    let key_style = Style::new().fg(theme::FG_MUTED).bg(theme::BG);
    let key_display = format!("  {:<8}", key);
    surface.draw_text(0, row, &truncate(&key_display, w), key_style);
    if w > key_display.len() {
        surface.draw_text(
            key_display.len(),
            row,
            &truncate(value, w.saturating_sub(key_display.len())),
            style,
        );
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    if max <= 1 {
        return "…".repeat(max);
    }
    let mut out: String = value.chars().take(max - 1).collect();
    out.push('…');
    out
}
