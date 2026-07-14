use serde::{Deserialize, Serialize};

use super::super::event::RyeOsUiIntent;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsOverlayChoice {
    pub label: String,
    pub hint: String,
    pub intent: RyeOsUiIntent,
    pub secondary_intent: Option<RyeOsUiIntent>,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsShortcutEntryVm {
    pub category: String,
    pub keys: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsTileIntentVm {
    pub label: String,
    pub title: String,
    pub intent: RyeOsUiIntent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsOverlayVm {
    pub id: String,
    pub title: String,
    pub widget: String,
    #[serde(default)]
    pub columns: Vec<String>,
    pub query: String,
    pub selected: usize,
    pub hint: String,
    pub items: Vec<RyeOsOverlayItemVm>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RyeOsOverlayItemVm {
    pub category: String,
    pub primary: String,
    pub secondary: String,
    pub meta: String,
    pub enabled: bool,
    pub intent: Option<RyeOsUiIntent>,
    pub secondary_intent: Option<RyeOsUiIntent>,
    /// Tree indent level: 0 for flat items and group headers, 1 for a
    /// header's children.
    #[serde(default)]
    pub depth: u8,
    /// A launcher group header row — its intent folds the group, and
    /// renderers draw the fold glyph from `expanded`.
    #[serde(default)]
    pub header: bool,
    #[serde(default)]
    pub expanded: bool,
}
