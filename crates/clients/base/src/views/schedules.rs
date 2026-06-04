//! Schedules and GC operational views.

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
    let ok = Style::new().fg(theme::GREEN).bg(theme::BG);
    let dim = Style::new().fg(theme::FG_DIM).bg(theme::BG);
    let muted = Style::new().fg(theme::FG_MUTED).bg(theme::BG);
    let warn = Style::new().fg(theme::YELLOW).bg(theme::BG);

    surface.draw_text(0, 0, "Schedules", header);
    let schedules = &model.store.schedules.schedules;
    if w > 16 {
        let summary = format!("{} schedules", schedules.len());
        surface.draw_text(w.saturating_sub(summary.len() + 1), 0, &summary, muted);
    }

    for (row, schedule) in (2..).zip(schedules.iter()) {
        if row >= h {
            break;
        }
        let (dot, status_style) = if schedule.enabled {
            ('●', ok)
        } else {
            ('○', muted)
        };
        surface.draw_char(0, row, dot, status_style);

        let status = schedule.last_fire_status.as_deref().unwrap_or("never");
        let line = if w > 90 {
            format!(
                " {:<24} {:<12} {:<24} fires:{:<4} {}",
                truncate(&schedule.schedule_id, 24),
                truncate(&schedule.schedule_type, 12),
                truncate(&schedule.expression, 24),
                schedule.total_fires,
                truncate(&schedule.item_ref, w.saturating_sub(72))
            )
        } else {
            format!(
                " {:<20} {:<10} {}",
                truncate(&schedule.schedule_id, 20),
                truncate(status, 10),
                truncate(&schedule.item_ref, w.saturating_sub(34))
            )
        };
        surface.draw_text(1, row, &line, if schedule.enabled { dim } else { warn });
    }

    if schedules.is_empty() && h > 3 {
        let msg = "No schedules registered";
        surface.draw_text(w.saturating_sub(msg.len()) / 2, h / 2, msg, muted);
    }

    surface
}

pub fn build_gc(model: &AppModel, w: usize, h: usize) -> TextSurface {
    let mut surface = TextSurface::new(w, h);
    surface.fill(Style::new().bg(theme::BG));

    if h == 0 || w == 0 {
        return surface;
    }

    let header = Style::new().fg(theme::FG).bg(theme::BG).bold();
    let accent = Style::new().fg(theme::ACCENT).bg(theme::BG).bold();
    let ok = Style::new().fg(theme::GREEN).bg(theme::BG);
    let warn = Style::new().fg(theme::YELLOW).bg(theme::BG);
    let dim = Style::new().fg(theme::FG_DIM).bg(theme::BG);
    let muted = Style::new().fg(theme::FG_MUTED).bg(theme::BG);

    surface.draw_text(0, 0, "GC Status", header);
    let Some(gc) = &model.store.gc_status else {
        let msg = "Waiting for GC status";
        surface.draw_text(w.saturating_sub(msg.len()) / 2, h / 2, msg, muted);
        return surface;
    };

    let status = if gc.running {
        "● running"
    } else {
        "○ idle"
    };
    surface.draw_text(0, 2, status, if gc.running { warn } else { ok });

    let mut row = 4;
    if let Some(state) = &gc.state {
        surface.draw_text(0, row, "State", accent);
        row += 1;
        let state_text = serde_json::to_string_pretty(state).unwrap_or_else(|_| state.to_string());
        for line in state_text.lines().take(h.saturating_sub(row + 2)) {
            if row >= h {
                break;
            }
            surface.draw_text(1, row, &truncate(line, w.saturating_sub(2)), dim);
            row += 1;
        }
        row += 1;
    }

    if row < h {
        surface.draw_text(0, row, "Recent events", accent);
        row += 1;
    }
    for event in gc.recent_events.iter().rev().take(h.saturating_sub(row)) {
        if row >= h {
            break;
        }
        let line = event
            .get("event")
            .or_else(|| event.get("phase"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| event.to_string());
        surface.draw_text(1, row, &truncate(&line, w.saturating_sub(2)), muted);
        row += 1;
    }

    if gc.recent_events.is_empty() && row < h {
        surface.draw_text(1, row, "No recent GC events", muted);
    }

    surface
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
