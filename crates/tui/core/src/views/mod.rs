//! View builders — each ViewSpec maps to a view builder that produces
//! a TextSurface from model state.

pub mod event_inspector;
pub mod remotes;
pub mod thread;
pub mod thread_list;

use crate::ids::TileId;
use crate::layout::Rect;
use crate::model::AppModel;
use crate::text_surface::{Border, Style, TextSurface};
use crate::theme;
use crate::workspace::ViewSpec;

/// Build a tile's text surface for the given view spec.
pub fn build_tile_view(
    model: &AppModel,
    tile_id: TileId,
    rect: Rect,
    focused: bool,
) -> TextSurface {
    let tile = match model.workspace.tiles.get(&tile_id) {
        Some(t) => t,
        None => return empty_surface(rect),
    };

    let inner_w = if rect.w > 2 { rect.w as usize - 2 } else { 1 };
    let inner_h = if rect.h > 2 { rect.h as usize - 2 } else { 1 };

    let mut surface = TextSurface::new(rect.w as usize, rect.h as usize);

    // Background fill
    surface.fill(Style::new().bg(theme::BG));

    // Border
    let border_style = if focused {
        theme::style_border_active()
    } else {
        theme::style_border()
    };
    surface.draw_box(
        0,
        0,
        rect.w as usize - 1,
        rect.h as usize - 1,
        Border::Rounded,
        border_style,
    );

    // Title
    let title = tile.view.title();
    let title_style = if focused {
        Style::new().fg(theme::ACCENT).bg(theme::BG).bold()
    } else {
        Style::new().fg(theme::FG_DIM).bg(theme::BG)
    };
    if inner_w > 4 {
        let title_display = if title.len() > inner_w - 2 {
            &title[..inner_w - 2]
        } else {
            &title
        };
        surface.draw_text(2, 0, title_display, title_style);
    }

    // Build inner content
    let content = match &tile.view {
        ViewSpec::ThreadList => thread_list::build(model, inner_w, inner_h),
        ViewSpec::Thread { .. } => thread::build(model, tile_id, inner_w, inner_h),
        ViewSpec::Remotes => remotes::build(model, inner_w, inner_h),
        ViewSpec::EventInspector => event_inspector::build(model, inner_w, inner_h),
        ViewSpec::Projects => projects_fallback(model, inner_w, inner_h),
        ViewSpec::SpaceBrowser { .. } => placeholder("Space Browser", inner_w, inner_h),
        ViewSpec::Trust => placeholder("Trust", inner_w, inner_h),
        ViewSpec::Graph { .. } => placeholder("Graph", inner_w, inner_h),
    };

    // Blit content inside border
    blit_surface(&mut surface, 1, 1, &content);

    surface
}

/// Build the input bar surface.
pub fn build_input_bar(model: &AppModel, rect: Rect) -> TextSurface {
    let mut surface = TextSurface::new(rect.w as usize, rect.h as usize);
    surface.fill(theme::style_input_bar());

    let _cap = model.workspace.focused_capability();
    let hint = model
        .workspace
        .focused_view()
        .map(|v| v.input_hint())
        .unwrap_or("input");

    // Draw hint
    let hint_text = format!(" {} ", hint);
    let hint_style = Style::new().fg(theme::BG_DARK).bg(theme::ACCENT);
    surface.draw_text(1, 0, &hint_text, hint_style);

    let hint_width = hint_text.chars().count();

    // Draw input text
    let text_style = Style::new().fg(theme::FG).bg(theme::BG_DARK);
    let max_text_w = rect.w as usize - hint_width - 3;
    let text = &model.workspace.input_bar.text;
    let display_text = if text.len() > max_text_w {
        &text[text.len().saturating_sub(max_text_w)..]
    } else {
        text.as_str()
    };
    surface.draw_text(hint_width + 2, 0, display_text, text_style);

    // Cursor indicator
    let cursor_x = hint_width + 2 + model.workspace.input_bar.cursor.min(display_text.len());
    if cursor_x < rect.w as usize {
        surface.draw_char(
            cursor_x,
            0,
            '▎',
            Style::new().fg(theme::ACCENT).bg(theme::BG_DARK),
        );
    }

    surface
}

/// Build overlay surfaces.
pub fn build_overlays(model: &AppModel, viewport: Rect) -> Vec<crate::frame::OverlaySurface> {
    use crate::frame::{OverlaySurface, OverlayType};
    use crate::model::OverlayState;

    let overlay = match &model.overlay {
        Some(o) => o,
        None => return Vec::new(),
    };

    match overlay {
        OverlayState::Help => {
            let w = 50.min(viewport.w as usize);
            let h = 20.min(viewport.h as usize);
            let mut surface = TextSurface::new(w, h);
            surface.fill(Style::new().bg(theme::BG));
            surface.draw_box(
                0,
                0,
                w - 1,
                h - 1,
                Border::Rounded,
                theme::style_border_active(),
            );

            let help_lines = [
                "Rye OS TUI — Keybindings",
                "",
                "  Enter        Submit prompt / command",
                "  Tab          Focus next tile",
                "  Shift+Tab    Focus previous tile",
                "  Ctrl+C       Quit (when input empty)",
                "  Ctrl+N       New session",
                "  Ctrl+P       Command palette",
                "  ?            This help",
                "",
                "  Input editing:",
                "  Left/Right   Move cursor",
                "  Up/Down      History navigation",
                "  Home/End     Jump to start/end",
                "  Backspace    Delete character",
                "",
                "  Press any key to close",
            ];

            for (i, line) in help_lines.iter().enumerate() {
                if i + 1 < h - 1 {
                    let style = if i == 0 {
                        Style::new().fg(theme::ACCENT).bg(theme::BG).bold()
                    } else {
                        Style::new().fg(theme::FG).bg(theme::BG)
                    };
                    surface.draw_text(2, i + 1, line, style);
                }
            }

            let x = (viewport.w as usize).saturating_sub(w) / 2;
            let y = (viewport.h as usize).saturating_sub(h) / 2;
            vec![OverlaySurface {
                rect: Rect::new(x as u16, y as u16, w as u16, h as u16),
                cells: surface,
                overlay_type: OverlayType::Help,
            }]
        }
        OverlayState::CommandPalette { query, .. } => {
            let w = 60.min(viewport.w as usize);
            let h = 12.min(viewport.h as usize);
            let mut surface = TextSurface::new(w, h);
            surface.fill(Style::new().bg(theme::BG));
            surface.draw_box(
                0,
                0,
                w - 1,
                h - 1,
                Border::Rounded,
                theme::style_border_active(),
            );

            surface.draw_text(
                2,
                1,
                "Command Palette",
                Style::new().fg(theme::ACCENT).bg(theme::BG).bold(),
            );

            // Query line
            let query_display = format!("> {}", query);
            surface.draw_text(
                2,
                3,
                &query_display,
                Style::new().fg(theme::FG).bg(theme::BG),
            );

            // Placeholder commands
            let commands = ["execute", "refresh", "kill-thread", "quit"];
            for (i, cmd) in commands.iter().enumerate() {
                if 5 + i < h - 1 {
                    surface.draw_text(4, 5 + i, cmd, Style::new().fg(theme::FG_DIM).bg(theme::BG));
                }
            }

            let x = (viewport.w as usize).saturating_sub(w) / 2;
            let y = (viewport.h as usize).saturating_sub(h) / 2;
            vec![OverlaySurface {
                rect: Rect::new(x as u16, y as u16, w as u16, h as u16),
                cells: surface,
                overlay_type: OverlayType::CommandPalette,
            }]
        }
        OverlayState::Confirm { message, .. } => {
            let w = 50.min(viewport.w as usize);
            let h = 7.min(viewport.h as usize);
            let mut surface = TextSurface::new(w, h);
            surface.fill(Style::new().bg(theme::BG));
            surface.draw_box(
                0,
                0,
                w - 1,
                h - 1,
                Border::Rounded,
                theme::style_border_active(),
            );

            surface.draw_text(2, 2, message, Style::new().fg(theme::FG).bg(theme::BG));
            surface.draw_text(
                2,
                4,
                "[y] Yes  [n/Esc] No",
                Style::new().fg(theme::FG_DIM).bg(theme::BG),
            );

            let x = (viewport.w as usize).saturating_sub(w) / 2;
            let y = (viewport.h as usize).saturating_sub(h) / 2;
            vec![OverlaySurface {
                rect: Rect::new(x as u16, y as u16, w as u16, h as u16),
                cells: surface,
                overlay_type: OverlayType::Confirm,
            }]
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn empty_surface(rect: Rect) -> TextSurface {
    let mut s = TextSurface::new(rect.w.max(1) as usize, rect.h.max(1) as usize);
    s.fill(Style::new().bg(theme::BG));
    s
}

fn placeholder(name: &str, w: usize, h: usize) -> TextSurface {
    let mut s = TextSurface::new(w, h);
    s.fill(Style::new().bg(theme::BG));
    let msg = format!("{} (coming soon)", name);
    let style = Style::new().fg(theme::FG_MUTED).bg(theme::BG);
    if h > 0 {
        let x = w.saturating_sub(msg.len()) / 2;
        let y = h / 2;
        s.draw_text(x, y, &msg, style);
    }
    s
}

fn projects_fallback(model: &AppModel, w: usize, h: usize) -> TextSurface {
    let mut s = TextSurface::new(w, h);
    s.fill(Style::new().bg(theme::BG));

    let header = Style::new().fg(theme::FG).bg(theme::BG).bold();
    let row_style = Style::new().fg(theme::FG_DIM).bg(theme::BG);

    if h > 0 {
        s.draw_text(0, 0, "Projects", header);
    }

    for (i, (_, project)) in model.store.projects.iter().enumerate() {
        if 2 + i >= h {
            break;
        }
        let line = format!(
            "  {} — {} items",
            project.path,
            project.item_counts.directives
                + project.item_counts.tools
                + project.item_counts.knowledge
        );
        s.draw_text(0, 2 + i, &line, row_style);
    }

    s
}

/// Blit a source surface onto a destination at (dst_x, dst_y).
fn blit_surface(dst: &mut TextSurface, dst_x: usize, dst_y: usize, src: &TextSurface) {
    for y in 0..src.height {
        for x in 0..src.width {
            let cell = src.get(x, y);
            if cell.rune != ' ' || !cell.bg.is_default() {
                dst.set(dst_x + x, dst_y + y, cell);
            }
        }
    }
}
