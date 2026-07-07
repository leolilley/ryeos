//! The timeline widget: coalesced entries from the engine fold render
//! as prose blocks, one-line pairs, and labeled separators. The
//! renderer shows the tail — a live conversation reads like a
//! conversation — and highlights the entry under the point, scrolling up
//! into history when the point walks back off the tail.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::text_surface::{Style, TextSurface};
use ryeos_client_base::ui::view_model::{RyeOsRowDetailVm, RyeOsTimelineEntryVm, RyeOsTone};

use super::super::primitives::fill_line;
use super::super::text::{display_width, join_with_right_meta, truncate, wrap_words};
use super::super::theme::{style_fg, style_muted, tone_glyph, tone_style, PANEL};

/// One rendered line plus the index of the entry it belongs to (None for
/// structural blanks and the empty-state line — they never take the point).
struct FeedLine {
    text: String,
    style: Style,
    entry: Option<usize>,
    /// Typeset inline markdown (`` `code` ``, `**bold**`) at draw time. Off for
    /// verbatim code lines and structural rows, where markers are literal.
    inline: bool,
    /// Call-tree indent, in columns, applied as a draw-time x-offset (not baked
    /// into `text`, so a per-keystroke re-render doesn't re-allocate the string).
    indent: usize,
}

impl FeedLine {
    /// A prose line — inline markdown is typeset when drawn.
    fn prose(text: String, style: Style, entry: Option<usize>) -> Self {
        Self {
            text,
            style,
            entry,
            inline: true,
            indent: 0,
        }
    }

    /// A line rendered literally (verbatim code, structural rows, blanks).
    fn plain(text: String, style: Style, entry: Option<usize>) -> Self {
        Self {
            text,
            style,
            entry,
            inline: false,
            indent: 0,
        }
    }
}

pub fn draw_timeline(
    surface: &mut TextSurface,
    rect: Rect,
    entries: &[RyeOsTimelineEntryVm],
    entry_indents: &[u8],
    selected: Option<usize>,
    entry_expandable: &[bool],
    entry_expanded: &[bool],
    entry_details: &[Vec<RyeOsRowDetailVm>],
) {
    let width = rect.w as usize;
    let height = rect.h as usize;
    if width == 0 || height == 0 {
        return;
    }
    let mut lines = Vec::new();
    push_timeline_lines(
        &mut lines,
        entries,
        entry_indents,
        width,
        entry_expandable,
        entry_expanded,
        entry_details,
    );

    let visible = lines.len().min(height);
    // Default bottom-anchored (the tail). Once the point is on an entry, use
    // the same midpoint scroll behavior as rows and tables.
    let mut start = lines.len().saturating_sub(visible);
    if let Some(sel) = selected {
        if let Some(first) = lines.iter().position(|line| line.entry == Some(sel)) {
            let last = lines
                .iter()
                .rposition(|line| line.entry == Some(sel))
                .unwrap_or(first);
            let selected_height = last.saturating_sub(first).saturating_add(1);
            let max_start = lines.len().saturating_sub(visible);
            start = if selected_height > visible {
                first
            } else {
                first
                    .saturating_sub(visible / 2)
                    .max(last.saturating_add(1).saturating_sub(visible))
            }
            .min(max_start);
        }
    }

    for (row, line) in lines.iter().skip(start).take(visible).enumerate() {
        let y = rect.y as usize + row;
        let highlighted = selected.is_some() && line.entry == selected;
        let style = if highlighted {
            // Subtle region highlight (magit-style): keep the tone fg, swap
            // the bg so the whole entry reads as "under the point".
            fill_line(surface, rect.x as usize, y, width, style_fg().bg(PANEL));
            line.style.bg(PANEL)
        } else {
            line.style
        };
        // Call-tree indent as a draw-time x-offset; the highlight fill above
        // still spans the full row.
        let x0 = rect.x as usize + line.indent.min(width.saturating_sub(1));
        let avail = width.saturating_sub(line.indent);
        if line.inline {
            draw_inline(surface, x0, y, avail, &line.text, style);
        } else {
            surface.draw_text(x0, y, &truncate(&line.text, avail), style);
        }
    }
}

fn push_timeline_lines(
    lines: &mut Vec<FeedLine>,
    entries: &[RyeOsTimelineEntryVm],
    entry_indents: &[u8],
    width: usize,
    entry_expandable: &[bool],
    entry_expanded: &[bool],
    entry_details: &[Vec<RyeOsRowDetailVm>],
) {
    if entries.is_empty() {
        lines.push(FeedLine::plain(
            "no timeline events loaded".to_string(),
            style_muted(),
            None,
        ));
        return;
    }
    for (index, entry) in entries.iter().enumerate() {
        // Call-tree indent: a graph node's tool calls and its directive fork
        // nest one level under the node. Prefix every line this entry emits with
        // two spaces per level, so the braid reads as a call tree. Panic-safe: a
        // missing/short indent vector degrades to depth 0 (flat).
        let depth = entry_indents.get(index).copied().unwrap_or(0) as usize;
        let indent_cols = (depth * 2).min(width.saturating_sub(1));
        let entry_width = width.saturating_sub(indent_cols).max(1);
        let first_line = lines.len();
        match entry {
            RyeOsTimelineEntryVm::Block { text, tone } => {
                // The braid stores the raw cognition prose; the lens typesets
                // it (block-level markdown). Inline spans are a later pass.
                push_markdown_block(lines, text, *tone, entry_width, index);
                // Padding between blocks — not part of the entry's point.
                lines.push(FeedLine::plain(String::new(), style_fg(), None));
            }
            RyeOsTimelineEntryVm::Line {
                primary,
                meta,
                tone,
                ..
            } => {
                lines.push(FeedLine::plain(
                    join_with_right_meta(tone_glyph(*tone), primary, meta.as_deref(), entry_width),
                    tone_style(*tone),
                    Some(index),
                ));
            }
            RyeOsTimelineEntryVm::Pair {
                summary,
                meta,
                tone,
                pending,
            } => {
                let glyph = if *pending {
                    "▸"
                } else if *tone == RyeOsTone::Danger {
                    "✗"
                } else {
                    "✓"
                };
                let style = if *pending {
                    tone_style(RyeOsTone::Accent)
                } else {
                    tone_style(*tone)
                };
                lines.push(FeedLine::plain(
                    join_with_right_meta(glyph, summary, meta.as_deref(), entry_width),
                    style,
                    Some(index),
                ));
            }
            RyeOsTimelineEntryVm::Separator { label } => {
                let label = format!(" {label} ");
                let rule_len = entry_width.saturating_sub(display_width(&label));
                let left = rule_len / 2;
                let right = rule_len.saturating_sub(left);
                lines.push(FeedLine::plain(
                    format!("{}{}{}", "─".repeat(left), label, "─".repeat(right)),
                    style_muted(),
                    Some(index),
                ));
            }
        }
        // Tag every line this entry emitted with its call-tree depth (two
        // columns per level), applied as a draw-time x-offset rather than baked
        // into the text — so a per-keystroke re-render never re-allocates the
        // line strings. Panic-safe: a missing indent degrades to flat (depth 0).
        if indent_cols > 0 {
            for line in &mut lines[first_line..] {
                line.indent = indent_cols;
            }
        }
        if entry_expandable.get(index).copied().unwrap_or(false)
            && entry_expanded.get(index).copied().unwrap_or(false)
        {
            for detail in entry_details.get(index).into_iter().flatten() {
                let label = if detail.field.is_empty() {
                    "detail".to_string()
                } else {
                    detail.field.clone()
                };
                let detail_indent = (indent_cols + 2).min(width.saturating_sub(1));
                let detail_width = width.saturating_sub(detail_indent).max(1);
                let body = format!("{label}: {}", detail.value);
                for (i, wrapped) in wrap_words(&body, detail_width.saturating_sub(2).max(1))
                    .into_iter()
                    .enumerate()
                {
                    let prefix = if i == 0 { "↳ " } else { "  " };
                    lines.push(FeedLine::plain(
                        format!("{prefix}{wrapped}"),
                        style_muted(),
                        Some(index),
                    ));
                    if let Some(line) = lines.last_mut() {
                        line.indent = detail_indent;
                    }
                }
            }
        }
    }
    while lines
        .last()
        .is_some_and(|line| line.text.is_empty() && line.entry.is_none())
    {
        lines.pop();
    }
}

/// Typeset a cognition prose block as light, block-level markdown:
/// - fenced ``` code blocks render verbatim (no reflow) on a panel bg so they
///   stand apart;
/// - ATX headings (`#`/`##`/`###`) render bold;
/// - bullet lists (`-`/`*`/`+`) get a glyph and a hanging indent;
/// - plain paragraphs reflow to width as before.
///
/// Prose/heading/bullet lines are marked `inline` so their inline markdown
/// (`` `code` ``, `**bold**`) is typeset at draw time (`draw_inline`); code
/// lines are verbatim. The braid keeps the raw text; this is purely the lens's
/// rendering of it.
fn push_markdown_block(
    lines: &mut Vec<FeedLine>,
    text: &str,
    tone: RyeOsTone,
    width: usize,
    index: usize,
) {
    let mut para = String::new();
    let mut in_code = false;
    for raw in text.lines() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            flush_paragraph(lines, &mut para, tone, width, index);
            in_code = !in_code;
            continue;
        }
        if in_code {
            // Verbatim, with a two-column gutter + panel bg as the code frame.
            lines.push(FeedLine::plain(
                format!("  {raw}"),
                style_muted().bg(PANEL),
                Some(index),
            ));
            continue;
        }
        if trimmed.is_empty() {
            flush_paragraph(lines, &mut para, tone, width, index);
            lines.push(FeedLine::plain(String::new(), style_fg(), Some(index)));
            continue;
        }
        if let Some(heading) = heading_text(trimmed) {
            flush_paragraph(lines, &mut para, tone, width, index);
            lines.push(FeedLine::prose(
                heading.to_string(),
                tone_style(tone).bold(),
                Some(index),
            ));
            continue;
        }
        if let Some(item) = bullet_item(trimmed) {
            flush_paragraph(lines, &mut para, tone, width, index);
            let inner = width.saturating_sub(2).max(1);
            for (i, wrapped) in wrap_words(item, inner).into_iter().enumerate() {
                let prefix = if i == 0 { "• " } else { "  " };
                lines.push(FeedLine::prose(
                    format!("{prefix}{wrapped}"),
                    tone_style(tone),
                    Some(index),
                ));
            }
            continue;
        }
        // Prose: accumulate soft-wrapped lines into one paragraph to reflow.
        if !para.is_empty() {
            para.push(' ');
        }
        para.push_str(trimmed);
    }
    flush_paragraph(lines, &mut para, tone, width, index);
}

/// Wrap the accumulated paragraph to `width` and clear it.
fn flush_paragraph(
    lines: &mut Vec<FeedLine>,
    para: &mut String,
    tone: RyeOsTone,
    width: usize,
    index: usize,
) {
    let trimmed = para.trim();
    if !trimmed.is_empty() {
        for wrapped in wrap_words(trimmed, width) {
            lines.push(FeedLine::prose(wrapped, tone_style(tone), Some(index)));
        }
    }
    para.clear();
}

/// The text of an ATX heading line (`# `/`## `/`### `), without the marker.
fn heading_text(line: &str) -> Option<&str> {
    for marker in ["### ", "## ", "# "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return Some(rest.trim());
        }
    }
    None
}

/// The text of a bullet-list item (`- `/`* `/`+ `), without the marker.
fn bullet_item(line: &str) -> Option<&str> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return Some(rest);
        }
    }
    None
}

/// Inline markdown emphasis recognized within a prose line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Emphasis {
    None,
    Code,
    Bold,
}

/// Split a prose line into styled runs at inline `` ` `` (code) and `**` (bold)
/// markers, stripping the markers. Non-nested; an unterminated marker styles the
/// rest of the line (graceful — inline markers rarely cross a wrap boundary).
fn parse_inline(text: &str) -> Vec<(String, Emphasis)> {
    let chars: Vec<char> = text.chars().collect();
    let mut runs: Vec<(String, Emphasis)> = Vec::new();
    let mut cur = String::new();
    let mut emph = Emphasis::None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '`' && emph != Emphasis::Bold {
            if !cur.is_empty() {
                runs.push((std::mem::take(&mut cur), emph));
            }
            emph = if emph == Emphasis::Code {
                Emphasis::None
            } else {
                Emphasis::Code
            };
            i += 1;
            continue;
        }
        if c == '*' && emph != Emphasis::Code && i + 1 < chars.len() && chars[i + 1] == '*' {
            if !cur.is_empty() {
                runs.push((std::mem::take(&mut cur), emph));
            }
            emph = if emph == Emphasis::Bold {
                Emphasis::None
            } else {
                Emphasis::Bold
            };
            i += 2;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    if !cur.is_empty() {
        runs.push((cur, emph));
    }
    runs
}

/// Draw a prose line, typesetting inline `` `code` `` (panel bg) and
/// `**bold**` runs and stripping their markers. Stops at `width`.
fn draw_inline(
    surface: &mut TextSurface,
    x0: usize,
    y: usize,
    width: usize,
    text: &str,
    base: Style,
) {
    let max_x = x0 + width;
    let mut x = x0;
    for (segment, emph) in parse_inline(text) {
        if x >= max_x {
            break;
        }
        let style = match emph {
            Emphasis::None => base,
            Emphasis::Code => base.bg(PANEL),
            Emphasis::Bold => base.bold(),
        };
        let shown = truncate(&segment, max_x - x);
        surface.draw_text(x, y, &shown, style);
        x += display_width(&shown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(primary: &str) -> RyeOsTimelineEntryVm {
        RyeOsTimelineEntryVm::Line {
            primary: primary.to_string(),
            meta: None,
            tone: RyeOsTone::Neutral,
            action: None,
            secondary_action: None,
        }
    }

    #[test]
    fn markdown_block_typesets_code_heading_and_bullets() {
        let mut lines = Vec::new();
        let text =
            "# Title\n\npara one\nwith soft wrap\n\n- first\n- second\n\n```\ncode  spaced\n```";
        push_markdown_block(&mut lines, text, RyeOsTone::Neutral, 60, 0);
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();

        // ATX heading: marker stripped, rendered bold.
        assert!(texts.iter().any(|t| *t == "Title"), "heading: {texts:?}");
        // Soft-wrapped prose reflows into one paragraph line.
        assert!(
            texts.iter().any(|t| *t == "para one with soft wrap"),
            "prose reflow: {texts:?}"
        );
        // Bullets get a glyph.
        assert!(texts.iter().any(|t| *t == "• first"), "bullet: {texts:?}");
        assert!(texts.iter().any(|t| *t == "• second"));
        // Fenced code renders verbatim (whitespace preserved) with a gutter,
        // and the ``` fences themselves are not emitted.
        assert!(
            texts.iter().any(|t| *t == "  code  spaced"),
            "verbatim code: {texts:?}"
        );
        assert!(!texts.iter().any(|t| t.contains("```")), "fences hidden");
    }

    #[test]
    fn parse_inline_splits_code_and_bold_runs_stripping_markers() {
        let runs = parse_inline("use `foo()` and **bold** text");
        assert_eq!(
            runs,
            vec![
                ("use ".to_string(), Emphasis::None),
                ("foo()".to_string(), Emphasis::Code),
                (" and ".to_string(), Emphasis::None),
                ("bold".to_string(), Emphasis::Bold),
                (" text".to_string(), Emphasis::None),
            ]
        );
    }

    #[test]
    fn parse_inline_unterminated_marker_styles_the_rest() {
        // No closing backtick: the tail renders as code (graceful), markers gone.
        let runs = parse_inline("see `here");
        assert_eq!(
            runs,
            vec![
                ("see ".to_string(), Emphasis::None),
                ("here".to_string(), Emphasis::Code),
            ]
        );
    }

    #[test]
    fn parse_inline_plain_text_is_one_none_run() {
        assert_eq!(
            parse_inline("just words"),
            vec![("just words".to_string(), Emphasis::None)]
        );
    }

    fn row_text(surface: &TextSurface, w: usize, y: usize) -> String {
        (0..w).map(|x| surface.get(x, y).rune).collect()
    }

    #[test]
    fn no_selection_tail_follows_newest() {
        let entries: Vec<_> = (0..10).map(|i| line(&format!("entry {i}"))).collect();
        let mut surface = TextSurface::new(20, 4);
        let rect = Rect {
            x: 0,
            y: 0,
            w: 20,
            h: 4,
        };
        draw_timeline(&mut surface, rect, &entries, &[], None, &[], &[], &[]);
        // The bottom row shows the newest entry — the feed tails.
        assert!(
            row_text(&surface, 20, 3).contains("entry 9"),
            "tail shows newest: {:?}",
            row_text(&surface, 20, 3)
        );
    }

    #[test]
    fn selected_entry_scrolls_into_view_and_highlights() {
        let entries: Vec<_> = (0..10).map(|i| line(&format!("entry {i}"))).collect();
        let mut surface = TextSurface::new(20, 4);
        let rect = Rect {
            x: 0,
            y: 0,
            w: 20,
            h: 4,
        };
        // Point on the oldest entry — far above the tail; the feed scrolls up.
        draw_timeline(&mut surface, rect, &entries, &[], Some(0), &[], &[], &[]);
        assert!(
            row_text(&surface, 20, 0).contains("entry 0"),
            "scrolled to reveal the selected oldest entry: {:?}",
            row_text(&surface, 20, 0)
        );
        // The selected row is highlighted (PANEL background across its width).
        assert!(
            (0..20).all(|x| surface.get(x, 0).bg == PANEL),
            "selected entry row is highlighted"
        );
        // A non-selected row is not.
        assert!(
            (0..20).any(|x| surface.get(x, 1).bg != PANEL),
            "non-selected rows are not highlighted"
        );
    }

    #[test]
    fn selected_entry_holds_midpoint_when_possible() {
        let entries: Vec<_> = (0..10).map(|i| line(&format!("entry {i}"))).collect();
        let mut surface = TextSurface::new(20, 5);
        let rect = Rect {
            x: 0,
            y: 0,
            w: 20,
            h: 5,
        };
        draw_timeline(&mut surface, rect, &entries, &[], Some(5), &[], &[], &[]);
        assert!(
            row_text(&surface, 20, 2).contains("entry 5"),
            "selected midpoint row: {:?}",
            row_text(&surface, 20, 2)
        );
    }
}
