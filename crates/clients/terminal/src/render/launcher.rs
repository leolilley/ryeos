//! The launcher overlay: centered panel listing launchable views.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::text_surface::{Border, Style, TextSurface};

use ryeos_client_base::studio::view_model::StudioViewModel;

use super::primitives::{draw_shadow, fill_rect};
use super::text::truncate;
use super::theme::{style_fg, style_muted, style_selected, ACCENT, FG, PANEL, WARN};

pub fn draw_launcher(surface: &mut TextSurface, vm: &StudioViewModel) {
    let w = surface.width.min(72).max(24);
    let h = (vm.launcher.items.len() + 4)
        .min(surface.height.saturating_sub(2))
        .max(6);
    let x = surface.width.saturating_sub(w) / 2;
    let y = surface.height.saturating_sub(h) / 3;
    let rect = Rect::new(x as u16, y as u16, w as u16, h as u16);
    draw_shadow(surface, rect);
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
        " launcher ",
        Style::new().fg(WARN).bg(PANEL).bold(),
    );
    surface.draw_text(
        x + 2,
        y + 1,
        &truncate(&format!("> {}", vm.launcher.query), w - 4),
        style_fg(),
    );
    for (i, item) in vm
        .launcher
        .items
        .iter()
        .take(h.saturating_sub(4))
        .enumerate()
    {
        let selected = i == vm.launcher.selected;
        let style = if selected {
            style_selected()
        } else if item.enabled {
            style_fg()
        } else {
            style_muted()
        };
        let line = format!("{}  {}", item.label, item.hint);
        surface.draw_text(x + 2, y + 3 + i, &truncate(&line, w - 4), style);
    }
}
