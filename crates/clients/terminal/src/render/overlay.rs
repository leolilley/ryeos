//! The overlay as a full-screen list: the open Surfaces, what needs you and
//! what is running, then its keys. The core holds whether it is open and which
//! row is selected; this only draws it.

use ryeos_client_base::text_surface::{Style, TextSurface};
use ryeos_client_base::ui::view_model::{RyeOsOverlayRowVm, RyeOsViewModel};

use super::primitives::fill_line;
use super::text::{display_width, truncate};
use super::theme::{ACCENT, BG, FG, style_muted, style_selected, tone_style};

/// Draw the overlay over the whole surface.
pub fn draw_overlay(surface: &mut TextSurface, vm: &RyeOsViewModel) {
    let overlay = &vm.overlay;
    let now_ms = i64::try_from(vm.now_ms).unwrap_or(i64::MAX);
    let width = surface.width;
    let height = surface.height;
    for y in 0..height {
        fill_line(surface, 0, y, width, Style::new().fg(FG).bg(BG));
    }
    if width < 4 || height < 2 {
        return;
    }
    // The last line holds the keys; everything else fits above it.
    let last = height - 1;
    let mut y = 0;

    y = label(surface, y, "OPEN SURFACES", Some(vm.session.surfaces.len()));
    if y < last {
        let mut x = 2;
        for surface_vm in &vm.session.surfaces {
            let mut name = format!("{} {}", surface_vm.ordinal, surface_vm.title);
            if surface_vm.chip.authored_in_project {
                name.push('·');
            }
            let room = width.saturating_sub(x + 1);
            if room < 2 {
                break;
            }
            let style = if surface_vm.active {
                Style::new().fg(FG).bg(BG).reverse()
            } else {
                style_muted()
            };
            x += surface.draw_text(x, y, &truncate(&format!(" {name} "), room), style);
            x += 2;
        }
        y += 2;
    }

    let offset = overlay.needs_you.len();
    for (title, rows, count, base) in [
        ("NEEDS YOU", &overlay.needs_you, overlay.needs_you_count, 0),
        ("RUNNING", &overlay.running, overlay.running_count, offset),
    ] {
        if y >= last {
            break;
        }
        y = label(surface, y, title, count);
        if count.is_none() {
            if y < last {
                surface.draw_text(2, y, "unavailable", style_muted());
                y += 1;
            }
        } else if rows.is_empty() {
            if y < last {
                surface.draw_text(2, y, "nothing here", style_muted());
                y += 1;
            }
        } else {
            for (index, row) in rows.iter().enumerate() {
                if y >= last {
                    break;
                }
                let selected = overlay.selected == Some(base + index);
                draw_row(surface, y, row, selected, now_ms);
                y += 1;
            }
            if let Some(more) = count
                .filter(|count| *count > rows.len())
                .map(|count| count - rows.len())
                && y < last
            {
                surface.draw_text(4, y, &format!("{more} more"), style_muted());
                y += 1;
            }
        }
        y += 1;
    }

    let keys = overlay
        .key_hints
        .iter()
        .map(|hint| format!("[{}] {}", hint.keys, hint.word))
        .collect::<Vec<_>>()
        .join("  ");
    surface.draw_text(1, last, &truncate(&keys, width - 2), style_muted());
}

/// A section label and its count; returns the next line.
fn label(surface: &mut TextSurface, y: usize, title: &str, count: Option<usize>) -> usize {
    let width = surface.width;
    let x = 1 + surface.draw_text(
        1,
        y,
        &truncate(title, width.saturating_sub(2)),
        style_muted().bold(),
    );
    if let Some(count) = count
        && x + 2 < width
    {
        surface.draw_text(x + 1, y, &count.to_string(), Style::new().fg(ACCENT).bg(BG));
    }
    y + 1
}

/// One row: `>` when selected, the tone dot, the title, where, and at the
/// right the selected row's action keys or how long it has been.
fn draw_row(
    surface: &mut TextSurface,
    y: usize,
    row: &RyeOsOverlayRowVm,
    selected: bool,
    now_ms: i64,
) {
    let width = surface.width;
    let base = if selected {
        style_selected()
    } else {
        Style::new().fg(FG).bg(BG)
    };
    fill_line(surface, 0, y, width, base);
    surface.draw_text(1, y, if selected { ">" } else { " " }, base);
    surface.draw_text(3, y, "●", tone_style(row.tone));
    let right = if selected && !row.actions.is_empty() {
        row.actions
            .iter()
            .map(|action| format!("[{}] {}", action.keys, action.word))
            .collect::<Vec<_>>()
            .join("  ")
    } else {
        elapsed(now_ms.saturating_sub(row.since_ms))
    };
    let right_width = display_width(&right).min(width.saturating_sub(8));
    let right_x = width.saturating_sub(right_width + 1);
    let words = format!("{} — {}", row.title, row.where_);
    let room = right_x.saturating_sub(5 + 1);
    surface.draw_text(5, y, &truncate(&words, room), base);
    surface.draw_text(right_x, y, &truncate(&right, right_width), base);
}

/// How long since, in the largest whole unit: `now`, `5m`, `2h`, `3d`.
fn elapsed(ms: i64) -> String {
    let seconds = ms.max(0) / 1000;
    match seconds {
        0..60 => "now".to_owned(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}
