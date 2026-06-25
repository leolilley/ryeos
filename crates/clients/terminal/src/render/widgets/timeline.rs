//! The timeline widget: coalesced entries from the engine fold render
//! as prose blocks, one-line pairs, and labeled separators. The
//! renderer shows the tail — a live conversation reads like a
//! conversation — and highlights the entry under the point, scrolling up
//! into history when the point walks back off the tail.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::studio::view_model::{StudioTimelineEntryVm, StudioTone};
use ryeos_client_base::text_surface::{Style, TextSurface};

use super::super::primitives::fill_line;
use super::super::text::{display_width, join_with_right_meta, truncate, wrap_words};
use super::super::theme::{style_fg, style_muted, tone_glyph, tone_style, PANEL};

/// One rendered line plus the index of the entry it belongs to (None for
/// structural blanks and the empty-state line — they never take the point).
struct FeedLine {
    text: String,
    style: Style,
    entry: Option<usize>,
}

pub fn draw_timeline(
    surface: &mut TextSurface,
    rect: Rect,
    entries: &[StudioTimelineEntryVm],
    selected: Option<usize>,
) {
    let width = rect.w as usize;
    let height = rect.h as usize;
    if width == 0 || height == 0 {
        return;
    }
    let mut lines = Vec::new();
    push_timeline_lines(&mut lines, entries, width);

    let visible = lines.len().min(height);
    // Default bottom-anchored (the tail). If the point sits above the
    // window, scroll up just enough to reveal its first line.
    let mut start = lines.len().saturating_sub(visible);
    if let Some(sel) = selected {
        if let Some(first) = lines.iter().position(|line| line.entry == Some(sel)) {
            if first < start {
                start = first;
            }
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
        surface.draw_text(rect.x as usize, y, &truncate(&line.text, width), style);
    }
}

fn push_timeline_lines(lines: &mut Vec<FeedLine>, entries: &[StudioTimelineEntryVm], width: usize) {
    if entries.is_empty() {
        lines.push(FeedLine {
            text: "no timeline events loaded".to_string(),
            style: style_muted(),
            entry: None,
        });
        return;
    }
    for (index, entry) in entries.iter().enumerate() {
        match entry {
            StudioTimelineEntryVm::Block { text, tone } => {
                // The braid stores the raw cognition prose; the lens typesets
                // it (block-level markdown). Inline spans are a later pass.
                push_markdown_block(lines, text, *tone, width, index);
                // Padding between blocks — not part of the entry's point.
                lines.push(FeedLine {
                    text: String::new(),
                    style: style_fg(),
                    entry: None,
                });
            }
            StudioTimelineEntryVm::Line {
                primary,
                meta,
                tone,
                ..
            } => {
                lines.push(FeedLine {
                    text: join_with_right_meta(tone_glyph(*tone), primary, meta.as_deref(), width),
                    style: tone_style(*tone),
                    entry: Some(index),
                });
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
                let style = if *pending {
                    tone_style(StudioTone::Accent)
                } else {
                    tone_style(*tone)
                };
                lines.push(FeedLine {
                    text: join_with_right_meta(glyph, summary, meta.as_deref(), width),
                    style,
                    entry: Some(index),
                });
            }
            StudioTimelineEntryVm::Separator { label } => {
                let label = format!(" {label} ");
                let rule_len = width.saturating_sub(display_width(&label));
                let left = rule_len / 2;
                let right = rule_len.saturating_sub(left);
                lines.push(FeedLine {
                    text: format!("{}{}{}", "─".repeat(left), label, "─".repeat(right)),
                    style: style_muted(),
                    entry: Some(index),
                });
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
/// Inline emphasis (`code`, **bold**) needs per-span cell styling and is a
/// later pass — this is block structure only. The braid keeps the raw text;
/// this is purely the lens's rendering of it.
fn push_markdown_block(
    lines: &mut Vec<FeedLine>,
    text: &str,
    tone: StudioTone,
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
            lines.push(FeedLine {
                text: format!("  {raw}"),
                style: style_muted().bg(PANEL),
                entry: Some(index),
            });
            continue;
        }
        if trimmed.is_empty() {
            flush_paragraph(lines, &mut para, tone, width, index);
            lines.push(FeedLine {
                text: String::new(),
                style: style_fg(),
                entry: Some(index),
            });
            continue;
        }
        if let Some(heading) = heading_text(trimmed) {
            flush_paragraph(lines, &mut para, tone, width, index);
            lines.push(FeedLine {
                text: heading.to_string(),
                style: tone_style(tone).bold(),
                entry: Some(index),
            });
            continue;
        }
        if let Some(item) = bullet_item(trimmed) {
            flush_paragraph(lines, &mut para, tone, width, index);
            let inner = width.saturating_sub(2).max(1);
            for (i, wrapped) in wrap_words(item, inner).into_iter().enumerate() {
                let prefix = if i == 0 { "• " } else { "  " };
                lines.push(FeedLine {
                    text: format!("{prefix}{wrapped}"),
                    style: tone_style(tone),
                    entry: Some(index),
                });
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
    tone: StudioTone,
    width: usize,
    index: usize,
) {
    let trimmed = para.trim();
    if !trimmed.is_empty() {
        for wrapped in wrap_words(trimmed, width) {
            lines.push(FeedLine {
                text: wrapped,
                style: tone_style(tone),
                entry: Some(index),
            });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn line(primary: &str) -> StudioTimelineEntryVm {
        StudioTimelineEntryVm::Line {
            primary: primary.to_string(),
            meta: None,
            tone: StudioTone::Neutral,
            action: None,
        }
    }

    #[test]
    fn markdown_block_typesets_code_heading_and_bullets() {
        let mut lines = Vec::new();
        let text = "# Title\n\npara one\nwith soft wrap\n\n- first\n- second\n\n```\ncode  spaced\n```";
        push_markdown_block(&mut lines, text, StudioTone::Neutral, 60, 0);
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
        draw_timeline(&mut surface, rect, &entries, None);
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
        draw_timeline(&mut surface, rect, &entries, Some(0));
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
}
