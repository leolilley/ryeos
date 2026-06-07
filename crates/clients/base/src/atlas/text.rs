use super::model::{AtlasItemKind, AtlasLensVm, NamespaceAtlasVm};
use crate::text_surface::{Style, TextSurface};
use crate::theme;

pub fn render_atlas(atlas: &NamespaceAtlasVm, w: usize, h: usize) -> TextSurface {
    let mut surface = TextSurface::new(w, h);
    surface.fill(Style::new().bg(theme::BG));
    if w == 0 || h == 0 {
        return surface;
    }

    let header = Style::new().fg(theme::FG).bg(theme::BG).bold();
    let dim = Style::new().fg(theme::FG_MUTED).bg(theme::BG);
    let normal = Style::new().fg(theme::FG).bg(theme::BG);
    let accent = Style::new().fg(theme::ACCENT).bg(theme::BG).bold();
    let highlight = Style::new().fg(theme::YELLOW).bg(theme::BG).bold();

    surface.draw_text(0, 0, "Namespace Atlas", header);
    let layer_label = atlas_layer_label(atlas);
    let lens_label = atlas_lens_label(atlas.ui.active_lens);
    let state_label = format!("{layer_label} · lens: {lens_label}");
    if w > state_label.len() + 20 {
        surface.draw_text(17, 0, &state_label, dim);
    }
    let count = atlas
        .nodes
        .iter()
        .filter(|node| {
            node.stack
                .iter()
                .any(|item| atlas.ui.item_visible(item.kind))
        })
        .count();
    let summary = format!("{} stacks · {} regions", count, atlas.regions.len());
    if w > summary.len() + 1 {
        surface.draw_text(w - summary.len() - 1, 0, &summary, dim);
    }

    if w >= 56 && h >= 16 {
        render_projected(atlas, &mut surface, normal, dim, accent, highlight);
    } else {
        render_outline(atlas, &mut surface, normal, dim, accent, highlight);
    }
    render_context_links(atlas, &mut surface, dim, highlight);

    surface
}

fn render_projected(
    atlas: &NamespaceAtlasVm,
    surface: &mut TextSurface,
    normal: Style,
    dim: Style,
    accent: Style,
    highlight: Style,
) {
    let w = surface.width;
    let h = surface.height;
    let x_span = (atlas.bounds.x_max - atlas.bounds.x_min).abs().max(1.0);
    let z_span = (atlas.bounds.z_max - atlas.bounds.z_min).abs().max(1.0);
    let left = 2usize;
    let top = 2usize;
    let draw_w = w.saturating_sub(4).max(1);
    let draw_h = h.saturating_sub(5).max(1);

    for node in &atlas.nodes {
        let visible_stack: Vec<_> = node
            .stack
            .iter()
            .filter(|item| atlas.ui.item_visible(item.kind))
            .collect();
        if visible_stack.is_empty() && !node.state.selected && !node.state.highlighted {
            continue;
        }
        let x =
            left + (((node.position[0] - atlas.bounds.x_min) / x_span) * draw_w as f32) as usize;
        let y = top + (((node.position[2] - atlas.bounds.z_min) / z_span) * draw_h as f32) as usize;
        if x >= w || y >= h {
            continue;
        }
        let style = if node.state.selected || node.state.highlighted {
            highlight
        } else if node.state.dimmed {
            dim
        } else if visible_stack.is_empty() {
            dim
        } else {
            normal
        };
        let ch = if node.namespace_key.is_empty() {
            '◎'
        } else if visible_stack.len() > 1 {
            '◉'
        } else if let Some(item) = visible_stack.first() {
            item.kind.glyph()
        } else {
            '·'
        };
        surface.draw_char(x.min(w - 1), y.min(h - 1), ch, style);
        if node.state.selected && x + node.label.len() + 2 < w {
            surface.draw_text(x + 2, y, &node.label, accent);
        }
    }
}

fn render_outline(
    atlas: &NamespaceAtlasVm,
    surface: &mut TextSurface,
    normal: Style,
    dim: Style,
    accent: Style,
    highlight: Style,
) {
    let mut row = 2usize;
    let mut nodes: Vec<_> = atlas
        .nodes
        .iter()
        .filter(|node| !node.path.is_empty())
        .collect();
    nodes.sort_by(|a, b| a.path.cmp(&b.path));
    for node in nodes
        .into_iter()
        .filter(|node| {
            node.stack
                .iter()
                .any(|item| atlas.ui.item_visible(item.kind))
                || node.state.selected
                || node.state.highlighted
        })
        .take(surface.height.saturating_sub(3))
    {
        let indent = "  ".repeat(node.depth.saturating_sub(1) as usize);
        let glyphs = stack_glyphs(
            node.stack
                .iter()
                .filter(|item| atlas.ui.item_visible(item.kind))
                .map(|item| item.kind),
        );
        let style = if node.state.selected || node.state.highlighted {
            highlight
        } else if node.state.dimmed {
            dim
        } else {
            normal
        };
        let text = format!("{indent}{glyphs} {}", node.label);
        surface.draw_text(1, row, &text, style);
        if node.state.selected {
            surface.draw_text(surface.width.saturating_sub(10), row, "selected", accent);
        }
        row += 1;
        if row >= surface.height {
            break;
        }
    }
}

fn render_context_links(
    atlas: &NamespaceAtlasVm,
    surface: &mut TextSurface,
    dim: Style,
    highlight: Style,
) {
    if atlas.links.is_empty() || surface.height < 4 || surface.width < 24 {
        return;
    }
    let start_row = surface.height.saturating_sub(atlas.links.len().min(3) + 1);
    surface.draw_text(1, start_row, "context", highlight);
    for (offset, link) in atlas.links.iter().take(3).enumerate() {
        let from = node_label(atlas, &link.from);
        let to = node_label(atlas, &link.to);
        let text = format!("{} → {}", from, to);
        surface.draw_text(1, start_row + offset + 1, &text, dim);
    }
}

fn node_label(atlas: &NamespaceAtlasVm, node_id: &str) -> String {
    atlas
        .nodes
        .iter()
        .find(|node| node.id == node_id)
        .map(|node| node.namespace_key.clone())
        .filter(|key| !key.is_empty())
        .unwrap_or_else(|| node_id.to_string())
}

fn stack_glyphs(kinds: impl Iterator<Item = AtlasItemKind>) -> String {
    let kinds: Vec<_> = kinds.collect();
    let mut glyphs = String::new();
    for kind in [
        AtlasItemKind::Directive,
        AtlasItemKind::Tool,
        AtlasItemKind::Knowledge,
        AtlasItemKind::Config,
        AtlasItemKind::File,
        AtlasItemKind::Other,
    ] {
        if kinds.iter().any(|candidate| *candidate == kind) {
            glyphs.push(kind.glyph());
        }
    }
    if glyphs.is_empty() {
        glyphs.push('◎');
    }
    glyphs
}

fn atlas_layer_label(atlas: &NamespaceAtlasVm) -> String {
    [
        (AtlasItemKind::Directive, "D"),
        (AtlasItemKind::Tool, "T"),
        (AtlasItemKind::Knowledge, "K"),
        (AtlasItemKind::Config, "C"),
        (AtlasItemKind::File, "F"),
    ]
    .into_iter()
    .map(|(kind, label)| {
        if atlas.ui.layer_visible(kind) {
            label.to_string()
        } else {
            "·".to_string()
        }
    })
    .collect::<Vec<_>>()
    .join(" ")
}

fn atlas_lens_label(lens: AtlasLensVm) -> &'static str {
    match lens {
        AtlasLensVm::None => "none",
        AtlasLensVm::Knowledge => "knowledge",
    }
}
