//! RyeOS UI local project registry and user RyeOS UI config handlers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::{HandlerError, parse_request};
use ryeos_app::principal::{
    HostedPrincipalResolver, LOCAL_PRINCIPAL_ID, LockedPrincipalStore, PrincipalStore,
};
use ryeos_app::state::AppState;
pub use ryeos_client_base::surface::saved_surfaces::{
    ParticularSurfaceRecord, ReusableSurfaceRecord, SavedSurfaceOrigin,
};
use ryeos_client_base::surface::view_sets::{
    ParticularViewSetResume, SavedViewSetTemplate, validate_particular_view_set_resumes,
    validate_saved_view_set_templates,
};
use ryeos_executor::executor::ServiceAvailability;

use crate::browser_session::admission::{
    ParticularSurfaceAdmission, RegisteredProjectSurfaceAdmission, SavedSurfaceAdmission,
};
use crate::seat_auth::require_seat_caller;
use crate::state::get_ui_state;

const PROJECTS_VERSION: u32 = 1;
const RYEOS_UI_CONFIG_VERSION: u32 = 6;
const RECENT_VERSION: u32 = 1;

pub(crate) mod reusable_surface;

pub(crate) fn completed_admission_append_limit(state: &AppState) -> Result<u64> {
    state
        .node_config
        .routes
        .iter()
        .find(|route| {
            route.response.source.as_deref() == Some(super::ui_seat::APPEND_DESCRIPTOR.service_ref)
        })
        .map(|route| route.limits.body_bytes_max)
        .ok_or_else(|| HandlerError::Internal("UI seat append route is absent".into()).into())
}

/// Charge the complete single-event wire envelope, not only the returned
/// descriptor. Decimal-string sequences conservatively include browser wire
/// encoding overhead; the append UUID is always canonical (36 bytes).
pub(crate) fn validate_completed_admission_size(
    result: &Value,
    producer: &ryeos_client_base::ui::UiPresentationProducer,
    operation_id: &str,
    max_result_bytes: u64,
    max_append_bytes: u64,
) -> std::result::Result<(), crate::browser_session::AttachmentStoreError> {
    use crate::browser_session::AttachmentStoreError;
    let payload = ryeos_client_base::ui::admission_operation::UiCompletedPresentationOperation {
        operation_id: operation_id.to_owned(),
        result: result.clone(),
    };
    let result_size = serde_json::to_vec(&payload)
        .map_err(|_| AttachmentStoreError::Capacity)?
        .len();
    let envelope = json!({
        "presentation_id":producer.presentation_id,"thread_id":producer.thread_id,
        "producer_incarnation":producer.producer_incarnation,
        "operation_id":operation_id,"first_engine_seq":u64::MAX.to_string(),
        "last_engine_seq":u64::MAX.to_string(),"event_count":1,"payload_digest":"f".repeat(64),
        "events":[{"engine_seq":u64::MAX.to_string(),
            "event_type":ryeos_client_base::ui::admission_operation::COMPLETED_EVENT,"payload":payload}]
    });
    let append_size = serde_json::to_vec(&envelope)
        .map_err(|_| AttachmentStoreError::Capacity)?
        .len();
    if max_result_bytes == 0
        || max_append_bytes == 0
        || u64::try_from(result_size).map_err(|_| AttachmentStoreError::Capacity)?
            > max_result_bytes
        || u64::try_from(append_size).map_err(|_| AttachmentStoreError::Capacity)?
            > max_append_bytes
    {
        return Err(AttachmentStoreError::Capacity);
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectsFile {
    pub version: u32,
    #[serde(default)]
    pub projects: Vec<ProjectEntry>,
}

impl Default for ProjectsFile {
    fn default() -> Self {
        Self {
            version: PROJECTS_VERSION,
            projects: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectEntry {
    pub local_id: String,
    pub name: String,
    pub root: String,
    pub added_at: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RyeOsConfigFile {
    pub version: u32,
    pub theme: String,
    pub landing_view: String,
    pub view_set_library_revision: u64,
    #[serde(default)]
    pub saved_view_sets: Vec<SavedViewSetTemplate>,
    pub particular_view_set_library_revision: u64,
    #[serde(default)]
    pub particular_view_sets: Vec<ParticularViewSetResume>,
    pub reusable_surface_library_revision: u64,
    #[serde(default)]
    pub reusable_surfaces: Vec<ReusableSurfaceRecord>,
    pub particular_surface_library_revision: u64,
    #[serde(default)]
    pub particular_surfaces: Vec<ParticularSurfaceRecord>,
}

impl Default for RyeOsConfigFile {
    fn default() -> Self {
        Self {
            version: RYEOS_UI_CONFIG_VERSION,
            theme: "system".into(),
            landing_view: "projects".into(),
            view_set_library_revision: 0,
            saved_view_sets: Vec::new(),
            particular_view_set_library_revision: 0,
            particular_view_sets: Vec::new(),
            reusable_surface_library_revision: 0,
            reusable_surfaces: Vec::new(),
            particular_surface_library_revision: 0,
            particular_surfaces: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecentFile {
    pub version: u32,
    #[serde(default)]
    pub recent_projects: Vec<RecentProject>,
}

impl Default for RecentFile {
    fn default() -> Self {
        Self {
            version: RECENT_VERSION,
            recent_projects: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecentProject {
    pub local_id: String,
    pub opened_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddProjectRequest {
    pub root: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgetProjectRequest {
    #[serde(default)]
    pub local_id: Option<String>,
    #[serde(default)]
    pub root: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveProjectRequest {
    pub local_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenProjectRequest {
    pub local_id: String,
    pub selection: ryeos_client_base::surface::opening::SurfaceOpenSelection,
    pub creation_request_id: String,
    pub placement: ryeos_client_base::surface::opening::SurfaceOpenPlacement,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TouchRecentRequest {
    pub local_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateConfigRequest {
    #[serde(default)]
    pub theme: Option<String>,
    #[serde(default)]
    pub landing_view: Option<String>,
    #[serde(default)]
    pub view_set_library: Option<ViewSetLibraryUpdate>,
    #[serde(default)]
    pub particular_view_set_library: Option<ParticularViewSetLibraryUpdate>,
    #[serde(default)]
    pub reusable_surface_library: Option<ReusableSurfaceLibraryUpdate>,
    #[serde(default)]
    pub particular_surface_library: Option<ParticularSurfaceLibraryUpdate>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReusableSurfaceLibraryUpdate {
    pub expected_revision: u64,
    pub reusable_surfaces: Vec<ReusableSurfaceRecord>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParticularSurfaceLibraryUpdate {
    pub expected_revision: u64,
    pub particular_surfaces: Vec<ParticularSurfaceRecord>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewSetLibraryUpdate {
    pub expected_revision: u64,
    pub saved_view_sets: Vec<SavedViewSetTemplate>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParticularViewSetLibraryUpdate {
    pub expected_revision: u64,
    pub particular_view_sets: Vec<ParticularViewSetResume>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeParticularViewSetRequest {
    pub id: String,
    pub expected_revision: u64,
    pub surface_ref: String,
    pub creation_request_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeParticularSurfaceRequest {
    pub id: String,
    pub expected_revision: u64,
    pub view_set_expected_revision: u64,
    pub creation_request_id: String,
}

fn compile_saved_surface_context(
    state: &AppState,
    session: &crate::browser_session::BrowserSession,
    origin: &SavedSurfaceOrigin,
    projects: &ProjectsFile,
) -> Result<(
    crate::browser_session::BindingAttachmentCandidate,
    Option<RegisteredProjectSurfaceAdmission>,
)> {
    let project = origin
        .project
        .as_ref()
        .map(|reference| {
            projects
                .projects
                .iter()
                .find(|entry| entry.local_id == reference.local_id)
                .cloned()
                .ok_or(HandlerError::NotFound)
        })
        .transpose()?;
    let canonical = project
        .as_ref()
        .map(|project| canonical_project_root(&project.root))
        .transpose()?;
    let authority = canonical
        .as_ref()
        .map(|path| {
            lillux::PinnedDirectory::open(path)
                .and_then(|directory| directory.context("saved Surface project disappeared"))
                .map(Arc::new)
        })
        .transpose()?;
    let request = super::ui_launch_mint::Request {
        ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.into(),
        surface_ref: origin.surface_ref.clone(),
        project_path: canonical.as_ref().map(|path| path.display().to_string()),
        user_principal_id: None,
    };
    let context = HandlerContext::new(
        session.principal_id.clone(),
        session.granted_caps.clone(),
        true,
    );
    let (compiled, presentation) = super::ui_launch_mint::compile_session_binding(
        &request,
        &context,
        state,
        authority.as_deref(),
    )?;
    let proof = match (&project, &canonical, &authority) {
        (Some(project), Some(path), Some(authority)) => Some(RegisteredProjectSurfaceAdmission {
            principal_id: session.principal_id.clone(),
            local_id: project.local_id.clone(),
            root: path.display().to_string(),
            authority: authority.clone(),
        }),
        (None, None, None) => None,
        _ => {
            return Err(
                HandlerError::Internal("saved Surface project authority mismatch".into()).into(),
            );
        }
    };
    Ok((
        crate::browser_session::BindingAttachmentCandidate {
            registered_project_id: project.map(|project| project.local_id),
            compiled_binding: Arc::new(compiled),
            presentation,
            project_authority: authority,
        },
        proof,
    ))
}

fn validate_saved_tab_binding(
    saved: &ParticularViewSetResume,
    candidate: &crate::browser_session::BindingAttachmentCandidate,
    admitted_collection_ids: &std::collections::BTreeSet<&str>,
) -> Result<()> {
    use ryeos_client_base::surface::view_sets::{
        LayoutSeedSpec, SavedViewMountRef, SavedViewSelectionSource,
    };
    fn refs(layout: &LayoutSeedSpec, out: &mut Vec<String>) {
        match layout {
            LayoutSeedSpec::Group { views, .. } => {
                out.extend(views.iter().map(|view| view.0.clone()))
            }
            LayoutSeedSpec::Split { first, second, .. } => {
                refs(first, out);
                refs(second, out);
            }
        }
    }
    let mut views = Vec::new();
    if let Some(root) = &saved.composition.root {
        refs(root, &mut views);
    }
    let parsed = ryeos_client_base::ui::content::views_from_surface(Some(
        &json!({"views":candidate.presentation.views}),
    ));
    for view in &views {
        if !matches!(
            candidate.compiled_binding.binding.views.get(view),
            Some(ryeos_api::surface_views::EmbeddedViewIdentity::Resolved { .. })
        ) {
            return Err(HandlerError::Forbidden(
                "saved tab contains a View outside its admitted Surface closure".into(),
            )
            .into());
        }
    }
    for relationship in &saved.relationships {
        let SavedViewMountRef::Tile { index } = &relationship.mount;
        let view = views
            .get(*index)
            .and_then(|reference| parsed.get(reference))
            .ok_or_else(|| {
                HandlerError::Forbidden("saved tab mount has no admitted View".into())
            })?;
        match &relationship.source {
            SavedViewSelectionSource::FollowOwnSet => {}
            SavedViewSelectionSource::RequiredSubject { facets, .. } => {
                if ryeos_client_base::ui::attachment::selection_dependencies(view)
                    != facets.iter().cloned().collect()
                {
                    return Err(HandlerError::Conflict(
                        "saved subject differs from current View dependencies".into(),
                    )
                    .into());
                }
            }
            SavedViewSelectionSource::FollowSet { saved_view_set_id }
                if !admitted_collection_ids.contains(saved_view_set_id.as_str()) =>
            {
                return Err(HandlerError::Conflict(
                    "particular Surface follower leaves its exact admitted collection".into(),
                )
                .into());
            }
            SavedViewSelectionSource::FollowSet { .. } => {}
        }
    }
    Ok(())
}

/// Resolve the entire saved collection under one principal config snapshot,
/// then publish one membership after all origin/tab closures pass one final
/// checked generation. No tab is independently published on failure.
pub async fn handle_particular_surface_resume(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let caller = require_seat_caller(&ctx, &state)?;
    let req: ResumeParticularSurfaceRequest = parse_request(params)?;
    if !uuid::Uuid::parse_str(&req.creation_request_id)
        .is_ok_and(|id| id.to_string() == req.creation_request_id)
    {
        return Err(
            HandlerError::BadRequest("creation_request_id must be canonical UUID".into()).into(),
        );
    }
    let (session, origin) = crate::seat_auth::require_compiled_ui_session(&ctx, &state)?;
    let producer = crate::seat_auth::require_compiled_ui_producer()?;
    let ui =
        get_ui_state(&state).ok_or_else(|| HandlerError::Internal("UiState not set".into()))?;
    let membership = ui
        .browser_sessions
        .require_presentation(
            &session.session_id,
            &producer.presentation_id,
            Some(&producer.thread_id),
            None,
            state.node_policy.generation_digest(),
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    ui.browser_sessions
        .require_presentation_attachment(
            &session.session_id,
            &producer.presentation_id,
            &origin.coordinate(),
            state.node_policy.generation_digest(),
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let root = ui
        .browser_sessions
        .resolve_attachment(&session.session_id, &membership.root_coordinate)
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let store = locked_principal_store(&ctx, &state).await?;
    let config = store.load_ui_config()?;
    require_particular_surface_revisions(&config, &req)?;
    let record = config
        .particular_surfaces
        .iter()
        .find(|record| record.id == req.id)
        .cloned()
        .ok_or(HandlerError::NotFound)?;
    let projects = store.load_projects()?;
    let (origin_candidate, origin_project) =
        compile_saved_surface_context(&state, &session, &record.origin, &projects)?;
    let mut candidates = Vec::with_capacity(record.view_set_ids.len());
    let mut proofs = Vec::with_capacity(record.view_set_ids.len());
    let mut saved_sets = Vec::with_capacity(record.view_set_ids.len());
    let admitted_collection_ids = record
        .view_set_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let bounds = super::ui_launch_mint::binding_request_bounds(&state)?;
    use sha2::{Digest, Sha256};
    for id in &record.view_set_ids {
        let saved = config
            .particular_view_sets
            .iter()
            .find(|set| &set.id == id)
            .cloned()
            .ok_or(HandlerError::NotFound)?;
        let context = SavedSurfaceOrigin {
            surface_ref: saved.context.surface_ref.clone(),
            project: saved.context.project.clone(),
        };
        let (candidate, project) =
            compile_saved_surface_context(&state, &session, &context, &projects)?;
        validate_saved_tab_binding(&saved, &candidate, &admitted_collection_ids)?;
        proofs.push(ParticularSurfaceAdmission {
            principal_id: session.principal_id.clone(),
            project,
            receipt: crate::browser_session::ParticularSurfaceReceipt {
                saved_id: saved.id.clone(),
                library_revision: config.particular_view_set_library_revision,
                record_digest: format!("{:x}", Sha256::digest(serde_json::to_vec(&saved)?)),
                surface_ref: context.surface_ref,
                registered_project_id: candidate.registered_project_id.clone(),
                resolved: ryeos_client_base::surface::view_sets::ResolvedParticularViewSet {
                    id: saved.id.clone(),
                    name: saved.name.clone(),
                    composition: saved.composition.clone(),
                    relationships: saved.relationships.clone(),
                    resolved_selection_work: None,
                },
            },
            max_receipt_bytes: usize::try_from(bounds.max_request_bytes)?,
        });
        candidates.push(candidate);
        saved_sets.push(saved);
    }
    let limits = crate::browser_session::PresentationLimits::from_policy(
        state
            .node_policy
            .require::<ryeos_node_runtime::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>(
            )?,
    )?;
    let append_limit = completed_admission_append_limit(&state)?.min(bounds.max_seat_append_bytes);
    let ingress =
        super::ui_seat::read_completed_seat_ingress(state.clone(), &session, &producer).await?;
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let checked = ingress.authorize(&state, &session, &producer)?;
    ui.browser_sessions
        .require_presentation_attachment(
            &session.session_id,
            &producer.presentation_id,
            &origin.coordinate(),
            state.node_policy.generation_digest(),
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let mut admitted_result = None;
    state.engine.with_checked_bundle_generation(|generation|->Result<()> {
        let current=generation.request_engine_generation_identity();
        for attachment in std::iter::once(root.compiled_binding.as_ref()).chain(std::iter::once(origin_candidate.compiled_binding.as_ref()))
            .chain(candidates.iter().map(|candidate|candidate.compiled_binding.as_ref())) {
            if attachment.binding.request_engine_generation_identity!=current {
                return Err(HandlerError::Conflict("UI generation changed during saved Surface resume".into()).into());
            }
        }
        let root_resolution=root.project_authority.as_ref().map(|authority|authority.descriptor_path()).transpose()?;
        super::ui_launch_mint::revalidate_binding_authority(generation,root.compiled_binding.as_ref(),root_resolution.as_deref(),root.project_authority.as_deref())?;
        for candidate in std::iter::once(&origin_candidate).chain(&candidates) {
            let resolution=candidate.project_authority.as_ref().map(|authority|authority.descriptor_path()).transpose()?;
            super::ui_launch_mint::revalidate_binding_authority(generation,candidate.compiled_binding.as_ref(),resolution.as_deref(),candidate.project_authority.as_deref())?;
        }
        for ((saved,proof),candidate) in saved_sets.iter().zip(&mut proofs).zip(&candidates) {
            proof.receipt.resolved.resolved_selection_work=saved.context.work.as_ref().map(|work|
                resolve_particular_work(&state,caller.principal_id(),candidate.project_authority.as_ref().map(|authority|authority.path()),&work.chain_root_id)
                    .and_then(|value|serde_json::from_value(value).map_err(Into::into))).transpose()?;
        }
        let receipt=crate::browser_session::SavedSurfaceRequestIdentity {
            provenance: ryeos_client_base::ui::binding::UiSurfaceCompositionProvenance::Library {
                saved_id:record.id.clone(),library_revision:req.expected_revision,
                record_digest:format!("{:x}",Sha256::digest(serde_json::to_vec(&json!({"record":record,"sets":saved_sets}))?)),
            },
            title:record.title.clone(),origin:record.origin.clone(),active_view_set_id:record.active_view_set_id.clone(),
            body:crate::browser_session::SavedSurfaceRequestBody::Particular { tabs:proofs.iter().map(|proof|proof.receipt.clone()).collect() },
        };
        let admission=SavedSurfaceAdmission::particular(session.principal_id.clone(),receipt,origin_project,proofs)?;
        let published = state.state_store.with_exact_seat_action_if_mixed_cut(&checked.seat(), &checked.cut(), || {
        ui.browser_sessions.reconcile_presentation_mount_frontier(
            &session.session_id, &producer.presentation_id, &membership.root_coordinate,
            state.node_policy.generation_digest(), ingress.next_mount_id(),
        ).map_err(|error| HandlerError::Conflict(error.to_string()))?;
        ui.browser_sessions.publish_saved_surface(&session.session_id,&producer.presentation_id,&req.creation_request_id,
            &membership.root_coordinate,origin_candidate,candidates,&admission,
            &limits,state.node_policy.generation_digest(),
            |surface,origin,tabs| {
                let retained=surface.saved_resume.as_ref().ok_or(crate::browser_session::AttachmentStoreError::PresentationInvalid)?;
                let selection=retained.particular_selection().ok_or(crate::browser_session::AttachmentStoreError::PresentationInvalid)?;
                let result=json!({"particular_surface":retained.public_descriptor(),"particular_selection":selection,
                    "ui_transition":{"kind":"resume_particular_surface","presentation_id":producer.presentation_id,
                        "surface":surface.public_descriptor(),"attachment":origin.public_descriptor(bounds),
                        "tab_attachments":tabs.iter().map(|attachment|attachment.public_descriptor(bounds)).collect::<Vec<_>>(),
                        "creation_request_id":req.creation_request_id}});
                validate_completed_admission_size(&result,&producer,&req.creation_request_id,bounds.max_request_bytes,append_limit)?;
                admitted_result=Some(result);Ok(())
            }).map_err(|error| anyhow::Error::from(HandlerError::Conflict(error.to_string())))
            })?;
        if published.is_none() {
            return Err(HandlerError::Conflict("seat action anchor changed before publication".into()).into());
        }
        Ok(())
    })?;
    admitted_result.ok_or_else(|| {
        HandlerError::Internal("saved Surface publication result missing".into()).into()
    })
}

fn require_particular_surface_revisions(
    config: &RyeOsConfigFile,
    req: &ResumeParticularSurfaceRequest,
) -> Result<()> {
    if config.particular_surface_library_revision != req.expected_revision {
        return Err(
            HandlerError::Conflict("particular Surface library revision changed".into()).into(),
        );
    }
    if config.particular_view_set_library_revision != req.view_set_expected_revision {
        return Err(
            HandlerError::Conflict("particular view-set library revision changed".into()).into(),
        );
    }
    Ok(())
}

pub async fn handle_projects_list(
    _params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let caller = require_seat_caller(&ctx, &state)?;
    let retained_project = caller
        .project_query_identity()?
        .map(|path| path.to_string_lossy().into_owned());
    let current_project = retained_project.as_deref();
    let store = resolve_principal_store(&ctx, &state)?;
    let projects = store.load_projects()?;
    let mut rows = projects.projects;
    if let Some(current) = current_project
        && !rows
            .iter()
            .any(|project| same_existing_dir(current, &project.root))
    {
        let root = PathBuf::from(current);
        rows.insert(
            0,
            ProjectEntry {
                local_id: "current".to_string(),
                name: inferred_project_name(&root),
                root: current.to_string(),
                added_at: String::new(),
                tags: Vec::new(),
            },
        );
    }
    Ok(json!({
        "version": projects.version,
        "projects": rows.into_iter().map(|project| {
            let registered = project.local_id != "current";
            project_view(project, current_project, registered)
        }).collect::<Vec<_>>()
    }))
}

pub async fn handle_projects_add(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    require_seat_caller(&ctx, &state)?;
    let req: AddProjectRequest = parse_request(params)?;
    let root = canonical_project_root(&req.root)?;
    let root_text = root.display().to_string();
    let store = locked_principal_store(&ctx, &state).await?;
    let mut projects = store.load_projects()?;

    if let Some(existing) = projects.projects.iter_mut().find(|p| p.root == root_text) {
        if let Some(name) = req.name.filter(|s| !s.trim().is_empty()) {
            existing.name = name;
        }
        if let Some(tags) = req.tags {
            existing.tags = tags;
        }
        let entry = existing.clone();
        store.write_projects(&projects)?;
        return Ok(
            json!({ "project": project_view(entry, Some(&root_text), true), "created": false }),
        );
    }

    let entry = ProjectEntry {
        local_id: format!("prj_{}", uuid::Uuid::new_v4().simple()),
        name: req
            .name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| inferred_project_name(&root)),
        root: root_text.clone(),
        added_at: lillux::time::iso8601_now(),
        tags: req.tags.unwrap_or_default(),
    };
    projects.projects.push(entry.clone());
    projects.projects.sort_by(|a, b| a.name.cmp(&b.name));
    store.write_projects(&projects)?;

    Ok(json!({ "project": project_view(entry, Some(&root_text), true), "created": true }))
}

pub async fn handle_projects_forget(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    require_seat_caller(&ctx, &state)?;
    let req: ForgetProjectRequest = parse_request(params)?;
    let local_id = req.local_id.as_deref().ok_or_else(|| {
        HandlerError::BadRequest("registered local_id is required for project forget".into())
    })?;

    let store = locked_principal_store(&ctx, &state).await?;
    let mut projects = store.load_projects()?;
    if !projects
        .projects
        .iter()
        .any(|project| project.local_id == local_id)
    {
        return Ok(json!({"removed": 0}));
    }
    let principal_id = retained_principal_id(&state)?;
    if get_ui_state(&state)
        .ok_or_else(|| HandlerError::Internal("UiState not set".into()))?
        .browser_sessions
        .has_retained_attachment_for_project(&principal_id, local_id)
    {
        return Err(HandlerError::Structured {
            code: "project_in_use".into(),
            status: 409,
            body: json!({"code":"project_in_use","local_id":local_id}),
        }
        .into());
    }
    let before = projects.projects.len();
    projects
        .projects
        .retain(|project| project.local_id != local_id);
    let removed = before - projects.projects.len();
    store.write_projects(&projects)?;

    let mut recent = store.load_recent()?;
    recent.recent_projects.retain(|r| {
        projects
            .projects
            .iter()
            .any(|project| project.local_id == r.local_id)
    });
    store.write_recent(&recent)?;

    Ok(json!({ "removed": removed }))
}

pub async fn handle_projects_resolve(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    require_seat_caller(&ctx, &state)?;
    let req: ResolveProjectRequest = parse_request(params)?;
    let store = resolve_principal_store(&ctx, &state)?;
    let projects = store.load_projects()?;
    let project = projects
        .projects
        .into_iter()
        .find(|p| p.local_id == req.local_id)
        .ok_or(HandlerError::NotFound)?;
    Ok(json!({ "project": project_view(project, None, true) }))
}

pub async fn handle_projects_open(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    require_seat_caller(&ctx, &state)?;
    let req: OpenProjectRequest = parse_request(params)?;
    if !matches!(
        &req.placement,
        ryeos_client_base::surface::opening::SurfaceOpenPlacement::NewSurface
    ) {
        return Err(HandlerError::BadRequest(
            "registered-project opening requires new_surface placement".into(),
        )
        .into());
    }
    let surface_ref = match &req.selection {
        ryeos_client_base::surface::opening::SurfaceOpenSelection::Authored { surface_ref } => {
            surface_ref.clone()
        }
        ryeos_client_base::surface::opening::SurfaceOpenSelection::Reusable { .. }
        | ryeos_client_base::surface::opening::SurfaceOpenSelection::Composed(_) => {
            return Err(HandlerError::BadRequest("registered-project opening requires an authored Surface; reusable Surfaces use their retained origins".into()).into());
        }
    };
    ryeos_client_base::surface::opening::SurfaceOpenRequest {
        creation_request_id: req.creation_request_id.clone(),
        selection: req.selection.clone(),
        placement: req.placement.clone(),
    }
    .validate()
    .map_err(HandlerError::BadRequest)?;
    if uuid::Uuid::parse_str(&req.creation_request_id)
        .ok()
        .map(|value| value.to_string())
        .as_deref()
        != Some(req.creation_request_id.as_str())
    {
        return Err(HandlerError::BadRequest(
            "creation_request_id must be a canonical UUID".into(),
        )
        .into());
    }
    let (session, origin) = crate::seat_auth::require_compiled_ui_session(&ctx, &state)?;
    let producer = crate::seat_auth::require_compiled_ui_producer()?;
    let ui =
        get_ui_state(&state).ok_or_else(|| HandlerError::Internal("UiState not set".into()))?;
    let membership = ui
        .browser_sessions
        .require_presentation(
            &session.session_id,
            &producer.presentation_id,
            Some(&producer.thread_id),
            None,
            state.node_policy.generation_digest(),
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    // The dispatch attachment must itself belong to this exact presentation.
    ui.browser_sessions
        .require_presentation_attachment(
            &session.session_id,
            &producer.presentation_id,
            &origin.coordinate(),
            state.node_policy.generation_digest(),
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let root_attachment = ui
        .browser_sessions
        .resolve_attachment(&session.session_id, &membership.root_coordinate)
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let project = {
        let store = locked_principal_store(&ctx, &state).await?;
        store
            .load_projects()?
            .projects
            .into_iter()
            .find(|entry| entry.local_id == req.local_id)
            .ok_or(HandlerError::NotFound)?
    };
    let canonical = canonical_project_root(&project.root)?;
    let root = canonical.display().to_string();
    let project_authority = Arc::new(
        lillux::PinnedDirectory::open(&canonical)?
            .context("selected UI project root disappeared")?,
    );
    let compile_context = HandlerContext::new(
        session.principal_id.clone(),
        session.granted_caps.clone(),
        true,
    );
    let compile_request = super::ui_launch_mint::Request {
        ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.into(),
        surface_ref,
        project_path: Some(root.clone()),
        user_principal_id: None,
    };
    let (compiled_binding, presentation) = super::ui_launch_mint::compile_session_binding(
        &compile_request,
        &compile_context,
        &state,
        Some(project_authority.as_ref()),
    )?;
    // Registry -> seat-transition -> session-store is the one publication order.
    // Retain the registry gate through the atomic membership/attachment commit.
    let store = locked_principal_store(&ctx, &state).await?;
    if !store
        .load_projects()?
        .projects
        .into_iter()
        .any(|entry| entry.local_id == project.local_id && entry.root == project.root)
    {
        return Err(
            HandlerError::Conflict("project registration changed during open".into()).into(),
        );
    }
    project_authority.ensure_path_binding()?;
    let admission = RegisteredProjectSurfaceAdmission {
        principal_id: session.principal_id.clone(),
        local_id: project.local_id.clone(),
        root: root.clone(),
        authority: project_authority.clone(),
    };
    let limits = crate::browser_session::PresentationLimits::from_policy(
        state
            .node_policy
            .require::<ryeos_node_runtime::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>(
            )?,
    )?;
    let request_bounds = super::ui_launch_mint::binding_request_bounds(&state)?;
    let append_limit = completed_admission_append_limit(&state)?;
    let mut admitted_result = None;
    let root_resolution = root_attachment
        .project_authority
        .as_ref()
        .map(|authority| authority.descriptor_path())
        .transpose()?;
    let project_resolution = project_authority.descriptor_path()?;
    let ingress =
        super::ui_seat::read_completed_seat_ingress(state.clone(), &session, &producer).await?;
    let _publication = {
        let _transition = ui
            .lock_seat_transition()
            .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
        let checked = ingress.authorize(&state, &session, &producer)?;
        ui.browser_sessions
            .require_presentation_attachment(
                &session.session_id,
                &producer.presentation_id,
                &origin.coordinate(),
                state.node_policy.generation_digest(),
            )
            .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
        state
            .engine
            .with_checked_bundle_generation(|generation| -> Result<_> {
                let current = generation.request_engine_generation_identity();
                if current
                    != root_attachment
                        .compiled_binding
                        .binding
                        .request_engine_generation_identity
                    || current != compiled_binding.binding.request_engine_generation_identity
                {
                    return Err(HandlerError::Conflict(
                        "UI engine generation changed during project Surface admission".into(),
                    )
                    .into());
                }
                super::ui_launch_mint::revalidate_binding_authority(
                    generation,
                    root_attachment.compiled_binding.as_ref(),
                    root_resolution.as_deref(),
                    root_attachment.project_authority.as_deref(),
                )?;
                super::ui_launch_mint::revalidate_binding_authority(
                    generation,
                    &compiled_binding,
                    Some(project_resolution.as_path()),
                    Some(project_authority.as_ref()),
                )?;
                let published = state.state_store.with_exact_seat_action_if_mixed_cut(&checked.seat(), &checked.cut(), || {
                ui.browser_sessions.reconcile_presentation_mount_frontier(
                    &session.session_id, &producer.presentation_id, &membership.root_coordinate,
                    state.node_policy.generation_digest(), ingress.next_mount_id(),
                ).map_err(|error| HandlerError::Conflict(error.to_string()))?;
                ui.browser_sessions
                    .publish_registered_project_surface(
                        &session.session_id,
                        &producer.presentation_id,
                        &req.creation_request_id,
                        &membership.root_coordinate,
                        crate::browser_session::BindingAttachmentCandidate {
                            registered_project_id: Some(project.local_id.clone()),
                            compiled_binding: Arc::new(compiled_binding),
                            presentation,
                            project_authority: Some(project_authority.clone()),
                        },
                        &limits,
                        state.node_policy.generation_digest(),
                        &admission,
                        |surface, attachment| {
                            let result = json!({
                                "project":project_view(ProjectEntry {root:root.clone(),..project.clone()},Some(&root),true),
                                "ui_transition":{"kind":"open_surface","presentation_id":producer.presentation_id,
                                    "opening":{"creation_request_id":req.creation_request_id,"selection":req.selection,"placement":req.placement},
                                    "creation_request_id":req.creation_request_id,"tab_attachments":[],
                                    "surface":surface.public_descriptor(),"attachment":attachment.public_descriptor(request_bounds)}
                            });
                            validate_completed_admission_size(&result,&producer,&req.creation_request_id,
                                request_bounds.max_request_bytes,append_limit.min(request_bounds.max_seat_append_bytes))?;
                            admitted_result = Some(result);
                            Ok(())
                        },
                    )
                    .map_err(|error| anyhow::Error::from(HandlerError::Conflict(error.to_string())))
                    })?;
                published.ok_or_else(|| anyhow::Error::from(HandlerError::Conflict("seat action anchor changed before publication".into())))
            })?
    };
    // Recent metadata cannot undo an already committed admission.
    if let Err(error) = store.touch_recent_project(&project.local_id) {
        tracing::warn!(%error, local_id = %project.local_id, "Surface admitted but recent project history was not updated");
    }
    admitted_result
        .ok_or_else(|| HandlerError::Internal("Surface publication result missing".into()).into())
}

/// Resume one durable particular view set without reviving any prior session
/// authority. Stable project/work identities are resolved again; a project
/// context receives a newly admitted Surface; projectless work compiles that
/// Surface explicitly without project authority, never on the invoking root.
pub async fn handle_particular_view_set_resume(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let caller = require_seat_caller(&ctx, &state)?;
    let req: ResumeParticularViewSetRequest = parse_request(params)?;
    if uuid::Uuid::parse_str(&req.creation_request_id)
        .ok()
        .map(|id| id.to_string())
        .as_deref()
        != Some(req.creation_request_id.as_str())
    {
        return Err(HandlerError::BadRequest(
            "creation_request_id must be a canonical UUID".into(),
        )
        .into());
    }
    let (session, origin) = crate::seat_auth::require_compiled_ui_session(&ctx, &state)?;
    let producer = crate::seat_auth::require_compiled_ui_producer()?;
    let ui =
        get_ui_state(&state).ok_or_else(|| HandlerError::Internal("UiState not set".into()))?;
    let membership = ui
        .browser_sessions
        .require_presentation(
            &session.session_id,
            &producer.presentation_id,
            Some(&producer.thread_id),
            None,
            state.node_policy.generation_digest(),
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    ui.browser_sessions
        .require_presentation_attachment(
            &session.session_id,
            &producer.presentation_id,
            &origin.coordinate(),
            state.node_policy.generation_digest(),
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let root_attachment = ui
        .browser_sessions
        .resolve_attachment(&session.session_id, &membership.root_coordinate)
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let (resume, project) = {
        let store = locked_principal_store(&ctx, &state).await?;
        let config = store.load_ui_config()?;
        if config.particular_view_set_library_revision != req.expected_revision {
            return Err(HandlerError::Conflict(
                "particular-view-set library revision changed".into(),
            )
            .into());
        }
        let resume = config
            .particular_view_sets
            .into_iter()
            .find(|entry| entry.id == req.id)
            .ok_or(HandlerError::NotFound)?;
        validate_particular_view_set_resumes(std::slice::from_ref(&resume))
            .map_err(HandlerError::BadRequest)?;
        if req.surface_ref != resume.context.surface_ref {
            return Err(HandlerError::Conflict(
                "requested Surface differs from the saved logical Surface".into(),
            )
            .into());
        }
        let projects = store.load_projects()?;
        let project = resume
            .context
            .project
            .as_ref()
            .map(|project| {
                projects
                    .projects
                    .into_iter()
                    .find(|entry| entry.local_id == project.local_id)
                    .ok_or(HandlerError::NotFound)
            })
            .transpose()?;
        (resume, project)
    };
    let canonical_project = project
        .as_ref()
        .map(|project| canonical_project_root(&project.root))
        .transpose()?;
    let project_authority = canonical_project
        .as_ref()
        .map(|canonical| {
            lillux::PinnedDirectory::open(canonical)
                .and_then(|directory| directory.context("particular project disappeared"))
                .map(Arc::new)
        })
        .transpose()?;
    let resolved_work = resume
        .context
        .work
        .as_ref()
        .map(|work| {
            resolve_particular_work(
                &state,
                caller.principal_id(),
                canonical_project.as_deref(),
                &work.chain_root_id,
            )
            .and_then(|value| serde_json::from_value(value).map_err(Into::into))
        })
        .transpose()?;
    let resolved = ryeos_client_base::surface::view_sets::ResolvedParticularViewSet {
        id: resume.id.clone(),
        name: resume.name.clone(),
        composition: resume.composition.clone(),
        relationships: resume.relationships.clone(),
        resolved_selection_work: resolved_work,
    };
    use sha2::{Digest, Sha256};
    let receipt = crate::browser_session::ParticularSurfaceReceipt {
        saved_id: resume.id.clone(),
        library_revision: req.expected_revision,
        record_digest: format!("{:x}", Sha256::digest(serde_json::to_vec(&resume)?)),
        surface_ref: req.surface_ref.clone(),
        registered_project_id: project.as_ref().map(|project| project.local_id.clone()),
        resolved,
    };
    let bounds = super::ui_launch_mint::binding_request_bounds(&state)?;
    let append_limit = completed_admission_append_limit(&state)?;
    let mut admitted_result = None;
    if serde_json::to_vec(&receipt)?.len() > usize::try_from(bounds.max_request_bytes)? {
        return Err(HandlerError::BadRequest(
            "particular resume receipt exceeds current request bounds".into(),
        )
        .into());
    }
    let compile_request = super::ui_launch_mint::Request {
        ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.into(),
        surface_ref: req.surface_ref,
        project_path: canonical_project
            .as_ref()
            .map(|root| root.display().to_string()),
        user_principal_id: None,
    };
    let compile_context = HandlerContext::new(
        session.principal_id.clone(),
        session.granted_caps.clone(),
        true,
    );
    let (binding, presentation) = super::ui_launch_mint::compile_session_binding(
        &compile_request,
        &compile_context,
        &state,
        project_authority.as_deref(),
    )?;
    if resume.relationships.iter().any(|relationship| {
        matches!(
            relationship.source,
            ryeos_client_base::surface::view_sets::SavedViewSelectionSource::FollowSet { .. }
        )
    }) {
        return Err(HandlerError::Conflict("particular resume cannot prove a saved-set follower until retained composition admission is available".into()).into());
    }
    fn admitted_layout(
        root: &ryeos_client_base::surface::view_sets::LayoutSeedSpec,
        views: &std::collections::BTreeMap<String, ryeos_api::surface_views::EmbeddedViewIdentity>,
    ) -> bool {
        use ryeos_client_base::surface::view_sets::LayoutSeedSpec;
        match root {
            LayoutSeedSpec::Group { views: refs, .. } => refs.iter().all(|reference| {
                matches!(
                    views.get(&reference.0),
                    Some(ryeos_api::surface_views::EmbeddedViewIdentity::Resolved { .. })
                )
            }),
            LayoutSeedSpec::Split { first, second, .. } => {
                admitted_layout(first, views) && admitted_layout(second, views)
            }
        }
    }
    if resume
        .composition
        .root
        .as_ref()
        .is_some_and(|root| !admitted_layout(root, &binding.binding.views))
    {
        return Err(HandlerError::Forbidden(
            "particular composition contains a View outside the selected Surface binding".into(),
        )
        .into());
    }
    fn layout_refs(
        root: &ryeos_client_base::surface::view_sets::LayoutSeedSpec,
        refs: &mut Vec<String>,
    ) {
        use ryeos_client_base::surface::view_sets::LayoutSeedSpec;
        match root {
            LayoutSeedSpec::Group { views, .. } => {
                refs.extend(views.iter().map(|view| view.0.clone()))
            }
            LayoutSeedSpec::Split { first, second, .. } => {
                layout_refs(first, refs);
                layout_refs(second, refs);
            }
        }
    }
    let mut refs = Vec::new();
    if let Some(root) = &resume.composition.root {
        layout_refs(root, &mut refs);
    }
    let parsed_views = ryeos_client_base::ui::content::views_from_surface(Some(
        &json!({"views":presentation.views}),
    ));
    for relationship in &resume.relationships {
        use ryeos_client_base::surface::view_sets::{SavedViewMountRef, SavedViewSelectionSource};
        let SavedViewMountRef::Tile { index } = &relationship.mount;
        let view = refs
            .get(*index)
            .and_then(|reference| parsed_views.get(reference))
            .ok_or_else(|| HandlerError::Forbidden("saved mount has no admitted View".into()))?;
        let expected = ryeos_client_base::ui::attachment::selection_dependencies(view);
        let declared = match &relationship.source {
            SavedViewSelectionSource::RequiredSubject { facets, .. } => {
                facets.iter().cloned().collect()
            }
            SavedViewSelectionSource::FollowOwnSet => continue,
            SavedViewSelectionSource::FollowSet { .. } => unreachable!("rejected before admission"),
        };
        if expected != declared {
            return Err(HandlerError::Conflict(
                "saved subject declaration differs from the current View dependencies".into(),
            )
            .into());
        }
    }
    let store = locked_principal_store(&ctx, &state).await?;
    let config = store.load_ui_config()?;
    if config.particular_view_set_library_revision != req.expected_revision
        || !config
            .particular_view_sets
            .iter()
            .any(|entry| entry == &resume)
    {
        return Err(
            HandlerError::Conflict("particular saved record changed during resume".into()).into(),
        );
    }
    if let Some(project) = &project
        && !store
            .load_projects()?
            .projects
            .iter()
            .any(|entry| entry.local_id == project.local_id && entry.root == project.root)
    {
        return Err(HandlerError::Conflict(
            "particular project registration changed during resume".into(),
        )
        .into());
    }
    if let Some(authority) = &project_authority {
        authority.ensure_path_binding()?;
    }
    let mut proof = ParticularSurfaceAdmission {
        principal_id: session.principal_id.clone(),
        project: match (&project, &canonical_project, &project_authority) {
            (Some(project), Some(root), Some(authority)) => {
                Some(RegisteredProjectSurfaceAdmission {
                    principal_id: session.principal_id.clone(),
                    local_id: project.local_id.clone(),
                    root: root.display().to_string(),
                    authority: authority.clone(),
                })
            }
            (None, None, None) => None,
            _ => {
                return Err(
                    HandlerError::Internal("particular project authority mismatch".into()).into(),
                );
            }
        },
        receipt,
        max_receipt_bytes: usize::try_from(bounds.max_request_bytes)?,
    };
    let root_resolution = root_attachment
        .project_authority
        .as_ref()
        .map(|authority| authority.descriptor_path())
        .transpose()?;
    let project_resolution = project_authority
        .as_ref()
        .map(|authority| authority.descriptor_path())
        .transpose()?;
    let ingress =
        super::ui_seat::read_completed_seat_ingress(state.clone(), &session, &producer).await?;
    let limits = crate::browser_session::PresentationLimits::from_policy(
        state
            .node_policy
            .require::<ryeos_node_runtime::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>(
            )?,
    )?;
    let _publication = {
        let _transition = ui
            .lock_seat_transition()
            .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
        let checked = ingress.authorize(&state, &session, &producer)?;
        ui.browser_sessions
            .require_presentation_attachment(
                &session.session_id,
                &producer.presentation_id,
                &origin.coordinate(),
                state.node_policy.generation_digest(),
            )
            .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
        proof.receipt.resolved.resolved_selection_work = resume
            .context
            .work
            .as_ref()
            .map(|work| {
                resolve_particular_work(
                    &state,
                    caller.principal_id(),
                    canonical_project.as_deref(),
                    &work.chain_root_id,
                )
                .and_then(|value| serde_json::from_value(value).map_err(Into::into))
            })
            .transpose()?;
        state
            .engine
            .with_checked_bundle_generation(|generation| -> Result<_> {
                let current = generation.request_engine_generation_identity();
                if current
                    != root_attachment
                        .compiled_binding
                        .binding
                        .request_engine_generation_identity
                    || current != binding.binding.request_engine_generation_identity
                {
                    return Err(HandlerError::Conflict(
                        "UI engine generation changed during particular resume".into(),
                    )
                    .into());
                }
                super::ui_launch_mint::revalidate_binding_authority(
                    generation,
                    root_attachment.compiled_binding.as_ref(),
                    root_resolution.as_deref(),
                    root_attachment.project_authority.as_deref(),
                )?;
                super::ui_launch_mint::revalidate_binding_authority(
                    generation,
                    &binding,
                    project_resolution.as_deref(),
                    project_authority.as_deref(),
                )?;
                let published = state.state_store.with_exact_seat_action_if_mixed_cut(&checked.seat(), &checked.cut(), || {
                ui.browser_sessions.reconcile_presentation_mount_frontier(
                    &session.session_id, &producer.presentation_id, &membership.root_coordinate,
                    state.node_policy.generation_digest(), ingress.next_mount_id(),
                ).map_err(|error| HandlerError::Conflict(error.to_string()))?;
                ui.browser_sessions
                    .publish_particular_surface(
                        &session.session_id,
                        &producer.presentation_id,
                        &req.creation_request_id,
                        &membership.root_coordinate,
                        crate::browser_session::BindingAttachmentCandidate {
                            registered_project_id: project
                                .as_ref()
                                .map(|project| project.local_id.clone()),
                            compiled_binding: Arc::new(binding),
                            presentation,
                            project_authority: project_authority.clone(),
                        },
                        &limits,
                        state.node_policy.generation_digest(),
                        &proof,
                        |surface, attachment| {
                            let retained = surface.particular_resume.as_ref()
                                .ok_or(crate::browser_session::AttachmentStoreError::PresentationInvalid)?;
                            let result = json!({"particular_view_set":retained.resolved,
                                "particular_selection":retained.particular_selection(),
                                "ui_transition":{"kind":"resume_particular_view_set","presentation_id":producer.presentation_id,
                                    "surface":surface.public_descriptor(),"attachment":attachment.public_descriptor(bounds),
                                    "creation_request_id":req.creation_request_id}});
                            validate_completed_admission_size(&result,&producer,&req.creation_request_id,
                                bounds.max_request_bytes,append_limit.min(bounds.max_seat_append_bytes))?;
                            admitted_result = Some(result);
                            Ok(())
                        },
                    )
                    .map_err(|error| anyhow::Error::from(HandlerError::Conflict(error.to_string())))
                    })?;
                published.ok_or_else(|| anyhow::Error::from(HandlerError::Conflict("seat action anchor changed before publication".into())))
            })?
    };
    admitted_result.ok_or_else(|| {
        HandlerError::Internal("particular publication result missing".into()).into()
    })
}

fn resolve_particular_work(
    state: &AppState,
    principal_id: &str,
    project_root: Option<&Path>,
    chain_root_id: &str,
) -> Result<Value> {
    let lineage = state.threads.continuation_lineage(chain_root_id)?;
    let root = lineage.first().ok_or(HandlerError::NotFound)?;
    let head = lineage.last().ok_or(HandlerError::NotFound)?;
    if root.thread.thread_id != chain_root_id {
        return Err(HandlerError::NotFound.into());
    }
    for placement in &lineage {
        if placement.thread.chain_root_id != chain_root_id
            || placement.thread.requested_by.as_deref() != Some(principal_id)
        {
            return Err(HandlerError::NotFound.into());
        }
        match (project_root, placement.thread.project_root.as_deref()) {
            (Some(expected), Some(actual))
                if same_existing_dir(expected.to_string_lossy().as_ref(), actual) => {}
            (None, None) => {}
            _ => return Err(HandlerError::NotFound.into()),
        }
    }
    Ok(json!({
        "thread": head.thread.thread_id,
        "chain_root": chain_root_id,
    }))
}

pub async fn handle_recent_touch(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    require_seat_caller(&ctx, &state)?;
    let req: TouchRecentRequest = parse_request(params)?;
    let store = locked_principal_store(&ctx, &state).await?;
    let projects = store.load_projects()?;
    if !projects.projects.iter().any(|p| p.local_id == req.local_id) {
        return Err(HandlerError::NotFound.into());
    }

    let recent = store.touch_recent_project(&req.local_id)?;
    Ok(json!({ "recent": recent.recent_projects }))
}

pub async fn handle_recent_list(
    _params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    require_seat_caller(&ctx, &state)?;
    let store = resolve_principal_store(&ctx, &state)?;
    let recent = store.load_recent()?;
    Ok(json!(recent))
}

pub async fn handle_config_get(
    _params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    require_seat_caller(&ctx, &state)?;
    let store = resolve_principal_store(&ctx, &state)?;
    let config = store.load_ui_config()?;
    let registered_project_id = crate::seat_auth::compiled_ui_attachment()
        .and_then(|attachment| attachment.registered_project_id.clone());
    let mut response = json!(config);
    // Each ordinary selection row carries the revision actually observed by
    // this source and its saved logical Surface. Neither is ambient authority.
    if let Some(rows) = response["particular_view_sets"].as_array_mut() {
        for row in rows {
            row["expected_revision"] = json!(config.particular_view_set_library_revision);
            row["surface_ref"] = row["context"]["surface_ref"].clone();
        }
    }
    if let Some(rows) = response["particular_surfaces"].as_array_mut() {
        for row in rows {
            row["expected_revision"] = json!(config.particular_surface_library_revision);
            row["view_set_expected_revision"] = json!(config.particular_view_set_library_revision);
        }
    }
    if let Some(rows) = response["reusable_surfaces"].as_array_mut() {
        for row in rows {
            row["expected_revision"] = json!(config.reusable_surface_library_revision);
            // Missing fresh subjects intentionally reopen as unresolved; this
            // source projection neither restores old values nor grants access.
            row["fresh_subjects"] = json!({});
        }
    }
    // Response-only row for the ordinary sections renderer. The durable file
    // remains the bounded config contract, with no duplicated library state.
    response["view_set_save_context"] = json!([{
        "label": "Save current view set",
        "description": "Uses the current set name; rename it to save another composition",
        "context": {
            "expected_revision": config.view_set_library_revision,
            "saved_view_sets": config.saved_view_sets
        }
    }]);
    response["particular_view_set_save_context"] = json!([{
        "label": "Retain this particular view set",
        "description": "Stores stable project and logical-work references; authority and live placement are resolved again on resume",
        "context": {
            "expected_revision": config.particular_view_set_library_revision,
            "particular_view_sets": config.particular_view_sets,
            "project_local_id": registered_project_id
        }
    }]);
    response["reusable_surface_save_context"] = json!([{
        "label": "Save current Surface",
        "description": "Captures its ordered tabs and original logical origin; subjects are supplied afresh when reused",
        "context": {
            "expected_revision": config.reusable_surface_library_revision,
            "reusable_surfaces": config.reusable_surfaces
        }
    }]);
    response["particular_surface_save_context"] = json!([{
        "label": "Retain current Surface",
        "description": "Atomically retains all ordered particular tabs with their independent logical subjects",
        "context": {
            "expected_revision": config.particular_surface_library_revision,
            "particular_surfaces": config.particular_surfaces,
            "view_set_expected_revision": config.particular_view_set_library_revision,
            "particular_view_sets": config.particular_view_sets
        }
    }]);
    Ok(response)
}

pub async fn handle_config_update(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    require_seat_caller(&ctx, &state)?;
    let req: UpdateConfigRequest = parse_request(params)?;
    let store = locked_principal_store(&ctx, &state).await?;
    let mut config = store.load_ui_config()?;
    apply_config_update(&mut config, req)?;
    store.write_ui_config(&config)?;
    Ok(json!(config))
}

fn apply_config_update(config: &mut RyeOsConfigFile, req: UpdateConfigRequest) -> Result<()> {
    // Validate the complete proposed file before replacing any library. This
    // includes references to particular sets updated in the same transaction,
    // and checked revision increments across independent namespaces.
    let mut staged = config.clone();
    apply_config_update_staged(&mut staged, req)?;
    *config = staged;
    Ok(())
}

fn apply_config_update_staged(
    config: &mut RyeOsConfigFile,
    req: UpdateConfigRequest,
) -> Result<()> {
    if let Some(theme) = req.theme.as_deref() {
        validate_choice("theme", theme, &["system", "light", "dark"])?;
    }
    if let Some(landing_view) = req.landing_view.as_deref() {
        validate_choice("landing_view", landing_view, &["projects"])?;
    }
    if let Some(update) = &req.view_set_library {
        validate_saved_view_set_templates(&update.saved_view_sets)
            .map_err(HandlerError::BadRequest)?;
        if update.expected_revision != config.view_set_library_revision {
            return Err(HandlerError::Conflict(format!(
                "view-set library revision advanced: expected {}, current {}",
                update.expected_revision, config.view_set_library_revision
            ))
            .into());
        }
    }
    if let Some(update) = &req.particular_view_set_library {
        validate_particular_view_set_resumes(&update.particular_view_sets)
            .map_err(HandlerError::BadRequest)?;
        if update.expected_revision != config.particular_view_set_library_revision {
            return Err(HandlerError::Conflict(format!(
                "particular-view-set library revision advanced: expected {}, current {}",
                update.expected_revision, config.particular_view_set_library_revision
            ))
            .into());
        }
    }
    if let Some(update) = &req.reusable_surface_library
        && update.expected_revision != config.reusable_surface_library_revision
    {
        return Err(
            HandlerError::Conflict("reusable-surface library revision advanced".into()).into(),
        );
    }
    if let Some(update) = &req.particular_surface_library
        && update.expected_revision != config.particular_surface_library_revision
    {
        return Err(
            HandlerError::Conflict("particular-surface library revision advanced".into()).into(),
        );
    }
    if let Some(theme) = req.theme {
        config.theme = theme;
    }
    if let Some(landing_view) = req.landing_view {
        config.landing_view = landing_view;
    }
    if let Some(update) = req.view_set_library {
        config.view_set_library_revision = config
            .view_set_library_revision
            .checked_add(1)
            .ok_or_else(|| HandlerError::Conflict("view-set library revision exhausted".into()))?;
        config.saved_view_sets = update.saved_view_sets;
    }
    if let Some(update) = req.particular_view_set_library {
        config.particular_view_set_library_revision = config
            .particular_view_set_library_revision
            .checked_add(1)
            .ok_or_else(|| {
                HandlerError::Conflict("particular-view-set library revision exhausted".into())
            })?;
        config.particular_view_sets = update.particular_view_sets;
    }
    if let Some(update) = req.reusable_surface_library {
        config.reusable_surface_library_revision = config
            .reusable_surface_library_revision
            .checked_add(1)
            .ok_or_else(|| {
                HandlerError::Conflict("reusable-surface library revision exhausted".into())
            })?;
        config.reusable_surfaces = update.reusable_surfaces;
    }
    if let Some(update) = req.particular_surface_library {
        config.particular_surface_library_revision = config
            .particular_surface_library_revision
            .checked_add(1)
            .ok_or_else(|| {
                HandlerError::Conflict("particular-surface library revision exhausted".into())
            })?;
        config.particular_surfaces = update.particular_surfaces;
    }
    validate_ui_config(config)
}

fn canonical_project_root(root: &str) -> Result<PathBuf> {
    let path = PathBuf::from(root);
    if !path.is_absolute() {
        return Err(HandlerError::BadRequest("project root must be absolute".into()).into());
    }
    let canonical = path
        .canonicalize()
        .map_err(|e| HandlerError::BadRequest(format!("project root is not accessible: {e}")))?;
    if !canonical.is_dir() {
        return Err(HandlerError::BadRequest("project root is not a directory".into()).into());
    }
    Ok(canonical)
}

trait RyeOsPrincipalStoreExt {
    fn load_projects(&self) -> Result<ProjectsFile>;
    fn load_ui_config(&self) -> Result<RyeOsConfigFile>;
    fn load_recent(&self) -> Result<RecentFile>;
}

impl RyeOsPrincipalStoreExt for PrincipalStore {
    fn load_projects(&self) -> Result<ProjectsFile> {
        let projects: ProjectsFile = self.load_yaml(&self.paths().projects_config())?;
        ensure_version("projects.yaml", projects.version, PROJECTS_VERSION)?;
        Ok(projects)
    }

    fn load_ui_config(&self) -> Result<RyeOsConfigFile> {
        let config: RyeOsConfigFile = self.load_yaml(&self.paths().ryeos_config())?;
        validate_ui_config(&config)?;
        Ok(config)
    }

    fn load_recent(&self) -> Result<RecentFile> {
        let recent: RecentFile = self.load_yaml(&self.paths().ryeos_recent())?;
        ensure_version("recent.yaml", recent.version, RECENT_VERSION)?;
        Ok(recent)
    }
}

trait LockedRyeOsPrincipalStoreExt {
    fn write_projects(&self, projects: &ProjectsFile) -> Result<()>;
    fn write_ui_config(&self, config: &RyeOsConfigFile) -> Result<()>;
    fn write_recent(&self, recent: &RecentFile) -> Result<()>;
    fn touch_recent_project(&self, local_id: &str) -> Result<RecentFile>;
}

impl LockedRyeOsPrincipalStoreExt for LockedPrincipalStore {
    fn write_projects(&self, projects: &ProjectsFile) -> Result<()> {
        ensure_version("projects.yaml", projects.version, PROJECTS_VERSION)?;
        self.write_yaml(&self.paths().projects_config(), projects)
    }

    fn write_ui_config(&self, config: &RyeOsConfigFile) -> Result<()> {
        validate_ui_config(config)?;
        self.write_yaml(&self.paths().ryeos_config(), config)
    }

    fn write_recent(&self, recent: &RecentFile) -> Result<()> {
        ensure_version("recent.yaml", recent.version, RECENT_VERSION)?;
        self.write_yaml(&self.paths().ryeos_recent(), recent)
    }

    fn touch_recent_project(&self, local_id: &str) -> Result<RecentFile> {
        let mut recent = self.load_recent()?;
        recent.recent_projects.retain(|p| p.local_id != local_id);
        recent.recent_projects.insert(
            0,
            RecentProject {
                local_id: local_id.to_string(),
                opened_at: lillux::time::iso8601_now(),
            },
        );
        recent.recent_projects.truncate(50);
        self.write_recent(&recent)?;
        Ok(recent)
    }
}

fn resolve_principal_store(ctx: &HandlerContext, state: &AppState) -> Result<PrincipalStore> {
    if let Some(user_principal_id) = compiled_user_principal_id(state)? {
        let resolver = HostedPrincipalResolver::for_app_root(&state.config.app_root);
        return PrincipalStore::resolve_with(&resolver, &user_principal_id);
    }
    require_local_store_principal(ctx, state)?;
    PrincipalStore::resolve_principal(LOCAL_PRINCIPAL_ID)
}

async fn locked_principal_store(
    ctx: &HandlerContext,
    state: &AppState,
) -> Result<LockedPrincipalStore> {
    if let Some(user_principal_id) = compiled_user_principal_id(state)? {
        let resolver = HostedPrincipalResolver::for_app_root(&state.config.app_root);
        return PrincipalStore::locked_with(&resolver, &user_principal_id).await;
    }
    require_local_store_principal(ctx, state)?;
    PrincipalStore::locked_principal(LOCAL_PRINCIPAL_ID).await
}

fn compiled_user_principal_id(state: &AppState) -> Result<Option<String>> {
    let Some(attachment) = crate::seat_auth::compiled_ui_attachment() else {
        return Ok(None);
    };
    let operator =
        ryeos_node_runtime::identity::NodeIdentity::load(&state.config.operator_signing_key_path)?;
    let principal_id = &attachment.compiled_binding.binding.principal_id;
    Ok((principal_id.as_str() != operator.principal_id().as_str()).then(|| principal_id.clone()))
}

fn require_local_store_principal(ctx: &HandlerContext, state: &AppState) -> Result<()> {
    if let Some(attachment) = crate::seat_auth::compiled_ui_attachment() {
        let operator = ryeos_node_runtime::identity::NodeIdentity::load(
            &state.config.operator_signing_key_path,
        )?;
        if attachment.compiled_binding.binding.principal_id != operator.principal_id() {
            return Err(HandlerError::Forbidden(
                "UI session has no retained local-operator store authority".into(),
            )
            .into());
        }
        return Ok(());
    }
    ryeos_app::operator_authority::require_admitted_operator(state, ctx)
        .map_err(|_| HandlerError::Forbidden("admitted operator required".into()))?;
    Ok(())
}

fn retained_principal_id(_state: &AppState) -> Result<String> {
    crate::seat_auth::compiled_ui_attachment()
        .map(|attachment| attachment.compiled_binding.binding.principal_id.clone())
        .ok_or_else(|| HandlerError::Forbidden("compiled UI attachment required".into()).into())
}

fn ensure_version(label: &str, found: u32, expected: u32) -> Result<()> {
    if found != expected {
        return Err(HandlerError::BadRequest(format!(
            "unsupported {label} version {found}; expected {expected}"
        ))
        .into());
    }
    Ok(())
}

fn validate_ui_config(config: &RyeOsConfigFile) -> Result<()> {
    ensure_version("ryeos-ui.yaml", config.version, RYEOS_UI_CONFIG_VERSION)?;
    validate_choice("theme", &config.theme, &["system", "light", "dark"])?;
    validate_choice("landing_view", &config.landing_view, &["projects"])?;
    validate_saved_view_set_templates(&config.saved_view_sets).map_err(HandlerError::BadRequest)?;
    validate_particular_view_set_resumes(&config.particular_view_sets)
        .map_err(HandlerError::BadRequest)?;
    validate_surface_libraries(config)
}
fn validate_surface_libraries(config: &RyeOsConfigFile) -> Result<()> {
    ryeos_client_base::surface::saved_surfaces::validate_saved_surface_libraries(
        &config.reusable_surfaces,
        &config.particular_surfaces,
        &config.particular_view_sets,
    )
    .map_err(HandlerError::BadRequest)?;
    // Canonical reference interpretation stays with the engine; the shared
    // data contract never grants admission from its logical origin strings.
    for origin in config
        .reusable_surfaces
        .iter()
        .map(|record| &record.origin)
        .chain(
            config
                .particular_surfaces
                .iter()
                .map(|record| &record.origin),
        )
        .chain(
            config
                .reusable_surfaces
                .iter()
                .flat_map(|record| record.tab_origins.values()),
        )
    {
        if !ryeos_engine::canonical_ref::CanonicalRef::parse(&origin.surface_ref)
            .is_ok_and(|reference| reference.kind == "surface")
        {
            return Err(HandlerError::BadRequest(
                "saved Surface origin is not a canonical Surface reference".into(),
            )
            .into());
        }
    }
    Ok(())
}

fn validate_choice(field: &str, value: &str, allowed: &[&str]) -> Result<()> {
    if allowed.contains(&value) {
        return Ok(());
    }
    Err(HandlerError::BadRequest(format!(
        "invalid {field} value '{value}'; expected one of: {}",
        allowed.join(", ")
    ))
    .into())
}

fn inferred_project_name(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("project")
        .to_string()
}

fn project_view(project: ProjectEntry, current_project: Option<&str>, registered: bool) -> Value {
    let exists = Path::new(&project.root).is_dir();
    let current = current_project.is_some_and(|current| same_existing_dir(current, &project.root));
    json!({
        "local_id": project.local_id,
        "name": project.name,
        "root": project.root,
        "added_at": project.added_at,
        "tags": project.tags,
        "exists": exists,
        "current": current,
        "registered": registered,
    })
}

pub(crate) fn authorize_launch_project(
    ctx: &HandlerContext,
    state: &AppState,
    requested_root: &str,
) -> Result<(lillux::PinnedDirectory, Option<String>)> {
    let requested = canonical_project_root(requested_root)?;
    let authority = lillux::PinnedDirectory::open(&requested)?
        .context("authorized UI project root disappeared")?;
    if ryeos_app::operator_authority::require_admitted_operator(state, ctx).is_ok() {
        let store = PrincipalStore::resolve_principal(LOCAL_PRINCIPAL_ID)?;
        let registered_project_id = store
            .load_projects()?
            .projects
            .into_iter()
            .find(|project| same_existing_dir(&project.root, requested.to_string_lossy().as_ref()))
            .map(|project| project.local_id);
        return Ok((authority, registered_project_id));
    }

    // Non-operator principals may select only a project already recorded in
    // their private principal space. The request path is never itself project
    // authority.
    let resolver = HostedPrincipalResolver::for_app_root(&state.config.app_root);
    let store = PrincipalStore::resolve_with(&resolver, &ctx.fingerprint)?;
    let projects = store.load_projects()?;
    let registered_project_id = projects
        .projects
        .into_iter()
        .find(|project| same_existing_dir(&project.root, requested.to_string_lossy().as_ref()))
        .map(|project| project.local_id);
    if let Some(registered_project_id) = registered_project_id {
        Ok((authority, Some(registered_project_id)))
    } else {
        Err(HandlerError::Forbidden(
            "requested UI project is not admitted by the caller's project registry".into(),
        )
        .into())
    }
}

/// Revalidate a registered launch project after binding compilation and keep
/// the existing principal YAML gate held until the pending launch token is
/// inserted. Forget takes this same gate before scanning retained attachment
/// authority, so it cannot pass between revalidation and token publication.
pub(crate) async fn lock_launch_project_registration(
    ctx: &HandlerContext,
    state: &AppState,
    registered_project_id: &str,
    canonical_root: &Path,
) -> Result<LockedPrincipalStore> {
    let store = if ryeos_app::operator_authority::require_admitted_operator(state, ctx).is_ok() {
        PrincipalStore::locked_principal(LOCAL_PRINCIPAL_ID).await?
    } else {
        let resolver = HostedPrincipalResolver::for_app_root(&state.config.app_root);
        PrincipalStore::locked_with(&resolver, &ctx.fingerprint).await?
    };
    let still_registered = store.load_projects()?.projects.into_iter().any(|project| {
        project.local_id == registered_project_id
            && same_existing_dir(&project.root, canonical_root.to_string_lossy().as_ref())
    });
    if !still_registered {
        return Err(
            HandlerError::Conflict("project registration changed during UI launch".into()).into(),
        );
    }
    Ok(store)
}

fn same_existing_dir(left: &str, right: &str) -> bool {
    let Ok(left) = Path::new(left).canonicalize() else {
        return false;
    };
    let Ok(right) = Path::new(right).canonicalize() else {
        return false;
    };
    left == right
}

macro_rules! descriptor {
    ($name:ident, $service_ref:literal, $endpoint:literal, $handler:ident) => {
        descriptor!($name, $service_ref, $endpoint, $handler, &[]);
    };
    ($name:ident, $service_ref:literal, $endpoint:literal, $handler:ident, $caps:expr) => {
        pub const $name: ServiceDescriptor = ServiceDescriptor {
            service_ref: $service_ref,
            endpoint: $endpoint,
            availability: ServiceAvailability::DaemonOnly,
            required_caps: $caps,
            handler: |params, ctx, state| {
                Box::pin(async move { $handler(params, ctx, state).await })
            },
        };
    };
}

descriptor!(
    PROJECTS_LIST_DESCRIPTOR,
    "service:projects/list",
    "projects.list",
    handle_projects_list
);
descriptor!(
    PROJECTS_ADD_DESCRIPTOR,
    "service:projects/add",
    "projects.add",
    handle_projects_add,
    &["ryeos.execute.service.projects/add"]
);
descriptor!(
    PROJECTS_FORGET_DESCRIPTOR,
    "service:projects/forget",
    "projects.forget",
    handle_projects_forget
);
descriptor!(
    PROJECTS_RESOLVE_DESCRIPTOR,
    "service:projects/resolve",
    "projects.resolve",
    handle_projects_resolve
);
descriptor!(
    PROJECTS_OPEN_DESCRIPTOR,
    "service:projects/open",
    "projects.open",
    handle_projects_open,
    &["ryeos.execute.service.projects/open"]
);
descriptor!(
    UI_PROJECTS_LIST_DESCRIPTOR,
    "service:ui/projects/list",
    "ui.projects.list",
    handle_projects_list
);
descriptor!(
    UI_PROJECTS_ADD_DESCRIPTOR,
    "service:ui/projects/add",
    "ui.projects.add",
    handle_projects_add,
    &["ryeos.execute.service.projects/add"]
);
descriptor!(
    UI_PROJECTS_FORGET_DESCRIPTOR,
    "service:ui/projects/forget",
    "ui.projects.forget",
    handle_projects_forget
);
descriptor!(
    UI_PROJECTS_RESOLVE_DESCRIPTOR,
    "service:ui/projects/resolve",
    "ui.projects.resolve",
    handle_projects_resolve
);
descriptor!(
    UI_PROJECTS_OPEN_DESCRIPTOR,
    "service:ui/projects/open",
    "ui.projects.open",
    handle_projects_open,
    &["ryeos.execute.service.projects/open"]
);
descriptor!(
    RYEOS_UI_PROJECTS_LIST_DESCRIPTOR,
    "service:ui/ryeos-ui/projects/list",
    "ui.ryeos.projects.list",
    handle_projects_list
);
descriptor!(
    RYEOS_UI_PROJECTS_ADD_DESCRIPTOR,
    "service:ui/ryeos-ui/projects/add",
    "ui.ryeos.projects.add",
    handle_projects_add,
    &["ryeos.execute.service.projects/add"]
);
descriptor!(
    RYEOS_UI_PROJECTS_FORGET_DESCRIPTOR,
    "service:ui/ryeos-ui/projects/forget",
    "ui.ryeos.projects.forget",
    handle_projects_forget
);
descriptor!(
    RYEOS_UI_PROJECTS_RESOLVE_DESCRIPTOR,
    "service:ui/ryeos-ui/projects/resolve",
    "ui.ryeos.projects.resolve",
    handle_projects_resolve
);
descriptor!(
    RYEOS_UI_PROJECTS_OPEN_DESCRIPTOR,
    "service:ui/ryeos-ui/projects/open",
    "ui.ryeos.projects.open",
    handle_projects_open,
    &["ryeos.execute.service.projects/open"]
);
descriptor!(
    RECENT_TOUCH_DESCRIPTOR,
    "service:ui/ryeos-ui/recent/touch",
    "ui.ryeos.recent.touch",
    handle_recent_touch
);
descriptor!(
    RECENT_LIST_DESCRIPTOR,
    "service:ui/ryeos-ui/recent/list",
    "ui.ryeos.recent.list",
    handle_recent_list
);
descriptor!(
    CONFIG_GET_DESCRIPTOR,
    "service:ui/ryeos-ui/config/get",
    "ui.ryeos.config.get",
    handle_config_get
);
descriptor!(
    CONFIG_UPDATE_DESCRIPTOR,
    "service:ui/ryeos-ui/config/update",
    "ui.ryeos.config.update",
    handle_config_update,
    &["ryeos.execute.service.ui/ryeos-ui/config/update"]
);
descriptor!(
    PARTICULAR_VIEW_SET_RESUME_DESCRIPTOR,
    "service:ui/ryeos-ui/view-sets/resume",
    "ui.ryeos.view-sets.resume",
    handle_particular_view_set_resume,
    &["ryeos.execute.service.ui/ryeos-ui/view-sets/resume"]
);
descriptor!(
    PARTICULAR_SURFACE_RESUME_DESCRIPTOR,
    "service:ui/ryeos-ui/surfaces/resume",
    "ui.ryeos.surfaces.resume",
    handle_particular_surface_resume,
    &["ryeos.execute.service.ui/ryeos-ui/surfaces/resume"]
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_mutation_descriptors_require_exact_operation_caps() {
        for (descriptor, operation) in [
            (&PROJECTS_ADD_DESCRIPTOR, "projects/add"),
            (&UI_PROJECTS_ADD_DESCRIPTOR, "projects/add"),
            (&RYEOS_UI_PROJECTS_ADD_DESCRIPTOR, "projects/add"),
            (&PROJECTS_OPEN_DESCRIPTOR, "projects/open"),
            (&UI_PROJECTS_OPEN_DESCRIPTOR, "projects/open"),
            (&RYEOS_UI_PROJECTS_OPEN_DESCRIPTOR, "projects/open"),
            (&CONFIG_UPDATE_DESCRIPTOR, "ui/ryeos-ui/config/update"),
            (
                &PARTICULAR_VIEW_SET_RESUME_DESCRIPTOR,
                "ui/ryeos-ui/view-sets/resume",
            ),
            (
                &PARTICULAR_SURFACE_RESUME_DESCRIPTOR,
                "ui/ryeos-ui/surfaces/resume",
            ),
            (
                &super::super::ui_session_attachments::OPEN_SURFACE_DESCRIPTOR,
                "ui/session/surfaces/open",
            ),
        ] {
            let cap = format!("ryeos.execute.service.{operation}");
            assert_eq!(
                descriptor.required_caps,
                &[cap.as_str()],
                "{}",
                descriptor.service_ref
            );
        }
    }

    #[test]
    fn admission_size_charges_completion_and_full_append_envelope() {
        let producer = ryeos_client_base::ui::UiPresentationProducer {
            presentation_id: uuid::Uuid::new_v4().to_string(),
            thread_id: "T-presentation".into(),
            producer_incarnation: uuid::Uuid::new_v4().to_string(),
        };
        let operation = uuid::Uuid::new_v4().to_string();
        let result = json!({"escaped":"\"\\\n".repeat(100)});
        let completed =
            ryeos_client_base::ui::admission_operation::UiCompletedPresentationOperation {
                operation_id: operation.clone(),
                result: result.clone(),
            };
        let result_bytes = serde_json::to_vec(&result).unwrap().len() as u64;
        let completed_bytes = serde_json::to_vec(&completed).unwrap().len() as u64;
        assert!(
            validate_completed_admission_size(
                &result,
                &producer,
                &operation,
                result_bytes,
                u64::MAX
            )
            .is_err()
        );
        assert!(
            validate_completed_admission_size(
                &result,
                &producer,
                &operation,
                completed_bytes,
                completed_bytes
            )
            .is_err()
        );
        assert!(
            validate_completed_admission_size(
                &result,
                &producer,
                &operation,
                completed_bytes,
                65536
            )
            .is_ok()
        );
        assert!(
            validate_completed_admission_size(&result, &producer, &operation, 0, 65536).is_err()
        );
    }

    #[test]
    fn projects_file_defaults_to_version_one() {
        let file = ProjectsFile::default();
        assert_eq!(file.version, 1);
        assert!(file.projects.is_empty());
    }

    #[test]
    fn ui_config_defaults_to_empty_revisioned_view_set_library() {
        let config = RyeOsConfigFile::default();
        assert_eq!(config.version, 6);
        assert_eq!(config.view_set_library_revision, 0);
        assert!(config.saved_view_sets.is_empty());
        assert_eq!(config.particular_view_set_library_revision, 0);
        assert!(config.particular_view_sets.is_empty());
        assert_eq!(config.reusable_surface_library_revision, 0);
        assert!(config.reusable_surfaces.is_empty());
        assert_eq!(config.particular_surface_library_revision, 0);
        assert!(config.particular_surfaces.is_empty());
        validate_ui_config(&config).unwrap();
    }

    fn saved_view_set_update(expected_revision: u64) -> UpdateConfigRequest {
        serde_json::from_value(json!({
            "view_set_library": {
                "expected_revision": expected_revision,
                "saved_view_sets": [{
                    "id": "development",
                    "name": "Development",
                    "composition": {
                        "schema": 2,
                        "id": "development",
                        "title": "Development",
                        "root": {
                            "type": "group",
                            "views": ["view:ryeos/development"],
                            "active": 0
                        }
                    },
                    "relationships": [{
                        "mount": {"kind": "tile", "index": 0},
                        "source": {"mode": "follow_own_set"}
                    }]
                }]
            }
        }))
        .unwrap()
    }

    #[test]
    fn view_set_library_update_is_revision_fenced() {
        let mut config = RyeOsConfigFile::default();
        apply_config_update(&mut config, saved_view_set_update(0)).unwrap();
        assert_eq!(config.view_set_library_revision, 1);
        assert_eq!(config.saved_view_sets.len(), 1);

        let before = config.clone();
        let error = apply_config_update(&mut config, saved_view_set_update(0)).unwrap_err();
        assert!(error.to_string().contains("revision advanced"));
        assert_eq!(config, before);
    }

    fn particular_view_set_update(expected_revision: u64) -> UpdateConfigRequest {
        serde_json::from_value(json!({
            "particular_view_set_library": {
                "expected_revision": expected_revision,
                "particular_view_sets": [{
                    "id": "development-current",
                    "name": "Development current",
                    "composition": {
                        "schema": 2,
                        "id": "development-current",
                        "title": "Development current",
                        "root": {
                            "type": "group",
                            "views": ["view:ryeos/development"],
                            "active": 0
                        }
                    },
                    "relationships": [{
                        "mount": {"kind": "tile", "index": 0},
                        "source": {"mode": "follow_own_set"}
                    }],
                    "context": {
                        "surface_ref": "surface:test/particular",
                        "project": {"local_id": "prj_example"},
                        "work": {"chain_root_id": "T-root"}
                    }
                }]
            }
        }))
        .unwrap()
    }

    #[test]
    fn particular_view_set_library_has_independent_revision_fence() {
        let mut config = RyeOsConfigFile::default();
        apply_config_update(&mut config, particular_view_set_update(0)).unwrap();
        assert_eq!(config.particular_view_set_library_revision, 1);
        assert_eq!(config.particular_view_sets.len(), 1);
        assert_eq!(config.view_set_library_revision, 0);

        let before = config.clone();
        let error = apply_config_update(&mut config, particular_view_set_update(0)).unwrap_err();
        assert!(error.to_string().contains("revision advanced"));
        assert_eq!(config, before);
    }

    fn reusable_surface_update(expected_revision: u64) -> UpdateConfigRequest {
        let template = saved_view_set_update(0)
            .view_set_library
            .unwrap()
            .saved_view_sets
            .remove(0);
        serde_json::from_value(json!({"reusable_surface_library": {
            "expected_revision":expected_revision,
            "reusable_surfaces":[{"id":"development","title":"Development","origin":{"surface_ref":"surface:test/origin"},
                "view_sets":[template],"tab_origins":{"development":{"surface_ref":"surface:test/origin"}},"active_view_set_id":"development"}]
        }}))
        .unwrap()
    }

    fn particular_surface_update(expected_revision: u64, set: &str) -> UpdateConfigRequest {
        serde_json::from_value(json!({"particular_surface_library": {
            "expected_revision":expected_revision,
            "particular_surfaces":[{"id":"development","title":"Development current","origin":{"surface_ref":"surface:test/origin"},
                "view_set_ids":[set],"active_view_set_id":set}]
        }}))
        .unwrap()
    }

    #[test]
    fn saved_surface_libraries_have_separate_ids_and_atomic_revision_fences() {
        let mut config = RyeOsConfigFile::default();
        apply_config_update(&mut config, reusable_surface_update(0)).unwrap();
        apply_config_update(&mut config, particular_view_set_update(0)).unwrap();
        apply_config_update(
            &mut config,
            particular_surface_update(0, "development-current"),
        )
        .unwrap();
        assert_eq!(
            config.reusable_surfaces[0].id,
            config.particular_surfaces[0].id
        );
        assert_eq!(config.reusable_surface_library_revision, 1);
        assert_eq!(config.particular_surface_library_revision, 1);
        let before = config.clone();
        assert!(apply_config_update(&mut config, reusable_surface_update(0)).is_err());
        assert_eq!(config, before);
        let mut combined = reusable_surface_update(1);
        combined.particular_surface_library =
            particular_surface_update(0, "development-current").particular_surface_library;
        assert!(apply_config_update(&mut config, combined).is_err());
        assert_eq!(config, before);
    }

    #[test]
    fn particular_surface_resume_rejects_tab_only_library_advance() {
        let mut config = RyeOsConfigFile::default();
        apply_config_update(&mut config, particular_view_set_update(0)).unwrap();
        apply_config_update(
            &mut config,
            particular_surface_update(0, "development-current"),
        )
        .unwrap();
        let request = ResumeParticularSurfaceRequest {
            id: "development".into(),
            expected_revision: config.particular_surface_library_revision,
            view_set_expected_revision: config.particular_view_set_library_revision,
            creation_request_id: uuid::Uuid::new_v4().to_string(),
        };
        assert!(require_particular_surface_revisions(&config, &request).is_ok());
        let mut changed_tabs = particular_view_set_update(1);
        changed_tabs
            .particular_view_set_library
            .as_mut()
            .unwrap()
            .particular_view_sets[0]
            .name = "Changed development tab".into();
        apply_config_update(&mut config, changed_tabs).unwrap();
        assert_eq!(
            config.particular_view_sets[0].name,
            "Changed development tab"
        );
        assert_eq!(
            config.particular_surface_library_revision,
            request.expected_revision
        );
        assert!(
            require_particular_surface_revisions(&config, &request)
                .unwrap_err()
                .to_string()
                .contains("particular view-set library revision changed")
        );
    }

    #[test]
    fn final_config_references_are_checked_without_partial_mutation() {
        let mut config = RyeOsConfigFile::default();
        let before = config.clone();
        assert!(apply_config_update(&mut config, particular_surface_update(0, "missing")).is_err());
        assert_eq!(config, before);
        let mut combined = particular_view_set_update(0);
        combined.particular_surface_library =
            particular_surface_update(0, "development-current").particular_surface_library;
        apply_config_update(&mut config, combined).unwrap();
        let before = config.clone();
        let remove = serde_json::from_value(json!({"particular_view_set_library": {
            "expected_revision":1,"particular_view_sets":[]
        }}))
        .unwrap();
        assert!(apply_config_update(&mut config, remove).is_err());
        assert_eq!(config, before);
    }

    #[test]
    fn saved_surface_records_preserve_order_and_reject_invalid_active_tabs_or_authority() {
        let mut update = reusable_surface_update(0);
        let record = &mut update
            .reusable_surface_library
            .as_mut()
            .unwrap()
            .reusable_surfaces[0];
        let mut second = record.view_sets[0].clone();
        second.id = "review".into();
        second.composition.id = "review".into();
        record.view_sets.push(second);
        record
            .tab_origins
            .insert("review".into(), record.origin.clone());
        record.active_view_set_id = "review".into();
        let mut config = RyeOsConfigFile::default();
        apply_config_update(&mut config, update).unwrap();
        let encoded = serde_json::to_value(&config).unwrap();
        let decoded: RyeOsConfigFile = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, config);
        assert_eq!(decoded.reusable_surfaces[0].view_sets[0].id, "development");
        assert_eq!(decoded.reusable_surfaces[0].active_view_set_id, "review");
        let mut invalid = reusable_surface_update(1);
        invalid
            .reusable_surface_library
            .as_mut()
            .unwrap()
            .reusable_surfaces[0]
            .active_view_set_id = "missing".into();
        let before = config.clone();
        assert!(apply_config_update(&mut config, invalid).is_err());
        assert_eq!(config, before);
        assert!(
            serde_json::from_value::<ParticularSurfaceRecord>(json!({
                "id":"private","title":"Private","view_set_ids":["development-current"],
                "origin":{"surface_ref":"surface:test/origin"},
                "active_view_set_id":"development-current", "binding_generation":9
            }))
            .is_err()
        );
        let mut predecessor = serde_json::to_value(RyeOsConfigFile::default()).unwrap();
        predecessor["version"] = json!(5);
        assert!(validate_ui_config(&serde_json::from_value(predecessor).unwrap()).is_err());
    }

    #[test]
    fn revision_exhaustion_and_tab_bounds_leave_all_libraries_unchanged() {
        let mut config = RyeOsConfigFile {
            particular_surface_library_revision: u64::MAX,
            ..RyeOsConfigFile::default()
        };
        let before = config.clone();
        let mut combined = reusable_surface_update(0);
        combined.particular_surface_library =
            particular_surface_update(u64::MAX, "missing").particular_surface_library;
        assert!(apply_config_update(&mut config, combined).is_err());
        assert_eq!(config, before);
        let mut update = reusable_surface_update(0);
        let record = &mut update
            .reusable_surface_library
            .as_mut()
            .unwrap()
            .reusable_surfaces[0];
        let original = record.view_sets[0].clone();
        for index in 1..=ryeos_client_base::surface::view_sets::MAX_VIEW_SETS {
            let mut tab = original.clone();
            tab.id = format!("tab-{index}");
            tab.composition.id = tab.id.clone();
            record
                .tab_origins
                .insert(tab.id.clone(), record.origin.clone());
            record.view_sets.push(tab);
        }
        assert!(apply_config_update(&mut config, update).is_err());
        assert_eq!(config, before);
    }

    #[test]
    fn relative_project_root_is_rejected() {
        let err = canonical_project_root("relative/path").unwrap_err();
        assert!(err.to_string().contains("project root must be absolute"));
    }

    #[test]
    fn unsupported_versions_are_rejected() {
        let err = ensure_version("projects.yaml", 2, 1).unwrap_err();
        assert!(
            err.to_string()
                .contains("unsupported projects.yaml version 2")
        );
    }

    #[test]
    fn invalid_config_choices_are_rejected() {
        let err = validate_choice("theme", "neon", &["system", "light", "dark"]).unwrap_err();
        assert!(err.to_string().contains("invalid theme value 'neon'"));
    }

    #[test]
    fn caller_authored_open_mode_is_not_a_ui_config_field() {
        let error = serde_json::from_value::<UpdateConfigRequest>(json!({
            "default_open_mode": "read_only"
        }))
        .expect_err("launch posture must come from the compiled signed surface binding");
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn project_view_marks_missing_roots_without_mutating_registry_entry() {
        let value = project_view(
            ProjectEntry {
                local_id: "prj_1".into(),
                name: "Missing".into(),
                root: "/definitely/missing/ryeos/project".into(),
                added_at: "2026-05-30T00:00:00Z".into(),
                tags: vec![],
            },
            None,
            true,
        );
        assert_eq!(value["exists"], false);
        assert_eq!(value["root"], "/definitely/missing/ryeos/project");
    }
}
