//! Frame — the shared rendering output boundary.
//!
//! Core produces a Frame. Terminal converts to ANSI cells.
//! Web converts to DOM/HTML + Canvas.

use crate::ids::TileId;
use crate::layout::Rect;
use crate::scene::ScenePrimitive;
use crate::text_surface::TextSurface;
use serde::{Deserialize, Serialize};

/// A complete rendering frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub background: Vec<ScenePrimitive>,
    pub tiles: Vec<TileSurface>,
    pub status_bar: StatusBarSurface,
    pub input: InputSurface,
    pub overlays: Vec<OverlaySurface>,
}

/// A tile's rendered text surface with position metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileSurface {
    pub tile_id: TileId,
    pub rect: Rect,
    pub focused: bool,
    pub title: String,
    pub cells: TextSurface,
}

/// The global input bar surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputSurface {
    pub rect: Rect,
    pub cells: TextSurface,
    pub hint: String,
}

/// Status bar surface showing daemon/thread/budget info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusBarSurface {
    pub rect: Rect,
    pub cells: TextSurface,
}

/// An overlay surface (modal, command palette, help).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlaySurface {
    pub rect: Rect,
    pub cells: TextSurface,
    pub overlay_type: OverlayType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OverlayType {
    CommandPalette,
    Confirm,
    Help,
}

// ---------------------------------------------------------------------------
// Frame construction
// ---------------------------------------------------------------------------

use crate::layout::layout_rects;
use crate::model::AppModel;

/// Build a complete frame from the current model state.
/// This is the main entry point for rendering.
pub fn build_frame(model: &AppModel) -> Frame {
    let viewport = model.runtime.viewport;

    // Background primitives (animated substrate)
    let background = model.visual.animation.generate_primitives();

    // Layout rects for tiles
    let _tile_rects = layout_rects(&model.workspace.layout, viewport);

    // Reserve 1 row at bottom for input bar + 1 for status
    let tiles_height = viewport.h.saturating_sub(2);
    let tiles_viewport = Rect::new(viewport.x, viewport.y, viewport.w, tiles_height);

    let tile_rects_adjusted = layout_rects(&model.workspace.layout, tiles_viewport);

    // Build tile surfaces
    let mut tiles = Vec::new();
    for (tile_id, rect) in &tile_rects_adjusted {
        if rect.is_empty() {
            continue;
        }
        let focused = *tile_id == model.workspace.focused_tile;
        let surface = crate::views::build_tile_view(model, *tile_id, *rect, focused);
        tiles.push(TileSurface {
            tile_id: *tile_id,
            rect: *rect,
            focused,
            title: model
                .workspace
                .tiles
                .get(tile_id)
                .map(|t| t.view.title())
                .unwrap_or_default(),
            cells: surface,
        });
    }

    // Input bar (bottom row)
    let input_rect = Rect::new(viewport.x, viewport.y + tiles_height, viewport.w, 1);
    let input_cells = crate::views::build_input_bar(model, input_rect);
    let input_hint = model
        .workspace
        .focused_view()
        .map(|v| v.input_hint())
        .unwrap_or("input")
        .to_string();
    let input = InputSurface {
        rect: input_rect,
        cells: input_cells,
        hint: input_hint,
    };

    // Status bar (second from bottom)
    let status_y = tiles_height.saturating_sub(1);
    let status_rect = Rect::new(viewport.x, status_y, viewport.w, 1);
    let status_cells = build_status_bar(model, viewport.w as usize);
    let status_bar = StatusBarSurface {
        rect: status_rect,
        cells: status_cells,
    };

    // Overlays
    let overlays = crate::views::build_overlays(model, viewport);

    Frame {
        background,
        tiles,
        status_bar,
        input,
        overlays,
    }
}

/// Build status bar surface from model data.
fn build_status_bar(model: &AppModel, width: usize) -> TextSurface {
    use crate::store::DaemonStatus;
    use crate::text_surface::Style;
    use crate::theme;

    let mut surface = TextSurface::new(width, 1);
    surface.fill(Style::new().bg(theme::BG));

    if width == 0 {
        return surface;
    }

    let bg = theme::BG;
    let fg = theme::FG;
    let fg_dim = theme::FG_MUTED;
    let fg_accent = theme::ACCENT;
    let fg_green = theme::GREEN;
    let fg_red = theme::RED;
    let fg_yellow = theme::YELLOW;
    let fg_orange = theme::ORANGE;

    let sep_style = Style::new().fg(theme::FG_DIM).bg(bg);

    let mut x = 1;

    // Daemon status
    let (daemon_icon, daemon_style) = match model.store.daemon.status {
        DaemonStatus::Connected => ('●', Style::new().fg(fg_green).bg(bg)),
        DaemonStatus::Connecting => ('◌', Style::new().fg(fg_yellow).bg(bg)),
        DaemonStatus::Disconnected => ('○', Style::new().fg(fg_red).bg(bg)),
    };
    surface.draw_char(x, 0, daemon_icon, daemon_style);
    x += 2;

    let daemon_url = &model.store.daemon.url;
    if !daemon_url.is_empty() {
        let short_url = if daemon_url.starts_with("http://") {
            &daemon_url[7..]
        } else {
            daemon_url
        };
        let max_url = 20.min(short_url.len());
        let url_display = &short_url[..max_url];
        surface.draw_text(x, 0, url_display, Style::new().fg(fg_dim).bg(bg));
        x += url_display.len() + 1;
    }

    // Separator
    surface.draw_text(x, 0, "│", sep_style);
    x += 2;

    // Thread count
    let total_threads = model.store.threads.len();
    let running = model.store.running_thread_count();
    let thread_text = if running > 0 {
        format!("{} threads ({} running)", total_threads, running)
    } else {
        format!("{} threads", total_threads)
    };
    let thread_style = if running > 0 {
        Style::new().fg(fg_yellow).bg(bg)
    } else {
        Style::new().fg(fg_dim).bg(bg)
    };
    surface.draw_text(x, 0, &thread_text, thread_style);
    x += thread_text.len() + 1;

    // Separator
    surface.draw_text(x, 0, "│", sep_style);
    x += 2;

    // Budget
    let spend = model.store.budget.total_spend_usd;
    let budget_style = if spend > 5.0 {
        Style::new().fg(fg_red).bg(bg)
    } else if spend > 2.0 {
        Style::new().fg(fg_orange).bg(bg)
    } else {
        Style::new().fg(fg).bg(bg)
    };
    let budget_text = format!("${:.2}", spend);
    surface.draw_text(x, 0, &budget_text, budget_style);
    x += budget_text.len() + 1;

    // Token count
    let total_tokens = model.store.budget.total_input_tokens + model.store.budget.total_output_tokens;
    if total_tokens > 0 {
        let tok_text = if total_tokens > 1_000_000 {
            format!("{:.1}M tok", total_tokens as f64 / 1_000_000.0)
        } else {
            format!("{:.1}k tok", total_tokens as f64 / 1000.0)
        };
        surface.draw_text(x, 0, &tok_text, Style::new().fg(fg_dim).bg(bg));
    }

    // Right side: help hint
    let help = "? help";
    surface.draw_text(
        width.saturating_sub(help.len() + 1),
        0,
        help,
        Style::new().fg(fg_accent).bg(bg),
    );

    surface
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::TileId;

    #[test]
    fn build_frame_returns_default_tiles_input_and_background() {
        let model = AppModel::new_default("/tmp/test");
        let frame = build_frame(&model);

        assert!(
            !frame.background.is_empty(),
            "should have substrate primitives"
        );
        assert_eq!(frame.tiles.len(), 3, "should have 3 tiles");
        assert!(
            !frame.input.cells.cells.is_empty(),
            "should have input surface"
        );
        assert!(frame.overlays.is_empty(), "no overlays by default");
    }

    #[test]
    fn build_frame_with_overlay() {
        let mut model = AppModel::new_default("/tmp/test");
        model.overlay = Some(crate::model::OverlayState::Help);
        let frame = build_frame(&model);
        assert_eq!(frame.overlays.len(), 1);
    }
}
