//! Tile and frame chrome: bars, tile frames with title,
//! corner marks, and the provenance/affordance footer. Chrome carries
//! truth (provenance, focus) — it is load-bearing, not decoration.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::text_surface::{Border, Style, TextSurface};
use ryeos_client_base::ui::event::RyeOsTransportFreshness;
use ryeos_client_base::ui::view_model::{RyeOsInputVm, RyeOsTone, RyeOsViewModel, RyeOsViewVm};

use super::input::draw_input_tile;
use super::primitives::{fill_line, fill_rect};
use super::text::{display_width, truncate};
use super::theme::{ACCENT, BG, FG, MUTED, WARN, mix_toward, style_fg, style_muted, tone_style};

/// The Surfaces switcher as the top line: each open Surface by its number and
/// name, the current one reversed, then `+`. With no open Surface the
/// session's title stands in.
pub fn draw_switcher(surface: &mut TextSurface, y: usize, vm: &RyeOsViewModel) {
    fill_line(surface, 0, y, surface.width, Style::new().fg(FG).bg(BG));
    // With the status line hidden, a lone dot at the right of this line keeps
    // the node's health visible, as the Rune client's corner dot does.
    let limit = if !vm.presentation.chrome.status_bar.visible && surface.width >= 4 {
        let at = surface.width - 2;
        surface.draw_text(at, y, "●", tone_style(node_tone(vm)));
        at
    } else {
        surface.width
    };
    let surfaces = &vm.session.surfaces;
    if surfaces.is_empty() {
        surface.draw_text(
            1,
            y,
            &truncate(&vm.session.title, limit.saturating_sub(1)),
            style_muted(),
        );
        return;
    }
    let entries: Vec<(String, bool)> = surfaces
        .iter()
        .map(|choice| {
            (
                format!(" {} {} ", choice.ordinal, choice.title),
                choice.active,
            )
        })
        .collect();
    draw_strip(surface, y, limit, &entries);
}

/// The view-set tabs as one line: bare words, the current one reversed, then
/// `+`. When they do not fit, the line starts at the current tab so it stays
/// in view.
pub fn draw_tab_band(surface: &mut TextSurface, y: usize, vm: &RyeOsViewModel) {
    fill_line(surface, 0, y, surface.width, Style::new().fg(FG).bg(BG));
    let entries: Vec<(String, bool)> = vm
        .presentation
        .chrome
        .top_bar
        .tabs
        .iter()
        .map(|tab| (format!(" {} ", tab.title), tab.active))
        .collect();
    draw_strip(surface, y, surface.width, &entries);
}

/// One strip of entries, a space between them and `+` after the last, drawn
/// left of column `limit`. The `+` keeps its columns; when the entries overflow
/// what is left, the strip starts at the current entry.
fn draw_strip(surface: &mut TextSurface, y: usize, limit: usize, entries: &[(String, bool)]) {
    const GAP: usize = 1;
    const PLUS: &str = " + ";
    let limit = limit.min(surface.width);
    let end = limit.saturating_sub(display_width(PLUS));
    let full = entries
        .iter()
        .map(|(label, _)| display_width(label) + GAP)
        .sum::<usize>();
    let start = if full < end {
        0
    } else {
        entries.iter().position(|(_, active)| *active).unwrap_or(0)
    };
    let mut x = 1usize;
    for (label, active) in entries.iter().skip(start) {
        // The gap after an entry stays inside `end`, so the `+` always has
        // its columns after the last entry drawn.
        let available = end.saturating_sub(x + GAP);
        if available < 2 {
            break;
        }
        let width = display_width(label);
        if !*active && width > available {
            break;
        }
        let style = if *active {
            Style::new().fg(FG).bg(BG).reverse()
        } else {
            style_muted()
        };
        x += surface.draw_text(x, y, &truncate(label, available), style) + GAP;
    }
    if x + display_width(PLUS) <= limit {
        surface.draw_text(x, y, PLUS, style_muted());
    }
}

/// The node's tone as the bars show it: a node the client cannot reach is
/// danger, a feed still connecting or catching up is warn.
fn node_tone(vm: &RyeOsViewModel) -> RyeOsTone {
    match vm.transport.freshness {
        RyeOsTransportFreshness::Current => vm.chrome.health_tone,
        RyeOsTransportFreshness::Reconnecting | RyeOsTransportFreshness::ExpiredOrRevoked => {
            RyeOsTone::Danger
        }
        RyeOsTransportFreshness::Connecting | RyeOsTransportFreshness::GapResnapshotRequired => {
            RyeOsTone::Warn
        }
    }
}

/// The bottom line: the keys at the left, then the status parts separated by
/// ` | ` at the right, ending with the node. No product name.
pub fn draw_status_bar(surface: &mut TextSurface, vm: &RyeOsViewModel) {
    let y = surface.height.saturating_sub(1);
    let model = &vm.presentation.chrome.status_bar;
    let energy = model.energy.clamp(0.0, 1.0);
    let mut bg = mix_toward(BG, ACCENT, 0.12 * energy);
    if model.attention.is_some() {
        bg = mix_toward(bg, WARN, 0.18);
    }
    fill_line(surface, 0, y, surface.width, Style::new().fg(MUTED).bg(bg));

    // The status parts, right-aligned: each segment, then the node.
    let mut parts: Vec<(Option<String>, String, RyeOsTone)> = model
        .segments
        .iter()
        .map(|segment| {
            (
                segment.label.clone().filter(|label| !label.is_empty()),
                segment.value.clone(),
                segment.tone,
            )
        })
        .collect();
    parts.push((None, format!("● {}", vm.chrome.health_label), node_tone(vm)));
    let part_width = |(label, value, _): &(Option<String>, String, RyeOsTone)| {
        label.as_ref().map_or(0, |label| display_width(label) + 1) + display_width(value)
    };
    const SEP: &str = " | ";
    // The node always shows; other parts drop from the left when the line is
    // short, and the keys take what is left.
    let budget = surface.width.saturating_sub(2);
    // The node takes at most half the line, so the keys always keep room.
    let mut shown = 1;
    let mut width = part_width(&parts[parts.len() - 1]).min(budget / 2);
    for part in parts.iter().rev().skip(1) {
        let wanted = width + display_width(SEP) + part_width(part);
        if wanted > budget / 2 {
            break;
        }
        width = wanted;
        shown += 1;
    }
    let width = width.min(budget);
    let status_start = surface.width.saturating_sub(width + 1);

    // The keys: as much of the hint as fits before the status.
    let keys_end = status_start.saturating_sub(2);
    if keys_end > 1 {
        surface.draw_text(
            1,
            y,
            &truncate(&model.key_hint, keys_end - 1),
            style_muted().bg(bg),
        );
    }

    let mut x = status_start;
    for (index, (label, value, tone)) in parts.iter().skip(parts.len() - shown).enumerate() {
        if index > 0 {
            x += surface.draw_text(x, y, SEP, style_muted().bg(bg));
        }
        let room = surface.width.saturating_sub(x + 1);
        if let Some(label) = label {
            x += surface.draw_text(
                x,
                y,
                &truncate(&format!("{label} "), room),
                style_muted().bg(bg),
            );
        }
        let room = surface.width.saturating_sub(x + 1);
        surface.draw_text(x, y, &truncate(value, room), tone_style(*tone).bg(bg));
        x += display_width(value).min(room);
    }
}

pub fn draw_tile(
    surface: &mut TextSurface,
    rect: Rect,
    _instance_key: &ryeos_client_base::ids::RyeOsViewInstanceKey,
    _tile_id: &str,
    focused: bool,
    title: &str,
    _intent_count: usize,
    view: &RyeOsViewVm,
    input: Option<&RyeOsInputVm>,
    border: Option<Border>,
    now_ms: u64,
) {
    let w = rect.w as usize;
    let h = rect.h as usize;
    if w == 0 || h == 0 {
        return;
    }
    let x = rect.x as usize;
    let y = rect.y as usize;
    // The tile reads like the input box: a bordered frame on the page
    // background, with its label in the TOP border and provenance in the
    // BOTTOM border — no separate title bar, rule, corner marks, tile id, or
    // intent count. Border weight/colour carries focus.
    let border_style = if focused {
        Style::new().fg(ACCENT).bg(BG)
    } else {
        Style::new().fg(MUTED).bg(BG)
    };
    fill_rect(surface, rect, Style::new().fg(FG).bg(BG));
    if w < 2 || h < 2 {
        super::draw_view(surface, rect, view, now_ms);
        return;
    }
    if let Some(border) = border {
        surface.draw_box(x, y, x + w - 1, y + h - 1, border, border_style);
    }
    // Label in the top border (authored title only).
    if w > 6 {
        let label = format!(" {title} ");
        let title_style = if focused {
            Style::new().fg(ACCENT).bg(BG).bold()
        } else {
            style_fg().bold()
        };
        surface.draw_text(
            x + 2,
            y,
            &truncate(&label, w.saturating_sub(4)),
            title_style,
        );
    }
    // Provenance (left) + affordance hints (right) in the bottom border.
    if w > 6
        && let Some((provenance, affordances)) = view_chrome(view)
    {
        let by = y + h - 1;
        surface.draw_text(
            x + 2,
            by,
            &truncate(&format!(" {provenance} "), w.saturating_sub(4)),
            style_muted(),
        );
        if !affordances.is_empty() && w > 16 {
            let right = format!(" {} ", affordances.join(" · "));
            let right = truncate(&right, w / 2);
            let right_w = display_width(&right);
            surface.draw_text(x + w.saturating_sub(right_w + 2), by, &right, style_muted());
        }
    }
    // Content fills the interior between the top and bottom borders.
    let inner = Rect::new(
        rect.x + 1,
        rect.y + 1,
        rect.w.saturating_sub(2),
        rect.h.saturating_sub(2),
    );
    // A live-filter input composes ABOVE its widget: a one-row filter strip at
    // the top, the widget (e.g. the thread table) filling the rest. Typing
    // narrows; the table still shows and Enter opens the selected row.
    if let Some(input) = input.filter(|i| i.live_filter) {
        if inner.h >= 2 {
            let filter_rect = Rect::new(inner.x, inner.y, inner.w, 1);
            super::input::draw_filter_line(surface, filter_rect, input, focused);
            let view_rect = Rect::new(inner.x, inner.y + 1, inner.w, inner.h - 1);
            super::draw_view(surface, view_rect, view, now_ms);
        } else {
            super::draw_view(surface, inner, view, now_ms);
        }
        return;
    }
    // A composer does not replace the view it belongs to.
    if let Some(input) = input {
        let input_height = inner.h.min(3);
        let view_rect = Rect::new(inner.x, inner.y, inner.w, inner.h - input_height);
        super::draw_view(surface, view_rect, view, now_ms);
        draw_input_tile(
            surface,
            Rect::new(inner.x, inner.y + view_rect.h, inner.w, input_height),
            input,
            None,
            None,
        );
        return;
    }
    super::draw_view(surface, inner, view, now_ms);
}

fn view_chrome(view: &RyeOsViewVm) -> Option<(&str, &[String])> {
    match view {
        RyeOsViewVm::Field { field } => field
            .provenance
            .first()
            .map(|provenance| (provenance.as_str(), &[][..])),
        RyeOsViewVm::Rows {
            provenance,
            affordance_hints,
            ..
        }
        | RyeOsViewVm::Timeline {
            provenance,
            affordance_hints,
            ..
        }
        | RyeOsViewVm::Table {
            provenance,
            affordance_hints,
            ..
        } => provenance
            .as_deref()
            .map(|provenance| (provenance, affordance_hints.as_slice())),
        RyeOsViewVm::Document { provenance, .. } => Some((provenance.as_str(), &[])),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_filter_tile_composes_filter_line_above_the_widget() {
        use ryeos_client_base::ui::view_model::{RyeOsTableRowVm, RyeOsTone};
        let view = RyeOsViewVm::Table {
            title: "threads".into(),
            source_status: None,
            columns: vec!["thread".into()],
            total_rows: 1,
            coverage: None,
            provenance: None,
            affordance_hints: vec![],
            rows: vec![RyeOsTableRowVm {
                id: "T-ab".into(),
                cursor: 0,
                cells: vec!["T-ab".into()],
                hierarchy: None,
                cell_tones: Vec::new(),
                tone: RyeOsTone::Neutral,
                intent: None,
                selected: false,
                expandable: false,
                expanded: false,
                detail: Vec::new(),
                changed_at_ms: None,
                changed_tone: None,
                raw: serde_json::Value::Null,
            }],
        };
        let input = RyeOsInputVm {
            address: ryeos_client_base::ui::model::RyeOsInputAddress {
                session_id: "fixture".into(),
                binding_attachment_id: "fixture".into(),
                binding_generation: 1,
                binding_digest: "fixture".into(),
                owner: ryeos_client_base::ui::model::RyeOsInputOwner::Surface {
                    presentation_id: ryeos_client_base::ids::PresentationId::try_new(
                        "fixture-seat",
                    )
                    .unwrap(),
                    surface_instance_id: ryeos_client_base::ids::SurfaceInstanceId::try_new(
                        "fixture-surface",
                    )
                    .unwrap(),
                    view_set_id: ryeos_client_base::ids::ViewSetId::new(1),
                },
                buffer: ryeos_client_base::ui::model::InputBufferKey::from_binding_coordinate(
                    ryeos_client_base::ids::RyeOsViewInstanceKey::surface_tile(
                        &ryeos_client_base::ids::PresentationId::try_new("fixture-seat").unwrap(),
                        &ryeos_client_base::ids::SurfaceInstanceId::try_new("fixture-surface")
                            .unwrap(),
                        ryeos_client_base::ids::TileId::new(1),
                    ),
                    "view:test/input",
                    "line",
                    &ryeos_client_base::ui::binding::UiBindingAttachmentCoordinate {
                        binding_attachment_id: "fixture".into(),
                        binding_generation: 1,
                        binding_digest: "fixture".into(),
                    },
                ),
            },
            cursor: 3,
            focused: false,
            route_label: String::new(),
            placeholder: "filter…".into(),
            hint: String::new(),
            submit_enabled: false,
            completion: vec![],
            live_filter: true,
            text: "run".into(),
        };
        let mut surface = TextSurface::new(40, 10);
        draw_tile(
            &mut surface,
            Rect::new(0, 0, 40, 10),
            &ryeos_client_base::ids::RyeOsViewInstanceKey::surface_tile(
                &ryeos_client_base::ids::PresentationId::try_new("fixture-seat").unwrap(),
                &ryeos_client_base::ids::SurfaceInstanceId::try_new("fixture-surface").unwrap(),
                ryeos_client_base::ids::TileId::new(1),
            ),
            "t",
            true,
            "threads",
            0,
            &view,
            Some(&input),
            Some(Border::Sharp),
            0,
        );
        let row = |y: usize| (0..40).map(|x| surface.get(x, y).rune).collect::<String>();
        // Filter strip on the first interior row (inside the top border).
        assert!(row(1).contains("filter"), "filter sigil: {:?}", row(1));
        assert!(row(1).contains("run"), "buffer text: {:?}", row(1));
        // The table still renders below the filter line (not replaced by it).
        let body = (2..9).map(row).collect::<Vec<_>>().join("\n");
        assert!(body.contains("T-ab"), "table rows below filter: {body:?}");
    }
}
