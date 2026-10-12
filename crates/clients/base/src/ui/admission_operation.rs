//! Retained identities for idempotent presentation admission.
//!
//! Prepared and completed records belong to the existing seat braid. Transport
//! receipt sequence numbers are not local log positions; adapters acknowledge
//! the exact captured local prefix only after validating its retained receipt.

use serde::{Deserialize, Serialize};

use super::binding::{
    UiBindingAttachmentCoordinate, UiBindingCoordinate, UiBindingPayload, UiBindingRequest,
    UiBindingRequestBounds, UiPresentationProducer,
};

pub const PREPARED_EVENT: &str = "seat.presentation_operation.prepared";
pub const COMPLETED_EVENT: &str = "seat.presentation_operation.completed";

/// Conservative complete SINGLE-event seat append envelope. Producers and
/// subjects are exact; append UUID/digest lengths are protocol-fixed, while
/// quoted maximum-u64 sequence strings cover browser and native encoders.
/// This measures transport only, never authenticates an operation.
pub fn single_seat_append_fits(
    producer: &UiPresentationProducer,
    event_type: &str,
    payload: &impl Serialize,
    max_append_bytes: u64,
) -> bool {
    ryeos_ui_presentation_contract::append_bound::single_seat_append_fits(
        &producer.presentation_id,
        &producer.thread_id,
        &producer.producer_incarnation,
        event_type,
        payload,
        max_append_bytes,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiPresentationOperationKind {
    OpenSurface,
    ImportSurfaceTabs,
    OpenPresentationDocument,
    ResumeParticularViewSet,
    ResumeParticularSurface,
}

impl UiPresentationOperationKind {
    pub fn is_surface_opening(self) -> bool {
        matches!(
            self,
            Self::OpenSurface | Self::ImportSurfaceTabs | Self::OpenPresentationDocument
        )
    }
}

/// Authored declaration differs from the resolved retained operation purpose.
pub fn resolve_presentation_operation(
    declaration: &serde_json::Value,
    params: &serde_json::Value,
) -> Result<
    (
        UiPresentationOperationKind,
        Option<crate::surface::opening::SurfaceOpenTarget>,
    ),
    String,
> {
    if declaration.as_str() == Some("surface_opening") {
        let target = crate::surface::opening::SurfaceOpenTarget::from_resolved_params(params)?;
        return Ok((target.placement.purpose(), Some(target)));
    }
    let purpose: UiPresentationOperationKind =
        serde_json::from_value(declaration.clone()).map_err(|error| error.to_string())?;
    if purpose.is_surface_opening() {
        return Err("opening must use the signed SurfaceOpening declaration".into());
    }
    Ok((purpose, None))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiParticularAdmissionSelection {
    ViewSet {
        id: String,
        expected_revision: u64,
        surface_ref: String,
    },
    Surface {
        id: String,
        expected_revision: u64,
    },
}

impl UiParticularAdmissionSelection {
    pub fn validate(&self) -> Result<(), String> {
        let (id, surface_ref) = match self {
            Self::ViewSet {
                id, surface_ref, ..
            } => (id, Some(surface_ref)),
            Self::Surface { id, .. } => (id, None),
        };
        if id.is_empty()
            || id.len() > crate::surface::view_sets::MAX_SAVED_VIEW_SET_ID_BYTES
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err("particular admission identity is invalid".into());
        }
        if surface_ref.is_some_and(|surface_ref| {
            !surface_ref
                .strip_prefix("surface:")
                .is_some_and(|reference| {
                    !reference.is_empty()
                        && reference.trim() == reference
                        && !reference.chars().any(char::is_control)
                })
        }) {
            return Err("particular view-set Surface reference is invalid".into());
        }
        Ok(())
    }

    pub fn from_resolved_params(
        purpose: UiPresentationOperationKind,
        params: &serde_json::Value,
    ) -> Result<Option<Self>, String> {
        if purpose.is_surface_opening() {
            return Ok(None);
        }
        let object = params
            .as_object()
            .ok_or("particular admission parameters must be an object")?;
        let id = object
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or("particular admission identity is absent")?;
        let expected_revision = object
            .get("expected_revision")
            .and_then(serde_json::Value::as_u64)
            .ok_or("particular admission revision is absent")?;
        match purpose {
            UiPresentationOperationKind::ResumeParticularViewSet => {
                let surface_ref = object
                    .get("surface_ref")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("particular view-set Surface reference is absent")?;
                let selection = Self::ViewSet {
                    id: id.into(),
                    expected_revision,
                    surface_ref: surface_ref.into(),
                };
                selection.validate()?;
                Ok(Some(selection))
            }
            UiPresentationOperationKind::ResumeParticularSurface => {
                let selection = Self::Surface {
                    id: id.into(),
                    expected_revision,
                };
                selection.validate()?;
                Ok(Some(selection))
            }
            UiPresentationOperationKind::OpenSurface
            | UiPresentationOperationKind::OpenPresentationDocument
            | UiPresentationOperationKind::ImportSurfaceTabs => unreachable!(),
        }
    }
}

/// Original operation identity survives a producer replacement. The original
/// producer records provenance, while every dispatch authenticates the CURRENT
/// producer independently. Neither this record nor its mount key grants access.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiPreparedPresentationOperation {
    /// Required current-contract cut. Predecessor local-sequence identities
    /// are unsupported history, not candidates for retrospective certification.
    pub schema: u32,
    pub operation_id: String,
    pub producer: UiPresentationProducer,
    /// Original producer-event position, never a local or chain-log sequence.
    pub prepared_engine_seq: u64,
    pub root_binding: UiBindingAttachmentCoordinate,
    pub body: UiPreparedPresentationOperationBody,
    pub request_bounds: UiBindingRequestBounds,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiPreparedAdmissionOperation {
    pub purpose: UiPresentationOperationKind,
    #[serde(deserialize_with = "required_surface_open_target")]
    pub surface_open_target: Option<crate::surface::opening::SurfaceOpenTarget>,
    #[serde(deserialize_with = "required_particular_selection")]
    pub particular_selection: Option<UiParticularAdmissionSelection>,
    pub invocation_origin: crate::ids::RyeOsViewInstanceKey,
    pub request: UiBindingRequest,
    pub success_notice: Option<String>,
}

fn required_surface_open_target<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<crate::surface::opening::SurfaceOpenTarget>, D::Error> {
    serde::Deserialize::deserialize(deserializer)
}

fn required_particular_selection<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<UiParticularAdmissionSelection>, D::Error> {
    serde::Deserialize::deserialize(deserializer)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiPreparedPresentationOperationBody {
    Admission(UiPreparedAdmissionOperation),
    Composition {
        request: super::composition_operation::UiCompositionRequest,
    },
    MoveFreeze {
        request: super::transfer_freeze::UiMoveFreezeRequest,
    },
}

impl UiPreparedPresentationOperation {
    /// Derive from the complete typed preparation, normalizing ONLY its own
    /// derived IDs. The same Rust owner is used by construction and verification.
    pub fn derive_identity(&self) -> Result<String, &'static str> {
        if self.schema != 2 {
            return Err("unsupported presentation preparation schema");
        }
        let mut body = self.body.clone();
        match &mut body {
            UiPreparedPresentationOperationBody::MoveFreeze { request } => {
                request.operation_id.clear();
            }
            UiPreparedPresentationOperationBody::Composition { request } => {
                request.operation_id.clear();
            }
            UiPreparedPresentationOperationBody::Admission(admission) => {
                let UiBindingPayload::Selection { record } = &mut admission.request.payload else {
                    return Err("presentation admission requires a selection payload");
                };
                record
                    .as_object_mut()
                    .ok_or("presentation admission requires an object selection")?
                    .remove("creation_request_id");
            }
        }
        let mut normalized_body =
            serde_json::to_value(&body).map_err(|_| "typed preparation body encoding failed")?;
        if matches!(
            body,
            UiPreparedPresentationOperationBody::Composition { .. }
                | UiPreparedPresentationOperationBody::MoveFreeze { .. }
        ) {
            normalized_body
                .get_mut("request")
                .and_then(serde_json::Value::as_object_mut)
                .ok_or("typed composition request encoding failed")?
                .remove("operation_id");
        }
        ryeos_ui_presentation_contract::freeze_witness::preparation_identity(
            self.schema,
            &self.producer,
            self.prepared_engine_seq,
            &self.root_binding,
            &self.request_bounds,
            &normalized_body,
        )
    }

    /// Constructor-only installation of the derived self references. Recovery
    /// must validate the retained original instead of calling this method.
    pub fn assign_identity(&mut self) -> Result<(), &'static str> {
        let identity = self.derive_identity()?;
        match &mut self.body {
            UiPreparedPresentationOperationBody::MoveFreeze { request } => {
                request.operation_id = identity.clone();
            }
            UiPreparedPresentationOperationBody::Composition { request } => {
                request.operation_id = identity.clone();
            }
            UiPreparedPresentationOperationBody::Admission(admission) => {
                let UiBindingPayload::Selection { record } = &mut admission.request.payload else {
                    return Err("presentation admission requires a selection payload");
                };
                record
                    .as_object_mut()
                    .ok_or("presentation admission requires an object selection")?
                    .insert(
                        "creation_request_id".into(),
                        serde_json::Value::String(identity.clone()),
                    );
            }
        }
        self.operation_id = identity;
        Ok(())
    }

    pub fn validate_identity(&self) -> Result<(), &'static str> {
        if self.derive_identity()? != self.operation_id {
            return Err("presentation operation differs from its original preparation commitment");
        }
        Ok(())
    }

    pub fn admission(&self) -> Option<&UiPreparedAdmissionOperation> {
        match &self.body {
            UiPreparedPresentationOperationBody::Admission(value) => Some(value),
            _ => None,
        }
    }
    pub fn composition(&self) -> Option<&super::composition_operation::UiCompositionRequest> {
        match &self.body {
            UiPreparedPresentationOperationBody::Composition { request } => Some(request),
            _ => None,
        }
    }
    pub fn move_freeze(&self) -> Option<&super::transfer_freeze::UiMoveFreezeRequest> {
        match &self.body {
            UiPreparedPresentationOperationBody::MoveFreeze { request } => Some(request),
            _ => None,
        }
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.validate_identity().is_err()
            || !canonical_operation_id(&self.operation_id)
            || self.producer.presentation_id.is_empty()
            || self.producer.thread_id.is_empty()
            || self.producer.producer_incarnation.len() != 64
            || !self
                .producer
                .producer_incarnation
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.root_binding.binding_attachment_id.is_empty()
            || self.root_binding.binding_generation == 0
            || self.root_binding.binding_digest.is_empty()
        {
            return Err("presentation operation has an invalid retained identity");
        }
        if let Some(request) = self.move_freeze() {
            if request.operation_id != self.operation_id
                || request.transfer.source.presentation_id != self.producer.presentation_id
                || request.transfer.source.thread_id != self.producer.thread_id
                || request.transfer.source.producer_incarnation
                    != self.producer.producer_incarnation
                || request.root_binding != self.root_binding
            {
                return Err("MOVE freeze differs from its retained producer or root identity");
            }
            request
                .validate(self.request_bounds.max_request_bytes)
                .map_err(|_| "MOVE freeze exceeds its closed shape or admitted bounds")?;
            let value = serde_json::to_value(self).map_err(|_| "MOVE freeze encoding failed")?;
            ryeos_ui_presentation_contract::move_freeze_lifecycle::preparation_commitment(&value)
                .map_err(|_| "MOVE freeze differs from shared receipt commitment")?
                .ok_or("MOVE freeze shared receipt commitment absent")?;
            return Ok(());
        }
        if let Some(request) = self.composition() {
            if request.operation_id != self.operation_id
                || request.presentation_id.as_str() != self.producer.presentation_id
            {
                return Err("composition operation differs from its retained identity");
            }
            request
                .validate(self.request_bounds.max_request_bytes)
                .map_err(|_| "composition operation exceeds its closed shape or admitted bounds")?;
            return Ok(());
        }
        let admission = self
            .admission()
            .ok_or("presentation operation body is not supported")?;
        if admission.purpose.is_surface_opening() != admission.surface_open_target.is_some() {
            return Err("Surface opening selection differs from admission purpose");
        }
        if admission.purpose.is_surface_opening() != admission.particular_selection.is_none()
            || !matches!(
                (admission.purpose, &admission.particular_selection),
                (UiPresentationOperationKind::OpenSurface, None)
                    | (UiPresentationOperationKind::OpenPresentationDocument, None)
                    | (UiPresentationOperationKind::ImportSurfaceTabs, None)
                    | (
                        UiPresentationOperationKind::ResumeParticularViewSet,
                        Some(UiParticularAdmissionSelection::ViewSet { .. })
                    )
                    | (
                        UiPresentationOperationKind::ResumeParticularSurface,
                        Some(UiParticularAdmissionSelection::Surface { .. })
                    )
            )
        {
            return Err("particular selection differs from admission purpose");
        }
        if admission
            .particular_selection
            .as_ref()
            .is_some_and(|selection| selection.validate().is_err())
        {
            return Err("particular selection has invalid identity");
        }
        if let Some(selection) = &admission.surface_open_target {
            if selection.placement.purpose() != admission.purpose {
                return Err("opening placement and purpose differ");
            }
            if matches!(&selection.placement, crate::surface::opening::SurfaceOpenPlacement::NewPresentation {root_binding} if root_binding != &self.root_binding)
            {
                return Err("document opening changes prepared shared root");
            }
            crate::surface::opening::SurfaceOpenRequest {
                creation_request_id: self.operation_id.clone(),
                selection: selection.selection.clone(),
                placement: selection.placement.clone(),
            }
            .validate()
            .map_err(|_| "Surface opening selection is invalid")?;
        }
        if !matches!(
            admission.request.coordinate,
            UiBindingCoordinate::Affordance { .. }
        ) {
            return Err("presentation admission requires an exact affordance");
        }
        let UiBindingPayload::Selection { record } = &admission.request.payload else {
            return Err("presentation admission requires an exact selection payload");
        };
        if !record.is_object()
            || record
                .get("creation_request_id")
                .and_then(serde_json::Value::as_str)
                != Some(self.operation_id.as_str())
        {
            return Err("presentation operation payload differs from its creation identity");
        }
        admission
            .request
            .validate_bounds(self.request_bounds)
            .map_err(|_| "presentation operation request exceeds its admitted bounds")?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiCompletedPresentationOperation {
    pub operation_id: String,
    /// The authenticated admission response, retained before applying it to
    /// local composition. No emitted effect ids or authority grants are saved.
    pub result: serde_json::Value,
}

impl UiCompletedPresentationOperation {
    /// Apply the same validation to a live response and authoritative replay.
    /// Transport/replay provenance does not excuse a contradictory purpose.
    pub fn validate_for(
        &self,
        prepared: &UiPreparedPresentationOperation,
    ) -> Result<(), &'static str> {
        prepared.validate()?;
        if self.operation_id != prepared.operation_id {
            return Err("presentation completion differs from its operation identity");
        }
        if let Some(request) = prepared.move_freeze() {
            let receipt: super::transfer_freeze::UiMoveFreezeCompletion =
                serde_json::from_value(self.result.clone())
                    .map_err(|_| "MOVE freeze completion has an invalid closed receipt")?;
            receipt.validate_for(request)?;
        } else if let Some(request) = prepared.composition() {
            let receipt: super::composition_completion::UiCompositionCompletion =
                serde_json::from_value(self.result.clone())
                    .map_err(|_| "composition completion has an invalid closed receipt")?;
            receipt
                .validate_for(request)
                .map_err(|_| "composition receipt contradicts its exact preparation")?;
        } else {
            let admission = prepared
                .admission()
                .ok_or("unknown presentation operation body")?;
            let kind = match admission.purpose {
                UiPresentationOperationKind::OpenSurface => "open_surface",
                UiPresentationOperationKind::OpenPresentationDocument => {
                    "open_presentation_document"
                }
                UiPresentationOperationKind::ImportSurfaceTabs => "import_surface_tabs",
                UiPresentationOperationKind::ResumeParticularViewSet => {
                    "resume_particular_view_set"
                }
                UiPresentationOperationKind::ResumeParticularSurface => "resume_particular_surface",
            };
            if self.operation_id != prepared.operation_id
                || self
                    .result
                    .pointer("/ui_transition/kind")
                    .and_then(serde_json::Value::as_str)
                    != Some(kind)
                || self
                    .result
                    .pointer("/ui_transition/presentation_id")
                    .and_then(serde_json::Value::as_str)
                    != Some(prepared.producer.presentation_id.as_str())
            {
                return Err("presentation completion contradicts its prepared identity or purpose");
            }
            let transition = self
                .result
                .get("ui_transition")
                .ok_or("presentation admission transition is absent")?;
            if transition
                .get("creation_request_id")
                .and_then(serde_json::Value::as_str)
                != Some(prepared.operation_id.as_str())
            {
                return Err("presentation completion differs from its creation identity");
            }
            let particular_selection = match self.result.get("particular_selection") {
                Some(value) => Some(
                    serde_json::from_value::<UiParticularAdmissionSelection>(value.clone())
                        .map_err(|_| "particular completion selection is malformed")?,
                ),
                None => None,
            };
            if particular_selection.as_ref() != admission.particular_selection.as_ref() {
                return Err("particular completion differs from its prepared selection");
            }
            if admission.purpose == UiPresentationOperationKind::OpenPresentationDocument {
                let receipt: super::binding::UiPreparedPresentationDocumentTransition =
                    serde_json::from_value(transition.clone())
                        .map_err(|_| "document completion has invalid closed shape")?;
                receipt
                    .validate()
                    .map_err(|_| "document completion has invalid initialization")?;
                if receipt.opening.creation_request_id != prepared.operation_id
                    || admission.surface_open_target.as_ref() != Some(&receipt.opening.target())
                    || receipt.prepared.root_binding != prepared.root_binding
                {
                    return Err("document completion changes exact prepared selection or root");
                }
            } else {
                let is_import = admission.purpose == UiPresentationOperationKind::ImportSurfaceTabs;
                if is_import {
                    serde_json::from_value::<super::binding::UiSurfaceTabsAdmissionTransition>(
                        transition.clone(),
                    )
                    .map_err(|_| "tab import completion has invalid closed shape")?
                    .validate()
                    .map_err(|_| "tab import completion has invalid closure")?;
                }
                let saved_surface = if is_import {
                    transition.get("composition")
                } else {
                    transition.pointer("/surface/saved_surface")
                };
                let saved_surface = match saved_surface {
                    Some(value) if !value.is_null() => {
                        let saved: super::binding::UiSavedSurfaceComposition =
                            serde_json::from_value(value.clone())
                                .map_err(|_| "saved Surface completion is malformed")?;
                        saved
                            .validate()
                            .map_err(|_| "saved Surface completion has an invalid closed shape")?;
                        Some(saved)
                    }
                    _ => None,
                };
                if let Some(target) = &admission.surface_open_target {
                    let opening: crate::surface::opening::SurfaceOpenRequest =
                        serde_json::from_value(
                            transition
                                .get("opening")
                                .cloned()
                                .ok_or("opening receipt request absent")?,
                        )
                        .map_err(|_| "opening receipt request malformed")?;
                    if opening.creation_request_id != prepared.operation_id
                        || &opening.target() != target
                    {
                        return Err("opening receipt changes prepared selection or placement");
                    }
                    let attachment_path = if is_import {
                        "/origin_attachment/root_ref"
                    } else {
                        "/attachment/root_ref"
                    };
                    match &target.selection {
                        crate::surface::opening::SurfaceOpenSelection::Authored { surface_ref } => {
                            if transition
                                .pointer(attachment_path)
                                .and_then(serde_json::Value::as_str)
                                != Some(surface_ref.as_str())
                                || saved_surface.is_some()
                            {
                                return Err(
                                    "authored Surface completion differs from prepared selection",
                                );
                            }
                        }
                        crate::surface::opening::SurfaceOpenSelection::Reusable {
                            id,
                            expected_revision,
                            fresh_subjects,
                        } => {
                            let saved = saved_surface
                                .as_ref()
                                .ok_or("reusable Surface completion absent")?;
                            let tabs = saved
                                .reusable_tabs()
                                .ok_or("reusable opening returned a particular receipt")?;
                            if saved
                                .library_identity()
                                .is_none_or(|(saved_id, revision, _)| {
                                    saved_id != id || revision != *expected_revision
                                })
                            {
                                return Err(
                                    "reusable completion differs from selected record revision",
                                );
                            }
                            let actual = tabs
                                .iter()
                                .map(|tab| (tab.saved_set_id.as_str(), &tab.fresh_subjects))
                                .collect::<std::collections::BTreeMap<_, _>>();
                            if actual.len() != tabs.len()
                                || fresh_subjects.iter().any(|(tab, values)| {
                                    actual
                                        .get(tab.as_str())
                                        .is_none_or(|actual| *actual != values)
                                })
                                || actual.iter().any(|(tab, values)| {
                                    !fresh_subjects.contains_key(*tab) && !values.is_empty()
                                })
                            {
                                return Err("reusable completion changes prepared fresh subjects");
                            }
                        }
                        crate::surface::opening::SurfaceOpenSelection::Composed(composition) => {
                            let saved = saved_surface
                                .as_ref()
                                .ok_or("composed opening has no admitted composition")?;
                            if !matches!(&saved.provenance,super::binding::UiSurfaceCompositionProvenance::Composed{composition:actual} if actual==composition)
                                || transition
                                    .pointer(attachment_path)
                                    .and_then(serde_json::Value::as_str)
                                    != Some(composition.origin.surface_ref.as_str())
                            {
                                return Err(
                                    "composed opening changes original origin or portable request",
                                );
                            }
                        }
                    }
                } else {
                    match admission.purpose {
                        UiPresentationOperationKind::ResumeParticularSurface => {
                            let Some(UiParticularAdmissionSelection::Surface {
                                id,
                                expected_revision,
                            }) = &admission.particular_selection
                            else {
                                return Err("particular Surface selection is absent");
                            };
                            if saved_surface.as_ref().is_none_or(|saved| {
                                saved.particular_tabs().is_none()
                                    || saved.library_identity().is_none_or(
                                        |(saved_id, revision, _)| {
                                            saved_id != id || revision != *expected_revision
                                        },
                                    )
                            }) {
                                return Err(
                                    "particular Surface completion differs from its selected record",
                                );
                            }
                        }
                        UiPresentationOperationKind::ResumeParticularViewSet => {
                            if saved_surface.is_some() {
                                return Err(
                                    "particular view-set completion carries a saved Surface descriptor",
                                );
                            }
                            let Some(UiParticularAdmissionSelection::ViewSet {
                                id,
                                surface_ref,
                                ..
                            }) = &admission.particular_selection
                            else {
                                return Err("particular view-set selection is absent");
                            };
                            let resolved: crate::surface::view_sets::ResolvedParticularViewSet =
                                serde_json::from_value(
                                    self.result
                                        .get("particular_view_set")
                                        .cloned()
                                        .ok_or("particular view-set completion is absent")?,
                                )
                                .map_err(|_| "particular view-set completion is malformed")?;
                            resolved.validate().map_err(
                                |_| "particular view-set completion has invalid presentation",
                            )?;
                            if resolved.id != *id
                                || transition
                                    .pointer("/attachment/root_ref")
                                    .and_then(serde_json::Value::as_str)
                                    != Some(surface_ref.as_str())
                            {
                                return Err(
                                    "particular view-set completion differs from its selected record",
                                );
                            }
                        }
                        UiPresentationOperationKind::OpenSurface
                        | UiPresentationOperationKind::OpenPresentationDocument
                        | UiPresentationOperationKind::ImportSurfaceTabs => {
                            return Err("Surface opening completion has no prepared selection");
                        }
                    }
                }
            }
        }
        if serde_json::to_vec(self)
            .ok()
            .is_none_or(|bytes| bytes.len() as u64 > prepared.request_bounds.max_request_bytes)
        {
            return Err("presentation completion exceeds its retained bound");
        }
        if !single_seat_append_fits(
            &prepared.producer,
            COMPLETED_EVENT,
            self,
            prepared.request_bounds.max_seat_append_bytes,
        ) {
            return Err("presentation completion exceeds its retained seat append bound");
        }
        Ok(())
    }
}

pub use ryeos_ui_presentation_contract::identity::UiSeatAppendIdentity;
pub use ryeos_ui_presentation_contract::identity::canonical_operation_id;

#[derive(Debug, Clone, Default)]
pub(crate) struct PresentationOperationState {
    pub producer: Option<UiPresentationProducer>,
    pub acknowledged_prefix_len: usize,
    /// Authenticated next producer-event sequence at the exact acknowledged
    /// local prefix, not SeatLog::next_sequence() or a chain position.
    pub acknowledged_engine_frontier: Option<u64>,
    pub prepared: std::collections::BTreeMap<String, u64>,
    pub completed: std::collections::BTreeMap<String, u64>,
    pub invalid: std::collections::BTreeSet<String>,
    pub replay_conflicts: std::collections::BTreeSet<String>,
    pub dirty: std::collections::BTreeSet<String>,
    pub dispatched: std::collections::BTreeSet<String>,
    pub applied: std::collections::BTreeSet<String>,
    pub refused: std::collections::BTreeSet<String>,
    pub closed_surfaces: std::collections::BTreeMap<String, super::seat::UiSurfaceClosure>,
    pub dirty_closures: std::collections::BTreeSet<String>,
    pub replay_blocked: bool,
}

impl PresentationOperationState {
    pub fn reserved_engine_seq(&self, local_index: usize) -> Option<u64> {
        let offset = u64::try_from(local_index.checked_sub(self.acknowledged_prefix_len)?).ok()?;
        let reserved = self.acknowledged_engine_frontier?.checked_add(offset)?;
        reserved.checked_add(1)?;
        Some(reserved)
    }

    pub fn fold_acknowledged(&mut self, event: &super::seat::SeatEvent) {
        match &event.kind {
            super::seat::SeatEventKind::PresentationOperationPrepared(operation) => {
                self.dirty.insert(operation.operation_id.clone());
                if self
                    .prepared
                    .insert(operation.operation_id.clone(), event.seq)
                    .is_some()
                {
                    self.invalid.insert(operation.operation_id.clone());
                }
            }
            super::seat::SeatEventKind::PresentationOperationCompleted(completion) => {
                self.dirty.insert(completion.operation_id.clone());
                if !self.prepared.contains_key(&completion.operation_id)
                    || self
                        .completed
                        .insert(completion.operation_id.clone(), event.seq)
                        .is_some()
                {
                    self.invalid.insert(completion.operation_id.clone());
                }
            }
            super::seat::SeatEventKind::SurfaceClosed(closure) => {
                if closure.validate().is_err() {
                    return;
                }
                self.dirty_closures
                    .insert(closure.surface_instance_id.clone());
                self.closed_surfaces
                    .insert(closure.surface_instance_id.clone(), closure.clone());
            }
            super::seat::SeatEventKind::Facet { .. }
            | super::seat::SeatEventKind::PresentationInitialized(_) => {}
        }
    }
    pub fn rebuild_index(&mut self, events: &[super::seat::SeatEvent]) {
        self.prepared.clear();
        self.completed.clear();
        self.invalid.clear();
        self.invalid.extend(self.replay_conflicts.iter().cloned());
        self.dirty.clear();
        self.dirty_closures.clear();
        self.closed_surfaces.clear();
        for event in events {
            self.fold_acknowledged(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn particular_selection_is_typed_bounded_and_purpose_exact() {
        let params = serde_json::json!({
            "id":"current","expected_revision":7,"surface_ref":"surface:test/work"
        });
        assert_eq!(
            UiParticularAdmissionSelection::from_resolved_params(
                UiPresentationOperationKind::ResumeParticularViewSet,
                &params,
            )
            .unwrap(),
            Some(UiParticularAdmissionSelection::ViewSet {
                id: "current".into(),
                expected_revision: 7,
                surface_ref: "surface:test/work".into(),
            })
        );
        assert_eq!(
            UiParticularAdmissionSelection::from_resolved_params(
                UiPresentationOperationKind::ResumeParticularSurface,
                &params,
            )
            .unwrap(),
            Some(UiParticularAdmissionSelection::Surface {
                id: "current".into(),
                expected_revision: 7,
            })
        );
        let mut invalid = params;
        invalid["surface_ref"] = serde_json::json!("view:test/work");
        assert!(
            UiParticularAdmissionSelection::from_resolved_params(
                UiPresentationOperationKind::ResumeParticularViewSet,
                &invalid,
            )
            .is_err()
        );
    }

    #[test]
    fn particular_completion_echoes_exact_selection_and_resolved_subject() {
        use serde_json::json;
        let mut prepared: UiPreparedPresentationOperation = serde_json::from_value(json!({
            "schema":2,"prepared_engine_seq":0,
            "operation_id":"11111111-1111-4111-8111-111111111111",
            "producer":{"presentation_id":"22222222-2222-4222-8222-222222222222","thread_id":"seat","producer_incarnation":"ab".repeat(32)},
            "root_binding":{"binding_attachment_id":"root","binding_generation":1,"binding_digest":"cd".repeat(32)},
            "request_bounds":{"max_request_bytes":65536,"max_input_bytes":4096,"max_seat_append_bytes":131072},
            "body":{"kind":"admission","purpose":"resume_particular_view_set","surface_open_target":null,
                "particular_selection":{"kind":"view_set","id":"current","expected_revision":7,"surface_ref":"surface:test/work"},
                "invocation_origin":"seat:22222222-2222-4222-8222-222222222222:dock:left",
                "request":{"binding_attachment_id":"root","binding_generation":1,"binding_digest":"cd".repeat(32),"coordinate":{"kind":"affordance","view_ref":"view:test/open","affordance_id":"open"},"payload":{"kind":"selection","record":{"creation_request_id":"11111111-1111-4111-8111-111111111111"}}},"success_notice":null}
        })).unwrap();
        prepared.assign_identity().unwrap();
        let selection = json!({"kind":"view_set","id":"current","expected_revision":7,"surface_ref":"surface:test/work"});
        let resolved = json!({"id":"current","name":"Current","composition":{"schema":2,"id":"current","title":"Current","root":null},
            "relationships":[],"resolved_selection_work":{"thread":"T-head","chain_root":"T-root"}});
        let mut completion = UiCompletedPresentationOperation {
            operation_id: prepared.operation_id.clone(),
            result: json!({"particular_selection":selection,"particular_view_set":resolved,"ui_transition":{
                "kind":"resume_particular_view_set","presentation_id":prepared.producer.presentation_id,
                "creation_request_id":prepared.operation_id,"attachment":{"root_ref":"surface:test/work"}}}),
        };
        completion.validate_for(&prepared).unwrap();
        let valid = completion.result.clone();
        for (pointer, value) in [
            ("/particular_selection/id", json!("other")),
            ("/particular_selection/expected_revision", json!(8)),
            ("/particular_view_set/id", json!("other")),
            (
                "/ui_transition/attachment/root_ref",
                json!("surface:test/other"),
            ),
            (
                "/particular_view_set/resolved_selection_work/thread",
                json!("bad/work"),
            ),
        ] {
            completion.result = valid.clone();
            *completion.result.pointer_mut(pointer).unwrap() = value;
            assert!(completion.validate_for(&prepared).is_err(), "{pointer}");
        }
        completion.result = valid;
        completion
            .result
            .as_object_mut()
            .unwrap()
            .remove("particular_selection");
        assert!(completion.validate_for(&prepared).is_err());
    }

    #[test]
    fn particular_surface_completion_matches_selected_record() {
        use serde_json::json;
        let mut prepared: UiPreparedPresentationOperation = serde_json::from_value(json!({
            "schema":2,"prepared_engine_seq":0,
            "operation_id":"11111111-1111-4111-8111-111111111111",
            "producer":{"presentation_id":"22222222-2222-4222-8222-222222222222","thread_id":"seat","producer_incarnation":"ab".repeat(32)},
            "root_binding":{"binding_attachment_id":"root","binding_generation":1,"binding_digest":"cd".repeat(32)},
            "request_bounds":{"max_request_bytes":65536,"max_input_bytes":4096,"max_seat_append_bytes":131072},
            "body":{"kind":"admission","purpose":"resume_particular_surface","surface_open_target":null,
                "particular_selection":{"kind":"surface","id":"workspace","expected_revision":4},
                "invocation_origin":"seat:22222222-2222-4222-8222-222222222222:dock:left",
                "request":{"binding_attachment_id":"root","binding_generation":1,"binding_digest":"cd".repeat(32),"coordinate":{"kind":"affordance","view_ref":"view:test/open","affordance_id":"open"},"payload":{"kind":"selection","record":{"creation_request_id":"11111111-1111-4111-8111-111111111111"}}},"success_notice":null}
        })).unwrap();
        prepared.assign_identity().unwrap();
        let mut completion = UiCompletedPresentationOperation {
            operation_id: prepared.operation_id.clone(),
            result: json!({"particular_selection":{"kind":"surface","id":"workspace","expected_revision":4},
                "ui_transition":{"kind":"resume_particular_surface","presentation_id":prepared.producer.presentation_id,
                    "creation_request_id":prepared.operation_id,"surface":{"saved_surface":{
                        "provenance":{"kind":"library","saved_id":"workspace","library_revision":4,"record_digest":"ef".repeat(32)},"active_view_set_id":"one",
                        "body":{"kind":"particular","tabs":[{"saved_set_id":"one","binding":{"binding_attachment_id":"tab","binding_generation":2,"binding_digest":"de".repeat(32)},
                            "resolved":{"id":"one","name":"One","composition":{"schema":2,"id":"one","title":"One","root":null},"relationships":[]}}]}}}}}),
        };
        completion.validate_for(&prepared).unwrap();
        completion.result["ui_transition"]["surface"]["saved_surface"]["provenance"]["library_revision"] =
            json!(5);
        assert!(completion.validate_for(&prepared).is_err());
    }

    #[test]
    fn reusable_completion_preserves_exact_prepared_selection() {
        use serde_json::json;
        let mut prepared: UiPreparedPresentationOperation = serde_json::from_value(json!({
            "schema":2,"prepared_engine_seq":0,
            "operation_id":"11111111-1111-4111-8111-111111111111",
            "producer":{"presentation_id":"22222222-2222-4222-8222-222222222222","thread_id":"seat","producer_incarnation":"ab".repeat(32)},
            "root_binding":{"binding_attachment_id":"root","binding_generation":1,"binding_digest":"cd".repeat(32)},
            "request_bounds":{"max_request_bytes":65536,"max_input_bytes":4096,"max_seat_append_bytes":131072},
            "body":{"kind":"admission","purpose":"open_surface","surface_open_target":{"selection":{"kind":"reusable","id":"solver","expected_revision":3,"fresh_subjects":{"run":{"subject":{"selection.work":"run-one"}}}},"placement":{"kind":"new_surface"}},"particular_selection":null,
                "invocation_origin":"seat:22222222-2222-4222-8222-222222222222:dock:left",
                "request":{"binding_attachment_id":"root","binding_generation":1,"binding_digest":"cd".repeat(32),"coordinate":{"kind":"affordance","view_ref":"view:test/open","affordance_id":"open"},"payload":{"kind":"selection","record":{"creation_request_id":"11111111-1111-4111-8111-111111111111"}}},"success_notice":null}
        })).unwrap();
        prepared.assign_identity().unwrap();
        let mut completion = UiCompletedPresentationOperation {
            operation_id: prepared.operation_id.clone(),
            result: json!({"ui_transition":{
                "kind":"open_surface","presentation_id":prepared.producer.presentation_id,
                "creation_request_id":prepared.operation_id,
                "surface":{"saved_surface":{"provenance":{"kind":"library","saved_id":"solver","library_revision":3,"record_digest":"ef".repeat(32)},"active_view_set_id":"run","body":{"kind":"reusable","tabs":[
                    {"saved_set_id":"run","binding":{"binding_attachment_id":"tab","binding_generation":2,"binding_digest":"de".repeat(32)},"origin":{"surface_ref":"surface:test/run"},"template":{"id":"run","name":"Run","composition":{"schema":2,"id":"run","title":"Run","root":{"type":"group","views":["view:test/work"],"active":0}},"relationships":[{"mount":{"kind":"tile","index":0},"source":{"mode":"required_subject","input":"subject","facets":["selection.work"]}}]},"fresh_subjects":{"subject":{"selection.work":"run-one"}}}
                ]}}}
            }}),
        };
        let target = prepared
            .admission()
            .unwrap()
            .surface_open_target
            .as_ref()
            .unwrap();
        completion.result["ui_transition"]["opening"] = json!({"creation_request_id":prepared.operation_id,"selection":target.selection,"placement":target.placement});
        completion.validate_for(&prepared).unwrap();
        let original = completion.result.clone();
        for (key, value) in [("saved_id", json!("other")), ("library_revision", json!(4))] {
            completion.result = original.clone();
            completion.result["ui_transition"]["surface"]["saved_surface"]["provenance"][key] =
                value;
            assert!(completion.validate_for(&prepared).is_err());
        }
        completion.result = original.clone();
        completion.result["ui_transition"]["surface"]["saved_surface"]["body"]["tabs"][0]["fresh_subjects"]
            ["subject"]["selection.work"] = json!("different");
        assert!(completion.validate_for(&prepared).is_err());
        completion.result = original.clone();
        completion.result["ui_transition"]["creation_request_id"] =
            json!("33333333-3333-4333-8333-333333333333");
        assert!(completion.validate_for(&prepared).is_err());
        completion.result = original.clone();
        completion.result["ui_transition"]["surface"]["saved_surface"]["active_view_set_id"] =
            json!("missing");
        assert!(completion.validate_for(&prepared).is_err());
        let mut predecessor = serde_json::to_value(&prepared).unwrap();
        predecessor["body"]
            .as_object_mut()
            .unwrap()
            .remove("surface_open_target");
        assert!(serde_json::from_value::<UiPreparedPresentationOperation>(predecessor).is_err());
        let mut predecessor = serde_json::to_value(&prepared).unwrap();
        predecessor["body"]
            .as_object_mut()
            .unwrap()
            .remove("particular_selection");
        assert!(serde_json::from_value::<UiPreparedPresentationOperation>(predecessor).is_err());
    }

    #[test]
    fn append_preflight_charges_wrappers_and_escaped_payload_bytes() {
        let producer = UiPresentationProducer {
            presentation_id: "12345678-1234-8123-8123-123456789abc".into(),
            thread_id: "T-retained-seat".into(),
            producer_incarnation: "ab".repeat(32),
        };
        let payload = serde_json::json!({"escaped":"\"\\\n".repeat(100)});
        let payload_bytes = serde_json::to_vec(&payload).unwrap().len() as u64;
        assert!(!single_seat_append_fits(
            &producer,
            PREPARED_EVENT,
            &payload,
            payload_bytes
        ));
        assert!(single_seat_append_fits(
            &producer,
            PREPARED_EVENT,
            &payload,
            65536
        ));
        assert!(!single_seat_append_fits(
            &producer,
            PREPARED_EVENT,
            &payload,
            0
        ));
    }

    #[test]
    fn operation_id_is_strict_and_not_a_reconnect_coordinate() {
        assert!(canonical_operation_id(
            "12345678-1234-8123-8123-123456789abc"
        ));
        assert!(!canonical_operation_id(
            "12345678-1234-8123-8123-123456789ABC"
        ));
        assert!(!canonical_operation_id(
            "12345678_1234-8123-8123-123456789abc"
        ));
    }

    fn identity_fixture() -> UiPreparedPresentationOperation {
        let mut operation: UiPreparedPresentationOperation = serde_json::from_value(serde_json::json!({
            "schema":2,"operation_id":"","prepared_engine_seq":8192,
            "producer":{"presentation_id":"22222222-2222-4222-8222-222222222222","thread_id":"seat","producer_incarnation":"ab".repeat(32)},
            "root_binding":{"binding_attachment_id":"root","binding_generation":1,"binding_digest":"cd".repeat(32)},
            "request_bounds":{"max_request_bytes":65536,"max_input_bytes":4096,"max_seat_append_bytes":131072},
            "body":{"kind":"admission","purpose":"resume_particular_surface","surface_open_target":null,
                "particular_selection":{"kind":"surface","id":"workspace","expected_revision":4},
                "invocation_origin":"seat:22222222-2222-4222-8222-222222222222:dock:left",
                "request":{"binding_attachment_id":"root","binding_generation":1,"binding_digest":"cd".repeat(32),"coordinate":{"kind":"affordance","view_ref":"view:test/open","affordance_id":"open"},"payload":{"kind":"selection","record":{"creation_request_id":"untrusted-self-id","selected":"work"}}},"success_notice":null}
        })).unwrap();
        operation.assign_identity().unwrap();
        operation.validate().unwrap();
        operation
    }

    #[test]
    fn preparation_commitment_normalizes_only_derived_self_ids() {
        let operation = identity_fixture();
        let identity = operation.operation_id.clone();
        assert_eq!(&identity[14..15], "8");
        assert!("89ab".contains(&identity[19..20]));
        let mut self_ids = operation.clone();
        self_ids.operation_id = "ignored outer self id".into();
        let UiPreparedPresentationOperationBody::Admission(admission) = &mut self_ids.body else {
            unreachable!()
        };
        let UiBindingPayload::Selection { record } = &mut admission.request.payload else {
            unreachable!()
        };
        record["creation_request_id"] = serde_json::json!({"arbitrary":"pre-existing self id"});
        assert_eq!(self_ids.derive_identity().unwrap(), identity);
        self_ids.assign_identity().unwrap();
        assert_eq!(self_ids, operation);

        let value = serde_json::to_value(&operation).unwrap();
        for (pointer, changed) in [
            ("/prepared_engine_seq", serde_json::json!(8193)),
            (
                "/producer/producer_incarnation",
                serde_json::json!("ef".repeat(32)),
            ),
            ("/root_binding/binding_generation", serde_json::json!(2)),
            (
                "/request_bounds/max_request_bytes",
                serde_json::json!(65535),
            ),
            (
                "/body/success_notice",
                serde_json::json!("different notice"),
            ),
            (
                "/body/particular_selection/expected_revision",
                serde_json::json!(5),
            ),
            (
                "/body/request/payload/record/selected",
                serde_json::json!("other work"),
            ),
        ] {
            let mut changed_value = value.clone();
            *changed_value.pointer_mut(pointer).unwrap() = changed;
            let changed: UiPreparedPresentationOperation =
                serde_json::from_value(changed_value).unwrap();
            assert_ne!(changed.derive_identity().unwrap(), identity, "{pointer}");
            assert!(
                changed.validate_identity().is_err(),
                "old UUID cannot certify {pointer}"
            );
        }
        for field in ["schema", "prepared_engine_seq"] {
            let mut predecessor = value.clone();
            predecessor.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<UiPreparedPresentationOperation>(predecessor).is_err()
            );
        }
    }

    #[test]
    fn composition_identity_normalizes_its_own_id_but_commits_request_and_position() {
        use super::super::composition_operation::{
            CompositionMutation, CompositionOwner, UiCompositionRequest,
        };
        let mut operation = identity_fixture();
        operation.body = UiPreparedPresentationOperationBody::Composition {
            request: UiCompositionRequest {
                schema: 1,
                presentation_id: crate::ids::PresentationId::try_new(
                    operation.producer.presentation_id.clone(),
                )
                .unwrap(),
                operation_id: "old self id".into(),
                expected_revision: 7,
                expected_next_mount_id: 20,
                owner: CompositionOwner::Surface {
                    surface_instance_id: crate::ids::SurfaceInstanceId::try_new(
                        "33333333-3333-4333-8333-333333333333",
                    )
                    .unwrap(),
                    binding: operation.root_binding.clone(),
                },
                mutation: CompositionMutation::CloseTab {
                    tab_id: crate::ids::ViewSetId::new(1),
                },
            },
        };
        operation.assign_identity().unwrap();
        operation.validate().unwrap();
        let identity = operation.operation_id.clone();
        let UiPreparedPresentationOperationBody::Composition { request } = &mut operation.body
        else {
            unreachable!()
        };
        request.operation_id = "different self id".into();
        assert_eq!(operation.derive_identity().unwrap(), identity);
        assert!(operation.validate().is_err());
        operation.assign_identity().unwrap();
        let UiPreparedPresentationOperationBody::Composition { request } = &mut operation.body
        else {
            unreachable!()
        };
        request.expected_revision += 1;
        assert_ne!(operation.derive_identity().unwrap(), identity);
    }

    #[test]
    fn retained_preparation_checks_original_wrapper_position_without_rederiving_for_writer() {
        let operation = identity_fixture();
        let mut retained = serde_json::json!({
            "chain_seq":9001,"event_type":PREPARED_EVENT,
            "payload":{"seq":operation.prepared_engine_seq,"payload":operation}
        });
        let decoded = super::super::seat::retained_seat_event(&retained)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.seq, 9001);
        assert_eq!(decoded.producer_engine_seq, Some(8192));
        assert_eq!(
            decoded.kind,
            super::super::seat::SeatEventKind::PresentationOperationPrepared(operation)
        );
        retained["payload"]["seq"] = serde_json::json!(9001);
        assert!(super::super::seat::retained_seat_event(&retained).is_err());
    }

    #[test]
    fn append_commitment_binds_exact_seat_original_interval_and_payload() {
        let batch = UiSeatAppendIdentity {
            presentation_id: "22222222-2222-4222-8222-222222222222".into(),
            chain_root_id: "root".into(),
            thread_id: "seat".into(),
            producer_incarnation: "ab".repeat(32),
            first_engine_seq: 30,
            last_engine_seq: 31,
            event_count: 2,
            payload_digest: "cd".repeat(32),
        };
        let identity = batch.derive_identity().unwrap();
        assert_eq!(batch.clone().derive_identity().unwrap(), identity);
        let mut changed = batch.clone();
        changed.payload_digest = "ef".repeat(32);
        assert_ne!(changed.derive_identity().unwrap(), identity);
        changed = batch.clone();
        changed.chain_root_id = "other-root".into();
        assert_ne!(changed.derive_identity().unwrap(), identity);
        changed = batch.clone();
        changed.producer_incarnation = "ef".repeat(32);
        assert_ne!(changed.derive_identity().unwrap(), identity);
        changed = batch.clone();
        changed.first_engine_seq += 1;
        changed.last_engine_seq += 1;
        assert_ne!(changed.derive_identity().unwrap(), identity);
        changed.event_count = 3;
        assert!(changed.derive_identity().is_err());
        changed.first_engine_seq = u64::MAX;
        changed.last_engine_seq = u64::MAX;
        changed.event_count = 1;
        assert!(changed.derive_identity().is_err());
    }

    #[test]
    fn preparation_reservation_survives_partial_ack_without_rebase_and_refuses_exhaustion() {
        let mut state = PresentationOperationState {
            acknowledged_prefix_len: 73,
            acknowledged_engine_frontier: Some(4096),
            ..Default::default()
        };
        assert_eq!(state.reserved_engine_seq(75), Some(4098));
        state.acknowledged_prefix_len += 1;
        state.acknowledged_engine_frontier = Some(4097);
        assert_eq!(state.reserved_engine_seq(75), Some(4098));
        assert_eq!(state.reserved_engine_seq(73), None);
        state.acknowledged_engine_frontier = Some(u64::MAX - 1);
        assert_eq!(state.reserved_engine_seq(74), Some(u64::MAX - 1));
        assert_eq!(state.reserved_engine_seq(75), None);
        state.acknowledged_engine_frontier = None;
        assert_eq!(state.reserved_engine_seq(74), None);
    }
}
