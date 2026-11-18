//! Diff rows as terminal lines, shared by a diff under a timeline entry and
//! a view whose whole body is one diff.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::text_surface::{Style, TextSurface};
use ryeos_client_base::ui::diff::{RyeOsDiffRowKind, RyeOsDiffVm};

use super::super::primitives::fill_line;
use super::super::text::truncate;
use super::super::theme::{PANEL, style_fg, style_muted, tone_style};

/// Columns a tab in diff text is drawn as.
const DIFF_TAB: &str = "    ";

/// A diff body as literal lines: per row its text, style and whether the
/// diff's cursor is on it. Line numbers take a gutter as wide as the largest
/// number drawn; a diff without numbers has no gutter. Marks are characters,
/// so the diff reads the same without colour.
pub(super) fn diff_lines(diff: &RyeOsDiffVm) -> Vec<(String, Style, bool)> {
    let gutter = diff
        .rows
        .iter()
        .flat_map(|row| [row.old_no, row.new_no])
        .flatten()
        .max()
        .map(|number| number.to_string().len());
    let number = |value: Option<usize>| match (gutter, value) {
        (Some(width), Some(value)) => format!("{value:>width$} "),
        (Some(width), None) => " ".repeat(width + 1),
        (None, _) => String::new(),
    };
    let mut lines = Vec::with_capacity(diff.rows.len() + 2);
    if let Some(above) = &diff.above {
        lines.push((format!("  ⋯ {above}"), style_muted(), false));
    }
    for (offset, row) in diff.rows.iter().enumerate() {
        let focus = diff.cursor == Some(diff.first_row + offset);
        let pointer = if focus { "› " } else { "  " };
        let disclosure = match row.collapsed {
            Some(true) => "▸ ",
            Some(false) => "▾ ",
            None => "",
        };
        let (body, style) = match row.kind {
            RyeOsDiffRowKind::Summary => {
                (format!("{disclosure}{}", row.text), tone_style(row.tone))
            }
            RyeOsDiffRowKind::File => (format!("{disclosure}{}", row.text), style_fg()),
            RyeOsDiffRowKind::Hunk => (format!("@@ {}", row.text), style_muted()),
            RyeOsDiffRowKind::Line => (
                format!(
                    "{}{}{}{}",
                    number(row.old_no),
                    number(row.new_no),
                    row.mark.unwrap_or(' '),
                    row.text.replace('\t', DIFF_TAB)
                ),
                tone_style(row.tone),
            ),
            RyeOsDiffRowKind::Fold | RyeOsDiffRowKind::More => {
                (format!("{disclosure}⋯ {}", row.text), style_muted())
            }
            RyeOsDiffRowKind::NotShown => (row.text.clone(), tone_style(row.tone)),
        };
        lines.push((format!("{pointer}{body}"), style, focus));
    }
    if let Some(below) = &diff.below {
        lines.push((format!("  ⋯ {below}"), style_muted(), false));
    }
    lines
}

/// Draws a view whose body is one diff: its rows top-down, scrolled so the
/// row under the cursor stays in view, that row filled like a selected one.
pub fn draw_diff(surface: &mut TextSurface, rect: Rect, diff: &RyeOsDiffVm) {
    let (width, height) = (rect.w as usize, rect.h as usize);
    if width == 0 || height == 0 {
        return;
    }
    let lines = diff_lines(diff);
    let start = lines
        .iter()
        .position(|(_, _, focus)| *focus)
        .map_or(0, |focus| focus.saturating_sub(height / 2))
        .min(lines.len().saturating_sub(height));
    for (row, (text, style, focus)) in lines.iter().skip(start).take(height).enumerate() {
        let y = rect.y as usize + row;
        let style = if *focus {
            fill_line(surface, rect.x as usize, y, width, style_fg().bg(PANEL));
            style.bg(PANEL)
        } else {
            *style
        };
        surface.draw_text(rect.x as usize, y, &truncate(text, width), style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> RyeOsDiffVm {
        let response = serde_json::json!({"state": "done", "blocks": [{
            "block": "changes", "basis": "recorded", "omitted_files": 0,
            "files": [{"path": "src/a.rs", "op": "modify", "added": 1, "removed": 1,
                       "binary": false,
                       "hunks": [{"old_start": 1, "new_start": 1,
                                  "lines": [" keep", "-old", "+new"]}]}]}]});
        let projections = serde_json::json!({"diff": {"words": {
            "basis.recorded": "Recorded", "file": "file", "op.modify": "modified"}}});
        ryeos_client_base::ui::diff::diff_vm_for_entry(&response, &projections).unwrap()
    }

    fn drawn(diff: &RyeOsDiffVm, height: u16) -> (TextSurface, Vec<String>) {
        let mut surface = TextSurface::new(40, usize::from(height));
        let rect = Rect {
            x: 0,
            y: 0,
            w: 40,
            h: height,
        };
        draw_diff(&mut surface, rect, diff);
        let rows = (0..usize::from(height))
            .map(|y| {
                (0..40)
                    .map(|x| surface.get(x, y).rune)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        (surface, rows)
    }

    #[test]
    fn a_diff_view_draws_its_rows_and_fills_the_cursor_row() {
        let mut diff = fixture();
        diff.cursor = Some(3);
        let (surface, rows) = drawn(&diff, 5);
        assert_eq!(
            rows,
            [
                "  ▾ Recorded · 1 file · +1 -1",
                "  ▾ src/a.rs · modified · +1 -1",
                "  1 1  keep",
                "› 2   -old",
                "    2 +new",
            ]
        );
        assert!((0..40).all(|x| surface.get(x, 3).bg == PANEL));
        assert!((0..40).any(|x| surface.get(x, 2).bg != PANEL));
        // A short tile scrolls to the cursor.
        let (_, rows) = drawn(&diff, 2);
        assert!(rows.iter().any(|row| row == "› 2   -old"), "{rows:?}");
    }
}
