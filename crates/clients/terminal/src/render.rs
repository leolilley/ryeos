//! Frame renderer — diff-based compositing of text surfaces.
//!
//! Keeps a previous-frame buffer and only writes cells that changed.
//! Eliminates flicker from full-screen clears. Same approach as ratatui's
//! TerminalBackend.

use crate::render_text;
use crossterm::{
    cursor::MoveTo,
    queue,
    style::{Attribute, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor},
};
use ryeos_client_base::text_surface::{Attr, Cell, Color as CoreColor};
use std::io::Write;

/// A flattened cell grid for diffing against the previous frame.
#[derive(Clone)]
struct ScreenBuffer {
    width: usize,
    height: usize,
    cells: Vec<Cell>,
}

impl ScreenBuffer {
    fn new(width: usize, height: usize) -> Self {
        let space = Cell {
            rune: ' ',
            width: 1,
            fg: CoreColor::Default,
            bg: CoreColor::Default,
            attr: Attr::empty(),
        };
        Self {
            width,
            height,
            cells: vec![space; width * height],
        }
    }

    fn get(&self, x: usize, y: usize) -> &Cell {
        &self.cells[y * self.width + x]
    }

    fn set(&mut self, x: usize, y: usize, cell: Cell) {
        if x < self.width && y < self.height {
            self.cells[y * self.width + x] = cell;
        }
    }
}

pub struct FrameRenderer {
    /// Previous frame's cell buffer for diff comparison.
    prev: Option<ScreenBuffer>,
}

impl FrameRenderer {
    pub fn new() -> Self {
        Self { prev: None }
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

        // Build frame from model
        let frame = ryeos_client_base::frame::build_frame(model);

        // Compose current frame into a flat buffer
        let mut current = ScreenBuffer::new(tw, th);

        // Set default background for all cells
        let bg_cell = Cell {
            rune: ' ',
            width: 1,
            fg: CoreColor::Default,
            bg: CoreColor::Rgb(30, 30, 30),
            attr: Attr::empty(),
        };
        for cell in current.cells.iter_mut() {
            *cell = bg_cell;
        }

        // Blit tile surfaces
        for tile in &frame.tiles {
            blit_surface(&mut current, &tile.cells, tile.rect.x, tile.rect.y);
        }

        // Blit status bar
        blit_surface(
            &mut current,
            &frame.status_bar.cells,
            frame.status_bar.rect.x,
            frame.status_bar.rect.y,
        );

        // Blit input bar
        blit_surface(
            &mut current,
            &frame.input.cells,
            frame.input.rect.x,
            frame.input.rect.y,
        );

        // Blit overlays (on top of everything)
        for overlay in &frame.overlays {
            blit_overlay_backdrop(&mut current, &overlay.rect);
            blit_surface(&mut current, &overlay.cells, overlay.rect.x, overlay.rect.y);
        }

        // Diff against previous frame and write only changed cells
        if let Some(ref prev) = self.prev {
            if prev.width == tw && prev.height == th {
                diff_and_write(stdout, prev, &current)?;
            } else {
                // Viewport resized — full repaint
                queue!(
                    stdout,
                    crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
                )?;
                diff_and_write(stdout, &ScreenBuffer::new(tw, th), &current)?;
            }
        } else {
            // First frame — full paint
            diff_and_write(stdout, &ScreenBuffer::new(tw, th), &current)?;
        }

        // Position cursor at input bar
        let input_y = frame.input.rect.y;
        let input_x = frame.input.rect.x;
        queue!(stdout, MoveTo(input_x, input_y), ResetColor)?;
        stdout.flush()?;

        self.prev = Some(current);
        Ok(())
    }
}

/// Blit a TextSurface into the screen buffer at the given offset.
fn blit_surface(
    buf: &mut ScreenBuffer,
    surface: &ryeos_client_base::text_surface::TextSurface,
    offset_x: u16,
    offset_y: u16,
) {
    for y in 0..surface.height {
        for x in 0..surface.width {
            let cell = surface.get(x, y);
            // Skip null continuation cells from wide chars
            if cell.rune == '\u{0}' {
                continue;
            }
            buf.set(offset_x as usize + x, offset_y as usize + y, cell);
        }
    }
}

/// Paint a dark backdrop for overlay areas.
fn blit_overlay_backdrop(buf: &mut ScreenBuffer, rect: &ryeos_client_base::layout::Rect) {
    let backdrop = Cell {
        rune: ' ',
        width: 1,
        fg: CoreColor::Rgb(0x50, 0x49, 0x45),
        bg: CoreColor::Rgb(0x1d, 0x20, 0x21),
        attr: Attr::empty(),
    };
    for y in rect.y..rect.y + rect.h {
        for x in rect.x..rect.x + rect.w {
            buf.set(x as usize, y as usize, backdrop);
        }
    }
}

/// Compare two screen buffers and write only changed cells.
///
/// Optimizations borrowed from ratatui's crossterm backend:
/// - Skip MoveTo when the next changed cell is adjacent to the previous one
/// - Only emit color/attribute escape sequences when they actually change
fn diff_and_write(
    stdout: &mut impl Write,
    prev: &ScreenBuffer,
    current: &ScreenBuffer,
) -> std::io::Result<()> {
    let mut last_x: Option<u16> = None;
    let mut last_y: Option<u16> = None;
    let mut last_fg: Option<CoreColor> = None;
    let mut last_bg: Option<CoreColor> = None;
    let mut last_attr: Option<Attr> = None;

    for y in 0..current.height {
        for x in 0..current.width {
            let cur = current.get(x, y);
            let prv = prev.get(x, y);

            if cur == prv {
                continue;
            }

            // Only move cursor if not adjacent to previous write
            let need_move = !matches!(
                (last_x, last_y),
                (Some(lx), Some(ly)) if lx + 1 == x as u16 && ly == y as u16
            );
            if need_move {
                queue!(stdout, MoveTo(x as u16, y as u16))?;
            }

            // Update fg if changed
            if last_fg != Some(cur.fg) {
                if cur.fg == CoreColor::Default {
                    queue!(stdout, SetForegroundColor(crossterm::style::Color::Reset))?;
                } else {
                    queue!(
                        stdout,
                        SetForegroundColor(render_text::to_crossterm_color(cur.fg))
                    )?;
                }
                last_fg = Some(cur.fg);
            }

            // Update bg if changed
            if last_bg != Some(cur.bg) {
                if cur.bg == CoreColor::Default {
                    queue!(stdout, SetBackgroundColor(crossterm::style::Color::Reset))?;
                } else {
                    queue!(
                        stdout,
                        SetBackgroundColor(render_text::to_crossterm_color(cur.bg))
                    )?;
                }
                last_bg = Some(cur.bg);
            }

            // Update attributes if changed
            let cur_attr = cur.attr;
            if last_attr != Some(cur_attr) {
                queue!(stdout, SetAttribute(Attribute::Reset))?;
                if cur_attr.contains(Attr::BOLD) {
                    queue!(stdout, SetAttribute(Attribute::Bold))?;
                }
                if cur_attr.contains(Attr::ITALIC) {
                    queue!(stdout, SetAttribute(Attribute::Italic))?;
                }
                if cur_attr.contains(Attr::UNDERLINE) {
                    queue!(stdout, SetAttribute(Attribute::Underlined))?;
                }
                if cur_attr.contains(Attr::REVERSE) {
                    queue!(stdout, SetAttribute(Attribute::Reverse))?;
                }
                last_attr = Some(cur_attr);
            }

            queue!(stdout, Print(cur.rune))?;
            last_x = Some(x as u16);
            last_y = Some(y as u16);
        }
    }

    Ok(())
}
