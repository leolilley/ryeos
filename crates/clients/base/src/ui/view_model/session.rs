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
    pub chip: RyeOsSurfaceChipVm,
    /// Count of items in this Surface that need the user; None when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_you: Option<usize>,
    /// Count of runs in this Surface the user launched or follows; None when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running: Option<usize>,
}

/// The switcher chip for a Surface: the mark to draw and whether the Surface
/// is authored in a project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsSurfaceChipVm {
    pub mark: RyeOsChipMarkVm,
    pub authored_in_project: bool,
}

/// Which mark a Surface chip draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsChipMarkVm {
    Grid,
    Key,
    #[default]
    Initial,
}
