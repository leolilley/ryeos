use serde::{Deserialize, Serialize};

use super::event::StudioAction;
use super::view_model::StudioTone;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioSceneModel {
    pub schema_version: String,
    pub generation: u64,
    pub camera: StudioCameraVm,
    pub objects: Vec<StudioSceneObjectVm>,
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

    if let Some(snapshot) = &core.data.snapshot {
        scene.objects.push(scene_object(
            "project:core",
            StudioSceneObjectKind::ProjectCore,
            [0.0, -0.2, 0.0],
            [scale_for_count(snapshot.project.iter().count()), 1.0, 1.0],
            "#fe8019",
            snapshot.project.as_ref().map(|project| project.path.clone()),
            StudioTone::Accent,
        ));

        scene.objects.push(scene_object(
            "spaces:ring",
            StudioSceneObjectKind::SpaceRing,
            [0.0, -0.4, 0.0],
            [scale_for_count(snapshot.local_node.spaces.len()), 1.0, 1.0],
            "#fabd2f",
            Some(format!("{} spaces", snapshot.local_node.spaces.len())),
            StudioTone::Neutral,
        ));

        scene.objects.push(scene_object(
            "services:beacon",
            StudioSceneObjectKind::ServiceBeacon,
            [-2.6, 0.0, -2.8],
            [scale_for_count(snapshot.local_node.services.len()), 1.0, 1.0],
            "#83a598",
            Some(format!("{} services", snapshot.local_node.services.len())),
            StudioTone::Neutral,
        ));

        scene.objects.push(scene_object(
            "threads:active",
            StudioSceneObjectKind::ThreadFlow,
            [2.4, 0.0, -2.0],
            [scale_for_count(snapshot.threads.active_count.max(0) as usize), 1.0, 1.0],
            "#d3869b",
            Some(format!("{} active threads", snapshot.threads.active_count.max(0))),
            if snapshot.threads.active_count > 0 {
                StudioTone::Accent
            } else {
                StudioTone::Neutral
            },
        ));

        scene.objects.push(scene_object(
            "schedules:pulse",
            StudioSceneObjectKind::SchedulePulse,
            [3.2, 0.0, 2.4],
            [scale_for_count(snapshot.schedules.enabled), 1.0, 1.0],
            "#b8bb26",
            Some(format!(
                "{} enabled / {} schedules",
                snapshot.schedules.enabled, snapshot.schedules.total
            )),
            if snapshot.schedules.enabled > 0 {
                StudioTone::Good
            } else {
                StudioTone::Neutral
            },
        ));

        for (index, remote) in snapshot.remotes.iter().enumerate() {
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
