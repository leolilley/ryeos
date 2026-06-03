use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use crate::atlas::{build_namespace_atlas, AtlasInput, AtlasItemInput, NamespaceAtlasVm};

use super::event::StudioAction;
use super::model::StudioInspectorState;
use super::view_model::StudioTone;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioSceneModel {
    pub schema_version: String,
    pub generation: u64,
    pub camera: StudioCameraVm,
    pub objects: Vec<StudioSceneObjectVm>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub atlas: Option<NamespaceAtlasVm>,
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
        }
    }
}

use super::model::StudioCore;

pub fn build_scene_model(core: &StudioCore) -> StudioSceneModel {
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

    if let Some(items) = &core.data.items {
        scene.objects.push(scene_object(
            "items:cluster",
            StudioSceneObjectKind::ItemCluster,
            [-3.0, 0.0, 1.8],
            [scale_for_count(items.items.len()), 1.0, 1.0],
            "#fabd2f",
            Some(format!("{} items", items.items.len())),
            StudioTone::Accent,
        ));
        let selected_ref = match &core.ui.inspector {
            StudioInspectorState::Item { canonical_ref } => Some(canonical_ref.clone()),
            _ => None,
        };
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
            ui: core.ui.atlas.clone(),
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

    if let Some(schedules) = &core.data.schedules {
        scene.objects.push(scene_object(
            "schedules:list",
            StudioSceneObjectKind::SchedulePulse,
            [3.8, 0.0, 1.6],
            [scale_for_count(schedules.schedules.len()), 1.0, 1.0],
            "#b8bb26",
            Some(format!("{} loaded schedules", schedules.schedules.len())),
            StudioTone::Good,
        ));
    }

    scene
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
    }
}

fn scale_for_count(count: usize) -> f32 {
    (0.65 + (count as f32).sqrt() * 0.12).min(2.2)
}

fn atlas_context_refs(core: &StudioCore) -> Vec<String> {
    let Some(inspection) = &core.data.item_inspection else {
        return Vec::new();
    };
    let current_ref = match &core.ui.inspector {
        StudioInspectorState::Item { canonical_ref } => canonical_ref.as_str(),
        _ => return Vec::new(),
    };
    if inspection.item.canonical_ref != current_ref {
        return Vec::new();
    }

    let mut refs = BTreeSet::new();
    if let Some(effective) = &inspection.effective {
        collect_refs_from_json(effective, &mut refs);
    }
    if let Some(raw) = &inspection.raw {
        collect_refs_from_text(&raw.content, &mut refs);
    }
    refs.into_iter()
        .filter(|item_ref| item_ref != current_ref)
        .collect()
}

fn collect_refs_from_json(value: &serde_json::Value, refs: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::String(value) => collect_refs_from_text(value, refs),
        serde_json::Value::Array(values) => {
            for value in values {
                collect_refs_from_json(value, refs);
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values() {
                collect_refs_from_json(value, refs);
            }
        }
        _ => {}
    }
}

fn collect_refs_from_text(text: &str, refs: &mut BTreeSet<String>) {
    for token in text.split(|ch: char| {
        ch.is_whitespace() || matches!(ch, ',' | '[' | ']' | '{' | '}' | '(' | ')' | '"' | '\'')
    }) {
        let token = token.trim_matches(|ch: char| matches!(ch, ':' | ';' | '.' | ','));
        if is_canonical_item_ref(token) {
            refs.insert(token.to_string());
        }
    }
}

fn is_canonical_item_ref(value: &str) -> bool {
    let Some((kind, bare)) = value.split_once(':') else {
        return false;
    };
    matches!(kind, "directive" | "tool" | "knowledge" | "config") && !bare.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::studio::dto::{StudioItemDto, StudioItemsDto};

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

        let scene = build_scene_model(&core);
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
    fn scene_model_highlights_inspected_context_refs() {
        let mut core = StudioCore::default();
        core.ui.inspector = StudioInspectorState::Item {
            canonical_ref: "directive:rye/core/create_tool".to_string(),
        };
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
                    canonical_ref: "knowledge:rye/core/signing".to_string(),
                    item_kind: "knowledge".to_string(),
                    bare_id: "signing".to_string(),
                    label: "signing".to_string(),
                    namespace: Some("rye/core".to_string()),
                    space: "project".to_string(),
                    source_path: ".ai/knowledge/rye/core/signing.md".to_string(),
                    executable: false,
                    trust: None,
                },
            ],
            ..StudioItemsDto::default()
        });
        core.data.item_inspection = Some(crate::studio::dto::StudioItemInspectionDto {
            item: crate::studio::dto::StudioInspectedItemDto {
                canonical_ref: "directive:rye/core/create_tool".to_string(),
                ..Default::default()
            },
            effective: Some(serde_json::json!({
                "context": ["knowledge:rye/core/signing"]
            })),
            ..Default::default()
        });

        let atlas = build_scene_model(&core).atlas.expect("atlas");
        let signing = atlas
            .nodes
            .iter()
            .find(|node| node.namespace_key == "rye/core/signing")
            .expect("signing knowledge");
        assert!(signing.state.highlighted);
    }
}
