//! Graph view — state graph topology visualization.

use crate::model::AppModel;
use crate::text_surface::Style;
use crate::text_surface::TextSurface;
use crate::theme;

pub fn build(_model: &AppModel, w: usize, h: usize) -> TextSurface {
    let mut surface = TextSurface::new(w, h);
    surface.fill(Style::new().bg(theme::BG));

    if h == 0 || w == 0 {
        return surface;
    }

    let header_style = Style::new().fg(theme::FG).bg(theme::BG).bold();
    let dim_style = Style::new().fg(theme::FG_DIM).bg(theme::BG);
    let _accent_style = Style::new().fg(theme::ACCENT).bg(theme::BG);
    let node_style = Style::new().fg(theme::YELLOW).bg(theme::BG);
    let edge_style = Style::new().fg(theme::FG_MUTED).bg(theme::BG);
    let muted_style = Style::new().fg(theme::FG_MUTED).bg(theme::BG);

    // Header
    surface.draw_text(0, 0, "Graph", header_style);

    // Placeholder graph visualization
    // In V1, we draw a simple ASCII art topology
    let mut row = 2;

    // Draw sample graph nodes
    let nodes = [
        ("start", 0.5, 0.2),
        ("action:fetch", 0.3, 0.4),
        ("action:parse", 0.7, 0.4),
        ("gate:check", 0.5, 0.6),
        ("return:result", 0.5, 0.8),
    ];

    if h > 10 && w > 30 {
        // Draw edges first (behind nodes)
        for i in 0..nodes.len().saturating_sub(1) {
            let (_, x1, y1) = nodes[i];
            let (_, x2, y2) = nodes[i + 1];
            let avail_w = (w.max(11) - 10) as f32;
            let avail_h = (h.max(5) - 4) as f32;
            let px1 = (x1 * avail_w) as usize + 5;
            let py1 = (y1 * avail_h) as usize + 3;
            let px2 = (x2 * avail_w) as usize + 5;
            let py2 = (y2 * avail_h) as usize + 3;

            // Simple line using dashes and pipes
            if py1 < h && py2 < h {
                let mid_y = (py1 + py2) / 2;
                if mid_y < h && px1 < w {
                    surface.draw_char(px1, mid_y, '│', edge_style);
                }
                if mid_y < h && px2 < w {
                    surface.draw_char(px2, mid_y, '│', edge_style);
                }
                // Horizontal connector
                if py1 < h {
                    let min_x = px1.min(px2);
                    let max_x = px1.max(px2);
                    for x in min_x..=max_x.min(w - 1) {
                        let ch = surface.get(x, py1).rune;
                        if ch == ' ' || ch == '─' {
                            surface.draw_char(x, py1, '─', edge_style);
                        }
                    }
                }
            }
        }

        // Draw nodes on top
        let avail_w = (w.max(11) - 10) as f32;
        let avail_h = (h.max(5) - 4) as f32;
        for (name, rx, ry) in &nodes {
            let px = (*rx * avail_w) as usize + 5;
            let py = (*ry * avail_h) as usize + 3;

            if py < h && px + name.len() + 2 < w {
                // Draw node box
                let label = format!("[{}]", name);
                surface.draw_text(px.saturating_sub(name.len() / 2), py, &label, node_style);
            }
        }

        // Step counter
        let step_info = "Step 0/0 · max_steps: —";
        if h > 2 {
            surface.draw_text(
                w.saturating_sub(step_info.len() + 1),
                0,
                step_info,
                muted_style,
            );
        }
    } else {
        // Compact mode
        surface.draw_text(1, row, "Graph topology view", dim_style);
        row += 1;
        surface.draw_text(1, row, "(expand tile for visualization)", muted_style);
    }

    surface
}
