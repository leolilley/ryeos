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
//! per widget primitive; `home`/`launcher`/`input` are compositions.
//! This file orchestrates: frame mode, layout traversal, view dispatch.

mod ambient;
mod chrome;
mod home;
mod input;
mod launcher;
mod primitives;
mod text;
mod theme;
mod widgets;

use ryeos_client_base::layout::Rect;
use ryeos_client_base::studio::view_model::{
    StudioFrameModeVm, StudioLayoutNodeVm, StudioSplitAxisVm, StudioViewModel, StudioViewVm,
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

    if matches!(vm.presentation.frame.mode, StudioFrameModeVm::Home) && vm.workspace.is_home {
        home::draw_home(
            &mut surface,
            Rect::new(0, 0, width as u16, height as u16),
            vm,
        );
        if vm.launcher.open {
            launcher::draw_launcher(&mut surface, vm);
        }
        return surface;
    }

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
    // Empty center (no computed tree): the background fill stands.
    if let Some(root) = &vm.workspace.root {
        let border = theme::border_for(&vm.presentation.chrome.border);
        draw_layout_node(&mut surface, center, root, border);
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
        launcher::draw_launcher(&mut surface, vm);
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
        } => chrome::draw_tile(
            surface,
            rect,
            tile_id,
            *focused,
            title,
            actions.len(),
            view,
            border,
        ),
        StudioLayoutNodeVm::Split {
            axis,
            ratio,
            first,
            second,
        } => match axis {
            StudioSplitAxisVm::Horizontal => {
                let first_w = ((rect.w as f32 * ratio).round() as u16)
                    .clamp(1, rect.w.saturating_sub(1).max(1));
                let gap = u16::from(rect.w > 4);
                let second_w = rect.w.saturating_sub(first_w + gap);
                draw_layout_node(
                    surface,
                    Rect::new(rect.x, rect.y, first_w, rect.h),
                    first,
                    border,
                );
                if second_w > 0 {
                    draw_layout_node(
                        surface,
                        Rect::new(rect.x + first_w + gap, rect.y, second_w, rect.h),
                        second,
                        border,
                    );
                }
            }
            StudioSplitAxisVm::Vertical => {
                let first_h = ((rect.h as f32 * ratio).round() as u16)
                    .clamp(1, rect.h.saturating_sub(1).max(1));
                let gap = u16::from(rect.h > 4);
                let second_h = rect.h.saturating_sub(first_h + gap);
                draw_layout_node(
                    surface,
                    Rect::new(rect.x, rect.y, rect.w, first_h),
                    first,
                    border,
                );
                if second_h > 0 {
                    draw_layout_node(
                        surface,
                        Rect::new(rect.x, rect.y + first_h + gap, rect.w, second_h),
                        second,
                        border,
                    );
                }
            }
        },
    }
}

fn draw_view(surface: &mut TextSurface, rect: Rect, view: &StudioViewVm) {
    if let StudioViewVm::Timeline { entries, .. } = view {
        widgets::timeline::draw_timeline(surface, rect, entries);
        return;
    }
    if let StudioViewVm::Rows { columns, rows, .. } = view {
        widgets::rows::draw_rows(surface, rect, columns, rows);
        return;
    }
    let mut lines = Vec::new();
    match view {
        StudioViewVm::Rows { .. } => unreachable!("rows views return above"),
        StudioViewVm::Timeline { .. } => unreachable!("timeline views return above"),
        StudioViewVm::Map { scene } | StudioViewVm::Atlas { scene } => {
            lines.push(format!("scene objects: {}", scene.objects.len()));
            lines.push("terminal atlas renderer pending; use rows for interaction".into());
        }
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
    fn home_mode_renders_missing_home_view_without_workspace_docks() {
        let session = BrowserSession {
            session_id: "S-home".to_string(),
            surface_ref: "surface:ryeos/studio/base".to_string(),
            effective_surface: Some(json!({
                "name": "studio-base",
                "version": "1.0.0",
                "home_view": "view:ryeos/home/brand",
                "views": {}
            })),
            ..Default::default()
        };
        let core = StudioCore::new(session, BrowserViewport::default(), 0);
        let vm = build_view_model(&core);

        let rendered = surface_text(&build_surface(&vm, 96, 28));

        assert!(rendered.contains("Welcome to RyeOS"));
        assert!(!rendered.contains("home view missing: view:ryeos/home/brand"));
        assert!(!rendered.contains("type RyeOS input"));
        assert!(!rendered.contains("RyeOS input"));
        assert!(!rendered.contains("service:threads/input"));
        assert!(!rendered.contains("T I L E S"));
    }
}
