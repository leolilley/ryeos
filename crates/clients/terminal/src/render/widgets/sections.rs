//! The sections widget: a foldable multi-section list (the magit-style
//! status surface). Each section is a `▾/▸ Title (count)` header followed by
//! its rows, indented; a collapsed section shows only its header. Row drawing
//! mirrors the rows widget (tone glyph, primary+secondary, right-aligned meta,
//! full-width selection) one indent level deeper.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::studio::view_model::{StudioRowVm, StudioSectionVm};
use ryeos_client_base::text_surface::TextSurface;

use super::super::primitives::fill_line;
use super::super::text::{display_width, truncate};
use super::super::theme::{style_fg, style_muted, style_selected, tone_glyph, tone_style};

/// Rows sit two cells in from the section header so the fold glyph column
/// stays clear of content.
const ROW_INDENT: usize = 2;

pub fn draw_sections(surface: &mut TextSurface, rect: Rect, sections: &[StudioSectionVm]) {
    let width = rect.w as usize;
    let height = rect.h as usize;
    if width == 0 || height == 0 {
        return;
    }
    let left = rect.x as usize;
    let bottom = rect.y as usize + height;
    let mut y = rect.y as usize;

    if sections.is_empty() && y < bottom {
        surface.draw_text(left, y, "no sections loaded", style_muted());
        return;
    }

    for section in sections {
        if y >= bottom {
            break;
        }
        let glyph = if section.collapsed { "▸" } else { "▾" };
        let header = format!("{glyph} {} ({})", section.title, section.count);
        surface.draw_text(left, y, &truncate(&header, width), style_fg().bold());
        y += 1;

        if section.collapsed {
            continue;
        }
        for row in &section.rows {
            if y >= bottom {
                break;
            }
            draw_row(surface, left, y, width, row);
            y += 1;
        }
    }
}

/// One indented row, matching the rows widget's layout offset by `ROW_INDENT`.
fn draw_row(surface: &mut TextSurface, left: usize, y: usize, width: usize, row: &StudioRowVm) {
    let style = if row.selected {
        style_selected()
    } else {
        style_fg()
    };
    fill_line(surface, left, y, width, style);

    let glyph_style = if row.selected { style } else { tone_style(row.tone) };
    let glyph_x = left + ROW_INDENT;
    surface.draw_text(glyph_x, y, tone_glyph(row.tone), glyph_style);

    let inner = width.saturating_sub(ROW_INDENT);
    let meta = row.meta.as_deref().unwrap_or_default();
    let meta_width = if meta.is_empty() {
        0
    } else {
        display_width(meta).min(inner / 3)
    };
    let primary_width = inner.saturating_sub(3 + meta_width + usize::from(meta_width > 0));
    let mut primary = row.primary.clone();
    if let Some(secondary) = &row.secondary {
        primary.push_str("  ");
        primary.push_str(secondary);
    }
    surface.draw_text(glyph_x + 2, y, &truncate(&primary, primary_width), style);

    if meta_width > 0 {
        let meta_text = truncate(meta, meta_width);
        let meta_x = left + width.saturating_sub(display_width(&meta_text));
        let meta_style = if row.selected { style } else { style_muted() };
        surface.draw_text(meta_x, y, &meta_text, meta_style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_client_base::studio::view_model::StudioTone;

    fn row(primary: &str, meta: Option<&str>) -> StudioRowVm {
        StudioRowVm {
            id: primary.to_string(),
            primary: primary.to_string(),
            secondary: None,
            meta: meta.map(str::to_string),
            kind: None,
            action: None,
            tone: StudioTone::Neutral,
            selected: false,
        }
    }

    fn section(title: &str, collapsed: bool, rows: Vec<StudioRowVm>) -> StudioSectionVm {
        StudioSectionVm {
            title: title.to_string(),
            count: rows.len(),
            collapsed,
            rows,
        }
    }

    fn row_text(surface: &TextSurface, w: usize, y: usize) -> String {
        (0..w).map(|x| surface.get(x, y).rune).collect()
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

    #[test]
    fn header_carries_fold_glyph_title_and_count() {
        let (mut s, rect) = surface(40, 6);
        draw_sections(
            &mut s,
            rect,
            &[section(
                "Threads",
                false,
                vec![row("T-ab", Some("running"))],
            )],
        );
        let header = row_text(&s, 40, 0);
        assert!(header.starts_with('▾'), "expanded glyph leads: {header:?}");
        assert!(header.contains("Threads"), "title rendered: {header:?}");
        assert!(header.contains("(1)"), "count rendered: {header:?}");
        // The row sits on the next line, indented past the fold column.
        let body = row_text(&s, 40, 1);
        assert!(body.contains("T-ab"), "row primary rendered: {body:?}");
        assert!(body.contains("running"), "row meta rendered: {body:?}");
        assert!(body.starts_with("  "), "row is indented: {body:?}");
    }

    #[test]
    fn collapsed_section_shows_only_its_header() {
        let (mut s, rect) = surface(40, 6);
        draw_sections(
            &mut s,
            rect,
            &[section("Threads", true, vec![row("T-ab", None), row("T-cd", None)])],
        );
        let header = row_text(&s, 40, 0);
        assert!(header.starts_with('▸'), "collapsed glyph leads: {header:?}");
        // Count still reflects the hidden rows.
        assert!(header.contains('2'), "count survives collapse: {header:?}");
        // No row text leaked onto the line below the header.
        assert!(
            row_text(&s, 40, 1).trim().is_empty(),
            "collapsed rows are hidden: {:?}",
            row_text(&s, 40, 1)
        );
    }

    #[test]
    fn sections_stack_with_their_rows() {
        let (mut s, rect) = surface(40, 8);
        draw_sections(
            &mut s,
            rect,
            &[
                section("Threads", false, vec![row("T-ab", None)]),
                section("Bundles", false, vec![row("studio", None)]),
            ],
        );
        assert!(row_text(&s, 40, 0).contains("Threads"));
        assert!(row_text(&s, 40, 1).contains("T-ab"));
        assert!(row_text(&s, 40, 2).contains("Bundles"));
        assert!(row_text(&s, 40, 3).contains("studio"));
    }

    #[test]
    fn empty_view_reports_no_sections() {
        let (mut s, rect) = surface(40, 4);
        draw_sections(&mut s, rect, &[]);
        assert!(row_text(&s, 40, 0).contains("no sections loaded"));
    }
}
