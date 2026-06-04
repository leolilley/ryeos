use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamespaceAtlasVm {
    pub schema_version: String,
    pub generation: u64,
    pub coordinate_system: String,
    pub root_label: String,
    pub bounds: AtlasBoundsVm,
    pub nodes: Vec<AtlasNodeVm>,
    pub links: Vec<AtlasLinkVm>,
    pub regions: Vec<AtlasRegionVm>,
    pub selected_ref: Option<String>,
    #[serde(default)]
    pub ui: AtlasUiStateVm,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct AtlasBoundsVm {
    pub radius_max: f32,
    pub x_min: f32,
    pub x_max: f32,
    pub z_min: f32,
    pub z_max: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtlasNodeVm {
    pub id: String,
    pub namespace_key: String,
    pub label: String,
    pub path: Vec<String>,
    pub depth: u16,
    pub angle: f32,
    pub angle_start: f32,
    pub angle_end: f32,
    pub radius: f32,
    pub position: [f32; 3],
    pub stack: Vec<AtlasStackItemVm>,
    pub state: AtlasVisualStateVm,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtlasStackItemVm {
    pub id: String,
    pub canonical_ref: String,
    pub kind: AtlasItemKind,
    pub scope: AtlasScope,
    pub label: String,
    pub source_path: String,
    pub executable: bool,
    pub y_offset: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtlasItemKind {
    Config,
    Knowledge,
    Tool,
    Directive,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtlasScope {
    Project,
    User,
    System,
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AtlasVisualStateVm {
    pub selected: bool,
    pub highlighted: bool,
    pub dimmed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtlasRegionVm {
    pub id: String,
    pub capability: String,
    pub label: String,
    pub path_prefix: Vec<String>,
    pub angle_start: f32,
    pub angle_end: f32,
    pub radius_min: f32,
    pub radius_max: f32,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtlasLinkVm {
    pub id: String,
    pub from: String,
    pub to: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtlasUiStateVm {
    #[serde(default = "default_visible_atlas_layers")]
    pub visible_layers: BTreeSet<AtlasItemKind>,
    #[serde(default)]
    pub active_lens: AtlasLensVm,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtlasLensVm {
    #[default]
    None,
    Knowledge,
}

impl Default for AtlasUiStateVm {
    fn default() -> Self {
        Self {
            visible_layers: default_visible_atlas_layers(),
            active_lens: AtlasLensVm::None,
        }
    }
}

impl AtlasUiStateVm {
    pub fn layer_visible(&self, kind: AtlasItemKind) -> bool {
        self.visible_layers.contains(&kind)
    }

    pub fn set_layer_visible(&mut self, kind: AtlasItemKind, visible: bool) {
        if visible {
            self.visible_layers.insert(kind);
        } else {
            self.visible_layers.remove(&kind);
        }
    }

    pub fn set_lens(&mut self, lens: AtlasLensVm) {
        self.active_lens = lens;
    }

    pub fn item_visible(&self, kind: AtlasItemKind) -> bool {
        if !self.layer_visible(kind) {
            return false;
        }
        match self.active_lens {
            AtlasLensVm::None => true,
            AtlasLensVm::Knowledge => kind == AtlasItemKind::Knowledge,
        }
    }
}

fn default_visible_atlas_layers() -> BTreeSet<AtlasItemKind> {
    [
        AtlasItemKind::Directive,
        AtlasItemKind::Tool,
        AtlasItemKind::Knowledge,
        AtlasItemKind::Config,
        AtlasItemKind::Other,
    ]
    .into_iter()
    .collect()
}

impl AtlasItemKind {
    pub fn from_str(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "directive" | "directives" => Self::Directive,
            "tool" | "tools" => Self::Tool,
            "knowledge" => Self::Knowledge,
            "config" | "configs" | "configuration" => Self::Config,
            _ => Self::Other,
        }
    }

    pub fn layer_offset(self) -> f32 {
        match self {
            Self::Config => 0.0,
            Self::Knowledge => 0.35,
            Self::Tool => 0.7,
            Self::Directive => 1.05,
            Self::Other => 0.18,
        }
    }

    pub fn glyph(self) -> char {
        match self {
            Self::Directive => '◆',
            Self::Tool => '⚙',
            Self::Knowledge => '◈',
            Self::Config => '◇',
            Self::Other => '●',
        }
    }
}

impl AtlasScope {
    pub fn from_str(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "project" | "project_ai" => Self::Project,
            "user" | "user_ai" => Self::User,
            "system" | "system_ai" => Self::System,
            _ => Self::Unknown,
        }
    }
}
