//! The field graph: a layered, deterministic layout of the core's field
//! projection, drawn as vector strokes with each entity a real element.
//!
//! Placement follows the browser shell's layout exactly (semantic rank, lane
//! and group drive position; stable ids break every tie), so the two
//! renderers put the same entity in the same place. Entities are positioned
//! elements rather than painted pixels, which gives press, focus and an
//! accessible name to each without hit-testing.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rune::{Color, El, Key, KeyEvent, LineCap, Shape, canvas, col, el, row, text, text_input};
use ryeos_client_base::ids::RyeOsViewInstanceKey;
use ryeos_client_base::ui::event::FieldStepDirection;
use ryeos_client_base::ui::field::{
    FieldEmphasis, FieldFill, FieldShape, FieldStroke, RyeOsArtifactPreviewVm, RyeOsFieldEntityVm,
    RyeOsFieldExpansionVm, RyeOsFieldRelationVm, RyeOsFieldVm, RyeOsIndexedGridVm,
    RyeOsSourcePhase,
};
use ryeos_client_base::ui::view_model::RyeOsTone;
use ryeos_client_base::ui::{RyeOsUiEvent, RyeOsUiIntent};
use ryeos_client_base::view_set::{FieldCursorState, FieldEventRefState};

use crate::theme::{self, Palette};
use crate::{App, Local};

const GROUP_WIDTH: f32 = 520.0;
const RANK_SPACING: f32 = 210.0;
const GROUP_RIGHT_GUTTER: f32 = 300.0;
const GROUP_HEADER_HEIGHT: f32 = 26.0;

/// One placed entity, centred on `x`, `y`.
struct Node<'a> {
    entity: &'a RyeOsFieldEntityVm,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    /// Members of its strongly connected component; above one is a cycle.
    cycle: usize,
}

struct GroupBox<'a> {
    id: &'a str,
    label: &'a str,
    collapsed: bool,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

struct Layout<'a> {
    nodes: Vec<Node<'a>>,
    edges: Vec<(&'a RyeOsFieldRelationVm, Vec<[f32; 2]>)>,
    groups: Vec<GroupBox<'a>>,
    width: f32,
    height: f32,
}

/// Entities a hidden layer or a collapsed group removes, unless selected:
/// selection reveals, so a search match is always reachable.
fn visible(field: &RyeOsFieldVm) -> Vec<&RyeOsFieldEntityVm> {
    let hidden: BTreeSet<&str> = field
        .layers
        .iter()
        .filter(|layer| !layer.visible)
        .map(|layer| layer.id.as_str())
        .collect();
    let collapsed: BTreeSet<&str> = field
        .groups
        .iter()
        .filter(|group| group.collapsed)
        .map(|group| group.id.as_str())
        .collect();
    field
        .entities
        .iter()
        .filter(|entity| {
            if entity.selected {
                return true;
            }
            if entity
                .group_id
                .as_deref()
                .is_some_and(|group| collapsed.contains(group))
            {
                return false;
            }
            entity.layer_ids.is_empty()
                || entity
                    .layer_ids
                    .iter()
                    .any(|layer| !hidden.contains(layer.as_str()))
        })
        .collect()
}

/// Tarjan's components over the visible graph, visiting ids and targets in
/// sorted order so the result is deterministic.
fn components(ids: &[&str], relations: &[&RyeOsFieldRelationVm]) -> Vec<Vec<String>> {
    let mut graph: BTreeMap<&str, Vec<&str>> = ids.iter().map(|id| (*id, Vec::new())).collect();
    for relation in relations {
        if let Some(targets) = graph.get_mut(relation.source_id.as_str()) {
            targets.push(relation.target_id.as_str());
        }
    }
    for targets in graph.values_mut() {
        targets.sort_unstable();
    }
    struct Walk<'g> {
        graph: &'g BTreeMap<&'g str, Vec<&'g str>>,
        next: usize,
        index: HashMap<&'g str, usize>,
        low: HashMap<&'g str, usize>,
        stack: Vec<&'g str>,
        on_stack: BTreeSet<&'g str>,
        out: Vec<Vec<String>>,
    }
    impl<'g> Walk<'g> {
        fn visit(&mut self, id: &'g str) {
            self.index.insert(id, self.next);
            self.low.insert(id, self.next);
            self.next += 1;
            self.stack.push(id);
            self.on_stack.insert(id);
            let targets = self.graph.get(id).cloned().unwrap_or_default();
            for target in targets {
                if !self.index.contains_key(target) {
                    self.visit(target);
                    let low = self.low[id].min(self.low[target]);
                    self.low.insert(id, low);
                } else if self.on_stack.contains(target) {
                    let low = self.low[id].min(self.index[target]);
                    self.low.insert(id, low);
                }
            }
            if self.low[id] == self.index[id] {
                let mut component = Vec::new();
                while let Some(member) = self.stack.pop() {
                    self.on_stack.remove(member);
                    component.push(member.to_owned());
                    if member == id {
                        break;
                    }
                }
                component.sort();
                self.out.push(component);
            }
        }
    }
    let mut walk = Walk {
        graph: &graph,
        next: 0,
        index: HashMap::new(),
        low: HashMap::new(),
        stack: Vec::new(),
        on_stack: BTreeSet::new(),
        out: Vec::new(),
    };
    let mut sorted: Vec<&str> = ids.to_vec();
    sorted.sort_unstable();
    for id in sorted {
        if !walk.index.contains_key(id) {
            walk.visit(id);
        }
    }
    walk.out
}

/// Each component's rank: its authored rank when one member declares it,
/// otherwise one past its deepest predecessor in topological order.
fn rank_components(
    components: &[Vec<String>],
    component_of: &HashMap<&str, usize>,
    relations: &[&RyeOsFieldRelationVm],
    entities: &[&RyeOsFieldEntityVm],
) -> HashMap<usize, i64> {
    let authored: HashMap<usize, i64> = entities
        .iter()
        .filter_map(|entity| Some((component_of[entity.id.as_str()], entity.rank?)))
        .collect();
    let mut incoming: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); components.len()];
    let mut outgoing: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); components.len()];
    for relation in relations {
        let source = component_of[relation.source_id.as_str()];
        let target = component_of[relation.target_id.as_str()];
        if source != target {
            outgoing[source].insert(target);
            incoming[target].insert(source);
        }
    }
    let mut remaining = incoming.clone();
    let mut queue: BTreeSet<usize> = (0..components.len())
        .filter(|index| remaining[*index].is_empty())
        .collect();
    let mut ranks = HashMap::new();
    while let Some(component) = queue.pop_first() {
        let rank = authored.get(&component).copied().unwrap_or_else(|| {
            incoming[component]
                .iter()
                .map(|parent| ranks.get(parent).copied().unwrap_or(0) + 1)
                .max()
                .unwrap_or(0)
                .max(0)
        });
        ranks.insert(component, rank);
        for next in &outgoing[component] {
            remaining[*next].remove(&component);
            if remaining[*next].is_empty() {
                queue.insert(*next);
            }
        }
    }
    for index in 0..components.len() {
        ranks
            .entry(index)
            .or_insert_with(|| authored.get(&index).copied().unwrap_or(0));
    }
    ranks
}

fn route(source: &Node, target: &Node) -> Vec<[f32; 2]> {
    let middle = (source.x + target.x) / 2.0;
    vec![
        [source.x + source.width / 2.0, source.y],
        [middle, source.y],
        [middle, target.y],
        [target.x - target.width / 2.0, target.y],
    ]
}

fn layout(field: &RyeOsFieldVm) -> Layout<'_> {
    let shown = visible(field);
    let ids: Vec<&str> = shown.iter().map(|entity| entity.id.as_str()).collect();
    let id_set: BTreeSet<&str> = ids.iter().copied().collect();
    let relations: Vec<&RyeOsFieldRelationVm> = field
        .relations
        .iter()
        .filter(|relation| {
            id_set.contains(relation.source_id.as_str())
                && id_set.contains(relation.target_id.as_str())
        })
        .collect();
    let components = components(&ids, &relations);
    let mut component_of: HashMap<&str, usize> = HashMap::new();
    for (index, members) in components.iter().enumerate() {
        for member in members {
            if let Some(id) = id_set.get(member.as_str()) {
                component_of.insert(id, index);
            }
        }
    }
    let ranks = rank_components(&components, &component_of, &relations, &shown);
    let group_order: HashMap<&str, usize> = field
        .groups
        .iter()
        .enumerate()
        .map(|(index, group)| (group.id.as_str(), index))
        .collect();
    let lanes: BTreeSet<&str> = shown
        .iter()
        .filter_map(|entity| entity.lane.as_deref())
        .filter(|lane| !lane.is_empty())
        .collect();
    let lane_order: HashMap<&str, usize> = lanes
        .into_iter()
        .enumerate()
        .map(|(index, lane)| (lane, index))
        .collect();

    // (group, rank, lane) buckets, iterated in key order.
    let mut buckets: BTreeMap<(usize, i64, usize), Vec<&RyeOsFieldEntityVm>> = BTreeMap::new();
    for entity in &shown {
        let group = entity
            .group_id
            .as_deref()
            .and_then(|group| group_order.get(group).copied())
            .unwrap_or(group_order.len());
        let rank = entity
            .rank
            .unwrap_or_else(|| ranks[&component_of[entity.id.as_str()]]);
        let lane = entity
            .lane
            .as_deref()
            .and_then(|lane| lane_order.get(lane).copied())
            .unwrap_or(lane_order.len());
        buckets.entry((group, rank, lane)).or_default().push(entity);
    }
    for entities in buckets.values_mut() {
        entities.sort_by(|left, right| {
            left.order
                .unwrap_or(0)
                .cmp(&right.order.unwrap_or(0))
                .then_with(|| left.id.cmp(&right.id))
        });
    }

    // A group's column is as wide as its deepest rank, so later groups start
    // after the span the earlier one actually occupies.
    let mut deepest: BTreeMap<usize, i64> =
        (0..field.groups.len()).map(|index| (index, 0)).collect();
    for (group, rank, _) in buckets.keys() {
        let entry = deepest.entry(*group).or_insert(0);
        *entry = (*entry).max(*rank);
    }
    let mut origins: HashMap<usize, f32> = HashMap::new();
    let mut next_origin = 0.0;
    for (group, rank) in &deepest {
        origins.insert(*group, next_origin);
        next_origin += GROUP_WIDTH.max(*rank as f32 * RANK_SPACING + GROUP_RIGHT_GUTTER);
    }

    let mut nodes = Vec::new();
    for ((group, rank, lane), entities) in &buckets {
        for (index, entity) in entities.iter().enumerate() {
            nodes.push(Node {
                entity,
                x: 120.0 + origins.get(group).copied().unwrap_or(0.0) + *rank as f32 * RANK_SPACING,
                y: 90.0 + *lane as f32 * 150.0 + index as f32 * 86.0,
                width: if entity.traits.shape == FieldShape::Aggregate {
                    156.0
                } else {
                    136.0
                },
                height: if entity.traits.shape == FieldShape::Grid {
                    74.0
                } else {
                    52.0
                },
                cycle: components[component_of[entity.id.as_str()]].len(),
            });
        }
    }
    let at: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.entity.id.as_str(), index))
        .collect();
    let edges = relations
        .iter()
        .map(|relation| {
            let points = route(
                &nodes[at[relation.source_id.as_str()]],
                &nodes[at[relation.target_id.as_str()]],
            );
            (*relation, points)
        })
        .collect();
    let groups: Vec<GroupBox> = field
        .groups
        .iter()
        .enumerate()
        .map(|(index, group)| {
            let members: Vec<&Node> = nodes
                .iter()
                .filter(|node| node.entity.group_id.as_deref() == Some(group.id.as_str()))
                .collect();
            let (x, y, width, height) = if members.is_empty() {
                let origin = origins
                    .get(&index)
                    .copied()
                    .unwrap_or(index as f32 * GROUP_WIDTH);
                (20.0 + origin, 20.0, 200.0, GROUP_HEADER_HEIGHT + 12.0)
            } else {
                let min_x = members
                    .iter()
                    .map(|n| n.x - n.width / 2.0)
                    .fold(f32::MAX, f32::min)
                    - 32.0;
                let max_x = members
                    .iter()
                    .map(|n| n.x + n.width / 2.0)
                    .fold(f32::MIN, f32::max)
                    + 32.0;
                let min_y = members
                    .iter()
                    .map(|n| n.y - n.height / 2.0)
                    .fold(f32::MAX, f32::min)
                    - 42.0;
                let max_y = members
                    .iter()
                    .map(|n| n.y + n.height / 2.0)
                    .fold(f32::MIN, f32::max)
                    + 28.0;
                (min_x, min_y, max_x - min_x, max_y - min_y)
            };
            GroupBox {
                id: &group.id,
                label: if group.label.is_empty() {
                    &group.id
                } else {
                    &group.label
                },
                collapsed: group.collapsed,
                x,
                y,
                width,
                height,
            }
        })
        .collect();
    let width = nodes
        .iter()
        .map(|node| node.x + node.width + 100.0)
        .chain(groups.iter().map(|group| group.x + group.width + 40.0))
        .fold(640.0, f32::max);
    let height = nodes
        .iter()
        .map(|node| node.y + node.height + 100.0)
        .chain(groups.iter().map(|group| group.y + group.height + 40.0))
        .fold(360.0, f32::max);
    Layout {
        nodes,
        edges,
        groups,
        width,
        height,
    }
}

// --- drawing -------------------------------------------------------------

fn tone(p: &Palette, tone: RyeOsTone) -> Color {
    p.tone(tone, p.muted)
}

/// Split a polyline into dashes of `on` points separated by `off`.
fn dashes(points: &[[f32; 2]], closed: bool, on: f32, off: f32) -> Vec<Vec<[f32; 2]>> {
    let mut path = points.to_vec();
    if closed && let Some(first) = points.first() {
        path.push(*first);
    }
    let mut out = Vec::new();
    let mut current: Vec<[f32; 2]> = Vec::new();
    let mut drawing = true;
    let mut left = on;
    for pair in path.windows(2) {
        let [mut a, b] = [pair[0], pair[1]];
        let mut length = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
        while length > 0.0 {
            let step = left.min(length);
            let t = step / length;
            let end = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
            if drawing {
                if current.is_empty() {
                    current.push(a);
                }
                current.push(end);
            }
            a = end;
            length -= step;
            left -= step;
            if left <= 0.0 {
                if drawing && current.len() >= 2 {
                    out.push(std::mem::take(&mut current));
                }
                current.clear();
                drawing = !drawing;
                left = if drawing { on } else { off };
            }
        }
    }
    if drawing && current.len() >= 2 {
        out.push(current);
    }
    out
}

fn stroke(
    shape: Shape,
    points: Vec<[f32; 2]>,
    closed: bool,
    style: FieldStroke,
    width: f32,
    color: Color,
) -> Shape {
    let pattern = match style {
        FieldStroke::Solid => None,
        FieldStroke::Dashed => Some((7.0, 5.0)),
        FieldStroke::Dotted => Some((2.0, 4.0)),
    };
    match pattern {
        None => shape.stroke_with(points, closed, width, LineCap::Round, color),
        Some((on, off)) => dashes(&points, closed, on, off)
            .into_iter()
            .fold(shape, |shape, dash| {
                shape.stroke_with(dash, false, width, LineCap::Butt, color)
            }),
    }
}

/// The outline of an entity shape inside a `width` × `height` box at the
/// origin.
fn outline(shape: FieldShape, width: f32, height: f32) -> Vec<[f32; 2]> {
    let (cx, cy, rx, ry) = (width / 2.0, height / 2.0, width / 2.0, height / 2.0);
    match shape {
        FieldShape::Disc | FieldShape::Ring | FieldShape::Dot => (0..48)
            .map(|step| {
                let angle = std::f32::consts::TAU * step as f32 / 48.0;
                [cx + angle.cos() * rx, cy + angle.sin() * ry]
            })
            .collect(),
        FieldShape::Diamond => vec![[cx, 0.0], [width, cy], [cx, height], [0.0, cy]],
        FieldShape::Hex => (0..6)
            .map(|step| {
                let angle = std::f32::consts::PI / 3.0 * step as f32;
                [cx + angle.cos() * rx, cy + angle.sin() * ry]
            })
            .collect(),
        FieldShape::Capsule => rounded(width, height, height / 2.0),
        _ => rounded(width, height, 7.0),
    }
}

fn rounded(width: f32, height: f32, radius: f32) -> Vec<[f32; 2]> {
    let r = radius.min(width / 2.0).min(height / 2.0);
    let corners = [
        (width - r, r, -90.0f32),
        (width - r, height - r, 0.0),
        (r, height - r, 90.0),
        (r, r, 180.0),
    ];
    let mut points = Vec::new();
    for (cx, cy, start) in corners {
        for step in 0..=6 {
            let angle = (start + 15.0 * step as f32).to_radians();
            points.push([cx + angle.cos() * r, cy + angle.sin() * r]);
        }
    }
    points
}

fn clip_label(value: &str, length: usize) -> String {
    if value.chars().count() <= length {
        value.to_owned()
    } else {
        let head: String = value.chars().take(length - 1).collect();
        format!("{head}…")
    }
}

fn ui(event: RyeOsUiEvent) -> impl Fn(&mut App) + 'static {
    move |app: &mut App| app.ui(event.clone())
}

/// A field control: small, muted and ruled, as the browser shell draws them.
fn button(label: impl Into<String>, enabled: bool, event: Option<RyeOsUiEvent>) -> El {
    control(None, label.into(), enabled, event, false)
}

/// A control led by an icon; `label` (possibly empty) follows it.
fn icon_button(
    icon: &'static rune::Icon,
    label: impl Into<String>,
    enabled: bool,
    event: Option<RyeOsUiEvent>,
) -> El {
    control(Some(icon), label.into(), enabled, event, false)
}

/// A control that shows an on state (a visible layer, the rail's cut).
fn toggle(
    icon: Option<&'static rune::Icon>,
    label: impl Into<String>,
    pressed: bool,
    event: RyeOsUiEvent,
) -> El {
    control(icon, label.into(), true, Some(event), pressed).focus_selected(pressed)
}

fn control(
    lead: Option<&'static rune::Icon>,
    label: String,
    enabled: bool,
    event: Option<RyeOsUiEvent>,
    pressed: bool,
) -> El {
    let p = Palette::get();
    let ink = if pressed { p.yellow } else { p.muted };
    let mut b = row()
        .px(6.0)
        .py(4.0)
        .gap(4.0)
        .center()
        .border(1.0, if pressed { p.yellow } else { p.line })
        .role("button")
        .focusable(enabled);
    if let Some(icon) = lead {
        b = b.child(crate::icons::glyph(icon, 10.0, ink));
    }
    if !label.is_empty() {
        b = b.child(text(label).font(theme::data(9.0)).color(ink));
    }
    match (enabled, event) {
        (true, Some(event)) => b = b.hover(p.plane_raised).on_press(ui(event)),
        _ => b = b.disabled(true).opacity(0.5),
    }
    b
}

fn activate(intent: &Option<RyeOsUiIntent>) -> Option<RyeOsUiEvent> {
    intent
        .clone()
        .map(|intent| RyeOsUiEvent::Activate { intent })
}

/// The key a mounted field's search text is kept under.
pub(crate) fn query_key(instance: &RyeOsViewInstanceKey) -> String {
    format!("field-query:{instance:?}")
}

pub fn view(field: &RyeOsFieldVm, instance: &RyeOsViewInstanceKey, local: &Local) -> El {
    let selected = field
        .selected
        .as_deref()
        .and_then(|id| field.entities.iter().find(|entity| entity.id == id));
    let expansion = selected.and_then(|selected| {
        field
            .expansions
            .iter()
            .find(|item| item.source == selected.source && item.root_id == selected.id)
    });
    // The stage fills the view; the toolbar, detail, evidence and rail float
    // over it, as in the browser shell.
    let mut root = el()
        .key(format!("field:{instance:?}"))
        .w_full()
        .h_full()
        .min_h(240.0)
        .label(field.title.clone())
        .clip_rounded(0.0)
        .child(stage(field, instance))
        .child(toolbar(field, instance, selected, expansion, local).pin(
            Some(0.0),
            Some(0.0),
            Some(0.0),
            None,
        ));
    let rail_shown = !field.replay.rail.is_empty();
    if rail_shown {
        root = root.child(rail(field, instance).pin(Some(10.0), None, Some(10.0), Some(10.0)));
    }
    if has_evidence(field, expansion) {
        let bottom = if rail_shown { 44.0 } else { 10.0 };
        root = root.child(evidence(field, instance, expansion, local).pin(
            Some(10.0),
            None,
            None,
            Some(bottom),
        ));
    }
    if selected.is_some() || !field.compare.is_empty() || !field.warnings.is_empty() {
        root =
            root.child(detail(field, instance, selected, local).pin(None, Some(58.0), Some(12.0), None));
    }
    root
}

fn toolbar(
    field: &RyeOsFieldVm,
    instance: &RyeOsViewInstanceKey,
    selected: Option<&RyeOsFieldEntityVm>,
    expansion: Option<&RyeOsFieldExpansionVm>,
    local: &Local,
) -> El {
    let p = Palette::get();
    let key = instance.clone();
    let replay = &field.replay;
    let step = |direction| RyeOsUiEvent::StepFieldCursor {
        instance_key: key.clone(),
        direction,
    };
    let mut controls = row()
        .gap(5.0)
        .child(
            icon_button(
                &crate::icons::PREVIOUS,
                "",
                replay.previous.is_some(),
                Some(step(FieldStepDirection::Previous)),
            )
            .label("Previous event"),
        )
        .child(button(
            if replay.playing { "Pause" } else { "Play" },
            replay.playing || replay.next.is_some(),
            Some(RyeOsUiEvent::SetFieldPlayback {
                instance_key: key.clone(),
                playing: !replay.playing,
            }),
        ))
        .child(
            icon_button(
                &crate::icons::NEXT,
                "",
                replay.next.is_some(),
                Some(step(FieldStepDirection::Next)),
            )
            .label("Next event"),
        )
        .child(button(
            "Live",
            replay.mode != "live",
            Some(step(FieldStepDirection::Live)),
        ));
    for group in &field.groups {
        controls = controls.child(
            icon_button(
                if group.collapsed {
                    &crate::icons::COLLAPSED
                } else {
                    &crate::icons::EXPANDED
                },
                group.label.clone(),
                true,
                Some(RyeOsUiEvent::SetFieldGroupCollapsed {
                    instance_key: key.clone(),
                    group_id: group.id.clone(),
                    collapsed: !group.collapsed,
                }),
            )
            .expanded(!group.collapsed),
        );
    }
    for layer in &field.layers {
        controls = controls.child(toggle(
            Some(if layer.visible {
                &crate::icons::DOT
            } else {
                &crate::icons::RING
            }),
            layer.label.clone(),
            layer.visible,
            RyeOsUiEvent::SetFieldLayerVisible {
                instance_key: key.clone(),
                layer_id: layer.id.clone(),
                visible: !layer.visible,
            },
        ));
    }
    if let Some(selected) = selected {
        if selected.activate_intent.is_some() {
            controls = controls.child(
                button("Activate", true, activate(&selected.activate_intent))
                    .label(format!("Activate {}", selected.label)),
            );
        }
        controls = controls.child(button(
            if field.compare.contains(&selected.id) {
                "Uncompare"
            } else {
                "Compare"
            },
            selected.compare_available,
            Some(RyeOsUiEvent::ToggleFieldCompare {
                instance_key: key.clone(),
                entity_id: selected.id.clone(),
            }),
        ));
        let (source, root_id) = (selected.source.clone(), selected.id.clone());
        let (label, enabled, event) = match expansion {
            Some(expansion) if expansion.can_continue => (
                "Continue",
                true,
                RyeOsUiEvent::ContinueFieldExpansion {
                    instance_key: key.clone(),
                    source: source.clone(),
                    root_id: root_id.clone(),
                },
            ),
            Some(_) => (
                "Expanded",
                false,
                RyeOsUiEvent::RequestFieldExpansion {
                    instance_key: key.clone(),
                    source: source.clone(),
                    root_id: root_id.clone(),
                },
            ),
            None => (
                "Expand",
                true,
                RyeOsUiEvent::RequestFieldExpansion {
                    instance_key: key.clone(),
                    source: source.clone(),
                    root_id: root_id.clone(),
                },
            ),
        };
        controls = controls.child(button(label, enabled, Some(event)));
        if expansion.is_some() {
            controls = controls.child(button(
                "Clear",
                true,
                Some(RyeOsUiEvent::ClearFieldExpansion {
                    instance_key: key.clone(),
                    source,
                    root_id,
                }),
            ));
        }
    }
    let sources = field
        .sources
        .iter()
        .map(|source| format!("{}:{}", source.name, phase_name(source.phase)))
        .collect::<Vec<_>>()
        .join(" · ");
    let edit_key = query_key(instance);
    let change_key = edit_key.clone();
    let query_instance = instance.clone();
    let move_instance = instance.clone();
    let search = text_input(
        &local.field_query(&edit_key, &field.search.query),
        theme::data(10.0),
    )
    .single_line()
    .placeholder("Search field", p.faint)
    .label("Search field entities")
    .color(p.text)
    .w(200.0)
    .px(8.0)
    .py(6.0)
    .border(1.0, p.line_strong)
    .fill(p.plane)
    .on_edit(move |app: &mut App| app.field_query_mut(&edit_key))
    .on_input_change(move |app: &mut App, _| {
        let query = app.field_query_text(&change_key);
        app.ui(RyeOsUiEvent::SetFieldQuery {
            instance_key: query_instance.clone(),
            query,
        });
    })
    .on_input_key(move |app: &mut App, key: &KeyEvent, _| {
        if crate::views::is_launcher_chord(key) {
            return app.core_key(key);
        }
        let delta = match key.key {
            Key::Up => -1,
            Key::Down => 1,
            _ => return false,
        };
        app.ui(RyeOsUiEvent::MoveFieldSearchMatch {
            instance_key: move_instance.clone(),
            delta,
        });
        true
    });
    row()
        .h(46.0)
        .gap(10.0)
        .px(10.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
        .fill(p.plane.alpha(0.94))
        .child(
            col()
                .max_w(220.0)
                .min_w(0.0)
                .gap(1.0)
                .child(
                    text(field.title.clone())
                        .font(rune::tok::TextStyle::new(12.0, 500.0).family(theme::UI))
                        .color(p.text)
                        .truncate(),
                )
                .child(
                    text(sources)
                        .font(theme::data(9.0))
                        .color(p.muted)
                        .truncate(),
                ),
        )
        .child(
            row()
                .key(format!("field-controls:{instance:?}"))
                .grow(1.0)
                .min_w(0.0)
                .scroll_x()
                .child(controls.gap(3.0)),
        )
        .child(search)
}

fn phase_name(phase: RyeOsSourcePhase) -> &'static str {
    match phase {
        RyeOsSourcePhase::Unresolved => "unresolved",
        RyeOsSourcePhase::Loading => "loading",
        RyeOsSourcePhase::Ready => "ready",
        RyeOsSourcePhase::Refreshing => "refreshing",
        RyeOsSourcePhase::Error => "error",
    }
}

fn rail(field: &RyeOsFieldVm, instance: &RyeOsViewInstanceKey) -> El {
    let mut entries = row().gap(3.0);
    for entry in &field.replay.rail {
        let cursor = FieldCursorState::BraidCut {
            anchor: FieldEventRefState {
                chain_root_id: entry.event.chain_root_id.clone(),
                chain_seq: entry.event.chain_seq,
                event_hash: entry.event.event_hash.clone(),
            },
        };
        entries = entries.child(toggle(
            None,
            entry.label.clone(),
            entry.selected,
            RyeOsUiEvent::SetFieldCursor {
                instance_key: instance.clone(),
                cursor,
            },
        ));
    }
    row()
        .key(format!("field-rail:{instance:?}"))
        .role("navigation")
        .label("Durable execution events")
        .scroll_x()
        .child(entries)
}

fn stage(field: &RyeOsFieldVm, instance: &RyeOsViewInstanceKey) -> El {
    let p = Palette::get();
    let layout = layout(field);
    let changed: BTreeSet<&str> = field
        .changes
        .iter()
        .map(|change| change.id.as_str())
        .collect();

    // Groups and relations paint first, in one shape under every entity.
    let mut lines = Shape::new();
    for group in &layout.groups {
        let box_points = vec![
            [group.x, group.y],
            [group.x + group.width, group.y],
            [group.x + group.width, group.y + group.height],
            [group.x, group.y + group.height],
        ];
        lines = lines.fill(
            vec![
                [group.x, group.y],
                [group.x + group.width, group.y],
                [group.x + group.width, group.y + GROUP_HEADER_HEIGHT],
                [group.x, group.y + GROUP_HEADER_HEIGHT],
            ],
            p.bg.alpha(0.94),
        );
        lines = stroke(
            lines,
            box_points,
            true,
            FieldStroke::Dashed,
            1.0,
            p.muted.alpha(0.52),
        );
    }
    for (relation, points) in &layout.edges {
        if points.len() < 2 {
            continue;
        }
        let width = if changed.contains(relation.id.as_str()) {
            4.0
        } else {
            match relation.emphasis {
                FieldEmphasis::Strong => 2.5,
                FieldEmphasis::Quiet => 0.7,
                FieldEmphasis::Normal => 1.3,
            }
        };
        let ink = if relation.selected {
            p.text
        } else {
            tone(&p, relation.tone)
        };
        lines = stroke(lines, points.clone(), false, relation.stroke, width, ink);
    }
    let mut plane = el().w(layout.width).h(layout.height).child(
        canvas(lines)
            .absolute(0.0, 0.0)
            .size(layout.width, layout.height)
            .a11y_hidden(),
    );
    for group in &layout.groups {
        let key = instance.clone();
        plane = plane.child(
            row()
                .absolute(group.x, group.y)
                .w(group.width)
                .h(GROUP_HEADER_HEIGHT)
                .px(8.0)
                .role("button")
                .focusable(true)
                .expanded(!group.collapsed)
                .label(format!("Group {}", group.label))
                .hover(p.plane_raised)
                .gap(6.0)
                .center_y()
                .child(crate::icons::glyph(
                    if group.collapsed {
                        &crate::icons::COLLAPSED
                    } else {
                        &crate::icons::EXPANDED
                    },
                    11.0,
                    p.muted,
                ))
                .child(
                    text(group.label.to_owned())
                        .font(theme::data(11.0))
                        .color(p.muted)
                        .truncate(),
                )
                .on_press(ui(RyeOsUiEvent::SetFieldGroupCollapsed {
                    instance_key: key,
                    group_id: group.id.to_owned(),
                    collapsed: !group.collapsed,
                })),
        );
    }
    for node in &layout.nodes {
        plane = plane.child(entity(
            instance,
            node,
            changed.contains(node.entity.id.as_str()),
        ));
    }
    let mut stage = col()
        .key(format!("field-stage:{instance:?}"))
        .w_full()
        .h_full()
        .pt(46.0)
        .role("tree")
        .label(format!("{} entities", field.title))
        .focusable(true)
        .on_key(stage_keys(field, instance))
        .scroll_xy()
        .child(plane);
    if let Some((tone_ink, title, detail)) = empty_state(field) {
        stage = stage.child(
            col()
                .absolute(24.0, 24.0)
                .max_w(420.0)
                .gap(4.0)
                .padding(12.0)
                .border(1.0, tone_ink)
                .fill(p.plane)
                .role("status")
                .child(text(title).font(theme::ui(12.0)).color(tone_ink))
                .child(text(detail).font(theme::ui(11.0)).color(p.muted).wrap()),
        );
    }
    stage
}

fn entity(instance: &RyeOsViewInstanceKey, node: &Node, changed: bool) -> El {
    let p = Palette::get();
    let entity = node.entity;
    let ink = tone(&p, entity.tone);
    let hollow = entity.traits.fill == FieldFill::Hollow;
    // Room around the outline for the change halo.
    let pad = 7.0;
    let (w, h) = (node.width, node.height);
    let shifted = |points: Vec<[f32; 2]>, by: f32| -> Vec<[f32; 2]> {
        points.into_iter().map(|[x, y]| [x + by, y + by]).collect()
    };
    let body = shifted(outline(entity.traits.shape, w, h), pad);
    let mut shape = Shape::new();
    if changed {
        let halo = outline(entity.traits.shape, w + 14.0, h + 14.0);
        shape = shape.stroke_with(halo, true, 8.0, LineCap::Round, ink.alpha(0.32));
    }
    let fill = match entity.traits.fill {
        FieldFill::Hollow => p.bg,
        FieldFill::Ghost => ink.alpha(0.35),
        FieldFill::Solid => ink,
    };
    shape = shape.fill(body.clone(), fill);
    shape = stroke(
        shape,
        body,
        true,
        entity.traits.stroke,
        if entity.selected { 4.0 } else { 2.0 },
        if entity.selected { p.text } else { ink },
    );
    let label_ink = if hollow { p.text } else { p.bg };
    let mut face = col()
        .absolute(pad, pad)
        .size(w, h)
        .center()
        .gap(2.0)
        .px(8.0)
        .child(
            text(clip_label(&entity.label, 20))
                .font(rune::tok::TextStyle::new(12.0, 600.0).family(theme::DATA))
                .color(label_ink)
                .truncate(),
        );
    let status = entity.status.as_deref().unwrap_or(&entity.kind);
    if !status.is_empty() {
        face = face.child(
            text(status.to_owned())
                .font(theme::data(10.0))
                .color(label_ink)
                .truncate(),
        );
    }
    let mut node_el = canvas(shape)
        .absolute(node.x - w / 2.0 - pad, node.y - h / 2.0 - pad)
        .size(w + pad * 2.0, h + pad * 2.0)
        .role("treeitem")
        .label(if entity.accessibility_label.is_empty() {
            entity.label.clone()
        } else {
            entity.accessibility_label.clone()
        })
        .focus_selected(entity.selected)
        .child(face);
    if node.cycle > 1 {
        node_el = node_el.child(
            text(format!("↻{}", node.cycle))
                .font(theme::data(10.0))
                .color(p.yellow)
                .absolute(pad + w - 30.0, pad + 4.0),
        );
    }
    let select_key = instance.clone();
    let select_id = entity.id.clone();
    node_el = node_el.on_press(move |app: &mut App| {
        app.ui(RyeOsUiEvent::SetFieldSelection {
            instance_key: select_key.clone(),
            entity_id: Some(select_id.clone()),
        });
    });
    if let Some(event) = activate(&entity.activate_intent) {
        node_el = node_el.on_double_press(ui(event));
    }
    node_el
}

/// The stage's keys, as the browser shell's entity tree takes them.
fn stage_keys(
    field: &RyeOsFieldVm,
    instance: &RyeOsViewInstanceKey,
) -> impl Fn(&mut App, &KeyEvent, &mut rune::Tx) -> bool + 'static {
    let instance = instance.clone();
    let order: Vec<String> = field.traversal.clone();
    let selected = field.selected.clone();
    let group_of: HashMap<String, (String, bool)> = field
        .entities
        .iter()
        .filter_map(|entity| {
            let group = field
                .groups
                .iter()
                .find(|group| Some(&group.id) == entity.group_id.as_ref())?;
            Some((entity.id.clone(), (group.id.clone(), group.collapsed)))
        })
        .collect();
    let activations: HashMap<String, RyeOsUiIntent> = field
        .entities
        .iter()
        .filter_map(|entity| Some((entity.id.clone(), entity.activate_intent.clone()?)))
        .collect();
    let comparable: BTreeSet<String> = field
        .entities
        .iter()
        .filter(|entity| entity.compare_available)
        .map(|entity| entity.id.clone())
        .collect();
    move |app: &mut App, key: &KeyEvent, _| {
        let instance_key = instance.clone();
        let current = selected.clone().or_else(|| order.first().cloned());
        let event = match &key.key {
            Key::Up | Key::Down => RyeOsUiEvent::MoveFieldSelection {
                instance_key,
                delta: if key.key == Key::Up { -1 } else { 1 },
            },
            Key::Home | Key::End => {
                let target = if key.key == Key::Home {
                    order.first()
                } else {
                    order.last()
                };
                let Some(target) = target else { return false };
                RyeOsUiEvent::SetFieldSelection {
                    instance_key,
                    entity_id: Some(target.clone()),
                }
            }
            Key::Left | Key::Right => {
                let Some((group_id, collapsed)) = current.as_ref().and_then(|id| group_of.get(id))
                else {
                    return false;
                };
                let collapse = key.key == Key::Left;
                if *collapsed == collapse {
                    return false;
                }
                RyeOsUiEvent::SetFieldGroupCollapsed {
                    instance_key,
                    group_id: group_id.clone(),
                    collapsed: collapse,
                }
            }
            Key::Enter => {
                let Some(intent) = selected.as_ref().and_then(|id| activations.get(id)) else {
                    return false;
                };
                RyeOsUiEvent::Activate {
                    intent: intent.clone(),
                }
            }
            Key::Text(typed) if typed == " " => {
                let Some(id) = selected.as_ref().filter(|id| comparable.contains(*id)) else {
                    return false;
                };
                RyeOsUiEvent::ToggleFieldCompare {
                    instance_key,
                    entity_id: id.clone(),
                }
            }
            _ => return false,
        };
        app.ui(event);
        true
    }
}

/// What a zero-entity field says about its sources, never more.
fn empty_state(field: &RyeOsFieldVm) -> Option<(Color, &'static str, &'static str)> {
    if !field.entities.is_empty() {
        return None;
    }
    let p = Palette::get();
    let sources = &field.sources;
    let any = |test: &dyn Fn(&ryeos_client_base::ui::field::RyeOsFieldSourceStatusVm) -> bool| {
        sources.iter().any(test)
    };
    Some(
        if any(&|s| s.phase == RyeOsSourcePhase::Error || s.error.is_some()) {
            (
                p.red,
                "Source evidence failed",
                "No entities are shown. Inspect Evidence for the source error.",
            )
        } else if any(&|s| s.phase == RyeOsSourcePhase::Unresolved) {
            (
                p.yellow,
                "Source unresolved",
                "No entities are shown for the current subject selection.",
            )
        } else if any(&|s| {
            matches!(
                s.phase,
                RyeOsSourcePhase::Loading | RyeOsSourcePhase::Refreshing
            )
        }) {
            (
                p.yellow,
                "Source evidence pending",
                "No entities are shown while source evidence is loading or refreshing.",
            )
        } else if any(&|s| s.truncated) {
            (
                p.yellow,
                "Bounded projection",
                "No entities are shown in this bounded source response; retained evidence may extend beyond it.",
            )
        } else if !sources.is_empty() && sources.iter().all(|s| s.phase == RyeOsSourcePhase::Ready)
        {
            (
                p.muted,
                "No entities in this projection",
                "This does not establish that no retained run or result exists.",
            )
        } else {
            (
                p.muted,
                "No entities projected",
                "Source availability is not established here. Inspect Evidence for retained context.",
            )
        },
    )
}

fn has_evidence(field: &RyeOsFieldVm, expansion: Option<&RyeOsFieldExpansionVm>) -> bool {
    !field.sources.is_empty()
        || !field.subjects.is_empty()
        || !field.metrics.is_empty()
        || !field.provenance.is_empty()
        || !field.replay.outside_cut.is_empty()
        || field.search.truncated
        || expansion.is_some()
}

fn evidence(
    field: &RyeOsFieldVm,
    instance: &RyeOsViewInstanceKey,
    expansion: Option<&RyeOsFieldExpansionVm>,
    local: &Local,
) -> El {
    let p = Palette::get();
    let key = format!("field-evidence:{instance:?}");
    let open = local.evidence_open.contains(&key);
    let mut flags: Vec<String> = field
        .sources
        .iter()
        .map(|source| {
            let mut flag = format!("{} {}", source.name, phase_name(source.phase));
            if source.truncated {
                flag.push_str(" bounded");
            }
            if source.error.is_some() {
                flag.push_str(" error");
            }
            flag
        })
        .collect();
    for (present, label) in [
        (!field.subjects.is_empty(), "subjects"),
        (!field.metrics.is_empty(), "metrics"),
        (!field.provenance.is_empty(), "provenance"),
        (!field.replay.outside_cut.is_empty(), "outside cut"),
        (field.search.truncated, "bounded search"),
        (expansion.is_some_and(|e| e.truncated), "bounded expansion"),
    ] {
        if present {
            flags.push(label.to_owned());
        }
    }
    let toggle_key = key.clone();
    let summary = row()
        .w_full()
        .min_w(0.0)
        .gap(10.0)
        .px(10.0)
        .py(5.0)
        .role("button")
        .focusable(true)
        .expanded(open)
        .hover(p.plane_raised)
        .child(crate::icons::glyph(
            if open {
                &crate::icons::EXPANDED
            } else {
                &crate::icons::COLLAPSED
            },
            10.0,
            p.muted,
        ))
        .child(text("EVIDENCE").font(theme::data(9.0)).color(p.muted))
        .child(
            text(flags.join(" · "))
                .font(theme::data(9.0))
                .color(p.faint)
                .truncate()
                .min_w(0.0),
        )
        .on_press(move |app: &mut App| app.toggle_evidence(toggle_key.clone()));
    let mut block = col()
        .max_w(580.0)
        .border(1.0, p.line)
        .fill(p.plane)
        .child(summary);
    if !open {
        return block;
    }
    let mut body = col()
        .key(key)
        .w_full()
        .max_h(260.0)
        .shrink()
        .min_h(0.0)
        .px(10.0)
        .pb(8.0)
        .gap(8.0);
    let heading = |label: String| text(label).font(theme::ui(11.0)).color(p.text);
    let line = |value: String, ink: Color| {
        text(value)
            .font(theme::data(9.0))
            .color(ink)
            .wrap_anywhere()
            .w_full()
    };
    for source in &field.sources {
        let mut section = col()
            .w_full()
            .gap(2.0)
            .child(heading(format!(
                "{} · {}",
                source.name,
                phase_name(source.phase)
            )))
            .child(line(format!("Source {}", source.source_ref), p.muted));
        if let Some(fingerprint) = &source.subject_fingerprint {
            section = section.child(line(format!("Subject fingerprint {fingerprint}"), p.muted));
        }
        if source.truncated {
            section = section.child(line(
                "Truncated response — this is not the complete retained field.".into(),
                p.yellow,
            ));
        }
        if let Some(error) = &source.error {
            section = section.child(line(error.clone(), p.red));
        }
        if let Some(revision) = &source.revision {
            section = section.child(line(format!("Revision {revision}"), p.faint));
        }
        for item in &source.evidence {
            section = section.child(line(item.clone(), p.muted));
        }
        if source.evidence_omitted > 0 {
            section = section.child(line(
                format!(
                    "{} further entity evidence references omitted from this display.",
                    source.evidence_omitted
                ),
                p.faint,
            ));
        }
        body = body.child(section);
    }
    if !field.subjects.is_empty() {
        let mut section = col()
            .w_full()
            .gap(2.0)
            .child(heading("Projected source subjects".into()));
        for subject in &field.subjects {
            let mut value = format!("{} · {} · {}", subject.source, subject.kind, subject.id);
            if let Some(definition) = &subject.definition_ref {
                value.push_str(&format!("\nDefinition {definition}"));
            }
            if let Some(digest) = &subject.effective_definition_digest {
                value.push_str(&format!("\nEffective digest {digest}"));
            }
            section = section.child(line(value, p.muted));
        }
        body = body.child(section);
    }
    if !field.metrics.is_empty() {
        let mut section = col()
            .w_full()
            .gap(2.0)
            .child(heading("Projected field metrics".into()));
        for metric in &field.metrics {
            let unit = metric
                .unit
                .as_deref()
                .map(|unit| format!(" {unit}"))
                .unwrap_or_default();
            section = section.child(line(
                format!("{} {}{unit}", metric.label, metric.value),
                p.muted,
            ));
        }
        body = body.child(section);
    }
    if !field.provenance.is_empty() {
        let mut section = col()
            .w_full()
            .gap(2.0)
            .child(heading("Projected entity/relation source refs".into()));
        for source_ref in &field.provenance {
            section = section.child(line(source_ref.clone(), p.muted));
        }
        body = body.child(section);
    }
    if let Some(expansion) = expansion {
        let mut section = col()
            .w_full()
            .gap(2.0)
            .child(heading(format!(
                "Expansion · {}",
                phase_name(expansion.phase)
            )))
            .child(line(
                format!(
                    "Applied depth {}; {} retained entities.",
                    expansion.applied_depth, expansion.entity_count
                ),
                p.muted,
            ));
        if expansion.truncated {
            section = section.child(line(
                "Expansion is bounded — additional retained evidence may exist.".into(),
                p.yellow,
            ));
        }
        body = body.child(section);
    }
    if !field.replay.outside_cut.is_empty() {
        let mut section = col()
            .w_full()
            .gap(2.0)
            .child(heading("Outside replay cut".into()))
            .child(line(
                "Concurrent retained context is not included in the selected braid cut.".into(),
                p.muted,
            ));
        for event in &field.replay.outside_cut {
            section = section.child(line(event.clone(), p.faint));
        }
        body = body.child(section);
    }
    if field.search.truncated {
        body = body.child(
            col()
                .w_full()
                .gap(2.0)
                .child(heading("Bounded search results".into()))
                .child(line(
                    "The visible matches are not the complete matching field.".into(),
                    p.muted,
                )),
        );
    }
    block = block.child(body.scroll(rune::ScrollSpec {
        stick_bottom: false,
    }));
    block
}

fn detail(
    field: &RyeOsFieldVm,
    instance: &RyeOsViewInstanceKey,
    selected: Option<&RyeOsFieldEntityVm>,
    local: &Local,
) -> El {
    let p = Palette::get();
    let mut aside = col()
        .key(format!("field-detail:{instance:?}"))
        .w(250.0)
        .max_h(420.0)
        .gap(6.0)
        .padding(10.0)
        .border(1.0, p.line_strong)
        .fill(p.plane.alpha(0.96))
        .scroll(rune::ScrollSpec {
            stick_bottom: false,
        });
    let previews_for = |entity: &RyeOsFieldEntityVm| -> Vec<&RyeOsArtifactPreviewVm> {
        field
            .previews
            .iter()
            .filter(|preview| {
                preview.entity_id.as_deref() == Some(entity.id.as_str())
                    && entity.preview_ids.contains(&preview.id)
            })
            .collect()
    };
    if let Some(selected) = selected {
        let meta = [
            Some(selected.kind.as_str()),
            selected.status.as_deref(),
            Some(selected.source.as_str()),
        ]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
        aside = aside
            .child(
                text(selected.label.clone())
                    .font(theme::ui(12.0))
                    .color(p.text)
                    .wrap_anywhere()
                    .w_full(),
            )
            .child(text(meta).font(theme::data(9.0)).color(p.muted))
            .child(crate::views::details_as(
                &selected.detail,
                crate::views::Pairs::Panel,
            ));
        if let Some(artifact) = &selected.artifact_ref {
            aside = aside.child(crate::views::detail_pairs(
                &[
                    ("Thread", artifact.thread_id.clone()),
                    ("Artifact", artifact.artifact_id.to_string()),
                    (
                        "Content hash",
                        artifact
                            .content_hash
                            .clone()
                            .unwrap_or_else(|| "Not recorded".into()),
                    ),
                ],
                crate::views::Pairs::Panel,
            ));
        }
        let mut connectors = row().gap(5.0);
        let mut any = false;
        for relation in field
            .relations
            .iter()
            .filter(|r| r.source_id == selected.id || r.target_id == selected.id)
        {
            any = true;
            let label = relation
                .label
                .clone()
                .unwrap_or_else(|| relation.kind.clone());
            connectors = connectors.child(
                button(
                    label,
                    relation.activate_intent.is_some(),
                    activate(&relation.activate_intent),
                )
                .label(relation.accessibility_label.clone()),
            );
        }
        if any {
            aside = aside.child(
                row()
                    .key(format!("field-connectors:{instance:?}"))
                    .w_full()
                    .scroll_x()
                    .child(connectors),
            );
        }
        for preview in previews_for(selected) {
            aside = aside.child(grid_preview(preview, local));
        }
    }
    let compared: Vec<&RyeOsFieldEntityVm> = field
        .compare
        .iter()
        .filter_map(|id| field.entities.iter().find(|entity| &entity.id == id))
        .collect();
    if !compared.is_empty() {
        let mut section = col()
            .w_full()
            .gap(6.0)
            .label("Compared field entities")
            .child(
                text("Compared entities")
                    .font(theme::ui(11.0))
                    .color(p.text),
            );
        for entity in compared {
            let mut article = col()
                .w_full()
                .gap(3.0)
                .label(format!("{} ({}) comparison", entity.label, entity.id))
                .child(
                    text(entity.label.clone())
                        .font(theme::ui(12.0))
                        .color(p.text),
                )
                .child(
                    text(format!("{} · {}", entity.id, entity.source))
                        .font(theme::data(9.0))
                        .color(p.muted),
                );
            let previews = previews_for(entity);
            if previews.is_empty() {
                article = article.child(
                    text("No grid preview is present in this projection.")
                        .font(theme::ui(11.0))
                        .color(p.faint),
                );
            }
            for preview in previews {
                article = article.child(grid_preview(preview, local));
            }
            section = section.child(article);
        }
        aside = aside.child(section);
    }
    for warning in &field.warnings {
        aside = aside.child(
            text(warning.clone())
                .font(theme::ui(11.0))
                .color(p.yellow)
                .wrap()
                .w_full(),
        );
    }
    aside
}

/// An indexed grid preview, one filled square per cell.
fn grid_preview(preview: &RyeOsArtifactPreviewVm, local: &Local) -> El {
    let p = Palette::get();
    let mut figure = col().gap(3.0).child(
        text(preview.label.clone())
            .font(theme::data(9.0))
            .color(p.muted),
    );
    let Some(grid) = &preview.grid else {
        return figure.child(
            text("This projected preview has no grid payload.")
                .font(theme::ui(11.0))
                .color(p.faint),
        );
    };
    let scale = 9.0;
    let colors: HashMap<u16, Color> = grid
        .palette
        .iter()
        .map(|entry| {
            (
                entry.index,
                rune::color::hex(&entry.color).unwrap_or(p.faint),
            )
        })
        .collect();
    let mut shape = Shape::new();
    for (index, cell) in grid.cells.iter().enumerate() {
        let (x, y) = (
            (index as u32 % grid.width.max(1)) as f32,
            (index as u32 / grid.width.max(1)) as f32,
        );
        let (left, top) = (x * scale, y * scale);
        shape = shape.fill(
            vec![
                [left, top],
                [left + scale, top],
                [left + scale, top + scale],
                [left, top + scale],
            ],
            colors.get(cell).copied().unwrap_or(p.faint),
        );
    }
    figure = figure.child(
        canvas(shape)
            .size(grid.width as f32 * scale, grid.height as f32 * scale)
            .label(format!(
                "{}: {} by {} grid",
                preview.label, grid.width, grid.height
            )),
    );
    figure.child(transcript(preview, grid, local))
}

/// The grid read cell by cell, a page of rows at a time, so a screen reader
/// hears every admitted cell rather than one label for the whole board.
fn transcript(preview: &RyeOsArtifactPreviewVm, grid: &RyeOsIndexedGridVm, local: &Local) -> El {
    let p = Palette::get();
    let key = format!("grid-transcript:{}", preview.id);
    let open = local.grid_transcripts.get(&key).copied();
    let toggle_key = key.clone();
    let toggle = text(if open.is_some() {
        "Hide grid cells"
    } else {
        "Read grid cells"
    })
    .font(theme::ui(11.0))
    .color(p.accent_bright)
    .key(format!("{key}:toggle"))
    .role("button")
    .expanded(open.is_some())
    .on_press(move |app: &mut App| app.toggle_grid_transcript(toggle_key.clone()));
    let mut block = col().gap(4.0).child(toggle);
    let Some(page) = open.and_then(|row| transcript_page(grid, row, TRANSCRIPT_ROWS)) else {
        return block;
    };
    let (previous_key, next_key) = (key.clone(), key.clone());
    let previous = page.start.saturating_sub(TRANSCRIPT_ROWS);
    let next = page.end;
    let mut nav = row()
        .gap(10.0)
        .center_y()
        .child(text("Previous rows").font(theme::ui(11.0)).color(if page.start == 0 {
            p.faint
        } else {
            p.accent_bright
        }).role("button").on_press(move |app: &mut App| {
            app.set_grid_transcript(previous_key.clone(), previous)
        }))
        .child(
            text(format!("Rows {}–{} of {}", page.start + 1, page.end, page.height))
                .font(theme::data(10.0))
                .color(p.muted)
                .role("status"),
        );
    if page.end < page.height {
        nav = nav.child(
            text("Next rows")
                .font(theme::ui(11.0))
                .color(p.accent_bright)
                .role("button")
                .on_press(move |app: &mut App| app.set_grid_transcript(next_key.clone(), next)),
        );
    }
    let mut rows = col()
        .gap(2.0)
        .label(format!("{} cells", preview.label))
        .child(nav);
    for (number, cells) in page.rows {
        rows = rows.child(
            text(format!("Row {number}: {}", cells.join(" | ")))
                .font(theme::data(10.0))
                .color(p.text)
                .wrap_anywhere()
                .selectable(true),
        );
    }
    block = block.child(rows);
    block
}

/// Rows a transcript page reads at once.
const TRANSCRIPT_ROWS: u32 = 16;

struct TranscriptPage {
    /// Row number (from one) and its cells, each `column N: name`.
    rows: Vec<(u32, Vec<String>)>,
    start: u32,
    end: u32,
    height: u32,
}

/// One page of `grid`'s transcript from `requested_row`, or nothing for a grid
/// whose cells do not match its declared size. Each cell names its palette
/// label, else its glyph, else its index, and says when it changed.
fn transcript_page(grid: &RyeOsIndexedGridVm, requested_row: u32, page_size: u32) -> Option<TranscriptPage> {
    let (width, height) = (grid.width, grid.height);
    if !(1..=512).contains(&width)
        || !(1..=512).contains(&height)
        || grid.cells.len() != (width as usize) * (height as usize)
    {
        return None;
    }
    let size = page_size.clamp(1, TRANSCRIPT_ROWS);
    let start = requested_row.min(height - 1);
    let end = (start + size).min(height);
    let names: HashMap<u16, &str> = grid
        .palette
        .iter()
        .map(|entry| {
            let name = entry
                .label
                .as_deref()
                .filter(|label| !label.is_empty())
                .unwrap_or(entry.glyph.as_str());
            (entry.index, name)
        })
        .collect();
    let changed: std::collections::HashSet<u32> = grid.changed.iter().copied().collect();
    let rows = (start..end)
        .map(|row| {
            let cells = (0..width)
                .map(|column| {
                    let offset = row * width + column;
                    let index = grid.cells[offset as usize];
                    let name = names
                        .get(&index)
                        .filter(|name| !name.is_empty())
                        .map_or_else(|| index.to_string(), |name| (*name).to_owned());
                    let mark = if changed.contains(&offset) { " (changed)" } else { "" };
                    format!("column {}: {name}{mark}", column + 1)
                })
                .collect();
            (row + 1, cells)
        })
        .collect();
    Some(TranscriptPage {
        rows,
        start,
        end,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_client_base::ui::field::RyeOsGridPaletteEntryVm;
    use serde_json::{Value, json};

    fn board(width: u32, height: u32, cells: Vec<u16>) -> RyeOsIndexedGridVm {
        RyeOsIndexedGridVm {
            width,
            height,
            cells,
            palette: vec![
                RyeOsGridPaletteEntryVm {
                    index: 0,
                    color: "#000000".into(),
                    glyph: ".".into(),
                    label: Some("empty".into()),
                },
                RyeOsGridPaletteEntryVm {
                    index: 1,
                    color: "#ffffff".into(),
                    glyph: "#".into(),
                    label: Some("wall".into()),
                },
            ],
            changed: Vec::new(),
            labels: Vec::new(),
        }
    }

    #[test]
    fn the_transcript_pages_every_cell_of_a_large_grid() {
        let mut grid = board(64, 64, (0..4096).map(|offset| (offset % 2) as u16).collect());
        grid.changed = vec![4095];
        let first = transcript_page(&grid, 0, 16).unwrap();
        assert_eq!(first.rows.len(), 16);
        assert_eq!(first.rows[0].1.len(), 64);
        assert_eq!(first.rows[0].1[0], "column 1: empty");
        assert_eq!(first.rows[0].1[1], "column 2: wall");
        let last = transcript_page(&grid, 48, 16).unwrap();
        assert_eq!(last.end, 64);
        let (number, cells) = last.rows.last().unwrap();
        assert_eq!(*number, 64);
        assert_eq!(cells.last().unwrap(), "column 64: wall (changed)");
        let mut heard = std::collections::HashSet::new();
        for start in (0..64).step_by(16) {
            for (number, cells) in transcript_page(&grid, start, 16).unwrap().rows {
                for (column, cell) in cells.iter().enumerate() {
                    assert!(cell.starts_with(&format!("column {}: ", column + 1)));
                    heard.insert((number, column));
                }
            }
        }
        assert_eq!(heard.len(), 4096);
    }

    #[test]
    fn the_transcript_refuses_inconsistent_grids_and_bounds_its_pages() {
        assert!(transcript_page(&board(2, 2, vec![0]), 0, 16).is_none());
        assert!(transcript_page(&board(513, 1, vec![0; 513]), 0, 16).is_none());
        let mut tall = board(1, 40, vec![3; 40]);
        tall.palette.clear();
        assert_eq!(transcript_page(&tall, 0, 999).unwrap().rows.len(), 16);
        assert_eq!(transcript_page(&tall, 39, 16).unwrap().rows[0].1[0], "column 1: 3");
        assert_eq!(transcript_page(&tall, 900, 16).unwrap().start, 39);
    }

    fn entity(
        id: &str,
        group: Value,
        layers: Value,
        lane: Value,
        rank: Value,
        order: Value,
        selected: bool,
        shape: &str,
    ) -> Value {
        json!({
            "id": id, "source": "s", "kind": "step", "role": null, "label": id,
            "secondary": null, "parent_id": null, "group_id": group, "layer_ids": layers,
            "lane": lane, "rank": rank, "order": order, "status": null, "tone": "neutral",
            "traits": { "shape": shape, "fill": "solid", "stroke": "solid", "emphasis": "normal", "motion": "none" },
            "badges": [], "artifact_ref": null, "preview_ids": [], "compare_available": false,
            "selected": selected, "selectable": true, "select_intent": null, "activate_intent": null,
            "accessibility_label": id, "detail": []
        })
    }

    fn relation(id: &str, source: &str, target: &str) -> Value {
        json!({
            "id": id, "kind": "next", "source_id": source, "target_id": target, "directed": true,
            "tone": "neutral", "stroke": "solid", "emphasis": "normal", "motion": "none",
            "label": null, "layer_ids": [], "selected": false, "activate_intent": null,
            "accessibility_label": id
        })
    }

    /// Groups, lanes, authored rank and order, a two-member cycle, a hidden
    /// layer, and a hidden entity that selection reveals.
    fn fixture() -> RyeOsFieldVm {
        let n = Value::Null;
        let group = |id: &str, label: &str| json!({ "id": id, "label": label, "parent_id": null, "layout": "flow", "collapsed": false, "aggregate": null });
        serde_json::from_value(json!({
            "schema_version": "ryeos.ui.field.vm.v2", "id": "field:test", "title": "Field",
            "revision": "r", "structural_revision": "s", "data_revision": "d", "local_revision": "l",
            "sources": [], "subjects": [],
            "groups": [group("build", "Build"), group("deploy", "Deploy"), group("empty", "Empty")],
            "layers": [{ "id": "hidden", "label": "Hidden", "visible": false, "count": 2 }],
            "entities": [
                entity("a", json!("build"), json!([]), n.clone(), n.clone(), n.clone(), false, "rect"),
                entity("b", json!("build"), json!([]), json!("x"), n.clone(), json!(2), false, "aggregate"),
                entity("c", json!("build"), json!([]), json!("x"), n.clone(), json!(1), false, "grid"),
                entity("d", json!("deploy"), json!([]), n.clone(), n.clone(), n.clone(), false, "disc"),
                entity("e", json!("deploy"), json!([]), n.clone(), n.clone(), n.clone(), false, "rect"),
                entity("f", n.clone(), json!([]), json!("y"), json!(4), n.clone(), false, "rect"),
                entity("g", n.clone(), json!(["hidden"]), n.clone(), n.clone(), n.clone(), false, "rect"),
                entity("h", n.clone(), json!(["hidden"]), n.clone(), n.clone(), n.clone(), true, "rect"),
            ],
            "relations": [
                relation("r1", "a", "b"), relation("r2", "a", "c"), relation("r3", "c", "d"),
                relation("r4", "d", "e"), relation("r5", "e", "d"), relation("r6", "e", "f"),
                relation("r7", "g", "a"), relation("r8", "h", "f"),
            ],
            "previews": [], "metrics": [], "traversal": [], "selected": "h", "compare": [],
            "cursor": null, "replay": { "mode": "live", "playing": false, "anchor": null, "previous": null,
                "next": null, "live_head": null, "rail": [], "outside_cut": [] },
            "search": { "query": "", "match_ids": [], "active_match": null, "truncated": false },
            "expansions": [], "changes": [], "warnings": [], "provenance": []
        }))
        .expect("valid field fixture")
    }

    /// Geometry the browser shell's layout computes for the same fixture, so
    /// both renderers place every entity identically.
    #[test]
    fn layout_matches_the_browser_shell() {
        let field = fixture();
        let layout = layout(&field);
        let mut nodes: Vec<(&str, [f32; 4], usize)> = layout
            .nodes
            .iter()
            .map(|node| {
                (
                    node.entity.id.as_str(),
                    [node.x, node.y, node.width, node.height],
                    node.cycle,
                )
            })
            .collect();
        nodes.sort_by(|left, right| left.0.cmp(right.0));
        assert_eq!(
            nodes,
            vec![
                ("a", [120.0, 390.0, 136.0, 52.0], 1),
                ("b", [330.0, 176.0, 156.0, 52.0], 1),
                ("c", [330.0, 90.0, 136.0, 74.0], 1),
                ("d", [1060.0, 390.0, 136.0, 52.0], 2),
                ("e", [1060.0, 476.0, 136.0, 52.0], 2),
                ("f", [2720.0, 240.0, 136.0, 52.0], 1),
                ("h", [1880.0, 390.0, 136.0, 52.0], 1),
            ]
        );
        let edges: Vec<(&str, Vec<[f32; 2]>)> = layout
            .edges
            .iter()
            .map(|(relation, points)| (relation.id.as_str(), points.clone()))
            .collect();
        assert_eq!(
            edges,
            vec![
                (
                    "r1",
                    vec![
                        [188.0, 390.0],
                        [225.0, 390.0],
                        [225.0, 176.0],
                        [252.0, 176.0]
                    ]
                ),
                (
                    "r2",
                    vec![[188.0, 390.0], [225.0, 390.0], [225.0, 90.0], [262.0, 90.0]]
                ),
                (
                    "r3",
                    vec![[398.0, 90.0], [695.0, 90.0], [695.0, 390.0], [992.0, 390.0]]
                ),
                (
                    "r4",
                    vec![
                        [1128.0, 390.0],
                        [1060.0, 390.0],
                        [1060.0, 476.0],
                        [992.0, 476.0]
                    ]
                ),
                (
                    "r5",
                    vec![
                        [1128.0, 476.0],
                        [1060.0, 476.0],
                        [1060.0, 390.0],
                        [992.0, 390.0]
                    ]
                ),
                (
                    "r6",
                    vec![
                        [1128.0, 476.0],
                        [1890.0, 476.0],
                        [1890.0, 240.0],
                        [2652.0, 240.0]
                    ]
                ),
                (
                    "r8",
                    vec![
                        [1948.0, 390.0],
                        [2300.0, 390.0],
                        [2300.0, 240.0],
                        [2652.0, 240.0]
                    ]
                ),
            ]
        );
        let groups: Vec<(&str, [f32; 4])> = layout
            .groups
            .iter()
            .map(|group| (group.id, [group.x, group.y, group.width, group.height]))
            .collect();
        assert_eq!(
            groups,
            vec![
                ("build", [20.0, 11.0, 420.0, 433.0]),
                ("deploy", [960.0, 322.0, 200.0, 208.0]),
                ("empty", [1260.0, 20.0, 200.0, 38.0]),
            ]
        );
        assert_eq!((layout.width, layout.height), (2956.0, 628.0));
    }

    #[test]
    fn dashes_alternate_along_the_whole_path() {
        // A dash that reaches a corner keeps the corner point.
        let pieces = dashes(&[[0.0, 0.0], [10.0, 0.0], [10.0, 10.0]], false, 5.0, 2.0);
        assert_eq!(
            pieces,
            vec![
                vec![[0.0, 0.0], [5.0, 0.0]],
                vec![[7.0, 0.0], [10.0, 0.0], [10.0, 2.0]],
                vec![[10.0, 4.0], [10.0, 9.0]],
            ]
        );
    }
}
