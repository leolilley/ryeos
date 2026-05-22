//! Thread view — streaming conversation with agent.

use crate::ids::TileId;
use crate::model::AppModel;
use crate::store::{ThreadPartKind, ThreadStatus};
use crate::text_surface::Style;
use crate::text_surface::TextSurface;
use crate::theme;

pub fn build(model: &AppModel, tile_id: TileId, w: usize, h: usize) -> TextSurface {
    let mut surface = TextSurface::new(w, h);
    surface.fill(Style::new().bg(theme::BG));

    if h == 0 || w == 0 {
        return surface;
    }

    // Find the thread associated with this tile
    let tile = model.workspace.tiles.get(&tile_id);
    let thread_id = tile.and_then(|t| match &t.view {
        crate::workspace::ViewSpec::Thread { thread_id } => *thread_id,
        _ => None,
    });

    let thread = thread_id.and_then(|id| model.store.threads.get(&id));

    match thread {
        None => build_empty_thread(&mut surface, w, h),
        Some(t) => build_thread_content(&mut surface, t, model, w, h),
    }

    surface
}

fn build_empty_thread(surface: &mut TextSurface, _w: usize, h: usize) {
    let style = Style::new().fg(theme::FG_MUTED).bg(theme::BG);
    let accent = Style::new().fg(theme::ACCENT).bg(theme::BG);

    if h < 4 {
        return;
    }

    let lines = [
        "",
        "  No active thread.",
        "",
        "  Type a prompt and press Enter to start.",
    ];

    let start_y = h.saturating_sub(lines.len()) / 2;
    for (i, line) in lines.iter().enumerate() {
        let s = if i == 0 || i == 2 { style } else { accent };
        surface.draw_text(0, start_y + i, line, s);
    }
}

fn build_thread_content(
    surface: &mut TextSurface,
    thread: &crate::store::ThreadModel,
    _model: &AppModel,
    w: usize,
    h: usize,
) {
    let fg = Style::new().fg(theme::FG).bg(theme::BG);
    let dim = Style::new().fg(theme::FG_DIM).bg(theme::BG);
    let accent = Style::new().fg(theme::ACCENT).bg(theme::BG);
    let green = Style::new().fg(theme::GREEN).bg(theme::BG);
    let yellow = Style::new().fg(theme::YELLOW).bg(theme::BG);
    let red = Style::new().fg(theme::RED).bg(theme::BG);
    let muted = Style::new().fg(theme::FG_MUTED).bg(theme::BG);

    let mut row = 0;

    // Status header
    let (status_text, status_style) = match thread.status {
        ThreadStatus::Running => ("running", yellow),
        ThreadStatus::Completed => ("completed", green),
        ThreadStatus::Failed => ("failed", red),
        ThreadStatus::Created => ("queued", muted),
        _ => ("unknown", dim),
    };

    let item_ref = thread.item_ref.as_deref().unwrap_or("thread");
    let header = format!("{} — {}", item_ref, status_text);
    surface.draw_text(0, row, &header, status_style);

    // Usage on right side
    let usage = &thread.usage;
    let usage_text = format!(
        "{}.{:.1}k tok · ${:.2}",
        usage.input_tokens / 1000,
        (usage.output_tokens as f64 / 1000.0),
        usage.spend_usd
    );
    if w > usage_text.len() + header.len() + 2 {
        surface.draw_text(
            w.saturating_sub(usage_text.len() + 1),
            row,
            &usage_text,
            dim,
        );
    }

    row += 1;

    // Separator
    if row < h {
        surface.draw_hline(0, row, w - 1, '─', dim);
        row += 1;
    }

    // Render thread parts (scroll from bottom)
    let parts = &thread.parts;
    let content_start = row;
    let content_height = h.saturating_sub(row);

    // Build lines from parts
    let mut lines: Vec<(String, Style)> = Vec::new();

    for part in parts {
        match part.kind {
            ThreadPartKind::UserMessage => {
                lines.push((format!("  You: {}", truncate(&part.text, w - 4)), accent));
            }
            ThreadPartKind::AssistantMessage => {
                for (i, line) in part.text.lines().enumerate() {
                    if i == 0 {
                        lines.push((format!("  {}", truncate(line, w - 2)), fg));
                    } else {
                        lines.push((format!("  {}", truncate(line, w - 2)), fg));
                    }
                }
            }
            ThreadPartKind::Thinking => {
                lines.push((
                    format!("  ◌ thinking... {}", truncate(&part.text, w - 20)),
                    muted,
                ));
            }
            ThreadPartKind::ToolCall => {
                let name = part.tool_name.as_deref().unwrap_or("tool");
                lines.push((
                    format!(
                        "  ⚒ {} {}",
                        name,
                        if part.text.is_empty() {
                            "".into()
                        } else {
                            truncate(&part.text, w - 20)
                        }
                    ),
                    yellow,
                ));
            }
            ThreadPartKind::ToolResult => {
                let name = part.tool_name.as_deref().unwrap_or("tool");
                let dur = part
                    .duration_ms
                    .map(|d| format!("{}ms", d))
                    .unwrap_or_default();
                lines.push((format!("  ✓ {} {}", name, dur), green));
            }
            ThreadPartKind::ChildThread => {
                lines.push((
                    format!("  ↳ child thread: {}", truncate(&part.text, w - 20)),
                    accent,
                ));
            }
            ThreadPartKind::System => {
                lines.push((format!("  {}", truncate(&part.text, w - 2)), muted));
            }
            ThreadPartKind::Context => {
                lines.push((
                    format!("  ◎ context: {}", truncate(&part.text, w - 15)),
                    dim,
                ));
            }
        }
    }

    // Streaming text (append)
    if !thread.streaming_text.is_empty() && thread.status == ThreadStatus::Running {
        for line in thread.streaming_text.lines() {
            lines.push((format!("  {}", truncate(line, w - 2)), fg));
        }
    }

    // Render visible lines (scroll to bottom)
    let scroll_offset = if lines.len() > content_height {
        lines.len() - content_height
    } else {
        0
    };

    for (i, (text, style)) in lines.iter().skip(scroll_offset).enumerate() {
        let y = content_start + i;
        if y >= h {
            break;
        }
        surface.draw_text(0, y, text, *style);
    }

    // Empty state
    if lines.is_empty() && content_height > 2 {
        let msg = "Waiting for output...";
        let x = w.saturating_sub(msg.len()) / 2;
        let y = content_start + content_height / 2;
        if y < h {
            surface.draw_text(x, y, msg, muted);
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}
