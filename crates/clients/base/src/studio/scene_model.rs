use serde::{Deserialize, Serialize};

use crate::atlas::{
    build_file_space_atlas, build_namespace_atlas, AtlasFileInput, AtlasFileSpaceInput, AtlasInput,
    AtlasItemInput, AtlasProjectionVm, AtlasUiStateVm, NamespaceAtlasVm,
};

use super::event::StudioAction;
use super::view_model::StudioTone;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioSceneModel {
    pub schema_version: String,
    pub generation: u64,
    pub camera: StudioCameraVm,
    pub objects: Vec<StudioSceneObjectVm>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub atlas: Option<NamespaceAtlasVm>,
    /// Ambient animation energy in `[0, 1]` — how alive the scene reads.
    /// The builder maps a real signal into it (the backdrop uses the
    /// node's live-thread count); renderers quicken pacing and lift
    /// brightness with it. `0.0` = idle calm.
    #[serde(default)]
    pub energy: f32,
    /// Optional light sweep declared by the scene content: a diagonal
    /// brightness band that traverses the objects by `generation`. The
    /// renderer implements the traversal generically; content only opts
    /// in and shapes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sweep: Option<SceneSweep>,
}

/// A scene-level light sweep: a band of brightness `width` wide (scene
/// units, along x+y) crossing the scene once every `period` generations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneSweep {
    pub period: u64,
    pub width: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioCameraVm {
    pub position: [f32; 3],
    pub target: [f32; 3],
    pub fov_degrees: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioSceneObjectVm {
    pub id: String,
    pub kind: StudioSceneObjectKind,
    pub position: [f32; 3],
    pub rotation: [f32; 3],
    pub scale: [f32; 3],
    pub color: String,
    pub opacity: f32,
    pub label: Option<String>,
    pub tone: StudioTone,
    pub selected: bool,
    pub action: Option<StudioAction>,
    /// Named glyph ramp the renderer draws this object's cells from
    /// (`"diamond"` for facet geometry; absent = the default dot ramp).
    /// A ramp NAME is generic widget vocabulary like a tone — which
    /// objects use which ramp stays content's choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glyph: Option<String>,
    /// Segment end (`to:` in scene content). Present → the object is an
    /// EDGE from `position` to here: the renderer rasterizes contiguous
    /// cells along it at cell resolution, so declared line-art stays a
    /// line at any terminal size instead of decomposing into sparse dots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<[f32; 3]>,
    /// SDF shape name for a [`StudioSceneObjectKind::Fill`] object
    /// (`"prism"`, `"sphere"`). Shape vocabulary is generic renderer
    /// capability; which shape a scene uses is content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudioSceneObjectKind {
    LocalNode,
    RemoteNode,
    ProjectCore,
    SpaceRing,
    ItemCluster,
    ThreadFlow,
    SchedulePulse,
    ServiceBeacon,
    Link,
    LabelAnchor,
    /// A point/particle: the generic renderer draws it as a dot sized by
    /// `scale` (`·`/`•`/`●`). The backdrop's orbiting motes are particles.
    Particle,
    /// A text object: the generic renderer draws its `label` at the
    /// projected position. The backdrop's "RYE OS"/welcome/hint lines are
    /// text objects, not a hardcoded welcome block.
    Text,
    /// A FILLED solid: the renderer rasterizes every interior cell from a
    /// signed-distance shape (`shape:` names it; `scale` carries its
    /// dimensions) through the density ramp, lit and animated. The
    /// backdrop prism is a fill; which shape stays content's choice.
    Fill,
}

impl Default for StudioSceneModel {
    fn default() -> Self {
        Self {
            schema_version: "ryeos.studio.scene.v1".to_string(),
            generation: 0,
            camera: StudioCameraVm {
                position: [0.0, 4.0, 9.0],
                target: [0.0, 0.0, 0.0],
                fov_degrees: 45.0,
            },
            objects: Vec::new(),
            atlas: None,
            energy: 0.0,
            sweep: None,
        }
    }
}

use super::model::StudioCore;

/// Build the scene for one atlas arrangement. `atlas` is the relevant
/// arrangement state: the ambient backdrop (`core.ui.atlas`) for the
/// empty-center namespace_atlas, or a tile's per-tile state for an Atlas
/// center tile. The underlying topology/item/file data is shared (global
/// in `core.data`); `atlas` selects the projection, layers, and lens.
/// Build the scene for one atlas/graph instance. `items`/`file_space`
/// override the shared `core.data.*` when this is a tile with its own
/// scoped dataset (per-tile content); pass `None` for the ambient scene to
/// read the shared global data. Topology stays shared (the node graph has
/// no per-tile scope yet — see #23 follow-ups).
pub fn build_scene_model(
    core: &StudioCore,
    atlas: &AtlasUiStateVm,
    items: Option<&super::dto::StudioItemsDto>,
    file_space: Option<&super::dto::StudioFileSpaceDto>,
) -> StudioSceneModel {
    let mut scene = StudioSceneModel {
        generation: core.generation,
        ..StudioSceneModel::default()
    };
    scene.objects.push(scene_object(
        "node:local",
        StudioSceneObjectKind::LocalNode,
        [0.0, 0.0, 0.0],
        [1.0, 1.0, 1.0],
        "#83a598",
        Some("Local node".to_string()),
        StudioTone::Neutral,
    ));

    if let Some(dimension) = &core.data.dimension {
        scene.objects.push(scene_object(
            "project:core",
            StudioSceneObjectKind::ProjectCore,
            [0.0, -0.2, 0.0],
            [scale_for_count(dimension.project.iter().count()), 1.0, 1.0],
            "#fe8019",
            dimension
                .project
                .as_ref()
                .map(|project| project.path.clone()),
            StudioTone::Accent,
        ));

        scene.objects.push(scene_object(
            "spaces:ring",
            StudioSceneObjectKind::SpaceRing,
            [0.0, -0.4, 0.0],
            [scale_for_count(dimension.local_node.spaces.len()), 1.0, 1.0],
            "#fabd2f",
            Some(format!("{} spaces", dimension.local_node.spaces.len())),
            StudioTone::Neutral,
        ));

        scene.objects.push(scene_object(
            "services:beacon",
            StudioSceneObjectKind::ServiceBeacon,
            [-2.6, 0.0, -2.8],
            [
                scale_for_count(dimension.local_node.services.len()),
                1.0,
                1.0,
            ],
            "#83a598",
            Some(format!("{} services", dimension.local_node.services.len())),
            StudioTone::Neutral,
        ));

        scene.objects.push(scene_object(
            "threads:active",
            StudioSceneObjectKind::ThreadFlow,
            [2.4, 0.0, -2.0],
            [
                scale_for_count(dimension.threads.active_count.max(0) as usize),
                1.0,
                1.0,
            ],
            "#d3869b",
            Some(format!(
                "{} active threads",
                dimension.threads.active_count.max(0)
            )),
            if dimension.threads.active_count > 0 {
                StudioTone::Accent
            } else {
                StudioTone::Neutral
            },
        ));

        scene.objects.push(scene_object(
            "schedules:pulse",
            StudioSceneObjectKind::SchedulePulse,
            [3.2, 0.0, 2.4],
            [scale_for_count(dimension.schedules.enabled), 1.0, 1.0],
            "#b8bb26",
            Some(format!(
                "{} enabled / {} schedules",
                dimension.schedules.enabled, dimension.schedules.total
            )),
            if dimension.schedules.enabled > 0 {
                StudioTone::Good
            } else {
                StudioTone::Neutral
            },
        ));

        for (index, remote) in dimension.remotes.iter().enumerate() {
            scene.objects.push(scene_object(
                &format!("remote:{}", remote.name),
                StudioSceneObjectKind::RemoteNode,
                [index as f32 + 2.0, 0.0, -2.0],
                [0.7, 0.7, 0.7],
                "#8ec07c",
                Some(remote.name.clone()),
                StudioTone::Good,
            ));
        }
    }

    if let Some(topology) = &core.data.topology {
        let limit = topology.nodes.len().min(48);
        let mut projected_nodes = Vec::new();
        for (index, node) in topology.nodes.iter().take(limit).enumerate() {
            let angle = index as f32 * 0.72;
            let radius = 4.2 + ((index % 4) as f32 * 0.45);
            let y = ((index % 5) as f32 - 2.0) * 0.22;
            let position = [angle.cos() * radius, y, angle.sin() * radius];
            let id = if node.id.is_empty() {
                format!("topology:node:{index}")
            } else {
                format!("topology:node:{}", node.id)
            };
            projected_nodes.push((node.id.clone(), position));
            let label = if node.label.is_empty() {
                node.ref_.clone()
            } else {
                node.label.clone()
            };
            let detail = serde_json::json!({
                "id": node.id,
                "kind": node.kind,
                "ref": node.ref_,
                "space": node.space,
                "path": node.path,
                "namespace": node.namespace,
                "virtual": node.virtual_,
                "missing": node.missing,
                "status": node.status,
                "trust": node.trust,
            });
            let mut object = scene_object(
                &id,
                scene_kind_for_topology(&node.kind),
                position,
                [0.42, 0.42, 0.42],
                color_for_topology(&node.kind, node.missing),
                Some(label.clone()),
                tone_for_topology(&node.kind, node.missing, node.trust.as_ref()),
            );
            object.opacity = if node.missing { 0.45 } else { 0.86 };
            object.action = Some(StudioAction::InspectSummary {
                title: format!("Topology: {label}"),
                detail,
            });
            scene.objects.push(object);
        }

        for (index, edge) in topology.edges.iter().take(64).enumerate() {
            let Some(from) = topology_node_position(&projected_nodes, &edge.from) else {
                continue;
            };
            let Some(to) = topology_node_position(&projected_nodes, &edge.to) else {
                continue;
            };
            let position = midpoint(from, to);
            let length = distance(from, to).max(0.2);
            let label = if edge.label.is_empty() {
                edge.type_.clone()
            } else {
                edge.label.clone()
            };
            let mut object = scene_object(
                &format!("topology:edge:{}", edge_id(edge, index)),
                StudioSceneObjectKind::Link,
                position,
                [length, 0.05, 0.05],
                "#928374",
                Some(label.clone()),
                StudioTone::Neutral,
            );
            object.opacity = 0.34;
            object.action = Some(StudioAction::InspectSummary {
                title: format!("Topology edge: {label}"),
                detail: serde_json::json!({
                    "id": edge.id,
                    "from": edge.from,
                    "to": edge.to,
                    "type": edge.type_,
                    "label": edge.label,
                    "source": edge.source,
                    "confidence": edge.confidence,
                }),
            });
            scene.objects.push(object);
        }

        if topology.nodes.len() > limit {
            scene.objects.push(scene_object(
                "topology:truncated",
                StudioSceneObjectKind::LabelAnchor,
                [0.0, 1.2, -4.8],
                [0.55, 0.55, 0.55],
                "#928374",
                Some(format!(
                    "{} more topology nodes",
                    topology.nodes.len() - limit
                )),
                StudioTone::Neutral,
            ));
        }
    }

    if let Some(items) = items.or(core.data.items.as_ref()) {
        scene.objects.push(scene_object(
            "items:cluster",
            StudioSceneObjectKind::ItemCluster,
            [-3.0, 0.0, 1.8],
            [scale_for_count(items.items.len()), 1.0, 1.0],
            "#fabd2f",
            Some(format!("{} items", items.items.len())),
            StudioTone::Accent,
        ));
        // Selection is a seat facet — the scene highlights what the
        // seat braid says is selected.
        let selected_ref = core
            .seat
            .fold()
            .get(crate::studio::seat::KEY_SELECTION)
            .and_then(|sel| sel.get("item"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let mut capabilities = core
            .data
            .session
            .as_ref()
            .map(|session| session.granted_caps.clone())
            .unwrap_or_default();
        if let Some(dimension) = &core.data.dimension {
            capabilities.extend(dimension.session.granted_caps.clone());
            for service in &dimension.local_node.services {
                capabilities.extend(service.required_caps.clone());
            }
        }
        capabilities.sort();
        capabilities.dedup();

        if atlas.active_projection == AtlasProjectionVm::AiSpace {
            scene.atlas = Some(build_namespace_atlas(AtlasInput {
                generation: core.generation,
                root_label: ".ai".to_string(),
                items: items
                    .items
                    .iter()
                    .map(|item| AtlasItemInput {
                        canonical_ref: item.canonical_ref.clone(),
                        kind: item.item_kind.clone(),
                        label: item.label.clone(),
                        namespace: item.namespace.clone(),
                        source_path: item.source_path.clone(),
                        scope: item.space.clone(),
                        executable: item.executable,
                    })
                    .collect(),
                capabilities,
                selected_ref,
                context_refs: atlas_context_refs(core),
                ui: atlas.clone(),
            }));
        }
    }

    if atlas.active_projection == AtlasProjectionVm::FileSpace {
        let file_space = file_space.or(core.data.file_space.as_ref());
        let selected_ref = core
            .seat
            .fold()
            .get(crate::studio::seat::KEY_SELECTION)
            .and_then(|sel| sel.get("file"))
            .map(|file| {
                format!(
                    "file:{}:{}",
                    file.get("root")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default(),
                    file.get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                )
            });
        scene.atlas = Some(build_file_space_atlas(AtlasFileSpaceInput {
            generation: core.generation,
            root_label: "File Space".to_string(),
            root: file_space
                .map(|file_space| file_space.root.clone())
                .unwrap_or_else(|| "project".to_string()),
            entries: file_space
                .map(|file_space| {
                    file_space
                        .entries
                        .iter()
                        .map(|entry| AtlasFileInput {
                            path: entry.path.clone(),
                            name: entry.name.clone(),
                            is_dir: entry.is_dir,
                            size: entry.size,
                        })
                        .collect()
                })
                .unwrap_or_default(),
            selected_ref,
            ui: atlas.clone(),
        }));
    }

    if let Some(threads) = &core.data.threads {
        scene.objects.push(scene_object(
            "threads:recent",
            StudioSceneObjectKind::ThreadFlow,
            [2.0, 0.0, -3.2],
            [scale_for_count(threads.threads.len()), 1.0, 1.0],
            "#d3869b",
            Some(format!("{} recent threads", threads.threads.len())),
            StudioTone::Accent,
        ));
    }

    if let Some(dimension) = &core.data.dimension {
        scene.objects.push(scene_object(
            "schedules:list",
            StudioSceneObjectKind::SchedulePulse,
            [3.8, 0.0, 1.6],
            [scale_for_count(dimension.schedules.total), 1.0, 1.0],
            "#b8bb26",
            Some(format!("{} schedules", dimension.schedules.total)),
            StudioTone::Good,
        ));
    }

    scene
}

/// Build a scene from a `widget: scene` view's body — the generic,
/// content-driven path. The body declares `objects: [...]`, each a
/// particle, edge (`to:`), filled solid (`kind: fill` + `shape:`), or
/// text object with a position, scale, color, tone, opacity, and (for
/// text) a label. The renderer draws them generically, so the background
/// is content with no per-art Rust; `generation` carries the frame clock
/// so the renderer can animate. +y is up; the renderer fits and centres
/// the declared extent proportionally.
pub fn scene_from_body(body: &serde_json::Value, generation: u64) -> StudioSceneModel {
    let mut scene = StudioSceneModel {
        generation,
        ..StudioSceneModel::default()
    };
    // Optional content-declared light sweep: `sweep: {period, width}` —
    // an empty block opts in with the defaults.
    if let Some(sweep) = body.get("sweep") {
        let period = sweep
            .get("period")
            .and_then(serde_json::Value::as_u64)
            .filter(|p| *p > 0)
            .unwrap_or(24);
        let width = sweep
            .get("width")
            .and_then(serde_json::Value::as_f64)
            .map(|w| w as f32)
            .filter(|w| *w > 0.0)
            .unwrap_or(4.0);
        scene.sweep = Some(SceneSweep { period, width });
    }
    let Some(objects) = body.get("objects").and_then(serde_json::Value::as_array) else {
        return scene;
    };
    for (index, obj) in objects.iter().enumerate() {
        let kind = match obj.get("kind").and_then(serde_json::Value::as_str) {
            Some("text") => StudioSceneObjectKind::Text,
            Some("fill") => StudioSceneObjectKind::Fill,
            _ => StudioSceneObjectKind::Particle,
        };
        let position = read_position(obj.get("position"));
        let scale = read_scale(obj.get("scale"));
        let color = obj
            .get("color")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("#ebdbb2")
            .to_string();
        let tone = scene_tone(obj.get("tone").and_then(serde_json::Value::as_str));
        let label = obj
            .get("label")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let opacity = obj
            .get("opacity")
            .and_then(serde_json::Value::as_f64)
            .map(|o| o as f32)
            .unwrap_or(1.0);
        let id = obj
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("backdrop:{index}"));
        let mut object = scene_object(&id, kind, position, scale, &color, label, tone);
        object.opacity = opacity;
        object.glyph = obj
            .get("glyph")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        if let Some(to) = obj.get("to") {
            object.end = Some(read_position(Some(to)));
        }
        object.shape = obj
            .get("shape")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        scene.objects.push(object);
    }
    scene
}

/// `[x, y]` or `[x, y, z]` -> `[x, y, z]` (z defaults 0).
fn read_position(v: Option<&serde_json::Value>) -> [f32; 3] {
    let arr = v.and_then(serde_json::Value::as_array);
    let get = |i: usize| {
        arr.and_then(|a| a.get(i))
            .and_then(serde_json::Value::as_f64)
            .map(|f| f as f32)
            .unwrap_or(0.0)
    };
    [get(0), get(1), get(2)]
}

/// A scalar scale -> uniform `[s, s, s]`; an array `[sx, sy, sz]` passes
/// through (a fill shape's dimensions).
fn read_scale(v: Option<&serde_json::Value>) -> [f32; 3] {
    if let Some(arr) = v.and_then(serde_json::Value::as_array) {
        let get = |i: usize| {
            arr.get(i)
                .and_then(serde_json::Value::as_f64)
                .map(|f| f as f32)
                .unwrap_or(0.0)
        };
        return [get(0), get(1), get(2)];
    }
    let s = v
        .and_then(serde_json::Value::as_f64)
        .map(|f| f as f32)
        .unwrap_or(1.0);
    [s, s, s]
}

fn scene_tone(name: Option<&str>) -> StudioTone {
    match name {
        Some("accent") => StudioTone::Accent,
        Some("good") => StudioTone::Good,
        Some("warn") => StudioTone::Warn,
        Some("danger") => StudioTone::Danger,
        _ => StudioTone::Neutral,
    }
}

fn scene_object(
    id: &str,
    kind: StudioSceneObjectKind,
    position: [f32; 3],
    scale: [f32; 3],
    color: &str,
    label: Option<String>,
    tone: StudioTone,
) -> StudioSceneObjectVm {
    StudioSceneObjectVm {
        id: id.to_string(),
        kind,
        position,
        rotation: [0.0, 0.0, 0.0],
        scale,
        color: color.to_string(),
        opacity: 1.0,
        label,
        tone,
        selected: false,
        action: None,
        glyph: None,
        end: None,
        shape: None,
    }
}

fn scale_for_count(count: usize) -> f32 {
    (0.65 + (count as f32).sqrt() * 0.12).min(2.2)
}

fn topology_node_position(nodes: &[(String, [f32; 3])], id: &str) -> Option<[f32; 3]> {
    nodes
        .iter()
        .find_map(|(node_id, position)| (node_id == id).then_some(*position))
}

fn midpoint(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        (a[0] + b[0]) * 0.5,
        (a[1] + b[1]) * 0.5,
        (a[2] + b[2]) * 0.5,
    ]
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    (dx * dx + dy * dy + dz * dz).sqrt()
}

fn edge_id(edge: &super::dto::StudioTopologyEdgeDto, index: usize) -> String {
    if edge.id.is_empty() {
        format!("{index}:{}:{}", edge.from, edge.to)
    } else {
        edge.id.clone()
    }
}

fn scene_kind_for_topology(kind: &str) -> StudioSceneObjectKind {
    match kind {
        "project" | "project_root" | "surface" => StudioSceneObjectKind::ProjectCore,
        "space" => StudioSceneObjectKind::SpaceRing,
        "service" | "route" | "handler" | "protocol" => StudioSceneObjectKind::ServiceBeacon,
        "thread" => StudioSceneObjectKind::ThreadFlow,
        "schedule" => StudioSceneObjectKind::SchedulePulse,
        _ => StudioSceneObjectKind::ItemCluster,
    }
}

fn color_for_topology(kind: &str, missing: bool) -> &'static str {
    if missing {
        return "#928374";
    }
    match kind {
        "directive" => "#fabd2f",
        "tool" => "#8ec07c",
        "knowledge" => "#83a598",
        "service" | "route" => "#b8bb26",
        "surface" | "project" | "project_root" => "#fe8019",
        _ => "#d5c4a1",
    }
}

fn tone_for_topology(
    kind: &str,
    missing: bool,
    trust: Option<&super::dto::StudioTopologyTrustSummaryDto>,
) -> StudioTone {
    if missing {
        StudioTone::Warn
    } else if matches!(trust.map(|trust| trust.class_.as_str()), Some("trusted")) {
        StudioTone::Good
    } else if matches!(
        trust.map(|trust| trust.class_.as_str()),
        Some("untrusted" | "unsigned")
    ) {
        StudioTone::Warn
    } else if matches!(kind, "surface" | "project" | "project_root") {
        StudioTone::Accent
    } else {
        StudioTone::Neutral
    }
}

fn atlas_context_refs(_core: &StudioCore) -> Vec<String> {
    // Context edges return when item inspection data flows through a
    // bound view source (content), not a typed DTO.
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::studio::dto::{
        StudioItemDto, StudioItemsDto, StudioTopologyDto, StudioTopologyEdgeDto,
        StudioTopologyEdgeSourceDto, StudioTopologyNodeDto, StudioTopologyNodeStatusDto,
        StudioTopologyTrustSummaryDto,
    };

    #[test]
    fn scene_from_body_reads_objects_as_content() {
        // The background is content: particle + text objects come from the
        // view body, not Rust. generation rides through for animation.
        let body = serde_json::json!({
            "objects": [
                { "kind": "particle", "position": [1.0, 2.0], "scale": 0.9, "color": "#d65d0e", "tone": "accent", "opacity": 0.95 },
                { "kind": "text", "position": [0.0, -8.0], "label": "RYE OS", "color": "#d65d0e", "tone": "accent" }
            ]
        });
        let scene = scene_from_body(&body, 7);
        assert_eq!(scene.generation, 7);
        let particle = scene
            .objects
            .iter()
            .find(|o| o.kind == StudioSceneObjectKind::Particle)
            .expect("particle from body");
        assert_eq!(particle.position, [1.0, 2.0, 0.0]);
        assert_eq!(particle.scale, [0.9, 0.9, 0.9]);
        assert!((particle.opacity - 0.95).abs() < 1e-6);
        let brand = scene
            .objects
            .iter()
            .find(|o| o.kind == StudioSceneObjectKind::Text && o.label.as_deref() == Some("RYE OS"))
            .expect("RYE OS text object from body");
        assert_eq!(brand.tone, StudioTone::Accent);
    }

    #[test]
    fn scene_from_body_empty_is_empty() {
        let scene = scene_from_body(&serde_json::json!({}), 0);
        assert!(scene.objects.is_empty());
    }

    #[test]
    fn scene_model_includes_namespace_atlas_for_items() {
        let mut core = StudioCore::default();
        core.generation = 42;
        core.data.items = Some(StudioItemsDto {
            items: vec![
                StudioItemDto {
                    canonical_ref: "directive:rye/core/create_tool".to_string(),
                    item_kind: "directive".to_string(),
                    bare_id: "create_tool".to_string(),
                    label: "create_tool".to_string(),
                    namespace: Some("rye/core".to_string()),
                    space: "project".to_string(),
                    source_path: ".ai/directives/rye/core/create_tool.md".to_string(),
                    executable: true,
                    trust: None,
                },
                StudioItemDto {
                    canonical_ref: "knowledge:rye/core/create_tool".to_string(),
                    item_kind: "knowledge".to_string(),
                    bare_id: "create_tool".to_string(),
                    label: "create_tool".to_string(),
                    namespace: Some("rye/core".to_string()),
                    space: "project".to_string(),
                    source_path: ".ai/knowledge/rye/core/create_tool.md".to_string(),
                    executable: false,
                    trust: None,
                },
            ],
            ..StudioItemsDto::default()
        });

        let scene = build_scene_model(&core, &core.ui.atlas, None, None);
        let atlas = scene.atlas.expect("atlas");
        assert_eq!(atlas.generation, 42);
        let stack_node = atlas
            .nodes
            .iter()
            .find(|node| node.namespace_key == "rye/core/create_tool")
            .expect("projected stack");
        assert_eq!(stack_node.stack.len(), 2);
    }

    #[test]
    fn per_tile_items_override_the_shared_dataset() {
        let mut core = StudioCore::default();
        core.generation = 7;
        // Shared/global dataset: one namespace.
        core.data.items = Some(StudioItemsDto {
            items: vec![StudioItemDto {
                canonical_ref: "directive:shared/global/x".to_string(),
                item_kind: "directive".to_string(),
                bare_id: "x".to_string(),
                label: "x".to_string(),
                namespace: Some("shared/global".to_string()),
                space: "project".to_string(),
                source_path: ".ai/directives/shared/global/x.md".to_string(),
                executable: true,
                trust: None,
            }],
            ..StudioItemsDto::default()
        });
        // This tile's own scoped items: a different namespace.
        let tile_items = StudioItemsDto {
            items: vec![StudioItemDto {
                canonical_ref: "knowledge:tile/scoped/y".to_string(),
                item_kind: "knowledge".to_string(),
                bare_id: "y".to_string(),
                label: "y".to_string(),
                namespace: Some("tile/scoped".to_string()),
                space: "project".to_string(),
                source_path: ".ai/knowledge/tile/scoped/y.md".to_string(),
                executable: false,
                trust: None,
            }],
            ..StudioItemsDto::default()
        };
        let atlas = build_scene_model(&core, &core.ui.atlas, Some(&tile_items), None)
            .atlas
            .expect("atlas");
        // The scene reflects the per-tile items, not the shared dataset.
        assert!(atlas
            .nodes
            .iter()
            .any(|n| n.namespace_key == "tile/scoped/y"));
        assert!(!atlas
            .nodes
            .iter()
            .any(|n| n.namespace_key == "shared/global/x"));
    }

    #[test]
    fn scene_model_projects_topology_nodes() {
        let mut core = StudioCore::default();
        core.data.topology = Some(StudioTopologyDto {
            nodes: vec![StudioTopologyNodeDto {
                id: "tool:demo/run".to_string(),
                kind: "tool".to_string(),
                label: "run".to_string(),
                ref_: "tool:demo/run".to_string(),
                status: Some(StudioTopologyNodeStatusDto {
                    resolved: true,
                    composed: Some(false),
                    executable: true,
                }),
                trust: Some(StudioTopologyTrustSummaryDto {
                    class_: "trusted".to_string(),
                    signer: Some("signer-fp".to_string()),
                }),
                ..Default::default()
            }],
            ..Default::default()
        });

        let scene = build_scene_model(&core, &core.ui.atlas, None, None);
        let node = scene
            .objects
            .iter()
            .find(|object| object.id == "topology:node:tool:demo/run")
            .expect("topology node object");
        assert_eq!(node.kind, StudioSceneObjectKind::ItemCluster);
        assert_eq!(node.label.as_deref(), Some("run"));
        assert_eq!(node.tone, StudioTone::Good);
        let Some(StudioAction::InspectSummary { detail, .. }) = &node.action else {
            panic!("topology node should inspect summary")
        };
        assert_eq!(detail["status"]["executable"], true);
        assert_eq!(detail["trust"]["class"], "trusted");
        assert_eq!(detail["trust"]["signer"], "signer-fp");
    }

    #[test]
    fn scene_model_projects_topology_edges() {
        let mut core = StudioCore::default();
        core.data.topology = Some(StudioTopologyDto {
            nodes: vec![
                StudioTopologyNodeDto {
                    id: "tool:demo/run".to_string(),
                    kind: "tool".to_string(),
                    label: "run".to_string(),
                    ref_: "tool:demo/run".to_string(),
                    ..Default::default()
                },
                StudioTopologyNodeDto {
                    id: "knowledge:demo/readme".to_string(),
                    kind: "knowledge".to_string(),
                    label: "readme".to_string(),
                    ref_: "knowledge:demo/readme".to_string(),
                    ..Default::default()
                },
            ],
            edges: vec![StudioTopologyEdgeDto {
                id: "edge-1".to_string(),
                from: "tool:demo/run".to_string(),
                to: "knowledge:demo/readme".to_string(),
                type_: "context".to_string(),
                label: "uses".to_string(),
                source: Some(StudioTopologyEdgeSourceDto {
                    field: Some("context".to_string()),
                    path: Some("/tmp/tool.yaml".to_string()),
                }),
                confidence: "declared".to_string(),
            }],
            ..Default::default()
        });

        let scene = build_scene_model(&core, &core.ui.atlas, None, None);
        let edge = scene
            .objects
            .iter()
            .find(|object| object.id == "topology:edge:edge-1")
            .expect("topology edge object");
        assert_eq!(edge.kind, StudioSceneObjectKind::Link);
        assert_eq!(edge.label.as_deref(), Some("uses"));
        assert!(edge.scale[0] > 0.2);
        let Some(StudioAction::InspectSummary { detail, .. }) = &edge.action else {
            panic!("topology edge should inspect summary")
        };
        assert_eq!(detail["source"]["field"], "context");
        assert_eq!(detail["source"]["path"], "/tmp/tool.yaml");
        assert_eq!(detail["confidence"], "declared");
    }
}
