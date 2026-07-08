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
use super::theme::{border_for, mix_toward, style_muted, tone_style, ACCENT, BG, FG, MUTED, WARN};

pub fn draw_top_bar(surface: &mut TextSurface, vm: &RyeOsViewModel) {
    // Breadcrumb: when a drill is open, prefix the return trail (root-first)
    // onto the current level so the operator sees the execution path they
    // stepped down and that Backspace walks back up. The current level reads its
    // own label (the cognition stepped into, e.g. `study`) when known, else the
    // focused view's title.
    let current = vm
        .workspace
        .lens_label
        .clone()
        .unwrap_or_else(|| vm.presentation.chrome.top_bar.focused_title.clone());
    let crumb = if vm.workspace.lens_trail.is_empty() {
        current
    } else {
        format!("{} ▸ {}", vm.workspace.lens_trail.join(" ▸ "), current)
    };
    let text = format!(
        " {}  {}  {} ",
        vm.presentation.chrome.version_label, crumb, vm.presentation.chrome.top_bar.layout_symbol
    );
    draw_bar(surface, 0, &text, ACCENT);
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
    let heartbeat = if energy > 0.05 {
        ["⋄", "◇", "◈", "◆"][(vm.generation as usize / 2) % 4]
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
    for segment in &vm.presentation.chrome.status_bar.segments {
        if x >= surface.width {
            return;
        }
        if let Some(label) = &segment.label {
            let label = format!("{}: ", letterspace(label));
            surface.draw_text(
                x,
                y,
                &truncate(&label, surface.width - x),
                style_muted().bg(bg),
            );
            x = x.saturating_add(display_width(&label));
        }
        let value = truncate(&segment.value, surface.width.saturating_sub(x));
        surface.draw_text(x, y, &value, tone_style(segment.tone).bg(bg));
        x = x.saturating_add(display_width(&value) + 2);
    }
    if x + 3 < surface.width {
        surface.draw_text(x, y, "·", style_muted().bg(bg));
        x += 2;
        surface.draw_text(
            x,
            y,
            &truncate(
                &vm.presentation.chrome.status_bar.key_hint,
                surface.width - x,
            ),
            style_muted().bg(bg),
        );
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
    let (dock_rects, center) = carve_docks(body, &vm.workspace.docks);
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
fn carve_docks<'a>(
    body: Rect,
    docks: &'a RyeOsDockPlaneVm,
) -> (Vec<(&'a RyeOsDockTileVm, Rect)>, Rect) {
    let mut center = body;
    let mut out = Vec::new();

    if let Some(left) = &docks.left {
        let w = left.size.min(center.w.saturating_sub(8));
        if w > 0 {
            out.push((left, Rect::new(center.x, center.y, w, center.h)));
            center.x = center.x.saturating_add(w.saturating_add(1));
            center.w = center.w.saturating_sub(w.saturating_add(1));
        }
    }

    if let Some(right) = &docks.right {
        let w = right.size.min(center.w.saturating_sub(8));
        if w > 0 {
            let x = center.x.saturating_add(center.w.saturating_sub(w));
            out.push((right, Rect::new(x, center.y, w, center.h)));
            center.w = center.w.saturating_sub(w.saturating_add(1));
        }
    }

    if let Some(top) = &docks.top {
        let h = top.size.min(center.h.saturating_sub(6));
        if h > 0 {
            out.push((top, Rect::new(center.x, center.y, center.w, h)));
            center.y = center.y.saturating_add(h.saturating_add(1));
            center.h = center.h.saturating_sub(h.saturating_add(1));
        }
    }

    if let Some(bottom) = &docks.bottom {
        let h = bottom.size.min(center.h.saturating_sub(6));
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
    // An input dock renders minimally on the page background — no PANEL
    // fill, no title, no shadow. Just the bordered buffer + cursor.
    if let Some(input) = dock.input.as_ref() {
        draw_input_tile(surface, rect, input, project_path, border);
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
        &truncate(&format!(" {} ", dock.title), w.saturating_sub(4)),
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
    _tile_id: &str,
    focused: bool,
    title: &str,
    _action_count: usize,
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
    // action count. Border weight/colour carries focus.
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
    if w > 6 {
        if let Some((provenance, affordances)) = view_chrome(view) {
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
    // A prompt input (no widget of its own, e.g. the foot input) renders as
    // the buffer + cursor only; the tile already owns the border/chrome.
    if let Some(input) = input {
        draw_input_tile(surface, inner, input, None, None);
        return;
    }
    super::draw_view(surface, inner, view, now_ms);
}

fn view_chrome(view: &RyeOsViewVm) -> Option<(&str, &[String])> {
    match view {
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
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_client_base::ui::model::RyeOsDockEdge;

    fn dock(edge: RyeOsDockEdge, size: u16) -> RyeOsDockTileVm {
        RyeOsDockTileVm {
            edge,
            title: "t".into(),
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
            columns: vec!["thread".into()],
            total_rows: 1,
            provenance: None,
            affordance_hints: vec![],
            rows: vec![RyeOsTableRowVm {
                id: "T-ab".into(),
                cells: vec!["T-ab".into()],
                cell_tones: Vec::new(),
                tone: RyeOsTone::Neutral,
                action: None,
                selected: false,
                expandable: false,
                expanded: false,
                detail: Vec::new(),
                changed_at_ms: None,
                raw: serde_json::Value::Null,
            }],
        };
        let input = RyeOsInputVm {
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
        // size 50 clamped to w-8 = 2.
        assert_eq!(rects[0].1.w, 2);
        assert!(center.w >= 7, "center keeps room");
    }
}
