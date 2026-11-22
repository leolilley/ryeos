//! Dialog framing only. Content and input use the same ordinary View widgets
//! and exact addresses as tile mounts; there is no terminal catalogue.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::layout_table::LAYOUT;
use ryeos_client_base::text_surface::{Border, Style, TextSurface};
use ryeos_client_base::ui::view_model::{RyeOsDialogVm, RyeOsRowVm, RyeOsViewVm};

use super::primitives::fill_rect;
use super::text::truncate;
use super::theme::{ACCENT, FG, PANEL};

pub fn draw_dialog(surface: &mut TextSurface, dialog: &RyeOsDialogVm, now_ms: u64) {
    let w = surface.width.min(LAYOUT.cells.dialog_max_width);
    let h = surface
        .height
        .saturating_sub(LAYOUT.cells.dialog_height_margin)
        .min(LAYOUT.cells.dialog_max_height);
    if w < LAYOUT.cells.dialog_least || h < LAYOUT.cells.dialog_least {
        return;
    }
    let x = surface.width.saturating_sub(w) / 2;
    let y = surface.height.saturating_sub(h) / LAYOUT.cells.dialog_top_divisor;
    let rect = Rect::new(x as u16, y as u16, w as u16, h as u16);
    fill_rect(surface, rect, Style::new().fg(FG).bg(PANEL));
    surface.draw_box(
        x,
        y,
        x + w - 1,
        y + h - 1,
        Border::Sharp,
        Style::new().fg(ACCENT).bg(PANEL),
    );
    surface.draw_text(
        x + 2,
        y,
        &truncate(&format!(" {} ", dialog.title), w - 4),
        Style::new().fg(ACCENT).bg(PANEL),
    );
    let mut content = Rect::new(
        (x + 1) as u16,
        (y + 1) as u16,
        (w - 2) as u16,
        (h - 2) as u16,
    );
    if let Some(input) = dialog.input.as_ref().filter(|input| input.live_filter) {
        super::input::draw_filter_line(
            surface,
            Rect::new(content.x, content.y, content.w, 1),
            input,
            input.focused,
        );
        content.y += 1;
        content.h = content.h.saturating_sub(1);
    }
    if let Some(input) = dialog.input.as_ref().filter(|input| !input.live_filter) {
        let input_h = content.h.min(LAYOUT.cells.dialog_input_max_rows);
        super::input::draw_input_tile(
            surface,
            Rect::new(
                content.x,
                content.y + content.h - input_h,
                content.w,
                input_h,
            ),
            input,
            None,
            Some(Border::Sharp),
        );
        content.h = content.h.saturating_sub(input_h);
    }
    // The places a chosen launcher view can land are one line under its list.
    let (view, places) = split_places(&dialog.view);
    if !places.is_empty() && content.h > 1 {
        let line_y = content.y + content.h - 1;
        let mut line = String::new();
        let mut selected_span = None;
        for place in &places {
            if !line.is_empty() {
                line.push_str("  ");
            }
            let start = line.chars().count();
            line.push_str(&format!(
                "{} {}",
                place.meta.as_deref().unwrap_or_default(),
                place.primary
            ));
            if place.selected {
                selected_span = Some((start, line.chars().count()));
            }
        }
        let line = truncate(&line, content.w as usize);
        surface.draw_text(
            content.x as usize,
            line_y as usize,
            &line,
            Style::new().fg(FG).bg(PANEL),
        );
        if let Some((start, end)) = selected_span {
            let shown: String = line.chars().skip(start).take(end - start).collect();
            surface.draw_text(
                content.x as usize + start,
                line_y as usize,
                &shown,
                Style::new().fg(PANEL).bg(ACCENT),
            );
        }
        content.h -= 1;
    }
    super::draw_view(surface, content, &view, now_ms);
}

/// A launcher's list without its places, and the places apart.
fn split_places(view: &RyeOsViewVm) -> (RyeOsViewVm, Vec<RyeOsRowVm>) {
    let RyeOsViewVm::Rows { rows, .. } = view else {
        return (view.clone(), Vec::new());
    };
    let is_place = |row: &RyeOsRowVm| row.launch.as_ref().is_some_and(|launch| launch.place);
    if !rows.iter().any(is_place) {
        return (view.clone(), Vec::new());
    }
    let places = rows.iter().filter(|row| is_place(row)).cloned().collect();
    let mut list = view.clone();
    if let RyeOsViewVm::Rows { rows, .. } = &mut list {
        rows.retain(|row| !is_place(row));
    }
    (list, places)
}
