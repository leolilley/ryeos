use serde::{Deserialize, Serialize};

use super::super::event::RyeOsUiIntent;

/// An ordinary admitted View mounted in the presentation's shared dialog region.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsDialogVm {
    pub instance_key: crate::ids::RyeOsViewInstanceKey,
    pub title: String,
    /// The mounted View's own `description`, which a dialog may show as its
    /// note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The View is the launcher's own list, drawn as the launcher sheet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub launcher: bool,
    pub view: super::RyeOsViewVm,
    pub input: Option<super::RyeOsInputVm>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsCommandChoice {
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
