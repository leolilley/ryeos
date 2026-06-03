//! Overview view — operational daemon/node snapshot.

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
    let ok = Style::new().fg(theme::GREEN).bg(theme::BG);
    let warn = Style::new().fg(theme::YELLOW).bg(theme::BG);
    let err = Style::new().fg(theme::RED).bg(theme::BG);
    let dim = Style::new().fg(theme::FG_DIM).bg(theme::BG);
    let muted = Style::new().fg(theme::FG_MUTED).bg(theme::BG);

    surface.draw_text(0, 0, "Overview", header);

    let Some(snapshot) = &model.store.studio else {
        let msg = "Waiting for daemon snapshot";
        surface.draw_text(w.saturating_sub(msg.len()) / 2, h / 2, msg, muted);
        return surface;
    };

    let mut row = 2;
    draw_section(&mut surface, w, h, &mut row, "Node", accent);
    if row < h {
        let (icon, style) = match snapshot.local_node.health_status.as_str() {
            "healthy" => ("●", ok),
            "degraded" => ("◐", warn),
            _ => ("○", err),
        };
        draw_kv(
            &mut surface,
            w,
            row,
            "health",
            &format!("{} {}", icon, snapshot.local_node.health_status),
            style,
        );
        row += 1;
    }
    if row < h {
        draw_kv(
            &mut surface,
            w,
            row,
            "identity",
            &short(&snapshot.local_node.identity.fingerprint),
            dim,
        );
        row += 1;
    }
    if row < h {
        draw_kv(
            &mut surface,
            w,
            row,
            "project",
            snapshot
                .project
                .as_ref()
                .map(|p| p.path.as_str())
                .unwrap_or("—"),
            dim,
        );
        row += 1;
    }
    if row < h {
        draw_kv(
            &mut surface,
            w,
            row,
            "surface",
            &snapshot.session.surface_ref,
            dim,
        );
        row += 2;
    }

    draw_section(&mut surface, w, h, &mut row, "Runtime", accent);
    for (label, value, style) in [
        (
            "services",
            format!(
                "{} registered · {}",
                snapshot.local_node.services.len(),
                snapshot.local_node.operational_services
            ),
            dim,
        ),
        (
            "threads",
            format!("{} active", model.store.daemon.active_threads),
            dim,
        ),
        (
            "schedules",
            format!(
                "{} enabled / {} total",
                snapshot.schedules.enabled, snapshot.schedules.total
            ),
            dim,
        ),
        (
            "gc",
            if snapshot.gc.running {
                "running".to_string()
            } else {
                format!("idle · {} recent", snapshot.gc.recent_event_count)
            },
            if snapshot.gc.running { warn } else { dim },
        ),
    ] {
        if row >= h {
            break;
        }
        draw_kv(&mut surface, w, row, label, &value, style);
        row += 1;
    }

    row += 1;
    draw_section(&mut surface, w, h, &mut row, "Spaces", accent);
    for space in snapshot.local_node.spaces.iter().take(4) {
        if row >= h {
            break;
        }
        draw_kv(&mut surface, w, row, &space.space, &space.path, muted);
        row += 1;
    }

    if !snapshot.local_node.missing_services.is_empty() && row + 2 < h {
        row += 1;
        draw_section(&mut surface, w, h, &mut row, "Missing services", accent);
        for service in snapshot.local_node.missing_services.iter().take(3) {
            if row >= h {
                break;
            }
            surface.draw_text(1, row, &truncate(service, w.saturating_sub(2)), err);
            row += 1;
        }
    }

    surface
}

fn draw_section(
    surface: &mut TextSurface,
    _w: usize,
    h: usize,
    row: &mut usize,
    title: &str,
    style: Style,
) {
    if *row < h {
        surface.draw_text(0, *row, title, style);
        *row += 1;
    }
}

fn draw_kv(surface: &mut TextSurface, w: usize, row: usize, key: &str, value: &str, style: Style) {
    let key_style = Style::new().fg(theme::FG_MUTED).bg(theme::BG);
    let key_display = format!("  {:<10}", key);
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

fn short(value: &str) -> String {
    if value.len() <= 16 {
        value.to_string()
    } else {
        format!("{}…{}", &value[..8], &value[value.len() - 6..])
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
