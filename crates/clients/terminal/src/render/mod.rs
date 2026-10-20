//! Terminal renderer for the shared RyeOs view model.
//!
//! This is intentionally a renderer only: it consumes `RyeOsViewModel`
//! and emits a terminal `TextSurface`. RyeOs state, intents, and
//! effects remain in `ryeos-client-base` so terminal and web share the
//! same product semantics.
//!
//! Layout of this module mirrors the renderer's planes: `theme` is the
//! only color authority (tone → palette; draw sites never invent
//! colors); `text` is width-aware string shaping; `primitives` are raw
//! surface ops; `chrome` is bars/frames/docks; `widgets/*` is one file
//! per widget primitive (incl. the generic `scene` renderer);
//! `dialog`/`input` are compositions. This file orchestrates: layout
//! traversal, the empty-center backdrop, view dispatch.

mod chrome;
mod dialog;
mod input;
mod primitives;
mod text;
mod theme;
mod widgets;

use ryeos_client_base::layout::Rect;
use ryeos_client_base::text_surface::{Border, Style, TextSurface};
use ryeos_client_base::ui::view_model::{
    RyeOsLayoutNodeVm, RyeOsSplitAxisVm, RyeOsTextLineVm, RyeOsViewModel, RyeOsViewVm,
};

use crate::render_text;

use primitives::draw_lines;
use text::{display_width, truncate};
use theme::{BG, FG, tone_style};

pub struct RyeOsTerminalRenderer {
    prev: Option<TextSurface>,
}

impl RyeOsTerminalRenderer {
    pub fn new() -> Self {
        Self { prev: None }
    }

    pub fn render(
        &mut self,
        stdout: &mut impl std::io::Write,
        vm: &RyeOsViewModel,
        width: u16,
        height: u16,
    ) -> std::io::Result<()> {
        let surface = build_surface(vm, width as usize, height as usize);
        render_text::render_text_surface(stdout, &surface, self.prev.as_ref(), 0, 0)?;
        self.prev = Some(surface);
        Ok(())
    }
}

fn build_surface(vm: &RyeOsViewModel, width: usize, height: usize) -> TextSurface {
    let width = width.max(1);
    let height = height.max(1);
    let mut surface = TextSurface::new(width, height);
    surface.fill(Style::new().fg(FG).bg(BG));

    // There is no "home" mode. Bars, ordinary docks and dialogs render in
    // every state. A maximized tile is the one deliberate exception: the
    // shared presentation layout projects it as the sole root and all clients
    // give it the complete body rather than retaining supporting docks.
    let top_h = if vm.presentation.chrome.top_bar.visible && height >= 3 {
        chrome::draw_top_bar(&mut surface, vm);
        1
    } else {
        0
    };
    let bottom_h = if vm.presentation.chrome.status_bar.visible && height >= 3 {
        chrome::draw_status_bar(&mut surface, vm);
        1
    } else {
        0
    };
    let body_h = height.saturating_sub(top_h + bottom_h).max(1);
    let body = Rect::new(0, top_h as u16, width as u16, body_h as u16);
    let maximized = matches!(
        vm.view_set.as_ref().and_then(|set| set.root.as_ref()),
        Some(RyeOsLayoutNodeVm::Tile {
            maximized: true,
            ..
        })
    );
    let center = if maximized {
        body
    } else {
        chrome::draw_docks(&mut surface, body, vm)
    };
    let draw_backdrop_underlay = vm.view_set.as_ref().is_some_and(|set| set.root.is_some())
        && vm.session.ambient.show_background
        && vm
            .session
            .ambient
            .opacity
            .is_some_and(|opacity| opacity > 0.0 && opacity < 1.0);
    if let Some(root) = vm.view_set.as_ref().and_then(|set| set.root.as_ref()) {
        if draw_backdrop_underlay && let Some(backdrop) = &vm.backdrop {
            widgets::scene::draw_scene(&mut surface, center, backdrop);
        }
        let border = theme::border_for(&vm.presentation.chrome.border);
        draw_layout_node(
            &mut surface,
            center,
            root,
            border,
            vm.now_ms,
            draw_backdrop_underlay,
        );
    } else if let Some(backdrop) = &vm.backdrop {
        // Empty center: the backdrop is content — the ONE generic scene
        // renderer draws it (particles twinkle by generation). No
        // per-art code, no background enum.
        widgets::scene::draw_scene(&mut surface, center, backdrop);
    }

    if let Some(active_dialog) = vm.dialogs.first() {
        primitives::dim_surface(&mut surface);
        dialog::draw_dialog(&mut surface, active_dialog, vm.now_ms);
    }

    surface
}

fn draw_layout_node(
    surface: &mut TextSurface,
    rect: Rect,
    node: &RyeOsLayoutNodeVm,
    border: Option<Border>,
    now_ms: u64,
    preserve_background: bool,
) {
    if rect.w == 0 || rect.h == 0 {
        return;
    }
    match node {
        RyeOsLayoutNodeVm::Tile {
            instance_key,
            tile_id,
            focused,
            group_label,
            title,
            attachment_label,
            intents,
            view,
            chrome_hidden,
            background_transparent,
            input,
            tabs,
            ..
        } => {
            let rect = if tabs.len() > 1 && rect.h > 1 {
                let labels = tabs
                    .iter()
                    .map(|tab| {
                        if tab.active {
                            format!("[{}]", tab.title)
                        } else {
                            tab.title.clone()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("  ");
                surface.draw_text(
                    rect.x as usize,
                    rect.y as usize,
                    &truncate(&labels, rect.w as usize),
                    theme::style_fg(),
                );
                Rect::new(rect.x, rect.y + 1, rect.w, rect.h - 1)
            } else {
                rect
            };
            if *chrome_hidden && input.is_none() {
                if !background_transparent {
                    primitives::fill_rect(surface, rect, theme::style_fg());
                }
                draw_view(surface, rect, view, now_ms);
            } else {
                let title = match attachment_label {
                    Some(attachment) => {
                        format!("{} · {attachment}", group_label.as_deref().unwrap_or(title))
                    }
                    None => group_label.as_deref().unwrap_or(title).to_string(),
                };
                chrome::draw_tile(
                    surface,
                    rect,
                    instance_key,
                    tile_id,
                    *focused,
                    &title,
                    intents.len(),
                    view,
                    input.as_ref(),
                    border,
                    now_ms,
                    preserve_background && *background_transparent,
                );
            }
        }
        RyeOsLayoutNodeVm::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let (first_rect, second_rect) = split_rect(rect, *axis, *ratio);
            draw_layout_node(
                surface,
                first_rect,
                first,
                border,
                now_ms,
                preserve_background,
            );
            if let Some(second_rect) = second_rect {
                draw_layout_node(
                    surface,
                    second_rect,
                    second,
                    border,
                    now_ms,
                    preserve_background,
                );
            }
        }
    }
}

/// Terminal split geometry — the concrete cell math for one split, pulled
/// out of `draw_layout_node` so it is pure and testable (Phase A of the
/// layout-plan roadmap). Returns the first child rect and the second
/// (`None` when it would be zero-sized). This encodes the CURRENT terminal
/// policy verbatim: round the split, a 1-cell gap once the axis dimension
/// exceeds 4, and the first child clamped to at least 1. The shared,
/// policy-parameterized resolver (Phase B) replaces this with this exact
/// behavior under a `GridPolicy` — these tests are its guard.
fn split_rect(rect: Rect, axis: RyeOsSplitAxisVm, ratio: f32) -> (Rect, Option<Rect>) {
    match axis {
        RyeOsSplitAxisVm::Horizontal => {
            let first_w =
                ((rect.w as f32 * ratio).round() as u16).clamp(1, rect.w.saturating_sub(1).max(1));
            let gap = u16::from(rect.w > 4);
            let second_w = rect.w.saturating_sub(first_w + gap);
            let first = Rect::new(rect.x, rect.y, first_w, rect.h);
            let second =
                (second_w > 0).then(|| Rect::new(rect.x + first_w + gap, rect.y, second_w, rect.h));
            (first, second)
        }
        RyeOsSplitAxisVm::Vertical => {
            let first_h =
                ((rect.h as f32 * ratio).round() as u16).clamp(1, rect.h.saturating_sub(1).max(1));
            let gap = u16::from(rect.h > 4);
            let second_h = rect.h.saturating_sub(first_h + gap);
            let first = Rect::new(rect.x, rect.y, rect.w, first_h);
            let second =
                (second_h > 0).then(|| Rect::new(rect.x, rect.y + first_h + gap, rect.w, second_h));
            (first, second)
        }
    }
}

fn draw_view(surface: &mut TextSurface, mut rect: Rect, view: &RyeOsViewVm, now_ms: u64) {
    let status = match view {
        RyeOsViewVm::Rows { source_status, .. }
        | RyeOsViewVm::Timeline { source_status, .. }
        | RyeOsViewVm::Table { source_status, .. }
        | RyeOsViewVm::Document { source_status, .. } => source_status.as_ref(),
        _ => None,
    };
    if let Some(status) = status {
        if rect.h == 0 {
            return;
        }
        let prefix = if status.stale {
            "Last observation (stale) · "
        } else {
            ""
        };
        let message = format!("{prefix}{}: {}", status.state, status.message);
        draw_lines(surface, Rect { h: 1, ..rect }, &[message]);
        rect.y += 1;
        rect.h -= 1;
    }
    if let RyeOsViewVm::Field { field } = view {
        widgets::field::draw_field(surface, rect, field);
        return;
    }
    if let RyeOsViewVm::Timeline {
        entries,
        entry_indents,
        entry_arrived_at_ms,
        selected,
        entry_expandable,
        entry_expanded,
        entry_details,
        ..
    } = view
    {
        widgets::timeline::draw_timeline(
            surface,
            rect,
            entries,
            widgets::timeline::TimelineEntryMeta {
                indents: entry_indents,
                arrived_at_ms: entry_arrived_at_ms,
                expandable: entry_expandable,
                expanded: entry_expanded,
                details: entry_details,
            },
            *selected,
            now_ms,
        );
        return;
    }
    if let RyeOsViewVm::Rows { columns, rows, .. } = view {
        widgets::rows::draw_rows(surface, rect, columns, rows, now_ms);
        return;
    }
    // Scenes (map/atlas) draw through the ONE generic scene renderer —
    // the same renderer the backdrop uses. No widget-specific scene code.
    if let RyeOsViewVm::Map { scene } | RyeOsViewVm::Atlas { scene } = view {
        widgets::scene::draw_scene(surface, rect, scene);
        return;
    }
    if let RyeOsViewVm::Sections { sections, .. } = view {
        widgets::sections::draw_sections(surface, rect, sections, now_ms);
        return;
    }
    if let RyeOsViewVm::Table {
        columns,
        rows,
        coverage,
        ..
    } = view
    {
        widgets::table::draw_table_with_coverage(
            surface,
            rect,
            columns,
            rows,
            coverage.as_ref(),
            now_ms,
        );
        return;
    }
    if let RyeOsViewVm::Text {
        lines, position, ..
    } = view
    {
        draw_text_view(surface, rect, lines, *position);
        return;
    }
    if let RyeOsViewVm::Document {
        path,
        content,
        truncated,
        ..
    } = view
    {
        let mut lines = vec![path.clone()];
        if *truncated {
            lines.push("[bounded preview]".to_string());
        }
        lines.extend(content.lines().map(str::to_owned));
        draw_lines(surface, rect, &lines);
        return;
    }
    let mut lines = Vec::new();
    match view {
        RyeOsViewVm::Field { .. } => unreachable!("field views return above"),
        RyeOsViewVm::Text { .. } => unreachable!("text views return above"),
        RyeOsViewVm::Document { .. } => unreachable!("document views return above"),
        RyeOsViewVm::Rows { .. } => unreachable!("rows views return above"),
        RyeOsViewVm::Timeline { .. } => unreachable!("timeline views return above"),
        RyeOsViewVm::Map { .. } | RyeOsViewVm::Atlas { .. } => {
            unreachable!("scene views return above")
        }
        RyeOsViewVm::Sections { .. } => unreachable!("sections views return above"),
        RyeOsViewVm::Table { .. } => unreachable!("table views return above"),
        RyeOsViewVm::Placeholder { title, message } => {
            lines.push(title.clone());
            lines.push(message.clone());
        }
        RyeOsViewVm::RequiredSubject {
            title,
            input,
            facets,
            actions,
        } => {
            lines.push(title.clone());
            lines.push(format!("{input}: {}", facets.join(", ")));
            if !actions.is_empty() {
                lines.push("Ctrl+K: choose a source for this subject".into());
                lines.extend(
                    actions
                        .iter()
                        .map(|action| format!("  in launcher · {}", action.label)),
                );
            }
        }
    }
    draw_lines(surface, rect, &lines);
}

fn draw_text_view(
    surface: &mut TextSurface,
    rect: Rect,
    lines: &[RyeOsTextLineVm],
    position: ryeos_client_base::ui::view_model::RyeOsTextPositionVm,
) {
    if rect.w == 0 || rect.h == 0 || lines.is_empty() {
        return;
    }
    let width = rect.w as usize;
    let height = rect.h as usize;
    let anchor_y = (position.y * height.saturating_sub(1) as f32).round() as usize;
    let start_y = rect.y as usize + anchor_y.saturating_sub(lines.len() / 2);
    for (index, line) in lines.iter().enumerate() {
        let y = start_y + index;
        if y >= rect.y as usize + height {
            break;
        }
        let text = truncate(&line.text, width);
        let text_w = display_width(&text);
        let anchor_x = (position.x * width.saturating_sub(1) as f32).round() as usize;
        let local_x = anchor_x
            .saturating_sub(text_w / 2)
            .min(width.saturating_sub(text_w));
        let x = rect.x as usize + local_x;
        surface.draw_text(x, y, &text, tone_style(line.tone));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_client_base::ui::model::{BrowserSession, BrowserViewport, RyeOsCore};
    use ryeos_client_base::ui::view_model::build_view_model;
    use serde_json::json;

    // Characterization tests: these pin the CURRENT terminal split policy
    // so Phase B (the shared resolver) cannot silently change the TUI.
    #[test]
    fn split_rect_horizontal_rounds_with_one_cell_gap() {
        let (first, second) =
            split_rect(Rect::new(0, 0, 100, 10), RyeOsSplitAxisVm::Horizontal, 0.6);
        // round(100 * 0.6) = 60; gap = 1 (w > 4); second = 100 - 60 - 1 = 39.
        assert_eq!((first.x, first.w), (0, 60));
        let second = second.expect("second child present");
        assert_eq!((second.x, second.w), (61, 39));
        // The pair plus the gap exactly fills the parent.
        assert_eq!(first.w + 1 + second.w, 100);
    }

    #[test]
    fn split_rect_vertical_rounds_with_one_cell_gap() {
        let (first, second) = split_rect(Rect::new(0, 0, 20, 30), RyeOsSplitAxisVm::Vertical, 0.5);
        assert_eq!((first.y, first.h), (0, 15));
        let second = second.expect("second child present");
        assert_eq!((second.y, second.h), (16, 14));
    }

    #[test]
    fn split_rect_tiny_widths_do_not_overflow_or_drop_first() {
        for w in 1u16..=5 {
            let (first, second) =
                split_rect(Rect::new(0, 0, w, 4), RyeOsSplitAxisVm::Horizontal, 0.6);
            assert!(
                first.w >= 1,
                "first child is always at least one cell (w={w})"
            );
            // No child escapes the parent bounds.
            assert!(first.x + first.w <= w);
            if let Some(second) = second {
                assert!(second.x + second.w <= w, "second stays in bounds (w={w})");
            }
        }
    }

    #[test]
    fn split_rect_drops_second_when_it_would_be_zero() {
        // w = 2: first_w = round(2*0.6)=1, gap=0 (w<=4), second = 2-1-0 = 1.
        // w = 1: first_w clamped to 1, gap=0, second = 1-1 = 0 -> None.
        let (_, second) = split_rect(Rect::new(0, 0, 1, 4), RyeOsSplitAxisVm::Horizontal, 0.6);
        assert!(second.is_none(), "no zero-width second child");
    }

    fn surface_text(surface: &TextSurface) -> String {
        let mut out = String::new();
        for y in 0..surface.height {
            for x in 0..surface.width {
                let ch = surface.get(x, y).rune;
                out.push(if ch == '\0' { ' ' } else { ch });
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn unresolved_subject_labels_actions_as_launcher_only() {
        let mut surface = TextSurface::new(80, 8);
        let view = RyeOsViewVm::RequiredSubject {
            title: "Subject required".into(),
            input: "thread".into(),
            facets: vec!["thread_id".into()],
            actions: vec![ryeos_client_base::ui::view_model::RyeOsTileIntentVm {
                label: "Use selection in Development".into(),
                title: "Exact selected subject".into(),
                intent: ryeos_client_base::ui::RyeOsUiIntent::OpenLauncher,
            }],
        };
        draw_view(&mut surface, Rect::new(0, 0, 80, 8), &view, 0);
        let text = surface_text(&surface);
        assert!(text.contains("Ctrl+K: choose a source for this subject"));
        assert!(text.contains("in launcher · Use selection in Development"));
    }

    #[test]
    fn native_top_bar_shows_projected_view_sets_and_keeps_active_set_on_narrow_widths() {
        let mut vm = build_view_model(&empty_center_core());
        let mut development = vm.presentation.chrome.top_bar.tabs[0].clone();
        development.number = 1;
        development.title = "Development".into();
        development.active = false;
        let mut review = development.clone();
        review.view_set_id = ryeos_client_base::ids::ViewSetId::new(999);
        review.number = 2;
        review.title = "Review".into();
        review.active = true;
        vm.presentation.chrome.top_bar.tabs = vec![development, review];

        let mut wide = TextSurface::new(90, 1);
        chrome::draw_top_bar(&mut wide, &vm);
        let text = surface_text(&wide);
        assert!(text.contains("02 Review"));
        assert!(text.contains("01 Development"));
        assert!(text.find("01 Development") < text.find("02 Review"));
        assert!(
            wide.cells
                .iter()
                .any(|cell| cell.rune == 'R' && cell.bg == theme::ACCENT)
        );

        for width in [1, 4, 9, 20] {
            let mut narrow = TextSurface::new(width, 1);
            chrome::draw_top_bar(&mut narrow, &vm);
            assert_eq!(narrow.cells.len(), width);
            if width == 20 {
                assert!(
                    surface_text(&narrow).contains("02"),
                    "active tab remains visible"
                );
            }
        }
        vm.chrome.health_label = "H".repeat(200);
        let mut long_health = TextSurface::new(40, 1);
        chrome::draw_top_bar(&mut long_health, &vm);
        assert!(surface_text(&long_health).contains("02 Review"));
        assert!(surface_text(&long_health).contains('…'));
    }

    #[test]
    fn status_bar_reserves_badge_and_complete_key_hint() {
        let mut vm = build_view_model(&empty_center_core());
        vm.presentation.chrome.status_bar.key_hint.clear();
        vm.presentation.chrome.status_bar.segments =
            vec![ryeos_client_base::ui::view_model::RyeOsStatusSegmentVm {
                id: "long".into(),
                label: None,
                value: "X".repeat(100),
                tone: ryeos_client_base::ui::view_model::RyeOsTone::Neutral,
                grow: false,
            }];
        let mut long = TextSurface::new(40, 1);
        chrome::draw_status_bar(&mut long, &vm);
        assert_eq!(long.get(32, 0).rune, '…');
        assert!(surface_text(&long).contains(" RyeOS "));

        vm.presentation.chrome.status_bar.segments.clear();
        vm.presentation.chrome.status_bar.key_hint = "KEY".into();
        let mut hint = TextSurface::new(20, 1);
        chrome::draw_status_bar(&mut hint, &vm);
        assert!(surface_text(&hint).contains("KEY"));
        assert!(surface_text(&hint).contains(" RyeOS "));
    }

    /// An admitted Surface declares the backdrop; a separate Shared Config
    /// owns the bottom input. Neither tab geometry nor focus grants a binding.
    fn empty_center_core() -> RyeOsCore {
        let session = BrowserSession {
            session_id: "S-backdrop".to_string(),
            ui_binding_contract_revision: ryeos_client_base::UI_BINDING_CONTRACT_REVISION.into(),
            root_attachment_id: "shared-attachment".into(),
            binding_attachments: vec![ryeos_client_base::ui::UiBindingAttachment {
                binding_attachment_id: "backdrop-attachment".into(),
                binding_generation: 1,
                binding_digest: "backdrop-binding".into(),
                root_ref: "surface:ryeos/ryeos/base".to_string(),
                root_generation: "backdrop-generation".into(),
                project_path: None,
                subject: ryeos_client_base::ui::UiBindingSubject::Projectless,
                posture: Default::default(),
                binding_request_bounds: ryeos_client_base::ui::UiBindingRequestBounds {
                    max_request_bytes: 4096,
                    max_input_bytes: 1024,
max_seat_append_bytes:64*1024,
},
                presentation: ryeos_client_base::presentation::UiCompositionPresentation::from_effective_surface(json!({
                "name": "ryeos-base",
                "version": "1.0.0",
                "backdrop": "view:test/backdrop",
                "views": {
                    "view:test/backdrop": {
                        "widget": "scene",
                        "body": { "objects": [
                            { "kind": "particle", "position": [0.0, 6.0], "scale": 0.9, "color": "#d65d0e", "tone": "accent" },
                            { "kind": "particle", "position": [-9.0, -3.5], "scale": 0.6, "color": "#8ec07c", "tone": "good" },
                            { "kind": "particle", "position": [10.0, 3.4], "scale": 0.5, "color": "#a89984", "tone": "neutral" },
                            { "kind": "text", "position": [0.0, -8.2], "label": "RYE OS", "color": "#d65d0e", "tone": "accent" }
                        ] }
                    }
                }
                })).unwrap(),
            }],
            ..Default::default()
        };
        let mut session = session;
        let surface_binding = ryeos_client_base::ui::binding::UiBindingAttachmentCoordinate {
            binding_attachment_id: "backdrop-attachment".into(),
            binding_generation: 1,
            binding_digest: "backdrop-binding".into(),
        };
        let shared = serde_json::from_value(json!({
            "binding_attachment_id":"shared-attachment", "binding_generation":2,
            "binding_digest":"shared-binding", "root_ref":"config:test/presentation",
            "root_generation":"shared-generation", "project_path":null, "subject":{"kind":"projectless"},
            "posture":"observation_only", "binding_request_bounds":{"max_request_bytes":4096,"max_input_bytes":1024,"max_seat_append_bytes":65536},
            "presentation":{"definition":{"kind":"shared_presentation","config":{
                "category":"ui","shared_presentation":{"schema":1,"title":"Fixture","launcher":"launch",
                    "dialogs":[{"id":"launch","title":"Launch","view":"view:test/launch"}],
                    "docks":{"bottom":{"content":"view:ryeos/input","open":true,"size":7}},
                    "capabilities":{"sources":[],"affordances":[]}}
            }}, "views":{
                "view:test/launch":{"widget":"text","body":"Fixture only"},
                "view:ryeos/input":{"widget":"text","input":{"id":"line","placeholder":"Ask or run a command","submit":"route"}}
            }}
        })).expect("actual Config descriptor fixture");
        session.binding_attachments.push(shared);
        let presentation_id = "11111111-1111-4111-8111-111111111111";
        let surface_instance_id = "22222222-2222-4222-8222-222222222222";
        let surface = session.binding_attachments[0]
            .presentation
            .definition
            .surface()
            .unwrap();
        let mount_manifest =
            ryeos_client_base::ui::composition_identity::UiSurfaceMountManifest::allocate(
                ryeos_client_base::ids::PresentationId::try_new(presentation_id).unwrap(),
                ryeos_client_base::ids::SurfaceInstanceId::try_new(surface_instance_id).unwrap(),
                1,
                surface
                    .to_mount_seeds()
                    .unwrap()
                    .into_iter()
                    .map(|seed| (seed, surface_binding.clone()))
                    .collect(),
                surface
                    .to_mount_seeds()
                    .unwrap()
                    .first()
                    .map(|seed| seed.id.as_str()),
            )
            .unwrap();
        session.presentations = vec![ryeos_client_base::ui::binding::UiPresentationMembership {
            presentation_id: presentation_id.into(),
            next_mount_id: mount_manifest.next_mount_id,
            initialization_digest: "a".repeat(64),
            root_binding: ryeos_client_base::ui::binding::UiBindingAttachmentCoordinate {
                binding_attachment_id: "shared-attachment".into(),
                binding_generation: 2,
                binding_digest: "shared-binding".into(),
            },
            thread_id: Some("fixture-seat".into()),
            chain_root_id: Some("fixture-chain".into()),
            surfaces: vec![ryeos_client_base::ui::binding::UiSurfaceMembership {
                saved_surface: None,
                surface_instance_id: surface_instance_id.into(),
                ordinal: 1,
                title: "ryeos-base".into(),
                binding: surface_binding,
                mount_manifest,
            }],
        }];
        let membership = &mut session.presentations[0];
        let initialization = ryeos_client_base::ui::presentation_initialization::UiPresentationInitialization {
            schema: 2,
            selection: ryeos_client_base::ui::presentation_initialization::UiPresentationInitialSelection::ConfiguredDefaults,
            presentation_id: presentation_id.into(),
            creation_request_id: "33333333-3333-4333-8333-333333333333".into(),
            root_binding: membership.root_binding.clone(),
            next_mount_id: membership.next_mount_id,
            surfaces: membership.surfaces.clone(),
            binding_attachments: session.binding_attachments.clone(),
        };
        initialization
            .validate()
            .expect("exact authored fixture initialization");
        membership.initialization_digest = initialization.digest().unwrap();
        let mut core = RyeOsCore::new(session, presentation_id, BrowserViewport::default(), 0);
        core.replay_seat_events([ryeos_client_base::ui::seat::SeatEvent {
            producer_engine_seq: None,
            seq: 0,
            kind: ryeos_client_base::ui::seat::SeatEventKind::PresentationInitialized(
                ryeos_client_base::ui::presentation_initialization::UiPresentationInitializationRecord {
                    operation_id: initialization.creation_request_id.clone(),
                    initialization,
                },
            ),
        }]);
        core.require_replayed_initialization()
            .expect("fixture initialization replay");
        core.dispatch(ryeos_client_base::ui::RyeOsEvent::Ui {
            event: ryeos_client_base::ui::RyeOsUiEvent::FocusInput,
        });
        core
    }

    #[test]
    fn empty_center_draws_backdrop_scene_and_bottom_input() {
        let vm = build_view_model(&empty_center_core());
        // The backdrop scene resolved on an empty center.
        assert!(vm.view_set.as_ref().unwrap().center_is_empty);
        assert!(vm.backdrop.is_some());

        let rendered = surface_text(&build_surface(&vm, 96, 28));
        // The backdrop scene draws its text objects + particles.
        assert!(rendered.contains("RYE OS"), "backdrop brand text renders");
        assert!(
            rendered.contains('·') || rendered.contains('•') || rendered.contains('●'),
            "backdrop particles render as dots"
        );
        // The real bottom input slot renders in this state (the bug fix) as
        // a minimal bordered box — no prompt sigil, route strip, or hint;
        // the border + cursor are the whole signal. The bottom rows carry
        // the box border.
        let lines: Vec<&str> = rendered.lines().collect();
        let tail = lines[lines.len().saturating_sub(8)..].join("\n");
        assert!(
            tail.contains('│') || tail.contains('┃'),
            "the bottom input slot renders its bordered box"
        );
        assert!(
            !rendered.contains("$ ") && !rendered.contains("Shift+Enter"),
            "the minimal input box drops the prompt sigil and hint"
        );
    }

    #[test]
    fn bottom_slot_is_the_active_input_on_empty_center() {
        // The bug's regression: on an empty center, the bottom slot is the
        // focused/active input instance carrying the prompt VM.
        let vm = build_view_model(&empty_center_core());
        let bottom = vm.docks.bottom.expect("shared bottom input present");
        let input = bottom.input.expect("bottom slot declares input");
        assert_eq!(input.placeholder, "Ask or run a command");
    }

    #[test]
    fn backdrop_twinkle_differs_across_generations() {
        // End-to-end animation proof: stepping `generation` repaints the
        // backdrop with different particle cells. The breathe is primarily
        // a colour blend, so cells compare as (glyph, fg) pairs — text
        // alone may coincide between adjacent frames.
        let styled = |surface: &ryeos_client_base::text_surface::TextSurface| {
            let mut cells = Vec::new();
            for y in 0..surface.height {
                for x in 0..surface.width {
                    let cell = surface.get(x, y);
                    cells.push((cell.rune, cell.fg));
                }
            }
            cells
        };
        let mut core = empty_center_core();
        core.runtime.now_ms = 0;
        let a = styled(&build_surface(&build_view_model(&core), 96, 28));
        core.runtime.now_ms = ryeos_client_base::ui::scene_model::SCENE_FRAME_MS;
        let b = styled(&build_surface(&build_view_model(&core), 96, 28));
        assert_ne!(a, b, "the backdrop renders differently across generations");
    }
}
