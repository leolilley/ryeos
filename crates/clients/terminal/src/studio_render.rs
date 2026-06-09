//! Terminal renderer for the shared Studio view model.
//!
//! This is intentionally a renderer only: it consumes `StudioViewModel` and
//! emits a terminal `TextSurface`. Studio state, actions, and effects remain in
//! `ryeos-client-base` so terminal and web share the same product semantics.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::studio::view_model::{
    StudioCodeBlockVm, StudioDockTileVm, StudioDockViewVm, StudioFrameModeVm, StudioLayoutNodeVm,
    StudioRowVm, StudioSectionVm, StudioTone, StudioViewModel, StudioViewVm,
};
use ryeos_client_base::text_surface::{Border, Color, Style, TextSurface};

use crate::render_text;

const BG: Color = Color::Rgb(0x1d, 0x20, 0x21);
const PANEL: Color = Color::Rgb(0x28, 0x28, 0x28);
const PANEL_2: Color = Color::Rgb(0x32, 0x30, 0x2f);
const SHADOW: Color = Color::Rgb(0x50, 0x49, 0x45);
const FG: Color = Color::Rgb(0xeb, 0xdb, 0xb2);
const FG_SOFT: Color = Color::Rgb(0xd5, 0xc4, 0xa1);
const MUTED: Color = Color::Rgb(0xa8, 0x99, 0x84);
const ACCENT: Color = Color::Rgb(0xd6, 0x5d, 0x0e);
const WARN: Color = Color::Rgb(0xfa, 0xbd, 0x2f);
const GOOD: Color = Color::Rgb(0xb8, 0xbb, 0x26);
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
    let mut parts = Vec::new();
    for segment in &vm.presentation.chrome.status_bar.segments {
        let value = match &segment.label {
            Some(label) => format!("{label}:{}", segment.value),
            None => segment.value.clone(),
        };
        parts.push(value);
    }
    let text = format!(
        " {}  ·  {} ",
        parts.join("  "),
        vm.presentation.chrome.status_bar.key_hint
    );
    let y = surface.height.saturating_sub(1);
    draw_bar(surface, y, &text, MUTED);
}

fn draw_bar(surface: &mut TextSurface, y: usize, text: &str, fg: Color) {
    surface.fill_rect(0, y, surface.width, 1, ' ', Style::new().fg(fg).bg(PANEL));
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
    let lines = match view {
        StudioDockViewVm::Input(input) => {
            let text = if input.text.is_empty() {
                input.placeholder.as_str()
            } else {
                input.text.as_str()
            };
            vec![
                input.route_label.clone(),
                format!("$ {text}{}", if input.text.is_empty() { "" } else { "▎" }),
                input.hint.clone(),
            ]
        }
        StudioDockViewVm::Threads { title, hint, rows } => {
            let mut lines = vec![title.clone(), hint.clone()];
            for row in rows.iter().take(rect.h.saturating_sub(3) as usize) {
                lines.push(format!(
                    "▶ {} {}",
                    row.primary,
                    row.meta.clone().unwrap_or_default()
                ));
            }
            lines
        }
        StudioDockViewVm::Inspector { title, hint } => vec![title.clone(), hint.clone()],
        StudioDockViewVm::Placeholder { message } => vec![message.clone()],
    };
    draw_lines(surface, rect, &lines);
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
            surface.fill_rect(
                rect.x as usize + 1,
                rect.y as usize + 1,
                w.saturating_sub(2),
                1,
                ' ',
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
                Style::new().fg(WARN).bg(PANEL_2).bold()
            } else {
                Style::new().fg(MUTED).bg(PANEL_2)
            },
        );
    }
    let inner = if rect.h > 4 {
        Rect::new(
            rect.x + 1,
            rect.y + 3,
            rect.w.saturating_sub(2),
            rect.h.saturating_sub(4),
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
    let mut lines = Vec::new();
    match view {
        StudioViewVm::Overview { metrics, sections } => {
            lines.push("overview".to_string());
            lines.push(String::new());
            lines.extend(
                metrics
                    .iter()
                    .map(|metric| format!("{:>10}  {}", metric.label, metric.value)),
            );
            push_sections(&mut lines, sections);
        }
        StudioViewVm::ThreadList { rows }
        | StudioViewVm::Rows { rows, .. }
        | StudioViewVm::Items { rows, .. } => push_rows(&mut lines, rows),
        StudioViewVm::Files {
            root,
            path,
            rows,
            preview,
        } => {
            lines.push(format!("{root}:/{path}"));
            lines.push(String::new());
            push_rows(&mut lines, rows);
            if let Some(preview) = preview {
                lines.push(String::new());
                lines.push(format!("preview: {} · {}", preview.title, preview.hint));
                if let Some(content) = &preview.content {
                    lines.extend(content.lines().take(8).map(|line| format!("  {line}")));
                }
            }
        }
        StudioViewVm::Inspector(inspector) => {
            lines.push(inspector.title.clone());
            if let Some(subtitle) = &inspector.subtitle {
                lines.push(subtitle.clone());
            }
            push_sections(&mut lines, &inspector.sections);
            push_code_blocks(&mut lines, &inspector.code_blocks);
            if inspector.empty {
                lines.push(
                    inspector
                        .empty_message
                        .clone()
                        .unwrap_or_else(|| "empty".into()),
                );
            }
        }
        StudioViewVm::Thread {
            thread_id,
            sections,
            code_blocks,
        } => {
            lines.push(format!(
                "thread {}",
                thread_id.as_deref().unwrap_or("selection")
            ));
            push_sections(&mut lines, sections);
            push_code_blocks(&mut lines, code_blocks);
        }
        StudioViewVm::Map { scene } | StudioViewVm::Atlas { scene } => {
            lines.push(format!("scene objects: {}", scene.objects.len()));
            lines
                .push("terminal atlas renderer pending; use rows/inspector for interaction".into());
        }
        StudioViewVm::Gc {
            running,
            recent_events,
        } => {
            lines.push(format!("gc running: {running}"));
            lines.extend(recent_events.iter().take(20).map(|value| value.to_string()));
        }
        StudioViewVm::Placeholder { title, message } => {
            lines.push(title.clone());
            lines.push(message.clone());
        }
    }
    draw_lines(surface, rect, &lines);
}

fn draw_home(surface: &mut TextSurface, rect: Rect, vm: &StudioViewModel) {
    let home = &vm.presentation.home;
    let w = rect.w as usize;
    let h = rect.h as usize;
    let tile_w = w.min(74).max(24);
    let tile_h = h.min(18).max(8);
    let x = rect.x as usize + w.saturating_sub(tile_w) / 2;
    let y = rect.y as usize + h.saturating_sub(tile_h) / 2;
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
    let lines = vec![
        home.brand.clone(),
        home.tagline.clone(),
        String::new(),
        home.description.clone(),
        String::new(),
        "> content-addressed. tamper-evident. verified".to_string(),
        String::new(),
        format!(
            "[enter] {}    [g] {}",
            home.primary_label, home.secondary_label
        ),
        String::new(),
        format!("$ {}", home.install_command),
        "alt+k launcher · ctrl+c quit".to_string(),
    ];
    draw_lines(
        surface,
        Rect::new(
            (x + 2) as u16,
            (y + 1) as u16,
            tile_w.saturating_sub(4) as u16,
            tile_h.saturating_sub(2) as u16,
        ),
        &lines,
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

fn push_rows(lines: &mut Vec<String>, rows: &[StudioRowVm]) {
    if rows.is_empty() {
        lines.push("no rows loaded".to_string());
    }
    for row in rows {
        let marker = if row.selected { "›" } else { " " };
        let mut line = format!("{marker} {}", row.primary);
        if let Some(secondary) = &row.secondary {
            line.push_str(&format!("  {secondary}"));
        }
        if let Some(meta) = &row.meta {
            line.push_str(&format!("  · {meta}"));
        }
        lines.push(line);
    }
}

fn push_sections(lines: &mut Vec<String>, sections: &[StudioSectionVm]) {
    for section in sections {
        lines.push(String::new());
        lines.push(section.title.clone());
        for (key, value) in &section.rows {
            lines.push(format!("  {key}: {value}"));
        }
    }
}

fn push_code_blocks(lines: &mut Vec<String>, blocks: &[StudioCodeBlockVm]) {
    for block in blocks {
        lines.push(String::new());
        lines.push(format!(
            "{}{}",
            block.label,
            block
                .language
                .as_deref()
                .map(|l| format!(" ({l})"))
                .unwrap_or_default()
        ));
        lines.extend(
            block
                .content
                .lines()
                .take(24)
                .map(|line| format!("  {line}")),
        );
    }
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
    surface.fill_rect(
        rect.x as usize,
        rect.y as usize,
        rect.w as usize,
        rect.h as usize,
        ' ',
        style,
    );
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

fn style_fg() -> Style {
    Style::new().fg(FG_SOFT).bg(PANEL)
}

fn style_muted() -> Style {
    Style::new().fg(MUTED).bg(PANEL)
}

fn style_selected() -> Style {
    Style::new().fg(FG).bg(ACCENT)
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
