//! Tile and frame chrome: bars, dock tiles, tile frames with title,
//! corner marks, and the provenance/affordance footer. Chrome carries
//! truth (provenance, focus) — it is load-bearing, not decoration.

use ryeos_client_base::layout::Rect;
use ryeos_client_base::text_surface::{Border, Color, Style, TextSurface};
use ryeos_client_base::ui::view_model::{
    RyeOsDockPlaneVm, RyeOsDockTileVm, RyeOsInputVm, RyeOsViewModel, RyeOsViewVm,
};

use super::input::draw_input_tile;
use super::primitives::{fill_line, fill_rect};
use super::text::{display_width, letterspace, truncate};
use super::theme::{ACCENT, BG, FG, MUTED, WARN, border_for, mix_toward, style_muted, tone_style};

pub fn draw_top_bar(surface: &mut TextSurface, vm: &RyeOsViewModel) {
    let tabs = &vm.presentation.chrome.top_bar.tabs;
    let title = vm
        .session
        .surface_ordinal
        .map(|ordinal| format!("{ordinal:02} {}", vm.session.title))
        .unwrap_or_else(|| vm.session.title.clone());
    let show_status = surface.width >= 36;
    let status = if show_status {
        truncate(
            &format!(" {} ", vm.chrome.health_label),
            (surface.width / 4).min(18),
        )
    } else {
        String::new()
    };
    // The native client uses the same projected view-set strip as the web
    // client. Keep authored order when it fits; on narrow widths start at the
    // active set so its identity remains visible without reordering the
    // displayed subsequence or inventing a second navigation owner.
    draw_bar(surface, 0, "", MUTED);
    let status_start = if show_status {
        surface.width.saturating_sub(display_width(&status))
    } else {
        surface.width
    };
    let title_budget = if tabs.is_empty() {
        status_start
    } else {
        (status_start / 3).min(24)
    };
    let title = truncate(&title, title_budget);
    let mut x = surface.draw_text(0, 0, &title, Style::new().fg(MUTED).bg(BG));
    if !tabs.is_empty() && x + 3 < status_start {
        x += surface.draw_text(x, 0, " │ ", Style::new().fg(MUTED).bg(BG));
        let tab_budget = status_start.saturating_sub(x);
        let full_width = tabs
            .iter()
            .map(|tab| display_width(&format!(" {:02} {} ", tab.number, tab.title)))
            .fold(0usize, usize::saturating_add)
            .saturating_add(tabs.len().saturating_sub(1).saturating_mul(3));
        let start = if full_width <= tab_budget {
            0
        } else {
            tabs.iter().position(|tab| tab.active).unwrap_or(0)
        };
        for tab in tabs.iter().skip(start) {
            let available = status_start.saturating_sub(x);
            if available < 4 {
                break;
            }
            let label = format!(" {:02} {} ", tab.number, tab.title);
            let width = display_width(&label);
            if !tab.active && width > available {
                break;
            }
            let style = if tab.active {
                Style::new().fg(BG).bg(ACCENT)
            } else {
                Style::new().fg(FG).bg(BG)
            };
            let rendered = if tab.active {
                truncate(&label, available)
            } else {
                label
            };
            x += surface.draw_text(x, 0, &rendered, style);
            if x + 2 < status_start {
                x += surface.draw_text(x, 0, " · ", Style::new().fg(MUTED).bg(BG));
            }
        }
    }
    if show_status {
        surface.draw_text(
            status_start,
            0,
            &status,
            tone_style(vm.chrome.health_tone).bg(BG),
        );
    }
}

pub fn draw_status_bar(surface: &mut TextSurface, vm: &RyeOsViewModel) {
    let y = surface.height.saturating_sub(1);
    let energy = vm.presentation.chrome.status_bar.energy.clamp(0.0, 1.0);
    let mut bg = mix_toward(BG, ACCENT, 0.12 * energy);
    if vm.presentation.chrome.status_bar.attention.is_some() {
        bg = mix_toward(bg, WARN, 0.18);
    }
    let base = Style::new().fg(MUTED).bg(bg);
    fill_line(surface, 0, y, surface.width, base);
    let mut x = 1usize;
    // Wall-clock stepped (144ms/step) so the pulse holds a steady rate no
    // matter how many events folded this frame.
    let heartbeat = if energy > 0.05 {
        ["⋄", "◇", "◈", "◆"][(vm.now_ms as usize / 144) % 4]
    } else {
        "⋄"
    };
    surface.draw_text(
        x,
        y,
        heartbeat,
        Style::new().fg(mix_toward(MUTED, ACCENT, energy)).bg(bg),
    );
    x += 2;

    // The key hint is the affordance the operator relies on to know what's
    // available — reserve its width (plus the "· " separator) up front so
    // the segment loop degrades into whatever budget remains instead of
    // squeezing the hint out entirely when the bar is tight.
    let key_hint = &vm.presentation.chrome.status_bar.key_hint;
    let hint_width = display_width(key_hint);
    let badge_start = if surface.width >= 7 {
        surface.width - 7
    } else {
        surface.width
    };
    let hint_reserve = if hint_width > 0 && hint_width + 3 < badge_start {
        hint_width + 3
    } else {
        0
    };
    let segments_end = badge_start.saturating_sub(hint_reserve);

    for segment in &vm.presentation.chrome.status_bar.segments {
        if x >= segments_end {
            break;
        }
        if let Some(label) = &segment.label {
            let label = format!("{}: ", letterspace(label));
            x += surface.draw_text(
                x,
                y,
                &truncate(&label, segments_end - x),
                style_muted().bg(bg),
            );
        }
        let value = truncate(&segment.value, segments_end.saturating_sub(x));
        x += surface.draw_text(x, y, &value, tone_style(segment.tone).bg(bg)) + 2;
    }

    if hint_reserve > 0 && x + 2 + hint_width <= badge_start {
        surface.draw_text(x, y, "·", style_muted().bg(bg));
        x += 2;
        surface.draw_text(x, y, key_hint, style_muted().bg(bg));
    }
    if surface.width >= 7 {
        surface.draw_text(badge_start, y, " RyeOS ", Style::new().fg(BG).bg(ACCENT));
    }
}

fn draw_bar(surface: &mut TextSurface, y: usize, text: &str, fg: Color) {
    fill_line(surface, 0, y, surface.width, Style::new().fg(fg).bg(BG));
    surface.draw_text(
        0,
        y,
        &truncate(text, surface.width),
        Style::new().fg(fg).bg(BG),
    );
}

pub fn draw_docks(surface: &mut TextSurface, body: Rect, vm: &RyeOsViewModel) -> Rect {
    let border = border_for(&vm.presentation.chrome.border);
    let project_path = vm.session.project_path.as_deref();
    let (dock_rects, center) = carve_docks(body, &vm.docks);
    for (dock, rect) in dock_rects {
        draw_dock_tile(surface, rect, dock, project_path, border, vm.now_ms);
    }
    center
}

/// Carve the edge docks out of `body` and return each dock's rect plus the
/// remaining center — pure geometry, separated from drawing so it can be
/// tested (Phase A of the layout-plan roadmap). The carving order is
/// left → right → top → bottom, each taking from the running center with a
/// 1-cell gap; left/right are bounded so the center keeps ≥ 8 cells wide,
/// top/bottom so it keeps ≥ 6 tall. This encodes the CURRENT terminal
/// policy verbatim.
fn carve_docks(body: Rect, docks: &RyeOsDockPlaneVm) -> (Vec<(&RyeOsDockTileVm, Rect)>, Rect) {
    let mut center = body;
    let mut out = Vec::new();

    if let Some(left) = &docks.left {
        let w = left.size.min(center.w.saturating_sub(9));
        if w > 0 {
            out.push((left, Rect::new(center.x, center.y, w, center.h)));
            center.x = center.x.saturating_add(w.saturating_add(1));
            center.w = center.w.saturating_sub(w.saturating_add(1));
        }
    }

    if let Some(right) = &docks.right {
        let w = right.size.min(center.w.saturating_sub(9));
        if w > 0 {
            let x = center.x.saturating_add(center.w.saturating_sub(w));
            out.push((right, Rect::new(x, center.y, w, center.h)));
            center.w = center.w.saturating_sub(w.saturating_add(1));
        }
    }

    if let Some(top) = &docks.top {
        let h = top.size.min(center.h.saturating_sub(7));
        if h > 0 {
            out.push((top, Rect::new(center.x, center.y, center.w, h)));
            center.y = center.y.saturating_add(h.saturating_add(1));
            center.h = center.h.saturating_sub(h.saturating_add(1));
        }
    }

    if let Some(bottom) = &docks.bottom {
        let h = bottom.size.min(center.h.saturating_sub(7));
        if h > 0 {
            let y = center.y.saturating_add(center.h.saturating_sub(h));
            out.push((bottom, Rect::new(center.x, y, center.w, h)));
            center.h = center.h.saturating_sub(h.saturating_add(1));
        }
    }

    (out, center)
}

fn draw_dock_tile(
    surface: &mut TextSurface,
    rect: Rect,
    dock: &RyeOsDockTileVm,
    project_path: Option<&str>,
    border: Option<Border>,
    now_ms: u64,
) {
    // An input remains with its content. Cell allocation is renderer-local;
    // input ownership and routing are the shared instance contract.
    if let Some(input) = dock.input.as_ref() {
        let input_height = rect.h.min(3);
        let view_rect = Rect::new(rect.x, rect.y, rect.w, rect.h - input_height);
        let input_rect = Rect::new(rect.x, rect.y + view_rect.h, rect.w, input_height);
        super::draw_view(surface, view_rect, &dock.view, now_ms);
        draw_input_tile(surface, input_rect, input, project_path, border);
        return;
    }

    // Slots sit flush on the page background (BG), separated by their
    // border — no PANEL fill, no shadow — consistent with the input box
    // and the tiles.
    fill_rect(surface, rect, Style::new().fg(FG).bg(BG));
    let x = rect.x as usize;
    let y = rect.y as usize;
    let w = rect.w as usize;
    let h = rect.h as usize;
    if w < 2 || h < 2 {
        return;
    }
    if let Some(border) = border {
        let border_fg = if dock.focused { ACCENT } else { MUTED };
        surface.draw_box(
            x,
            y,
            x + w - 1,
            y + h - 1,
            border,
            Style::new().fg(border_fg).bg(BG),
        );
    }
    surface.draw_text(
        x + 2,
        y,
        &truncate(
            &format!(
                " {}{} ",
                dock.title,
                dock.attachment_label
                    .as_ref()
                    .map(|label| format!(" · {label}"))
                    .unwrap_or_default()
            ),
            w.saturating_sub(4),
        ),
        Style::new().fg(ACCENT).bg(BG).bold(),
    );
    // Dock content renders through the SAME widget dispatch as center
    // tiles — rows with tones, timelines, scenes — not a crude subset.
    let inner = Rect::new(
        rect.x + 1,
        rect.y + 1,
        rect.w.saturating_sub(2),
        rect.h.saturating_sub(2),
    );
    super::draw_view(surface, inner, &dock.view, now_ms);
}

#[allow(clippy::too_many_arguments)]
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
    preserve_background: bool,
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
    if !preserve_background {
        fill_rect(surface, rect, Style::new().fg(FG).bg(BG));
    }
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
            style_muted()
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
    use ryeos_client_base::ui::model::RyeOsDockEdge;

    fn dock(edge: RyeOsDockEdge, size: u16) -> RyeOsDockTileVm {
        RyeOsDockTileVm {
            instance_key: ryeos_client_base::ids::RyeOsViewInstanceKey::shared_dock(
                &ryeos_client_base::ids::PresentationId::try_new(
                    "11111111-1111-4111-8111-111111111111",
                )
                .unwrap(),
                match edge {
                    RyeOsDockEdge::Top => "top",
                    RyeOsDockEdge::Bottom => "bottom",
                    RyeOsDockEdge::Left => "left",
                    RyeOsDockEdge::Right => "right",
                },
            )
            .unwrap(),
            edge,
            title: "t".into(),
            attachment_label: None,
            heading: None,
            supplement: None,
            size,
            focused: false,
            view: RyeOsViewVm::Placeholder {
                title: "t".into(),
                message: "m".into(),
            },
            input: None,
        }
    }

    #[test]
    fn carve_docks_bottom_leaves_center_above_and_preserves_bounds() {
        let docks = RyeOsDockPlaneVm {
            bottom: Some(dock(RyeOsDockEdge::Bottom, 7)),
            ..Default::default()
        };
        let body = Rect::new(0, 0, 100, 30);
        let (rects, center) = carve_docks(body, &docks);
        assert_eq!(rects.len(), 1);
        let (_, bottom) = rects[0];
        // The bottom dock is 7 tall, anchored at the bottom of the body.
        assert_eq!((bottom.y, bottom.h), (23, 7));
        // The center sits above it, minus the 1-cell gap, never overlapping.
        assert_eq!(center.y, 0);
        assert_eq!(center.h, 22);
        assert!(center.y + center.h < bottom.y, "center is above the dock");
        assert!(
            bottom.y + bottom.h <= body.y + body.h,
            "dock stays in bounds"
        );
    }

    #[test]
    fn carve_docks_left_and_right_keep_center_in_bounds() {
        let docks = RyeOsDockPlaneVm {
            left: Some(dock(RyeOsDockEdge::Left, 20)),
            right: Some(dock(RyeOsDockEdge::Right, 20)),
            ..Default::default()
        };
        let body = Rect::new(0, 0, 100, 30);
        let (rects, center) = carve_docks(body, &docks);
        assert_eq!(rects.len(), 2);
        assert!(
            center.x > 0 && center.x + center.w <= 100,
            "center within body"
        );
        // Order is left then right.
        assert_eq!(rects[0].1.x, 0);
        assert!(rects[1].1.x > center.x);
    }

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
            false,
        );
        let row = |y: usize| (0..40).map(|x| surface.get(x, y).rune).collect::<String>();
        // Filter strip on the first interior row (inside the top border).
        assert!(row(1).contains("filter"), "filter sigil: {:?}", row(1));
        assert!(row(1).contains("run"), "buffer text: {:?}", row(1));
        // The table still renders below the filter line (not replaced by it).
        let body = (2..9).map(row).collect::<Vec<_>>().join("\n");
        assert!(body.contains("T-ab"), "table rows below filter: {body:?}");
    }

    #[test]
    fn carve_docks_tiny_body_drops_docks_that_would_starve_center() {
        // A left dock can't take so much that the center drops below 8 wide.
        let docks = RyeOsDockPlaneVm {
            left: Some(dock(RyeOsDockEdge::Left, 50)),
            ..Default::default()
        };
        let body = Rect::new(0, 0, 10, 6);
        let (rects, center) = carve_docks(body, &docks);
        // Reserve both the centre minimum and the one-cell gap.
        assert_eq!(rects[0].1.w, 1);
        assert_eq!(center.w, 8);
    }

    #[test]
    fn carve_docks_reserves_both_gaps_and_center_minima() {
        let docks = RyeOsDockPlaneVm {
            left: Some(dock(RyeOsDockEdge::Left, 50)),
            right: Some(dock(RyeOsDockEdge::Right, 50)),
            top: Some(dock(RyeOsDockEdge::Top, 50)),
            bottom: Some(dock(RyeOsDockEdge::Bottom, 50)),
        };
        for width in 0..25 {
            for height in 0..20 {
                let body = Rect::new(3, 4, width, height);
                let (rects, center) = carve_docks(body, &docks);
                assert!(center.w >= width.min(8));
                assert!(center.h >= height.min(6));
                assert!(center.x >= body.x && center.y >= body.y);
                assert!(center.x + center.w <= body.x + body.w);
                assert!(center.y + center.h <= body.y + body.h);
                assert!(rects.len() <= 4);
            }
        }
    }
}
