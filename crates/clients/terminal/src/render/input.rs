//! The input tile — deliberately minimal. A bordered box on the page
//! background with the buffer and a block cursor. The bottom border
//! carries quiet context: the submit target on the left (what you're
//! aiming at — the seat route, e.g. "→ … (new chain)" / "→ chained on
//! T-…") and the project path on the right. No `$`, no hint line, no
//! title — the border, target, and cursor are the whole signal.

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

    // Quiet context on the bottom border: the project path, right-aligned.
    let bottom_y = y + h - 1;
    let cwd_w = if let Some(path) = project_path {
        let label = format!(" {} ", shorten_home(path));
        let lw = display_width(&label);
        if lw + 4 < w {
            surface.draw_text(
                x + w.saturating_sub(lw + 2),
                bottom_y,
                &label,
                Style::new().fg(MUTED).bg(BG),
            );
            lw
        } else {
            0
        }
    } else {
        0
    };

    // The submit target on the bottom border, left side: what this input is
    // aiming at (the seat route, already formatted as "→ …"). Truncated to
    // fit before the cwd label so they never collide.
    if !input.route_label.is_empty() {
        let budget = w.saturating_sub(cwd_w + 6);
        if budget >= 6 {
            let label = truncate(&format!(" {} ", input.route_label), budget);
            surface.draw_text(x + 2, bottom_y, &label, Style::new().fg(MUTED).bg(BG));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn input_vm(route_label: &str) -> StudioInputVm {
        StudioInputVm {
            cursor: 0,
            route_label: route_label.to_string(),
            placeholder: String::new(),
            hint: String::new(),
            submit_enabled: false,
            completion: Vec::new(),
            text: String::new(),
        }
    }

    fn row_text(surface: &TextSurface, w: usize, y: usize) -> String {
        (0..w).map(|x| surface.get(x, y).rune).collect()
    }

    #[test]
    fn input_border_shows_the_route_target() {
        let mut surface = TextSurface::new(60, 7);
        let rect = Rect {
            x: 0,
            y: 0,
            w: 60,
            h: 7,
        };
        draw_input_tile(
            &mut surface,
            rect,
            &input_vm("→ service:foo/bar (new chain)"),
            Some("/tmp/project"),
            Some(Border::Sharp),
        );
        let bottom = row_text(&surface, 60, 6);
        assert!(bottom.contains('→'), "target glyph missing: {bottom:?}");
        assert!(
            bottom.contains("service:foo/bar"),
            "route target missing from border: {bottom:?}"
        );
    }
}
