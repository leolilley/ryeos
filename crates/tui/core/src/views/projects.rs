//! Projects view — list of known projects.

use crate::model::AppModel;
use crate::text_surface::Style;
use crate::text_surface::TextSurface;
use crate::theme;

pub fn build(model: &AppModel, w: usize, h: usize) -> TextSurface {
    let mut surface = TextSurface::new(w, h);
    surface.fill(Style::new().bg(theme::BG));

    if h == 0 || w == 0 {
        return surface;
    }

    let header_style = Style::new().fg(theme::FG).bg(theme::BG).bold();
    let row_style = Style::new().fg(theme::FG_DIM).bg(theme::BG);
    let dim_style = Style::new().fg(theme::FG_MUTED).bg(theme::BG);
    let name_style = Style::new().fg(theme::ACCENT).bg(theme::BG);

    surface.draw_text(0, 0, "Projects", header_style);

    let mut row = 2;
    for (_, project) in &model.store.projects {
        if row >= h {
            break;
        }

        // Project name
        let max_name = w.saturating_sub(4);
        let name_display = if project.name.len() > max_name {
            &project.name[..max_name]
        } else {
            &project.name
        };
        surface.draw_text(1, row, name_display, name_style);
        row += 1;

        // Project path
        if row < h {
            let max_path = w.saturating_sub(6);
            let path_display = if project.path.len() > max_path {
                &project.path[..max_path]
            } else {
                &project.path
            };
            let path_line = format!("  {}", path_display);
            surface.draw_text(1, row, &path_line, row_style);
            row += 1;
        }

        // Item counts
        if row < h {
            let counts = format!(
                "  {} dir · {} tool · {} know",
                project.item_counts.directives,
                project.item_counts.tools,
                project.item_counts.knowledge,
            );
            surface.draw_text(1, row, &counts, dim_style);
            row += 1;
        }

        row += 1; // blank line between projects
    }

    // Empty state
    if model.store.projects.is_empty() && h > 3 {
        let msg = "No projects";
        let x = w.saturating_sub(msg.len()) / 2;
        surface.draw_text(x, h / 2, msg, dim_style);
    }

    surface
}
