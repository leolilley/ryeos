//! Explicit session-wide binding attachment lifecycle operations.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_app::thread_lifecycle::ThreadFinalizeParams;
use ryeos_client_base::surface::opening::{SurfaceOpenRequest, SurfaceOpenSelection};
use ryeos_executor::executor::ServiceAvailability;

use crate::browser_session::{AttachmentStoreError, BindingAttachmentCoordinate};
use crate::handlers::ui_seat::seat_kind;
use crate::state::get_ui_state;

/// Admit one real Surface into an existing authenticated presentation. This
/// operation never creates a cookie, chooses a project, or claims a producer.
pub async fn handle_open_surface(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let request: SurfaceOpenRequest = serde_json::from_value(params)
        .map_err(|error| HandlerError::BadRequest(format!("invalid Surface open: {error}")))?;
    request.validate().map_err(HandlerError::BadRequest)?;
    // Producer and root authority are daemon-retained dispatch sideband, never
    // browser-selected JSON. Signed result_effect admission is checked by the
    // dispatcher against the acknowledged prepared operation before entry.
    let (session, origin) = crate::seat_auth::require_compiled_ui_session(&ctx, &state)?;
    let producer = crate::seat_auth::require_compiled_ui_producer()?;
    let session_id = session.session_id.clone();
    let ui = get_ui_state(&state).expect("UiState not set");
    let initial_ingress =
        super::ui_seat::read_completed_seat_ingress(state.clone(), &session, &producer).await?;
    let (root, root_binding) = {
        let _transition = ui
            .lock_seat_transition()
            .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
        initial_ingress.authorize(&state, &session, &producer)?;
        let membership = ui
            .browser_sessions
            .require_presentation(
                &session_id,
                &producer.presentation_id,
                Some(&producer.thread_id),
                None,
                state.node_policy.generation_digest(),
            )
            .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
        ui.browser_sessions
            .require_presentation_attachment(
                &session_id,
                &producer.presentation_id,
                &origin.coordinate(),
                state.node_policy.generation_digest(),
            )
            .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
        let root = ui
            .browser_sessions
            .resolve_attachment(&session_id, &membership.root_coordinate)
            .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
        (root, membership.root_coordinate)
    };
    if matches!(
        &request.selection,
        SurfaceOpenSelection::Reusable { .. } | SurfaceOpenSelection::Composed(_)
    ) {
        return open_reusable_surface(
            &request,
            &ctx,
            &state,
            &session,
            &producer,
            &origin.coordinate(),
            &root,
            &root_binding,
        )
        .await;
    }
    let SurfaceOpenSelection::Authored { surface_ref } = &request.selection else {
        unreachable!("closed selection handled")
    };
    let compile_context = HandlerContext::new(
        session.principal_id.clone(),
        session.granted_caps.clone(),
        true,
    );
    let selection = super::ui_launch_mint::Request {
        ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.into(),
        surface_ref: surface_ref.clone(),
        project_path: root.compiled_binding.binding.project_root.clone(),
        user_principal_id: None,
    };
    let (binding, presentation) = super::ui_launch_mint::compile_session_binding(
        &selection,
        &compile_context,
        &state,
        root.project_authority.as_deref(),
    )?;
    let bounds = super::ui_launch_mint::binding_request_bounds(&state)?;
    let append_limit = super::ui_projects::completed_admission_append_limit(&state)?
        .min(bounds.max_seat_append_bytes);
    let limits = crate::browser_session::PresentationLimits::from_policy(
        state
            .node_policy
            .require::<ryeos_node_runtime::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>(
            )?,
    )?;
    let resolution_root = root
        .project_authority
        .as_ref()
        .map(|authority| authority.descriptor_path())
        .transpose()?;
    let ingress =
        super::ui_seat::read_completed_seat_ingress(state.clone(), &session, &producer).await?;
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let admission = ingress.authorize(&state, &session, &producer)?;
    ui.browser_sessions
        .require_presentation_attachment(
            &session_id,
            &producer.presentation_id,
            &origin.coordinate(),
            state.node_policy.generation_digest(),
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let mut admitted_result = None;
    state
        .engine
        .with_checked_bundle_generation(|generation| -> Result<()> {
            let current_generation = generation.request_engine_generation_identity();
            if current_generation
                != root
                    .compiled_binding
                    .binding
                    .request_engine_generation_identity
                || current_generation != binding.binding.request_engine_generation_identity
            {
                return Err(HandlerError::Conflict(
                    "UI engine generation changed during Surface admission".into(),
                )
                .into());
            }
            for compiled in [root.compiled_binding.as_ref(), &binding] {
                super::ui_launch_mint::revalidate_binding_authority(
                    generation,
                    compiled,
                    resolution_root.as_deref(),
                    root.project_authority.as_deref(),
                )?;
            }
            let published = state.state_store.with_exact_seat_action_if_mixed_cut(&admission.seat(), &admission.cut(), || {
                    ui.browser_sessions
                        .reconcile_presentation_mount_frontier(
                            &session_id,
                            &producer.presentation_id,
                            &root_binding,
                            state.node_policy.generation_digest(),
                            ingress.next_mount_id(),
                        )
                        .map_err(|error| HandlerError::Conflict(error.to_string()))?;
                    if matches!(&request.placement, ryeos_client_base::surface::opening::SurfaceOpenPlacement::NewPresentation { .. }) {
                        admitted_result = Some(prepare_selected_document_at_cut(&state,&session,&producer,&request,&root_binding,
                            crate::browser_session::BindingAttachmentCandidate {
                                registered_project_id:root.registered_project_id.clone(),compiled_binding:Arc::new(binding),presentation,project_authority:root.project_authority.clone(),
                            },Vec::new(),None)?);
                        return Ok(());
                    }
                    if matches!(
                        &request.placement,
                        ryeos_client_base::surface::opening::SurfaceOpenPlacement::AppendTabs { .. }
                    ) {
                        ui.browser_sessions
                            .publish_surface_tabs(
                                &session_id,
                                &producer.presentation_id,
                                &root_binding,
                                &request,
                                ingress.composition_replay(),
                                crate::browser_session::BindingAttachmentCandidate {
                                    registered_project_id: root.registered_project_id.clone(),
                                    compiled_binding: Arc::new(binding),
                                    presentation,
                                    project_authority: root.project_authority.clone(),
                                },
                                Vec::new(),
                                None,
                                &limits,
                                bounds,
                                state.node_policy.generation_digest(),
                                |transition| {
                                    let result = json!({"ui_transition":transition});
                                    super::ui_projects::validate_completed_admission_size(
                                        &result,
                                        &producer,
                                        &request.creation_request_id,
                                        bounds.max_request_bytes,
                                        append_limit,
                                    )?;
                                    admitted_result = Some(result);
                                    Ok(())
                                },
                            )
                            .map_err(|error| HandlerError::Conflict(error.to_string()))?;
                        return Ok(());
                    }
                    ui.browser_sessions
                .publish_surface(
                    &session_id,
                    &producer.presentation_id,
                    &request.creation_request_id,
                    &root_binding,
                    crate::browser_session::BindingAttachmentCandidate {
                        registered_project_id: root.registered_project_id.clone(),
                        compiled_binding: Arc::new(binding),
                        presentation,
                        project_authority: root.project_authority.clone(),
                    },
                    &limits,
                    state.node_policy.generation_digest(),
                    |surface, attachment| {
                        let result = json!({"ui_transition": {
                            "kind":"open_surface", "presentation_id":producer.presentation_id,
                            "opening":request,
                            "surface":surface.public_descriptor(),
                            "attachment":attachment.public_descriptor(bounds),
                            "tab_attachments":[], "creation_request_id":request.creation_request_id,
                        }});
                        super::ui_projects::validate_completed_admission_size(
                            &result,
                            &producer,
                            &request.creation_request_id,
                            bounds.max_request_bytes,
                            append_limit,
                        )?;
                        admitted_result = Some(result);
                        Ok(())
                    },
                )
                .map_err(|error| HandlerError::Conflict(error.to_string()))?;
                    Ok(())
                },
            )?;
            if published.is_none() {
                return Err(HandlerError::Conflict(
                    "seat action anchor changed before publication".into(),
                )
                .into());
            }
            Ok(())
        })?;
    finish_open_result(
        &state,
        &session,
        admitted_result
            .ok_or_else(|| HandlerError::Internal("Surface publication result missing".into()))?,
    )
}

async fn open_reusable_surface(
    request: &SurfaceOpenRequest,
    ctx: &HandlerContext,
    state: &Arc<AppState>,
    session: &crate::browser_session::BrowserSession,
    producer: &ryeos_client_base::ui::UiPresentationProducer,
    invoking_attachment: &BindingAttachmentCoordinate,
    root: &Arc<crate::browser_session::AdmittedBindingAttachment>,
    root_binding: &BindingAttachmentCoordinate,
) -> Result<Value> {
    use super::ui_projects::reusable_surface::{PreparedReusableSurface, PreparedReusableTab};
    let prepared = super::ui_projects::reusable_surface::prepare_reusable_surface(
        request, ctx, state, session,
    )
    .await?;
    let PreparedReusableSurface {
        store: _store,
        receipt,
        origin_candidate,
        origin_project,
        tabs,
    } = prepared;
    // The existing principal lock remains alive until the atomic publication
    // returns; no library update can change the selected record or origins.
    let mut candidates = Vec::with_capacity(tabs.len());
    let mut projects = Vec::with_capacity(tabs.len());
    for PreparedReusableTab { candidate, project } in tabs {
        projects.push(project);
        candidates.push(candidate);
    }
    let admission = crate::browser_session::admission::SavedSurfaceAdmission::reusable(
        session.principal_id.clone(),
        receipt,
        origin_project,
        projects,
    )?;
    let bounds = super::ui_launch_mint::binding_request_bounds(state)?;
    let append_limit = super::ui_projects::completed_admission_append_limit(state)?
        .min(bounds.max_seat_append_bytes);
    let limits = crate::browser_session::PresentationLimits::from_policy(
        state
            .node_policy
            .require::<ryeos_node_runtime::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>(
            )?,
    )?;
    let ui = get_ui_state(state).expect("UiState not set");
    let ingress =
        super::ui_seat::read_completed_seat_ingress(state.clone(), session, producer).await?;
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let checked = ingress.authorize(state, session, producer)?;
    ui.browser_sessions
        .require_presentation_attachment(
            &session.session_id,
            &producer.presentation_id,
            invoking_attachment,
            state.node_policy.generation_digest(),
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let mut admitted_result = None;
    state.engine.with_checked_bundle_generation(|generation| -> Result<()> {
        let current = generation.request_engine_generation_identity();
        if root.compiled_binding.binding.request_engine_generation_identity != current {
            return Err(HandlerError::Conflict("UI generation changed during reusable Surface admission".into()).into());
        }
        let root_path = root.project_authority.as_ref().map(|authority| authority.descriptor_path()).transpose()?;
        super::ui_launch_mint::revalidate_binding_authority(generation, root.compiled_binding.as_ref(),
            root_path.as_deref(), root.project_authority.as_deref())?;
        for candidate in std::iter::once(&origin_candidate).chain(&candidates) {
            if candidate.compiled_binding.binding.request_engine_generation_identity != current {
                return Err(HandlerError::Conflict("UI generation changed during reusable tab admission".into()).into());
            }
            let path = candidate.project_authority.as_ref().map(|authority| authority.descriptor_path()).transpose()?;
            super::ui_launch_mint::revalidate_binding_authority(generation, candidate.compiled_binding.as_ref(),
                path.as_deref(), candidate.project_authority.as_deref())?;
        }
        let published = state.state_store.with_exact_seat_action_if_mixed_cut(&checked.seat(), &checked.cut(), || {
        ui.browser_sessions.reconcile_presentation_mount_frontier(
            &session.session_id, &producer.presentation_id, root_binding,
            state.node_policy.generation_digest(), ingress.next_mount_id(),
        ).map_err(|error| HandlerError::Conflict(error.to_string()))?;
        if matches!(&request.placement, ryeos_client_base::surface::opening::SurfaceOpenPlacement::NewPresentation { .. }) {
            admitted_result=Some(prepare_selected_document_at_cut(state,session,producer,request,root_binding,origin_candidate,candidates,Some(&admission))?);
            return Ok(());
        }
        if matches!(&request.placement, ryeos_client_base::surface::opening::SurfaceOpenPlacement::AppendTabs { .. }) {
            ui.browser_sessions.publish_surface_tabs(&session.session_id,&producer.presentation_id,root_binding,request,
                ingress.composition_replay(),origin_candidate,candidates,Some(&admission),
                &limits,bounds,state.node_policy.generation_digest(),
                |transition| {
                    let result=json!({"ui_transition":transition});
                    super::ui_projects::validate_completed_admission_size(&result,producer,&request.creation_request_id,bounds.max_request_bytes,append_limit)?;
                    admitted_result=Some(result); Ok(())
                }).map_err(|error|HandlerError::Conflict(error.to_string()))?;
            return Ok(());
        }
        ui.browser_sessions.publish_saved_surface(&session.session_id, &producer.presentation_id, &request.creation_request_id,
            root_binding, origin_candidate, candidates, &admission,
            &limits, state.node_policy.generation_digest(),
            |surface, origin, tabs| {
                let result = json!({"ui_transition":{"kind":"open_surface","presentation_id":producer.presentation_id,
                    "opening":request,
                    "creation_request_id":request.creation_request_id,"surface":surface.public_descriptor(),
                    "attachment":origin.public_descriptor(bounds),
                    "tab_attachments":tabs.iter().map(|attachment| attachment.public_descriptor(bounds)).collect::<Vec<_>>()}});
                super::ui_projects::validate_completed_admission_size(&result, producer, &request.creation_request_id,
                    bounds.max_request_bytes, append_limit)?;
                admitted_result = Some(result);
                Ok(())
            }).map_err(|error| HandlerError::Conflict(error.to_string()))?;
        Ok(())
            })?;
        if published.is_none() {
            return Err(HandlerError::Conflict("seat action anchor changed before publication".into()).into());
        }
        Ok(())
    })?;
    finish_open_result(
        state,
        session,
        admitted_result.ok_or_else(|| {
            HandlerError::Internal("reusable Surface publication result missing".into())
        })?,
    )
}

/// Called only inside the already-authorized parent operation's exact mixed cut.
fn prepare_selected_document_at_cut(
    state: &AppState,
    session: &crate::browser_session::BrowserSession,
    producer: &ryeos_client_base::ui::UiPresentationProducer,
    request: &SurfaceOpenRequest,
    parent_root: &BindingAttachmentCoordinate,
    origin: crate::browser_session::BindingAttachmentCandidate,
    tabs: Vec<crate::browser_session::BindingAttachmentCandidate>,
    proof: Option<&crate::browser_session::admission::SavedSurfaceAdmission>,
) -> Result<Value> {
    use ryeos_client_base::ui::binding::{
        UiPreparedPresentationDocument, UiPreparedPresentationDocumentKind,
        UiPreparedPresentationDocumentTransition,
    };
    let ryeos_client_base::surface::opening::SurfaceOpenPlacement::NewPresentation { root_binding } =
        &request.placement
    else {
        return Err(HandlerError::BadRequest("selected document placement absent".into()).into());
    };
    if root_binding.binding_attachment_id != parent_root.binding_attachment_id
        || root_binding.binding_generation != parent_root.binding_generation
        || root_binding.binding_digest != parent_root.binding_digest
    {
        return Err(HandlerError::Forbidden(
            "selected document root differs from exact parent root".into(),
        )
        .into());
    }
    let limits = crate::browser_session::PresentationLimits::from_policy(
        state
            .node_policy
            .require::<ryeos_node_runtime::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>(
            )?,
    )?;
    let bounds = super::ui_launch_mint::binding_request_bounds(state)?;
    let append_limit = super::ui_projects::completed_admission_append_limit(state)?
        .min(bounds.max_seat_append_bytes);
    let mut result = None;
    get_ui_state(state)
        .expect("UiState not set")
        .browser_sessions
        .prepare_selected_presentation(
            &session.session_id,
            request,
            origin,
            tabs,
            proof,
            &limits,
            state.node_policy.generation_digest(),
            bounds,
            |membership, descriptors| {
                super::ui_seat::initialization::preflight(state, membership)?;
                super::ui_session_current::preflight_initial_publication(
                    state,
                    session,
                    membership,
                    descriptors,
                )?;
                let transition = UiPreparedPresentationDocumentTransition {
                    kind: UiPreparedPresentationDocumentKind::OpenPresentationDocument,
                    presentation_id: producer.presentation_id.clone(),
                    creation_request_id: request.creation_request_id.clone(),
                    opening: request.clone(),
                    prepared: UiPreparedPresentationDocument {
                        presentation_id: membership.presentation_id.clone(),
                        thread_id: membership
                            .thread_id
                            .clone()
                            .ok_or(AttachmentStoreError::PresentationInvalid)?,
                        chain_root_id: membership
                            .chain_root_id
                            .clone()
                            .ok_or(AttachmentStoreError::PresentationInvalid)?,
                        root_binding: root_binding.clone(),
                        initialization: membership.initialization.clone(),
                    },
                };
                transition
                    .validate()
                    .map_err(|_| AttachmentStoreError::PresentationInvalid)?;
                let value = json!({"ui_transition":transition});
                super::ui_projects::validate_completed_admission_size(
                    &value,
                    producer,
                    &request.creation_request_id,
                    bounds.max_request_bytes,
                    append_limit,
                )?;
                result = Some(value);
                Ok(())
            },
        )
        .map_err(|error| HandlerError::Conflict(error.to_string()))?;
    result.ok_or_else(|| {
        HandlerError::Internal("selected document publication missing".into()).into()
    })
}

fn finish_open_result(
    state: &AppState,
    session: &crate::browser_session::BrowserSession,
    result: Value,
) -> Result<Value> {
    if result
        .pointer("/ui_transition/kind")
        .and_then(Value::as_str)
        != Some("open_presentation_document")
    {
        return Ok(result);
    }
    let transition: ryeos_client_base::ui::binding::UiPreparedPresentationDocumentTransition =
        serde_json::from_value(result["ui_transition"].clone())?;
    let membership = get_ui_state(state)
        .expect("UiState not set")
        .browser_sessions
        .require_presentation(
            &session.session_id,
            &transition.prepared.presentation_id,
            Some(&transition.prepared.thread_id),
            Some(&transition.prepared.chain_root_id),
            state.node_policy.generation_digest(),
        )
        .map_err(|error| HandlerError::Conflict(error.to_string()))?;
    if membership.initialization != transition.prepared.initialization {
        return Err(HandlerError::Conflict("selected initialization changed".into()).into());
    }
    // Reuses Prepare's lifecycle/journal owner. This is not a child Claim.
    super::ui_seat::initialization::materialize_unclaimed(state, session, membership)?;
    Ok(result)
}

pub const OPEN_SURFACE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/session/surfaces/open",
    endpoint: "ui.session.surfaces.open",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.ui/session/surfaces/open"],
    handler: |params, ctx, state| Box::pin(handle_open_surface(params, ctx, state)),
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DetachRequest {
    binding_attachment_id: String,
    binding_generation: u64,
    binding_digest: String,
}

impl DetachRequest {
    fn coordinate(&self) -> BindingAttachmentCoordinate {
        BindingAttachmentCoordinate {
            binding_attachment_id: self.binding_attachment_id.clone(),
            binding_generation: self.binding_generation,
            binding_digest: self.binding_digest.clone(),
        }
    }
}

fn session_id(ctx: &HandlerContext) -> Result<String, HandlerError> {
    ctx.fingerprint
        .strip_prefix("session:")
        .map(str::to_owned)
        .ok_or_else(|| HandlerError::Forbidden("session cookie required".into()))
}

fn client_ref(coordinate: &BindingAttachmentCoordinate) -> String {
    format!(
        "client:ryeos/ui-session/{}/{}:{}",
        coordinate.binding_attachment_id, coordinate.binding_generation, coordinate.binding_digest,
    )
}

/// How a seat whose attachment is detached ends. Its lease is released with
/// the attachment, so the seat finishes as an expired one does: the typed
/// lease-expiry outcome and nothing else, which is the only terminal tip the
/// seat's history replay accepts.
pub(crate) fn detach_finalize_params(thread_id: &str) -> ThreadFinalizeParams {
    ThreadFinalizeParams {
        thread_id: thread_id.to_owned(),
        status: "completed".into(),
        outcome_code: Some("seat_lease_expired".into()),
        result: None,
        error: None,
        metadata: None,
        artifacts: Vec::new(),
        final_cost: None,
        summary_json: None,
    }
}

pub async fn handle_detach(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let ui = get_ui_state(&state).expect("UiState not set");
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session_id = session_id(&ctx)?;
    let request: DetachRequest = serde_json::from_value(params)
        .map_err(|error| HandlerError::BadRequest(format!("invalid request: {error}")))?;
    let coordinate = request.coordinate();
    let attachment = match ui
        .browser_sessions
        .attachment_for_detach(&session_id, &coordinate)
    {
        Ok(attachment) => attachment,
        Err(AttachmentStoreError::RootAttachmentImmutable) => {
            return Err(HandlerError::Structured {
                code: "surface_attachment_immutable".into(),
                status: 409,
                body: json!({"code":"surface_attachment_immutable"}),
            }
            .into());
        }
        Err(error) => return Err(HandlerError::Forbidden(error.to_string()).into()),
    };

    let Some(_attachment) = attachment else {
        return Ok(json!({"detached":false,"attachment":coordinate}));
    };
    let owner = format!("session:{session_id}");
    let exact_client_ref = client_ref(&coordinate);
    // The lease table is keyed by every seat thread and queried without a
    // presentation limit, so cleanup cannot silently omit an older seat.
    for thread_id in state
        .state_store
        .seat_leases_for_owner_client(&owner, &exact_client_ref)?
    {
        let detail = state
            .state_store
            .get_thread(&thread_id)?
            .ok_or_else(|| HandlerError::Internal("leased seat thread disappeared".into()))?;
        if detail.kind != seat_kind(&state)?
            || detail.requested_by.as_deref() != Some(owner.as_str())
            || detail.executor_ref != exact_client_ref
        {
            return Err(HandlerError::Internal(
                "attachment seat lease identity is inconsistent".into(),
            )
            .into());
        }
        if detail.status == "running" {
            state
                .threads
                .finalize_thread(&detach_finalize_params(&detail.thread_id))?;
        }
        state.state_store.remove_seat_lease(&detail.thread_id)?;
    }
    // Revoke only after all recoverable cleanup completes. A failed cleanup
    // leaves the attachment admitted, so the exact request can be retried.
    ui.browser_sessions
        .revoke_attachment(&session_id, &coordinate)
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    Ok(json!({"detached":true,"attachment":coordinate}))
}

pub const DETACH_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/session/attachments/detach",
    endpoint: "ui.session.attachments.detach",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(handle_detach(params, ctx, state)),
};
