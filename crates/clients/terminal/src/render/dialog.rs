//! Dialog framing only. Content and input use the same ordinary View widgets
//! and exact addresses as dock/tile mounts; there is no terminal catalogue.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::text_surface::{Border, Style, TextSurface};
use ryeos_client_base::ui::view_model::RyeOsDialogVm;

use super::primitives::fill_rect;
use super::text::truncate;
use super::theme::{ACCENT, FG, PANEL};

pub fn draw_dialog(surface: &mut TextSurface, dialog: &RyeOsDialogVm, now_ms: u64) {
    let w = surface.width.min(84);
    let h = surface.height.saturating_sub(4).min(32);
    if w < 4 || h < 4 {
        return;
    }
    let x = surface.width.saturating_sub(w) / 2;
    let y = surface.height.saturating_sub(h) / 3;
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
        let input_h = content.h.min(4);
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
    super::draw_view(surface, content, &dialog.view, now_ms);
}
