use serde::{Deserialize, Serialize};

use super::super::event::RyeOsUiIntent;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RyeOsSessionVm {
    pub session_id: String,
    pub project_path: Option<String>,
    pub root_ref: String,
    pub title: String,
    pub surface_ordinal: Option<u64>,
    pub surfaces: Vec<RyeOsSurfaceChoiceVm>,
    pub user_principal_id: Option<String>,
    pub posture: crate::ui::binding::UiEffectivePosture,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsSurfaceChoiceVm {
    pub surface_instance_id: crate::ids::SurfaceInstanceId,
    pub ordinal: u64,
    pub title: String,
    pub active: bool,
    pub select_intent: RyeOsUiIntent,
    pub rename_intent: Option<RyeOsUiIntent>,
    pub close_intent: Option<RyeOsUiIntent>,
}
