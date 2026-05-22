//! Text renderer — diff-based terminal cell painting via crossterm.
//!
//! Converts core TextSurface cells to ANSI output using crossterm commands.

use crossterm::{
    cursor::MoveTo,
    queue,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::Clear,
};
use ryeos_tui_core::text_surface::{Cell, Color as CoreColor, TextSurface};
use std::io::Write;

/// Render a complete text surface to the terminal using diff painting.
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
        for y in 0..new.height {
            queue!(
                stdout,
                MoveTo(offset_x, offset_y + y as u16),
                ResetColor,
                SetAttribute(Attribute::Reset),
            )?;
            for x in 0..new.width {
                let cell = new.get(x, y);
                paint_cell_inline(stdout, cell)?;
            }
        }
    } else {
        let prev_surf = prev.as_ref().unwrap();
        for y in 0..new.height {
            // Quick row check: hash first and last cells as heuristic
            let first_new = new.get(0, y);
            let last_new = new.get(new.width.saturating_sub(1), y);
            let first_old = prev_surf.get(0, y);
            let last_old = prev_surf.get(prev_surf.width.saturating_sub(1), y);

            // Always scan full row for correctness
            let mut row_changed = false;
            for x in 0..new.width {
                if new.get(x, y) != prev_surf.get(x, y) {
                    row_changed = true;
                    break;
                }
            }

            if row_changed {
                for x in 0..new.width {
                    let new_cell = new.get(x, y);
                    let old_cell = prev_surf.get(x, y);
                    if new_cell != old_cell {
                        queue!(
                            stdout,
                            MoveTo(offset_x + x as u16, offset_y + y as u16),
                            ResetColor,
                            SetAttribute(Attribute::Reset),
                        )?;
                        paint_cell_inline(stdout, new_cell)?;
                    }
                }
            }
        }
    }

    *prev = Some(new.clone());
    stdout.flush()?;
    Ok(())
}

fn paint_cell_inline(stdout: &mut impl Write, cell: Cell) -> std::io::Result<()> {
    if cell.fg != CoreColor::Default {
        queue!(stdout, SetForegroundColor(to_crossterm_color(cell.fg)))?;
    }
    if cell.bg != CoreColor::Default {
        queue!(stdout, SetBackgroundColor(to_crossterm_color(cell.bg)))?;
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
