//! The timeline widget: coalesced entries from the engine fold render
//! as prose blocks, one-line pairs, and labeled separators. The
//! renderer shows the tail — a live conversation reads like a
//! conversation.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::studio::view_model::{StudioTimelineEntryVm, StudioTone};
use ryeos_client_base::text_surface::{Style, TextSurface};

use super::super::text::{display_width, join_with_right_meta, truncate, wrap_words};
use super::super::theme::{style_fg, style_muted, tone_glyph, tone_style};

pub fn draw_timeline(surface: &mut TextSurface, rect: Rect, entries: &[StudioTimelineEntryVm]) {
    let width = rect.w as usize;
    let height = rect.h as usize;
    if width == 0 || height == 0 {
        return;
    }
    let mut lines = Vec::new();
    push_timeline_lines(&mut lines, entries, width);
    let visible = lines.len().min(height);
    let start = lines.len().saturating_sub(visible);
    for (row, (line, style)) in lines.iter().skip(start).take(visible).enumerate() {
        surface.draw_text(
            rect.x as usize,
            rect.y as usize + row,
            &truncate(line, width),
            *style,
        );
    }
}

fn push_timeline_lines(
    lines: &mut Vec<(String, Style)>,
    entries: &[StudioTimelineEntryVm],
    width: usize,
) {
    if entries.is_empty() {
        lines.push(("no timeline events loaded".to_string(), style_muted()));
        return;
    }
    for entry in entries {
        match entry {
            StudioTimelineEntryVm::Block { text, tone } => {
                for wrapped in wrap_words(text, width) {
                    lines.push((wrapped, tone_style(*tone)));
                }
                lines.push((String::new(), style_fg()));
            }
            StudioTimelineEntryVm::Line {
                primary,
                meta,
                tone,
            } => {
                lines.push((
                    join_with_right_meta(tone_glyph(*tone), primary, meta.as_deref(), width),
                    tone_style(*tone),
                ));
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
                lines.push((
                    join_with_right_meta(glyph, summary, meta.as_deref(), width),
                    style,
                ));
            }
            StudioTimelineEntryVm::Separator { label } => {
                let label = format!(" {label} ");
                let rule_len = width.saturating_sub(display_width(&label));
                let left = rule_len / 2;
                let right = rule_len.saturating_sub(left);
                lines.push((
                    format!("{}{}{}", "─".repeat(left), label, "─".repeat(right)),
                    style_muted(),
                ));
            }
        }
    }
    while lines.last().is_some_and(|(line, _)| line.is_empty()) {
        lines.pop();
    }
}
