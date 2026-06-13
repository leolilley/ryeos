//! The home composition: full-body negative space, welcome + key hints
//! right-of-center (the ambient mark will own the left), and a
//! full-width input panel anchored flush at the bottom.
//!
//! The panel border follows the surface-declared chrome border
//! (`presentation.chrome.border`): thick | thin | hidden | none, mapped
//! by `theme::border_for` — the single border authority. No shadows —
//! border weight does the work in the terminal.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::studio::view_model::StudioViewModel;
use ryeos_client_base::text_surface::{Style, TextSurface};

use super::text::{display_width, truncate};
use super::theme::{border_for, ACCENT, BG, FG, MUTED};

pub fn draw_home(surface: &mut TextSurface, rect: Rect, vm: &StudioViewModel) {
    let w = rect.w as usize;
    let h = rect.h as usize;
    if w == 0 || h == 0 {
        return;
    }

    // Input panel: full width, flush at the bottom, 3 interior rows.
    let box_h = 5usize.min(h);
    let box_w = w;
    let box_y = h.saturating_sub(box_h);

    draw_welcome_block(surface, w, box_y);

    if box_w >= 4 && box_h >= 3 {
        draw_input_panel(surface, vm, box_w, box_y, box_h);
    }
}

fn draw_welcome_block(surface: &mut TextSurface, w: usize, box_y: usize) {
    if box_y < 8 {
        return;
    }
    // Right-of-center at wide widths (the ambient mark takes the left);
    // left-aligned with a margin when narrow.
    let text_x = if w >= 70 { w / 2 } else { 2 };
    let max_w = w.saturating_sub(text_x + 1);
    let welcome_y = box_y * 2 / 5;

    // The ambient mark holds the left half, vertically centered against
    // the welcome block, only when there's honest room for it.
    if w >= 70 && box_y > super::ambient::MARK_HEIGHT + 2 {
        let mark_x = (w / 4)
            .saturating_sub(super::ambient::MARK_WIDTH / 2)
            .max(2);
        let mark_y = (welcome_y + 1)
            .saturating_sub(super::ambient::MARK_HEIGHT / 2)
            .max(1);
        if mark_y + super::ambient::MARK_HEIGHT < box_y {
            super::ambient::draw_mark(surface, mark_x, mark_y);
        }
    }

    surface.draw_text(
        text_x,
        welcome_y,
        &truncate("Welcome to RyeOS", max_w),
        Style::new().fg(ACCENT).bg(BG).bold(),
    );

    let hints = [("alt+k", " for launcher"), ("ctrl+c", " to quit")];
    for (offset, (key, rest)) in hints.iter().enumerate() {
        let y = welcome_y + 2 + offset;
        if y + 1 >= box_y {
            break;
        }
        surface.draw_text(text_x, y, key, Style::new().fg(FG).bg(BG).bold());
        let rest_x = text_x + display_width(key);
        surface.draw_text(
            rest_x,
            y,
            &truncate(rest, max_w.saturating_sub(display_width(key))),
            Style::new().fg(MUTED).bg(BG),
        );
    }
}

fn draw_input_panel(
    surface: &mut TextSurface,
    vm: &StudioViewModel,
    box_w: usize,
    box_y: usize,
    box_h: usize,
) {
    if let Some(border) = border_for(&vm.presentation.chrome.border) {
        surface.draw_box(
            0,
            box_y,
            box_w - 1,
            box_y + box_h - 1,
            border,
            Style::new().fg(FG).bg(BG),
        );
    }

    // Labels live ON the border rows, Amp-style: identity top-right,
    // context bottom-right. They stay put across border treatments.
    draw_border_label(
        surface,
        box_w,
        box_y,
        " rye os ",
        Style::new().fg(ACCENT).bg(BG),
    );
    if let Some(project) = vm.session.project_path.as_deref() {
        let bottom_label = format!(" {} ", shorten_home(project));
        draw_border_label(
            surface,
            box_w,
            box_y + box_h - 1,
            &bottom_label,
            Style::new().fg(MUTED).bg(BG),
        );
    }

    // Block cursor on the first interior row, one cell of padding.
    surface.draw_char(2, box_y + 1, '█', Style::new().fg(FG).bg(BG));
}

fn draw_border_label(surface: &mut TextSurface, box_w: usize, y: usize, label: &str, style: Style) {
    let label_w = display_width(label);
    if label_w + 4 >= box_w {
        return;
    }
    let x = box_w.saturating_sub(label_w + 2);
    surface.draw_text(x, y, label, style);
}

fn shorten_home(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}
