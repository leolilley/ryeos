//! The launcher overlay — a command palette. The background is dimmed
//! behind it (drawn by `build_surface`); the palette itself is a bordered
//! box with a prompt, a category column, the command, and a muted
//! description, with the selected row highlighted full-width.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::studio::view_model::{StudioLauncherItemVm, StudioViewModel};
use ryeos_client_base::text_surface::{Border, Style, TextSurface};

use super::primitives::fill_rect;
use super::text::{display_width, truncate};
use super::theme::{ACCENT, BG, FG, GOOD, MUTED, PANEL};

const CAT_W: usize = 12;

pub fn draw_launcher(surface: &mut TextSurface, vm: &StudioViewModel) {
    let items = &vm.launcher.items;
    let w = surface.width.min(76).max(28);
    let max_rows = surface.height.saturating_sub(8).max(3);
    let rows = items.len().min(max_rows);
    let h = rows + 4; // border(2) + prompt + gap
    let x = surface.width.saturating_sub(w) / 2;
    let y = surface.height.saturating_sub(h) / 3;
    let rect = Rect::new(x as u16, y as u16, w as u16, h as u16);

    fill_rect(surface, rect, Style::new().fg(FG).bg(PANEL));
    surface.draw_box(
        x,
        y,
        x + w - 1,
        y + h - 1,
        Border::Thick,
        Style::new().fg(ACCENT).bg(PANEL),
    );
    surface.draw_text(
        x + 2,
        y,
        " Launcher ",
        Style::new().fg(ACCENT).bg(PANEL).bold(),
    );

    // Prompt with a block cursor at the end of the query.
    let prompt_y = y + 1;
    surface.draw_text(
        x + 2,
        prompt_y,
        ">",
        Style::new().fg(ACCENT).bg(PANEL).bold(),
    );
    let query = truncate(&vm.launcher.query, w.saturating_sub(6));
    surface.draw_text(x + 4, prompt_y, &query, Style::new().fg(FG).bg(PANEL));
    let cur_x = x + 4 + display_width(&query);
    surface.draw_char(cur_x, prompt_y, ' ', Style::new().fg(PANEL).bg(FG));

    let start = scroll_start(vm.launcher.selected, rows, items.len());
    for (row, item) in items.iter().skip(start).take(rows).enumerate() {
        let i = start + row;
        let ry = y + 3 + row;
        draw_row(
            surface,
            x + 1,
            ry,
            w.saturating_sub(2),
            item,
            i == vm.launcher.selected,
        );
    }
}

/// Keep the selected row in view when the list is longer than the box.
fn scroll_start(selected: usize, rows: usize, total: usize) -> usize {
    if total <= rows || selected < rows {
        0
    } else {
        (selected + 1 - rows).min(total - rows)
    }
}

fn draw_row(
    surface: &mut TextSurface,
    x: usize,
    y: usize,
    w: usize,
    item: &StudioLauncherItemVm,
    selected: bool,
) {
    let (category, command) = split_label(&item.label);
    let bg = if selected { ACCENT } else { PANEL };
    let row_fg = if selected { BG } else { FG };
    let cat_fg = if selected { BG } else { GOOD };
    let desc_fg = if selected { BG } else { MUTED };

    // Full-width row fill (so the selected highlight spans the box).
    for col in 0..w {
        surface.draw_char(x + col, y, ' ', Style::new().fg(row_fg).bg(bg));
    }

    // Category column, right-aligned.
    let cat = truncate(&category, CAT_W);
    let cat_x = x + CAT_W.saturating_sub(display_width(&cat));
    surface.draw_text(cat_x, y, &cat, Style::new().fg(cat_fg).bg(bg));

    // Command (bold), then a muted description in the remaining space.
    let cmd_x = x + CAT_W + 2;
    let cmd = truncate(&command, w.saturating_sub(CAT_W + 3));
    let cmd_style = if item.enabled {
        Style::new().fg(row_fg).bg(bg).bold()
    } else {
        Style::new().fg(desc_fg).bg(bg)
    };
    surface.draw_text(cmd_x, y, &cmd, cmd_style);

    if !item.hint.is_empty() {
        let desc_x = cmd_x + display_width(&cmd) + 2;
        if desc_x < x + w {
            let desc = truncate(&item.hint, x + w - desc_x);
            surface.draw_text(desc_x, y, &desc, Style::new().fg(desc_fg).bg(bg));
        }
    }
}

/// Derive `(category, command)` from a launcher label: a `a/b/c` ref shows
/// the last segment as the command and the prior segment as the category;
/// a plain label has no category.
fn split_label(label: &str) -> (String, String) {
    match label.rsplit_once('/') {
        Some((head, last)) => {
            let cat = head.rsplit('/').next().unwrap_or(head);
            (cat.to_string(), last.to_string())
        }
        None => (String::new(), label.to_string()),
    }
}
