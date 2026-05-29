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
        None => build_empty_thread(&mut surface, model, w, h),
        Some(t) => build_thread_content(&mut surface, t, model, w, h),
    }

    surface
}

fn build_empty_thread(surface: &mut TextSurface, model: &AppModel, w: usize, h: usize) {
    if let Some(inspection) = &model.store.thread_inspection {
        build_thread_inspection(surface, inspection, w, h);
        return;
    }

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
    model: &AppModel,
    w: usize,
    h: usize,
) {
    if let Some(inspection) = &model.store.thread_inspection {
        let daemon_id = thread.daemon_id.as_deref().unwrap_or_default();
        if inspection.thread_id == daemon_id || inspection.thread_id == thread.id.0.to_string() {
            build_thread_inspection(surface, inspection, w, h);
            return;
        }
    }

    let fg = Style::new().fg(theme::FG).bg(theme::BG);
    let dim = Style::new().fg(theme::FG_DIM).bg(theme::BG);
    let accent = Style::new().fg(theme::ACCENT).bg(theme::BG);
    let green = Style::new().fg(theme::GREEN).bg(theme::BG);
    let yellow = Style::new().fg(theme::YELLOW).bg(theme::BG);
    let red = Style::new().fg(theme::RED).bg(theme::BG);
    let blue = Style::new().fg(theme::BLUE).bg(theme::BG);
    let muted = Style::new().fg(theme::FG_MUTED).bg(theme::BG);
    let bg = theme::BG;

    // Get expanded turns from tile state
    let expanded_turns = model
        .workspace
        .tiles
        .get(&model.workspace.focused_tile)
        .and_then(|t| match &t.local {
            crate::workspace::ViewLocalState::Thread(state) => Some(state.expanded_turns.clone()),
            _ => None,
        })
        .unwrap_or_default();

    let mut row = 0;

    // Status header
    let (status_text, status_style) = match thread.status {
        ThreadStatus::Running => ("● running", yellow),
        ThreadStatus::Completed => ("✓ completed", green),
        ThreadStatus::Failed => ("✗ failed", red),
        ThreadStatus::Created => ("○ queued", muted),
        ThreadStatus::Cancelled => ("✗ cancelled", dim),
        ThreadStatus::TimedOut => ("⏱ timed out", yellow),
        _ => ("? unknown", dim),
    };

    let item_ref = thread.item_ref.as_deref().unwrap_or("thread");
    let header = format!("{} {}", status_text, item_ref);
    surface.draw_text(0, row, &header, status_style);

    // Usage on right side
    let usage = &thread.usage;
    let in_tok = crate::widgets::text::format_token_count(usage.input_tokens);
    let out_tok = crate::widgets::text::format_token_count(usage.output_tokens);
    let cost = crate::widgets::text::format_cost(usage.spend_usd);
    let usage_text = format!("{}in/{}out · {}", in_tok, out_tok, cost);
    if w > usage_text.len() + header.len() + 2 {
        surface.draw_text(
            w.saturating_sub(usage_text.len() + 1),
            row,
            &usage_text,
            dim,
        );
    }

    // Duration (shown in status bar area)
    if thread.started_at_ms.is_some() {
        // Duration info is already visible in the header
    }

    row += 1;

    // Separator
    if row < h {
        surface.draw_hline(0, row, w - 1, '─', dim);
        row += 1;
    }

    // Render thread parts
    let content_start = row;
    let content_height = h.saturating_sub(row);

    let mut lines: Vec<(String, Style)> = Vec::new();
    let mut turn_idx: u64 = 0;

    for part in &thread.parts {
        let is_expanded = expanded_turns.contains(&turn_idx);

        match part.kind {
            ThreadPartKind::UserMessage => {
                // User prompt with word wrap
                let wrapped = crate::widgets::text::word_wrap(&part.text, w.saturating_sub(4));
                lines.push(("▸ You".to_string(), accent));
                for line in &wrapped {
                    lines.push((format!("  {}", line), fg));
                }
                lines.push(("".to_string(), fg));
                turn_idx += 1;
            }
            ThreadPartKind::AssistantMessage => {
                // Assistant response with expand/collapse
                let text = if is_expanded {
                    part.text.clone()
                } else {
                    // Show first 3 lines collapsed
                    let all_lines: Vec<&str> = part.text.lines().collect();
                    if all_lines.len() <= 4 {
                        part.text.clone()
                    } else {
                        let mut preview: Vec<&str> = all_lines[..3].to_vec();
                        preview.push("  ... (Space to expand)");
                        preview.join("\n")
                    }
                };
                let wrapped = crate::widgets::text::word_wrap(&text, w.saturating_sub(2));
                for line in &wrapped {
                    lines.push((format!("  {}", line), fg));
                }
                lines.push(("".to_string(), fg));
                turn_idx += 1;
            }
            ThreadPartKind::Thinking => {
                if is_expanded {
                    let wrapped = crate::widgets::text::word_wrap(&part.text, w.saturating_sub(6));
                    lines.push(("  ◌ thinking".to_string(), muted));
                    for line in &wrapped {
                        lines.push((format!("    {}", line), dim));
                    }
                } else {
                    let preview = crate::widgets::text::truncate(
                        &part.text.replace('\n', " "),
                        w.saturating_sub(20),
                    );
                    lines.push((format!("  ◌ thinking: {}...", preview), muted));
                }
                turn_idx += 1;
            }
            ThreadPartKind::ToolCall => {
                let name = part.tool_name.as_deref().unwrap_or("tool");
                let expand_marker = if is_expanded { "▾" } else { "▸" };

                if is_expanded && !part.text.is_empty() {
                    let wrapped = crate::widgets::text::word_wrap(&part.text, w.saturating_sub(8));
                    lines.push((format!("  {} ⚒ {}", expand_marker, name), yellow));
                    for line in &wrapped {
                        lines.push((format!("      {}", line), dim));
                    }
                } else {
                    let args_preview = if part.text.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " {}",
                            crate::widgets::text::truncate(
                                &part.text.replace('\n', " "),
                                w.saturating_sub(name.len() + 15)
                            )
                        )
                    };
                    lines.push((
                        format!("  {} ⚒ {}{}", expand_marker, name, args_preview),
                        yellow,
                    ));
                }
                turn_idx += 1;
            }
            ThreadPartKind::ToolResult => {
                let name = part.tool_name.as_deref().unwrap_or("tool");
                let dur = part
                    .duration_ms
                    .map(crate::widgets::text::format_duration_ms)
                    .unwrap_or_default();

                if is_expanded && !part.text.is_empty() {
                    let wrapped = crate::widgets::text::word_wrap(&part.text, w.saturating_sub(6));
                    lines.push((format!("  ✓ {} {}", name, dur), green));
                    for line in &wrapped {
                        lines.push((format!("    {}", line), dim));
                    }
                } else {
                    lines.push((format!("  ✓ {} {}", name, dur), green));
                }
                turn_idx += 1;
            }
            ThreadPartKind::ChildThread => {
                lines.push((
                    format!("  ↳ {}", crate::widgets::text::truncate(&part.text, w - 6)),
                    accent,
                ));
                turn_idx += 1;
            }
            ThreadPartKind::System => {
                lines.push((
                    format!("  {}", crate::widgets::text::truncate(&part.text, w - 2)),
                    muted,
                ));
            }
            ThreadPartKind::Context => {
                lines.push((
                    format!("  ◎ {}", crate::widgets::text::truncate(&part.text, w - 6)),
                    blue,
                ));
            }
        }
    }

    // Streaming text (append)
    if !thread.streaming_text.is_empty() && thread.status == ThreadStatus::Running {
        let wrapped = crate::widgets::text::word_wrap(&thread.streaming_text, w.saturating_sub(2));
        for line in &wrapped {
            lines.push((format!("  {}", line), fg));
        }
        // Blinking cursor
        lines.push(("  ▎".to_string(), Style::new().fg(theme::ACCENT).bg(bg)));
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

    // Scroll indicator
    if scroll_offset > 0 {
        let indicator = format!("↑ {} more lines", scroll_offset);
        surface.draw_text(
            w.saturating_sub(indicator.len() + 1),
            content_start,
            &indicator,
            muted,
        );
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

fn build_thread_inspection(
    surface: &mut TextSurface,
    inspection: &crate::store::ThreadInspectionModel,
    w: usize,
    h: usize,
) {
    let header = Style::new().fg(theme::FG).bg(theme::BG).bold();
    let accent = Style::new().fg(theme::ACCENT).bg(theme::BG).bold();
    let dim = Style::new().fg(theme::FG_DIM).bg(theme::BG);
    let muted = Style::new().fg(theme::FG_MUTED).bg(theme::BG);

    let title = format!("{} {}", inspection.status, inspection.thread_id);
    surface.draw_text(0, 0, &truncate(&title, w), header);
    let mut row = 2;
    draw_kv(surface, w, row, "item", &inspection.item_ref, accent);
    row += 1;
    draw_kv(surface, w, row, "kind", &inspection.kind, dim);
    row += 1;
    draw_kv(surface, w, row, "created", &inspection.created_at, muted);
    row += 1;
    if let Some(started) = &inspection.started_at {
        draw_kv(surface, w, row, "started", started, muted);
        row += 1;
    }
    if let Some(finished) = &inspection.finished_at {
        draw_kv(surface, w, row, "finished", finished, muted);
        row += 1;
    }

    if row < h {
        row += 1;
        let summary = format!(
            "{} children · {} events · {} artifacts",
            inspection.children.len(),
            inspection.events.len(),
            inspection.artifacts.len()
        );
        surface.draw_text(0, row, &truncate(&summary, w), accent);
        row += 2;
    }

    if row < h {
        surface.draw_text(0, row, "Recent events", accent);
        row += 1;
    }
    for event in inspection.events.iter().rev().take(h.saturating_sub(row)) {
        let line = event
            .get("event_type")
            .or_else(|| event.get("event"))
            .or_else(|| event.get("kind"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| event.to_string());
        surface.draw_text(1, row, &truncate(&line, w.saturating_sub(2)), muted);
        row += 1;
    }

    if row + 2 < h {
        row += 1;
        surface.draw_text(0, row, "Result", accent);
        row += 1;
        let result = inspection
            .result
            .as_ref()
            .map(|value| serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()))
            .unwrap_or_else(|| "not available".into());
        for line in result.lines().take(h.saturating_sub(row)) {
            surface.draw_text(1, row, &truncate(line, w.saturating_sub(2)), dim);
            row += 1;
        }
    }
}

fn draw_kv(surface: &mut TextSurface, w: usize, row: usize, key: &str, value: &str, style: Style) {
    let key_style = Style::new().fg(theme::FG_MUTED).bg(theme::BG);
    let key_display = format!("  {:<8}", key);
    surface.draw_text(0, row, &truncate(&key_display, w), key_style);
    if w > key_display.len() {
        surface.draw_text(
            key_display.len(),
            row,
            &truncate(value, w.saturating_sub(key_display.len())),
            style,
        );
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    if max <= 1 {
        return "…".repeat(max);
    }
    let mut out: String = value.chars().take(max - 1).collect();
    out.push('…');
    out
}
