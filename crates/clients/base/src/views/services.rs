//! Services view — daemon service catalog and command surface.

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
    let dim = Style::new().fg(theme::FG_DIM).bg(theme::BG);
    let muted = Style::new().fg(theme::FG_MUTED).bg(theme::BG);

    surface.draw_text(0, 0, "Services", header);

    let Some(snapshot) = &model.store.cockpit else {
        let msg = "Waiting for daemon snapshot";
        surface.draw_text(w.saturating_sub(msg.len()) / 2, h / 2, msg, muted);
        return surface;
    };

    if w > 24 {
        let summary = format!(
            "{} services · {} verbs · {} aliases",
            snapshot.local_node.services.len(),
            snapshot.local_node.verbs.len(),
            snapshot.local_node.aliases.len()
        );
        surface.draw_text(w.saturating_sub(summary.len() + 1), 0, &summary, muted);
    }

    let mut row = 2;
    surface.draw_text(0, row, "Catalog", accent);
    row += 1;

    let mut services = snapshot.local_node.services.clone();
    services.sort_by(|a, b| a.endpoint.cmp(&b.endpoint));
    for service in services.iter().take(h.saturating_sub(row + 1)) {
        if row >= h {
            break;
        }
        let service_style = if service.availability.to_ascii_lowercase().contains("daemon") {
            ok
        } else {
            dim
        };
        let caps = if service.required_caps.is_empty() {
            "open".to_string()
        } else {
            service.required_caps.join(",")
        };
        let line = if w > 80 {
            format!(
                "  {:<30} {:<32} {}",
                truncate(&service.endpoint, 30),
                truncate(&service.service_ref, 32),
                truncate(&caps, w.saturating_sub(68))
            )
        } else {
            format!("  {}", truncate(&service.endpoint, w.saturating_sub(4)))
        };
        surface.draw_text(0, row, &line, service_style);
        row += 1;
    }

    if row + 3 < h {
        row += 1;
        surface.draw_text(0, row, "Commands", accent);
        row += 1;

        let mut verbs = snapshot.local_node.verbs.clone();
        verbs.sort_by(|a, b| a.name.cmp(&b.name));
        for verb in verbs.iter().take(4) {
            if row >= h {
                break;
            }
            let target = verb.target.as_deref().unwrap_or("—");
            let line = format!(
                "  {:<18} {}",
                truncate(&verb.name, 18),
                truncate(target, w.saturating_sub(22))
            );
            surface.draw_text(0, row, &line, dim);
            row += 1;
        }
    }

    if !snapshot.local_node.missing_services.is_empty() && row + 2 < h {
        row += 1;
        surface.draw_text(0, row, "Missing", accent);
        row += 1;
        for service in snapshot.local_node.missing_services.iter().take(3) {
            if row >= h {
                break;
            }
            surface.draw_text(2, row, &truncate(service, w.saturating_sub(3)), warn);
            row += 1;
        }
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
