//! Terminal renderer for the shared Studio view model.
//!
//! This is intentionally a renderer only: it consumes `StudioViewModel` and
//! emits a terminal `TextSurface`. Studio state, actions, and effects remain in
//! `ryeos-client-base` so terminal and web share the same product semantics.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::studio::view_model::{
    StudioDockTileVm, StudioDockViewVm, StudioFrameModeVm, StudioInputVm, StudioLayoutNodeVm,
    StudioRowVm, StudioTimelineEntryVm, StudioTone, StudioViewModel, StudioViewVm,
};
use ryeos_client_base::text_surface::{Border, Color, Style, TextSurface};

use crate::render_text;

const BG: Color = Color::Rgb(0x1d, 0x20, 0x21);
const PANEL: Color = Color::Rgb(0x28, 0x28, 0x28);
const PANEL_2: Color = Color::Rgb(0x3c, 0x38, 0x36);
const SHADOW: Color = Color::Rgb(0x50, 0x49, 0x45);
const FG: Color = Color::Rgb(0xeb, 0xdb, 0xb2);
const FG_SOFT: Color = Color::Rgb(0xd5, 0xc4, 0xa1);
const MUTED: Color = Color::Rgb(0xa8, 0x99, 0x84);
const ACCENT: Color = Color::Rgb(0xd6, 0x5d, 0x0e);
const WARN: Color = Color::Rgb(0xfa, 0xbd, 0x2f);
const GOOD: Color = Color::Rgb(0x8e, 0xc0, 0x7c);
const DANGER: Color = Color::Rgb(0xfb, 0x49, 0x34);

pub struct StudioTerminalRenderer {
    prev: Option<TextSurface>,
}

impl StudioTerminalRenderer {
    pub fn new() -> Self {
        Self { prev: None }
    }

    pub fn render(
        &mut self,
        stdout: &mut impl std::io::Write,
        vm: &StudioViewModel,
        width: u16,
        height: u16,
    ) -> std::io::Result<()> {
        let surface = build_surface(vm, width as usize, height as usize);
        render_text::render_text_surface(stdout, &surface, &mut self.prev, 0, 0)
    }
}

fn build_surface(vm: &StudioViewModel, width: usize, height: usize) -> TextSurface {
    let width = width.max(1);
    let height = height.max(1);
    let mut surface = TextSurface::new(width, height);
    surface.fill(Style::new().fg(FG).bg(BG));

    let top_h = if vm.presentation.chrome.top_bar.visible && height >= 3 {
        draw_top_bar(&mut surface, vm);
        1
    } else {
        0
    };
    let bottom_h = if vm.presentation.chrome.status_bar.visible && height >= 3 {
        draw_status_bar(&mut surface, vm);
        1
    } else {
        0
    };
    let body_h = height.saturating_sub(top_h + bottom_h).max(1);
    let body = Rect::new(0, top_h as u16, width as u16, body_h as u16);
    let center = draw_docks(&mut surface, body, vm);

    if matches!(vm.presentation.frame.mode, StudioFrameModeVm::Home) && vm.workspace.is_home {
        draw_home(&mut surface, center, vm);
    } else {
        draw_layout_node(&mut surface, center, &vm.workspace.root);
    }

    for (index, notice) in vm.notices.iter().rev().take(2).enumerate() {
        let text = format!(" {} ", notice.message);
        let y = top_h + index;
        if y < height.saturating_sub(bottom_h) {
            surface.draw_text(
                2,
                y,
                &truncate(&text, width.saturating_sub(4)),
                tone_style(notice.tone),
            );
        }
    }

    if vm.launcher.open {
        draw_launcher(&mut surface, vm);
    }

    surface
}

fn draw_top_bar(surface: &mut TextSurface, vm: &StudioViewModel) {
    let text = format!(
        " {}  {}  {} ",
        vm.presentation.chrome.version_label,
        vm.presentation.chrome.top_bar.focused_title,
        vm.presentation.chrome.top_bar.layout_symbol
    );
    draw_bar(surface, 0, &text, ACCENT);
}

fn draw_status_bar(surface: &mut TextSurface, vm: &StudioViewModel) {
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
            let label = format!("{}:", letterspace(label));
            surface.draw_text(x, y, &truncate(&label, surface.width - x), style_muted());
            x = x.saturating_add(label.chars().count());
        }
        surface.draw_text(
            x,
            y,
            &truncate(&segment.value, surface.width.saturating_sub(x)),
            tone_style(segment.tone),
        );
        x = x.saturating_add(segment.value.chars().count() + 2);
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

fn draw_docks(surface: &mut TextSurface, body: Rect, vm: &StudioViewModel) -> Rect {
    let mut center = body;
    let docks = &vm.workspace.docks;

    if let Some(left) = &docks.left {
        let w = left.size.min(center.w.saturating_sub(8));
        if w > 0 {
            let rect = Rect::new(center.x, center.y, w, center.h);
            draw_dock_tile(surface, rect, left);
            center.x = center.x.saturating_add(w.saturating_add(1));
            center.w = center.w.saturating_sub(w.saturating_add(1));
        }
    }

    if let Some(right) = &docks.right {
        let w = right.size.min(center.w.saturating_sub(8));
        if w > 0 {
            let x = center.x.saturating_add(center.w.saturating_sub(w));
            let rect = Rect::new(x, center.y, w, center.h);
            draw_dock_tile(surface, rect, right);
            center.w = center.w.saturating_sub(w.saturating_add(1));
        }
    }

    if let Some(top) = &docks.top {
        let h = top.size.min(center.h.saturating_sub(6));
        if h > 0 {
            let rect = Rect::new(center.x, center.y, center.w, h);
            draw_dock_tile(surface, rect, top);
            center.y = center.y.saturating_add(h.saturating_add(1));
            center.h = center.h.saturating_sub(h.saturating_add(1));
        }
    }

    if let Some(bottom) = &docks.bottom {
        let h = bottom.size.min(center.h.saturating_sub(6));
        if h > 0 {
            let y = center.y.saturating_add(center.h.saturating_sub(h));
            let rect = Rect::new(center.x, y, center.w, h);
            draw_dock_tile(surface, rect, bottom);
            center.h = center.h.saturating_sub(h.saturating_add(1));
        }
    }

    center
}

fn draw_dock_tile(surface: &mut TextSurface, rect: Rect, dock: &StudioDockTileVm) {
    draw_shadow(surface, rect);
    fill_rect(surface, rect, Style::new().fg(FG).bg(PANEL));
    let x = rect.x as usize;
    let y = rect.y as usize;
    let w = rect.w as usize;
    let h = rect.h as usize;
    if w < 2 || h < 2 {
        return;
    }
    surface.draw_box(
        x,
        y,
        x + w - 1,
        y + h - 1,
        Border::Sharp,
        Style::new().fg(SHADOW).bg(PANEL),
    );
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
    draw_dock_view(surface, inner, &dock.view);
}

fn draw_dock_view(surface: &mut TextSurface, rect: Rect, view: &StudioDockViewVm) {
    if let StudioDockViewVm::Input(input) = view {
        draw_input_dock(surface, rect, input);
        return;
    }
    let lines = match view {
        StudioDockViewVm::Input(_) => unreachable!("input docks return above"),
        StudioDockViewVm::View(view) => match view {
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
                let mut lines = vec![title.clone()];
                push_timeline_lines(&mut lines, entries, rect.w as usize);
                lines
            }
            StudioViewVm::Placeholder { title, message } => {
                vec![title.clone(), message.clone()]
            }
            _ => vec!["unsupported dock view".to_string()],
        },
        StudioDockViewVm::Placeholder { message } => vec![message.clone()],
    };
    draw_lines(surface, rect, &lines);
}

fn draw_input_dock(surface: &mut TextSurface, rect: Rect, input: &StudioInputVm) {
    let width = rect.w as usize;
    let height = rect.h as usize;
    if width == 0 || height == 0 {
        return;
    }
    let route_style = Style::new().fg(FG).bg(PANEL_2).bold();
    fill_line(
        surface,
        rect.x as usize,
        rect.y as usize,
        width,
        route_style,
    );
    surface.draw_text(
        rect.x as usize,
        rect.y as usize,
        &truncate(&input.route_label, width),
        route_style,
    );

    if height < 2 {
        return;
    }
    let prompt_y = rect.y as usize + 1;
    let text = if input.text.is_empty() {
        input.placeholder.as_str()
    } else {
        input.text.as_str()
    };
    surface.draw_text(
        rect.x as usize,
        prompt_y,
        "$ ",
        Style::new().fg(ACCENT).bg(PANEL).bold(),
    );
    let text_x = rect.x as usize + 2;
    let visible = truncate(text, width.saturating_sub(2));
    let text_style = if input.text.is_empty() {
        style_muted()
    } else {
        style_fg()
    };
    surface.draw_text(text_x, prompt_y, &visible, text_style);
    let cursor_x = text_x
        + input
            .text
            .chars()
            .take(input.cursor)
            .count()
            .min(width.saturating_sub(3));
    surface.draw_char(cursor_x, prompt_y, ' ', Style::new().fg(PANEL).bg(ACCENT));

    if height >= 3 {
        surface.draw_text(
            rect.x as usize,
            rect.y as usize + 2,
            &truncate(&input.hint, width),
            style_muted(),
        );
    }
}

fn draw_layout_node(surface: &mut TextSurface, rect: Rect, node: &StudioLayoutNodeVm) {
    if rect.w == 0 || rect.h == 0 {
        return;
    }
    match node {
        StudioLayoutNodeVm::Tile {
            tile_id,
            focused,
            title,
            actions,
            view,
        } => draw_tile(surface, rect, tile_id, *focused, title, actions.len(), view),
        StudioLayoutNodeVm::Split {
            axis,
            ratio,
            first,
            second,
        } => match axis {
            ryeos_client_base::studio::view_model::StudioSplitAxisVm::Horizontal => {
                let first_w = ((rect.w as f32 * ratio).round() as u16)
                    .clamp(1, rect.w.saturating_sub(1).max(1));
                let gap = u16::from(rect.w > 4);
                let second_w = rect.w.saturating_sub(first_w + gap);
                draw_layout_node(surface, Rect::new(rect.x, rect.y, first_w, rect.h), first);
                if second_w > 0 {
                    draw_layout_node(
                        surface,
                        Rect::new(rect.x + first_w + gap, rect.y, second_w, rect.h),
                        second,
                    );
                }
            }
            ryeos_client_base::studio::view_model::StudioSplitAxisVm::Vertical => {
                let first_h = ((rect.h as f32 * ratio).round() as u16)
                    .clamp(1, rect.h.saturating_sub(1).max(1));
                let gap = u16::from(rect.h > 4);
                let second_h = rect.h.saturating_sub(first_h + gap);
                draw_layout_node(surface, Rect::new(rect.x, rect.y, rect.w, first_h), first);
                if second_h > 0 {
                    draw_layout_node(
                        surface,
                        Rect::new(rect.x, rect.y + first_h + gap, rect.w, second_h),
                        second,
                    );
                }
            }
        },
    }
}

fn draw_tile(
    surface: &mut TextSurface,
    rect: Rect,
    tile_id: &str,
    focused: bool,
    title: &str,
    action_count: usize,
    view: &StudioViewVm,
) {
    let w = rect.w as usize;
    let h = rect.h as usize;
    if w == 0 || h == 0 {
        return;
    }
    draw_shadow(surface, rect);
    let border_style = if focused {
        Style::new().fg(ACCENT).bg(PANEL)
    } else {
        Style::new().fg(SHADOW).bg(PANEL)
    };
    fill_rect(surface, rect, Style::new().fg(FG).bg(PANEL));
    if w >= 2 && h >= 2 {
        surface.draw_box(
            rect.x as usize,
            rect.y as usize,
            rect.x as usize + w - 1,
            rect.y as usize + h - 1,
            Border::Sharp,
            border_style,
        );
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
                let right_w = right.chars().count();
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
    draw_view(surface, inner, view);
}

fn draw_view(surface: &mut TextSurface, rect: Rect, view: &StudioViewVm) {
    if let StudioViewVm::Timeline { entries, .. } = view {
        draw_timeline(surface, rect, entries);
        return;
    }
    if let StudioViewVm::Rows { columns, rows, .. } = view {
        draw_rows(surface, rect, columns, rows);
        return;
    }
    let mut lines = Vec::new();
    match view {
        StudioViewVm::Rows { .. } => unreachable!("rows views return above"),
        StudioViewVm::Timeline { .. } => unreachable!("timeline views return above"),
        StudioViewVm::Map { scene } | StudioViewVm::Atlas { scene } => {
            lines.push(format!("scene objects: {}", scene.objects.len()));
            lines.push("terminal atlas renderer pending; use rows for interaction".into());
        }
        StudioViewVm::Placeholder { title, message } => {
            lines.push(title.clone());
            lines.push(message.clone());
        }
    }
    draw_lines(surface, rect, &lines);
}

fn draw_rows(surface: &mut TextSurface, rect: Rect, columns: &[String], rows: &[StudioRowVm]) {
    let width = rect.w as usize;
    let height = rect.h as usize;
    if width == 0 || height == 0 {
        return;
    }
    let mut y = rect.y as usize;
    let bottom = rect.y as usize + height;
    if !columns.is_empty() && y < bottom {
        let header = letterspace(&columns.join(" · "));
        surface.draw_text(rect.x as usize, y, &truncate(&header, width), style_muted());
        y += 1;
    }
    if rows.is_empty() && y < bottom {
        surface.draw_text(rect.x as usize, y, "no rows loaded", style_muted());
        return;
    }
    for row in rows.iter().take(bottom.saturating_sub(y)) {
        let style = if row.selected {
            style_selected()
        } else {
            style_fg()
        };
        fill_line(surface, rect.x as usize, y, width, style);
        let glyph_style = if row.selected {
            style
        } else {
            tone_style(row.tone)
        };
        surface.draw_text(rect.x as usize, y, tone_glyph(row.tone), glyph_style);

        let meta = row.meta.as_deref().unwrap_or_default();
        let meta_width = if meta.is_empty() {
            0
        } else {
            meta.chars().count().min(width / 3)
        };
        let primary_width = width.saturating_sub(3 + meta_width + usize::from(meta_width > 0));
        let mut primary = row.primary.clone();
        if let Some(secondary) = &row.secondary {
            primary.push_str("  ");
            primary.push_str(secondary);
        }
        surface.draw_text(
            rect.x as usize + 2,
            y,
            &truncate(&primary, primary_width),
            style,
        );
        if meta_width > 0 {
            let meta_text = truncate(meta, meta_width);
            let meta_x = rect.x as usize + width.saturating_sub(meta_text.chars().count());
            let meta_style = if row.selected { style } else { style_muted() };
            surface.draw_text(meta_x, y, &meta_text, meta_style);
        }
        y += 1;
    }
}

fn draw_timeline(surface: &mut TextSurface, rect: Rect, entries: &[StudioTimelineEntryVm]) {
    let width = rect.w as usize;
    let height = rect.h as usize;
    if width == 0 || height == 0 {
        return;
    }
    let mut lines = Vec::new();
    push_timeline_lines(&mut lines, entries, width);
    let visible = lines.len().min(height);
    let start = lines.len().saturating_sub(visible);
    for (row, line) in lines.iter().skip(start).take(visible).enumerate() {
        let style = if line.starts_with('─') {
            style_muted()
        } else if line.starts_with('✓') {
            tone_style(StudioTone::Good)
        } else if line.starts_with('✗') {
            tone_style(StudioTone::Danger)
        } else if line.starts_with('▸') {
            tone_style(StudioTone::Accent)
        } else {
            style_fg()
        };
        surface.draw_text(
            rect.x as usize,
            rect.y as usize + row,
            &truncate(line, width),
            style,
        );
    }
}

fn draw_home(surface: &mut TextSurface, rect: Rect, vm: &StudioViewModel) {
    let home = &vm.presentation.home;
    let w = rect.w as usize;
    let h = rect.h as usize;
    let tile_w = w.min(82).max(28);
    let tile_h = h.min(20).max(10);
    let x = rect.x as usize + w.saturating_sub(tile_w) / 2;
    let y = rect.y as usize + h.saturating_sub(tile_h) / 3;
    let panel = Rect::new(x as u16, y as u16, tile_w as u16, tile_h as u16);
    draw_shadow(surface, panel);
    fill_rect(surface, panel, Style::new().fg(FG).bg(PANEL));
    surface.draw_box(
        x,
        y,
        x + tile_w - 1,
        y + tile_h - 1,
        Border::Sharp,
        Style::new().fg(FG).bg(PANEL),
    );
    let inner_x = x + 2;
    let inner_w = tile_w.saturating_sub(4);
    draw_centered(
        surface,
        inner_x,
        y + 2,
        inner_w,
        &letterspace(&home.brand),
        Style::new().fg(WARN).bg(PANEL).bold(),
    );
    draw_centered(
        surface,
        inner_x,
        y + 4,
        inner_w,
        &home.tagline,
        style_muted(),
    );

    let description_y = y + 6;
    for (offset, line) in wrap_words(&home.description, inner_w)
        .into_iter()
        .take(3)
        .enumerate()
    {
        draw_centered(
            surface,
            inner_x,
            description_y + offset,
            inner_w,
            &line,
            style_fg(),
        );
    }

    let ticker = home
        .terminal_lines
        .get((vm.generation as usize).wrapping_div(24) % home.terminal_lines.len().max(1))
        .cloned()
        .unwrap_or_else(|| "content-addressed. tamper-evident. verified.".to_string());
    if tile_h > 12 {
        let ticker_y = y + tile_h.saturating_sub(7);
        fill_line(
            surface,
            inner_x,
            ticker_y,
            inner_w,
            Style::new().fg(FG).bg(PANEL_2),
        );
        surface.draw_text(
            inner_x,
            ticker_y,
            &truncate(&format!("› {ticker}"), inner_w),
            Style::new().fg(FG).bg(PANEL_2),
        );
    }

    let cta_y = y + tile_h.saturating_sub(5);
    if cta_y > y && cta_y < y + tile_h - 1 {
        draw_centered(
            surface,
            inner_x,
            cta_y,
            inner_w,
            &format!(
                "ENTER {}  ·  G {}",
                home.primary_label, home.secondary_label
            ),
            tone_style(StudioTone::Accent),
        );
    }

    let command_y = y + tile_h.saturating_sub(3);
    if command_y > y && command_y < y + tile_h - 1 {
        draw_centered(
            surface,
            inner_x,
            command_y,
            inner_w,
            &format!("$ {}", home.install_command),
            style_muted(),
        );
    }

    let status = letterspace("alt+k launcher · ctrl+c quit");
    surface.draw_text(
        inner_x,
        y + tile_h - 2,
        &truncate(&status, inner_w),
        style_muted(),
    );
}

fn draw_launcher(surface: &mut TextSurface, vm: &StudioViewModel) {
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

fn push_timeline_lines(lines: &mut Vec<String>, entries: &[StudioTimelineEntryVm], width: usize) {
    if entries.is_empty() {
        lines.push("no timeline events loaded".to_string());
        return;
    }
    for entry in entries {
        match entry {
            StudioTimelineEntryVm::Block { text, .. } => {
                for wrapped in wrap_words(text, width) {
                    lines.push(wrapped);
                }
                lines.push(String::new());
            }
            StudioTimelineEntryVm::Line { primary, meta, .. } => {
                lines.push(join_with_right_meta("•", primary, meta.as_deref(), width));
            }
            StudioTimelineEntryVm::Pair {
                summary,
                meta,
                tone,
                pending,
            } => {
                let glyph = if *pending {
                    "▸"
                } else if *tone == StudioTone::Danger {
                    "✗"
                } else {
                    "✓"
                };
                lines.push(join_with_right_meta(glyph, summary, meta.as_deref(), width));
            }
            StudioTimelineEntryVm::Separator { label } => {
                let label = format!(" {label} ");
                let rule_len = width.saturating_sub(label.chars().count());
                let left = rule_len / 2;
                let right = rule_len.saturating_sub(left);
                lines.push(format!(
                    "{}{}{}",
                    "─".repeat(left),
                    label,
                    "─".repeat(right)
                ));
            }
        }
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
}

fn join_with_right_meta(prefix: &str, primary: &str, meta: Option<&str>, width: usize) -> String {
    let left = format!("{prefix} {primary}");
    let Some(meta) = meta.filter(|m| !m.is_empty()) else {
        return left;
    };
    let left_w = left.chars().count();
    let meta_w = meta.chars().count();
    if left_w + meta_w + 2 >= width {
        format!("{left} · {meta}")
    } else {
        format!("{left}{}{}", " ".repeat(width - left_w - meta_w), meta)
    }
}

fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let line_w = line.chars().count();
        let word_w = word.chars().count();
        if line_w > 0 && line_w + 1 + word_w > width {
            out.push(line);
            line = String::new();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
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

fn draw_lines(surface: &mut TextSurface, rect: Rect, lines: &[String]) {
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

fn fill_rect(surface: &mut TextSurface, rect: Rect, style: Style) {
    let x = rect.x as usize;
    let y = rect.y as usize;
    let w = rect.w as usize;
    let h = rect.h as usize;
    if w == 0 || h == 0 {
        return;
    }
    surface.fill_rect(x, y, x + w - 1, y + h - 1, ' ', style);
}

fn fill_line(surface: &mut TextSurface, x: usize, y: usize, width: usize, style: Style) {
    if width == 0 {
        return;
    }
    surface.fill_rect(x, y, x + width - 1, y, ' ', style);
}

fn draw_shadow(surface: &mut TextSurface, rect: Rect) {
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

fn tone_style(tone: StudioTone) -> Style {
    match tone {
        StudioTone::Good => Style::new().fg(GOOD).bg(PANEL),
        StudioTone::Warn => Style::new().fg(WARN).bg(PANEL),
        StudioTone::Danger => Style::new().fg(DANGER).bg(PANEL),
        StudioTone::Accent => Style::new().fg(ACCENT).bg(PANEL),
        StudioTone::Neutral => style_fg(),
    }
}

fn tone_glyph(tone: StudioTone) -> &'static str {
    match tone {
        StudioTone::Good => "✓",
        StudioTone::Warn => "!",
        StudioTone::Danger => "✗",
        StudioTone::Accent => "›",
        StudioTone::Neutral => "•",
    }
}

fn draw_centered(
    surface: &mut TextSurface,
    x: usize,
    y: usize,
    width: usize,
    text: &str,
    style: Style,
) {
    if width == 0 {
        return;
    }
    let text = truncate(text, width);
    let text_w = text.chars().count();
    let offset = width.saturating_sub(text_w) / 2;
    surface.draw_text(x + offset, y, &text, style);
}

fn style_fg() -> Style {
    Style::new().fg(FG_SOFT).bg(PANEL)
}

fn style_muted() -> Style {
    Style::new().fg(MUTED).bg(PANEL)
}

fn style_selected() -> Style {
    Style::new().fg(FG).bg(ACCENT)
}

fn letterspace(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| [ch.to_ascii_uppercase(), ' '])
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn truncate(value: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    let mut text: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    text.push('…');
    text
}
