//! Frame renderer — composites text surfaces for the terminal.
//!
//! Pipeline:
//! 1. Clear screen with background color
//! 2. Paint TextSurface layers (tiles, status bar, input bar) using crossterm
//! 3. Paint overlay surfaces last (highest z-order)
//!
//! Background animation (braille scene rasterizer) is disabled during
//! early development. Re-enable by restoring Layer 1 from git history.

use crate::render_text;
use crossterm::{
    cursor::MoveTo,
    queue,
    style::{Attribute, Print, ResetColor, SetAttribute, SetBackgroundColor},
};
use ryeos_client_base::frame::OverlayType;
use ryeos_client_base::text_surface::{Attr, Color as CoreColor};
use std::io::Write;

pub struct FrameRenderer;

impl FrameRenderer {
    pub fn new() -> Self {
        Self
    }

    pub fn render(
        &mut self,
        stdout: &mut impl Write,
        model: &mut ryeos_client_base::model::AppModel,
        viewport_w: u16,
        viewport_h: u16,
    ) -> std::io::Result<()> {
        let tw = viewport_w as usize;
        let th = viewport_h as usize;
        if tw == 0 || th == 0 {
            return Ok(());
        }

        // Build frame
        let frame = ryeos_client_base::frame::build_frame(model);

        // Layer 1: Clear screen with background color
        queue!(
            stdout,
            MoveTo(0, 0),
            SetBackgroundColor(crossterm::style::Color::Rgb { r: 30, g: 30, b: 30 })
        )?;
        // Fill each row to ensure full coverage
        for y in 0..viewport_h {
            queue!(stdout, MoveTo(0, y), Print(" ".repeat(tw)))?;
        }

        // Layer 2: Paint tile text surfaces
        for tile in &frame.tiles {
            paint_text_surface(stdout, &tile.cells, tile.rect.x, tile.rect.y)?;
        }

        // Layer 4: Paint status bar
        paint_text_surface(
            stdout,
            &frame.status_bar.cells,
            frame.status_bar.rect.x,
            frame.status_bar.rect.y,
        )?;

        // Layer 5: Paint input bar
        paint_text_surface(
            stdout,
            &frame.input.cells,
            frame.input.rect.x,
            frame.input.rect.y,
        )?;

        // Layer 3: Paint overlays (highest z-order)
        for overlay in &frame.overlays {
            // Draw a semi-transparent backdrop behind the overlay
            paint_overlay_backdrop(stdout, &overlay.rect, overlay.overlay_type.clone())?;
            paint_text_surface(stdout, &overlay.cells, overlay.rect.x, overlay.rect.y)?;
        }

        // Position cursor at input bar
        let input_y = frame.input.rect.y;
        queue!(stdout, MoveTo(0, input_y), ResetColor)?;
        stdout.flush()
    }
}

/// Paint a TextSurface onto the terminal at the given offset.
fn paint_text_surface(
    stdout: &mut impl Write,
    surface: &ryeos_client_base::text_surface::TextSurface,
    offset_x: u16,
    offset_y: u16,
) -> std::io::Result<()> {
    use crossterm::style::{SetBackgroundColor, SetForegroundColor};

    for y in 0..surface.height {
        queue!(
            stdout,
            MoveTo(offset_x, offset_y + y as u16),
            ResetColor,
            SetAttribute(Attribute::Reset)
        )?;

        for x in 0..surface.width {
            let cell = surface.get(x, y);

            // Skip null continuation cells from wide chars
            if cell.rune == '\u{0}' {
                continue;
            }

            // Skip completely transparent cells (space with default bg)
            if cell.rune == ' ' && cell.bg == CoreColor::Default && cell.fg == CoreColor::Default {
                queue!(stdout, Print(' '))?;
                continue;
            }

            // Set colors
            if cell.fg != CoreColor::Default {
                queue!(
                    stdout,
                    SetForegroundColor(render_text::to_crossterm_color(cell.fg))
                )?;
            } else {
                queue!(stdout, ResetColor)?;
            }
            if cell.bg != CoreColor::Default {
                queue!(
                    stdout,
                    SetBackgroundColor(render_text::to_crossterm_color(cell.bg))
                )?;
            }

            // Set attributes
            if cell.attr.contains(Attr::BOLD) {
                queue!(stdout, SetAttribute(Attribute::Bold))?;
            }
            if cell.attr.contains(Attr::ITALIC) {
                queue!(stdout, SetAttribute(Attribute::Italic))?;
            }
            if cell.attr.contains(Attr::UNDERLINE) {
                queue!(stdout, SetAttribute(Attribute::Underlined))?;
            }
            if cell.attr.contains(Attr::REVERSE) {
                queue!(stdout, SetAttribute(Attribute::Reverse))?;
            }

            queue!(stdout, Print(cell.rune))?;
        }
    }

    Ok(())
}

/// Paint a semi-transparent backdrop behind overlay areas.
fn paint_overlay_backdrop(
    stdout: &mut impl Write,
    rect: &ryeos_client_base::layout::Rect,
    _overlay_type: OverlayType,
) -> std::io::Result<()> {
    use crossterm::style::{SetBackgroundColor, SetForegroundColor};

    // Dim the area behind the overlay with a dark background
    let bg = crossterm::style::Color::Rgb {
        r: 0x1d,
        g: 0x20,
        b: 0x21,
    };
    queue!(
        stdout,
        SetBackgroundColor(bg),
        SetForegroundColor(crossterm::style::Color::Rgb {
            r: 0x50,
            g: 0x49,
            b: 0x45
        })
    )?;

    for y in rect.y..rect.y + rect.h {
        queue!(stdout, MoveTo(rect.x, y))?;
        for _ in 0..rect.w {
            queue!(stdout, Print(' '))?;
        }
    }

    Ok(())
}
