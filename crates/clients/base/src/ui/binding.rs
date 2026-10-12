//! Renderer-neutral wire coordinates for daemon-compiled UI bindings.
//!
//! These DTOs identify an entry in a session-bound compiled table. They do not
//! carry an executable item ref, capability grant, or client-lowered command.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Exact public attachment coordinate. These bytes select retained authority;
/// they do not contain it and cannot admit an attachment by themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiBindingAttachmentCoordinate {
    pub binding_attachment_id: String,
    pub binding_generation: u64,
    pub binding_digest: String,
}

/// Session-owned membership projection for one independently presented core.
/// A copied descriptor is a lookup hint, never a producer claim or a grant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiPresentationMembership {
    pub presentation_id: String,
    pub root_binding: UiBindingAttachmentCoordinate,
    pub thread_id: Option<String>,
    pub chain_root_id: Option<String>,
    /// Checked presentation-wide allocation frontier; not a composition revision.
    pub next_mount_id: u64,
    /// Commitment to the immutable server-written initialization on this seat.
    /// It is not authority to claim a producer or to admit a binding.
    pub initialization_digest: String,
    pub surfaces: Vec<UiSurfaceMembership>,
}

pub use super::presentation_initialization::UiPresentationInitialization;

/// A Surface's session-retained presentation identity. Mutable tab/layout
/// state is owned by the Rust core, not by this admission descriptor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSurfaceMembership {
    pub surface_instance_id: String,
    pub ordinal: u64,
    pub title: String,
    pub binding: UiBindingAttachmentCoordinate,
    /// Immutable admitted initial layout and exact allocated mount identities.
    pub mount_manifest: super::composition_identity::UiSurfaceMountManifest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_surface: Option<UiSavedSurfaceComposition>,
}

/// Already admitted ordered saved tabs. Coordinates are session-owned
/// membership lookups; neither the saved IDs nor this projection grant access.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSavedSurfaceComposition {
    pub provenance: UiSurfaceCompositionProvenance,
    pub active_view_set_id: String,
    pub body: UiSavedSurfaceCompositionBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiSavedSurfaceCompositionBody {
    Particular { tabs: Vec<UiSavedSurfaceTab> },
    Reusable { tabs: Vec<UiReusableSurfaceTab> },
    Composed { tabs: Vec<UiReusableSurfaceTab> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiSurfaceCompositionProvenance {
    Library {
        saved_id: String,
        library_revision: u64,
        record_digest: String,
    },
    Composed {
        composition: crate::surface::opening::ComposedSurfaceSelection,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSurfaceTabsAdmissionTransition {
    pub kind: UiSurfaceTabsAdmissionKind,
    pub presentation_id: String,
    pub creation_request_id: String,
    pub opening: crate::surface::opening::SurfaceOpenRequest,
    pub composition: Option<UiSavedSurfaceComposition>,
    pub origin_attachment: UiBindingAttachment,
    pub tab_attachments: Vec<UiBindingAttachment>,
    pub manifest: super::composition_identity::UiSurfaceMountManifest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiSurfaceTabsAdmissionKind {
    ImportSurfaceTabs,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiPreparedPresentationDocument {
    pub presentation_id: String,
    pub thread_id: String,
    pub chain_root_id: String,
    pub root_binding: UiBindingAttachmentCoordinate,
    pub initialization: UiPresentationInitialization,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiPreparedPresentationDocumentKind {
    OpenPresentationDocument,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiPreparedPresentationDocumentTransition {
    pub kind: UiPreparedPresentationDocumentKind,
    pub presentation_id: String,
    pub creation_request_id: String,
    pub opening: crate::surface::opening::SurfaceOpenRequest,
    pub prepared: UiPreparedPresentationDocument,
}
impl UiPreparedPresentationDocumentTransition {
    pub fn validate(&self) -> Result<(), String> {
        self.opening.validate()?;
        self.prepared.initialization.validate()?;
        let crate::surface::opening::SurfaceOpenPlacement::NewPresentation { root_binding } =
            &self.opening.placement
        else {
            return Err("document receipt has a non-document placement".into());
        };
        if !super::admission_operation::canonical_operation_id(&self.presentation_id)
            || self.creation_request_id != self.opening.creation_request_id
            || self.prepared.presentation_id == self.presentation_id
            || self.prepared.presentation_id != self.prepared.initialization.presentation_id
            || self.prepared.initialization.creation_request_id != self.creation_request_id
            || &self.prepared.root_binding != root_binding
            || self.prepared.initialization.root_binding != self.prepared.root_binding
            || self.prepared.thread_id.is_empty()
            || self.prepared.chain_root_id != self.prepared.thread_id
        {
            return Err("prepared document identity differs from opening".into());
        }
        match &self.prepared.initialization.selection {
            super::presentation_initialization::UiPresentationInitialSelection::SelectedSurface{opening} if opening==&self.opening=>Ok(()),
            _=>Err("prepared document initialization changes selected opening".into()),
        }
    }
}

impl UiSurfaceTabsAdmissionTransition {
    pub fn validate(&self) -> Result<(), String> {
        use crate::surface::opening::SurfaceOpenPlacement;
        self.opening.validate()?;
        self.origin_attachment.validate().map_err(str::to_owned)?;
        self.manifest.validate()?;
        let SurfaceOpenPlacement::AppendTabs {
            destination_surface_instance_id,
            expected_next_mount_id,
            ..
        } = &self.opening.placement
        else {
            return Err("tab import receipt has a new-Surface placement".into());
        };
        if self.creation_request_id != self.opening.creation_request_id
            || self.manifest.presentation_id.as_str() != self.presentation_id
            || &self.manifest.surface_instance_id != destination_surface_instance_id
            || self.manifest.allocation_start != *expected_next_mount_id
        {
            return Err("tab import identity or allocation contradicts request".into());
        }
        let (seeds, active) = validate_surface_open_selection(
            &self.opening.selection,
            self.composition.as_ref(),
            &self.origin_attachment,
            &self.tab_attachments,
        )?;
        let expected = super::composition_identity::UiSurfaceMountManifest::allocate(
            self.manifest.presentation_id.clone(),
            destination_surface_instance_id.clone(),
            *expected_next_mount_id,
            seeds,
            active.as_deref(),
        )?;
        if expected != self.manifest {
            return Err("import manifest differs from exact ordered composition".into());
        }
        Ok(())
    }
}

pub fn validate_surface_open_selection(
    selection: &crate::surface::opening::SurfaceOpenSelection,
    composition: Option<&UiSavedSurfaceComposition>,
    origin_attachment: &UiBindingAttachment,
    tab_attachments: &[UiBindingAttachment],
) -> Result<
    (
        Vec<(
            crate::surface::view_sets::ViewSetSeedSpec,
            UiBindingAttachmentCoordinate,
        )>,
        Option<String>,
    ),
    String,
> {
    use crate::surface::opening::SurfaceOpenSelection;
    origin_attachment.validate().map_err(str::to_owned)?;
    let coordinate = |entry: &UiBindingAttachment| UiBindingAttachmentCoordinate {
        binding_attachment_id: entry.binding_attachment_id.clone(),
        binding_generation: entry.binding_generation,
        binding_digest: entry.binding_digest.clone(),
    };
    let mut descriptors = BTreeMap::new();
    for descriptor in tab_attachments {
        descriptor.validate().map_err(str::to_owned)?;
        if descriptors
            .insert(descriptor.binding_attachment_id.as_str(), descriptor)
            .is_some()
        {
            return Err("tab import repeats a descriptor".into());
        }
    }
    let (seeds, active) = match (selection, &composition) {
        (SurfaceOpenSelection::Authored { surface_ref }, None) => {
            if &origin_attachment.root_ref != surface_ref || !descriptors.is_empty() {
                return Err("authored tab import changes origin or adds descriptors".into());
            }
            let seeds = origin_attachment
                .presentation
                .definition
                .surface()
                .ok_or("import origin is not a Surface")?
                .to_mount_seeds()?;
            let active = seeds.first().map(|seed| seed.id.clone());
            (
                seeds
                    .into_iter()
                    .map(|seed| (seed, coordinate(&origin_attachment)))
                    .collect::<Vec<_>>(),
                active,
            )
        }
        (selection, Some(composition)) => {
            composition.validate()?;
            let tabs = composition
                .portable_tabs()
                .ok_or("import cannot carry particular resume")?;
            match (selection, &composition.provenance) {
                (
                    SurfaceOpenSelection::Reusable {
                        id,
                        expected_revision,
                        fresh_subjects,
                    },
                    UiSurfaceCompositionProvenance::Library {
                        saved_id,
                        library_revision,
                        ..
                    },
                ) if id == saved_id && expected_revision == library_revision => {
                    if fresh_subjects
                        .keys()
                        .any(|id| !tabs.iter().any(|tab| &tab.saved_set_id == id))
                        || tabs.iter().any(|tab| {
                            fresh_subjects
                                .get(&tab.saved_set_id)
                                .map_or(!tab.fresh_subjects.is_empty(), |values| {
                                    values != &tab.fresh_subjects
                                })
                        })
                    {
                        return Err("import changes reusable fresh subjects".into());
                    }
                }
                (
                    SurfaceOpenSelection::Composed(request),
                    UiSurfaceCompositionProvenance::Composed {
                        composition: actual,
                    },
                ) if request == actual => {
                    if !origin_matches_descriptor(&request.origin, &origin_attachment) {
                        return Err("composed import origin differs".into());
                    }
                }
                _ => return Err("import composition differs from selected provenance".into()),
            }
            if descriptors.len() != tabs.len() {
                return Err("tab import descriptor closure differs".into());
            }
            for tab in tabs {
                let descriptor = descriptors
                    .get(tab.binding.binding_attachment_id.as_str())
                    .ok_or("imported tab descriptor missing")?;
                if coordinate(descriptor) != tab.binding
                    || !origin_matches_descriptor(&tab.origin, descriptor)
                {
                    return Err("imported tab binding or origin differs".into());
                }
            }
            (
                composition.mount_seeds(),
                Some(composition.active_view_set_id.clone()),
            )
        }
        _ => return Err("import selection/body differs".into()),
    };

    Ok((seeds, active))
}

pub fn origin_matches_descriptor(
    origin: &crate::surface::saved_surfaces::SavedSurfaceOrigin,
    descriptor: &UiBindingAttachment,
) -> bool {
    origin.surface_ref == descriptor.root_ref
        && match (&origin.project, &descriptor.subject) {
            (None, UiBindingSubject::Projectless) => true,
            (Some(project), UiBindingSubject::RegisteredProject { local_id }) => {
                &project.local_id == local_id
            }
            _ => false,
        }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiReusableSurfaceTab {
    pub saved_set_id: String,
    pub binding: UiBindingAttachmentCoordinate,
    pub origin: crate::surface::saved_surfaces::SavedSurfaceOrigin,
    pub template: crate::surface::view_sets::SavedViewSetTemplate,
    pub fresh_subjects: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
}

impl UiSavedSurfaceComposition {
    pub fn library_identity(&self) -> Option<(&str, u64, &str)> {
        match &self.provenance {
            UiSurfaceCompositionProvenance::Library {
                saved_id,
                library_revision,
                record_digest,
            } => Some((saved_id, *library_revision, record_digest)),
            UiSurfaceCompositionProvenance::Composed { .. } => None,
        }
    }
    /// Validate the closed, authority-free shape projected by saved-Surface
    /// admission. Binding coordinates remain lookup coordinates; the daemon
    /// separately proves that every one belongs to retained session authority.
    pub fn validate(&self) -> Result<(), String> {
        use crate::surface::view_sets::{
            SavedViewSelectionSource, SavedViewSetTemplate, validate_saved_view_set_templates,
        };

        let bounded_id = |value: &str| {
            !value.is_empty()
                && value.len() <= crate::surface::view_sets::MAX_SAVED_VIEW_SET_ID_BYTES
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        };
        let valid_binding = |binding: &UiBindingAttachmentCoordinate| {
            !binding.binding_attachment_id.is_empty()
                && binding.binding_generation != 0
                && !binding.binding_digest.is_empty()
        };
        let valid_origin = |origin: &crate::surface::saved_surfaces::SavedSurfaceOrigin| {
            origin
                .surface_ref
                .strip_prefix("surface:")
                .is_some_and(|reference| {
                    !reference.is_empty()
                        && reference.trim() == reference
                        && !reference.chars().any(char::is_control)
                })
                && origin.project.as_ref().is_none_or(|project| {
                    !project.local_id.is_empty()
                        && project.local_id.len()
                            <= crate::surface::view_sets::MAX_PROJECT_LOCAL_ID_BYTES
                        && project.local_id.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':')
                        })
                })
        };
        match (&self.provenance, &self.body) {
            (
                UiSurfaceCompositionProvenance::Library {
                    saved_id,
                    record_digest,
                    ..
                },
                UiSavedSurfaceCompositionBody::Particular { .. }
                | UiSavedSurfaceCompositionBody::Reusable { .. },
            ) if bounded_id(saved_id)
                && record_digest.len() == 64
                && record_digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) => {}
            (
                UiSurfaceCompositionProvenance::Composed { composition },
                UiSavedSurfaceCompositionBody::Composed { tabs },
            ) => {
                composition.validate()?;
                if composition.active_view_set_id != self.active_view_set_id
                    || composition.tabs.len() != tabs.len()
                    || composition
                        .tabs
                        .iter()
                        .zip(tabs)
                        .any(|(requested, admitted)| {
                            requested.origin != admitted.origin
                                || requested.template != admitted.template
                                || requested.fresh_subjects != admitted.fresh_subjects
                        })
                {
                    return Err("composed admission differs from its exact portable request".into());
                }
            }
            _ => return Err("Surface composition provenance/body is inconsistent".into()),
        }

        let mut ids = std::collections::BTreeSet::new();
        let mut bindings = std::collections::BTreeSet::new();
        let mut templates = Vec::new();
        match &self.body {
            UiSavedSurfaceCompositionBody::Particular { tabs } => {
                for tab in tabs {
                    if !bounded_id(&tab.saved_set_id)
                        || !ids.insert(tab.saved_set_id.as_str())
                        || !valid_binding(&tab.binding)
                        || !bindings.insert((
                            tab.binding.binding_attachment_id.as_str(),
                            tab.binding.binding_generation,
                            tab.binding.binding_digest.as_str(),
                        ))
                        || tab.resolved.id != tab.saved_set_id
                        || tab.resolved.validate().is_err()
                    {
                        return Err("particular saved Surface tab is inconsistent".into());
                    }
                    templates.push(SavedViewSetTemplate {
                        id: tab.resolved.id.clone(),
                        name: tab.resolved.name.clone(),
                        composition: tab.resolved.composition.clone(),
                        relationships: tab.resolved.relationships.clone(),
                    });
                }
            }
            UiSavedSurfaceCompositionBody::Reusable { tabs }
            | UiSavedSurfaceCompositionBody::Composed { tabs } => {
                for tab in tabs {
                    if !bounded_id(&tab.saved_set_id)
                        || !ids.insert(tab.saved_set_id.as_str())
                        || !valid_binding(&tab.binding)
                        || !bindings.insert((
                            tab.binding.binding_attachment_id.as_str(),
                            tab.binding.binding_generation,
                            tab.binding.binding_digest.as_str(),
                        ))
                        || tab.template.id != tab.saved_set_id
                        || !valid_origin(&tab.origin)
                    {
                        return Err("reusable saved Surface tab is inconsistent".into());
                    }
                    let required = tab
                        .template
                        .relationships
                        .iter()
                        .filter_map(|relationship| match &relationship.source {
                            SavedViewSelectionSource::RequiredSubject { input, facets } => {
                                Some((input.as_str(), facets))
                            }
                            _ => None,
                        })
                        .collect::<std::collections::BTreeMap<_, _>>();
                    for (input, supplied) in &tab.fresh_subjects {
                        let facets = required
                            .get(input.as_str())
                            .ok_or("reusable saved Surface supplies an unused subject")?;
                        if supplied.len() != facets.len()
                            || facets.iter().any(|facet| !supplied.contains_key(facet))
                        {
                            return Err(
                                "reusable saved Surface subject differs from its exact contract"
                                    .into(),
                            );
                        }
                    }
                    templates.push(tab.template.clone());
                }
            }
        }
        if ids.is_empty() || !ids.contains(self.active_view_set_id.as_str()) {
            return Err("saved Surface has no exact active tab".into());
        }
        validate_saved_view_set_templates(&templates)?;
        for template in &templates {
            if template.relationships.iter().any(|relationship| {
                matches!(
                    &relationship.source,
                    SavedViewSelectionSource::FollowSet { saved_view_set_id }
                        if !ids.contains(saved_view_set_id.as_str())
                )
            }) {
                return Err("saved Surface follower leaves its saved collection".into());
            }
        }
        Ok(())
    }

    /// Ordered admitted tab seeds, preserving their exact binding coordinates.
    /// This projects retained data only; it does not validate admission authority.
    pub fn mount_seeds(
        &self,
    ) -> Vec<(
        crate::surface::view_sets::ViewSetSeedSpec,
        UiBindingAttachmentCoordinate,
    )> {
        match &self.body {
            UiSavedSurfaceCompositionBody::Particular { tabs } => tabs
                .iter()
                .map(|tab| (tab.resolved.composition.clone(), tab.binding.clone()))
                .collect(),
            UiSavedSurfaceCompositionBody::Reusable { tabs }
            | UiSavedSurfaceCompositionBody::Composed { tabs } => tabs
                .iter()
                .map(|tab| (tab.template.composition.clone(), tab.binding.clone()))
                .collect(),
        }
    }
    pub fn tab_count(&self) -> usize {
        match &self.body {
            UiSavedSurfaceCompositionBody::Particular { tabs } => tabs.len(),
            UiSavedSurfaceCompositionBody::Reusable { tabs }
            | UiSavedSurfaceCompositionBody::Composed { tabs } => tabs.len(),
        }
    }
    pub fn tab_bindings(&self) -> Vec<&UiBindingAttachmentCoordinate> {
        match &self.body {
            UiSavedSurfaceCompositionBody::Particular { tabs } => {
                tabs.iter().map(|tab| &tab.binding).collect()
            }
            UiSavedSurfaceCompositionBody::Reusable { tabs }
            | UiSavedSurfaceCompositionBody::Composed { tabs } => {
                tabs.iter().map(|tab| &tab.binding).collect()
            }
        }
    }
    pub fn particular_tabs(&self) -> Option<&[UiSavedSurfaceTab]> {
        match &self.body {
            UiSavedSurfaceCompositionBody::Particular { tabs } => Some(tabs),
            _ => None,
        }
    }
    pub fn reusable_tabs(&self) -> Option<&[UiReusableSurfaceTab]> {
        match &self.body {
            UiSavedSurfaceCompositionBody::Reusable { tabs } => Some(tabs),
            _ => None,
        }
    }
    pub fn portable_tabs(&self) -> Option<&[UiReusableSurfaceTab]> {
        match &self.body {
            UiSavedSurfaceCompositionBody::Reusable { tabs }
            | UiSavedSurfaceCompositionBody::Composed { tabs } => Some(tabs),
            _ => None,
        }
    }
}

#[cfg(test)]
mod saved_surface_tests {
    use super::*;
    use serde_json::json;

    fn reusable() -> UiSavedSurfaceComposition {
        serde_json::from_value(json!({
            "provenance":{"kind":"library","saved_id":"saved","library_revision":1,"record_digest":"ab".repeat(32)},
            "active_view_set_id":"one","body":{"kind":"reusable","tabs":[{
                "saved_set_id":"one",
                "binding":{"binding_attachment_id":"attachment","binding_generation":1,"binding_digest":"digest"},
                "origin":{"surface_ref":"surface:test/one"},
                "template":{"id":"one","name":"One","composition":{"schema":2,"id":"one","title":"One","root":null},"relationships":[]},
                "fresh_subjects":{}
            }]}
        }))
        .unwrap()
    }

    #[test]
    fn saved_surface_closed_shape_requires_exact_active_tab_and_digest() {
        let valid = reusable();
        valid.validate().unwrap();
        let mut missing = valid.clone();
        missing.active_view_set_id = "missing".into();
        assert!(missing.validate().is_err());
        let mut digest = valid;
        let UiSurfaceCompositionProvenance::Library { record_digest, .. } = &mut digest.provenance
        else {
            unreachable!()
        };
        *record_digest = "not-a-digest".into();
        assert!(digest.validate().is_err());
    }

    #[test]
    fn particular_saved_surface_requires_bounded_resolved_work_coordinates() {
        let particular = || -> UiSavedSurfaceComposition {
            serde_json::from_value(json!({
                "provenance":{"kind":"library","saved_id":"saved","library_revision":1,"record_digest":"ab".repeat(32)},
                "active_view_set_id":"one","body":{"kind":"particular","tabs":[{
                    "saved_set_id":"one",
                    "binding":{"binding_attachment_id":"attachment","binding_generation":1,"binding_digest":"digest"},
                    "resolved":{"id":"one","name":"One","composition":{"schema":2,"id":"one","title":"One","root":null},
                        "relationships":[],"resolved_selection_work":{"thread":"T-head","chain_root":"T-root"}}
                }]}
            }))
            .unwrap()
        };
        particular().validate().unwrap();

        for (field, value) in [
            ("thread", String::new()),
            ("thread", "bad/work".into()),
            ("chain_root", String::new()),
            (
                "chain_root",
                "x".repeat(crate::surface::view_sets::MAX_WORK_CHAIN_ROOT_ID_BYTES + 1),
            ),
        ] {
            let mut invalid = particular();
            let UiSavedSurfaceCompositionBody::Particular { tabs } = &mut invalid.body else {
                unreachable!()
            };
            let work = tabs[0].resolved.resolved_selection_work.as_mut().unwrap();
            match field {
                "thread" => work.thread = value,
                "chain_root" => work.chain_root = value,
                _ => unreachable!(),
            }
            assert!(invalid.validate().is_err());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSavedSurfaceTab {
    pub saved_set_id: String,
    pub binding: UiBindingAttachmentCoordinate,
    pub resolved: crate::surface::view_sets::ResolvedParticularViewSet,
}

/// Exact active writer coordinate supplied by seat admission. Possessing these
/// identifiers is not authority: the daemon checks retained membership and the
/// current producer under the seat-transition admission lock on every dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiPresentationProducer {
    pub presentation_id: String,
    pub thread_id: String,
    pub producer_incarnation: String,
}

/// Authenticated read-only observation of the retained presentation writer.
/// It is not a claim, a lease renewal or evidence that a document has died.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiPresentationProducerObservation {
    pub presentation_id: String,
    pub thread_id: String,
    pub chain_root_id: String,
    pub root_binding: UiBindingAttachmentCoordinate,
    /// Checked composition revision at this same authenticated seat cut.
    /// This is observation only; transfer admission rechecks it under CAS.
    pub revision: u64,
    /// Exact authenticated initialization/control CAS event, never its content
    /// digest or a producer incarnation. This is a fence coordinate, not a grant.
    pub control_predecessor: UiSeatControlPredecessor,
    pub producer: UiObservedPresentationProducer,
}

pub use ryeos_ui_presentation_contract::seat_control::{
    SEAT_PRODUCER_EVENT, SEAT_PRODUCER_SCHEMA, UiSeatControlPredecessor, UiSeatControlRequest,
    UiSeatProducer, UiSeatProducerClaim, UiSeatProducerState,
};

#[cfg(test)]
mod control_reexport_tests {
    #[test]
    fn public_control_request_is_the_data_only_contract_without_a_conversion() {
        let mut request = super::UiSeatControlRequest::Claim {
            schema: 2,
            operation_id: String::new(),
            presentation_id: "22222222-2222-4222-8222-222222222222".into(),
            thread_id: "seat".into(),
            chain_root_id: "root".into(),
            predecessor: super::UiSeatControlPredecessor::Initialization {
                event_hash: "ab".repeat(32),
            },
            intent_nonce: "11111111-1111-4111-8111-111111111111".into(),
            claim: super::UiSeatProducerClaim::Fresh,
        };
        request.assign_identity().unwrap();
        let shared: ryeos_ui_presentation_contract::seat_control::UiSeatControlRequest = request;
        assert_eq!(
            shared.operation_id(),
            "5fb13967-e0b2-8a65-877d-1eeba338294f"
        );
        shared.validate().unwrap();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiObservedPresentationProducer {
    Unclaimed,
    Active { producer_incarnation: String },
    Relinquished { producer_incarnation: String },
    Closed { producer_incarnation: String },
}

/// Transport envelope. Binding identity and producer identity are different
/// checks; neither an attachment nor a session cookie supplies a seat writer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiInvocationRequest {
    pub producer: UiPresentationProducer,
    pub binding: UiBindingRequest,
    /// Required only for a signed presentation-admission result effect. The
    /// daemon resolves and compares the existing seat's original prepared row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation_operation_id: Option<String>,
}

impl UiInvocationRequest {
    pub fn validate_bounds(
        &self,
        bounds: UiBindingRequestBounds,
    ) -> Result<(), UiBindingRequestError> {
        if self.producer.presentation_id.is_empty()
            || self.producer.thread_id.is_empty()
            || self.producer.producer_incarnation.is_empty()
        {
            return Err(UiBindingRequestError::InvalidProducer);
        }
        self.binding.validate_bounds(bounds)?;
        let encoded = serde_json::to_vec(self).map_err(|_| UiBindingRequestError::NotJson)?;
        let actual = u64::try_from(encoded.len()).unwrap_or(u64::MAX);
        if actual > bounds.max_request_bytes {
            return Err(UiBindingRequestError::TooLarge {
                actual,
                maximum: bounds.max_request_bytes,
            });
        }
        Ok(())
    }
}

/// Informational posture derived by the daemon from the compiled binding.
/// It is presentation data, never an authorization input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiEffectivePosture {
    #[default]
    ObservationOnly,
    Interactive,
}

/// Public projection of one server-admitted binding. This is not a grant:
/// every request is independently checked against the cookie-authenticated
/// session's retained attachment. Display paths cannot admit a project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiBindingSubject {
    Projectless,
    UnregisteredProject,
    RegisteredProject { local_id: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiBindingAttachment {
    pub binding_attachment_id: String,
    pub binding_generation: u64,
    pub binding_digest: String,
    pub root_ref: String,
    pub root_generation: String,
    pub presentation: crate::presentation::UiCompositionPresentation,
    pub project_path: Option<String>,
    pub subject: UiBindingSubject,
    pub posture: UiEffectivePosture,
    pub binding_request_bounds: UiBindingRequestBounds,
}

impl UiBindingAttachment {
    /// Validate the public coordinate before retaining it. This establishes
    /// shape only; the daemon remains the authority for admission.
    pub fn validate(&self) -> Result<(), &'static str> {
        let has_project = self.project_path.as_deref().is_some_and(|path| {
            !path.is_empty() && path.trim() == path && !path.chars().any(char::is_control)
        });
        match &self.subject {
            UiBindingSubject::Projectless if self.project_path.is_none() => {}
            UiBindingSubject::UnregisteredProject if has_project => {}
            UiBindingSubject::RegisteredProject { local_id }
                if has_project
                    && !local_id.is_empty()
                    && local_id.len() <= crate::surface::view_sets::MAX_PROJECT_LOCAL_ID_BYTES
                    && local_id.trim() == local_id
                    && !local_id.chars().any(char::is_control) => {}
            _ => return Err("binding attachment subject contradicts its project metadata"),
        }
        if self.binding_attachment_id.is_empty()
            || self.binding_generation == 0
            || self.binding_digest.is_empty()
            || self.root_ref.is_empty()
            || self.root_generation.is_empty()
        {
            return Err("binding attachment has an incomplete coordinate");
        }
        if self.binding_request_bounds.max_request_bytes == 0
            || self.binding_request_bounds.max_input_bytes == 0
            || self.binding_request_bounds.max_seat_append_bytes == 0
        {
            return Err("binding attachment has invalid request bounds");
        }
        // Shape only: the engine owns canonical reference parsing and trust.
        let expected = self.presentation.definition.expected_kind();
        let prefix = format!("{expected}:");
        if !self
            .root_ref
            .strip_prefix(&prefix)
            .is_some_and(|path| !path.is_empty())
        {
            return Err("binding attachment root kind does not match its declaration");
        }
        if self
            .presentation
            .definition
            .surface()
            .is_some_and(|surface| surface.views.is_some())
        {
            return Err("binding attachment Views must be in the separate presentation map");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiBindingCoordinate {
    Source {
        view_ref: String,
        channel: String,
    },
    Affordance {
        view_ref: String,
        affordance_id: String,
    },
    /// The one input route authored by the effective surface. Its executable
    /// template is retained in the compiled session binding.
    SurfaceRoute,
}

/// Reserved channel coordinates for sources declared by an input block rather
/// than in the view's ordinary `sources` map. These remain coordinates inside
/// the compiled view binding; they are not executable refs supplied by a
/// renderer.
pub fn input_mentions_channel(input_id: &str) -> String {
    format!("input.{input_id}.mentions")
}

pub fn input_completion_channel(input_id: &str) -> String {
    format!("input.{input_id}.completion")
}

/// One session-bound source fetch or affordance invocation. The daemon proves
/// that payload kind matches the coordinate and signed producer before it
/// substitutes or dispatches anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiBindingRequest {
    pub binding_attachment_id: String,
    pub binding_generation: u64,
    pub binding_digest: String,
    pub coordinate: UiBindingCoordinate,
    pub payload: UiBindingPayload,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiBindingPayload {
    SourceParameters {
        params: Value,
    },
    Selection {
        record: Value,
    },
    Tokens {
        tokens: Vec<String>,
        #[serde(default)]
        arguments: Value,
    },
    Input {
        value: String,
        /// Dynamic delivery coordinate from the session's seat braid. This is
        /// accepted only for `SurfaceRoute`; it can select a continuation but
        /// cannot alter the signed executable route.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        route: Option<UiBindingRouteContext>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiBindingRouteContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_root_id: Option<String>,
    #[serde(default)]
    pub interrupt: bool,
}

impl UiBindingRequest {
    /// Enforce the bounds compiled into the active session binding. The daemon
    /// independently enforces the same authority; this client check prevents
    /// avoidable allocation and transport work, but never grants permission.
    pub fn validate_bounds(
        &self,
        bounds: UiBindingRequestBounds,
    ) -> Result<(), UiBindingRequestError> {
        if self.binding_attachment_id.is_empty()
            || self.binding_generation == 0
            || self.binding_digest.is_empty()
        {
            return Err(UiBindingRequestError::InvalidAttachment);
        }
        if bounds.max_request_bytes == 0
            || bounds.max_input_bytes == 0
            || bounds.max_seat_append_bytes == 0
        {
            return Err(UiBindingRequestError::InvalidBounds);
        }
        let encoded = serde_json::to_vec(self).map_err(|_| UiBindingRequestError::NotJson)?;
        let encoded_bytes = u64::try_from(encoded.len()).unwrap_or(u64::MAX);
        if encoded_bytes > bounds.max_request_bytes {
            return Err(UiBindingRequestError::TooLarge {
                actual: encoded_bytes,
                maximum: bounds.max_request_bytes,
            });
        }
        let input_bytes = |value: &str| u64::try_from(value.len()).unwrap_or(u64::MAX);
        if let UiBindingPayload::Input { value, .. } = &self.payload
            && input_bytes(value) > bounds.max_input_bytes
        {
            return Err(UiBindingRequestError::InputTooLarge {
                actual: input_bytes(value),
                maximum: bounds.max_input_bytes,
            });
        }
        let coordinate_matches = matches!(
            (&self.coordinate, &self.payload),
            (
                UiBindingCoordinate::Source { .. },
                UiBindingPayload::SourceParameters { .. }
            ) | (
                UiBindingCoordinate::Affordance { .. },
                UiBindingPayload::Selection { .. } | UiBindingPayload::Tokens { .. }
            ) | (
                UiBindingCoordinate::Affordance { .. },
                UiBindingPayload::Input { route: None, .. }
            ) | (
                UiBindingCoordinate::SurfaceRoute,
                UiBindingPayload::Input { .. }
            )
        );
        if !coordinate_matches {
            return Err(UiBindingRequestError::PayloadMismatch);
        }
        Ok(())
    }
}

/// Limits compiled for the active binding/session by existing authority.
/// These are inputs to validation, not client-selected defaults.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiBindingRequestBounds {
    pub max_request_bytes: u64,
    pub max_input_bytes: u64,
    /// Existing signed seat/append route body limit. Admission write-ahead
    /// charges its full event envelope before creating a provisional record.
    pub max_seat_append_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UiBindingRequestError {
    #[error("UI invocation requires an exact presentation producer coordinate")]
    InvalidProducer,
    #[error("UI binding request requires an exact admitted attachment coordinate")]
    InvalidAttachment,
    #[error("UI binding request bounds must be non-zero")]
    InvalidBounds,
    #[error("UI binding request is not canonical JSON")]
    NotJson,
    #[error("UI binding request is {actual} bytes; maximum is {maximum}")]
    TooLarge { actual: u64, maximum: u64 },
    #[error("UI binding input is {actual} bytes; maximum is {maximum}")]
    InputTooLarge { actual: u64, maximum: u64 },
    #[error("UI binding coordinate and producer payload do not match")]
    PayloadMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invocation_has_one_closed_producer_envelope_and_bounds_the_whole_request() {
        let binding = serde_json::json!({
            "binding_attachment_id": "attachment", "binding_generation": 1, "binding_digest": "digest",
            "coordinate": {"kind": "source", "view_ref": "view:test/data", "channel": "default"},
            "payload": {"kind": "source_parameters", "params": {}}
        });
        assert!(serde_json::from_value::<UiInvocationRequest>(binding.clone()).is_err());
        let mut wire = serde_json::json!({
            "producer": {"presentation_id": "presentation", "thread_id": "seat", "producer_incarnation": "producer"},
            "binding": binding,
        });
        let request: UiInvocationRequest = serde_json::from_value(wire.clone()).unwrap();
        let actual = serde_json::to_vec(&request).unwrap().len() as u64;
        assert!(
            request
                .validate_bounds(UiBindingRequestBounds {
                    max_request_bytes: actual,
                    max_input_bytes: actual,
                    max_seat_append_bytes: 64 * 1024,
                })
                .is_ok()
        );
        assert!(matches!(
            request.validate_bounds(UiBindingRequestBounds {
                max_request_bytes: actual - 1,
                max_input_bytes: actual,
                max_seat_append_bytes: 64 * 1024,
            }),
            Err(UiBindingRequestError::TooLarge { .. })
        ));
        wire["producer"]["scopes"] = serde_json::json!(["*"]);
        assert!(serde_json::from_value::<UiInvocationRequest>(wire).is_err());
    }

    #[test]
    fn attachment_descriptor_requires_complete_coordinate_and_bounds() {
        let valid = serde_json::json!({
            "binding_attachment_id": "attachment-one",
            "binding_generation": 1,
            "binding_digest": "digest-one",
            "root_ref": "surface:test",
            "root_generation": "generation-one",
            "presentation": {"definition": {"kind": "surface", "surface": {"name": "test"}}, "views": {}},
            "project_path": null,
            "subject": {"kind":"projectless"},
            "posture": "observation_only",
            "binding_request_bounds": {"max_request_bytes": 4096, "max_input_bytes": 1024,"max_seat_append_bytes":65536}
        });
        assert!(
            serde_json::from_value::<UiBindingAttachment>(valid.clone())
                .unwrap()
                .validate()
                .is_ok()
        );
        let mut missing_subject = valid.clone();
        missing_subject.as_object_mut().unwrap().remove("subject");
        assert!(serde_json::from_value::<UiBindingAttachment>(missing_subject).is_err());
        let mut project = valid.clone();
        project["project_path"] = serde_json::json!("/project");
        assert!(
            serde_json::from_value::<UiBindingAttachment>(project.clone())
                .unwrap()
                .validate()
                .is_err()
        );
        project["subject"] = serde_json::json!({"kind":"unregistered_project"});
        assert!(
            serde_json::from_value::<UiBindingAttachment>(project.clone())
                .unwrap()
                .validate()
                .is_ok()
        );
        project["subject"] =
            serde_json::json!({"kind":"registered_project","local_id":"project-one"});
        assert!(
            serde_json::from_value::<UiBindingAttachment>(project.clone())
                .unwrap()
                .validate()
                .is_ok()
        );
        project["subject"]["local_id"] = serde_json::json!(
            "x".repeat(crate::surface::view_sets::MAX_PROJECT_LOCAL_ID_BYTES + 1)
        );
        assert!(
            serde_json::from_value::<UiBindingAttachment>(project.clone())
                .unwrap()
                .validate()
                .is_err()
        );
        project["subject"]["local_id"] = serde_json::json!(" ");
        assert!(
            serde_json::from_value::<UiBindingAttachment>(project)
                .unwrap()
                .validate()
                .is_err()
        );
        for field in [
            "binding_attachment_id",
            "binding_digest",
            "root_ref",
            "root_generation",
        ] {
            let mut invalid = valid.clone();
            invalid[field] = Value::String(String::new());
            assert!(
                serde_json::from_value::<UiBindingAttachment>(invalid)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        for path in [
            "/binding_generation",
            "/binding_request_bounds/max_request_bytes",
            "/binding_request_bounds/max_input_bytes",
        ] {
            let mut invalid = valid.clone();
            *invalid.pointer_mut(path).unwrap() = Value::from(0);
            assert!(
                serde_json::from_value::<UiBindingAttachment>(invalid)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
    }

    #[test]
    fn root_kind_nested_views_and_predecessor_fields_are_refused() {
        let mut value = serde_json::json!({
            "binding_attachment_id": "attachment-one", "binding_generation": 1,
            "binding_digest": "digest-one", "root_ref": "config:test",
            "root_generation": "generation-one",
            "presentation": {"definition": {"kind": "surface", "surface": {"name": "test"}}, "views": {}},
            "project_path": null, "subject":{"kind":"projectless"}, "posture": "observation_only",
            "binding_request_bounds": {"max_request_bytes": 4096, "max_input_bytes": 1024,"max_seat_append_bytes":65536}
        });
        assert!(
            serde_json::from_value::<UiBindingAttachment>(value.clone())
                .unwrap()
                .validate()
                .is_err()
        );
        value["root_ref"] = serde_json::json!("surface:test");
        value["presentation"]["definition"]["surface"]["views"] = serde_json::json!({});
        assert!(
            serde_json::from_value::<UiBindingAttachment>(value.clone())
                .unwrap()
                .validate()
                .is_err()
        );
        value["surface_ref"] = serde_json::json!("surface:test");
        assert!(serde_json::from_value::<UiBindingAttachment>(value).is_err());
    }

    #[test]
    fn request_requires_the_complete_attachment_coordinate() {
        let request = UiBindingRequest {
            binding_attachment_id: "attachment-one".into(),
            binding_generation: 1,
            binding_digest: "fixture-digest".into(),
            coordinate: UiBindingCoordinate::Source {
                view_ref: "view:test/read".into(),
                channel: "default".into(),
            },
            payload: UiBindingPayload::SourceParameters {
                params: serde_json::json!({}),
            },
        };
        let bounds = UiBindingRequestBounds {
            max_request_bytes: 4096,
            max_input_bytes: 4096,
            max_seat_append_bytes: 64 * 1024,
        };
        assert!(request.validate_bounds(bounds).is_ok());
        for field in [
            "binding_attachment_id",
            "binding_generation",
            "binding_digest",
        ] {
            let mut encoded = serde_json::to_value(&request).unwrap();
            encoded.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<UiBindingRequest>(encoded).is_err());
        }
        for bad in [
            UiBindingRequest {
                binding_attachment_id: String::new(),
                ..request.clone()
            },
            UiBindingRequest {
                binding_generation: 0,
                ..request.clone()
            },
            UiBindingRequest {
                binding_digest: String::new(),
                ..request
            },
        ] {
            assert_eq!(
                bad.validate_bounds(bounds),
                Err(UiBindingRequestError::InvalidAttachment)
            );
        }
    }

    #[test]
    fn coordinate_cannot_smuggle_an_execution_target() {
        let encoded = serde_json::to_value(UiBindingCoordinate::Affordance {
            view_ref: "view:example/work".to_string(),
            affordance_id: "resume".to_string(),
        })
        .unwrap();
        assert!(encoded.get("item_ref").is_none());
        assert!(encoded.get("tokens").is_none());
        assert!(encoded.get("capabilities").is_none());
    }

    #[test]
    fn request_bounds_and_coordinate_payload_pair_are_closed() {
        let mismatch = UiBindingRequest {
            binding_attachment_id: "fixture-attachment".into(),
            binding_generation: 1,
            binding_digest: "sha256:fixture".to_string(),
            coordinate: UiBindingCoordinate::Source {
                view_ref: "view:example/work".to_string(),
                channel: "default".to_string(),
            },
            payload: UiBindingPayload::Input {
                value: "hello".to_string(),
                route: None,
            },
        };
        assert_eq!(
            mismatch.validate_bounds(UiBindingRequestBounds {
                max_request_bytes: 1024,
                max_input_bytes: 16,
                max_seat_append_bytes: 64 * 1024,
            }),
            Err(UiBindingRequestError::PayloadMismatch)
        );

        let too_large = UiBindingRequest {
            binding_attachment_id: "fixture-attachment".into(),
            binding_generation: 1,
            binding_digest: "sha256:fixture".to_string(),
            coordinate: UiBindingCoordinate::Affordance {
                view_ref: "view:example/work".to_string(),
                affordance_id: "submit".to_string(),
            },
            payload: UiBindingPayload::Input {
                value: "xxxxx".to_string(),
                route: None,
            },
        };
        assert!(matches!(
            too_large.validate_bounds(UiBindingRequestBounds {
                max_request_bytes: 1024,
                max_input_bytes: 4,
                max_seat_append_bytes: 64 * 1024,
            }),
            Err(UiBindingRequestError::InputTooLarge { .. })
        ));
    }
}
