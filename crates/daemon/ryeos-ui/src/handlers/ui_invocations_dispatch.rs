//! `ui/invocations/dispatch` — browser-session invocation transport.
//!
//! The browser names only an entry in the exact binding compiled when its
//! session was minted. Executable refs, command tokens and capability claims
//! remain server-side signed data. Renderer-local navigation, focus and
//! overlay changes reduce in the shared client core and never cross this
//! execution transport.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::service_registry::UiDispatchMode;
use ryeos_app::state::AppState;
use ryeos_client_base::ui::{
    UiBindingCoordinate, UiBindingPayload, UiBindingRequest, UiInvocationRequest,
};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_executor::executor::ServiceAvailability;

use crate::browser_session::{
    AdmittedBindingAttachment, BindingAttachmentCoordinate, BrowserSession,
};
use crate::compiled_binding::{
    CompiledUiAffordance, CompiledUiProducer, CompiledUiResultEffect, CompiledUiSource,
    CompiledUiTarget,
};
use crate::state::get_ui_state;
use crate::{seat_auth::SeatCaller, thread_authorization::authorize_exact_thread_subjects};

struct BoundInvocation {
    target: CompiledUiTarget,
    params: Value,
    /// Whether the bound entry is a source a project-authored item declared.
    /// Read from the retained compiled binding, never from the request.
    project_authored_source: bool,
}

pub(crate) fn validate_prepared_operation_binding(
    state: &AppState,
    scopes: &[String],
    attachment: &AdmittedBindingAttachment,
    operation: &ryeos_client_base::ui::admission_operation::UiPreparedPresentationOperation,
) -> Result<()> {
    let admission = operation.admission().ok_or_else(|| {
        HandlerError::BadRequest("composition operations are not executable invocations".into())
    })?;
    let marker = crate::seat_auth::attachment_project_query_identity(attachment)
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?
        .map(|path| path.to_string_lossy().into_owned());
    let markers = SessionMarkers::for_attachment(state, attachment, scopes, marker.as_deref());
    let bound = resolve_binding_request(attachment, &admission.request, &markers)?;
    use ryeos_client_base::ui::admission_operation::UiPresentationOperationKind;
    let purpose = match bound.target.result_effect {
        Some(CompiledUiResultEffect::SurfaceOpening) => {
            resolved_surface_open_request(&bound.params)?
                .placement
                .purpose()
        }
        Some(CompiledUiResultEffect::ResumeParticularViewSet) => {
            UiPresentationOperationKind::ResumeParticularViewSet
        }
        Some(CompiledUiResultEffect::ResumeParticularSurface) => {
            UiPresentationOperationKind::ResumeParticularSurface
        }
        None => {
            return Err(HandlerError::BadRequest(
                "ordinary binding cannot prepare a presentation admission".into(),
            )
            .into());
        }
    };
    if purpose != admission.purpose
        || bound
            .params
            .get("creation_request_id")
            .and_then(Value::as_str)
            != Some(operation.operation_id.as_str())
    {
        return Err(HandlerError::BadRequest(
            "prepared presentation purpose or creation coordinate differs from signed binding"
                .into(),
        )
        .into());
    }
    validate_surface_open_request(
        purpose,
        &bound.params,
        admission.surface_open_target.as_ref(),
    )?;
    let resolved_particular = ryeos_client_base::ui::admission_operation::UiParticularAdmissionSelection::from_resolved_params(
        purpose,
        &bound.params,
    )
    .map_err(HandlerError::BadRequest)?;
    if admission.particular_selection != resolved_particular {
        return Err(HandlerError::BadRequest(
            "retained particular selection differs from signed resolved input".into(),
        )
        .into());
    }
    Ok(())
}

/// Compare the retained selection with the signed template's resolved input,
/// never with the caller's unresolved selection record.
fn resolved_surface_open_request(
    resolved_params: &Value,
) -> Result<ryeos_client_base::surface::opening::SurfaceOpenRequest> {
    ryeos_client_base::surface::opening::SurfaceOpenRequest::from_resolved_params(resolved_params)
        .map_err(|error| HandlerError::BadRequest(error).into())
}

fn validate_surface_open_request(
    purpose: ryeos_client_base::ui::admission_operation::UiPresentationOperationKind,
    resolved_params: &Value,
    request: Option<&ryeos_client_base::surface::opening::SurfaceOpenTarget>,
) -> Result<()> {
    use ryeos_client_base::ui::admission_operation::UiPresentationOperationKind;
    if !matches!(
        purpose,
        UiPresentationOperationKind::OpenSurface
            | UiPresentationOperationKind::ImportSurfaceTabs
            | UiPresentationOperationKind::OpenPresentationDocument
    ) {
        if request.is_some() {
            return Err(HandlerError::BadRequest(
                "non-opening admission carries a Surface selection".into(),
            )
            .into());
        }
        return Ok(());
    }
    let resolved = resolved_surface_open_request(resolved_params)?;
    if resolved.placement.purpose() != purpose || request != Some(&resolved.target()) {
        return Err(HandlerError::BadRequest(
            "retained Surface request differs from signed resolved input".into(),
        )
        .into());
    }
    Ok(())
}

/// Extract session_id from the handler context's fingerprint.
fn session_id_from_context(ctx: &HandlerContext) -> Option<String> {
    ctx.fingerprint.strip_prefix("session:").map(String::from)
}

fn invocation_context_for_session(session: &BrowserSession) -> HandlerContext {
    HandlerContext::new(
        session.principal_id.clone(),
        session.granted_caps.clone(),
        true,
    )
}

pub(crate) struct PreparedInvocation {
    pub(crate) project: ryeos_executor::execution::project_source::ResolvedProjectContext,
    pub(crate) exec_ctx: ryeos_executor::executor::ExecutionContext,
    pub(crate) project_content: Option<lillux::PinnedDirectory>,
}

pub async fn handle(input: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let invocation: UiInvocationRequest = serde_json::from_value(input.clone()).map_err(|e| {
        HandlerError::BadRequest(format!("invalid ui.invocations.dispatch request: {e}"))
    })?;
    enforce_request_bounds(&state, &invocation, &input)?;
    let presentation_operation_id = invocation.presentation_operation_id;
    let req = invocation.binding;
    let producer = invocation.producer;

    // Require browser session.
    let session_id = session_id_from_context(&ctx).ok_or_else(|| {
        HandlerError::Forbidden("session cookie required for UI invocation dispatch".into())
    })?;

    let ui = get_ui_state(&state).expect("UiState not set");

    let session = ui
        .browser_sessions
        .get_session(&session_id)
        .ok_or_else(|| HandlerError::Forbidden("session expired or invalid".into()))?;
    let attachment_coordinate = BindingAttachmentCoordinate {
        binding_attachment_id: req.binding_attachment_id.clone(),
        binding_generation: req.binding_generation,
        binding_digest: req.binding_digest.clone(),
    };
    let attachment = ui
        .browser_sessions
        .require_presentation_attachment(
            &session_id,
            &producer.presentation_id,
            &attachment_coordinate,
            state.node_policy.generation_digest(),
        )
        .map_err(|error| binding_stale(&error.to_string()))?;
    if attachment
        .compiled_binding
        .binding
        .node_policy_generation_digest
        != state.node_policy.generation_digest()
    {
        return Err(binding_stale(
            "the node policy generation changed after session mint",
        ));
    }
    authorize_route_context(&ctx, &state, &attachment, &req)?;
    let project_access =
        crate::seat_auth::attachment_project_access(&attachment).map_err(|_| {
            binding_stale("the selected project path no longer names its retained authority")
        })?;
    let project_path = match project_access.as_ref() {
        Some(access) => access.path().to_path_buf(),
        None => state.config.app_root.clone(),
    };
    // Authored source parameters and projection filters receive the stable
    // validated project identity.  The descriptor-rooted path above remains
    // solely the filesystem resolution coordinate and must never escape into
    // a request or durable row.
    let project_marker = crate::seat_auth::attachment_project_query_identity(&attachment)?
        .map(|path| path.to_string_lossy().into_owned());
    let markers = SessionMarkers::for_attachment(
        &state,
        &attachment,
        &session.granted_caps,
        project_marker.as_deref(),
    );
    let mut bound = resolve_binding_request(&attachment, &req, &markers)?;
    let item_ref = bound.target.identity.canonical_ref.clone();
    // Every read passes the node's seat read policy: parameters are pinned and
    // checked here, and the returned shaping decides what the result may
    // disclose. A read is a bound source, or any target declared read-only.
    let seat_reads = if matches!(req.coordinate, UiBindingCoordinate::Source { .. })
        || bound.target.source_safe
    {
        let policy = state
            .node_policy
            .require::<ryeos_node_runtime::node_policy::sections::read_disclosure::ReadDisclosurePolicy>(
        )?;
        let admitted = crate::seat_read_policy::decide(
            policy,
            &item_ref,
            &bound.target.parameter_schema,
            Some(crate::seat_read_policy::RequestFacts {
                params: std::mem::take(&mut bound.params),
                session: crate::seat_read_policy::SessionFacts {
                    principal_id: &attachment.compiled_binding.binding.principal_id,
                    project_root: project_marker.as_deref(),
                },
            }),
        )
        .map_err(|refusal| HandlerError::Forbidden(refusal.to_string()))?;
        bound.params = admitted.params.ok_or_else(|| {
            HandlerError::Internal("seat read policy returned no parameters for a call".into())
        })?;
        admitted.shaping
    } else {
        None
    };
    let invocation_ctx = invocation_context_for_session(&session);
    let prepared = prepare_item_ref(
        &invocation_ctx,
        &state,
        &project_path,
        project_access
            .as_ref()
            .map(|access| access.try_clone_directory())
            .transpose()?,
    )?;
    let current = prepared
        .exec_ctx
        .engine
        .with_checked_bundle_generation(|generation| {
            if generation.request_engine_generation_identity()
                != attachment
                    .compiled_binding
                    .binding
                    .request_engine_generation_identity
            {
                return Err(binding_stale(
                    "the engine generation changed after session mint",
                ));
            }
            super::ui_launch_mint::revalidate_binding_authority(
                generation,
                attachment.compiled_binding.as_ref(),
                attachment
                    .project_authority
                    .as_ref()
                    .map(|_| project_path.as_path()),
                prepared.project_content.as_ref(),
            )?;
            super::ui_launch_mint::compile_target(generation, &prepared, &item_ref)
                .map_err(ui_target_refusal_to_handler)
        })?;
    if current != bound.target {
        return Err(binding_stale(
            "a signed binding target changed after session mint",
        ));
    }
    let verified = match prepared.project_content.as_ref() {
        Some(project_content) => prepared
            .exec_ctx
            .engine
            .resolve_verified_under_project_authority(
                &prepared.exec_ctx.plan_ctx,
                &CanonicalRef::parse(&item_ref)?,
                &prepared.project.effective_path,
                project_content,
            )
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?,
        None => ryeos_executor::executor::resolve_and_verify(
            &prepared.exec_ctx.engine,
            &prepared.exec_ctx.plan_ctx,
            &item_ref,
            None,
        )
        .map_err(|error| HandlerError::BadRequest(format!("{error:#}")))?,
    };

    let invocation_id = uuid::Uuid::new_v4().to_string();
    let trusted_handler_context = select_trusted_handler_context(
        dispatch_mode(bound.target.dispatch_class),
        &ctx,
        &invocation_ctx,
    );

    let source_safe = bound.target.source_safe;
    let ingress =
        super::ui_seat::read_completed_seat_ingress(state.clone(), &session, &producer).await?;
    // The synchronous token issue is the admission boundary. Do not hold the
    // process-local transition mutex across execution/await: admitted work
    // keeps its ordinary execution owner if a later takeover replaces the UI.
    let _dispatch_admission = {
        let _transition = ui
            .lock_seat_transition()
            .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
        let admitted = ingress.authorize(&state, &session, &producer)?;
        match (
            bound.target.result_effect,
            presentation_operation_id.as_deref(),
        ) {
            (Some(_), Some(operation_id)) => {
                let operation =
                    ingress.require_prepared(&state, &session, &producer, operation_id, &req)?;
                validate_prepared_operation_binding(
                    &state,
                    &session.granted_caps,
                    &attachment,
                    &operation,
                )?;
                if bound
                    .params
                    .get("creation_request_id")
                    .and_then(Value::as_str)
                    != Some(operation_id)
                {
                    return Err(HandlerError::Conflict(
                        "resolved creation UUID differs from retained operation".into(),
                    )
                    .into());
                }
            }
            (Some(_), None) => {
                return Err(HandlerError::Forbidden(
                    "presentation admission requires acknowledged retained preparation".into(),
                )
                .into());
            }
            (None, Some(_)) => {
                return Err(HandlerError::BadRequest(
                    "ordinary invocation cannot carry a presentation operation".into(),
                )
                .into());
            }
            (None, None) => {}
        }
        state
            .engine
            .with_checked_bundle_generation(|generation| -> Result<_> {
                if generation.request_engine_generation_identity()
                    != attachment
                        .compiled_binding
                        .binding
                        .request_engine_generation_identity
                {
                    return Err(binding_stale(
                        "UI generation changed before dispatch admission",
                    ));
                }
                state
                    .state_store
                    .with_exact_seat_action_if_mixed_cut(&admitted.seat(), &admitted.cut(), || {
                        ui.browser_sessions
                            .admit_presentation_dispatch(
                                &session_id,
                                &producer.presentation_id,
                                &attachment_coordinate,
                                state.node_policy.generation_digest(),
                            )
                            .map_err(|error| binding_stale(&error.to_string()))
                    })?
                    .ok_or_else(|| {
                        HandlerError::Conflict(
                            "seat action anchor changed before dispatch admission".into(),
                        )
                        .into()
                    })
            })?
    };
    let expected_opening =
        if bound.target.result_effect == Some(CompiledUiResultEffect::SurfaceOpening) {
            Some(resolved_surface_open_request(&bound.params)?)
        } else {
            None
        };
    let mut result = crate::seat_auth::with_compiled_ui_source_session(
        &session,
        attachment.clone(),
        Some(producer.clone()),
        bound.project_authored_source,
        async {
            if runs_unrecorded(&bound.target) {
                // Surface admission runs inline here, not on a daemon-owned
                // task, so its large future lives on the heap.
                Box::pin(execute_unrecorded_service(
                    &item_ref,
                    bound.params,
                    &state,
                    prepared,
                    verified,
                    trusted_handler_context,
                ))
                .await
            } else {
                execute_prepared_item_ref(
                    &item_ref,
                    bound.params,
                    &state,
                    prepared,
                    verified,
                    &invocation_ctx.scopes,
                    trusted_handler_context,
                    super::ui_usage::ui_usage_subject(project_marker.as_deref()),
                    session.principal_id.as_str(),
                )
                .await
            }
        },
    )
    .await?;
    // A presentation result effect is validated and retained as the service's
    // own value, so the execution envelope around it is not part of the result.
    if bound.target.result_effect.is_some() {
        result = result.get_mut("result").map(Value::take).ok_or_else(|| {
            HandlerError::Internal("service execution returned no result envelope".into())
        })?;
    }
    let mut publish = || -> Result<Value> {
        if let Some(shaping) = &seat_reads {
            let slot: &mut Value = if bound.target.result_effect.is_some() {
                &mut result
            } else {
                result.get_mut("result").ok_or_else(|| {
                    HandlerError::Internal("service execution returned no result envelope".into())
                })?
            };
            *slot = shaping
                .apply(std::mem::take(slot))
                .map_err(|refusal| HandlerError::Forbidden(refusal.to_string()))?;
        }
        if let Some(expected) = &expected_opening {
            let actual = result
                .get("ui_transition")
                .and_then(|transition| transition.get("opening"))
                .cloned()
                .ok_or_else(|| {
                    HandlerError::Internal(
                        "Surface opening returned no exact request commitment".into(),
                    )
                })?;
            let actual: ryeos_client_base::surface::opening::SurfaceOpenRequest =
                serde_json::from_value(actual).map_err(|error| {
                    HandlerError::Internal(format!(
                        "Surface opening returned invalid request commitment: {error}"
                    ))
                })?;
            if &actual != expected {
                return Err(HandlerError::Internal(
                    "Surface opening result differs from the signed resolved request".into(),
                )
                .into());
            }
        }
        retain_declared_result_effect(&bound.target, &producer.presentation_id, &mut result)?;

        ui.session_bus.publish(
            &session_id,
            "invocation.dispatched",
            json!({
                "target": { "kind": "ref", "ref": item_ref },
                "invocation_id": invocation_id,
                "status": "executed",
            }),
        );

        Ok(json!({
            "status": "executed",
            "target": { "kind": "ref", "ref": item_ref },
            "invocation_id": invocation_id,
            "binding_attachment_id": attachment_coordinate.binding_attachment_id,
            "binding_generation": attachment_coordinate.binding_generation,
            "binding_digest": attachment_coordinate.binding_digest,
            "result": result,
        }))
    };
    if source_safe {
        // Execution has its original admitted owner. A fresh completed cut
        // qualifies disclosure; a stale UI cannot undo or reissue that work.
        let fresh =
            super::ui_seat::read_completed_seat_ingress(state.clone(), &session, &producer).await?;
        let _transition = ui
            .lock_seat_transition()
            .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
        let admitted = fresh.authorize(&state, &session, &producer)?;
        return state
            .engine
            .with_checked_bundle_generation(|generation| -> Result<Value> {
                if generation.request_engine_generation_identity()
                    != attachment
                        .compiled_binding
                        .binding
                        .request_engine_generation_identity
                {
                    return Err(binding_stale(
                        "UI generation changed before result disclosure",
                    ));
                }
                state
                    .state_store
                    .with_exact_seat_action_if_mixed_cut(&admitted.seat(), &admitted.cut(), || {
                        ui.browser_sessions
                            .require_presentation_attachment(
                                &session_id,
                                &producer.presentation_id,
                                &attachment_coordinate,
                                state.node_policy.generation_digest(),
                            )
                            .map_err(|error| binding_stale(&error.to_string()))?;
                        publish()
                    })?
                    .ok_or_else(|| {
                        HandlerError::Conflict("source result cut changed before disclosure".into())
                            .into()
                    })
            });
    }
    publish()
}

fn retain_declared_result_effect(
    target: &CompiledUiTarget,
    presentation_id: &str,
    result: &mut Value,
) -> Result<()> {
    let Some(fields) = result.as_object_mut() else {
        if target.result_effect.is_some() {
            return Err(HandlerError::Internal(
                "service declared a UI result effect but returned a non-object".into(),
            )
            .into());
        }
        return Ok(());
    };
    let authored = fields.remove("ui_transition");
    let authored_particular_selection = target
        .result_effect
        .and_then(|_| fields.remove("particular_selection"));
    if target.result_effect == Some(CompiledUiResultEffect::SurfaceOpening)
        && authored
            .as_ref()
            .and_then(|value| value.get("kind"))
            .and_then(Value::as_str)
            == Some("open_presentation_document")
    {
        let transition: ryeos_client_base::ui::binding::UiPreparedPresentationDocumentTransition =
            serde_json::from_value(authored.ok_or_else(|| {
                HandlerError::Internal("presentation preparation returned no transition".into())
            })?)
            .map_err(|error| {
                HandlerError::Internal(format!(
                    "presentation preparation returned invalid transition: {error}",
                ))
            })?;
        transition.validate().map_err(HandlerError::Internal)?;
        if transition.presentation_id != presentation_id
            || transition.creation_request_id != transition.opening.creation_request_id
            || transition.opening.placement.purpose()
                != ryeos_client_base::ui::admission_operation::UiPresentationOperationKind::OpenPresentationDocument
            || authored_particular_selection.is_some()
        {
            return Err(HandlerError::Internal(
                "presentation preparation returned contradictory admission".into(),
            ).into());
        }
        fields.insert("ui_transition".into(), serde_json::to_value(transition)?);
        return Ok(());
    }
    if target.result_effect == Some(CompiledUiResultEffect::SurfaceOpening)
        && authored
            .as_ref()
            .and_then(|value| value.get("kind"))
            .and_then(Value::as_str)
            == Some("import_surface_tabs")
    {
        let transition: ryeos_client_base::ui::binding::UiSurfaceTabsAdmissionTransition =
            serde_json::from_value(authored.ok_or_else(|| {
                HandlerError::Internal("Surface import returned no transition".into())
            })?)
            .map_err(|error| {
                HandlerError::Internal(format!(
                    "Surface import returned invalid transition: {error}",
                ))
            })?;
        transition.validate().map_err(HandlerError::Internal)?;
        if transition.presentation_id != presentation_id
            || transition.creation_request_id != transition.opening.creation_request_id
            || transition.opening.placement.purpose()
                != ryeos_client_base::ui::admission_operation::UiPresentationOperationKind::ImportSurfaceTabs
            || authored_particular_selection.is_some()
            || transition.origin_attachment.validate().is_err()
            || transition.origin_attachment.presentation.definition.surface().is_none()
            || transition.tab_attachments.iter().any(|attachment| attachment.validate().is_err())
        {
            return Err(HandlerError::Internal("Surface import returned contradictory admission".into()).into());
        }
        fields.insert("ui_transition".into(), serde_json::to_value(transition)?);
        return Ok(());
    }
    match target.result_effect {
        None => Ok(()),
        Some(CompiledUiResultEffect::SurfaceOpening)
        | Some(CompiledUiResultEffect::ResumeParticularSurface) => {
            #[derive(Deserialize, serde::Serialize)]
            #[serde(deny_unknown_fields)]
            struct SurfaceAdmissionTransition {
                kind: String,
                presentation_id: String,
                surface: ryeos_client_base::ui::binding::UiSurfaceMembership,
                attachment: ryeos_client_base::ui::UiBindingAttachment,
                tab_attachments: Vec<ryeos_client_base::ui::UiBindingAttachment>,
                creation_request_id: String,
                #[serde(default, skip_serializing_if = "Option::is_none")]
                opening: Option<ryeos_client_base::surface::opening::SurfaceOpenRequest>,
            }
            let transition: SurfaceAdmissionTransition =
                serde_json::from_value(authored.ok_or_else(|| {
                    HandlerError::Internal("Surface admission returned no typed transition".into())
                })?)
                .map_err(|error| {
                    HandlerError::Internal(format!(
                        "Surface admission returned invalid transition: {error}"
                    ))
                })?;
            let expected_kind =
                if target.result_effect == Some(CompiledUiResultEffect::SurfaceOpening) {
                    "open_surface"
                } else {
                    "resume_particular_surface"
                };
            let valid_opening = match (&transition.opening, expected_kind) {
                (Some(opening), "open_surface") => {
                    opening.validate().is_ok()
                        && opening.creation_request_id == transition.creation_request_id
                        && matches!(
                            &opening.placement,
                            ryeos_client_base::surface::opening::SurfaceOpenPlacement::NewSurface
                        )
                }
                (None, "resume_particular_surface") => true,
                _ => false,
            };
            let valid_opening_selection = match transition.opening.as_ref() {
                None => expected_kind == "resume_particular_surface",
                Some(opening) => {
                    use ryeos_client_base::surface::opening::SurfaceOpenSelection;
                    use ryeos_client_base::ui::binding::UiSurfaceCompositionProvenance;
                    match (
                        &opening.selection,
                        transition.surface.saved_surface.as_ref(),
                    ) {
                        (SurfaceOpenSelection::Authored { surface_ref }, None) => {
                            surface_ref.as_str() == transition.attachment.root_ref.as_str()
                        }
                        (
                            SurfaceOpenSelection::Reusable {
                                id,
                                expected_revision,
                                fresh_subjects,
                            },
                            Some(composition),
                        ) => {
                            composition.validate().is_ok()
                                && composition.library_identity().is_some_and(
                                    |(saved_id, revision, _)| {
                                        id == saved_id && expected_revision.eq(&revision)
                                    },
                                )
                                && composition.portable_tabs().is_some_and(|tabs| {
                                    fresh_subjects
                                        .keys()
                                        .all(|id| tabs.iter().any(|tab| &tab.saved_set_id == id))
                                        && tabs.iter().all(|tab| {
                                            fresh_subjects
                                                .get(&tab.saved_set_id)
                                                .map_or(tab.fresh_subjects.is_empty(), |subjects| {
                                                    subjects == &tab.fresh_subjects
                                                })
                                        })
                                })
                        }
                        (SurfaceOpenSelection::Composed(request), Some(composition)) => {
                            composition.validate().is_ok()
                                && matches!(&composition.provenance, UiSurfaceCompositionProvenance::Composed { composition: retained } if retained == request)
                                && ryeos_client_base::ui::binding::origin_matches_descriptor(
                                    &request.origin,
                                    &transition.attachment,
                                )
                        }
                        _ => false,
                    }
                }
            };
            let particular_selection = authored_particular_selection
                .map(serde_json::from_value)
                .transpose()
                .map_err(|_| {
                    HandlerError::Internal(
                        "Surface admission returned an invalid particular selection".into(),
                    )
                })?;
            let valid_particular_selection = if expected_kind == "open_surface" {
                particular_selection.is_none()
            } else {
                matches!(
                    &particular_selection,
                    Some(ryeos_client_base::ui::admission_operation::UiParticularAdmissionSelection::Surface { .. })
                )
            };
            let is_uuid = |value: &str| {
                uuid::Uuid::parse_str(value)
                    .ok()
                    .is_some_and(|parsed| parsed.to_string() == value)
            };
            let attachment_coordinate =
                |attachment: &ryeos_client_base::ui::UiBindingAttachment| {
                    ryeos_client_base::ui::UiBindingAttachmentCoordinate {
                        binding_attachment_id: attachment.binding_attachment_id.clone(),
                        binding_generation: attachment.binding_generation,
                        binding_digest: attachment.binding_digest.clone(),
                    }
                };
            let mut descriptors = std::collections::BTreeMap::new();
            for descriptor in &transition.tab_attachments {
                if descriptor.validate().is_err()
                    || descriptor.presentation.definition.surface().is_none()
                    || descriptors
                        .insert(descriptor.binding_attachment_id.as_str(), descriptor)
                        .is_some()
                {
                    return Err(HandlerError::Internal(
                        "Surface admission returned invalid tab attachments".into(),
                    )
                    .into());
                }
            }
            let mut tabs = std::collections::BTreeSet::new();
            let valid_composition = match transition.surface.saved_surface.as_ref() {
                None => {
                    expected_kind == "open_surface"
                        && descriptors.is_empty()
                        && transition
                            .attachment
                            .presentation
                            .definition
                            .surface()
                            .is_some_and(|surface| {
                                surface.display_title() == transition.surface.title
                            })
                }
                Some(composition) => {
                    composition.validate().map_err(HandlerError::Internal)?;
                    if composition.portable_tabs().is_some_and(|tabs| {
                        tabs.iter().any(|tab| {
                            descriptors
                                .get(tab.binding.binding_attachment_id.as_str())
                                .is_none_or(|descriptor| {
                                    !ryeos_client_base::ui::binding::origin_matches_descriptor(
                                        &tab.origin,
                                        descriptor,
                                    )
                                })
                        })
                    }) {
                        return Err(HandlerError::Internal(
                            "Surface tab origin differs from admitted descriptor".into(),
                        )
                        .into());
                    }
                    let tab_pairs = match &composition.body {
                        ryeos_client_base::ui::binding::UiSavedSurfaceCompositionBody::Particular { tabs }
                            if expected_kind == "resume_particular_surface"
                                && matches!(
                                    &particular_selection,
                                    Some(ryeos_client_base::ui::admission_operation::UiParticularAdmissionSelection::Surface { id, expected_revision })
                                        if composition.library_identity().is_some_and(|(saved_id, revision, _)| id == saved_id && *expected_revision == revision)
                                ) => tabs.iter()
                                .map(|tab| (tab.saved_set_id.as_str(), &tab.binding)).collect::<Vec<_>>(),
                        ryeos_client_base::ui::binding::UiSavedSurfaceCompositionBody::Reusable { tabs }
                            if expected_kind == "open_surface" => tabs.iter()
                                .map(|tab| (tab.saved_set_id.as_str(), &tab.binding)).collect::<Vec<_>>(),
                        ryeos_client_base::ui::binding::UiSavedSurfaceCompositionBody::Composed { tabs }
                            if expected_kind == "open_surface" => tabs.iter()
                                .map(|tab| (tab.saved_set_id.as_str(), &tab.binding)).collect::<Vec<_>>(),
                        _ => return Err(HandlerError::Internal("Surface receipt body contradicts its declared effect".into()).into()),
                    };
                    !tab_pairs.is_empty()
                        && tab_pairs.len() <= ryeos_client_base::surface::view_sets::MAX_VIEW_SETS
                        && tab_pairs.len() == descriptors.len()
                        && tab_pairs.iter().all(|(id, binding)| {
                            tabs.insert(*id)
                                && descriptors
                                    .get(binding.binding_attachment_id.as_str())
                                    .is_some_and(|descriptor| {
                                        **binding == attachment_coordinate(descriptor)
                                    })
                        })
                        && tabs.contains(composition.active_view_set_id.as_str())
                        && if expected_kind == "resume_particular_surface" {
                            fields.get("particular_surface")
                                == Some(&serde_json::to_value(composition)?)
                        } else {
                            !fields.contains_key("particular_surface")
                        }
                }
            };
            if transition.kind != expected_kind
                || !valid_opening
                || !valid_opening_selection
                || transition.presentation_id != presentation_id
                || ![
                    transition.presentation_id.as_str(),
                    transition.surface.surface_instance_id.as_str(),
                    transition.creation_request_id.as_str(),
                ]
                .into_iter()
                .all(is_uuid)
                || transition.surface.ordinal == 0
                || transition.surface.title.trim().is_empty()
                || transition.attachment.validate().is_err()
                || transition
                    .attachment
                    .presentation
                    .definition
                    .surface()
                    .is_none()
                || transition.surface.binding != attachment_coordinate(&transition.attachment)
                || !valid_particular_selection
                || !valid_composition
            {
                return Err(HandlerError::Internal(
                    "Surface admission returned contradictory admission".into(),
                )
                .into());
            }
            let manifest = &transition.surface.mount_manifest;
            manifest.validate().map_err(|error| {
                HandlerError::Internal(format!(
                    "Surface admission returned invalid mount manifest: {error}"
                ))
            })?;
            let expected_seeds = if let Some(saved) = &transition.surface.saved_surface {
                saved.mount_seeds()
            } else {
                transition
                    .attachment
                    .presentation
                    .definition
                    .surface()
                    .ok_or_else(|| {
                        HandlerError::Internal("Surface origin definition is absent".into())
                    })?
                    .to_mount_seeds()
                    .map_err(HandlerError::Internal)?
                    .into_iter()
                    .map(|seed| (seed, transition.surface.binding.clone()))
                    .collect()
            };
            if manifest.presentation_id.as_str() != transition.presentation_id
                || manifest.surface_instance_id.as_str() != transition.surface.surface_instance_id
                || manifest
                    .tabs
                    .iter()
                    .map(|tab| (tab.seed.clone(), tab.binding.clone()))
                    .collect::<Vec<_>>()
                    != expected_seeds
            {
                return Err(HandlerError::Internal(
                    "Surface mount manifest contradicts its admitted owner or seeds".into(),
                )
                .into());
            }
            let origin_surface = transition
                .attachment
                .presentation
                .definition
                .surface()
                .ok_or_else(|| {
                    HandlerError::Internal("Surface origin definition is absent".into())
                })?;
            for set in manifest
                .instantiate_with_exact_ids(&origin_surface.tiling)
                .map_err(HandlerError::Internal)?
            {
                let tab = manifest
                    .tabs
                    .iter()
                    .find(|tab| tab.view_set_id == set.id)
                    .ok_or_else(|| {
                        HandlerError::Internal("Surface manifest tab is absent".into())
                    })?;
                let descriptor = if transition.surface.saved_surface.is_some() {
                    *descriptors
                        .get(tab.binding.binding_attachment_id.as_str())
                        .ok_or_else(|| {
                            HandlerError::Internal("Surface tab descriptor is absent".into())
                        })?
                } else {
                    &transition.attachment
                };
                if set.tiles.values().any(|tile| {
                    !descriptor
                        .presentation
                        .views
                        .contains_key(&tile.view.view_ref)
                }) {
                    return Err(HandlerError::Internal(
                        "Surface manifest View is outside its admitted closure".into(),
                    )
                    .into());
                }
            }
            fields.insert("ui_transition".into(), serde_json::to_value(transition)?);
            if let Some(selection) = particular_selection {
                fields.insert(
                    "particular_selection".into(),
                    serde_json::to_value(selection)?,
                );
            }
            Ok(())
        }
        Some(CompiledUiResultEffect::ResumeParticularViewSet) => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct ResumeTransition {
                kind: String,
                presentation_id: String,
                surface: ryeos_client_base::ui::binding::UiSurfaceMembership,
                attachment: ryeos_client_base::ui::UiBindingAttachment,
                creation_request_id: String,
            }
            let transition: ResumeTransition =
                serde_json::from_value(authored.ok_or_else(|| {
                    HandlerError::Internal(
                        "particular-set resume returned no typed UI transition".into(),
                    )
                })?)
                .map_err(|_| {
                    HandlerError::Internal(
                        "particular-set resume returned an invalid typed UI transition".into(),
                    )
                })?;
            let resolved: ryeos_client_base::surface::view_sets::ResolvedParticularViewSet =
                serde_json::from_value(fields.remove("particular_view_set").ok_or_else(|| {
                    HandlerError::Internal("particular-set resume returned no resolved set".into())
                })?)
                .map_err(|_| {
                    HandlerError::Internal(
                        "particular-set resume returned an invalid resolved set".into(),
                    )
                })?;
            let particular_selection: ryeos_client_base::ui::admission_operation::UiParticularAdmissionSelection =
                serde_json::from_value(authored_particular_selection.ok_or_else(|| {
                    HandlerError::Internal(
                        "particular-set resume returned no typed particular selection".into(),
                    )
                })?)
                .map_err(|_| {
                    HandlerError::Internal(
                        "particular-set resume returned an invalid particular selection".into(),
                    )
                })?;
            if transition.kind != "resume_particular_view_set"
                || transition.presentation_id != presentation_id
                || [
                    &transition.presentation_id,
                    &transition.surface.surface_instance_id,
                    &transition.creation_request_id,
                ]
                .iter()
                .any(|value| {
                    uuid::Uuid::parse_str(value)
                        .ok()
                        .is_none_or(|parsed| parsed.to_string().as_str() != value.as_str())
                })
                || transition.surface.ordinal == 0
                || transition.attachment.validate().is_err()
                || transition
                    .attachment
                    .presentation
                    .definition
                    .surface()
                    .is_none_or(|surface| surface.display_title() != transition.surface.title)
                || transition.surface.binding.binding_attachment_id
                    != transition.attachment.binding_attachment_id
                || transition.surface.binding.binding_generation
                    != transition.attachment.binding_generation
                || transition.surface.binding.binding_digest != transition.attachment.binding_digest
                || !matches!(
                    &particular_selection,
                    ryeos_client_base::ui::admission_operation::UiParticularAdmissionSelection::ViewSet { id, surface_ref, .. }
                        if id == &resolved.id
                            && surface_ref == &transition.attachment.root_ref
                )
            {
                return Err(HandlerError::Internal(
                    "particular-set resume returned an invalid typed UI transition".into(),
                )
                .into());
            }
            fields.insert(
                "particular_view_set".to_string(),
                serde_json::to_value(resolved)?,
            );
            fields.insert(
                "particular_selection".to_string(),
                serde_json::to_value(particular_selection)?,
            );
            fields.insert(
                "ui_transition".to_string(),
                json!({
                    "kind": "resume_particular_view_set",
                    "presentation_id": transition.presentation_id,
                    "surface": transition.surface,
                    "attachment": transition.attachment,
                    "creation_request_id": transition.creation_request_id,
                }),
            );
            Ok(())
        }
    }
}

fn authorize_route_context(
    ctx: &HandlerContext,
    state: &AppState,
    attachment: &Arc<AdmittedBindingAttachment>,
    req: &UiBindingRequest,
) -> Result<()> {
    let (
        UiBindingCoordinate::SurfaceRoute,
        UiBindingPayload::Input {
            route: Some(route), ..
        },
    ) = (&req.coordinate, &req.payload)
    else {
        return Ok(());
    };
    let Some(thread_id) = route.thread_id.as_deref() else {
        return Ok(());
    };
    let subjects = authorize_exact_thread_subjects(
        ctx,
        state,
        &SeatCaller::Attachment(attachment.clone()),
        &[thread_id],
    )?;
    if route.chain_root_id.as_deref() != Some(subjects[0].chain_root_id.as_str()) {
        return Err(HandlerError::NotFound.into());
    }
    Ok(())
}

fn dispatch_mode(class: crate::compiled_binding::CompiledUiDispatchClass) -> UiDispatchMode {
    match class {
        crate::compiled_binding::CompiledUiDispatchClass::Verified => UiDispatchMode::Verified,
        crate::compiled_binding::CompiledUiDispatchClass::SessionLocal => {
            UiDispatchMode::SessionLocal
        }
    }
}

fn binding_stale(message: &str) -> anyhow::Error {
    HandlerError::Structured {
        code: "ui_binding_stale".to_string(),
        status: 409,
        body: json!({"code":"ui_binding_stale","error":message,"retryable":false,"remediation":"mint a new UI session"}),
    }.into()
}

fn enforce_request_bounds(
    state: &AppState,
    req: &UiInvocationRequest,
    input: &Value,
) -> Result<()> {
    let maximum = state
        .node_config
        .routes
        .iter()
        .find(|route| route.response.source.as_deref() == Some(DESCRIPTOR.service_ref))
        .map(|route| route.limits.body_bytes_max)
        .ok_or_else(|| HandlerError::Internal("UI binding dispatch route is absent".into()))?;
    let actual = serde_json::to_vec(input)?.len() as u64;
    if actual > maximum {
        return Err(HandlerError::BadRequest(format!(
            "UI binding request is {actual} bytes; maximum is {maximum}"
        ))
        .into());
    }
    req.validate_bounds(ryeos_client_base::ui::UiBindingRequestBounds {
        max_request_bytes: maximum,
        max_input_bytes: maximum,
        max_seat_append_bytes: super::ui_launch_mint::binding_request_bounds(state)?
            .max_seat_append_bytes,
    })
    .map_err(|error| HandlerError::BadRequest(error.to_string()).into())
}

fn resolve_binding_request(
    attachment: &AdmittedBindingAttachment,
    req: &UiBindingRequest,
    markers: &SessionMarkers<'_>,
) -> Result<BoundInvocation> {
    let project_root = markers.project_root;
    match (&req.coordinate, &req.payload) {
        (UiBindingCoordinate::SurfaceRoute, UiBindingPayload::Input { value, route }) => {
            let surface_route = attachment
                .compiled_binding
                .binding
                .surface_route
                .as_ref()
                .ok_or_else(|| {
                    HandlerError::Forbidden(
                        "surface route is absent from the compiled binding".into(),
                    )
                })?;
            let mut params = resolve_session_markers(&surface_route.parameters, markers)?;
            let params = params.as_object_mut().ok_or_else(|| {
                HandlerError::Internal("compiled surface route parameters are not an object".into())
            })?;
            params.insert(
                surface_route.bindings.input_parameter.clone(),
                Value::String(value.clone()),
            );
            if let Some(route) = route {
                validate_route_context(route)?;
                if let Some(thread_id) = &route.thread_id {
                    let parameter = surface_route
                        .bindings
                        .thread_target_parameter
                        .as_ref()
                        .ok_or_else(|| {
                            HandlerError::BadRequest(
                                "the signed surface route does not accept a thread target".into(),
                            )
                        })?;
                    params.insert(
                        parameter.clone(),
                        json!({"kind":"thread","thread_id":thread_id}),
                    );
                }
                if route.interrupt {
                    let parameter = surface_route
                        .bindings
                        .interrupt_intent_parameter
                        .as_ref()
                        .ok_or_else(|| {
                            HandlerError::BadRequest(
                                "the signed surface route does not accept interrupt delivery"
                                    .into(),
                            )
                        })?;
                    params.insert(parameter.clone(), Value::String("interrupt".to_string()));
                }
            }
            Ok(BoundInvocation {
                project_authored_source: false,
                target: surface_route.target.clone(),
                params: Value::Object(params.clone()),
            })
        }
        (
            UiBindingCoordinate::Source { view_ref, channel },
            UiBindingPayload::SourceParameters { params },
        ) => {
            let source = attachment
                .compiled_binding
                .binding
                .sources
                .get(view_ref)
                .and_then(|entries| entries.get(channel))
                .ok_or_else(|| {
                    HandlerError::Forbidden(
                        "source coordinate is absent from the compiled binding".into(),
                    )
                })?;
            Ok(BoundInvocation {
                project_authored_source: source.project_authored,
                target: source.target.clone(),
                params: bind_source_parameters(source, params, markers)?,
            })
        }
        (
            UiBindingCoordinate::Affordance {
                view_ref,
                affordance_id,
            },
            payload,
        ) => {
            let entry = attachment
                .compiled_binding
                .binding
                .affordances
                .get(view_ref)
                .and_then(|entries| entries.get(affordance_id))
                .ok_or_else(|| {
                    HandlerError::Forbidden(
                        "affordance coordinate is absent from the compiled binding".into(),
                    )
                })?;
            let CompiledUiAffordance::Execution {
                producer,
                invoke,
                target,
            } = entry
            else {
                return Err(HandlerError::BadRequest(
                    "UI-local affordances do not use execution dispatch".into(),
                )
                .into());
            };
            let (actual_producer, payload) = match payload {
                UiBindingPayload::Selection { record } => (
                    CompiledUiProducer::Selection,
                    ryeos_client_base::ui::content::Payload::Selection(record),
                ),
                UiBindingPayload::Input { value, route } if route.is_none() => (
                    CompiledUiProducer::Input,
                    ryeos_client_base::ui::content::Payload::Input(value),
                ),
                UiBindingPayload::Tokens { tokens, arguments } => (
                    CompiledUiProducer::Tokens,
                    ryeos_client_base::ui::content::Payload::Tokens { tokens, arguments },
                ),
                UiBindingPayload::Input { .. } => {
                    return Err(HandlerError::BadRequest(
                        "affordance input cannot carry surface-route context".into(),
                    )
                    .into());
                }
                UiBindingPayload::SourceParameters { .. } => {
                    return Err(HandlerError::BadRequest(
                        "source payload cannot invoke an affordance".into(),
                    )
                    .into());
                }
            };
            if *producer != actual_producer {
                return Err(HandlerError::BadRequest(
                    "payload producer does not match the signed affordance".into(),
                )
                .into());
            }
            let authored = json!({"invoke": invoke});
            let resolved = ryeos_client_base::ui::content::resolve_affordance_invoke(
                &authored,
                match actual_producer {
                    CompiledUiProducer::Selection => {
                        ryeos_client_base::ui::content::Producer::Selection
                    }
                    CompiledUiProducer::Input => ryeos_client_base::ui::content::Producer::Input,
                    CompiledUiProducer::Tokens => ryeos_client_base::ui::content::Producer::Tokens,
                },
                &payload,
            )
            .ok_or_else(|| {
                HandlerError::BadRequest(
                    "affordance payload does not satisfy the signed template".into(),
                )
            })?;
            let params = match resolved {
                ryeos_client_base::ui::content::AffordanceInvoke::Service {
                    item_ref,
                    args,
                    ..
                } => {
                    if item_ref != target.identity.canonical_ref {
                        return Err(binding_stale(
                            "resolved affordance target differs from its compiled target",
                        ));
                    }
                    args
                }
                ryeos_client_base::ui::content::AffordanceInvoke::Rye { tokens, args, .. } => {
                    json!({"project_path": project_root, "tokens": tokens, "arguments": args})
                }
                ryeos_client_base::ui::content::AffordanceInvoke::Ui { .. }
                | ryeos_client_base::ui::content::AffordanceInvoke::OpenSavedViewSet { .. }
                | ryeos_client_base::ui::content::AffordanceInvoke::SaveActiveViewSet { .. }
                | ryeos_client_base::ui::content::AffordanceInvoke::SaveParticularViewSet {
                    ..
                }
                | ryeos_client_base::ui::content::AffordanceInvoke::SaveActiveSurface { .. }
                | ryeos_client_base::ui::content::AffordanceInvoke::SaveParticularSurface {
                    ..
                } => {
                    return Err(HandlerError::BadRequest(
                        "UI-local affordance crossed execution dispatch".into(),
                    )
                    .into());
                }
            };
            let params = resolve_session_markers(&params, markers)?;
            Ok(BoundInvocation {
                project_authored_source: false,
                target: target.clone(),
                params,
            })
        }
        _ => Err(HandlerError::BadRequest(
            "binding coordinate and payload kind do not match".into(),
        )
        .into()),
    }
}

fn validate_route_context(route: &ryeos_client_base::ui::UiBindingRouteContext) -> Result<()> {
    for (name, value) in [
        ("thread_id", route.thread_id.as_deref()),
        ("chain_root_id", route.chain_root_id.as_deref()),
    ] {
        if value.is_some_and(|value| {
            value.is_empty() || value.len() > 256 || value.chars().any(char::is_control)
        }) {
            return Err(HandlerError::BadRequest(format!(
                "surface route `{name}` is not a bounded identifier"
            ))
            .into());
        }
    }
    if route.thread_id.is_some() != route.chain_root_id.is_some() {
        return Err(HandlerError::BadRequest(
            "surface route thread_id and chain_root_id must be supplied together".into(),
        )
        .into());
    }
    if route.interrupt && route.thread_id.is_none() {
        return Err(HandlerError::BadRequest(
            "surface route interrupt requires an exact thread_id".into(),
        )
        .into());
    }
    Ok(())
}

fn bind_source_parameters(
    source: &CompiledUiSource,
    supplied: &Value,
    markers: &SessionMarkers<'_>,
) -> Result<Value> {
    let supplied = supplied
        .as_object()
        .ok_or_else(|| HandlerError::BadRequest("source parameters must be an object".into()))?;
    let template = source.parameters.as_object().ok_or_else(|| {
        HandlerError::Internal("compiled source parameters are not an object".into())
    })?;
    if supplied
        .keys()
        .any(|key| !template.contains_key(key) && !source.dynamic_parameters.contains(key))
    {
        return Err(HandlerError::BadRequest(
            "source parameters contain a key absent from the signed binding".into(),
        )
        .into());
    }
    let mut bound = serde_json::Map::new();
    for (key, authored) in template {
        let supplied = supplied.get(key);
        let value = if source.dynamic_parameters.contains(key) {
            supplied
                .map(|value| bind_dynamic_source_parameter(key, value))
                .transpose()?
                .unwrap_or_else(|| authored.clone())
        } else {
            bind_authored_parameter(authored, supplied, key)?
        };
        bound.insert(key.clone(), value);
    }
    for key in &source.dynamic_parameters {
        if template.contains_key(key) {
            continue;
        }
        if let Some(value) = supplied.get(key) {
            bound.insert(key.clone(), bind_dynamic_source_parameter(key, value)?);
        }
    }
    let value = Value::Object(bound);
    resolve_session_markers(&value, markers)
}

fn bind_dynamic_source_parameter(key: &str, value: &Value) -> Result<Value> {
    if !value.is_string() {
        return Err(HandlerError::BadRequest(format!(
            "dynamic source parameter `{key}` must be a string"
        ))
        .into());
    }
    Ok(value.clone())
}

fn bind_authored_parameter(
    authored: &Value,
    supplied: Option<&Value>,
    path: &str,
) -> Result<Value> {
    match authored {
        Value::String(marker) if marker.starts_with("@facet:") => {
            supplied.cloned().ok_or_else(|| {
                HandlerError::BadRequest(format!(
                    "source parameter `{path}` requires its signed facet value"
                ))
                .into()
            })
        }
        Value::String(marker) if marker.starts_with("@session:") => {
            Ok(Value::String(marker.clone()))
        }
        Value::Object(authored_fields) => {
            let supplied_fields = supplied.and_then(Value::as_object).ok_or_else(|| {
                HandlerError::BadRequest(format!(
                    "source parameter `{path}` must preserve its signed object shape"
                ))
            })?;
            if supplied_fields
                .keys()
                .any(|key| !authored_fields.contains_key(key))
            {
                return Err(HandlerError::BadRequest(format!(
                    "source parameter `{path}` contains an unsigned field"
                ))
                .into());
            }
            Ok(Value::Object(
                authored_fields
                    .iter()
                    .map(|(key, value)| {
                        let child = format!("{path}.{key}");
                        Ok((
                            key.clone(),
                            bind_authored_parameter(value, supplied_fields.get(key), &child)?,
                        ))
                    })
                    .collect::<Result<_>>()?,
            ))
        }
        Value::Array(authored_values) => {
            let supplied_values = supplied.and_then(Value::as_array).ok_or_else(|| {
                HandlerError::BadRequest(format!(
                    "source parameter `{path}` must preserve its signed array shape"
                ))
            })?;
            if authored_values.len() != supplied_values.len() {
                return Err(HandlerError::BadRequest(format!(
                    "source parameter `{path}` changed its signed array length"
                ))
                .into());
            }
            Ok(Value::Array(
                authored_values
                    .iter()
                    .zip(supplied_values)
                    .enumerate()
                    .map(|(index, (value, supplied))| {
                        bind_authored_parameter(value, Some(supplied), &format!("{path}[{index}]"))
                    })
                    .collect::<Result<_>>()?,
            ))
        }
        _ if supplied.is_none() || supplied == Some(authored) => Ok(authored.clone()),
        _ => Err(HandlerError::BadRequest(format!(
            "source parameter `{path}` attempts to replace signed data"
        ))
        .into()),
    }
}

/// What the session markers of a signed binding entry resolve from, taken from
/// the admitted attachment at dispatch. The path markers read the retained
/// checkout; the typed markers read who the seat is and what it stands for,
/// judged now: `@session:key` is the seat's key (`fp:<hex>`),
/// `@session:principal` the owner that key acts for (`pid:v1:<hex>`), and
/// `@session:project` the project the seat states (`pid:v1:<hex>/<name>`).
/// A typed marker that cannot resolve is a typed refusal, never an empty value.
pub(crate) struct SessionMarkers<'a> {
    project_root: Option<&'a str>,
    typed: Option<TypedSessionFacts<'a>>,
}

/// What the typed markers are judged from.
struct TypedSessionFacts<'a> {
    state: &'a AppState,
    compiled: &'a crate::compiled_binding::SessionCompiledUiBinding,
    scopes: &'a [String],
}

impl<'a> SessionMarkers<'a> {
    pub(crate) fn for_attachment(
        state: &'a AppState,
        attachment: &'a AdmittedBindingAttachment,
        scopes: &'a [String],
        project_root: Option<&'a str>,
    ) -> Self {
        Self {
            project_root,
            typed: Some(TypedSessionFacts {
                state,
                compiled: &attachment.compiled_binding,
                scopes,
            }),
        }
    }

    #[cfg(test)]
    fn project_root_only(project_root: Option<&'a str>) -> Self {
        Self {
            project_root,
            typed: None,
        }
    }

    fn typed(&self, marker: &str) -> Result<&TypedSessionFacts<'a>> {
        self.typed.as_ref().ok_or_else(|| {
            HandlerError::Internal(format!("session marker `{marker}` has no session to read"))
                .into()
        })
    }

    fn key(&self) -> Result<String> {
        Ok(self
            .typed("@session:key")?
            .compiled
            .binding
            .principal_id
            .clone())
    }

    fn principal(&self) -> Result<String> {
        let facts = self.typed("@session:principal")?;
        let key = &facts.compiled.binding.principal_id;
        match facts.state.engine.trust_store.owner_for(key) {
            Ok(owner) => Ok(owner.principal.to_string()),
            Err(refusal) => Err(owner_marker_refusal(refusal)),
        }
    }

    fn project(&self) -> Result<String> {
        let facts = self.typed("@session:project")?;
        match crate::seat_project::resolve_for_binding(facts.state, facts.compiled, facts.scopes)? {
            ryeos_client_base::ui::UiSeatProject::Stated { project } => Ok(project),
            ryeos_client_base::ui::UiSeatProject::Absent { absence } => Err(marker_refusal(
                absence.code(),
                "this seat's checkout states no project",
            )),
            ryeos_client_base::ui::UiSeatProject::Projectless => Err(marker_refusal(
                "project_required",
                "this signed binding entry requires a seat that states a project",
            )),
        }
    }
}

/// What `@session:principal` answers when the node cannot name the owner of
/// the seat's key: a retired or revoked key is the typed `owner_key_not_active`;
/// any other refusal is the node's own fault.
fn owner_marker_refusal(refusal: ryeos_engine::trust::OwnerRefusal) -> anyhow::Error {
    match refusal {
        ryeos_engine::trust::OwnerRefusal::KeyNotActive { .. } => marker_refusal(
            "owner_key_not_active",
            "this seat's key no longer acts for an owner",
        ),
        other => {
            HandlerError::Internal(format!("resolve the owner of the seat key: {other}")).into()
        }
    }
}

/// A typed refusal of a session marker: the code names why the seat cannot
/// stand for what the marker asks.
fn marker_refusal(code: &str, message: &str) -> anyhow::Error {
    HandlerError::Structured {
        code: code.to_owned(),
        status: 409,
        body: json!({"code": code, "error": message}),
    }
    .into()
}

fn resolve_session_markers(value: &Value, markers: &SessionMarkers<'_>) -> Result<Value> {
    let project_root = markers.project_root;
    match value {
        Value::String(marker) if marker == "@session:key" => Ok(Value::String(markers.key()?)),
        Value::String(marker) if marker == "@session:principal" => {
            Ok(Value::String(markers.principal()?))
        }
        Value::String(marker) if marker == "@session:project" => {
            Ok(Value::String(markers.project()?))
        }
        Value::String(marker) if marker == "@session:project_root_or_null" => {
            Ok(project_root.map(Value::from).unwrap_or(Value::Null))
        }
        Value::String(marker) if marker == "@session:project_root" => {
            project_root.map(Value::from).ok_or_else(|| {
                HandlerError::BadRequest(
                    "this signed binding entry requires a project-scoped UI session".into(),
                )
                .into()
            })
        }
        Value::String(marker) if marker.starts_with("@session:") => Err(HandlerError::Internal(
            format!("unsupported compiled session marker `{marker}`"),
        )
        .into()),
        Value::Array(values) => Ok(Value::Array(
            values
                .iter()
                .map(|value| resolve_session_markers(value, markers))
                .collect::<Result<_>>()?,
        )),
        Value::Object(values) => Ok(Value::Object(
            values
                .iter()
                .map(|(key, value)| Ok((key.clone(), resolve_session_markers(value, markers)?)))
                .collect::<Result<_>>()?,
        )),
        value => Ok(value.clone()),
    }
}

fn select_trusted_handler_context(
    dispatch_mode: UiDispatchMode,
    browser_context: &HandlerContext,
    invocation_context: &HandlerContext,
) -> HandlerContext {
    match dispatch_mode {
        // These services intentionally operate on the transient browser
        // session and therefore need its session:<id> principal.
        UiDispatchMode::SessionLocal => browser_context.clone(),
        // Ordinary verified services execute under the durable principal and
        // scopes sealed into the execution context. Supplying the browser
        // session context here would create an identity mismatch.
        UiDispatchMode::Verified => invocation_context.clone(),
    }
}

pub(crate) fn prepare_item_ref(
    ctx: &HandlerContext,
    state: &AppState,
    project_path: &std::path::Path,
    project_content: Option<lillux::PinnedDirectory>,
) -> Result<PreparedInvocation> {
    // Resolution is not project execution. The verified descriptor decides
    // whether this request belongs to the daemon-local read lane or the
    // ordinary live-project execution lane, so requiring write authority here
    // makes every source-safe service unsatisfiable by construction.
    let project_source = ryeos_executor::execution::project_source::ProjectSource::LiveFs;
    let checkout_id = format!(
        "ui-{}-{:08x}",
        lillux::time::timestamp_millis(),
        rand::random::<u32>()
    );
    let project_ctx = ryeos_executor::execution::project_source::resolve_project_context(
        state,
        &project_source,
        project_path,
        &ctx.fingerprint,
        &checkout_id,
        None,
    )
    .map_err(|e| HandlerError::Internal(format!("resolve project context: {e}")))?;

    let plan_ctx = ryeos_engine::contracts::PlanContext {
        logical_project: ryeos_app::project_registry::live_logical_project(
            state,
            &ctx.fingerprint,
            &ctx.scopes,
            project_path,
        ),
        requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
            ryeos_engine::contracts::Principal {
                fingerprint: ctx.fingerprint.clone(),
                scopes: ctx.scopes.clone(),
            },
        ),
        project_context: ryeos_engine::contracts::ProjectContext::LocalPath {
            path: project_ctx.effective_path.clone(),
        },
        subject_resolution_authority: ryeos_engine::contracts::SubjectResolutionAuthority::LiveFs,
        current_site_id: state.threads.site_id().to_string(),
        origin_site_id: state.threads.site_id().to_string(),
        execution_hints: Default::default(),
        scheduled_fire: None,
        validate_only: false,
    };

    let exec_ctx = ryeos_executor::executor::ExecutionContext {
        principal_fingerprint: ctx.fingerprint.clone(),
        caller_scopes: ctx.scopes.clone(),
        engine: project_ctx.request_engine.clone(),
        plan_ctx,
        requested_call: None,
        disclosure_class: ryeos_app::operator_authority::disclosure_class_for(state, ctx),
    };

    Ok(PreparedInvocation {
        project: project_ctx,
        exec_ctx,
        project_content,
    })
}

/// Whether a compiled target runs on the unrecorded daemon-service executor:
/// a read-only source, or a service that declares a session transition. The
/// declaration is the only key; compilation already refused it on anything
/// but a trusted-bundle, session-local, non-read-only service.
fn runs_unrecorded(target: &crate::compiled_binding::CompiledUiTarget) -> bool {
    target.source_safe || target.result_effect.is_some()
}

async fn execute_unrecorded_service(
    item_ref: &str,
    params: Value,
    state: &AppState,
    prepared: PreparedInvocation,
    verified: ryeos_engine::contracts::VerifiedItem,
    local_handler_context: HandlerContext,
) -> Result<Value> {
    let schema_is_daemon_service = prepared
        .exec_ctx
        .engine
        .kinds
        .get(&verified.resolved.kind)
        .and_then(|schema| schema.execution())
        .and_then(|execution| execution.terminator.as_ref())
        .is_some_and(|terminator| {
            matches!(
                terminator,
                ryeos_engine::kind_registry::TerminatorDecl::InProcess {
                    registry: ryeos_engine::kind_registry::InProcessRegistryKind::Services,
                    ..
                }
            )
        });
    if !schema_is_daemon_service {
        return Err(HandlerError::Forbidden(
            "source-safe dispatch is restricted to verified daemon services".into(),
        )
        .into());
    }
    let thread_profile = prepared
        .exec_ctx
        .engine
        .kinds
        .get(&verified.resolved.kind)
        .and_then(|schema| schema.execution())
        .and_then(|execution| execution.thread_profile.as_ref())
        .map(|profile| profile.name.clone())
        .ok_or_else(|| {
            HandlerError::Internal(format!(
                "verified executable kind '{}' has no execution.thread_profile",
                verified.resolved.kind
            ))
        })?;

    // Read-only UI sources and declared session transitions are in-process
    // daemon work. They need the live project only as a resolution/input
    // scope; manufacturing project execution provenance for them both
    // over-authorizes the handler and wrongly demands
    // `ryeos.write.project.live`. Availability, signed required_caps, audit
    // policy, and session-local context are still enforced by the shared
    // verified service executor.
    let result = ryeos_executor::executor::execute_service_verified(
        verified,
        item_ref,
        params,
        ryeos_executor::executor::ExecutionMode::Live,
        &prepared.exec_ctx,
        state,
        ryeos_executor::executor::ServiceRecordingContext {
            authority_source:
                ryeos_executor::executor::ServiceRecordingAuthoritySource::UnrecordedOnly,
            usage_subject: None,
            usage_subject_asserted_by: None,
        },
        None,
        None,
        Some(local_handler_context),
    )
    .await
    .map_err(map_dispatch_error)?;

    Ok(json!({
        "thread": {
            "thread_id": result.invocation_id,
            "recorded": result.recorded,
            "kind": thread_profile,
            "item_ref": item_ref,
            "status": "completed",
            "trust_class": format!("{:?}", result.trust_class),
            "effective_caps": result.effective_caps,
        },
        "result": result.value,
    }))
}

async fn execute_prepared_item_ref(
    item_ref: &str,
    params: Value,
    state: &AppState,
    prepared: PreparedInvocation,
    verified: ryeos_engine::contracts::VerifiedItem,
    authority_scopes: &[String],
    local_handler_context: HandlerContext,
    usage_subject: ryeos_state::UsageSubject,
    usage_asserted_by: &str,
) -> Result<Value> {
    let root_canonical = CanonicalRef::parse(item_ref)
        .map_err(|e| HandlerError::BadRequest(format!("invalid item ref: {e}")))?;

    // A recorded dispatch executes against the live project. When the session
    // is project-bound, the project is the retained pinned directory: its
    // bound identity is the execution root, the held descriptor must still be
    // what that identity names, and the subject is re-resolved and verified
    // through the descriptor at that same root. The descriptor-rooted
    // resolution coordinate never becomes provenance.
    let (project_root, verified, exec_ctx) = match prepared.project_content.as_ref() {
        Some(directory) => {
            directory.ensure_path_binding().map_err(|error| {
                HandlerError::Forbidden(format!(
                    "the session project no longer names its retained directory: {error:#}"
                ))
            })?;
            let project_root = directory.path().to_path_buf();
            let mut exec_ctx = prepared.exec_ctx.clone();
            exec_ctx.plan_ctx.project_context =
                ryeos_engine::contracts::ProjectContext::LocalPath {
                    path: project_root.clone(),
                };
            let verified = exec_ctx
                .engine
                .resolve_verified_under_project_authority(
                    &exec_ctx.plan_ctx,
                    &root_canonical,
                    &project_root,
                    directory,
                )
                .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
            (project_root, verified, exec_ctx)
        }
        None => (
            prepared.project.effective_path.clone(),
            verified,
            prepared.exec_ctx.clone(),
        ),
    };
    let resolved_authority = ryeos_app::execution_policy::resolve_standard_local_live_authority(
        &project_root,
        authority_scopes.to_vec(),
        &exec_ctx.principal_fingerprint,
        &state.isolation,
    )
    .map_err(|error| {
        // The refusal names the capability the seat lacks. It travels as a
        // typed field so a non-operator seat reads it too: a capability name
        // is policy vocabulary, not a path or a secret.
        let message = format!("{error:#}");
        let capability = ryeos_engine::execution_contract::LIVE_PROJECT_WRITE_CAPABILITY;
        let mut body = json!({
            "code": "capability_required",
            "error": message,
            "retryable": false,
        });
        if message.contains(capability) {
            body["capability"] = json!(capability);
        }
        HandlerError::Structured {
            code: "capability_required".to_owned(),
            status: 403,
            body,
        }
    })?;
    let provenance = ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
        project_root.clone(),
        exec_ctx.engine.clone(),
        resolved_authority.project,
    )
    .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;

    // The handler of a recorded service runs on a task the daemon owns, so
    // the compiled dispatch authority this task holds is carried to it.
    let local_handler_context = match crate::seat_auth::current_compiled_ui_task_scope() {
        Some(scope) => local_handler_context.with_task_scope(scope),
        None => local_handler_context,
    };
    let dispatch_req = ryeos_executor::dispatch::DispatchRequest {
        launch_mode: "wait",
        target_site_id: None,
        validate_only: false,
        params,
        ref_bindings: Default::default(),
        product_selections: Vec::new(),
        acting_principal: exec_ctx.principal_fingerprint.as_str(),
        project_path: &project_root,
        provenance,
        lifecycle_authority: resolved_authority.lifecycle,
        launch_timings: None,
        original_root_kind: root_canonical.kind.as_str(),
        pre_minted_thread_id: None,
        usage_subject: Some(usage_subject),
        usage_subject_asserted_by: Some(usage_asserted_by.to_string()),
        previous_thread_id: None,
        initial_facets: Default::default(),
        root_admission: None,
        root_dispatch_evidence: None,
        parent_execution_context: None,
        effect_authority: None,
    };

    let result = ryeos_executor::dispatch::dispatch_verified_with_handler_context(
        item_ref,
        verified,
        local_handler_context,
        &dispatch_req,
        &exec_ctx,
        state,
    )
    .await
    .map_err(dispatch_error_to_handler)
    .map_err(Into::into);
    drop(dispatch_req);
    result
}

fn map_dispatch_error(error: anyhow::Error) -> HandlerError {
    let error = error
        .downcast::<ryeos_executor::dispatch_error::DispatchError>()
        .unwrap_or_else(ryeos_executor::dispatch_error::DispatchError::Internal);
    dispatch_error_to_handler(error)
}

/// Answer a refused binding target with its own name. A target of a kind
/// that is not a service is refused again here, at dispatch, whatever a
/// retained binding says.
fn ui_target_refusal_to_handler(error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<crate::compiled_binding::UiTargetRefusal>() {
        Some(refusal) => HandlerError::Structured {
            code: refusal.code().to_owned(),
            status: 403,
            body: json!({
                "code": refusal.code(),
                "error": refusal.to_string(),
                "retryable": false,
            }),
        }
        .into(),
        None => error,
    }
}

fn dispatch_error_to_handler(error: ryeos_executor::dispatch_error::DispatchError) -> HandlerError {
    HandlerError::Structured {
        code: error.code().to_owned(),
        status: error.http_status().as_u16(),
        body: ryeos_executor::structured_error::dispatch_error_value(&error),
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/invocations/dispatch",
    endpoint: "ui.invocations.dispatch",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    fn attach_fixture_manifest(transition: &mut Value) {
        if transition["kind"] == "open_surface" {
            let selection = if transition["surface"]["saved_surface"].is_object() {
                let provenance = &transition["surface"]["saved_surface"]["provenance"];
                json!({"kind":"reusable","id":provenance["saved_id"],
                    "expected_revision":provenance["library_revision"],"fresh_subjects":{}})
            } else {
                json!({"kind":"authored","surface_ref":transition["attachment"]["root_ref"]})
            };
            transition["opening"] = json!({"selection":selection,
                "placement":{"kind":"new_surface"},"creation_request_id":transition["creation_request_id"]});
        }
        use ryeos_client_base::ids::{PresentationId, SurfaceInstanceId};
        use ryeos_client_base::ui::{
            UiBindingAttachmentCoordinate, composition_identity::UiSurfaceMountManifest,
        };
        let binding: UiBindingAttachmentCoordinate =
            serde_json::from_value(transition["surface"]["binding"].clone()).unwrap();
        let seeds = if transition["surface"]["saved_surface"].is_object() {
            let saved: ryeos_client_base::ui::binding::UiSavedSurfaceComposition =
                serde_json::from_value(transition["surface"]["saved_surface"].clone()).unwrap();
            saved.mount_seeds()
        } else {
            let attachment: ryeos_client_base::ui::UiBindingAttachment =
                serde_json::from_value(transition["attachment"].clone()).unwrap();
            attachment
                .presentation
                .definition
                .surface()
                .unwrap()
                .to_mount_seeds()
                .unwrap()
                .into_iter()
                .map(|seed| (seed, binding.clone()))
                .collect()
        };
        let active = seeds.first().map(|(seed, _)| seed.id.clone());
        let manifest = UiSurfaceMountManifest::allocate(
            PresentationId::try_new(transition["presentation_id"].as_str().unwrap()).unwrap(),
            SurfaceInstanceId::try_new(
                transition["surface"]["surface_instance_id"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap(),
            1,
            seeds,
            active.as_deref(),
        )
        .unwrap();
        transition["surface"]["mount_manifest"] = serde_json::to_value(manifest).unwrap();
    }

    #[test]
    fn surface_selection_proof_matches_resolved_input_not_unresolved_record() {
        use ryeos_client_base::surface::opening::SurfaceOpenSelection;
        use ryeos_client_base::ui::admission_operation::UiPresentationOperationKind as Purpose;
        let selection = SurfaceOpenSelection::Reusable {
            id: "saved-a".into(),
            expected_revision: 7,
            fresh_subjects: BTreeMap::from([(
                "tab-a".into(),
                BTreeMap::from([(
                    "subject".into(),
                    BTreeMap::from([("thread_id".into(), serde_json::json!("T-a"))]),
                )]),
            )]),
        };
        let params = serde_json::json!({"selection": selection,
            "placement":{"kind":"new_surface"},
            "creation_request_id":"00000000-0000-4000-8000-000000000001"});
        let retained = resolved_surface_open_request(&params).unwrap().target();
        assert!(
            validate_surface_open_request(Purpose::OpenSurface, &params, Some(&retained)).is_ok()
        );
        for field in ["id", "expected_revision", "fresh_subjects"] {
            let mut changed = params.clone();
            changed["selection"][field] = match field {
                "id" => serde_json::json!("saved-b"),
                "expected_revision" => serde_json::json!(8),
                _ => serde_json::json!({}),
            };
            assert!(
                validate_surface_open_request(Purpose::OpenSurface, &changed, Some(&retained))
                    .is_err()
            );
        }
        assert!(
            validate_surface_open_request(
                Purpose::OpenSurface,
                &serde_json::json!({"surface_ref":"surface:test/a"}),
                Some(&retained)
            )
            .is_err()
        );
        assert!(validate_surface_open_request(Purpose::OpenSurface, &params, None).is_err());
        let mut changed_placement = params.clone();
        changed_placement["placement"] = serde_json::json!({"kind":"append_tabs"});
        assert!(
            validate_surface_open_request(
                Purpose::OpenSurface,
                &changed_placement,
                Some(&retained)
            )
            .is_err()
        );
        assert!(
            validate_surface_open_request(
                Purpose::ResumeParticularSurface,
                &params,
                Some(&retained)
            )
            .is_err()
        );
        assert!(
            validate_surface_open_request(Purpose::ResumeParticularSurface, &params, None).is_ok()
        );
    }

    #[test]
    fn prepared_document_commits_exact_shared_root_and_distinct_purpose() {
        use ryeos_client_base::ui::admission_operation::UiPresentationOperationKind as Purpose;
        let params = json!({
            "creation_request_id":"00000000-0000-4000-8000-000000000001",
            "selection":{"kind":"authored","surface_ref":"surface:test/selected"},
            "placement":{"kind":"new_presentation","root_binding":{
                "binding_attachment_id":"shared-root","binding_generation":1,"binding_digest":"a".repeat(64)
            }}
        });
        let retained = resolved_surface_open_request(&params).unwrap().target();
        assert!(
            validate_surface_open_request(
                Purpose::OpenPresentationDocument,
                &params,
                Some(&retained)
            )
            .is_ok()
        );
        for purpose in [Purpose::OpenSurface, Purpose::ImportSurfaceTabs] {
            assert!(validate_surface_open_request(purpose, &params, Some(&retained)).is_err());
        }
        for (field, value) in [
            ("binding_attachment_id", json!("another-root")),
            ("binding_generation", json!(2)),
            ("binding_digest", json!("b".repeat(64))),
        ] {
            let mut changed = params.clone();
            changed["placement"]["root_binding"][field] = value;
            assert!(
                validate_surface_open_request(
                    Purpose::OpenPresentationDocument,
                    &changed,
                    Some(&retained)
                )
                .is_err()
            );
        }
        assert!(
            validate_surface_open_request(Purpose::OpenPresentationDocument, &params, None)
                .is_err()
        );
    }

    #[test]
    fn import_preparation_commits_every_destination_coordinate() {
        use ryeos_client_base::ui::admission_operation::UiPresentationOperationKind as Purpose;
        let params = json!({
            "creation_request_id":"00000000-0000-4000-8000-000000000001",
            "selection":{"kind":"authored","surface_ref":"surface:test/source"},
            "placement":{"kind":"append_tabs",
                "destination_surface_instance_id":"11111111-1111-4111-8111-111111111111",
                "destination_binding":{"binding_attachment_id":"destination","binding_generation":2,"binding_digest":"ab".repeat(32)},
                "expected_composition_revision":3,"expected_next_mount_id":4,
                "insertion_index":1,"activate_imported_tab":false}
        });
        let retained = resolved_surface_open_request(&params).unwrap().target();
        assert!(
            validate_surface_open_request(Purpose::ImportSurfaceTabs, &params, Some(&retained))
                .is_ok()
        );
        assert!(
            validate_surface_open_request(Purpose::OpenSurface, &params, Some(&retained)).is_err()
        );
        for (field, replacement) in [
            (
                "destination_surface_instance_id",
                json!("22222222-2222-4222-8222-222222222222"),
            ),
            (
                "destination_binding",
                json!({"binding_attachment_id":"other","binding_generation":2,"binding_digest":"ab".repeat(32)}),
            ),
            ("expected_composition_revision", json!(4)),
            ("expected_next_mount_id", json!(5)),
            ("insertion_index", json!(0)),
            ("activate_imported_tab", json!(true)),
        ] {
            let mut changed = params.clone();
            changed["placement"][field] = replacement;
            assert!(
                validate_surface_open_request(
                    Purpose::ImportSurfaceTabs,
                    &changed,
                    Some(&retained)
                )
                .is_err(),
                "{field}"
            );
        }
    }

    fn target(result_effect: Option<CompiledUiResultEffect>) -> CompiledUiTarget {
        use ryeos_api::surface_views::EffectiveUiItemIdentity;
        use ryeos_engine::resolution::{EffectiveDefinitionDigest, TrustClass};

        CompiledUiTarget {
            identity: EffectiveUiItemIdentity {
                chain: Vec::new(),
                canonical_ref: "service:test/result".to_string(),
                effective_definition_digest: EffectiveDefinitionDigest::parse("44".repeat(32))
                    .expect("fixture digest"),
                effective_trust_class: TrustClass::TrustedBundle,
            },
            required_caps: Vec::new(),
            source_safe: false,
            dispatch_class: crate::compiled_binding::CompiledUiDispatchClass::Verified,
            result_effect,
            parameter_schema: BTreeMap::new(),
        }
    }

    #[test]
    fn only_a_declared_transition_or_a_read_only_source_runs_unrecorded() {
        // A service that declares no effect keeps the recorded lane, whatever
        // its result carries.
        assert!(!runs_unrecorded(&target(None)));
        assert!(runs_unrecorded(&target(Some(
            CompiledUiResultEffect::SurfaceOpening
        ))));
        let mut read_only = target(None);
        read_only.source_safe = true;
        assert!(runs_unrecorded(&read_only));
    }

    fn session(user_principal_id: Option<String>) -> BrowserSession {
        let now = Instant::now();
        BrowserSession {
            session_id: "session-1".to_string(),
            created_at: now,
            expires_at: now + Duration::from_secs(60),
            absolute_expires_at: now + Duration::from_secs(60),
            idle_timeout: Duration::from_secs(60),
            principal_id: "fp:7e577e577e577e577e577e577e577e577e577e577e577e577e577e577e577e57"
                .to_string(),
            granted_caps: vec!["ui.read".to_string()],
            user_principal_id,
            root_attachment_id: "attachment-1".to_string(),
            attachments: Default::default(),
            pending_transfer_attachments: Default::default(),
            presentations: Default::default(),
            next_attachment_generation: 1,
            next_surface_ordinal: 1,
            art_grants: Default::default(),
        }
    }

    #[test]
    fn prepared_document_result_checks_exact_child_initialization() {
        use ryeos_client_base::ui::binding::UiBindingAttachment;
        use ryeos_client_base::ui::composition_identity::UiSurfaceMountManifest;
        let parent = "11111111-1111-4111-8111-111111111111";
        let child = "22222222-2222-4222-8222-222222222222";
        let creation = "33333333-3333-4333-8333-333333333333";
        let surface = "44444444-4444-4444-8444-444444444444";
        let coordinate = |id: &str| json!({"binding_attachment_id":id,"binding_generation":1,"binding_digest":"a".repeat(64)});
        let descriptor = |id: &str, root_ref: &str, definition: Value| {
            json!({
                "binding_attachment_id":id,"binding_generation":1,"binding_digest":"a".repeat(64),
                "root_ref":root_ref,"root_generation":"generation","project_path":null,"subject":{"kind":"projectless"},"posture":"observation_only","seat_project":{"state":"projectless"},
                "named_items": [], "binding_request_bounds":{"max_request_bytes":65536,"max_input_bytes":1024,"max_seat_append_bytes":65536},
                "presentation":{"definition":definition,"views":{"view:test/work":{"widget":"rows"}}}
            })
        };
        let root = descriptor(
            "root",
            "config:test/shared",
            json!({"kind":"shared_presentation","config":{
                "category":"ui","shared_presentation":{"schema":2,"title":"RyeOS","launcher":"launch",
                    "dialogs":[{"id":"launch","title":"Launch","view":"view:test/work"}],"capabilities":{"sources":[],"affordances":[]}}
            }}),
        );
        let origin = descriptor(
            "surface",
            "surface:test/work",
            json!({"kind":"surface","surface":{"name":"Work","tiles":["view:test/work"]}}),
        );
        let origin_typed: UiBindingAttachment = serde_json::from_value(origin.clone()).unwrap();
        let seeds = origin_typed
            .presentation
            .definition
            .surface()
            .unwrap()
            .to_mount_seeds()
            .unwrap();
        let active = seeds[0].id.clone();
        let manifest = UiSurfaceMountManifest::allocate(
            ryeos_client_base::ids::PresentationId::try_new(child).unwrap(),
            ryeos_client_base::ids::SurfaceInstanceId::try_new(surface).unwrap(),
            1,
            seeds
                .into_iter()
                .map(|seed| (seed, serde_json::from_value(coordinate("surface")).unwrap()))
                .collect(),
            Some(&active),
        )
        .unwrap();
        let opening = json!({"creation_request_id":creation,"selection":{"kind":"authored","surface_ref":"surface:test/work"},"placement":{"kind":"new_presentation","root_binding":coordinate("root")}});
        let result = json!({"ui_transition":{"kind":"open_presentation_document","presentation_id":parent,"creation_request_id":creation,"opening":opening,
            "prepared":{"presentation_id":child,"thread_id":"T-55555555-5555-4555-8555-555555555555","chain_root_id":"T-55555555-5555-4555-8555-555555555555","root_binding":coordinate("root"),
                "initialization":{"schema":2,"selection":{"kind":"selected_surface","opening":opening},"presentation_id":child,"creation_request_id":creation,
                    "root_binding":coordinate("root"),"next_mount_id":manifest.next_mount_id,
                    "surfaces":[{"surface_instance_id":surface,"ordinal":1,"title":"Work","binding":coordinate("surface"),"mount_manifest":manifest,"saved_surface":null}],
                    "binding_attachments":[root,origin]}}}});
        retain_declared_result_effect(
            &target(Some(CompiledUiResultEffect::SurfaceOpening)),
            parent,
            &mut result.clone(),
        )
        .unwrap();
        for pointer in [
            "/ui_transition/prepared/root_binding/binding_digest",
            "/ui_transition/prepared/initialization/root_binding/binding_digest",
        ] {
            let mut changed = result.clone();
            *changed.pointer_mut(pointer).unwrap() = json!("b".repeat(64));
            assert!(
                retain_declared_result_effect(
                    &target(Some(CompiledUiResultEffect::SurfaceOpening)),
                    parent,
                    &mut changed
                )
                .is_err()
            );
        }
        let mut changed = result.clone();
        changed["ui_transition"]["prepared"]["initialization"]["selection"] =
            json!({"kind":"configured_defaults"});
        assert!(
            retain_declared_result_effect(
                &target(Some(CompiledUiResultEffect::SurfaceOpening)),
                parent,
                &mut changed
            )
            .is_err()
        );
        assert!(
            retain_declared_result_effect(
                &target(Some(CompiledUiResultEffect::SurfaceOpening)),
                child,
                &mut result.clone()
            )
            .is_err()
        );
    }

    #[test]
    fn invocation_context_uses_compiled_binding_principal() {
        let invocation_ctx = invocation_context_for_session(&session(Some(
            "fp:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        )));

        assert_eq!(
            invocation_ctx.fingerprint,
            "fp:7e577e577e577e577e577e577e577e577e577e577e577e577e577e577e577e57"
        );
        assert!(invocation_ctx.verified);
        assert_eq!(invocation_ctx.scopes, vec!["ui.read".to_string()]);
    }

    #[test]
    fn invocation_context_never_uses_cookie_transport_as_authority() {
        let invocation_ctx = invocation_context_for_session(&session(None));

        assert_eq!(
            invocation_ctx.fingerprint,
            "fp:7e577e577e577e577e577e577e577e577e577e577e577e577e577e577e577e57"
        );
        assert!(invocation_ctx.verified);
        assert_eq!(invocation_ctx.scopes, vec!["ui.read".to_string()]);
    }

    /// The caps a session's invocation is launched with are the session's own
    /// grant. The executor then refuses any declared cap this grant does not
    /// cover (`caller_capability_not_covered`, pinned in `root_admission_e2e`),
    /// so a directive cannot be born with more than the session holds.
    #[tokio::test]
    async fn a_session_invocation_is_prepared_with_the_sessions_grant_alone() {
        let (_tmp, _owner, state) = crate::test_support::build_test_state();
        let project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".ai")).unwrap();
        let mut granted = session(None);
        granted.granted_caps = vec!["ryeos.execute.directive.only-this".to_string()];

        let ctx = invocation_context_for_session(&granted);
        let prepared = prepare_item_ref(&ctx, &state, project.path(), None)
            .expect("prepare the session's invocation");

        assert_eq!(
            prepared.exec_ctx.principal_fingerprint,
            granted.principal_id
        );
        assert_eq!(prepared.exec_ctx.caller_scopes, granted.granted_caps);
        match &prepared.exec_ctx.plan_ctx.requested_by {
            ryeos_engine::contracts::EffectivePrincipal::Local(principal) => {
                assert_eq!(principal.fingerprint, granted.principal_id);
                assert_eq!(principal.scopes, granted.granted_caps);
            }
            other => panic!("expected the session's local principal, got {other:?}"),
        }
    }

    #[test]
    fn saved_surface_transition_rejects_extra_descriptors_and_coordinate_drift() {
        let presentation = "11111111-1111-4111-8111-111111111111";
        let binding = json!({"binding_attachment_id":"attachment-tab", "binding_generation":2,"binding_digest":"11"});
        let attachment = json!({"binding_attachment_id":"attachment-tab", "binding_generation":2,"binding_digest":"11",
            "root_ref":"surface:test/base","root_generation":"22","project_path":null,"subject":{"kind":"projectless"},"posture":"observation_only","seat_project":{"state":"projectless"},
            "presentation":{"definition":{"kind":"surface","surface":{"name":"Fixture"}},"views":{}},
            "named_items": [], "binding_request_bounds":{"max_request_bytes":65536,"max_input_bytes":65536,"max_seat_append_bytes":65536}});
        let composition = json!({"provenance":{"kind":"library","saved_id":"saved-surface","library_revision":1,"record_digest":"ab".repeat(32)},
            "active_view_set_id":"set-one","body":{"kind":"particular","tabs":[{"saved_set_id":"set-one","binding":binding,
                "resolved":{"id":"set-one","name":"First","composition":{"schema":2,"id":"set-one","title":"First","root":null},
                    "relationships":[]}}]}});
        let mut result = json!({"particular_surface":composition,
            "particular_selection":{"kind":"surface","id":"saved-surface","expected_revision":1},"ui_transition":{
            "kind":"resume_particular_surface","presentation_id":presentation,
            "creation_request_id":"33333333-3333-4333-8333-333333333333",
            "surface":{"surface_instance_id":"22222222-2222-4222-8222-222222222222","ordinal":1,"title":"Fixture",
                "binding":binding,"saved_surface":composition},"attachment":attachment,"tab_attachments":[attachment]}});
        attach_fixture_manifest(&mut result["ui_transition"]);
        retain_declared_result_effect(
            &target(Some(CompiledUiResultEffect::ResumeParticularSurface)),
            presentation,
            &mut result.clone(),
        )
        .unwrap();
        for mutation in [
            "extra",
            "binding",
            "composition",
            "unknown",
            "manifest_owner",
            "manifest_seed",
            "selection",
        ] {
            let mut invalid = result.clone();
            match mutation {
                "extra" => invalid["ui_transition"]["tab_attachments"]
                    .as_array_mut()
                    .unwrap()
                    .push(attachment.clone()),
                "binding" => {
                    invalid["ui_transition"]["surface"]["saved_surface"]["body"]["tabs"][0]["binding"]
                        ["binding_generation"] = json!(3)
                }
                "composition" => {
                    invalid["particular_surface"]["active_view_set_id"] = json!("other")
                }
                "unknown" => {
                    invalid["ui_transition"]["compatibility_attachment"] = attachment.clone()
                }
                "manifest_owner" => {
                    invalid["ui_transition"]["surface"]["mount_manifest"]["surface_instance_id"] =
                        json!("44444444-4444-4444-8444-444444444444")
                }
                "manifest_seed" => {
                    invalid["ui_transition"]["surface"]["mount_manifest"]["tabs"][0]["seed"]["title"] =
                        json!("Different retained seed")
                }
                "selection" => invalid["particular_selection"]["expected_revision"] = json!(2),
                _ => unreachable!(),
            }
            assert!(
                retain_declared_result_effect(
                    &target(Some(CompiledUiResultEffect::ResumeParticularSurface)),
                    presentation,
                    &mut invalid
                )
                .is_err(),
                "{mutation}"
            );
        }
    }

    #[test]
    fn undeclared_service_result_cannot_forge_a_ui_transition() {
        let mut result = json!({
            "value": 7,
            "ui_transition": {
                "kind": "forged_transition",
                "session_id": "forged",
                "launch_url": "/ui/launch/forged"
            }
        });

        retain_declared_result_effect(
            &target(None),
            "11111111-1111-4111-8111-111111111111",
            &mut result,
        )
        .expect("sanitize result");

        assert_eq!(result, json!({"value": 7}));
    }

    #[test]
    fn reusable_surface_effect_requires_reusable_body_and_exact_descriptor_closure() {
        let presentation = "11111111-1111-4111-8111-111111111111";
        let binding = json!({"binding_attachment_id":"reusable-tab","binding_generation":2,"binding_digest":"11".repeat(32)});
        let attachment = json!({"binding_attachment_id":"reusable-tab","binding_generation":2,"binding_digest":"11".repeat(32),
            "root_ref":"surface:test/base","root_generation":"22".repeat(32),"project_path":null,"subject":{"kind":"projectless"},"posture":"observation_only","seat_project":{"state":"projectless"},
            "presentation":{"definition":{"kind":"surface","surface":{"name":"Fixture"}},"views":{}},
            "named_items": [], "binding_request_bounds":{"max_request_bytes":65536,"max_input_bytes":65536,"max_seat_append_bytes":65536}});
        let composition = json!({"provenance":{"kind":"library","saved_id":"reusable","library_revision":2,"record_digest":"ab".repeat(32)},"active_view_set_id":"set-one",
            "body":{"kind":"reusable","tabs":[{"saved_set_id":"set-one","binding":binding,"origin":{"surface_ref":"surface:test/base"},
                "template":{"id":"set-one","name":"First","composition":{"schema":2,"id":"set-one","title":"First","root":null},"relationships":[]},"fresh_subjects":{}}]}});
        let mut result = json!({"ui_transition":{"kind":"open_surface","presentation_id":presentation,
            "creation_request_id":"33333333-3333-4333-8333-333333333333",
            "surface":{"surface_instance_id":"22222222-2222-4222-8222-222222222222","ordinal":1,"title":"Fixture","binding":binding,"saved_surface":composition},
            "attachment":attachment,"tab_attachments":[attachment]}});
        attach_fixture_manifest(&mut result["ui_transition"]);
        retain_declared_result_effect(
            &target(Some(CompiledUiResultEffect::SurfaceOpening)),
            presentation,
            &mut result.clone(),
        )
        .unwrap();
        let mut duplicate = result.clone();
        duplicate["ui_transition"]["tab_attachments"]
            .as_array_mut()
            .unwrap()
            .push(attachment);
        assert!(
            retain_declared_result_effect(
                &target(Some(CompiledUiResultEffect::SurfaceOpening)),
                presentation,
                &mut duplicate
            )
            .is_err()
        );
        let mut wrong_origin = result.clone();
        wrong_origin["ui_transition"]["surface"]["saved_surface"]["body"]["tabs"][0]["origin"]["surface_ref"] =
            json!("surface:test/other");
        assert!(
            retain_declared_result_effect(
                &target(Some(CompiledUiResultEffect::SurfaceOpening)),
                presentation,
                &mut wrong_origin,
            )
            .is_err()
        );
        let mut missing_commitment = result.clone();
        missing_commitment["ui_transition"]
            .as_object_mut()
            .unwrap()
            .remove("opening");
        assert!(
            retain_declared_result_effect(
                &target(Some(CompiledUiResultEffect::SurfaceOpening)),
                presentation,
                &mut missing_commitment,
            )
            .is_err()
        );
        let mut wrong_kind = result;
        wrong_kind["ui_transition"]["kind"] = json!("resume_particular_surface");
        assert!(
            retain_declared_result_effect(
                &target(Some(CompiledUiResultEffect::ResumeParticularSurface)),
                presentation,
                &mut wrong_kind
            )
            .is_err()
        );
    }

    #[test]
    fn import_effect_requires_exact_destination_manifest_and_origin() {
        use ryeos_client_base::ids::{PresentationId, SurfaceInstanceId};
        use ryeos_client_base::ui::{
            UiBindingAttachment, UiBindingAttachmentCoordinate,
            composition_identity::UiSurfaceMountManifest,
        };
        let presentation = "11111111-1111-4111-8111-111111111111";
        let destination = "22222222-2222-4222-8222-222222222222";
        let creation = "33333333-3333-4333-8333-333333333333";
        let attachment: UiBindingAttachment = serde_json::from_value(json!({
            "binding_attachment_id":"origin","binding_generation":2,"binding_digest":"ab".repeat(32),
            "root_ref":"surface:test/base","root_generation":"cd".repeat(32),
            "presentation":{"definition":{"kind":"surface","surface":{"name":"Fixture"}},"views":{}},
            "project_path":null,"subject":{"kind":"projectless"},"posture":"observation_only","seat_project":{"state":"projectless"},
            "named_items": [], "binding_request_bounds":{"max_request_bytes":65536,"max_input_bytes":4096,"max_seat_append_bytes":65536}
        })).unwrap();
        let coordinate = UiBindingAttachmentCoordinate {
            binding_attachment_id: attachment.binding_attachment_id.clone(),
            binding_generation: attachment.binding_generation,
            binding_digest: attachment.binding_digest.clone(),
        };
        let seeds = attachment
            .presentation
            .definition
            .surface()
            .unwrap()
            .to_mount_seeds()
            .unwrap();
        let active = seeds.first().map(|seed| seed.id.clone());
        let manifest = UiSurfaceMountManifest::allocate(
            PresentationId::try_new(presentation).unwrap(),
            SurfaceInstanceId::try_new(destination).unwrap(),
            5,
            seeds
                .into_iter()
                .map(|seed| (seed, coordinate.clone()))
                .collect(),
            active.as_deref(),
        )
        .unwrap();
        let opening = json!({"creation_request_id":creation,"selection":{"kind":"authored","surface_ref":"surface:test/base"},
            "placement":{"kind":"append_tabs","destination_surface_instance_id":destination,
                "destination_binding":{"binding_attachment_id":"destination","binding_generation":1,"binding_digest":"ef".repeat(32)},
                "expected_composition_revision":3,"expected_next_mount_id":5,"insertion_index":0,"activate_imported_tab":false}});
        let result = json!({"ui_transition":{"kind":"import_surface_tabs","presentation_id":presentation,"creation_request_id":creation,
            "opening":opening,"composition":null,"origin_attachment":attachment,"tab_attachments":[],"manifest":manifest}});
        retain_declared_result_effect(
            &target(Some(CompiledUiResultEffect::SurfaceOpening)),
            presentation,
            &mut result.clone(),
        )
        .unwrap();
        for (field, value) in [
            (
                "surface_instance_id",
                json!("44444444-4444-4444-8444-444444444444"),
            ),
            ("allocation_start", json!(6)),
        ] {
            let mut changed = result.clone();
            changed["ui_transition"]["manifest"][field] = value;
            assert!(
                retain_declared_result_effect(
                    &target(Some(CompiledUiResultEffect::SurfaceOpening)),
                    presentation,
                    &mut changed
                )
                .is_err(),
                "{field}"
            );
        }
        let mut changed = result;
        changed["ui_transition"]["origin_attachment"]["root_ref"] = json!("surface:test/other");
        assert!(
            retain_declared_result_effect(
                &target(Some(CompiledUiResultEffect::SurfaceOpening)),
                presentation,
                &mut changed
            )
            .is_err()
        );
    }

    #[test]
    fn declared_attachment_effect_is_typed_and_retained() {
        let mut transition = json!({
            "kind": "open_surface",
            "creation_request_id":"33333333-3333-4333-8333-333333333333",
            "tab_attachments":[],
            "presentation_id":"11111111-1111-4111-8111-111111111111",
            "surface":{"surface_instance_id":"22222222-2222-4222-8222-222222222222","ordinal":1,"title":"Fixture",
                "binding":{"binding_attachment_id":"attachment-2","binding_generation":2,"binding_digest":"11"}},
            "attachment": {
                "binding_attachment_id": "attachment-2",
                "binding_generation": 2,
                "binding_digest": "11",
                "root_ref": "surface:test/base",
                "root_generation": "22",
                "presentation": {"definition": {"kind": "surface", "surface": {"name": "Fixture"}}, "views": {}},
                "project_path": null,
                "subject": {"kind":"projectless"},
                "posture": "observation_only","seat_project":{"state":"projectless"},
                "named_items": [], "binding_request_bounds": {"max_request_bytes": 1024, "max_input_bytes": 512,"max_seat_append_bytes":65536}
            }
        });
        let attachment: ryeos_client_base::ui::UiBindingAttachment =
            serde_json::from_value(transition["attachment"].clone()).unwrap();
        transition["attachment"] = serde_json::to_value(attachment).unwrap();
        attach_fixture_manifest(&mut transition);
        let mut result = json!({"ui_transition": transition.clone()});

        retain_declared_result_effect(
            &target(Some(CompiledUiResultEffect::SurfaceOpening)),
            "11111111-1111-4111-8111-111111111111",
            &mut result,
        )
        .expect("retain signed result effect");

        assert_eq!(result["ui_transition"], transition);
        for (field, value) in [
            (
                "presentation",
                json!("33333333-3333-4333-8333-333333333333"),
            ),
            ("binding", json!("changed")),
            ("title", json!("Changed title")),
        ] {
            let mut mismatched = transition.clone();
            match field {
                "presentation" => mismatched["presentation_id"] = value,
                "binding" => mismatched["surface"]["binding"]["binding_digest"] = value,
                "title" => mismatched["surface"]["title"] = value,
                _ => unreachable!(),
            }
            assert!(
                retain_declared_result_effect(
                    &target(Some(CompiledUiResultEffect::SurfaceOpening)),
                    "11111111-1111-4111-8111-111111111111",
                    &mut json!({"ui_transition":mismatched}),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn declared_attachment_effect_rejects_missing_descriptor() {
        let mut result = json!({
            "ui_transition": {
                "kind": "open_surface"
            }
        });

        let error = retain_declared_result_effect(
            &target(Some(CompiledUiResultEffect::SurfaceOpening)),
            "11111111-1111-4111-8111-111111111111",
            &mut result,
        )
        .expect_err("missing descriptor must fail closed");

        assert!(
            error
                .to_string()
                .contains("Surface admission returned invalid transition")
        );
    }

    #[test]
    fn declared_attachment_effect_rejects_unknown_fields() {
        let mut result = json!({
            "ui_transition": {
                "kind": "open_surface",
                "attachment": {},
                "session_id": "forged"
            }
        });

        assert!(
            retain_declared_result_effect(
                &target(Some(CompiledUiResultEffect::SurfaceOpening)),
                "11111111-1111-4111-8111-111111111111",
                &mut result,
            )
            .is_err()
        );
    }

    #[test]
    fn particular_resume_effect_requires_typed_resolution_and_exact_insertion() {
        let mut result = json!({
            "particular_selection": {
                "kind": "view_set",
                "id": "current",
                "expected_revision": 1,
                "surface_ref": "surface:test/base"
            },
            "particular_view_set": {
                "id": "current",
                "name": "Current",
                "composition": {
                    "schema": 2,
                    "id": "current",
                    "title": "Current",
                    "root": {"type":"group","views":["view:test/retained"],"active":0}
                },
                "relationships": [],
                "resolved_selection_work": {
                    "thread": "T-head",
                    "chain_root": "T-root"
                }
            },
            "ui_transition": {
                "kind": "resume_particular_view_set",
                "presentation_id": "11111111-1111-4111-8111-111111111111",
                "creation_request_id": "33333333-3333-4333-8333-333333333333",
                "surface": {"surface_instance_id":"22222222-2222-4222-8222-222222222222", "ordinal":1, "title":"Fixture", "binding":{"binding_attachment_id":"attachment-2", "binding_generation":2,"binding_digest":"11"}},
                "attachment": {"binding_attachment_id":"attachment-2", "binding_generation":2,"binding_digest":"11", "root_ref":"surface:test/base", "root_generation":"22", "presentation":{"definition":{"kind":"surface","surface":{"name":"Fixture"}},"views":{}}, "project_path":null,"subject":{"kind":"projectless"},"posture":"observation_only","seat_project":{"state":"projectless"},"named_items": [], "binding_request_bounds":{"max_request_bytes":1024,"max_input_bytes":512,"max_seat_append_bytes":65536}}
            }
        });
        result["ui_transition"]["attachment"]["presentation"]["views"] =
            json!({"view:test/retained":{"widget":"rows"}});
        let resolved: ryeos_client_base::surface::view_sets::ResolvedParticularViewSet =
            serde_json::from_value(result["particular_view_set"].clone()).unwrap();
        let coordinate =
            serde_json::from_value(result["ui_transition"]["surface"]["binding"].clone()).unwrap();
        // A particular resume allocates the retained composition, not the
        // authored Surface's empty default tab. This mirrors its publication owner.
        let manifest =
            ryeos_client_base::ui::composition_identity::UiSurfaceMountManifest::allocate(
                ryeos_client_base::ids::PresentationId::try_new(
                    "11111111-1111-4111-8111-111111111111",
                )
                .unwrap(),
                ryeos_client_base::ids::SurfaceInstanceId::try_new(
                    "22222222-2222-4222-8222-222222222222",
                )
                .unwrap(),
                1,
                vec![(resolved.composition.clone(), coordinate)],
                Some(&resolved.composition.id),
            )
            .unwrap();
        result["ui_transition"]["surface"]["mount_manifest"] =
            serde_json::to_value(&manifest).unwrap();
        retain_declared_result_effect(
            &target(Some(CompiledUiResultEffect::ResumeParticularViewSet)),
            "11111111-1111-4111-8111-111111111111",
            &mut result,
        )
        .expect("retain typed particular-set resolution");
        assert_eq!(
            result["ui_transition"]["surface"]["mount_manifest"],
            serde_json::to_value(&manifest).unwrap()
        );
        assert_eq!(manifest.tabs[0].seed.id, "current");
        assert_eq!(manifest.tabs[0].tile_ids.len(), 1);
        assert_eq!(
            result["ui_transition"]["surface"]["binding"]["binding_attachment_id"],
            "attachment-2"
        );
        assert_eq!(
            result["particular_view_set"]["resolved_selection_work"]["thread"],
            "T-head"
        );

        let mut wrong_selection = result.clone();
        wrong_selection["particular_selection"]["id"] = json!("different");
        assert!(
            retain_declared_result_effect(
                &target(Some(CompiledUiResultEffect::ResumeParticularViewSet)),
                "11111111-1111-4111-8111-111111111111",
                &mut wrong_selection,
            )
            .is_err()
        );

        let mut wrong_origin = result.clone();
        wrong_origin["ui_transition"]["surface"]["binding"]["binding_attachment_id"] =
            json!("attachment-9");
        assert!(
            retain_declared_result_effect(
                &target(Some(CompiledUiResultEffect::ResumeParticularViewSet)),
                "11111111-1111-4111-8111-111111111111",
                &mut wrong_origin,
            )
            .is_err()
        );

        result["ui_transition"]["attachment"] = json!({
            "binding_attachment_id": "attachment-9",
            "binding_generation": 2,
            "binding_digest": "11",
            "root_ref": "surface:test/base",
            "root_generation": "22",
            "presentation": {"definition": {"kind": "surface", "surface": {"name": "Fixture"}}, "views": {}},
            "project_path": null,
            "subject": {"kind":"projectless"},
            "posture": "observation_only","seat_project":{"state":"projectless"},
            "named_items": [], "binding_request_bounds": {"max_request_bytes": 1024, "max_input_bytes": 512,"max_seat_append_bytes":65536}
        });
        assert!(
            retain_declared_result_effect(
                &target(Some(CompiledUiResultEffect::ResumeParticularViewSet)),
                "11111111-1111-4111-8111-111111111111",
                &mut result,
            )
            .is_err()
        );
    }

    #[test]
    fn binding_request_cannot_decode_an_arbitrary_target() {
        let request = serde_json::from_value::<UiBindingRequest>(serde_json::json!({
            "binding_attachment_id": "attachment-1",
            "binding_generation": 1,
            "binding_digest": "11",
            "coordinate": { "kind": "source", "view_ref": "view:x", "channel": "default" },
            "payload": { "kind": "source_parameters", "params": {} },
            "target": { "kind": "ref", "ref": "service:commands/submit" }
        }));
        assert!(request.is_err());
    }

    #[test]
    fn verified_dispatch_uses_the_durable_invocation_context() {
        let browser_context = HandlerContext::new("session:session-1".to_string(), vec![], false);
        let invocation_context = HandlerContext::new(
            "fp:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            vec!["ui.read".to_string()],
            true,
        );

        let selected = select_trusted_handler_context(
            UiDispatchMode::Verified,
            &browser_context,
            &invocation_context,
        );

        assert_eq!(selected.fingerprint, invocation_context.fingerprint);
        assert_eq!(selected.scopes, invocation_context.scopes);
        assert!(selected.verified);
    }

    #[test]
    fn session_local_dispatch_keeps_the_browser_session_context() {
        let browser_context = HandlerContext::new("session:session-1".to_string(), vec![], false);
        let invocation_context = HandlerContext::new(
            "fp:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            vec!["ui.read".to_string()],
            true,
        );

        let selected = select_trusted_handler_context(
            UiDispatchMode::SessionLocal,
            &browser_context,
            &invocation_context,
        );

        assert_eq!(selected.fingerprint, browser_context.fingerprint);
        assert_eq!(selected.scopes, browser_context.scopes);
        assert!(!selected.verified);
    }

    #[test]
    fn optional_project_marker_preserves_projectless_command_dispatch() {
        assert_eq!(
            resolve_session_markers(
                &json!({"project_path": "@session:project_root_or_null"}),
                &SessionMarkers::project_root_only(None),
            )
            .unwrap(),
            json!({"project_path": null})
        );
        assert_eq!(
            resolve_session_markers(
                &json!({"project_path": "@session:project_root_or_null"}),
                &SessionMarkers::project_root_only(Some("/project")),
            )
            .unwrap(),
            json!({"project_path": "/project"})
        );
    }

    #[test]
    fn required_project_marker_still_refuses_projectless_sessions() {
        assert!(
            resolve_session_markers(
                &json!("@session:project_root"),
                &SessionMarkers::project_root_only(None)
            )
            .is_err()
        );
    }

    /// A seat of the fixture key, on a checkout that states no project (the
    /// fixture registered none) or on no checkout at all.
    fn marker_session(
        state: &AppState,
        checkout: Option<&std::path::Path>,
    ) -> (Arc<AdmittedBindingAttachment>, Vec<String>) {
        let launch = crate::test_support::launch_context(
            "surface:ryeos/ui/base",
            checkout
                .map(|path| path.to_string_lossy().into_owned())
                .as_deref(),
            crate::compiled_binding::EffectiveUiPosture::ObservationOnly,
            None,
        );
        let (session_id, token) = crate::test_support::mint_launch(state, launch);
        let ui = get_ui_state(state).expect("UI state");
        assert_eq!(
            ui.browser_sessions.consume_launch_token(&token),
            Some(session_id.clone())
        );
        let session = ui
            .browser_sessions
            .get_session(&session_id)
            .expect("session");
        (
            session.attachments[&session.root_attachment_id].clone(),
            session.granted_caps.clone(),
        )
    }

    fn refusal_code(error: &anyhow::Error) -> Option<String> {
        match error.downcast_ref::<HandlerError>() {
            Some(HandlerError::Structured { code, .. }) => Some(code.clone()),
            _ => None,
        }
    }

    #[test]
    fn a_retired_or_revoked_key_is_the_typed_owner_refusal_and_other_faults_are_not() {
        use ryeos_engine::trust::{OwnerRefusal, SignerStatus};
        for status in [SignerStatus::Retired, SignerStatus::Revoked] {
            let error = owner_marker_refusal(OwnerRefusal::KeyNotActive { status });
            assert_eq!(
                refusal_code(&error).as_deref(),
                Some("owner_key_not_active")
            );
        }
        let error = owner_marker_refusal(OwnerRefusal::Unavailable);
        assert!(
            refusal_code(&error).is_none(),
            "a damaged registry is not a seat refusal"
        );
    }

    #[test]
    fn a_seat_on_a_registered_checkout_states_its_project_and_loses_it_with_the_registration() {
        let (_dir, _owner, state) = crate::test_support::build_test_state();
        let checkout = tempfile::TempDir::new().expect("checkout");
        let canonical = checkout.path().canonicalize().expect("canonical checkout");
        let identity = crate::test_support::register_operator_checkout(&state, &canonical, "demo");

        // A seat of the operator's key on that checkout, sealed with the
        // project the registration states.
        let operator = crate::test_support::operator_launch_context(&state);
        let key = if operator.fingerprint.starts_with("fp:") {
            operator.fingerprint.clone()
        } else {
            format!("fp:{}", operator.fingerprint)
        };
        let mut launch = crate::test_support::launch_context(
            "surface:ryeos/ui/base",
            Some(canonical.to_string_lossy().as_ref()),
            crate::compiled_binding::EffectiveUiPosture::ObservationOnly,
            None,
        );
        {
            let binding = &mut Arc::make_mut(&mut launch.compiled_binding).binding;
            binding.principal_id = key;
            binding.signer_project =
                ryeos_state::project_identity::LogicalProject::Stated(identity.clone());
        }
        let (session_id, token) = crate::test_support::mint_launch(&state, launch);
        let ui = get_ui_state(&state).expect("UI state");
        assert_eq!(
            ui.browser_sessions.consume_launch_token(&token),
            Some(session_id.clone())
        );
        let session = ui
            .browser_sessions
            .get_session(&session_id)
            .expect("session");
        let attachment = session.attachments[&session.root_attachment_id].clone();
        let scopes = vec!["*".to_owned()];
        let markers = SessionMarkers::for_attachment(&state, &attachment, &scopes, None);

        // The registration states the project: the marker is its identity, with the name.
        let stated = resolve_session_markers(&json!("@session:project"), &markers).unwrap();
        assert_eq!(stated, json!(identity.to_string()));
        assert!(stated.as_str().unwrap().ends_with("/demo"));

        // The registration is gone: the same seat answers the typed absence,
        // so the answer is judged now and not carried from mint.
        ryeos_node::project_registry::unregister(
            &ryeos_node::project_registry::RegistrationChange {
                app_root: &state.config.app_root,
                node: &state.identity,
                request_digest: &"0".repeat(64),
                path: &canonical,
                identity: &identity,
            },
        )
        .expect("unregister the checkout");
        let error = resolve_session_markers(&json!("@session:project"), &markers).unwrap_err();
        assert_eq!(refusal_code(&error).as_deref(), Some("unregistered"));
    }

    #[test]
    fn the_typed_markers_say_who_the_seat_is_and_what_it_stands_for() {
        let (_dir, _owner, state) = crate::test_support::build_test_state();
        let project = tempfile::TempDir::new().expect("checkout");

        // A seat on no checkout: the key and its owner resolve; a project does not.
        let (attachment, scopes) = marker_session(&state, None);
        let markers = SessionMarkers::for_attachment(&state, &attachment, &scopes, None);
        assert_eq!(
            resolve_session_markers(&json!({"k": "@session:key"}), &markers).unwrap(),
            json!({"k": crate::test_support::FIXTURE_KEY})
        );
        let owner = state
            .engine
            .trust_store
            .owner_for(crate::test_support::FIXTURE_KEY)
            .expect("the fixture key acts for its own owner")
            .principal
            .to_string();
        assert!(owner.starts_with("pid:v1:"));
        assert_eq!(
            resolve_session_markers(&json!("@session:principal"), &markers).unwrap(),
            json!(owner)
        );
        let error = resolve_session_markers(&json!("@session:project"), &markers).unwrap_err();
        assert_eq!(refusal_code(&error).as_deref(), Some("project_required"));

        // A seat on a checkout that states no project: the typed absence, never a path.
        let (attachment, scopes) = marker_session(&state, Some(project.path()));
        let markers = SessionMarkers::for_attachment(&state, &attachment, &scopes, None);
        let error = resolve_session_markers(&json!("@session:project"), &markers).unwrap_err();
        assert_eq!(refusal_code(&error).as_deref(), Some("unregistered"));

        // The typed markers need a session to read: without one they are refused.
        let bare = SessionMarkers::project_root_only(None);
        assert!(resolve_session_markers(&json!("@session:key"), &bare).is_err());
    }
}
