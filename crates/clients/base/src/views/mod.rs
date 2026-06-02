//! View builders — each ViewSpec maps to a view builder that produces
//! a TextSurface from model state.

pub mod event_inspector;
pub mod files;
pub mod graph;
pub mod item_inspector;
pub mod overview;
pub mod projects;
pub mod remotes;
pub mod schedules;
pub mod services;
pub mod space;
pub mod thread;
pub mod thread_list;
pub mod trust;

use crate::commands::format_keybind_display;
use crate::ids::TileId;
use crate::layout::Rect;
use crate::model::AppModel;
use crate::text_surface::{Border, Style, TextSurface};
use crate::theme;
use crate::workspace::ViewSpec;

/// Build a tile's text surface for the given view spec.
pub fn build_tile_view(
    model: &AppModel,
    tile_id: TileId,
    rect: Rect,
    focused: bool,
) -> TextSurface {
    let tile = match model.workspace.tiles.get(&tile_id) {
        Some(t) => t,
        None => return empty_surface(rect),
    };

    let inner_w = if rect.w > 2 { rect.w as usize - 2 } else { 1 };
    let inner_h = if rect.h > 2 { rect.h as usize - 2 } else { 1 };

    let mut surface = TextSurface::new(rect.w as usize, rect.h as usize);

    // Background fill
    surface.fill(Style::new().bg(theme::BG));

    // Border
    let border_style = if focused {
        theme::style_border_active()
    } else {
        theme::style_border()
    };
    surface.draw_box(
        0,
        0,
        rect.w as usize - 1,
        rect.h as usize - 1,
        Border::Rounded,
        border_style,
    );

    // Title
    let title = tile.view.title();
    let title_style = if focused {
        Style::new().fg(theme::ACCENT).bg(theme::BG).bold()
    } else {
        Style::new().fg(theme::FG_DIM).bg(theme::BG)
    };
    if inner_w > 4 {
        let title_display = if title.len() > inner_w - 2 {
            &title[..inner_w - 2]
        } else {
            &title
        };
        surface.draw_text(2, 0, title_display, title_style);
    }

    // Build inner content
    let content = match &tile.view {
        ViewSpec::ThreadList => thread_list::build(model, tile_id, inner_w, inner_h),
        ViewSpec::Thread { .. } => thread::build(model, tile_id, inner_w, inner_h),
        ViewSpec::Overview => overview::build(model, inner_w, inner_h),
        ViewSpec::Remotes => remotes::build(model, inner_w, inner_h),
        ViewSpec::Services => services::build(model, inner_w, inner_h),
        ViewSpec::ItemInspector => item_inspector::build(model, inner_w, inner_h),
        ViewSpec::Schedules => schedules::build(model, inner_w, inner_h),
        ViewSpec::GcStatus => schedules::build_gc(model, inner_w, inner_h),
        ViewSpec::Files => files::build(model, tile_id, inner_w, inner_h),
        ViewSpec::EventInspector => event_inspector::build(model, tile_id, inner_w, inner_h),
        ViewSpec::Projects => projects::build(model, inner_w, inner_h),
        ViewSpec::SpaceBrowser { .. } => space::build(model, tile_id, inner_w, inner_h),
        ViewSpec::Trust => trust::build(model, inner_w, inner_h),
        ViewSpec::Atlas => graph::build(model, inner_w, inner_h),
        ViewSpec::Graph { .. } => graph::build(model, inner_w, inner_h),
    };

    // Blit content inside border
    blit_surface(&mut surface, 1, 1, &content);

    surface
}

/// Build the input bar surface.
pub fn build_input_bar(model: &AppModel, rect: Rect) -> TextSurface {
    let mut surface = TextSurface::new(rect.w as usize, rect.h as usize);
    surface.fill(theme::style_input_bar());

    let _cap = model.workspace.focused_capability();
    let hint = model
        .workspace
        .focused_view()
        .map(|v| v.input_hint())
        .unwrap_or("input");

    // Draw hint
    let hint_text = format!(" {} ", hint);
    let hint_style = Style::new().fg(theme::BG_DARK).bg(theme::ACCENT);
    surface.draw_text(1, 0, &hint_text, hint_style);

    let hint_width = hint_text.chars().count();

    // Draw input text
    let text_style = Style::new().fg(theme::FG).bg(theme::BG_DARK);
    let max_text_w = rect.w as usize - hint_width - 3;
    let text = &model.workspace.input_bar.text;
    let display_text = if text.len() > max_text_w {
        &text[text.len().saturating_sub(max_text_w)..]
    } else {
        text.as_str()
    };
    surface.draw_text(hint_width + 2, 0, display_text, text_style);

    // Cursor indicator
    let cursor_x = hint_width + 2 + model.workspace.input_bar.cursor.min(display_text.len());
    if cursor_x < rect.w as usize {
        surface.draw_char(
            cursor_x,
            0,
            '▎',
            Style::new().fg(theme::ACCENT).bg(theme::BG_DARK),
        );
    }

    surface
}

/// Build overlay surfaces.
pub fn build_overlays(model: &AppModel, viewport: Rect) -> Vec<crate::frame::OverlaySurface> {
    use crate::frame::{OverlaySurface, OverlayType};
    use crate::model::OverlayState;

    let overlay = match &model.overlay {
        Some(o) => o,
        None => return Vec::new(),
    };

    match overlay {
        OverlayState::Help => {
            let w = 56.min(viewport.w as usize);
            let h = 32.min(viewport.h as usize);
            let mut surface = TextSurface::new(w, h);
            surface.fill(Style::new().bg(theme::BG));
            surface.draw_box(
                0,
                0,
                w - 1,
                h - 1,
                Border::Rounded,
                theme::style_border_active(),
            );

            // Build help lines from keymap
            let km = &model.keymap;
            let bind = |id: &str| -> String {
                km.get_binding(id)
                    .map(format_keybind_display)
                    .unwrap_or_else(|| "—".into())
            };

            let help_lines: Vec<(String, bool)> = vec![
                ("Rye OS TUI — Keybindings".into(), false),
                ("".into(), false),
                ("  Global:".into(), true),
                (
                    format!("  {:<14}Submit / open item", bind("nav.open")),
                    false,
                ),
                (
                    format!("  {:<14}Focus next tile", bind("focus.next")),
                    false,
                ),
                (
                    format!("  {:<14}Focus previous tile", bind("focus.prev")),
                    false,
                ),
                (
                    format!("  {:<14}Quit / clear input", bind("app.quit")),
                    false,
                ),
                (format!("  {:<14}New session", bind("session.new")), false),
                (format!("  {:<14}Command palette", bind("palette")), false),
                (format!("  {:<14}This help", bind("help")), false),
                ("".into(), false),
                ("  Tile management:".into(), true),
                (
                    format!("  {:<14}Split horizontal", bind("layout.split_h")),
                    false,
                ),
                (
                    format!("  {:<14}Split vertical", bind("layout.split_v")),
                    false,
                ),
                (
                    format!("  {:<14}Close focused tile", bind("layout.close")),
                    false,
                ),
                (format!("  {:<14}Reset layout", bind("layout.reset")), false),
                ("".into(), false),
                ("  Navigation:".into(), true),
                (format!("  {:<14}Cursor up", bind("nav.up")), false),
                (format!("  {:<14}Cursor down", bind("nav.down")), false),
                (
                    format!("  {:<14}Expand / collapse", bind("nav.toggle_expand")),
                    false,
                ),
                (
                    format!(
                        "  {:<14}Scroll by page",
                        bind("nav.page_up") + "/" + &bind("nav.page_down")
                    ),
                    false,
                ),
                ("".into(), false),
                ("  Input editing (hardcoded):".into(), true),
                ("  ←/→          Move cursor".into(), false),
                ("  Home/End     Jump to start/end".into(), false),
                ("  Backspace    Delete character".into(), false),
                ("  Delete       Delete forward".into(), false),
                ("".into(), false),
                ("  Press any key to close".into(), false),
            ];

            for (i, (line, is_header)) in help_lines.iter().enumerate() {
                if i + 1 < h - 1 {
                    let style = if i == 0 {
                        Style::new().fg(theme::ACCENT).bg(theme::BG).bold()
                    } else if *is_header {
                        Style::new().fg(theme::YELLOW).bg(theme::BG).bold()
                    } else {
                        Style::new().fg(theme::FG).bg(theme::BG)
                    };
                    surface.draw_text(2, i + 1, line, style);
                }
            }

            let x = (viewport.w as usize).saturating_sub(w) / 2;
            let y = (viewport.h as usize).saturating_sub(h) / 2;
            vec![OverlaySurface {
                rect: Rect::new(x as u16, y as u16, w as u16, h as u16),
                cells: surface,
                overlay_type: OverlayType::Help,
            }]
        }
        OverlayState::CommandPalette { query, selected } => {
            let w = 60.min(viewport.w as usize);
            let h = 16.min(viewport.h as usize);
            let mut surface = TextSurface::new(w, h);
            surface.fill(Style::new().bg(theme::BG));
            surface.draw_box(
                0,
                0,
                w - 1,
                h - 1,
                Border::Rounded,
                theme::style_border_active(),
            );

            surface.draw_text(
                2,
                1,
                "Command Palette",
                Style::new().fg(theme::ACCENT).bg(theme::BG).bold(),
            );

            // Query line
            let query_display = format!("> {}▎", query);
            surface.draw_text(
                2,
                3,
                &query_display,
                Style::new().fg(theme::FG).bg(theme::BG),
            );

            // Filter and display affordances
            let all_affordances = model.active_affordances();
            let matches = crate::commands::filter_affordances(&all_affordances, query);
            let max_visible = h.saturating_sub(5);

            // Scroll window: ensure selected item is visible
            let scroll_offset = if *selected >= max_visible {
                *selected - max_visible + 1
            } else {
                0
            };

            for (i, aff) in matches
                .iter()
                .skip(scroll_offset)
                .take(max_visible)
                .enumerate()
            {
                let is_selected = scroll_offset + i == *selected;
                let bg = if is_selected {
                    theme::ACCENT
                } else {
                    theme::BG
                };
                let fg = if is_selected {
                    theme::BG
                } else {
                    theme::FG_DIM
                };
                let style = Style::new().fg(fg).bg(bg);

                let label = crate::widgets::text::truncate(
                    &format!("{}: {}", aff.category, aff.label),
                    w - 6,
                );
                surface.draw_text(3, 5 + i, &label, style);

                // Description on right for selected item
                if is_selected && w > label.len() + 10 {
                    let desc = crate::widgets::text::truncate(
                        &aff.description,
                        w.saturating_sub(label.len() + 8),
                    );
                    surface.draw_text(
                        4 + label.len(),
                        5 + i,
                        &desc,
                        Style::new().fg(theme::FG_MUTED).bg(bg),
                    );
                }
            }

            // No results
            if matches.is_empty() {
                surface.draw_text(
                    4,
                    5,
                    "No matching commands",
                    Style::new().fg(theme::FG_MUTED).bg(theme::BG),
                );
            }

            let x = (viewport.w as usize).saturating_sub(w) / 2;
            let y = (viewport.h as usize).saturating_sub(h) / 2;
            vec![OverlaySurface {
                rect: Rect::new(x as u16, y as u16, w as u16, h as u16),
                cells: surface,
                overlay_type: OverlayType::CommandPalette,
            }]
        }
        OverlayState::Confirm { message, .. } => {
            let w = 50.min(viewport.w as usize);
            let h = 7.min(viewport.h as usize);
            let mut surface = TextSurface::new(w, h);
            surface.fill(Style::new().bg(theme::BG));
            surface.draw_box(
                0,
                0,
                w - 1,
                h - 1,
                Border::Rounded,
                theme::style_border_active(),
            );

            surface.draw_text(2, 2, message, Style::new().fg(theme::FG).bg(theme::BG));
            surface.draw_text(
                2,
                4,
                "[y] Yes  [n/Esc] No",
                Style::new().fg(theme::FG_DIM).bg(theme::BG),
            );

            let x = (viewport.w as usize).saturating_sub(w) / 2;
            let y = (viewport.h as usize).saturating_sub(h) / 2;
            vec![OverlaySurface {
                rect: Rect::new(x as u16, y as u16, w as u16, h as u16),
                cells: surface,
                overlay_type: OverlayType::Confirm,
            }]
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn empty_surface(rect: Rect) -> TextSurface {
    let mut s = TextSurface::new(rect.w.max(1) as usize, rect.h.max(1) as usize);
    s.fill(Style::new().bg(theme::BG));
    s
}

/// Blit a source surface onto a destination at (dst_x, dst_y).
fn blit_surface(dst: &mut TextSurface, dst_x: usize, dst_y: usize, src: &TextSurface) {
    for y in 0..src.height {
        for x in 0..src.width {
            let cell = src.get(x, y);
            if cell.rune != ' ' || !cell.bg.is_default() {
                dst.set(dst_x + x, dst_y + y, cell);
            }
        }
    }
}
