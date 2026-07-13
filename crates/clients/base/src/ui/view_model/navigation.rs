use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RyeOsSessionVm {
    pub session_id: String,
    pub project_path: Option<String>,
    pub surface_ref: String,
    #[serde(default)]
    pub ambient: RyeOsAmbientVm,
    pub user_principal_id: Option<String>,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsAmbientVm {
    pub show_background: bool,
    pub opacity: Option<f32>,
    pub mode: RyeOsAmbientModeVm,
    pub atlas: Option<RyeOsAmbientAtlasVm>,
}

impl Default for RyeOsAmbientVm {
    fn default() -> Self {
        Self {
            show_background: true,
            opacity: None,
            mode: RyeOsAmbientModeVm::Ambient,
            atlas: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsAmbientModeVm {
    #[default]
    Ambient,
    NamespaceAtlas,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsAmbientAtlasVm {
    pub style: RyeOsAmbientAtlasStyleVm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsAmbientAtlasStyleVm {
    #[default]
    #[serde(rename = "flat_2d")]
    Flat2d,
    #[serde(rename = "paper_3d")]
    Paper3d,
}
