//! The input tile — deliberately minimal. A bordered box on the page
//! background with the buffer and a block cursor; the project path sits
//! on the bottom border as quiet context. No route strip, no `$`, no
//! hint line, no title — the border and cursor are the whole signal.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::studio::view_model::StudioInputVm;
use ryeos_client_base::text_surface::{Border, Style, TextSurface};

use super::primitives::fill_rect;
use super::text::{display_width, input_cursor_byte, truncate};
use super::theme::{BG, FG, MUTED};

pub fn draw_input_tile(
    surface: &mut TextSurface,
    rect: Rect,
    input: &StudioInputVm,
    project_path: Option<&str>,
    border: Option<Border>,
) {
    let w = rect.w as usize;
    let h = rect.h as usize;
    if w < 2 || h < 2 {
        return;
    }
    let x = rect.x as usize;
    let y = rect.y as usize;

    // The box sits flush on the page background — no PANEL fill.
    fill_rect(surface, rect, Style::new().fg(FG).bg(BG));
    if let Some(border) = border {
        surface.draw_box(
            x,
            y,
            x + w - 1,
            y + h - 1,
            border,
            Style::new().fg(FG).bg(BG),
        );
    }

    // Quiet context: the project path on the bottom border, right-aligned.
    if let Some(path) = project_path {
        let label = format!(" {} ", shorten_home(path));
        let lw = display_width(&label);
        if lw + 4 < w {
            surface.draw_text(
                x + w.saturating_sub(lw + 2),
                y + h - 1,
                &label,
                Style::new().fg(MUTED).bg(BG),
            );
        }
    }

    // Buffer + block cursor on the first interior row, one cell of padding.
    let inner_x = x + 2;
    let row_y = y + 1;
    let avail = w.saturating_sub(4);
    if avail == 0 {
        return;
    }
    let visible = truncate(&input.text, avail);
    surface.draw_text(inner_x, row_y, &visible, Style::new().fg(FG).bg(BG));

    let cursor_byte = input_cursor_byte(&input.text, input.cursor);
    let cursor_col = display_width(&input.text[..cursor_byte]).min(avail.saturating_sub(1));
    let cursor_char = input.text[cursor_byte..].chars().next().unwrap_or(' ');
    // A true block cursor: invert the cell (the char shows through when
    // there is one; an empty buffer shows a solid block).
    surface.draw_char(
        inner_x + cursor_col,
        row_y,
        cursor_char,
        Style::new().fg(BG).bg(FG),
    );
}

fn shorten_home(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}
