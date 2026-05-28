//! Frame — the shared rendering output boundary.
//!
//! Core produces a Frame. Terminal converts to ANSI cells.
//! Web converts to DOM/HTML + Canvas.
//!
//! A frame contains:
//! - `background`: 3D scene primitives for the animated substrate
//! - `tiles`: text surfaces for each visible workspace tile
//! - `status_bar`: the bottom status bar surface
//! - `input`: the input bar surface (at the very bottom)
//! - `overlays`: modal overlays (help, command palette, confirm)

use crate::ids::TileId;
use crate::layout::{layout_rects, Rect};
use crate::scene::ScenePrimitive;
use crate::text_surface::{Style, TextSurface};
use crate::theme;
#[allow(unused_imports)]
use crate::workspace::InputCapability;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Frame types
// ---------------------------------------------------------------------------

/// A complete rendering frame with all layers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    /// 3D scene primitives for the background.
    pub background: Vec<ScenePrimitive>,
    /// Tile text surfaces keyed by tile ID.
    pub tiles: Vec<TileSurface>,
    /// Bottom status bar.
    pub status_bar: StatusBarSurface,
    /// Input bar (prompt/filter) at the very bottom.
    pub input: InputSurface,
    /// Modal overlays drawn on top of everything.
    pub overlays: Vec<OverlaySurface>,
}

/// A tile's rendered text surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileSurface {
    pub tile_id: TileId,
    pub rect: Rect,
    pub cells: TextSurface,
}

/// Status bar surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusBarSurface {
    pub rect: Rect,
    pub cells: TextSurface,
}

/// Input bar surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputSurface {
    pub rect: Rect,
    pub cells: TextSurface,
}

/// An overlay surface (help, command palette, confirm).
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
    SplashText,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Height of the status bar in terminal rows.
const STATUS_BAR_HEIGHT: u16 = 1;

/// Height of the input bar in terminal rows.
const INPUT_BAR_HEIGHT: u16 = 1;

// ---------------------------------------------------------------------------
// Frame construction
// ---------------------------------------------------------------------------

use crate::model::AppModel;

/// Build a complete frame from the current model state.
///
/// The frame includes all layers: background scene, workspace tiles,
/// status bar, input bar, and any active overlays.
pub fn build_frame(model: &mut AppModel) -> Frame {
    let viewport = model.runtime.viewport;

    // 1. Generate 3D scene primitives
    let background = model.visual.animation.generate_primitives();

    // 2. Compute layout rects for workspace tiles
    //    Reserve bottom rows for status bar + input bar
    let workspace_viewport = Rect::new(
        viewport.x,
        viewport.y,
        viewport.w,
        viewport
            .h
            .saturating_sub(STATUS_BAR_HEIGHT + INPUT_BAR_HEIGHT),
    );
    let tile_rects = layout_rects(&model.workspace.layout, workspace_viewport);

    // 3. Build tile surfaces
    let tiles: Vec<TileSurface> = tile_rects
        .iter()
        .map(|(&tile_id, &rect)| {
            let focused = tile_id == model.workspace.focused_tile;
            let cells = crate::views::build_tile_view(model, tile_id, rect, focused);
            TileSurface {
                tile_id,
                rect,
                cells,
            }
        })
        .collect();

    // 4. Build status bar
    let status_bar_y = viewport
        .h
        .saturating_sub(STATUS_BAR_HEIGHT + INPUT_BAR_HEIGHT);
    let status_bar_rect = Rect::new(0, status_bar_y, viewport.w, STATUS_BAR_HEIGHT);
    let status_bar = build_status_bar(model, status_bar_rect);

    // 5. Build input bar
    let input_bar_y = viewport.h.saturating_sub(INPUT_BAR_HEIGHT);
    let input_bar_rect = Rect::new(0, input_bar_y, viewport.w, INPUT_BAR_HEIGHT);
    let input = InputSurface {
        rect: input_bar_rect,
        cells: crate::views::build_input_bar(model, input_bar_rect),
    };

    // 6. Build overlays
    let overlays = crate::views::build_overlays(model, viewport);

    Frame {
        background,
        tiles,
        status_bar,
        input,
        overlays,
    }
}

// ---------------------------------------------------------------------------
// Status bar builder
// ---------------------------------------------------------------------------

fn build_status_bar(model: &AppModel, rect: Rect) -> StatusBarSurface {
    let mut surface = TextSurface::new(rect.w as usize, rect.h as usize);
    surface.fill(theme::style_status_bar());

    let w = rect.w as usize;

    // Left section: daemon status
    let status_text = match &model.store.daemon.status {
        crate::store::DaemonStatus::Connected => "● ryeos",
        crate::store::DaemonStatus::Connecting => "◌ connecting…",
        crate::store::DaemonStatus::Disconnected => "○ disconnected",
    };
    let status_color = match &model.store.daemon.status {
        crate::store::DaemonStatus::Connected => theme::STATUS_OK,
        crate::store::DaemonStatus::Connecting => theme::STATUS_BUSY,
        crate::store::DaemonStatus::Disconnected => theme::STATUS_ERR,
    };
    surface.draw_text(
        1,
        0,
        status_text,
        Style::new().fg(status_color).bg(theme::BG_DARK),
    );

    // Middle section: thread count
    let running = model.store.running_thread_count();
    let total = model.store.threads.len();
    let thread_text = format!("threads:{}/{}", running, total);
    let mid_x = w.saturating_sub(thread_text.len()) / 2;
    surface.draw_text(
        mid_x,
        0,
        &thread_text,
        Style::new().fg(theme::FG_DIM).bg(theme::BG_DARK),
    );

    // Right section: surface source + identity / trust status
    let mut right_parts: Vec<String> = Vec::new();
    if let Some(ref_str) = &model.surface.requested_ref {
        right_parts.push(ref_str.clone());
    }
    right_parts.push(model.surface.source_label.to_string());
    if let Some(id) = &model.store.identity {
        if id.has_signing_key {
            right_parts.push(format!(
                "🔑 {}",
                &id.fingerprint[..8.min(id.fingerprint.len())]
            ));
        } else {
            right_parts.push("⚠ no key".into());
        }
    }
    let right_text = right_parts.join(" ");
    let right_x = w.saturating_sub(right_text.len() + 1);
    let right_style = if model.surface.is_local_preview {
        Style::new().fg(theme::YELLOW).bg(theme::BG_DARK)
    } else if model.surface.is_trusted {
        Style::new().fg(theme::GREEN).bg(theme::BG_DARK)
    } else {
        Style::new().fg(theme::FG_DIM).bg(theme::BG_DARK)
    };
    surface.draw_text(right_x, 0, &right_text, right_style);

    StatusBarSurface {
        rect,
        cells: surface,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_frame_returns_primitives() {
        let mut model = AppModel::new_default("/tmp/test");
        // Tick to initialize the 3D scene
        model.visual.animation.tick(16, &crate::store::Store::new());
        model.runtime.viewport = Rect::new(0, 0, 200, 60);
        let frame = build_frame(&mut model);

        assert!(!frame.background.is_empty(), "should have scene primitives");
    }

    #[test]
    fn build_frame_has_tiles() {
        let mut model = AppModel::new_default("/tmp/test");
        model.visual.animation.tick(16, &crate::store::Store::new());
        model.runtime.viewport = Rect::new(0, 0, 200, 60);
        let frame = build_frame(&mut model);

        assert_eq!(
            frame.tiles.len(),
            3,
            "default workspace should have 3 tiles"
        );
    }

    #[test]
    fn build_frame_has_status_bar() {
        let mut model = AppModel::new_default("/tmp/test");
        model.visual.animation.tick(16, &crate::store::Store::new());
        model.runtime.viewport = Rect::new(0, 0, 200, 60);
        let frame = build_frame(&mut model);

        assert_eq!(frame.status_bar.rect.h, 1);
        assert_eq!(frame.status_bar.rect.w, 200);
    }

    #[test]
    fn build_frame_has_input_bar() {
        let mut model = AppModel::new_default("/tmp/test");
        model.visual.animation.tick(16, &crate::store::Store::new());
        model.runtime.viewport = Rect::new(0, 0, 200, 60);
        let frame = build_frame(&mut model);

        assert_eq!(frame.input.rect.h, 1);
        assert!(frame.input.rect.y > 0, "input should be below tiles");
    }

    #[test]
    fn build_frame_overlays_default_empty() {
        let mut model = AppModel::new_default("/tmp/test");
        model.visual.animation.tick(16, &crate::store::Store::new());
        model.runtime.viewport = Rect::new(0, 0, 200, 60);
        let frame = build_frame(&mut model);

        assert!(frame.overlays.is_empty(), "no overlays by default");
    }

    #[test]
    fn build_frame_with_help_overlay() {
        let mut model = AppModel::new_default("/tmp/test");
        model.visual.animation.tick(16, &crate::store::Store::new());
        model.runtime.viewport = Rect::new(0, 0, 200, 60);
        model.overlay = Some(crate::model::OverlayState::Help);
        let frame = build_frame(&mut model);

        assert_eq!(frame.overlays.len(), 1);
        assert_eq!(frame.overlays[0].overlay_type, OverlayType::Help);
    }
}
