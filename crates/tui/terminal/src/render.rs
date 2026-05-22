//! Frame renderer — composites background scene + tile text surfaces
//! into a single terminal frame.

use ryeos_tui_core::frame::Frame;
use ryeos_tui_core::text_surface::{Color, Style, TextSurface};

use crate::render_scene;
use crate::render_text;

/// Holds state for the terminal renderer (previous frame for diffing).
pub struct FrameRenderer {
    prev_surface: Option<TextSurface>,
}

impl FrameRenderer {
    pub fn new() -> Self {
        Self { prev_surface: None }
    }

    /// Render a complete frame to stdout.
    pub fn render(
        &mut self,
        stdout: &mut impl std::io::Write,
        frame: &Frame,
        viewport_w: u16,
        viewport_h: u16,
    ) -> std::io::Result<()> {
        let w = viewport_w as usize;
        let h = viewport_h as usize;
        if w == 0 || h == 0 {
            return Ok(());
        }

        // Build composite surface
        let mut composite = TextSurface::new(w, h);
        composite.fill(Style::new().bg(Color::Rgb(0x1d, 0x20, 0x21))); // BG_DARK

        // Render scene primitives into background
        render_scene::render_scene(&frame.background, &mut composite);

        // Blit tile surfaces
        for tile in &frame.tiles {
            blit_tile(&mut composite, tile);
        }

        // Blit input bar
        blit_surface_at(
            &mut composite,
            &frame.input.cells,
            frame.input.rect.x as usize,
            frame.input.rect.y as usize,
        );

        // Status bar (second from bottom)
        blit_surface_at(
            &mut composite,
            &frame.status_bar.cells,
            frame.status_bar.rect.x as usize,
            frame.status_bar.rect.y as usize,
        );

        // Blit overlays
        for overlay in &frame.overlays {
            // Dim the background first (simple: just blit the overlay on top)
            blit_surface_at(
                &mut composite,
                &overlay.cells,
                overlay.rect.x as usize,
                overlay.rect.y as usize,
            );
        }

        // Diff-paint to terminal
        render_text::render_text_surface(stdout, &composite, &mut self.prev_surface, 0, 0)?;

        Ok(())
    }
}

fn blit_tile(dst: &mut TextSurface, tile: &ryeos_tui_core::frame::TileSurface) {
    blit_surface_at(dst, &tile.cells, tile.rect.x as usize, tile.rect.y as usize);
}

fn blit_surface_at(dst: &mut TextSurface, src: &TextSurface, offset_x: usize, offset_y: usize) {
    for y in 0..src.height {
        for x in 0..src.width {
            let cell = src.get(x, y);
            // Only overwrite if the cell has content (non-space or non-default bg)
            if cell.rune != ' ' || !cell.bg.is_default() {
                dst.set(offset_x + x, offset_y + y, cell);
            }
        }
    }
}


