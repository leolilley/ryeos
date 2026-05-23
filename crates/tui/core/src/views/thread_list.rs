//! Thread list view — recent threads from the store.

use crate::model::AppModel;
use crate::ids::TileId;
use crate::store::ThreadStatus;
use crate::text_surface::Style;
use crate::text_surface::TextSurface;
use crate::theme;
use crate::workspace::ViewLocalState;

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub fn build(model: &AppModel, tile_id: TileId, w: usize, h: usize) -> TextSurface {
    let mut surface = TextSurface::new(w, h);
    surface.fill(Style::new().bg(theme::BG));

    if h == 0 || w == 0 {
        return surface;
    }

    // Get filter from THIS tile's view-local state
    let (filter, cursor) = model
        .workspace
        .tiles
        .get(&tile_id)
        .and_then(|t| match &t.local {
            ViewLocalState::ThreadList { filter, cursor } => {
                Some((filter.clone(), *cursor))
            }
            _ => None,
        })
        .unwrap_or_default();

    let header_style = Style::new().fg(theme::FG).bg(theme::BG).bold();
    let row_style = Style::new().fg(theme::FG_DIM).bg(theme::BG);
    let running_style = Style::new().fg(theme::YELLOW).bg(theme::BG);
    let completed_style = Style::new().fg(theme::GREEN).bg(theme::BG);
    let failed_style = Style::new().fg(theme::RED).bg(theme::BG);
    let cursor_style = Style::new().fg(theme::ACCENT).bg(theme::BG);
    let dim_style = Style::new().fg(theme::FG_MUTED).bg(theme::BG);

    // Header
    surface.draw_text(0, 0, "Threads", header_style);

    let running_count = model.store.running_thread_count();
    if running_count > 0 && w > 20 {
        let info = format!("({} running)", running_count);
        surface.draw_text(w.saturating_sub(info.len() + 1), 0, &info, running_style);
    }

    // Thread rows
    let threads = model.store.recent_threads();
    let mut row = 2;

    let mut visible_idx = 0;
    for thread in &threads {
        // Apply filter
        if !filter.is_empty() {
            let matches = thread
                .item_ref
                .as_ref()
                .map(|r| r.contains(&filter))
                .unwrap_or(false)
                || format!("{:?}", thread.status)
                    .to_lowercase()
                    .contains(&filter.to_lowercase());
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
        surface.draw_char(0, row, cursor_ch, cursor_style);

        // Status icon
        let (status_ch, status_style) = match thread.status {
            ThreadStatus::Running => {
                let frame = SPINNER_FRAMES
                    [(model.visual.animation.time_ms as usize / 100) % SPINNER_FRAMES.len()];
                (frame, running_style)
            }
            ThreadStatus::Completed => ('✓', completed_style),
            ThreadStatus::Failed => ('✗', failed_style),
            ThreadStatus::Created => ('○', dim_style),
            ThreadStatus::Cancelled => ('○', dim_style),
            ThreadStatus::Killed => ('✗', dim_style),
            ThreadStatus::TimedOut => ('✗', dim_style),
            ThreadStatus::Continued => ('↳', dim_style),
        };
        surface.draw_char(2, row, status_ch, status_style);

        // Thread ID (truncated)
        let id_str = format!("thr_{:06x}", thread.id.0 & 0xFFFFFF);
        let id_display = &id_str[..id_str.len().min(10)];
        surface.draw_text(4, row, id_display, dim_style);

        // Item ref
        let item_ref = thread.item_ref.as_deref().unwrap_or("—");
        let max_item_len = w.saturating_sub(18);
        let item_display = if item_ref.len() > max_item_len {
            &item_ref[..max_item_len]
        } else {
            item_ref
        };
        surface.draw_text(15, row, item_display, row_style);

        // Duration/cost on right side
        if let Some(dur) = elapsed_str(thread.started_at_ms, thread.completed_at_ms) {
            if w > 45 {
                surface.draw_text(w.saturating_sub(dur.len() + 8), row, &dur, dim_style);
            }
        }

        if thread.usage.spend_usd > 0.0 && w > 50 {
            let cost = format!("${:.2}", thread.usage.spend_usd);
            surface.draw_text(w.saturating_sub(cost.len() + 1), row, &cost, row_style);
        }

        row += 1;
        visible_idx += 1;
    }

    // Empty state
    if threads.is_empty() && h > 3 {
        let msg = "No threads yet";
        let x = w.saturating_sub(msg.len()) / 2;
        surface.draw_text(x, h / 2, msg, dim_style);
    }

    surface
}

fn elapsed_str(start_ms: Option<i64>, end_ms: Option<i64>) -> Option<String> {
    let start = start_ms?;
    let end = end_ms.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    });
    let secs = ((end - start) / 1000) as u64;
    if secs < 60 {
        Some(format!("{}s", secs))
    } else if secs < 3600 {
        Some(format!("{}m{}s", secs / 60, secs % 60))
    } else {
        Some(format!("{}h{}m", secs / 3600, (secs % 3600) / 60))
    }
}
