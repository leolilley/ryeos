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
    scene.objects.push(StudioSceneObjectVm {
        id: "node:local".to_string(),
        kind: StudioSceneObjectKind::LocalNode,
        position: [0.0, 0.0, 0.0],
        rotation: [0.0, 0.0, 0.0],
        scale: [1.0, 1.0, 1.0],
        color: "#83a598".to_string(),
        opacity: 1.0,
        label: Some("Local node".to_string()),
        tone: StudioTone::Neutral,
        selected: false,
        action: None,
    });
    if let Some(snapshot) = &core.data.snapshot {
        for (index, remote) in snapshot.remotes.iter().enumerate() {
            scene.objects.push(StudioSceneObjectVm {
                id: format!("remote:{}", remote.name),
                kind: StudioSceneObjectKind::RemoteNode,
                position: [index as f32 + 2.0, 0.0, -2.0],
                rotation: [0.0, 0.0, 0.0],
                scale: [0.7, 0.7, 0.7],
                color: "#8ec07c".to_string(),
                opacity: 0.9,
                label: Some(remote.name.clone()),
                tone: StudioTone::Good,
                selected: false,
                action: None,
            });
        }
    }
    scene
}
