//! Terminal renderer for the shared Studio view model.
//!
//! This is intentionally a renderer only: it consumes `StudioViewModel`
//! and emits a terminal `TextSurface`. Studio state, actions, and
//! effects remain in `ryeos-client-base` so terminal and web share the
//! same product semantics.
//!
//! Layout of this module mirrors the renderer's planes: `theme` is the
//! only color authority (tone → palette; draw sites never invent
//! colors); `text` is width-aware string shaping; `primitives` are raw
//! surface ops; `chrome` is bars/frames/docks; `widgets/*` is one file
//! per widget primitive (incl. the generic `scene` renderer);
//! `launcher`/`input` are compositions. This file orchestrates: layout
//! traversal, the empty-center backdrop, view dispatch.

mod chrome;
mod help;
mod input;
mod launcher;
mod primitives;
mod text;
mod theme;
mod widgets;

use ryeos_client_base::layout::Rect;
use ryeos_client_base::studio::view_model::{
    StudioLayoutNodeVm, StudioSplitAxisVm, StudioViewModel, StudioViewVm,
};
use ryeos_client_base::text_surface::{Border, Style, TextSurface};

use crate::render_text;

use primitives::draw_lines;
use text::truncate;
use theme::{tone_style, BG, FG};

pub struct StudioTerminalRenderer {
    prev: Option<TextSurface>,
}

impl StudioTerminalRenderer {
    pub fn new() -> Self {
        Self { prev: None }
    }

    pub fn render(
        &mut self,
        stdout: &mut impl std::io::Write,
        vm: &StudioViewModel,
        width: u16,
        height: u16,
    ) -> std::io::Result<()> {
        let surface = build_surface(vm, width as usize, height as usize);
        render_text::render_text_surface(stdout, &surface, &mut self.prev, 0, 0)
    }
}

fn build_surface(vm: &StudioViewModel, width: usize, height: usize) -> TextSurface {
    let width = width.max(1);
    let height = height.max(1);
    let mut surface = TextSurface::new(width, height);
    surface.fill(Style::new().fg(FG).bg(BG));

    // There is no "home" mode. The bars, docks (incl. the real bottom
    // input slot), and launcher render in EVERY state. The only branch is
    // backdrop-vs-tiles in the center: an empty center draws the backdrop
    // scene; tiles fill it otherwise.
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
    let center = chrome::draw_docks(&mut surface, body, vm);
    if let Some(root) = &vm.workspace.root {
        let border = theme::border_for(&vm.presentation.chrome.border);
        draw_layout_node(&mut surface, center, root, border);
    } else if let Some(backdrop) = &vm.workspace.backdrop {
        // Empty center: the backdrop is content — the ONE generic scene
        // renderer draws it (particles twinkle by generation). No
        // per-art code, no background enum.
        widgets::scene::draw_scene(&mut surface, center, backdrop);
    }

    for (index, notice) in vm.notices.iter().rev().take(2).enumerate() {
        let text = format!(" {} ", notice.message);
        let y = top_h + index;
        if y < height.saturating_sub(bottom_h) {
            surface.draw_text(
                2,
                y,
                &truncate(&text, width.saturating_sub(4)),
                tone_style(notice.tone),
            );
        }
    }

    if vm.launcher.open {
        // The overlay dims the whole frame behind it (a scrim), then draws
        // the palette on top at full brightness.
        primitives::dim_surface(&mut surface);
        launcher::draw_launcher(&mut surface, vm);
    } else if vm.help.open {
        // The keys overlay is a sibling scrim — never stacked with the
        // launcher (the keymap makes them mutually exclusive).
        primitives::dim_surface(&mut surface);
        help::draw_help(&mut surface, vm);
    }

    surface
}

fn draw_layout_node(
    surface: &mut TextSurface,
    rect: Rect,
    node: &StudioLayoutNodeVm,
    border: Option<Border>,
) {
    if rect.w == 0 || rect.h == 0 {
        return;
    }
    match node {
        StudioLayoutNodeVm::Tile {
            tile_id,
            focused,
            title,
            actions,
            view,
            input,
        } => chrome::draw_tile(
            surface,
            rect,
            tile_id,
            *focused,
            title,
            actions.len(),
            view,
            input.as_ref(),
            border,
        ),
        StudioLayoutNodeVm::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let (first_rect, second_rect) = split_rect(rect, *axis, *ratio);
            draw_layout_node(surface, first_rect, first, border);
            if let Some(second_rect) = second_rect {
                draw_layout_node(surface, second_rect, second, border);
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
fn split_rect(rect: Rect, axis: StudioSplitAxisVm, ratio: f32) -> (Rect, Option<Rect>) {
    match axis {
        StudioSplitAxisVm::Horizontal => {
            let first_w =
                ((rect.w as f32 * ratio).round() as u16).clamp(1, rect.w.saturating_sub(1).max(1));
            let gap = u16::from(rect.w > 4);
            let second_w = rect.w.saturating_sub(first_w + gap);
            let first = Rect::new(rect.x, rect.y, first_w, rect.h);
            let second =
                (second_w > 0).then(|| Rect::new(rect.x + first_w + gap, rect.y, second_w, rect.h));
            (first, second)
        }
        StudioSplitAxisVm::Vertical => {
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

fn draw_view(surface: &mut TextSurface, rect: Rect, view: &StudioViewVm) {
    if let StudioViewVm::Timeline {
        entries,
        entry_indents,
        selected,
        ..
    } = view
    {
        widgets::timeline::draw_timeline(surface, rect, entries, entry_indents, *selected);
        return;
    }
    if let StudioViewVm::Rows { columns, rows, .. } = view {
        widgets::rows::draw_rows(surface, rect, columns, rows);
        return;
    }
    // Scenes (map/atlas) draw through the ONE generic scene renderer —
    // the same renderer the backdrop uses. No widget-specific scene code.
    if let StudioViewVm::Map { scene } | StudioViewVm::Atlas { scene } = view {
        widgets::scene::draw_scene(surface, rect, scene);
        return;
    }
    if let StudioViewVm::Sections { sections, .. } = view {
        widgets::sections::draw_sections(surface, rect, sections);
        return;
    }
    if let StudioViewVm::Table { columns, rows, .. } = view {
        widgets::table::draw_table(surface, rect, columns, rows);
        return;
    }
    let mut lines = Vec::new();
    match view {
        StudioViewVm::Rows { .. } => unreachable!("rows views return above"),
        StudioViewVm::Timeline { .. } => unreachable!("timeline views return above"),
        StudioViewVm::Map { .. } | StudioViewVm::Atlas { .. } => {
            unreachable!("scene views return above")
        }
        StudioViewVm::Sections { .. } => unreachable!("sections views return above"),
        StudioViewVm::Table { .. } => unreachable!("table views return above"),
        StudioViewVm::Placeholder { title, message } => {
            lines.push(title.clone());
            lines.push(message.clone());
        }
    }
    draw_lines(surface, rect, &lines);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_client_base::studio::model::{BrowserSession, BrowserViewport, StudioCore};
    use ryeos_client_base::studio::view_model::build_view_model;
    use serde_json::json;

    // Characterization tests: these pin the CURRENT terminal split policy
    // so Phase B (the shared resolver) cannot silently change the TUI.
    #[test]
    fn split_rect_horizontal_rounds_with_one_cell_gap() {
        let (first, second) =
            split_rect(Rect::new(0, 0, 100, 10), StudioSplitAxisVm::Horizontal, 0.6);
        // round(100 * 0.6) = 60; gap = 1 (w > 4); second = 100 - 60 - 1 = 39.
        assert_eq!((first.x, first.w), (0, 60));
        let second = second.expect("second child present");
        assert_eq!((second.x, second.w), (61, 39));
        // The pair plus the gap exactly fills the parent.
        assert_eq!(first.w + 1 + second.w, 100);
    }

    #[test]
    fn split_rect_vertical_rounds_with_one_cell_gap() {
        let (first, second) = split_rect(Rect::new(0, 0, 20, 30), StudioSplitAxisVm::Vertical, 0.5);
        assert_eq!((first.y, first.h), (0, 15));
        let second = second.expect("second child present");
        assert_eq!((second.y, second.h), (16, 14));
    }

    #[test]
    fn split_rect_tiny_widths_do_not_overflow_or_drop_first() {
        for w in 1u16..=5 {
            let (first, second) =
                split_rect(Rect::new(0, 0, w, 4), StudioSplitAxisVm::Horizontal, 0.6);
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
        let (_, second) = split_rect(Rect::new(0, 0, 1, 4), StudioSplitAxisVm::Horizontal, 0.6);
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

    /// A surface whose empty center declares a backdrop scene and a
    /// bottom input slot — the post-cut shape (no home mode).
    fn empty_center_core() -> StudioCore {
        let session = BrowserSession {
            session_id: "S-backdrop".to_string(),
            surface_ref: "surface:ryeos/studio/base".to_string(),
            effective_surface: Some(json!({
                "name": "studio-base",
                "version": "1.0.0",
                "backdrop": "view:ryeos/backdrop/shard",
                "slots": {
                    "bottom": { "content": "view:ryeos/input", "open": true, "size": 7 }
                },
                "views": {
                    "view:ryeos/input": {
                        "widget": "text",
                        "input": { "id": "line", "placeholder": "Ask or run a command", "submit": "route" }
                    },
                    "view:ryeos/backdrop/shard": {
                        "widget": "scene",
                        "body": { "objects": [
                            { "kind": "particle", "position": [0.0, 6.0], "scale": 0.9, "color": "#d65d0e", "tone": "accent" },
                            { "kind": "particle", "position": [-9.0, -3.5], "scale": 0.6, "color": "#8ec07c", "tone": "good" },
                            { "kind": "particle", "position": [10.0, 3.4], "scale": 0.5, "color": "#a89984", "tone": "neutral" },
                            { "kind": "text", "position": [0.0, -8.2], "label": "RYE OS", "color": "#d65d0e", "tone": "accent" }
                        ] }
                    }
                }
            })),
            ..Default::default()
        };
        StudioCore::new(session, BrowserViewport::default(), 0)
    }

    #[test]
    fn empty_center_draws_backdrop_scene_and_bottom_input() {
        let vm = build_view_model(&empty_center_core());
        // The backdrop scene resolved on an empty center.
        assert!(vm.workspace.center_is_empty);
        assert!(vm.workspace.backdrop.is_some());

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
        let bottom = vm
            .workspace
            .docks
            .bottom
            .expect("bottom input slot present");
        let input = bottom.input.expect("bottom slot declares input");
        assert_eq!(input.placeholder, "Ask or run a command");
    }

    #[test]
    fn backdrop_twinkle_differs_across_generations() {
        // End-to-end animation proof: stepping `generation` repaints the
        // backdrop with different particle cells (the twinkle). This is
        // the generation → scene → render pipeline working.
        let mut core = empty_center_core();
        core.generation = 0;
        let a = surface_text(&build_surface(&build_view_model(&core), 96, 28));
        core.generation = 1;
        let b = surface_text(&build_surface(&build_view_model(&core), 96, 28));
        assert_ne!(a, b, "the backdrop renders differently across generations");
    }
}
