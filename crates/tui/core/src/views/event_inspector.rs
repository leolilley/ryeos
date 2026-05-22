//! Event inspector — raw event stream fallback view.

use crate::model::AppModel;
use crate::text_surface::{Style, TextSurface};
use crate::theme;

pub fn build(model: &AppModel, w: usize, h: usize) -> TextSurface {
    let mut surface = TextSurface::new(w, h);
    surface.fill(Style::new().bg(theme::BG));

    if h == 0 || w == 0 {
        return surface;
    }

    let header_style = Style::new().fg(theme::FG).bg(theme::BG).bold();
    let dim_style = Style::new().fg(theme::FG_DIM).bg(theme::BG);
    let muted_style = Style::new().fg(theme::FG_MUTED).bg(theme::BG);

    surface.draw_text(0, 0, "Events", header_style);

    let count = model.store.events.len();
    let count_text = format!("({} events)", count);
    if w > count_text.len() + 10 {
        surface.draw_text(
            w.saturating_sub(count_text.len() + 1),
            0,
            &count_text,
            dim_style,
        );
    }

    // Show recent events (newest first, bottom-anchored)
    let mut row = 2;
    let start = if count > h - 2 { count - (h - 2) } else { 0 };

    for event in model.store.events.iter().skip(start) {
        if row >= h {
            break;
        }

        // Type badge
        surface.draw_text(1, row, &event.event_type, dim_style);

        // Timestamp
        let ts = format!("t={}", event.timestamp_ms % 100_000);
        let _max_w = w.saturating_sub(event.event_type.len() + ts.len() + 5);
        if w > 30 {
            surface.draw_text(w.saturating_sub(ts.len() + 1), row, &ts, muted_style);
        }

        row += 1;
    }

    if count == 0 && h > 3 {
        let msg = "No events yet";
        let x = w.saturating_sub(msg.len()) / 2;
        surface.draw_text(x, h / 2, msg, muted_style);
    }

    surface
}
