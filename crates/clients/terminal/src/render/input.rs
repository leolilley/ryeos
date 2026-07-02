//! The input tile — deliberately minimal. A bordered box on the page
//! background with the buffer and a block cursor. The bottom border
//! carries quiet context: the submit target on the left (what you're
//! aiming at — the seat route, e.g. "→ … (new chain)" / "→ chained on
//! T-…") and the project path on the right. No `$`, no title — the
//! border, target, and cursor are the whole signal in normal use.
//!
//! The one contextual exception is command completion: while the buffer
//! is in command mode (`completion` non-empty) a single muted hint line
//! sits under the buffer, showing the candidate tokens or — once a
//! command matches — its argument grammar with the *current* argument
//! accented (`[name]`). It appears only while completing, so the box
//! stays clean the rest of the time.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::studio::view_model::StudioInputVm;
use ryeos_client_base::text_surface::{Border, Style, TextSurface};

use super::primitives::fill_rect;
use super::text::{display_width, input_cursor_byte, truncate};
use super::theme::{ACCENT, BG, FG, MUTED};

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

    // Contextual completion hint: only while in command mode, and only if
    // there is room for a line between the buffer and the bottom border.
    let hint_y = row_y + 1;
    if let Some(hint) = input.completion.first() {
        if hint_y < bottom_y {
            draw_hint(surface, inner_x, hint_y, hint, avail);
        }
    }
}

/// A one-row live-filter strip: a `filter›` sigil, the buffer, and a block
/// cursor when focused (or the placeholder when empty). Unlike `draw_input_tile`
/// this draws no box — it composes ABOVE a widget inside the tile's own frame,
/// so a table lens shows its rows with a filter line at the top.
pub fn draw_filter_line(surface: &mut TextSurface, rect: Rect, input: &StudioInputVm, focused: bool) {
    let w = rect.w as usize;
    if w < 4 {
        return;
    }
    let x = rect.x as usize;
    let y = rect.y as usize;
    let sigil = "filter\u{203a} ";
    let sigil_w = display_width(sigil);
    surface.draw_text(x, y, sigil, Style::new().fg(MUTED).bg(BG));
    let field_x = x + sigil_w;
    let avail = w.saturating_sub(sigil_w + 1);
    if avail == 0 {
        return;
    }
    if input.text.is_empty() {
        let ph = truncate(&input.placeholder, avail);
        surface.draw_text(field_x, y, &ph, Style::new().fg(MUTED).bg(BG));
    } else {
        let visible = truncate(&input.text, avail);
        surface.draw_text(field_x, y, &visible, Style::new().fg(FG).bg(BG));
    }
    // The block cursor marks focus — only the focused tile owns keystrokes.
    if focused {
        let cursor_byte = input_cursor_byte(&input.text, input.cursor);
        let cursor_col = display_width(&input.text[..cursor_byte]).min(avail.saturating_sub(1));
        let cursor_char = input.text[cursor_byte..].chars().next().unwrap_or(' ');
        surface.draw_char(field_x + cursor_col, y, cursor_char, Style::new().fg(BG).bg(FG));
    }
}

/// Draw the completion hint muted, accenting any `[current-arg]` segments
/// so the argument the operator is on stands out. Single line, width-bounded.
fn draw_hint(surface: &mut TextSurface, x: usize, y: usize, hint: &str, max_w: usize) {
    let line = truncate(hint, max_w);
    let mut col = 0usize;
    let mut accent = false;
    for ch in line.chars() {
        if col >= max_w {
            break;
        }
        if ch == '[' {
            accent = true;
        }
        let style = if accent {
            Style::new().fg(ACCENT).bg(BG)
        } else {
            Style::new().fg(MUTED).bg(BG)
        };
        surface.draw_char(x + col, y, ch, style);
        if ch == ']' {
            accent = false;
        }
        col += display_width(&ch.to_string()).max(1);
    }
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
            live_filter: false,
            text: String::new(),
        }
    }

    fn row_text(surface: &TextSurface, w: usize, y: usize) -> String {
        (0..w).map(|x| surface.get(x, y).rune).collect()
    }

    fn cell_fg(
        surface: &TextSurface,
        x: usize,
        y: usize,
    ) -> ryeos_client_base::text_surface::Color {
        surface.get(x, y).fg
    }

    #[test]
    fn completion_hint_renders_only_in_command_mode_with_arg_accented() {
        let mut surface = TextSurface::new(60, 7);
        let rect = Rect {
            x: 0,
            y: 0,
            w: 60,
            h: 7,
        };
        let mut vm = input_vm("→ /commands");
        vm.text = "/thread input".to_string();
        // The arg grammar with the current argument bracketed, as the
        // engine's `argument_hint` produces it.
        vm.completion = vec!["args: [line] — send input to a thread".to_string()];
        draw_input_tile(&mut surface, rect, &vm, None, Some(Border::Sharp));

        // The hint sits on the row below the buffer (buffer is row 1).
        let hint_row = row_text(&surface, 60, 2);
        assert!(
            hint_row.contains("[line]"),
            "completion hint missing from row below buffer: {hint_row:?}"
        );

        // The bracketed current argument is accented; the surrounding text muted.
        let bracket_x = hint_row.find('[').expect("bracket present");
        assert_eq!(
            cell_fg(&surface, bracket_x, 2),
            ACCENT,
            "current arg accented"
        );
        let pre_x = hint_row.find('a').expect("the 'args:' label is present");
        assert_eq!(cell_fg(&surface, pre_x, 2), MUTED, "hint body muted");
    }

    #[test]
    fn no_completion_hint_when_not_completing() {
        let mut surface = TextSurface::new(60, 7);
        let rect = Rect {
            x: 0,
            y: 0,
            w: 60,
            h: 7,
        };
        let mut vm = input_vm("→ service:foo/bar (new chain)");
        vm.text = "hello world".to_string();
        // No completion in plain prose mode.
        draw_input_tile(&mut surface, rect, &vm, None, Some(Border::Sharp));
        // Interior only (exclude the box's left/right border columns).
        let interior: String = (2..58).map(|x| surface.get(x, 2).rune).collect();
        assert!(
            interior.trim().is_empty(),
            "no hint line should show outside command mode: {interior:?}"
        );
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
