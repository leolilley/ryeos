//! The keys/help overlay — a static cheat-sheet of the global bindings,
//! grouped by category. The scrim behind it is drawn by the caller; this is a
//! bordered box on top, read-only (no point, unlike the launcher).

use ryeos_client_base::layout::Rect;
use ryeos_client_base::studio::view_model::StudioViewModel;
use ryeos_client_base::text_surface::{Border, Style, TextSurface};

use super::primitives::fill_rect;
use super::text::{display_width, truncate};
use super::theme::{ACCENT, FG, GOOD, MUTED, PANEL};

const CAT_W: usize = 8;
const KEYS_W: usize = 15;

pub fn draw_help(surface: &mut TextSurface, vm: &StudioViewModel) {
    let entries = &vm.help.entries;
    let w = surface.width.min(80).max(44);
    // border/title(1) + entries + footer(1) + bottom border(1).
    let h = entries.len() + 3;
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
    surface.draw_text(x + 2, y, " Keys ", Style::new().fg(ACCENT).bg(PANEL).bold());

    let mut last_cat: &str = "";
    for (row, e) in entries.iter().enumerate() {
        let ry = y + 1 + row;
        // Category column, right-aligned, shown once per contiguous group.
        if e.category.as_str() != last_cat {
            let cat = truncate(&e.category, CAT_W);
            let cat_x = x + 1 + CAT_W.saturating_sub(display_width(&cat));
            surface.draw_text(cat_x, ry, &cat, Style::new().fg(GOOD).bg(PANEL));
            last_cat = e.category.as_str();
        }
        let keys_x = x + 1 + CAT_W + 1;
        let keys = truncate(&e.keys, KEYS_W);
        surface.draw_text(keys_x, ry, &keys, Style::new().fg(FG).bg(PANEL).bold());
        let desc_x = keys_x + KEYS_W + 1;
        if desc_x < x + w - 1 {
            let desc = truncate(&e.description, x + w - 1 - desc_x);
            surface.draw_text(desc_x, ry, &desc, Style::new().fg(MUTED).bg(PANEL));
        }
    }

    surface.draw_text(
        x + 2,
        y + h - 2,
        "? or Esc to close",
        Style::new().fg(MUTED).bg(PANEL),
    );
}
