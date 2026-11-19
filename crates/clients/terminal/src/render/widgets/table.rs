//! The table widget: aligned cells under column headers, a leading tone
//! glyph, full-width selection. The typed list surface for non-chat lenses —
//! richer than the rows widget's primary/secondary/meta. Equal column widths
//! for now (weighted columns are a later refinement).
//!
//! A column's role (`ColumnRole`) is honoured only where a character grid can
//! show it: `count` is right-aligned, `id` and `ref` are cut in the middle,
//! `path` is cut from the left, `hash` is its first eight characters, and
//! `status` is a tone mark and the word in one cell, with no mark before the
//! first cell. Font, dimming and default width have no meaning here and are
//! ignored.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::text_surface::TextSurface;
use ryeos_client_base::ui::content::ColumnRole;
use ryeos_client_base::ui::view_model::{RyeOsTableCoverageVm, RyeOsTableRowVm, RyeOsTone};

use super::super::primitives::fill_line;
use super::super::text::{display_width, truncate};
use super::super::theme::{
    ACCENT, active_pulse_style, shimmer_style, style_fg, style_muted, style_selected, tone_glyph,
    tone_style,
};
use super::rows::draw_detail;

/// Cells sit two columns in, past the tone-glyph gutter.
const GUTTER: usize = 2;

/// The least height at which the coverage foot (rule and sentence) is drawn
/// and still leaves the header and a row.
const FOOT_MIN_HEIGHT: usize = 4;

/// Indent per level of a hierarchical table, in columns.
const INDENT: &str = "  ";

#[cfg(test)]
pub fn draw_table(
    surface: &mut TextSurface,
    rect: Rect,
    columns: &[String],
    rows: &[RyeOsTableRowVm],
    now_ms: u64,
) {
    draw_table_with_coverage(surface, rect, columns, rows, None, now_ms);
}

#[cfg(test)]
pub fn draw_table_with_coverage(
    surface: &mut TextSurface,
    rect: Rect,
    columns: &[String],
    rows: &[RyeOsTableRowVm],
    coverage: Option<&RyeOsTableCoverageVm>,
    now_ms: u64,
) {
    draw_table_with_roles(surface, rect, columns, &[], rows, coverage, now_ms);
}

pub fn draw_table_with_roles(
    surface: &mut TextSurface,
    rect: Rect,
    columns: &[String],
    roles: &[ColumnRole],
    rows: &[RyeOsTableRowVm],
    coverage: Option<&RyeOsTableCoverageVm>,
    now_ms: u64,
) {
    let width = rect.w as usize;
    let height = rect.h as usize;
    if width == 0 || height == 0 {
        return;
    }
    let left = rect.x as usize;
    let mut bottom = rect.y as usize + height;
    let mut y = rect.y as usize;
    // The coverage sentence sits at the foot, dim, above a rule.
    if let Some(coverage) = coverage
        && height >= FOOT_MIN_HEIGHT
    {
        let style = match coverage.tone {
            RyeOsTone::Neutral => style_muted(),
            tone => tone_style(tone),
        };
        bottom -= 1;
        surface.draw_text(left, bottom, &truncate(&coverage.label, width), style);
        bottom -= 1;
        surface.draw_text(left, bottom, &"─".repeat(width), style_muted());
    }

    // Column count drives the layout: prefer the declared headers, else fall
    // back to the widest row so cells still align when headers are absent.
    let ncols = columns
        .len()
        .max(rows.iter().map(|r| r.cells.len()).max().unwrap_or(0))
        .max(1);
    let col_w = width.saturating_sub(GUTTER) / ncols;
    // Each cell truncates one cell short of its column so neighbours don't touch.
    let cell_w = col_w.saturating_sub(1).max(1);
    let layout = RowLayout {
        left,
        width,
        ncols,
        col_w,
        cell_w,
        roles,
    };

    if !columns.is_empty() && y < bottom {
        for (i, header) in columns.iter().enumerate() {
            surface.draw_text(
                left + GUTTER + i * col_w,
                y,
                &truncate(&header.to_uppercase(), cell_w),
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
        surface.draw_text(left, y, "no rows in this page", style_muted());
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
            draw_row(surface, layout, y, row, now_ms);
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
            draw_row(surface, layout, y, row, now_ms);
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

/// Horizontal geometry shared by every row of one table draw: the row
/// band plus the derived column grid.
#[derive(Clone, Copy)]
struct RowLayout<'a> {
    left: usize,
    width: usize,
    ncols: usize,
    col_w: usize,
    cell_w: usize,
    roles: &'a [ColumnRole],
}

fn draw_row(
    surface: &mut TextSurface,
    layout: RowLayout<'_>,
    y: usize,
    row: &RyeOsTableRowVm,
    now_ms: u64,
) {
    let RowLayout {
        left,
        width,
        ncols,
        col_w,
        cell_w,
        roles,
    } = layout;
    let has_status = roles.contains(&ColumnRole::Status);
    let mut style = if row.selected {
        style_selected()
    } else {
        style_fg()
    };
    style = active_pulse_style(style, row.tone, now_ms);
    style = shimmer_style(
        style,
        row.changed_at_ms,
        row.changed_tone
            .map(|tone| tone_style(tone).fg)
            .unwrap_or(ACCENT),
        now_ms,
    );
    fill_line(surface, left, y, width, style);

    let glyph_style = if row.selected {
        style
    } else {
        tone_style(row.tone)
    };
    // A table with a status column carries the tone in that cell instead.
    let glyph = if row.expandable {
        Some(if row.expanded { "▾" } else { "▸" })
    } else if has_status {
        None
    } else {
        Some(tone_glyph(row.tone))
    };
    if let Some(glyph) = glyph {
        surface.draw_text(left, y, glyph, glyph_style);
    }

    for (i, cell) in row.cells.iter().enumerate().take(ncols) {
        // Cells take the body ink; a status word takes its tone's colour,
        // unless the whole row is selected.
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
        } else {
            style
        };
        let cell_x = left + GUTTER + i * col_w;
        let role = roles.get(i).copied().unwrap_or_default();
        if role == ColumnRole::Status {
            // The mark in the row's tone, then the word, in one cell.
            let tone = row
                .cell_tones
                .get(i)
                .copied()
                .flatten()
                .unwrap_or(row.tone);
            let mark_style = if row.selected {
                style
            } else {
                tone_style(tone)
            };
            surface.draw_text(cell_x, y, tone_glyph(tone), mark_style);
            if cell_w > 2 {
                surface.draw_text(cell_x + 2, y, &truncate(cell, cell_w - 2), cell_style);
            }
            continue;
        }
        if i == 0
            && let Some(tree) = &row.hierarchy
        {
            let prefix = hierarchy_prefix(tree);
            let prefix_width = display_width(&prefix).min(cell_w);
            surface.draw_text(cell_x, y, &truncate(&prefix, cell_w), cell_style);
            if prefix_width < cell_w {
                surface.draw_text(
                    cell_x + prefix_width,
                    y,
                    &truncate(cell, cell_w - prefix_width),
                    cell_style,
                );
            }
            continue;
        }
        let shown = role_text(role, cell, cell_w);
        let x = if role == ColumnRole::Count {
            cell_x + cell_w.saturating_sub(display_width(&shown))
        } else {
            cell_x
        };
        surface.draw_text(x, y, &shown, cell_style);
    }
}

/// What a cell shows for its role within `width` columns: a hash is its first
/// eight characters, an id or ref is cut in the middle, a path from the left.
fn role_text(role: ColumnRole, cell: &str, width: usize) -> String {
    match role {
        ColumnRole::Hash => truncate(&cell.chars().take(8).collect::<String>(), width),
        ColumnRole::Id | ColumnRole::Ref => cut_middle(cell, width),
        ColumnRole::Path => cut_left(cell, width),
        _ => truncate(cell, width),
    }
}

/// `value` cut to `width` columns by an ellipsis in the middle.
fn cut_middle(value: &str, width: usize) -> String {
    if display_width(value) <= width {
        return value.to_string();
    }
    if width < 3 {
        return truncate(value, width);
    }
    let chars: Vec<char> = value.chars().collect();
    let keep = width - 1;
    let head = keep.div_ceil(2).min(chars.len());
    let tail = (keep - head).min(chars.len() - head);
    let mut text: String = chars[..head].iter().collect();
    text.push('…');
    text.extend(chars[chars.len() - tail..].iter());
    text
}

/// `value` cut to `width` columns by an ellipsis at the start.
fn cut_left(value: &str, width: usize) -> String {
    if display_width(value) <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let chars: Vec<char> = value.chars().collect();
    let tail = (width - 1).min(chars.len());
    let mut text = String::from("…");
    text.extend(chars[chars.len() - tail..].iter());
    text
}

fn hierarchy_prefix(tree: &ryeos_client_base::ui::view_model::RyeOsTableHierarchyVm) -> String {
    let mut prefix = INDENT.repeat(tree.ancestor_continues.len());
    prefix.push_str(if tree.has_children {
        if tree.collapsed { "▸ " } else { "▾ " }
    } else {
        "  "
    });
    prefix
}

fn row_height(row: &RyeOsTableRowVm) -> usize {
    1 + row.detail.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_client_base::ui::view_model::{RyeOsRowDetailVm, RyeOsTableHierarchyVm, RyeOsTone};

    fn trow(tone: RyeOsTone, cells: &[&str]) -> RyeOsTableRowVm {
        RyeOsTableRowVm {
            id: cells.first().copied().unwrap_or_default().to_string(),
            cursor: 0,
            cells: cells.iter().map(|c| c.to_string()).collect(),
            hierarchy: None,
            cell_tones: Vec::new(),
            tone,
            intent: None,
            selected: false,
            expandable: false,
            expanded: false,
            detail: Vec::new(),
            changed_at_ms: None,
            changed_tone: None,
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
    fn table_coverage_is_visible_on_nonempty_page() {
        let (mut s, rect) = surface(72, 5);
        draw_table_with_coverage(
            &mut s,
            rect,
            &columns(),
            &[trow(RyeOsTone::Neutral, &["T-ab", "work", "running"])],
            Some(&RyeOsTableCoverageVm {
                state: "observed".into(),
                reason: "projection_locator_not_exhaustive".into(),
                label: "This list may not include everything.".into(),
                tone: RyeOsTone::Warn,
            }),
            0,
        );
        // The row sits under the header; the sentence is the last line, over a rule.
        assert!(row_text(&s, 72, 1).contains("T-ab"));
        assert!(row_text(&s, 72, 3).starts_with('─'));
        assert!(row_text(&s, 72, 4).contains("This list may not include everything."));
        assert!(!row_text(&s, 72, 4).contains("projection_locator"));
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
        let header = row_text(&s, 60, 0);
        let ncols = 3usize;
        let col_w = (60 - GUTTER) / ncols;
        let cell_x = |i: usize| GUTTER + i * col_w;
        assert!(from_col(&header, cell_x(0)).starts_with("THREAD"));
        assert!(from_col(&header, cell_x(1)).starts_with("ITEM"));
        assert!(from_col(&header, cell_x(2)).starts_with("STATUS"));

        // Each cell starts at the same x as its column header.
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
    fn hierarchy_prefix_indents_two_spaces_per_level_and_marks_fold_state() {
        assert_eq!(
            hierarchy_prefix(&RyeOsTableHierarchyVm {
                ancestor_continues: vec![true, false],
                is_last: false,
                has_children: true,
                collapsed: true,
            }),
            "    ▸ "
        );
        assert_eq!(
            hierarchy_prefix(&RyeOsTableHierarchyVm {
                ancestor_continues: vec![],
                is_last: true,
                has_children: false,
                collapsed: false,
            }),
            "  "
        );
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
        assert!(row_text(&s, 40, 1).contains("no rows in this page"));
    }

    fn draw_roled(
        width: usize,
        roles: &[ColumnRole],
        rows: &[RyeOsTableRowVm],
    ) -> (TextSurface, usize) {
        let (mut s, rect) = surface(width, 4);
        draw_table_with_roles(&mut s, rect, &columns(), roles, rows, None, 0);
        let col_w = (width - GUTTER) / 3;
        (s, col_w)
    }

    #[test]
    fn a_count_column_is_right_aligned_within_its_cell() {
        let roles = [ColumnRole::Text, ColumnRole::Text, ColumnRole::Count];
        let (s, col_w) = draw_roled(60, &roles, &[trow(RyeOsTone::Neutral, &["a", "b", "42"])]);
        let body = row_text(&s, 60, 1);
        let cell_x = GUTTER + 2 * col_w;
        let cell_w = col_w - 1;
        // The last figure sits at the cell's right edge.
        assert_eq!(from_col(&body, cell_x + cell_w - 2)[..2].to_string(), "42");
        assert_eq!(from_col(&body, cell_x).chars().next(), Some(' '));
    }

    #[test]
    fn a_hash_column_shows_eight_characters() {
        let roles = [ColumnRole::Hash, ColumnRole::Text, ColumnRole::Text];
        let hash = "0123456789abcdef0123";
        let (s, _) = draw_roled(60, &roles, &[trow(RyeOsTone::Neutral, &[hash, "b", "c"])]);
        let body = row_text(&s, 60, 1);
        assert!(from_col(&body, GUTTER).starts_with("01234567"));
        assert!(!body.contains("012345678"));
    }

    #[test]
    fn an_id_is_cut_in_the_middle_and_a_path_from_the_left() {
        assert_eq!(cut_middle("T-0123456789abcdef", 10), "T-012…cdef");
        assert_eq!(cut_middle("short", 10), "short");
        assert_eq!(cut_left("/home/leo/projects/x", 8), "…jects/x");
        assert_eq!(cut_left("/a", 8), "/a");
        assert_eq!(role_text(ColumnRole::Text, "abcdefghij", 5), "abcd…");
    }

    #[test]
    fn a_status_column_carries_the_mark_and_the_row_draws_none_before_it() {
        let roles = [ColumnRole::Id, ColumnRole::Text, ColumnRole::Status];
        let row = trow(RyeOsTone::Danger, &["T-ab", "ops", "failed"]);
        let (s, col_w) = draw_roled(60, &roles, &[row.clone()]);
        let body = row_text(&s, 60, 1);
        // Nothing before the first cell, then the mark and the word.
        assert_eq!(s.get(0, 1).rune, ' ');
        let status = from_col(&body, GUTTER + 2 * col_w);
        assert!(status.starts_with("✗ failed"), "{status:?}");
        // A table with no status column keeps the mark before the first cell.
        let (plain, _) = draw_roled(60, &[], &[row]);
        assert_eq!(plain.get(0, 1).rune, '✗');
    }
}
