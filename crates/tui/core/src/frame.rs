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

    // Status bar (second from bottom) — rendered as part of the input region for now
    // The terminal renderer can composite these separately

    // Overlays
    let overlays = crate::views::build_overlays(model, viewport);

    Frame {
        background,
        tiles,
        input,
        overlays,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::TileId;

    #[test]
    fn build_frame_returns_default_tiles_input_and_background() {
        let model = AppModel::new_default("/tmp/test");
        let frame = build_frame(&model);

        assert!(!frame.background.is_empty(), "should have substrate primitives");
        assert_eq!(frame.tiles.len(), 3, "should have 3 tiles");
        assert!(!frame.input.cells.cells.is_empty(), "should have input surface");
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
