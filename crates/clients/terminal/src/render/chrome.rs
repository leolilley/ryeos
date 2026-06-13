//! Tile and frame chrome: bars, dock tiles, tile frames with title,
//! corner marks, and the provenance/affordance footer. Chrome carries
//! truth (provenance, focus) — it is load-bearing, not decoration.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::studio::view_model::{
    StudioDockTileVm, StudioInputVm, StudioViewModel, StudioViewVm,
};
use ryeos_client_base::text_surface::{Border, Color, Style, TextSurface};

use super::input::draw_input_dock;
use super::primitives::{draw_lines, draw_shadow, fill_line, fill_rect};
use super::text::{display_width, letterspace, truncate};
use super::theme::{
    border_for, style_muted, style_selected, tone_style, ACCENT, FG, MUTED, PANEL, PANEL_2, SHADOW,
    WARN,
};
use super::widgets;

pub fn draw_top_bar(surface: &mut TextSurface, vm: &StudioViewModel) {
    let text = format!(
        " {}  {}  {} ",
        vm.presentation.chrome.version_label,
        vm.presentation.chrome.top_bar.focused_title,
        vm.presentation.chrome.top_bar.layout_symbol
    );
    draw_bar(surface, 0, &text, ACCENT);
}

pub fn draw_status_bar(surface: &mut TextSurface, vm: &StudioViewModel) {
    let y = surface.height.saturating_sub(1);
    fill_line(
        surface,
        0,
        y,
        surface.width,
        Style::new().fg(MUTED).bg(PANEL),
    );
    let mut x = 1usize;
    for segment in &vm.presentation.chrome.status_bar.segments {
        if x >= surface.width {
            return;
        }
        if let Some(label) = &segment.label {
            let label = format!("{}: ", letterspace(label));
            surface.draw_text(x, y, &truncate(&label, surface.width - x), style_muted());
            x = x.saturating_add(display_width(&label));
        }
        let value = truncate(&segment.value, surface.width.saturating_sub(x));
        surface.draw_text(x, y, &value, tone_style(segment.tone));
        x = x.saturating_add(display_width(&value) + 2);
    }
    if x + 3 < surface.width {
        surface.draw_text(x, y, "·", style_muted());
        x += 2;
        surface.draw_text(
            x,
            y,
            &truncate(
                &vm.presentation.chrome.status_bar.key_hint,
                surface.width - x,
            ),
            style_muted(),
        );
    }
}

fn draw_bar(surface: &mut TextSurface, y: usize, text: &str, fg: Color) {
    fill_line(surface, 0, y, surface.width, Style::new().fg(fg).bg(PANEL));
    surface.draw_text(
        0,
        y,
        &truncate(text, surface.width),
        Style::new().fg(fg).bg(PANEL),
    );
}

pub fn draw_docks(surface: &mut TextSurface, body: Rect, vm: &StudioViewModel) -> Rect {
    let mut center = body;
    let docks = &vm.workspace.docks;
    let border = border_for(&vm.presentation.chrome.border);

    if let Some(left) = &docks.left {
        let w = left.size.min(center.w.saturating_sub(8));
        if w > 0 {
            let rect = Rect::new(center.x, center.y, w, center.h);
            draw_dock_tile(surface, rect, left, border);
            center.x = center.x.saturating_add(w.saturating_add(1));
            center.w = center.w.saturating_sub(w.saturating_add(1));
        }
    }

    if let Some(right) = &docks.right {
        let w = right.size.min(center.w.saturating_sub(8));
        if w > 0 {
            let x = center.x.saturating_add(center.w.saturating_sub(w));
            let rect = Rect::new(x, center.y, w, center.h);
            draw_dock_tile(surface, rect, right, border);
            center.w = center.w.saturating_sub(w.saturating_add(1));
        }
    }

    if let Some(top) = &docks.top {
        let h = top.size.min(center.h.saturating_sub(6));
        if h > 0 {
            let rect = Rect::new(center.x, center.y, center.w, h);
            draw_dock_tile(surface, rect, top, border);
            center.y = center.y.saturating_add(h.saturating_add(1));
            center.h = center.h.saturating_sub(h.saturating_add(1));
        }
    }

    if let Some(bottom) = &docks.bottom {
        let h = bottom.size.min(center.h.saturating_sub(6));
        if h > 0 {
            let y = center.y.saturating_add(center.h.saturating_sub(h));
            let rect = Rect::new(center.x, y, center.w, h);
            draw_dock_tile(surface, rect, bottom, border);
            center.h = center.h.saturating_sub(h.saturating_add(1));
        }
    }

    center
}

fn draw_dock_tile(
    surface: &mut TextSurface,
    rect: Rect,
    dock: &StudioDockTileVm,
    border: Option<Border>,
) {
    draw_shadow(surface, rect);
    fill_rect(surface, rect, Style::new().fg(FG).bg(PANEL));
    let x = rect.x as usize;
    let y = rect.y as usize;
    let w = rect.w as usize;
    let h = rect.h as usize;
    if w < 2 || h < 2 {
        return;
    }
    if let Some(border) = border {
        surface.draw_box(
            x,
            y,
            x + w - 1,
            y + h - 1,
            border,
            Style::new().fg(SHADOW).bg(PANEL),
        );
    }
    surface.draw_text(
        x + 2,
        y,
        &truncate(&format!(" {} ", dock.title), w.saturating_sub(4)),
        Style::new().fg(WARN).bg(PANEL).bold(),
    );
    let inner = Rect::new(
        rect.x + 1,
        rect.y + 1,
        rect.w.saturating_sub(2),
        rect.h.saturating_sub(2),
    );
    // An instance that declares `input` renders as the prompt (any widget
    // may carry a prompt — input is an orthogonal capability).
    if let Some(input) = dock.input.as_ref() {
        draw_input_dock(surface, inner, input);
        return;
    }
    draw_dock_view(surface, inner, &dock.view);
}

fn draw_dock_view(surface: &mut TextSurface, rect: Rect, view: &StudioViewVm) {
    let lines = match view {
        StudioViewVm::Rows { title, rows, .. } => {
            let mut lines = vec![title.clone()];
            for row in rows.iter().take(rect.h.saturating_sub(2) as usize) {
                lines.push(format!(
                    "{} {} {}",
                    if row.selected { "▶" } else { " " },
                    row.primary,
                    row.meta.clone().unwrap_or_default()
                ));
            }
            lines
        }
        StudioViewVm::Timeline { title, entries, .. } => {
            surface.draw_text(
                rect.x as usize,
                rect.y as usize,
                &truncate(title, rect.w as usize),
                style_muted(),
            );
            if rect.h > 1 {
                widgets::timeline::draw_timeline(
                    surface,
                    Rect::new(rect.x, rect.y + 1, rect.w, rect.h.saturating_sub(1)),
                    entries,
                );
            }
            return;
        }
        StudioViewVm::Placeholder { title, message } => {
            vec![title.clone(), message.clone()]
        }
        _ => vec!["unsupported dock view".to_string()],
    };
    draw_lines(surface, rect, &lines);
}

#[allow(clippy::too_many_arguments)]
pub fn draw_tile(
    surface: &mut TextSurface,
    rect: Rect,
    tile_id: &str,
    focused: bool,
    title: &str,
    action_count: usize,
    view: &StudioViewVm,
    input: Option<&StudioInputVm>,
    border: Option<Border>,
) {
    let w = rect.w as usize;
    let h = rect.h as usize;
    if w == 0 || h == 0 {
        return;
    }
    draw_shadow(surface, rect);
    // The focused accent stays color-only; the glyph set follows the
    // surface-declared border style.
    let border_style = if focused {
        Style::new().fg(ACCENT).bg(PANEL)
    } else {
        Style::new().fg(SHADOW).bg(PANEL)
    };
    fill_rect(surface, rect, Style::new().fg(FG).bg(PANEL));
    if w >= 2 && h >= 2 {
        if let Some(border) = border {
            surface.draw_box(
                rect.x as usize,
                rect.y as usize,
                rect.x as usize + w - 1,
                rect.y as usize + h - 1,
                border,
                border_style,
            );
        }
        if h > 3 {
            fill_line(
                surface,
                rect.x as usize + 1,
                rect.y as usize + 1,
                w.saturating_sub(2),
                Style::new().fg(FG).bg(PANEL_2),
            );
            for x in (rect.x as usize + 1)..(rect.x as usize + w.saturating_sub(1)) {
                surface.draw_char(
                    x,
                    rect.y as usize + 2,
                    '─',
                    Style::new().fg(SHADOW).bg(PANEL),
                );
            }
        }
        let action_hint = if action_count > 0 {
            format!("  {action_count} actions")
        } else {
            String::new()
        };
        let label = format!(" {title} #{tile_id}{action_hint} ");
        surface.draw_text(
            rect.x as usize + 2,
            rect.y as usize + 1,
            &truncate(&label, w.saturating_sub(4)),
            if focused {
                style_selected().bold()
            } else {
                Style::new().fg(MUTED).bg(PANEL_2)
            },
        );
        if focused {
            draw_corner_marks(surface, rect);
        }
    }
    let footer = view_chrome(view);
    if h > 5 {
        let footer_y = rect.y as usize + h - 2;
        fill_line(
            surface,
            rect.x as usize + 1,
            footer_y,
            w.saturating_sub(2),
            Style::new().fg(MUTED).bg(PANEL),
        );
        if let Some((provenance, affordances)) = footer {
            surface.draw_text(
                rect.x as usize + 2,
                footer_y,
                &truncate(provenance, w.saturating_sub(4)),
                style_muted(),
            );
            if !affordances.is_empty() && w > 10 {
                let right = affordances.join(" · ");
                let right = truncate(&right, w.saturating_sub(4));
                let right_w = display_width(&right);
                let x = rect.x as usize + w.saturating_sub(right_w + 2);
                surface.draw_text(x, footer_y, &right, style_muted());
            }
        }
    }
    let inner = if rect.h > 4 {
        Rect::new(
            rect.x + 1,
            rect.y + 3,
            rect.w.saturating_sub(2),
            rect.h.saturating_sub(if h > 5 { 5 } else { 4 }),
        )
    } else {
        Rect::new(
            rect.x + 1,
            rect.y + 1,
            rect.w.saturating_sub(2),
            rect.h.saturating_sub(2),
        )
    };
    // An instance that declares `input` renders as the prompt.
    if let Some(input) = input {
        draw_input_dock(surface, inner, input);
        return;
    }
    super::draw_view(surface, inner, view);
}

fn view_chrome(view: &StudioViewVm) -> Option<(&str, &[String])> {
    match view {
        StudioViewVm::Rows {
            provenance,
            affordance_hints,
            ..
        }
        | StudioViewVm::Timeline {
            provenance,
            affordance_hints,
            ..
        } => provenance
            .as_deref()
            .map(|provenance| (provenance, affordance_hints.as_slice())),
        _ => None,
    }
}

fn draw_corner_marks(surface: &mut TextSurface, rect: Rect) {
    let x = rect.x as usize;
    let y = rect.y as usize;
    let w = rect.w as usize;
    let h = rect.h as usize;
    if w < 2 || h < 2 {
        return;
    }
    let style = Style::new().fg(ACCENT).bg(PANEL).bold();
    surface.draw_char(x, y, '╔', style);
    surface.draw_char(x + w - 1, y, '╗', style);
    surface.draw_char(x, y + h - 1, '╚', style);
    surface.draw_char(x + w - 1, y + h - 1, '╝', style);
}
