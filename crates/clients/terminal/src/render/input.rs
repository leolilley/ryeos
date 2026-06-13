//! The input dock: route target strip, prompt buffer with a true
//! inverted cursor, and the completion/notice hint line.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::studio::view_model::StudioInputVm;
use ryeos_client_base::text_surface::{Style, TextSurface};

use super::primitives::fill_line;
use super::text::{display_width, input_cursor_byte, truncate};
use super::theme::{style_fg, style_muted, ACCENT, FG, PANEL, PANEL_2};

pub fn draw_input_dock(surface: &mut TextSurface, rect: Rect, input: &StudioInputVm) {
    let width = rect.w as usize;
    let height = rect.h as usize;
    if width == 0 || height == 0 {
        return;
    }
    let route_style = Style::new().fg(FG).bg(PANEL_2).bold();
    fill_line(
        surface,
        rect.x as usize,
        rect.y as usize,
        width,
        route_style,
    );
    surface.draw_text(
        rect.x as usize,
        rect.y as usize,
        &truncate(&input.route_label, width),
        route_style,
    );

    if height < 2 {
        return;
    }
    let prompt_y = rect.y as usize + 1;
    let text = if input.text.is_empty() {
        input.placeholder.as_str()
    } else {
        input.text.as_str()
    };
    surface.draw_text(
        rect.x as usize,
        prompt_y,
        "$ ",
        Style::new().fg(ACCENT).bg(PANEL).bold(),
    );
    let text_x = rect.x as usize + 2;
    let visible = truncate(text, width.saturating_sub(2));
    let text_style = if input.text.is_empty() {
        style_muted()
    } else {
        style_fg()
    };
    surface.draw_text(text_x, prompt_y, &visible, text_style);
    let cursor_byte = input_cursor_byte(&input.text, input.cursor);
    let cursor_x = text_x
        + display_width(&truncate(
            &input.text[..cursor_byte],
            width.saturating_sub(2),
        ))
        .min(width.saturating_sub(3));
    let cursor_char = input.text[cursor_byte..].chars().next().unwrap_or(' ');
    surface.draw_char(
        cursor_x,
        prompt_y,
        cursor_char,
        Style::new().fg(PANEL).bg(ACCENT),
    );

    if height >= 3 {
        surface.draw_text(
            rect.x as usize,
            rect.y as usize + 2,
            &truncate(&input.hint, width),
            style_muted(),
        );
    }
}
