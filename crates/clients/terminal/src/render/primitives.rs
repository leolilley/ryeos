//! Raw surface operations shared by every composition: fills, shadows,
//! plain line stacks. No VM knowledge here beyond styles.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::text_surface::{Style, TextSurface};

use super::text::truncate;
use super::theme::{style_fg, style_selected, PANEL, SHADOW, WARN};

pub fn fill_rect(surface: &mut TextSurface, rect: Rect, style: Style) {
    let x = rect.x as usize;
    let y = rect.y as usize;
    let w = rect.w as usize;
    let h = rect.h as usize;
    if w == 0 || h == 0 {
        return;
    }
    surface.fill_rect(x, y, x + w - 1, y + h - 1, ' ', style);
}

pub fn fill_line(surface: &mut TextSurface, x: usize, y: usize, width: usize, style: Style) {
    if width == 0 {
        return;
    }
    surface.fill_rect(x, y, x + width - 1, y, ' ', style);
}

/// Dim the entire surface toward the page background — the scrim behind an
/// overlay. Each cell's colours are blended toward `BG` so content stays
/// readable but recedes; the overlay then draws on top at full brightness.
pub fn dim_surface(surface: &mut TextSurface) {
    use ryeos_client_base::text_surface::Style;
    for y in 0..surface.height {
        for x in 0..surface.width {
            let cell = surface.get(x, y);
            let fg = dim_color(cell.fg, 0.62);
            let bg = dim_color(cell.bg, 0.45);
            let rune = if cell.rune == '\0' { ' ' } else { cell.rune };
            surface.draw_char(x, y, rune, Style::new().fg(fg).bg(bg));
        }
    }
}

fn dim_color(
    c: ryeos_client_base::text_surface::Color,
    t: f32,
) -> ryeos_client_base::text_surface::Color {
    use super::theme::BG;
    use ryeos_client_base::text_surface::Color;
    match (c, BG) {
        (Color::Rgb(r, g, b), Color::Rgb(br, bg_, bb)) => {
            let mix = |c: u8, d: u8| ((c as f32) * (1.0 - t) + (d as f32) * t).round() as u8;
            Color::Rgb(mix(r, br), mix(g, bg_), mix(b, bb))
        }
        _ => c,
    }
}

pub fn draw_shadow(surface: &mut TextSurface, rect: Rect) {
    if rect.w < 2 || rect.h < 2 {
        return;
    }
    let shadow = Rect::new(
        rect.x.saturating_add(1),
        rect.y.saturating_add(1),
        rect.w.saturating_sub(1),
        rect.h.saturating_sub(1),
    );
    fill_rect(surface, shadow, Style::new().fg(SHADOW).bg(SHADOW));
}

pub fn draw_lines(surface: &mut TextSurface, rect: Rect, lines: &[String]) {
    let width = rect.w as usize;
    let height = rect.h as usize;
    if width == 0 || height == 0 {
        return;
    }
    for (i, line) in lines.iter().take(height).enumerate() {
        let style = if line.starts_with('›') {
            style_selected()
        } else if i == 0 {
            Style::new().fg(WARN).bg(PANEL).bold()
        } else {
            style_fg()
        };
        surface.draw_text(
            rect.x as usize,
            rect.y as usize + i,
            &truncate(line, width),
            style,
        );
    }
}
