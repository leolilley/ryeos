//! Text renderer — diff-based terminal cell painting via crossterm.
//!
//! Converts core TextSurface cells to ANSI output using crossterm commands.

use crossterm::{
    cursor::MoveTo,
    queue,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{Clear, ClearType},
};
use ryeos_client_base::text_surface::{Cell, Color as CoreColor, TextSurface};
use std::io::Write;

/// Render a complete text surface to the terminal using diff painting.
///
/// Full redraws (first frame, resize) clear the screen first — the
/// emulator reflows the previous frame during a resize and absolute
/// repaints don't cover the displaced junk — and dedupe color codes
/// across runs of same-styled cells, which keeps resize storms cheap
/// enough to repaint per event.
#[allow(dead_code)]
pub fn render_text_surface(
    stdout: &mut impl Write,
    new: &TextSurface,
    prev: &mut Option<TextSurface>,
    offset_x: u16,
    offset_y: u16,
) -> std::io::Result<()> {
    let full_redraw = prev.is_none()
        || prev.as_ref().unwrap().width != new.width
        || prev.as_ref().unwrap().height != new.height;

    if full_redraw {
        queue!(
            stdout,
            ResetColor,
            SetAttribute(Attribute::Reset),
            Clear(ClearType::All),
        )?;
        let mut brush = Brush::default();
        for y in 0..new.height {
            queue!(stdout, MoveTo(offset_x, offset_y + y as u16))?;
            for x in 0..new.width {
                paint_cell(stdout, new.get(x, y), &mut brush)?;
            }
        }
    } else {
        let prev_surf = prev.as_ref().unwrap();
        for y in 0..new.height {
            let mut row_changed = false;
            for x in 0..new.width {
                if new.get(x, y) != prev_surf.get(x, y) {
                    row_changed = true;
                    break;
                }
            }

            if row_changed {
                let mut brush = Brush::default();
                for x in 0..new.width {
                    let new_cell = new.get(x, y);
                    let old_cell = prev_surf.get(x, y);
                    if new_cell != old_cell {
                        queue!(stdout, MoveTo(offset_x + x as u16, offset_y + y as u16))?;
                        paint_cell(stdout, new_cell, &mut brush)?;
                    }
                }
            }
        }
    }

    *prev = Some(new.clone());
    stdout.flush()?;
    Ok(())
}

/// Last emitted fg/bg, so runs of same-styled cells emit color codes
/// once instead of per cell.
#[derive(Default)]
struct Brush {
    fg: Option<CoreColor>,
    bg: Option<CoreColor>,
}

fn paint_cell(stdout: &mut impl Write, cell: Cell, brush: &mut Brush) -> std::io::Result<()> {
    if brush.fg != Some(cell.fg) {
        match cell.fg {
            CoreColor::Default => queue!(stdout, SetForegroundColor(Color::Reset))?,
            fg => queue!(stdout, SetForegroundColor(to_crossterm_color(fg)))?,
        }
        brush.fg = Some(cell.fg);
    }
    if brush.bg != Some(cell.bg) {
        match cell.bg {
            CoreColor::Default => queue!(stdout, SetBackgroundColor(Color::Reset))?,
            bg => queue!(stdout, SetBackgroundColor(to_crossterm_color(bg)))?,
        }
        brush.bg = Some(cell.bg);
    }
    if cell.rune == '\u{0}' {
        // continuation cell for wide char — skip
        return Ok(());
    }
    queue!(stdout, Print(cell.rune.to_string()))?;
    Ok(())
}

/// Convert core Color to crossterm Color.
pub fn to_crossterm_color(color: CoreColor) -> Color {
    match color {
        CoreColor::Default => Color::Reset,
        CoreColor::Index(i) => Color::AnsiValue(i),
        CoreColor::Rgb(r, g, b) => Color::Rgb { r, g, b },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_color_adapter_maps_default_index_rgb() {
        assert!(matches!(
            to_crossterm_color(CoreColor::Default),
            Color::Reset
        ));
        assert!(matches!(
            to_crossterm_color(CoreColor::Index(42)),
            Color::AnsiValue(42)
        ));
        assert!(matches!(
            to_crossterm_color(CoreColor::Rgb(1, 2, 3)),
            Color::Rgb { .. }
        ));
    }
}
