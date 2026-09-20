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
use ryeos_client_base::surface::view_sets::{
    ParticularViewSetResume, SavedViewSetTemplate, validate_particular_view_set_resumes,
    validate_saved_view_set_templates,
};
use ryeos_executor::executor::ServiceAvailability;

use crate::seat_auth::require_seat_caller;
use crate::state::get_ui_state;

const PROJECTS_VERSION: u32 = 1;
const RYEOS_UI_CONFIG_VERSION: u32 = 4;
const RECENT_VERSION: u32 = 1;

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
    let session_id = session_id_from_context(&ctx)
        .ok_or_else(|| HandlerError::Forbidden("browser session required".into()))?;
    let origin = crate::seat_auth::compiled_ui_attachment()
        .ok_or_else(|| HandlerError::Forbidden("compiled UI attachment required".into()))?;
    let project = {
        let store = locked_principal_store(&ctx, &state).await?;
        store
            .load_projects()?
            .projects
            .into_iter()
            .find(|p| p.local_id == req.local_id)
            .ok_or(HandlerError::NotFound)?
    };

    let canonical = canonical_project_root(&project.root)?;
    let root = canonical.display().to_string();
    let project_authority = Arc::new(
        lillux::PinnedDirectory::open(&canonical)?
            .context("selected UI project root disappeared")?,
    );
    let ui =
        get_ui_state(&state).ok_or_else(|| HandlerError::Internal("UiState not set".into()))?;
    let session = ui
        .browser_sessions
        .get_session(session_id)
        .ok_or_else(|| HandlerError::Forbidden("session expired or invalid".into()))?;
    let compile_context = HandlerContext::new(
        session.principal_id.clone(),
        session.granted_caps.clone(),
        true,
    );
    let compile_request = super::ui_launch_mint::Request {
        ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.to_owned(),
        surface_ref: origin.surface_ref.clone(),
        project_path: Some(root.clone()),
        user_principal_id: None,
    };
    let (compiled_binding, effective_surface) = super::ui_launch_mint::compile_session_binding(
        &compile_request,
        &compile_context,
        &state,
        Some(project_authority.as_ref()),
    )?;

    // Reacquire the existing principal-registry gate after compilation. Both
    // publish and forget take registry -> session-store in this order, making
    // the live-attachment predicate atomic with registry mutation.
    let store = locked_principal_store(&ctx, &state).await?;
    let still_registered = store
        .load_projects()?
        .projects
        .into_iter()
        .any(|entry| entry.local_id == project.local_id && entry.root == project.root);
    if !still_registered {
        return Err(
            HandlerError::Conflict("project registration changed during open".into()).into(),
        );
    }
    let policy = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>(
    )?;
    let request_bounds = super::ui_launch_mint::binding_request_bounds(&state)?;
    let attachment = ui.browser_sessions.publish_attachment(
        session_id,
        &origin.coordinate(),
        crate::browser_session::BindingAttachmentCandidate {
            registered_project_id: Some(project.local_id.clone()),
            compiled_binding: Arc::new(compiled_binding),
            effective_surface,
            project_authority: Some(project_authority),
        },
        usize::try_from(policy.max_live_binding_attachments_per_session)?,
        state.node_policy.generation_digest(),
    )?;
    let descriptor = attachment.public_descriptor(request_bounds);
    // Recent history is presentation metadata, not attachment authority. Once
    // publication succeeds, a recent-file failure must not turn the admitted
    // attachment into an unreturned/stranded capability.
    let recent = match store.touch_recent_project(&project.local_id) {
        Ok(recent) => recent,
        Err(error) => {
            tracing::warn!(
                %error,
                local_id = %project.local_id,
                "binding attachment admitted but recent project history was not updated"
            );
            store.load_recent().unwrap_or_default()
        }
    };

    Ok(json!({
        "project": project_view(
            ProjectEntry { root: root.clone(), ..project.clone() },
            Some(&root),
            true
        ),
        "recent": recent.recent_projects,
        "ui_transition": {
            "kind": "admit_binding_attachment",
            "attachment": descriptor,
        }
    }))
}

/// Resume one durable particular view set without reviving any prior session
/// authority. Stable project/work identities are resolved again; a project
/// context receives a freshly compiled retained attachment, while projectless
/// work remains on the exact invoking attachment.
pub async fn handle_particular_view_set_resume(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let caller = require_seat_caller(&ctx, &state)?;
    let req: ResumeParticularViewSetRequest = parse_request(params)?;
    let session_id = session_id_from_context(&ctx)
        .ok_or_else(|| HandlerError::Forbidden("browser session required".into()))?;
    let origin = crate::seat_auth::compiled_ui_attachment()
        .ok_or_else(|| HandlerError::Forbidden("compiled UI attachment required".into()))?;

    let (resume, project) = {
        let store = locked_principal_store(&ctx, &state).await?;
        let config = store.load_ui_config()?;
        let resume = config
            .particular_view_sets
            .into_iter()
            .find(|candidate| candidate.id == req.id)
            .ok_or(HandlerError::NotFound)?;
        let projects = store.load_projects()?;
        let project = resume
            .context
            .project
            .as_ref()
            .map(|project_ref| {
                projects
                    .projects
                    .into_iter()
                    .find(|candidate| candidate.local_id == project_ref.local_id)
                    .ok_or(HandlerError::NotFound)
            })
            .transpose()?;
        (resume, project)
    };

    let canonical_project = project
        .as_ref()
        .map(|project| canonical_project_root(&project.root))
        .transpose()?;
    if canonical_project.is_none()
        && (origin.project_query_identity.is_some()
            || origin.project_authority.is_some()
            || origin.registered_project_id.is_some())
    {
        return Err(HandlerError::Forbidden(
            "projectless particular view sets require a projectless invoking attachment".into(),
        )
        .into());
    }
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
        })
        .transpose()?;

    let ui =
        get_ui_state(&state).ok_or_else(|| HandlerError::Internal("UiState not set".into()))?;
    let session = ui
        .browser_sessions
        .get_session(session_id)
        .ok_or_else(|| HandlerError::Forbidden("session expired or invalid".into()))?;

    let (attachment, insertion_attachment_id) = if let (Some(project), Some(canonical)) =
        (project.as_ref(), canonical_project.as_ref())
    {
        let root = canonical.display().to_string();
        let project_authority = Arc::new(
            lillux::PinnedDirectory::open(canonical)?
                .context("selected UI project root disappeared")?,
        );
        let compile_context = HandlerContext::new(
            session.principal_id.clone(),
            session.granted_caps.clone(),
            true,
        );
        let compile_request = super::ui_launch_mint::Request {
            ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.to_owned(),
            surface_ref: origin.surface_ref.clone(),
            project_path: Some(root),
            user_principal_id: None,
        };
        let (compiled_binding, effective_surface) = super::ui_launch_mint::compile_session_binding(
            &compile_request,
            &compile_context,
            &state,
            Some(project_authority.as_ref()),
        )?;

        // Compilation happens outside the principal-store gate. Reacquire and
        // revalidate both durable records before publishing the new authority.
        let store = locked_principal_store(&ctx, &state).await?;
        let config_unchanged = store
            .load_ui_config()?
            .particular_view_sets
            .into_iter()
            .any(|candidate| candidate == resume);
        let project_unchanged = store
            .load_projects()?
            .projects
            .into_iter()
            .any(|candidate| {
                candidate.local_id == project.local_id && candidate.root == project.root
            });
        if !config_unchanged || !project_unchanged {
            return Err(HandlerError::Conflict(
                "particular view-set context changed during resume".into(),
            )
            .into());
        }
        let policy = state
            .node_policy
            .require::<ryeos_app::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>(
            )?;
        let request_bounds = super::ui_launch_mint::binding_request_bounds(&state)?;
        let retained = ui.browser_sessions.publish_attachment(
            session_id,
            &origin.coordinate(),
            crate::browser_session::BindingAttachmentCandidate {
                registered_project_id: Some(project.local_id.clone()),
                compiled_binding: Arc::new(compiled_binding),
                effective_surface,
                project_authority: Some(project_authority),
            },
            usize::try_from(policy.max_live_binding_attachments_per_session)?,
            state.node_policy.generation_digest(),
        )?;
        let descriptor = retained.public_descriptor(request_bounds);
        let insertion = descriptor.binding_attachment_id.clone();
        (Some(descriptor), insertion)
    } else {
        // A projectless particular set cannot mint new authority. Its current
        // compiled origin is the only valid insertion context.
        let store = locked_principal_store(&ctx, &state).await?;
        if !store
            .load_ui_config()?
            .particular_view_sets
            .into_iter()
            .any(|candidate| candidate == resume)
        {
            return Err(HandlerError::Conflict(
                "particular view-set context changed during resume".into(),
            )
            .into());
        }
        (None, origin.binding_attachment_id.clone())
    };

    Ok(json!({
        "particular_view_set": {
            "id": resume.id,
            "name": resume.name,
            "composition": resume.composition,
            "relationships": resume.relationships,
            "resolved_selection_work": resolved_work,
        },
        "ui_transition": {
            "kind": "resume_particular_view_set",
            "attachment": attachment,
            "insertion_attachment_id": insertion_attachment_id,
        }
    }))
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

fn project_root_locator_for_forget(root: &str) -> Result<String> {
    let path = PathBuf::from(root);
    if !path.is_absolute() {
        return Err(HandlerError::BadRequest("project root must be absolute".into()).into());
    }
    match path.canonicalize() {
        Ok(canonical) => Ok(canonical.display().to_string()),
        Err(_) => Ok(path.display().to_string()),
    }
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
        ryeos_app::identity::NodeIdentity::load(&state.config.operator_signing_key_path)?;
    let principal_id = &attachment.compiled_binding.binding.principal_id;
    Ok((principal_id.as_str() != operator.principal_id().as_str()).then(|| principal_id.clone()))
}

fn require_local_store_principal(ctx: &HandlerContext, state: &AppState) -> Result<()> {
    if let Some(attachment) = crate::seat_auth::compiled_ui_attachment() {
        let operator =
            ryeos_app::identity::NodeIdentity::load(&state.config.operator_signing_key_path)?;
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
        .map_err(|error| HandlerError::BadRequest(error).into())
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

fn session_id_from_context(ctx: &HandlerContext) -> Option<&str> {
    ctx.fingerprint.strip_prefix("session:")
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
        pub const $name: ServiceDescriptor = ServiceDescriptor {
            service_ref: $service_ref,
            endpoint: $endpoint,
            availability: ServiceAvailability::DaemonOnly,
            required_caps: &[],
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
    handle_projects_add
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
    handle_projects_open
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
    handle_projects_add
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
    handle_projects_open
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
    handle_projects_add
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
    handle_projects_open
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
    handle_config_update
);
descriptor!(
    PARTICULAR_VIEW_SET_RESUME_DESCRIPTOR,
    "service:ui/ryeos-ui/view-sets/resume",
    "ui.ryeos.view-sets.resume",
    handle_particular_view_set_resume
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_file_defaults_to_version_one() {
        let file = ProjectsFile::default();
        assert_eq!(file.version, 1);
        assert!(file.projects.is_empty());
    }

    #[test]
    fn ui_config_defaults_to_empty_revisioned_view_set_library() {
        let config = RyeOsConfigFile::default();
        assert_eq!(config.version, 4);
        assert_eq!(config.view_set_library_revision, 0);
        assert!(config.saved_view_sets.is_empty());
        assert_eq!(config.particular_view_set_library_revision, 0);
        assert!(config.particular_view_sets.is_empty());
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
                        "id": "development",
                        "title": "Development",
                        "root": {
                            "type": "group",
                            "views": ["view:ryeos/development"],
                            "active": 0
                        },
                        "slots": {}
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
                        "id": "development-current",
                        "title": "Development current",
                        "root": {
                            "type": "group",
                            "views": ["view:ryeos/development"],
                            "active": 0
                        },
                        "slots": {}
                    },
                    "relationships": [{
                        "mount": {"kind": "tile", "index": 0},
                        "source": {"mode": "follow_own_set"}
                    }],
                    "context": {
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

    #[test]
    fn relative_project_root_is_rejected() {
        let err = canonical_project_root("relative/path").unwrap_err();
        assert!(err.to_string().contains("project root must be absolute"));
    }

    #[test]
    fn forget_root_locator_accepts_missing_absolute_paths() {
        let locator = project_root_locator_for_forget("/definitely/missing/ryeos/project")
            .expect("missing absolute path should still be usable for forget");
        assert_eq!(locator, "/definitely/missing/ryeos/project");
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
