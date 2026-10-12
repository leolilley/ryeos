//! Authored shared presentation input, consumed by the existing config kind.
//!
//! This declaration selects content; it is not a session, seat, attachment or
//! execution grant. The daemon must resolve the Config and each View at one
//! checked generation, then use the ordinary binding compiler. Surface seeds
//! require their own admission. No renderer fallback supplies missing content.

use crate::surface::{
    SlotContentSpec, SlotsSpec, SurfaceCapabilitySpec, SurfaceSpec, ViewKindSpec,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const SHARED_PRESENTATION_SCHEMA: u32 = 1;

/// Requested root selection, not provenance or admission. The daemon resolves
/// this exact kind and retains the resulting engine identity independently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiCompositionSelection {
    SharedPresentation { config_ref: String },
    Surface { surface_ref: String },
}

impl UiCompositionSelection {
    pub fn item_ref(&self) -> &str {
        match self {
            Self::SharedPresentation { config_ref } => config_ref,
            Self::Surface { surface_ref } => surface_ref,
        }
    }

    pub fn expected_kind(&self) -> &'static str {
        match self {
            Self::SharedPresentation { .. } => "config",
            Self::Surface { .. } => "surface",
        }
    }
}

/// Actual signed composition declaration. Shared Config is never a hidden
/// Surface. Embedded View values are retained separately from authored input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiCompositionDefinition {
    SharedPresentation { config: SharedPresentationConfig },
    Surface { surface: SurfaceSpec },
}

impl UiCompositionDefinition {
    pub fn expected_kind(&self) -> &'static str {
        match self {
            Self::SharedPresentation { .. } => "config",
            Self::Surface { .. } => "surface",
        }
    }

    pub fn surface(&self) -> Option<&SurfaceSpec> {
        match self {
            Self::Surface { surface } => Some(surface),
            Self::SharedPresentation { .. } => None,
        }
    }

    pub fn capabilities(&self) -> Option<&SurfaceCapabilitySpec> {
        match self {
            Self::SharedPresentation { config } => Some(&config.shared_presentation.capabilities),
            Self::Surface { surface } => surface.capabilities.as_ref(),
        }
    }

    pub fn title(&self) -> &str {
        match self {
            Self::SharedPresentation { config } => &config.shared_presentation.title,
            Self::Surface { surface } => surface.display_title(),
        }
    }
}

/// Sanitized renderer-neutral presentation produced beside a compiled binding.
/// Identities and authority remain in the daemon-retained binding, not this
/// projection. A degraded View is an honest value, not fabricated provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiCompositionPresentation {
    pub definition: UiCompositionDefinition,
    pub views: BTreeMap<String, serde_json::Value>,
}

impl UiCompositionPresentation {
    /// Render/load projection for a real Surface declaration. Config callers
    /// receive None, rather than a fabricated empty Surface or an authority
    /// fallback. Embedded Views stay separate on the attachment wire.
    pub fn surface_value(&self) -> Result<Option<serde_json::Value>, serde_json::Error> {
        let Some(surface) = self.definition.surface() else {
            return Ok(None);
        };
        let mut value = serde_json::to_value(surface)?;
        value["views"] = serde_json::to_value(&self.views)?;
        Ok(Some(value))
    }

    /// Decode a Surface at the resolver boundary, separating its effective View
    /// closure from the typed declaration. Not a predecessor attachment parser.
    pub fn from_effective_surface(mut value: serde_json::Value) -> Result<Self, serde_json::Error> {
        crate::surface::view_sets::validate_effective_view_sets(&value)
            .map_err(<serde_json::Error as serde::de::Error>::custom)?;
        let fields = value.as_object_mut().ok_or_else(|| {
            <serde_json::Error as serde::de::Error>::custom("effective Surface is not a mapping")
        })?;
        let views = match fields.remove("views") {
            Some(views) => serde_json::from_value(views)?,
            None => BTreeMap::new(),
        };
        Ok(Self {
            definition: UiCompositionDefinition::Surface {
                surface: serde_json::from_value(value)?,
            },
            views,
        })
    }
}

/// Consumer-specific body of an effective config item. Config metadata and
/// provenance remain owned by the resolver, not these presentation fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedPresentationConfig {
    pub category: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    pub shared_presentation: SharedPresentationSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedPresentationSpec {
    pub schema: u32,
    pub title: String,
    /// Stable mount id of an ordinary dialog View, not a canonical ref or an
    /// executable endpoint. It remains available with no Surface open.
    pub launcher: String,
    /// Ordered entries. Equal View refs may have distinct mounted identities.
    pub dialogs: Vec<PresentationDialogSpec>,
    /// Four optional shared regions, outside every Surface's tab composition.
    #[serde(default)]
    pub docks: SlotsSpec,
    /// Ordered opening selections. Their closures are not admitted by merely
    /// including them here; opening each reuses Surface admission.
    #[serde(default)]
    pub initial_surfaces: Vec<PresentationSurfaceSeed>,
    /// Requested ceilings, not grants. Actual actions/sources are supplied by
    /// resolved Views and attenuated under current principal/node authority.
    pub capabilities: SurfaceCapabilitySpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresentationDialogSpec {
    pub id: String,
    pub title: String,
    pub view: ViewKindSpec,
}

impl PresentationDialogSpec {
    /// Common authored dialog grammar for Config and contextual Surface roots.
    /// Reference validation belongs to the existing canonical-reference owner.
    pub fn validate_entries(
        entries: &[Self],
        limits: SharedPresentationLimits,
        mut validate_view: impl FnMut(&str) -> Result<(), String>,
    ) -> Result<(), String> {
        if entries.len() > limits.max_dialogs {
            return Err("presentation exceeds dialog count limit".into());
        }
        let mut ids = BTreeSet::new();
        for entry in entries {
            validate_mount_id(&entry.id, limits.max_mount_id_bytes)?;
            if !ids.insert(entry.id.as_str()) {
                return Err(format!("duplicate dialog id '{}'", entry.id));
            }
            validate_text(&entry.title, limits.max_label_bytes, "dialog title")?;
            validate_view(&entry.view.0)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresentationSurfaceSeed {
    pub id: String,
    pub title: String,
    pub surface: String,
}

/// Bounds supplied by the existing admission/configuration owner. There is no
/// renderer-selected default policy. Raw input must also be bounded before
/// deserialization by that owner.
#[derive(Debug, Clone, Copy)]
pub struct SharedPresentationLimits {
    pub max_declaration_bytes: usize,
    pub max_dialogs: usize,
    pub max_initial_surfaces: usize,
    pub max_label_bytes: usize,
    pub max_mount_id_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentationReferenceKind {
    View,
    Surface,
}

impl SharedPresentationSpec {
    /// Validate presentation shape and ask the existing canonical-reference
    /// owner to validate each expected kind. This function never resolves or
    /// trusts a ref, and deliberately does not duplicate the engine's parser.
    pub fn validate(
        &self,
        limits: SharedPresentationLimits,
        mut validate_reference: impl FnMut(PresentationReferenceKind, &str) -> Result<(), String>,
    ) -> Result<(), String> {
        if self.schema != SHARED_PRESENTATION_SCHEMA {
            return Err("shared presentation schema is not current".into());
        }
        let encoded = serde_json::to_vec(self)
            .map_err(|error| format!("shared presentation cannot be encoded: {error}"))?;
        if encoded.len() > limits.max_declaration_bytes {
            return Err("shared presentation exceeds declaration byte limit".into());
        }
        validate_text(&self.title, limits.max_label_bytes, "presentation title")?;
        if self.dialogs.len() > limits.max_dialogs {
            return Err("shared presentation exceeds dialog count limit".into());
        }
        if self.initial_surfaces.len() > limits.max_initial_surfaces {
            return Err("shared presentation exceeds initial Surface count limit".into());
        }

        PresentationDialogSpec::validate_entries(&self.dialogs, limits, |view| {
            validate_reference(PresentationReferenceKind::View, view)
        })?;
        let dialog_ids: BTreeSet<_> = self
            .dialogs
            .iter()
            .map(|dialog| dialog.id.as_str())
            .collect();
        validate_mount_id(&self.launcher, limits.max_mount_id_bytes)?;
        if !dialog_ids.contains(self.launcher.as_str()) {
            return Err("shared launcher does not name a declared dialog mount".into());
        }

        for dock in [
            &self.docks.top,
            &self.docks.left,
            &self.docks.right,
            &self.docks.bottom,
        ]
        .into_iter()
        .flatten()
        {
            if dock.size == 0 {
                return Err("shared dock size must be nonzero".into());
            }
            let SlotContentSpec::View(view_ref) = &dock.content;
            validate_reference(PresentationReferenceKind::View, view_ref)?;
        }
        let mut surface_ids = BTreeSet::new();
        for seed in &self.initial_surfaces {
            validate_mount_id(&seed.id, limits.max_mount_id_bytes)?;
            if !surface_ids.insert(seed.id.as_str()) {
                return Err(format!("duplicate initial Surface id '{}'", seed.id));
            }
            validate_text(&seed.title, limits.max_label_bytes, "Surface title")?;
            validate_reference(PresentationReferenceKind::Surface, &seed.surface)?;
        }
        Ok(())
    }

    /// Exact shared View closure to resolve. Initial Surfaces are deliberately
    /// excluded. Deduplication is for definition resolution only, not mounts.
    pub fn referenced_views(&self) -> BTreeSet<&str> {
        self.dialogs
            .iter()
            .map(|dialog| dialog.view.0.as_str())
            .chain(
                [
                    &self.docks.top,
                    &self.docks.left,
                    &self.docks.right,
                    &self.docks.bottom,
                ]
                .into_iter()
                .flatten()
                .map(|dock| {
                    let SlotContentSpec::View(view_ref) = &dock.content;
                    view_ref.as_str()
                }),
            )
            .collect()
    }
}

fn validate_text(value: &str, max_bytes: usize, field: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(format!(
            "{field} is empty, contains controls or exceeds its byte limit"
        ));
    }
    Ok(())
}

fn validate_mount_id(value: &str, max_bytes: usize) -> Result<(), String> {
    if value.is_empty()
        || value.len() > max_bytes
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("presentation mount id must be a bounded ASCII identifier".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn declaration() -> SharedPresentationSpec {
        serde_json::from_value(json!({
            "schema": 1,
            "title": "RyeOS",
            "launcher": "launch",
            "dialogs": [{"id": "launch", "title": "Launch", "view": "view:ryeos/launcher"}],
            "capabilities": {"sources": [], "affordances": []}
        }))
        .unwrap()
    }

    fn limits() -> SharedPresentationLimits {
        SharedPresentationLimits {
            max_declaration_bytes: 4096,
            max_dialogs: 8,
            max_initial_surfaces: 8,
            max_label_bytes: 128,
            max_mount_id_bytes: 64,
        }
    }

    // Test reference owner only; production uses the engine's canonical parser.
    fn reference(kind: PresentationReferenceKind, value: &str) -> Result<(), String> {
        let prefix = match kind {
            PresentationReferenceKind::View => "view:",
            PresentationReferenceKind::Surface => "surface:",
        };
        value
            .strip_prefix(prefix)
            .filter(|path| !path.is_empty())
            .map(|_| ())
            .ok_or_else(|| "reference kind refused".into())
    }

    #[test]
    fn shared_controls_require_neither_surface_nor_dock() {
        let spec = declaration();
        assert!(spec.validate(limits(), reference).is_ok());
        assert!(spec.initial_surfaces.is_empty());
        assert_eq!(
            spec.referenced_views(),
            BTreeSet::from(["view:ryeos/launcher"])
        );
    }

    #[test]
    fn view_resolution_deduplicates_definitions_not_mounts() {
        let mut spec = declaration();
        spec.dialogs.push(PresentationDialogSpec {
            id: "second".into(),
            title: "Second".into(),
            view: spec.dialogs[0].view.clone(),
        });
        spec.initial_surfaces.push(PresentationSurfaceSeed {
            id: "home".into(),
            title: "Home".into(),
            surface: "surface:ryeos/home".into(),
        });
        assert!(spec.validate(limits(), reference).is_ok());
        assert_eq!(spec.dialogs.len(), 2);
        assert_eq!(spec.referenced_views().len(), 1);
    }

    #[test]
    fn unknown_fields_and_predecessor_schema_are_refused() {
        let mut spec = declaration();
        spec.schema = 0;
        assert!(spec.validate(limits(), reference).is_err());
        let mut value = serde_json::to_value(declaration()).unwrap();
        value["mode"] = json!("observe");
        assert!(serde_json::from_value::<SharedPresentationSpec>(value).is_err());
    }

    #[test]
    fn missing_launcher_duplicate_ids_and_wrong_kind_are_refused() {
        let mut spec = declaration();
        spec.launcher = "missing".into();
        assert!(spec.validate(limits(), reference).is_err());
        spec = declaration();
        spec.dialogs.push(spec.dialogs[0].clone());
        assert!(spec.validate(limits(), reference).is_err());
        spec = declaration();
        spec.initial_surfaces.push(PresentationSurfaceSeed {
            id: "home".into(),
            title: "Home".into(),
            surface: "view:ryeos/home".into(),
        });
        assert!(spec.validate(limits(), reference).is_err());
    }

    #[test]
    fn owner_limits_and_ordinary_dock_refs_are_enforced() {
        let mut spec = declaration();
        let mut bounded = limits();
        bounded.max_dialogs = 0;
        assert!(spec.validate(bounded, reference).is_err());
        bounded = limits();
        bounded.max_declaration_bytes = 1;
        assert!(spec.validate(bounded, reference).is_err());
        spec.docks.left = Some(
            serde_json::from_value(json!({
                "content": "view:ryeos/terminal", "open": false, "size": 8
            }))
            .unwrap(),
        );
        assert!(spec.validate(limits(), reference).is_ok());
        assert!(spec.referenced_views().contains("view:ryeos/terminal"));
        spec.docks.left.as_mut().unwrap().size = 0;
        assert!(spec.validate(limits(), reference).is_err());
    }
}
