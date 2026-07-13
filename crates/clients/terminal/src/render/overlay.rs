//! Generic overlay host. Overlays are transient surface layers over the
//! workspace; their content is data-projected in the shared VM and rendered
//! here by widget shape, not by product-specific overlay names.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::text_surface::{Border, Style, TextSurface};
use ryeos_client_base::ui::view_model::{RyeOsOverlayItemVm, RyeOsOverlayVm};

use super::primitives::{fill_line, fill_rect};
use super::text::{display_width, truncate};
use super::theme::{ACCENT, BG, FG, GOOD, MUTED, PANEL};

const CAT_W: usize = 12;
const TABLE_PRIMARY_W: usize = 24;

pub fn draw_overlay(surface: &mut TextSurface, overlay: &RyeOsOverlayVm) {
    match overlay.widget.as_str() {
        "table" => draw_table_overlay(surface, overlay),
        _ => draw_palette_overlay(surface, overlay),
    }
}

fn draw_palette_overlay(surface: &mut TextSurface, overlay: &RyeOsOverlayVm) {
    let w = surface.width.clamp(32, 76);
    let max_rows = surface.height.saturating_sub(8).max(3);
    let rows = overlay.items.len().min(max_rows);
    let h = rows + 4;
    let x = surface.width.saturating_sub(w) / 2;
    let y = surface.height.saturating_sub(h) / 3;
    let rect = Rect::new(x as u16, y as u16, w as u16, h as u16);
    draw_panel(surface, rect, &overlay.title);
    draw_prompt(surface, x, y + 1, w, &overlay.query);

    let start = scroll_start(overlay.selected, rows, overlay.items.len());
    for (row, item) in overlay.items.iter().skip(start).take(rows).enumerate() {
        draw_palette_row(
            surface,
            x + 1,
            y + 3 + row,
            w.saturating_sub(2),
            item,
            start + row == overlay.selected && item.enabled,
        );
    }
    draw_hint(surface, x, y + h - 2, &overlay.hint, w);
}

fn draw_table_overlay(surface: &mut TextSurface, overlay: &RyeOsOverlayVm) {
    let w = surface.width.clamp(44, 84);
    let max_rows = surface.height.saturating_sub(8).max(4);
    let rows = overlay.items.len().min(max_rows);
    let h = rows + 5;
    let x = surface.width.saturating_sub(w) / 2;
    let y = surface.height.saturating_sub(h) / 3;
    let rect = Rect::new(x as u16, y as u16, w as u16, h as u16);
    draw_panel(surface, rect, &overlay.title);
    draw_prompt(surface, x, y + 1, w, &overlay.query);
    let first_header = overlay
        .columns
        .first()
        .map(String::as_str)
        .unwrap_or("Keys");
    let second_header = overlay
        .columns
        .get(1)
        .map(String::as_str)
        .unwrap_or("Action");
    let first_w = TABLE_PRIMARY_W;
    surface.draw_text(
        x + 2,
        y + 3,
        first_header,
        Style::new().fg(GOOD).bg(PANEL).bold(),
    );
    surface.draw_text(
        x + 2 + first_w,
        y + 3,
        second_header,
        Style::new().fg(GOOD).bg(PANEL).bold(),
    );
    let start = scroll_start(overlay.selected, rows, overlay.items.len());
    for (row, item) in overlay.items.iter().skip(start).take(rows).enumerate() {
        let ry = y + 4 + row;
        let selected = start + row == overlay.selected && item.enabled;
        let bg = if selected { ACCENT } else { PANEL };
        let fg = if selected {
            BG
        } else if item.enabled {
            FG
        } else {
            MUTED
        };
        fill_line(
            surface,
            x + 1,
            ry,
            w.saturating_sub(2),
            Style::new().fg(fg).bg(bg),
        );
        if item.header {
            // A group header: fold glyph + title across the row; its
            // Enter intent folds the group, so it reads as a control.
            let glyph = if item.expanded { "▾" } else { "▸" };
            let header_fg = if selected { BG } else { GOOD };
            surface.draw_text(
                x + 2,
                ry,
                &truncate(&format!("{glyph} {}", item.primary), w.saturating_sub(4)),
                Style::new().fg(header_fg).bg(bg).bold(),
            );
            continue;
        }
        let indent = 2 * item.depth as usize;
        surface.draw_text(
            x + 2 + indent,
            ry,
            &truncate(&item.primary, first_w.saturating_sub(1 + indent)),
            Style::new().fg(fg).bg(bg).bold(),
        );
        let detail = if item.meta.is_empty() {
            item.secondary.clone()
        } else {
            format!("{} · {}", item.secondary, item.meta)
        };
        surface.draw_text(
            x + 2 + first_w,
            ry,
            &truncate(&detail, w.saturating_sub(first_w + 5)),
            Style::new().fg(if selected { BG } else { MUTED }).bg(bg),
        );
    }
    draw_hint(surface, x, y + h - 2, &overlay.hint, w);
}

fn draw_panel(surface: &mut TextSurface, rect: Rect, title: &str) {
    fill_rect(surface, rect, Style::new().fg(FG).bg(PANEL));
    let x = rect.x as usize;
    let y = rect.y as usize;
    let w = rect.w as usize;
    let h = rect.h as usize;
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
        &format!(" {} ", title),
        Style::new().fg(ACCENT).bg(PANEL).bold(),
    );
}

fn draw_prompt(surface: &mut TextSurface, x: usize, y: usize, w: usize, query: &str) {
    surface.draw_text(x + 2, y, ">", Style::new().fg(ACCENT).bg(PANEL).bold());
    let query = truncate(query, w.saturating_sub(6));
    surface.draw_text(x + 4, y, &query, Style::new().fg(FG).bg(PANEL));
    let cur_x = x + 4 + display_width(&query);
    surface.draw_char(cur_x, y, ' ', Style::new().fg(PANEL).bg(FG));
}

fn draw_hint(surface: &mut TextSurface, x: usize, y: usize, hint: &str, w: usize) {
    if !hint.is_empty() {
        surface.draw_text(
            x + 2,
            y,
            &truncate(hint, w.saturating_sub(4)),
            Style::new().fg(MUTED).bg(PANEL),
        );
    }
}

fn draw_palette_row(
    surface: &mut TextSurface,
    x: usize,
    y: usize,
    w: usize,
    item: &RyeOsOverlayItemVm,
    selected: bool,
) {
    let bg = if selected { ACCENT } else { PANEL };
    let row_fg = if selected { BG } else { FG };
    let cat_fg = if selected { BG } else { GOOD };
    let desc_fg = if selected { BG } else { MUTED };
    fill_line(surface, x, y, w, Style::new().fg(row_fg).bg(bg));
    if item.header {
        let glyph = if item.expanded { "▾" } else { "▸" };
        surface.draw_text(
            x + 1,
            y,
            &truncate(&format!("{glyph} {}", item.primary), w.saturating_sub(2)),
            Style::new().fg(cat_fg).bg(bg).bold(),
        );
        return;
    }
    let cat = truncate(&item.category, CAT_W);
    let cat_x = x + CAT_W.saturating_sub(display_width(&cat));
    surface.draw_text(cat_x, y, &cat, Style::new().fg(cat_fg).bg(bg));
    let primary_x = x + CAT_W + 2;
    let primary = truncate(&item.primary, w.saturating_sub(CAT_W + 3));
    let primary_style = if item.enabled {
        Style::new().fg(row_fg).bg(bg).bold()
    } else {
        Style::new().fg(desc_fg).bg(bg)
    };
    surface.draw_text(primary_x, y, &primary, primary_style);
    if !item.secondary.is_empty() {
        let desc_x = primary_x + display_width(&primary) + 2;
        if desc_x < x + w {
            surface.draw_text(
                desc_x,
                y,
                &truncate(&item.secondary, x + w - desc_x),
                Style::new().fg(desc_fg).bg(bg),
            );
        }
    }
}

fn scroll_start(selected: usize, rows: usize, total: usize) -> usize {
    selected
        .saturating_sub(rows / 2)
        .min(total.saturating_sub(rows))
}
