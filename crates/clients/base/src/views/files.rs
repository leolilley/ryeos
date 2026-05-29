//! Files view — read-only safe project file listing.

use crate::ids::TileId;
use crate::model::AppModel;
use crate::text_surface::{Style, TextSurface};
use crate::theme;
use crate::workspace::ViewLocalState;

pub fn build(model: &AppModel, tile_id: TileId, w: usize, h: usize) -> TextSurface {
    let mut surface = TextSurface::new(w, h);
    surface.fill(Style::new().bg(theme::BG));

    if h == 0 || w == 0 {
        return surface;
    }

    let header = Style::new().fg(theme::FG).bg(theme::BG).bold();
    let dir_style = Style::new().fg(theme::ACCENT).bg(theme::BG).bold();
    let file_style = Style::new().fg(theme::FG_DIM).bg(theme::BG);
    let muted = Style::new().fg(theme::FG_MUTED).bg(theme::BG);
    let warn = Style::new().fg(theme::YELLOW).bg(theme::BG);
    let selected_style = Style::new().fg(theme::BG).bg(theme::ACCENT).bold();

    surface.draw_text(0, 0, "Files", header);
    let Some(files) = &model.store.files else {
        let msg = "Waiting for project file listing";
        surface.draw_text(w.saturating_sub(msg.len()) / 2, h / 2, msg, muted);
        return surface;
    };

    let location = if files.path.is_empty() {
        files.root.clone()
    } else {
        format!("{}/{}", files.root, files.path)
    };
    if w > 10 {
        surface.draw_text(7, 0, &truncate(&location, w.saturating_sub(8)), muted);
    }

    if files.truncated && h > 1 {
        surface.draw_text(0, 1, "listing truncated", warn);
    }

    let cursor = model
        .workspace
        .tiles
        .get(&tile_id)
        .and_then(|tile| match &tile.local {
            ViewLocalState::GenericList { cursor, .. } => Some(*cursor),
            _ => None,
        })
        .unwrap_or(0);

    let list_height = if model.store.file_read.is_some() {
        h.saturating_div(2).max(4)
    } else {
        h
    };

    let mut row = 2;
    let mut idx = 0;
    if !files.path.is_empty() && row < list_height {
        let is_selected = cursor == 0;
        let style = if is_selected {
            selected_style
        } else {
            dir_style
        };
        surface.draw_text(0, row, "◂", style);
        surface.draw_text(1, row, " ../", style);
        row += 1;
        idx += 1;
    }

    for entry in &files.entries {
        if row >= list_height {
            break;
        }
        let is_selected = idx == cursor;
        let icon = if entry.is_dir { "▸" } else { "•" };
        surface.draw_text(
            0,
            row,
            icon,
            if is_selected {
                selected_style
            } else if entry.is_dir {
                dir_style
            } else {
                muted
            },
        );

        let size = if entry.is_dir {
            "dir".to_string()
        } else {
            entry
                .size
                .map(format_size)
                .unwrap_or_else(|| "file".to_string())
        };
        let name = if entry.is_dir {
            format!("{}/", entry.name)
        } else {
            entry.name.clone()
        };
        let line = if w > 72 {
            format!(" {:<48} {:>10}", truncate(&name, 48), size)
        } else {
            format!(" {}", truncate(&name, w.saturating_sub(3)))
        };
        let style = if is_selected {
            selected_style
        } else if entry.is_dir {
            dir_style
        } else {
            file_style
        };
        surface.draw_text(1, row, &line, style);
        row += 1;
        idx += 1;
    }

    if files.entries.is_empty() && h > 3 {
        let msg = "No files found";
        surface.draw_text(w.saturating_sub(msg.len()) / 2, h / 2, msg, muted);
    }

    if let Some(file) = &model.store.file_read {
        let mut row = list_height.saturating_add(1);
        if row < h {
            let title = if file.truncated {
                format!("{} ({} bytes, truncated)", file.path, file.size)
            } else {
                format!("{} ({} bytes)", file.path, file.size)
            };
            surface.draw_text(0, row, &truncate(&title, w), header);
            row += 1;
        }
        for line in file.content.lines().take(h.saturating_sub(row)) {
            if row >= h {
                break;
            }
            surface.draw_text(1, row, &truncate(line, w.saturating_sub(2)), file_style);
            row += 1;
        }
    }

    surface
}

fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    if bytes >= MIB {
        format!("{} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{} B", bytes)
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
