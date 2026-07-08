//! The table widget: aligned cells under column headers, a leading tone
//! glyph, full-width selection. The typed list surface for non-chat lenses —
//! richer than the rows widget's primary/secondary/meta. Equal column widths
//! for now (weighted/right-aligned columns are a later refinement).

use ryeos_client_base::layout::Rect;
use ryeos_client_base::text_surface::{Style, TextSurface};
use ryeos_client_base::ui::view_model::{RyeOsRowDetailVm, RyeOsTableRowVm, RyeOsTone};

use super::super::primitives::fill_line;
use super::super::text::{letterspace, truncate};
use super::super::theme::{
    ACCENT, mix_toward, style_fg, style_muted, style_selected, tone_glyph, tone_style,
};

/// Cells sit two columns in, past the tone-glyph gutter.
const GUTTER: usize = 2;

pub fn draw_table(
    surface: &mut TextSurface,
    rect: Rect,
    columns: &[String],
    rows: &[RyeOsTableRowVm],
    now_ms: u64,
) {
    let width = rect.w as usize;
    let height = rect.h as usize;
    if width == 0 || height == 0 {
        return;
    }
    let left = rect.x as usize;
    let bottom = rect.y as usize + height;
    let mut y = rect.y as usize;

    // Column count drives the layout: prefer the declared headers, else fall
    // back to the widest row so cells still align when headers are absent.
    let ncols = columns
        .len()
        .max(rows.iter().map(|r| r.cells.len()).max().unwrap_or(0))
        .max(1);
    let col_w = width.saturating_sub(GUTTER) / ncols;
    // Each cell truncates one cell short of its column so neighbours don't touch.
    let cell_w = col_w.saturating_sub(1).max(1);

    if !columns.is_empty() && y < bottom {
        for (i, header) in columns.iter().enumerate() {
            surface.draw_text(
                left + GUTTER + i * col_w,
                y,
                &truncate(&letterspace(header), cell_w),
                style_muted(),
            );
        }
        y += 1;
    }

    let rows_area = bottom.saturating_sub(y);
    if rows_area == 0 {
        return;
    }
    if rows.is_empty() {
        surface.draw_text(left, y, "no rows loaded", style_muted());
        return;
    }

    let selected_idx = rows.iter().position(|row| row.selected).unwrap_or(0);
    if rows.iter().all(|row| row.detail.is_empty()) {
        let max_offset = rows.len().saturating_sub(rows_area);
        let offset = selected_idx.saturating_sub(rows_area / 2).min(max_offset);
        for row in rows.iter().skip(offset) {
            if y >= bottom {
                break;
            }
            draw_row(surface, left, y, width, ncols, col_w, cell_w, row, now_ms);
            y += 1;
        }
        return;
    }

    // Scroll to keep the selected row on screen: no scroll while the cursor is
    // in the top half, then scroll so the cursor holds the halfway line —
    // clamped so the last page fills the view instead of trailing blank space.
    let selected_line = rows
        .iter()
        .take(selected_idx)
        .map(row_height)
        .sum::<usize>();
    let selected_height = rows.get(selected_idx).map(row_height).unwrap_or(1);
    let total_lines = rows.iter().map(row_height).sum::<usize>();
    let max_offset = total_lines.saturating_sub(rows_area);
    let centered_offset = selected_line.saturating_sub(rows_area / 2);
    let offset = if selected_height > rows_area {
        selected_line
    } else {
        centered_offset.max(
            selected_line
                .saturating_add(selected_height)
                .saturating_sub(rows_area),
        )
    }
    .min(max_offset);
    let mut skipped = 0usize;

    for row in rows {
        let height = row_height(row);
        if skipped + height <= offset {
            skipped += height;
            continue;
        }
        let line_skip = offset.saturating_sub(skipped).min(height.saturating_sub(1));
        if y >= bottom {
            break;
        }
        if line_skip == 0 {
            draw_row(surface, left, y, width, ncols, col_w, cell_w, row, now_ms);
            y += 1;
        }
        let detail_start = line_skip.saturating_sub(1);
        for detail in row.detail.iter().skip(detail_start) {
            if y >= bottom {
                break;
            }
            draw_detail(surface, left, y, width, detail);
            y += 1;
        }
        skipped += height;
    }
}

fn draw_row(
    surface: &mut TextSurface,
    left: usize,
    y: usize,
    width: usize,
    ncols: usize,
    col_w: usize,
    cell_w: usize,
    row: &RyeOsTableRowVm,
    now_ms: u64,
) {
    let mut style = if row.selected {
        style_selected()
    } else {
        style_fg()
    };
    style = active_pulse_style(style, row.tone, now_ms);
    style = shimmer_style(style, row.changed_at_ms, now_ms);
    fill_line(surface, left, y, width, style);

    let glyph_style = if row.selected {
        style
    } else {
        tone_style(row.tone)
    };
    let glyph = if row.expandable {
        if row.expanded { "▾" } else { "▸" }
    } else {
        tone_glyph(row.tone)
    };
    surface.draw_text(left, y, glyph, glyph_style);

    for (i, cell) in row.cells.iter().enumerate().take(ncols) {
        // First column is the identifier (foreground); later columns are
        // secondary detail (muted) — unless the whole row is selected or
        // the column projected its own tone for this cell.
        let cell_tone = row
            .cell_tones
            .get(i)
            .copied()
            .flatten()
            .filter(|tone| *tone != RyeOsTone::Neutral);
        let cell_style = if row.selected {
            style
        } else if let Some(tone) = cell_tone {
            tone_style(tone)
        } else if i == 0 {
            style
        } else {
            style_muted()
        };
        surface.draw_text(
            left + GUTTER + i * col_w,
            y,
            &truncate(cell, cell_w),
            cell_style,
        );
    }
}

fn row_height(row: &RyeOsTableRowVm) -> usize {
    1 + row.detail.len()
}

fn shimmer_style(style: Style, changed_at_ms: Option<u64>, now_ms: u64) -> Style {
    let Some(changed_at_ms) = changed_at_ms else {
        return style;
    };
    let age = now_ms.saturating_sub(changed_at_ms);
    if age >= 1_200 {
        return style;
    }
    let weight = 0.35 * (1.0 - age as f32 / 1_200.0);
    style.fg(mix_toward(style.fg, ACCENT, weight))
}

fn active_pulse_style(style: Style, tone: RyeOsTone, now_ms: u64) -> Style {
    if tone != RyeOsTone::Accent {
        return style;
    }
    let phase = (now_ms / 180) % 8;
    let wave = match phase {
        0 | 7 => 0.08,
        1 | 6 => 0.14,
        2 | 5 => 0.20,
        _ => 0.26,
    };
    style.fg(mix_toward(style.fg, ACCENT, wave))
}

fn draw_detail(
    surface: &mut TextSurface,
    left: usize,
    y: usize,
    width: usize,
    detail: &RyeOsRowDetailVm,
) {
    fill_line(surface, left, y, width, style_fg());
    let label = format!("  {}: ", detail.field);
    surface.draw_text(left, y, &truncate(&label, width), style_muted());
    let x = left + label.chars().count().min(width);
    if x < left + width {
        let style = detail.tone.map(tone_style).unwrap_or_else(style_fg);
        surface.draw_text(x, y, &truncate(&detail.value, left + width - x), style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_client_base::ui::view_model::RyeOsTone;

    fn trow(tone: RyeOsTone, cells: &[&str]) -> RyeOsTableRowVm {
        RyeOsTableRowVm {
            id: cells.first().copied().unwrap_or_default().to_string(),
            cells: cells.iter().map(|c| c.to_string()).collect(),
            cell_tones: Vec::new(),
            tone,
            action: None,
            selected: false,
            expandable: false,
            expanded: false,
            detail: Vec::new(),
            changed_at_ms: None,
            raw: serde_json::Value::Null,
        }
    }

    fn row_text(surface: &TextSurface, w: usize, y: usize) -> String {
        (0..w).map(|x| surface.get(x, y).rune).collect()
    }

    /// Slice a rendered row starting at display column `col`. Indexing must be
    /// by character, not byte: the tone-glyph gutter ('•') is one column but
    /// three bytes, so a byte offset would land mid-glyph.
    fn from_col(row: &str, col: usize) -> String {
        row.chars().skip(col).collect()
    }

    fn surface(w: usize, h: usize) -> (TextSurface, Rect) {
        (
            TextSurface::new(w, h),
            Rect {
                x: 0,
                y: 0,
                w: w as u16,
                h: h as u16,
            },
        )
    }

    fn columns() -> Vec<String> {
        ["thread", "item", "status"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn headers_and_cells_share_column_origins() {
        let (mut s, rect) = surface(60, 4);
        draw_table(
            &mut s,
            rect,
            &columns(),
            &[trow(RyeOsTone::Neutral, &["T-ab", "ops/base", "running"])],
            0,
        );
        // Headers render letterspaced/uppercased (house style); normalise to
        // compare against the declared column names.
        let header = row_text(&s, 60, 0);
        let flat = header.to_lowercase().replace(' ', "");
        assert!(flat.contains("thread"), "header rendered: {header:?}");
        assert!(flat.contains("item"), "header rendered: {header:?}");
        assert!(flat.contains("status"), "header rendered: {header:?}");

        // Each cell starts at the same x as its column header.
        let ncols = 3usize;
        let col_w = (60 - GUTTER) / ncols;
        let cell_x = |i: usize| GUTTER + i * col_w;
        let body = row_text(&s, 60, 1);
        assert!(
            from_col(&body, cell_x(0)).starts_with("T-ab"),
            "first cell aligned under its column: {body:?}"
        );
        assert!(
            from_col(&body, cell_x(1)).starts_with("ops/base"),
            "second cell aligned under its column: {body:?}"
        );
        assert!(
            from_col(&body, cell_x(2)).starts_with("running"),
            "third cell aligned under its column: {body:?}"
        );
    }

    #[test]
    fn rows_stack_below_the_header() {
        let (mut s, rect) = surface(60, 6);
        draw_table(
            &mut s,
            rect,
            &columns(),
            &[
                trow(RyeOsTone::Neutral, &["T-ab", "ops/base", "running"]),
                trow(RyeOsTone::Neutral, &["T-cd", "ops/scan", "completed"]),
            ],
            0,
        );
        assert!(row_text(&s, 60, 1).contains("T-ab"));
        assert!(row_text(&s, 60, 2).contains("T-cd"));
    }

    #[test]
    fn selected_row_fills_its_line() {
        use super::super::super::theme::ACCENT;
        let (mut s, rect) = surface(60, 4);
        let mut r = trow(RyeOsTone::Neutral, &["T-ab", "ops/base", "running"]);
        r.selected = true;
        draw_table(&mut s, rect, &columns(), &[r], 0);
        assert!(
            (0..60).all(|x| s.get(x, 1).bg == ACCENT),
            "selected row is highlighted full-width: {:?}",
            row_text(&s, 60, 1)
        );
    }

    #[test]
    fn columns_align_without_declared_headers() {
        // No headers: the layout falls back to the widest row so cells still
        // land on stable column origins.
        let (mut s, rect) = surface(60, 4);
        draw_table(
            &mut s,
            rect,
            &[],
            &[trow(RyeOsTone::Neutral, &["a", "b", "c"])],
            0,
        );
        let col_w = (60 - GUTTER) / 3;
        let body = row_text(&s, 60, 0);
        assert!(from_col(&body, GUTTER).starts_with('a'));
        assert!(from_col(&body, GUTTER + col_w).starts_with('b'));
        assert!(from_col(&body, GUTTER + 2 * col_w).starts_with('c'));
    }

    #[test]
    fn table_scrolls_to_keep_selected_visible_past_halfway() {
        let cols = vec!["thread".to_string()];
        let make = || {
            (0..20)
                .map(|i| trow(RyeOsTone::Neutral, &[format!("T-{i}").as_str()]))
                .collect::<Vec<_>>()
        };

        // Cursor in the top half → no scroll (row 0 still shown). Height 5 =
        // 1 header + 4 row lines, so the halfway line is 2 rows down.
        let (mut s, rect) = surface(40, 5);
        let mut rows = make();
        rows[1].selected = true;
        draw_table(&mut s, rect, &cols, &rows, 0);
        let top: String = (1..5)
            .map(|y| row_text(&s, 40, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            top.contains("T-0"),
            "top-half cursor doesn't scroll: {top:?}"
        );

        // Cursor past halfway → scrolls: early rows gone, selected on screen.
        let (mut s, rect) = surface(40, 5);
        let mut rows = make();
        rows[10].selected = true;
        draw_table(&mut s, rect, &cols, &rows, 0);
        let body: String = (1..5)
            .map(|y| row_text(&s, 40, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            body.contains("T-10"),
            "selected row visible after scroll: {body:?}"
        );
        assert!(!body.contains("T-0"), "early rows scrolled off: {body:?}");
    }

    #[test]
    fn table_scrolls_by_lines_when_expanded_row_precedes_selection() {
        let cols = vec!["thread".to_string()];
        let mut rows = (0..20)
            .map(|i| trow(RyeOsTone::Neutral, &[format!("T-{i}").as_str()]))
            .collect::<Vec<_>>();
        rows[2].expanded = true;
        rows[2].detail = (0..6)
            .map(|i| RyeOsRowDetailVm {
                field: format!("field_{i}"),
                value: format!("value_{i}"),
                tone: None,
            })
            .collect();
        rows[10].selected = true;

        let (mut s, rect) = surface(40, 6);
        draw_table(&mut s, rect, &cols, &rows, 0);
        let body = (1..6)
            .map(|y| row_text(&s, 40, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            body.contains("T-10"),
            "selected row remains visible despite expanded row above it: {body:?}"
        );
    }

    #[test]
    fn cell_tone_overrides_the_muted_secondary_style() {
        let (mut s, rect) = surface(60, 4);
        let mut toned = trow(RyeOsTone::Neutral, &["T-ab", "suspended", "running"]);
        toned.cell_tones = vec![None, Some(RyeOsTone::Warn), None];
        draw_table(&mut s, rect, &columns(), &[toned], 0);

        let (mut plain_s, plain_rect) = surface(60, 4);
        draw_table(
            &mut plain_s,
            plain_rect,
            &columns(),
            &[trow(RyeOsTone::Neutral, &["T-ab", "suspended", "running"])],
            0,
        );

        let col_w = (60 - GUTTER) / 3;
        let toned_cell = s.get(GUTTER + col_w, 1);
        let plain_cell = plain_s.get(GUTTER + col_w, 1);
        assert_eq!(toned_cell.rune, plain_cell.rune, "same cell text");
        assert_ne!(
            toned_cell.fg, plain_cell.fg,
            "a column-toned cell renders distinctly from the muted default"
        );
        // Untouched columns keep the default styling.
        assert_eq!(s.get(GUTTER, 1).fg, plain_s.get(GUTTER, 1).fg);
    }

    #[test]
    fn empty_view_reports_no_rows() {
        let (mut s, rect) = surface(40, 4);
        draw_table(&mut s, rect, &columns(), &[], 0);
        // The "no rows" notice sits on the line below the header.
        assert!(row_text(&s, 40, 1).contains("no rows loaded"));
    }
}
