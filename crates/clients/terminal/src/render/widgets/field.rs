//! Terminal realization of the renderer-neutral living field.
//!
//! Web owns spatial geometry. The terminal keeps the same selection,
//! traversal, groups, layers, replay state, evidence, and generic traits as a
//! bounded outline. It never branches on project roles or entity kinds.

use std::collections::{BTreeMap, BTreeSet};

use ryeos_client_base::layout::Rect;
use ryeos_client_base::text_surface::{Style, TextSurface};
use ryeos_client_base::ui::field::{
    FieldFill, FieldShape, RyeOsFieldEntityVm, RyeOsFieldVm, RyeOsSourcePhase,
};

use super::super::primitives::fill_line;
use super::super::text::{display_width, truncate};
use super::super::theme::{ACCENT, BG, style_fg, style_muted, style_selected, tone_style};
use super::indexed_grid::draw_indexed_grid;

pub fn draw_field(surface: &mut TextSurface, rect: Rect, field: &RyeOsFieldVm) {
    let width = rect.w as usize;
    let height = rect.h as usize;
    if width == 0 || height == 0 {
        return;
    }
    let bottom = rect.y as usize + height;
    let mut y = rect.y as usize;

    let source_errors = field
        .sources
        .iter()
        .filter(|source| source.error.is_some() || source.phase == RyeOsSourcePhase::Error)
        .count();
    let bounded_sources = field
        .sources
        .iter()
        .filter(|source| source.truncated)
        .count();
    let mut source_health = Vec::new();
    if source_errors > 0 {
        source_health.push(format!("source errors:{source_errors}"));
    }
    if bounded_sources > 0 {
        source_health.push(format!("bounded sources:{bounded_sources}"));
    }
    if !field.replay.outside_cut.is_empty() {
        source_health.push(format!("outside cut:{}", field.replay.outside_cut.len()));
    }
    if field.search.truncated {
        source_health.push("bounded search".to_string());
    }
    source_health.extend(field.sources.iter().map(|source| {
        format!(
            "{}:{:?}{}{}",
            source.name,
            source.phase,
            if source.truncated { ":bounded" } else { "" },
            if source.error.is_some() || source.phase == RyeOsSourcePhase::Error {
                ":error"
            } else {
                ""
            },
        )
        .to_ascii_lowercase()
    }));
    let source_health = source_health.join(" · ");
    let mode = if field.replay.mode == "live" {
        "LIVE".to_string()
    } else {
        field
            .replay
            .anchor
            .as_ref()
            .map(|anchor| format!("CUT {}", anchor.chain_seq))
            .unwrap_or_else(|| "CUT".to_string())
    };
    draw_two_sided(
        surface,
        rect.x as usize,
        y,
        width,
        &field.title,
        &mode,
        Style::new().fg(ACCENT).bg(BG).bold(),
    );
    y += 1;
    if y < bottom && !source_health.is_empty() {
        surface.draw_text(
            rect.x as usize,
            y,
            &truncate(&source_health, width),
            style_muted(),
        );
        y += 1;
    }

    if y < bottom && !field.layers.is_empty() {
        let layers = field
            .layers
            .iter()
            .map(|layer| {
                format!(
                    "{} {}({})",
                    if layer.visible { "●" } else { "○" },
                    layer.label,
                    layer.count
                )
            })
            .collect::<Vec<_>>()
            .join(" · ");
        surface.draw_text(rect.x as usize, y, &truncate(&layers, width), style_muted());
        y += 1;
    }

    if y < bottom {
        let search = if field.search.query.is_empty() {
            "search —".to_string()
        } else {
            format!(
                "search {:?} {}/{}",
                field.search.query,
                field
                    .search
                    .active_match
                    .as_ref()
                    .and_then(|active| field.search.match_ids.iter().position(|id| id == active))
                    .map(|index| index + 1)
                    .unwrap_or_default(),
                field.search.match_ids.len()
            )
        };
        let replay = format!(
            "{}{}{}",
            if field.replay.previous.is_some() {
                "◀"
            } else {
                "·"
            },
            if field.replay.playing { "▶" } else { "Ⅱ" },
            if field.replay.next.is_some() {
                "▶"
            } else {
                "·"
            },
        );
        draw_two_sided(
            surface,
            rect.x as usize,
            y,
            width,
            &search,
            &replay,
            style_muted(),
        );
        y += 1;
    }

    if y < bottom && !field.replay.rail.is_empty() {
        let selected_rail = field
            .replay
            .rail
            .iter()
            .position(|entry| entry.selected)
            .unwrap_or_else(|| field.replay.rail.len().saturating_sub(1));
        let start = selected_rail.saturating_sub(2);
        let rail = field
            .replay
            .rail
            .iter()
            .skip(start)
            .take(5)
            .map(|entry| {
                format!(
                    "{}{} {}",
                    if entry.selected { "◆" } else { "·" },
                    entry.event.chain_seq,
                    entry.label
                )
            })
            .collect::<Vec<_>>()
            .join(" ─ ");
        surface.draw_text(rect.x as usize, y, &truncate(&rail, width), style_muted());
        y += 1;
    }

    let body_start_y = y;
    let hidden_layers = field
        .layers
        .iter()
        .filter(|layer| !layer.visible)
        .map(|layer| layer.id.as_str())
        .collect::<BTreeSet<_>>();
    let by_id = field
        .entities
        .iter()
        .map(|entity| (entity.id.as_str(), entity))
        .collect::<BTreeMap<_, _>>();
    let selected = field.selected.as_deref();

    let mut lines = Vec::<FieldLine<'_>>::new();
    for group in &field.groups {
        let count = field
            .entities
            .iter()
            .filter(|entity| entity.group_id.as_deref() == Some(group.id.as_str()))
            .count();
        lines.push(FieldLine::Group {
            label: &group.label,
            count,
            collapsed: group.collapsed,
            selected_within: selected.is_some_and(|selected| {
                field.entities.iter().any(|entity| {
                    entity.id == selected && entity.group_id.as_deref() == Some(group.id.as_str())
                })
            }),
        });
        if group.collapsed {
            continue;
        }
        for entity in field
            .entities
            .iter()
            .filter(|entity| entity.group_id.as_deref() == Some(group.id.as_str()))
        {
            if is_visible(entity, &hidden_layers) || selected == Some(entity.id.as_str()) {
                lines.push(FieldLine::Entity {
                    entity,
                    depth: entity_depth(entity, &by_id),
                });
            }
        }
    }
    for entity in field.entities.iter().filter(|entity| {
        entity.group_id.is_none()
            || !field
                .groups
                .iter()
                .any(|group| Some(group.id.as_str()) == entity.group_id.as_deref())
    }) {
        if is_visible(entity, &hidden_layers) || selected == Some(entity.id.as_str()) {
            lines.push(FieldLine::Entity {
                entity,
                depth: entity_depth(entity, &by_id),
            });
        }
    }

    let selected_entity = field
        .selected
        .as_deref()
        .and_then(|id| field.entities.iter().find(|entity| entity.id == id));
    let selected_source = selected_entity.and_then(|entity| {
        field
            .sources
            .iter()
            .find(|source| source.name == entity.source)
    });
    let detail_source = selected_source
        .filter(|source| {
            source.error.is_some() || source.phase == RyeOsSourcePhase::Error || source.truncated
        })
        .or_else(|| {
            field.sources.iter().find(|source| {
                source.error.is_some()
                    || source.phase == RyeOsSourcePhase::Error
                    || source.truncated
            })
        })
        .or(selected_source)
        .or_else(|| field.sources.first());
    let source_evidence = detail_source.and_then(|source| {
        if source.error.is_none() && source.revision.is_none() && source.evidence.is_empty() {
            return None;
        }
        let mut parts = vec![format!("source {}", source.name)];
        if let Some(error) = &source.error {
            parts.push(format!("error: {error}"));
        }
        if let Some(revision) = &source.revision {
            parts.push(format!("revision {revision}"));
        }
        if let Some(evidence) = source.evidence.first() {
            parts.push(format!("evidence: {evidence}"));
            if source.evidence.len() > 1 {
                parts.push(format!("+{} more", source.evidence.len() - 1));
            }
        }
        Some(parts.join(" · "))
    });
    let selected_preview = selected_entity.and_then(|entity| {
        entity
            .preview_ids
            .iter()
            .find_map(|id| field.previews.iter().find(|preview| &preview.id == id))
    });
    let grid_width = if width >= 58
        && selected_preview
            .and_then(|preview| preview.grid.as_ref())
            .is_some()
    {
        (width / 3).max(18)
    } else {
        0
    };
    let list_width = width.saturating_sub(grid_width + usize::from(grid_width > 0));
    let footer_lines = 1
        + field.warnings.len().min(3)
        + field.metrics.len().min(3)
        + usize::from(field.metrics.len() > 3)
        + field.subjects.len().min(2)
        + usize::from(field.subjects.len() > 2)
        + usize::from(selected_entity.is_some())
        + usize::from(selected_entity.is_some_and(|entity| entity.artifact_ref.is_some()))
        + usize::from(source_evidence.is_some())
        + usize::from(!field.provenance.is_empty());
    let footer_y = bottom.saturating_sub(footer_lines).max(y).min(bottom);
    let available = footer_y.saturating_sub(y);
    let selected_line = lines.iter().position(|line| match line {
        FieldLine::Entity { entity, .. } => selected == Some(entity.id.as_str()),
        FieldLine::Group {
            selected_within, ..
        } => *selected_within,
    });
    let offset = selected_line
        .map(|selected| selected.saturating_sub(available / 2))
        .unwrap_or_default()
        .min(lines.len().saturating_sub(available));
    for line in lines.iter().skip(offset).take(available) {
        match line {
            FieldLine::Group {
                label,
                count,
                collapsed,
                selected_within,
                ..
            } => {
                let marker = if *collapsed { "▸" } else { "▾" };
                let text = format!("{marker} {label} ({count})");
                let style = if *selected_within && *collapsed {
                    fill_line(surface, rect.x as usize, y, list_width, style_selected());
                    style_selected()
                } else {
                    style_muted()
                };
                surface.draw_text(rect.x as usize, y, &truncate(&text, list_width), style);
            }
            FieldLine::Entity { entity, depth } => {
                let style = if entity.selected {
                    style_selected()
                } else {
                    style_fg()
                };
                fill_line(surface, rect.x as usize, y, list_width, style);
                let indent = "  ".repeat((*depth).min(8) + 1);
                let glyph = entity_glyph(entity);
                let status = entity.status.as_deref().unwrap_or_default();
                let suffix = if status.is_empty() {
                    entity.kind.as_str()
                } else {
                    status
                };
                let right_width = display_width(suffix).min(list_width / 3);
                let left_width =
                    list_width.saturating_sub(right_width + usize::from(right_width > 0));
                let left = format!("{indent}{glyph} {}", entity.label);
                surface.draw_text(
                    rect.x as usize,
                    y,
                    &truncate(&left, left_width),
                    if entity.selected {
                        style
                    } else {
                        tone_style(entity.tone)
                    },
                );
                if right_width > 0 {
                    let suffix = truncate(suffix, right_width);
                    surface.draw_text(
                        rect.x as usize + list_width.saturating_sub(display_width(&suffix)),
                        y,
                        &suffix,
                        if entity.selected {
                            style
                        } else {
                            style_muted()
                        },
                    );
                }
            }
        }
        y += 1;
        if y >= bottom {
            break;
        }
    }

    if let Some(preview) = selected_preview
        && let Some(grid) = preview.grid.as_ref()
        && grid_width > 0
    {
        draw_indexed_grid(
            surface,
            Rect::new(
                rect.x + list_width as u16 + 1,
                body_start_y as u16,
                grid_width as u16,
                available as u16,
            ),
            &preview.label,
            grid,
        );
    }

    // Detail, warnings, evidence, and controls are stable chrome. Anchor them
    // to the bottom of the field instead of immediately after a short entity
    // list, where they can overwrite the adjacent preview grid.
    y = footer_y;

    if y < bottom
        && let Some(entity) = selected_entity
    {
        if let Some(reference) = &entity.artifact_ref {
            let coordinate = format!(
                "artifact {} / {}{}",
                reference.thread_id,
                reference.artifact_id,
                reference
                    .content_hash
                    .as_ref()
                    .map(|hash| format!(" @ {hash}"))
                    .unwrap_or_default()
            );
            let coordinate = if display_width(&coordinate) <= width {
                coordinate
            } else if width >= display_width("! artifact coordinate incomplete; widen view") {
                "! artifact coordinate incomplete; widen view".to_string()
            } else if width >= display_width("! incomplete") {
                "! incomplete".to_string()
            } else {
                "!".to_string()
            };
            surface.draw_text(
                rect.x as usize,
                y,
                &truncate(&coordinate, width),
                style_muted(),
            );
            y += 1;
        }
        if y >= bottom {
            return;
        }
        let expansion = field
            .expansions
            .iter()
            .find(|item| item.source == entity.source && item.root_id == entity.id);
        let change = field.changes.iter().find(|change| change.id == entity.id);
        let detail = format!(
            "{} · source {} · compare {}/2{}{}{}",
            entity.id,
            entity.source,
            field.compare.len(),
            expansion
                .map(|item| format!(
                    " · expanded {}{}",
                    item.entity_count,
                    if item.can_continue { "+" } else { "" }
                ))
                .unwrap_or_default(),
            change
                .map(|item| format!(" · {:?}", item.kind).to_ascii_lowercase())
                .unwrap_or_default(),
            relation_summary(field, &entity.id),
        );
        surface.draw_text(rect.x as usize, y, &truncate(&detail, width), style_muted());
        y += 1;
    }

    if y < bottom
        && let Some(detail) = &source_evidence
    {
        surface.draw_text(rect.x as usize, y, &truncate(detail, width), style_muted());
        y += 1;
    }

    for subject in field.subjects.iter().take(2) {
        if y >= bottom {
            break;
        }
        let mut coordinate = format!(
            "subject {} / {} / {}",
            subject.source, subject.kind, subject.id
        );
        if let Some(reference) = &subject.definition_ref {
            coordinate.push_str(&format!(" · definition {reference}"));
        }
        if let Some(digest) = &subject.effective_definition_digest {
            coordinate.push_str(&format!(" @ {digest}"));
        }
        let coordinate = if display_width(&coordinate) <= width {
            coordinate
        } else if width >= display_width("! subject coordinate incomplete; widen view") {
            "! subject coordinate incomplete; widen view".to_string()
        } else if width >= display_width("! incomplete") {
            "! incomplete".to_string()
        } else {
            "!".to_string()
        };
        surface.draw_text(rect.x as usize, y, &coordinate, style_muted());
        y += 1;
    }
    if y < bottom && field.subjects.len() > 2 {
        let text = format!("+{} more projected subjects", field.subjects.len() - 2);
        surface.draw_text(rect.x as usize, y, &truncate(&text, width), style_muted());
        y += 1;
    }

    for message in field.warnings.iter().take(3) {
        if y >= bottom {
            break;
        }
        let text = format!("! {message}");
        surface.draw_text(
            rect.x as usize,
            y,
            &truncate(&text, width),
            tone_style(ryeos_client_base::ui::view_model::RyeOsTone::Warn),
        );
        y += 1;
    }

    for metric in field.metrics.iter().take(3) {
        if y >= bottom {
            break;
        }
        let value = metric.value.to_string();
        let text = match &metric.unit {
            Some(unit) if !unit.is_empty() => format!("{}: {value} {unit}", metric.label),
            _ => format!("{}: {value}", metric.label),
        };
        surface.draw_text(rect.x as usize, y, &truncate(&text, width), style_muted());
        y += 1;
    }
    if y < bottom && field.metrics.len() > 3 {
        let text = format!("+{} more projected metrics", field.metrics.len() - 3);
        surface.draw_text(rect.x as usize, y, &truncate(&text, width), style_muted());
        y += 1;
    }

    if y < bottom && !field.provenance.is_empty() {
        let text = format!("evidence: {}", field.provenance.join(" · "));
        surface.draw_text(rect.x as usize, y, &truncate(&text, width), style_muted());
        y += 1;
    }

    if y < bottom {
        let hints = if field.selected_activation_intent().is_some() {
            "Enter activate · / search · n/N matches · [/] replay · p play · l live · ←/→ group · space compare · e expand"
        } else {
            "/ search · n/N matches · [/] replay · p play · l live · ←/→ group · space compare · e expand"
        };
        surface.draw_text(rect.x as usize, y, &truncate(hints, width), style_muted());
    }
}

fn relation_summary(field: &RyeOsFieldVm, entity_id: &str) -> String {
    let labels = field
        .relations
        .iter()
        .filter_map(|relation| {
            if relation.source_id == entity_id {
                Some(format!("{} → {}", relation.kind, relation.target_id))
            } else if relation.target_id == entity_id {
                Some(format!("{} ← {}", relation.kind, relation.source_id))
            } else {
                None
            }
        })
        .take(3)
        .collect::<Vec<_>>();
    if labels.is_empty() {
        String::new()
    } else {
        format!(" · {}", labels.join("; "))
    }
}

enum FieldLine<'a> {
    Group {
        label: &'a str,
        count: usize,
        collapsed: bool,
        selected_within: bool,
    },
    Entity {
        entity: &'a RyeOsFieldEntityVm,
        depth: usize,
    },
}

fn is_visible(entity: &RyeOsFieldEntityVm, hidden_layers: &BTreeSet<&str>) -> bool {
    entity.layer_ids.is_empty()
        || entity
            .layer_ids
            .iter()
            .any(|layer| !hidden_layers.contains(layer.as_str()))
}

fn entity_depth(entity: &RyeOsFieldEntityVm, by_id: &BTreeMap<&str, &RyeOsFieldEntityVm>) -> usize {
    let mut depth = 0usize;
    let mut parent = entity.parent_id.as_deref();
    let mut seen = BTreeSet::new();
    while let Some(parent_id) = parent {
        if depth >= 8 || !seen.insert(parent_id) {
            break;
        }
        depth += 1;
        parent = by_id
            .get(parent_id)
            .and_then(|parent| parent.parent_id.as_deref());
    }
    depth
}

fn entity_glyph(entity: &RyeOsFieldEntityVm) -> &'static str {
    match (entity.traits.shape, entity.traits.fill) {
        (FieldShape::Dot, _) => "·",
        (FieldShape::Disc, FieldFill::Hollow) | (FieldShape::Ring, _) => "○",
        (FieldShape::Disc, _) => "●",
        (FieldShape::Rect, FieldFill::Hollow) => "□",
        (FieldShape::Rect, _) => "■",
        (FieldShape::Capsule, _) => "▰",
        (FieldShape::Diamond, FieldFill::Hollow) => "◇",
        (FieldShape::Diamond, _) => "◆",
        (FieldShape::Hex, _) => "⬢",
        (FieldShape::Anchor, _) => "⌾",
        (FieldShape::Aggregate, _) => "◫",
        (FieldShape::Grid, _) => "▦",
    }
}

fn draw_two_sided(
    surface: &mut TextSurface,
    x: usize,
    y: usize,
    width: usize,
    left: &str,
    right: &str,
    style: Style,
) {
    let right = truncate(right, width / 3);
    let right_width = display_width(&right);
    let left_width = width.saturating_sub(right_width + usize::from(right_width > 0));
    surface.draw_text(x, y, &truncate(left, left_width), style);
    if right_width > 0 {
        surface.draw_text(x + width.saturating_sub(right_width), y, &right, style);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn fixture() -> RyeOsFieldVm {
        serde_json::from_value(serde_json::json!({
            "schema_version": "ryeos.ui.field.vm.v2",
            "id": "field:test",
            "title": "Execution field",
            "revision": "all",
            "structural_revision": "structure",
            "data_revision": "data",
            "local_revision": "local",
            "sources": [],
            "subjects": [],
            "groups": [{
                "id": "work", "label": "Work", "parent_id": null,
                "layout": "flow", "collapsed": true, "aggregate": null
            }],
            "layers": [{ "id": "live", "label": "Live", "visible": true, "count": 1 }],
            "entities": [{
                "id": "step:one", "source": "execution", "kind": "step", "role": null,
                "label": "Observe", "secondary": null, "parent_id": null,
                "group_id": "work", "layer_ids": ["live"], "lane": null,
                "rank": 1, "order": 1, "status": "running", "tone": "accent",
                "traits": {
                    "shape": "rect", "fill": "solid", "stroke": "solid",
                    "emphasis": "normal", "motion": "flow"
                },
                "badges": [], "preview_ids": [], "selected": true, "selectable": true,
                "select_intent": null, "activate_intent": null,
                "compare_available": false,
                "accessibility_label": "Observe; running", "detail": []
            }],
            "relations": [], "previews": [], "metrics": [],
            "traversal": ["step:one"], "selected": "step:one", "compare": [],
            "cursor": {
                "chain_root_id": "T-root", "chain_seq": 4, "event_hash": "event:4"
            },
            "replay": {
                "mode": "braid_cut", "playing": false,
                "anchor": { "chain_root_id": "T-root", "chain_seq": 4, "event_hash": "event:4" },
                "previous": { "chain_root_id": "T-root", "chain_seq": 3, "event_hash": "event:3" },
                "next": { "chain_root_id": "T-root", "chain_seq": 5, "event_hash": "event:5" },
                "live_head": { "chain_root_id": "T-root", "chain_seq": 5, "event_hash": "event:5" },
                "rail": [{
                    "event": { "chain_root_id": "T-root", "chain_seq": 4, "event_hash": "event:4" },
                    "label": "observe", "selected": true
                }],
                "outside_cut": []
            },
            "search": {
                "query": "observe", "match_ids": ["step:one"],
                "active_match": "step:one", "truncated": false
            },
            "expansions": [], "changes": [], "warnings": [],
            "provenance": ["service:field/execution"]
        }))
        .expect("valid field fixture")
    }

    fn row_text(surface: &TextSurface, y: usize) -> String {
        (0..surface.width).map(|x| surface.get(x, y).rune).collect()
    }

    #[test]
    fn collapsed_selected_group_keeps_selection_replay_and_controls_visible() {
        let mut surface = TextSurface::new(80, 30);
        draw_field(&mut surface, Rect::new(0, 0, 80, 30), &fixture());
        let text = (0..30)
            .map(|y| row_text(&surface, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("CUT 4"));
        assert!(text.contains("● Live(1)"));
        assert!(text.contains("◆4 observe"));
        assert!(text.contains("▸ Work (1)"));
        assert!(text.contains("/ search"));
        let group_row = (0..30)
            .find(|y| row_text(&surface, *y).contains("▸ Work (1)"))
            .expect("group row");
        assert_eq!(surface.get(0, group_row).bg, ACCENT);
    }

    #[test]
    fn footer_is_bottom_anchored_when_the_entity_list_is_short() {
        let mut surface = TextSurface::new(80, 30);
        draw_field(&mut surface, Rect::new(0, 0, 80, 30), &fixture());

        assert!(row_text(&surface, 29).contains("/ search"));
        assert!(row_text(&surface, 28).contains("evidence:"));
        assert!(row_text(&surface, 27).contains("step:one"));
    }

    #[test]
    fn selected_activation_hint_requires_an_admitted_intent() {
        let mut field = fixture();
        let mut surface = TextSurface::new(80, 30);
        draw_field(&mut surface, Rect::new(0, 0, 80, 30), &field);
        assert!(!row_text(&surface, 29).contains("Enter activate"));

        field.entities[0].activate_intent = Some(ryeos_client_base::ui::RyeOsUiIntent::Refresh);
        let mut actionable = TextSurface::new(80, 30);
        draw_field(&mut actionable, Rect::new(0, 0, 80, 30), &field);
        assert!(row_text(&actionable, 29).starts_with("Enter activate"));

        field.entities[0].activate_intent = None;
        field.relations.push(
            serde_json::from_value(serde_json::json!({
                "id":"edge:one", "kind":"link", "source_id":"step:one", "target_id":"step:two",
                "directed":true, "tone":"accent", "stroke":"solid", "emphasis":"normal",
                "motion":"none", "label":"Follow", "layer_ids":[], "selected":false,
                "activate_intent":{"type":"refresh"}, "accessibility_label":"Follow"
            }))
            .unwrap(),
        );
        let mut relation = TextSurface::new(80, 30);
        draw_field(&mut relation, Rect::new(0, 0, 80, 30), &field);
        assert!(row_text(&relation, 29).starts_with("Enter activate"));

        let mut second = field.relations[0].clone();
        second.id = "edge:two".into();
        field.relations.push(second);
        let mut ambiguous = TextSurface::new(80, 30);
        draw_field(&mut ambiguous, Rect::new(0, 0, 80, 30), &field);
        assert!(!row_text(&ambiguous, 29).contains("Enter activate"));

        field.selected = None;
        let mut unselected = TextSurface::new(80, 30);
        draw_field(&mut unselected, Rect::new(0, 0, 80, 30), &field);
        assert!(!row_text(&unselected, 29).contains("Enter activate"));
    }

    #[test]
    fn projected_metrics_are_visible_with_explicit_bounded_omission() {
        let mut field = fixture();
        field.metrics = (0..5)
            .map(|index| ryeos_client_base::ui::field::RyeOsMetricVm {
                id: format!("metric:{index}"),
                label: format!("Metric {index}"),
                value: serde_json::json!(index % 2 == 0),
                unit: Some("count".into()),
            })
            .collect();
        let mut surface = TextSurface::new(80, 30);
        draw_field(&mut surface, Rect::new(0, 0, 80, 30), &field);
        let text = (0..30)
            .map(|y| row_text(&surface, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Metric 0: true count"));
        assert!(text.contains("Metric 2: true count"));
        assert!(text.contains("+2 more projected metrics"));
        assert!(!text.contains("Metric 3:"));
    }

    #[test]
    fn selected_artifact_coordinate_is_complete_or_explicitly_incomplete() {
        let mut field = fixture();
        let hash = "a".repeat(64);
        field.entities[0].artifact_ref = Some(
            serde_json::from_value(serde_json::json!({
                "thread_id": "T-root", "artifact_id": 27, "content_hash": hash.clone()
            }))
            .unwrap(),
        );
        let mut narrow = TextSurface::new(80, 30);
        draw_field(&mut narrow, Rect::new(0, 0, 80, 30), &field);
        let narrow_rows = (0..30).map(|y| row_text(&narrow, y)).collect::<Vec<_>>();
        assert!(
            narrow_rows
                .iter()
                .any(|row| row.contains("artifact coordinate incomplete"))
        );
        assert!(narrow_rows.iter().any(|row| row.contains("step:one")));
        assert!(!narrow_rows.iter().any(|row| row.contains(&hash)));

        let mut compact = TextSurface::new(20, 30);
        draw_field(&mut compact, Rect::new(0, 0, 20, 30), &field);
        assert!((0..30).any(|y| row_text(&compact, y).contains("! incomplete")));

        let mut wide = TextSurface::new(140, 30);
        draw_field(&mut wide, Rect::new(0, 0, 140, 30), &field);
        let wide_rows = (0..30).map(|y| row_text(&wide, y)).collect::<Vec<_>>();
        assert!(
            wide_rows
                .iter()
                .any(|row| { row.contains("artifact T-root / 27 @") && row.contains(&hash) })
        );
    }

    #[test]
    fn projected_subjects_remain_visible_without_entities_and_bound_overflow() {
        let mut field = fixture();
        field.entities.clear();
        field.selected = None;
        field.subjects = (0..4)
            .map(|index| ryeos_client_base::ui::field::RyeOsFieldSubjectVm {
                source: "execution".into(),
                kind: "thread".into(),
                id: format!("T-subject-{index}"),
                definition_ref: Some("graph:arc3/solve/game_solver".into()),
                effective_definition_digest: Some(format!("digest:{index}")),
            })
            .collect();
        let mut surface = TextSurface::new(140, 30);
        draw_field(&mut surface, Rect::new(0, 0, 140, 30), &field);
        let text = (0..30)
            .map(|y| row_text(&surface, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("subject execution / thread / T-subject-0"));
        assert!(text.contains("definition graph:arc3/solve/game_solver @ digest:0"));
        assert!(text.contains("subject execution / thread / T-subject-1"));
        assert!(text.contains("+2 more projected subjects"));
        assert!(!text.contains("T-subject-2"));
    }

    #[test]
    fn narrow_subject_coordinate_is_not_silently_truncated() {
        let mut field = fixture();
        let exact_id = format!("T-{}", "a".repeat(64));
        field.subjects = vec![ryeos_client_base::ui::field::RyeOsFieldSubjectVm {
            source: "execution".into(),
            kind: "thread".into(),
            id: exact_id.clone(),
            definition_ref: None,
            effective_definition_digest: None,
        }];
        let mut surface = TextSurface::new(20, 30);
        draw_field(&mut surface, Rect::new(0, 0, 20, 30), &field);
        let text = (0..30)
            .map(|y| row_text(&surface, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("! incomplete"));
        assert!(!text.contains(&exact_id));
    }

    #[test]
    fn bounded_and_outside_cut_source_evidence_is_visible_in_terminal_field() {
        let mut field = fixture();
        field.sources.push(
            serde_json::from_value(serde_json::json!({
                "name": "execution", "source_ref": "service:field/execution",
                "subject_fingerprint": null, "revision": "revision:42",
                "phase": "error", "truncated": true,
                "error": "retained source unavailable", "evidence": ["signed:42"], "evidence_omitted": 0
            }))
            .unwrap(),
        );
        field.replay.outside_cut = vec!["concurrent:one".into(), "concurrent:two".into()];
        field.search.truncated = true;
        let mut surface = TextSurface::new(140, 30);
        draw_field(&mut surface, Rect::new(0, 0, 140, 30), &field);
        let text = (0..30)
            .map(|y| row_text(&surface, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("execution:error:bounded:error"));
        assert!(text.contains("outside cut:2"));
        assert!(text.contains("bounded search"));
        assert!(text.contains("retained source unavailable"));
        assert!(text.contains("revision revision:42"));
        assert!(text.contains("evidence: signed:42"));
    }

    #[test]
    fn compact_field_prioritizes_failure_over_healthy_selected_source() {
        let mut field = fixture();
        field.sources = vec![
            serde_json::from_value(serde_json::json!({
                "name":"execution", "source_ref":"service:field/execution",
                "subject_fingerprint":null, "revision":"healthy", "phase":"ready",
                "truncated":false, "error":null, "evidence":[], "evidence_omitted":0
            }))
            .unwrap(),
            serde_json::from_value(serde_json::json!({
                "name":"board", "source_ref":"service:field/board",
                "subject_fingerprint":null, "revision":null, "phase":"error",
                "truncated":true, "error":"board unavailable", "evidence":[], "evidence_omitted":0
            }))
            .unwrap(),
            serde_json::from_value(serde_json::json!({
                "name":"archive", "source_ref":"service:field/archive",
                "subject_fingerprint":null, "revision":null, "phase":"error",
                "truncated":false, "error":null, "evidence":[], "evidence_omitted":0
            }))
            .unwrap(),
        ];
        field.replay.outside_cut = vec!["concurrent".into()];
        field.search.truncated = true;
        let mut surface = TextSurface::new(80, 30);
        draw_field(&mut surface, Rect::new(0, 0, 80, 30), &field);
        let health = row_text(&surface, 1);
        assert!(health.contains("source errors:2"));
        assert!(health.contains("bounded sources:1"));
        assert!(health.contains("outside cut:1"));
        assert!(health.contains("bounded search"));
        let text = (0..30)
            .map(|y| row_text(&surface, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("source board · error: board unavailable"));
    }

    #[test]
    fn fixed_scale_field_renders_to_80_by_30_within_release_gate() {
        let mut field = fixture();
        field.groups[0].collapsed = false;
        let prototype = field.entities[0].clone();
        field.entities = (0..1_000)
            .map(|index| {
                let mut entity = prototype.clone();
                entity.id = format!("entity:{index}");
                entity.label = format!("Entity {index}");
                entity.order = Some(index);
                entity.selected = index == 500;
                entity
            })
            .collect();
        field.traversal = field
            .entities
            .iter()
            .map(|entity| entity.id.clone())
            .collect();
        field.selected = Some("entity:500".to_string());
        field.layers[0].count = 1_000;
        let iterations = if cfg!(debug_assertions) { 1 } else { 50 };
        let mut samples = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let started = Instant::now();
            let mut surface = TextSurface::new(80, 30);
            draw_field(&mut surface, Rect::new(0, 0, 80, 30), &field);
            assert_eq!(row_text(&surface, 16).chars().count(), 80);
            samples.push(started.elapsed());
        }
        samples.sort();
        let p95 = samples[(samples.len() * 95).div_ceil(100).saturating_sub(1)];
        if !cfg!(debug_assertions) {
            assert!(
                p95.as_millis() <= 50,
                "80x30 field render p95 was {p95:?} (limit 50ms)"
            );
        }
    }
}
