//! Studio local project registry and user Studio config handlers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::{parse_request, HandlerError};
use ryeos_app::state::AppState;
use ryeos_app::user_space::{read_yaml_or_default, write_yaml_atomic, UserSpacePaths};
use ryeos_executor::executor::ServiceAvailability;

use crate::state::get_ui_state;

const PROJECTS_VERSION: u32 = 1;
const STUDIO_CONFIG_VERSION: u32 = 1;
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StudioConfigFile {
    pub version: u32,
    pub theme: String,
    pub landing_view: String,
    pub default_open_mode: String,
}

impl Default for StudioConfigFile {
    fn default() -> Self {
        Self {
            version: STUDIO_CONFIG_VERSION,
            theme: "system".into(),
            landing_view: "projects".into(),
            default_open_mode: "normal".into(),
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
    pub tags: Vec<String>,
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
    pub default_open_mode: Option<String>,
}

pub async fn handle_projects_list(
    _params: Value,
    _ctx: HandlerContext,
    _state: Arc<AppState>,
) -> Result<Value> {
    let paths = UserSpacePaths::resolve()?;
    let projects: ProjectsFile = read_yaml_or_default(&paths.projects_config())?;
    Ok(json!({
        "version": projects.version,
        "projects": projects.projects.into_iter().map(project_view).collect::<Vec<_>>()
    }))
}

pub async fn handle_projects_add(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    ensure_writable_session(&ctx, &state)?;
    let req: AddProjectRequest = parse_request(params)?;
    let root = canonical_project_root(&req.root)?;
    let root_text = root.display().to_string();
    let paths = UserSpacePaths::resolve()?;
    let mut projects: ProjectsFile = read_yaml_or_default(&paths.projects_config())?;

    if let Some(existing) = projects.projects.iter_mut().find(|p| p.root == root_text) {
        if let Some(name) = req.name.filter(|s| !s.trim().is_empty()) {
            existing.name = name;
        }
        existing.tags = req.tags;
        let entry = existing.clone();
        write_yaml_atomic(&paths.projects_config(), &projects)?;
        return Ok(json!({ "project": project_view(entry), "created": false }));
    }

    let entry = ProjectEntry {
        local_id: format!("prj_{}", uuid::Uuid::new_v4().simple()),
        name: req
            .name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| inferred_project_name(&root)),
        root: root_text,
        added_at: lillux::time::iso8601_now(),
        tags: req.tags,
    };
    projects.projects.push(entry.clone());
    projects.projects.sort_by(|a, b| a.name.cmp(&b.name));
    write_yaml_atomic(&paths.projects_config(), &projects)?;

    Ok(json!({ "project": project_view(entry), "created": true }))
}

pub async fn handle_projects_forget(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    ensure_writable_session(&ctx, &state)?;
    let req: ForgetProjectRequest = parse_request(params)?;
    if req.local_id.is_none() && req.root.is_none() {
        return Err(HandlerError::BadRequest("local_id or root is required".into()).into());
    }
    let root = match req.root.as_deref() {
        Some(root) => Some(canonical_project_root(root)?.display().to_string()),
        None => None,
    };

    let paths = UserSpacePaths::resolve()?;
    let mut projects: ProjectsFile = read_yaml_or_default(&paths.projects_config())?;
    let before = projects.projects.len();
    projects.projects.retain(|p| {
        req.local_id
            .as_deref()
            .is_some_and(|id| id == p.local_id)
            .then_some(false)
            .unwrap_or_else(|| root.as_deref().is_none_or(|r| r != p.root))
    });
    let removed = before - projects.projects.len();
    write_yaml_atomic(&paths.projects_config(), &projects)?;

    let mut recent: RecentFile = read_yaml_or_default(&paths.studio_recent())?;
    recent.recent_projects.retain(|r| {
        projects
            .projects
            .iter()
            .any(|project| project.local_id == r.local_id)
    });
    write_yaml_atomic(&paths.studio_recent(), &recent)?;

    Ok(json!({ "removed": removed }))
}

pub async fn handle_projects_resolve(
    params: Value,
    _ctx: HandlerContext,
    _state: Arc<AppState>,
) -> Result<Value> {
    let req: ResolveProjectRequest = parse_request(params)?;
    let paths = UserSpacePaths::resolve()?;
    let projects: ProjectsFile = read_yaml_or_default(&paths.projects_config())?;
    let project = projects
        .projects
        .into_iter()
        .find(|p| p.local_id == req.local_id)
        .ok_or(HandlerError::NotFound)?;
    Ok(json!({ "project": project_view(project) }))
}

pub async fn handle_recent_touch(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    ensure_writable_session(&ctx, &state)?;
    let req: TouchRecentRequest = parse_request(params)?;
    let paths = UserSpacePaths::resolve()?;
    let projects: ProjectsFile = read_yaml_or_default(&paths.projects_config())?;
    if !projects.projects.iter().any(|p| p.local_id == req.local_id) {
        return Err(HandlerError::NotFound.into());
    }

    let mut recent: RecentFile = read_yaml_or_default(&paths.studio_recent())?;
    recent
        .recent_projects
        .retain(|p| p.local_id != req.local_id);
    recent.recent_projects.insert(
        0,
        RecentProject {
            local_id: req.local_id,
            opened_at: lillux::time::iso8601_now(),
        },
    );
    recent.recent_projects.truncate(50);
    write_yaml_atomic(&paths.studio_recent(), &recent)?;
    Ok(json!({ "recent": recent.recent_projects }))
}

pub async fn handle_recent_list(
    _params: Value,
    _ctx: HandlerContext,
    _state: Arc<AppState>,
) -> Result<Value> {
    let paths = UserSpacePaths::resolve()?;
    let recent: RecentFile = read_yaml_or_default(&paths.studio_recent())?;
    Ok(json!(recent))
}

pub async fn handle_config_get(
    _params: Value,
    _ctx: HandlerContext,
    _state: Arc<AppState>,
) -> Result<Value> {
    let paths = UserSpacePaths::resolve()?;
    let config: StudioConfigFile = read_yaml_or_default(&paths.studio_config())?;
    Ok(json!(config))
}

pub async fn handle_config_update(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    ensure_writable_session(&ctx, &state)?;
    let req: UpdateConfigRequest = parse_request(params)?;
    let paths = UserSpacePaths::resolve()?;
    let mut config: StudioConfigFile = read_yaml_or_default(&paths.studio_config())?;
    if let Some(theme) = req.theme {
        config.theme = theme;
    }
    if let Some(landing_view) = req.landing_view {
        config.landing_view = landing_view;
    }
    if let Some(default_open_mode) = req.default_open_mode {
        config.default_open_mode = default_open_mode;
    }
    write_yaml_atomic(&paths.studio_config(), &config)?;
    Ok(json!(config))
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

fn session_id_from_context(ctx: &HandlerContext) -> Option<&str> {
    ctx.fingerprint.strip_prefix("session:")
}

fn ensure_writable_session(ctx: &HandlerContext, state: &AppState) -> Result<()> {
    let session_id = session_id_from_context(ctx)
        .ok_or_else(|| HandlerError::Forbidden("browser session required".into()))?;
    let session = get_ui_state(state)
        .expect("UiState not set")
        .browser_sessions
        .get_session(session_id)
        .ok_or(HandlerError::Forbidden("session expired or invalid".into()))?;
    if session.read_only {
        return Err(HandlerError::Forbidden("session is read-only".into()).into());
    }
    Ok(())
}

fn inferred_project_name(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("project")
        .to_string()
}

fn project_view(project: ProjectEntry) -> Value {
    let exists = Path::new(&project.root).is_dir();
    json!({
        "local_id": project.local_id,
        "name": project.name,
        "root": project.root,
        "added_at": project.added_at,
        "tags": project.tags,
        "exists": exists,
    })
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
    "service:ui/studio/projects/list",
    "ui.studio.projects.list",
    handle_projects_list
);
descriptor!(
    PROJECTS_ADD_DESCRIPTOR,
    "service:ui/studio/projects/add",
    "ui.studio.projects.add",
    handle_projects_add
);
descriptor!(
    PROJECTS_FORGET_DESCRIPTOR,
    "service:ui/studio/projects/forget",
    "ui.studio.projects.forget",
    handle_projects_forget
);
descriptor!(
    PROJECTS_RESOLVE_DESCRIPTOR,
    "service:ui/studio/projects/resolve",
    "ui.studio.projects.resolve",
    handle_projects_resolve
);
descriptor!(
    RECENT_TOUCH_DESCRIPTOR,
    "service:ui/studio/recent/touch",
    "ui.studio.recent.touch",
    handle_recent_touch
);
descriptor!(
    RECENT_LIST_DESCRIPTOR,
    "service:ui/studio/recent/list",
    "ui.studio.recent.list",
    handle_recent_list
);
descriptor!(
    CONFIG_GET_DESCRIPTOR,
    "service:ui/studio/config/get",
    "ui.studio.config.get",
    handle_config_get
);
descriptor!(
    CONFIG_UPDATE_DESCRIPTOR,
    "service:ui/studio/config/update",
    "ui.studio.config.update",
    handle_config_update
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
    fn relative_project_root_is_rejected() {
        let err = canonical_project_root("relative/path").unwrap_err();
        assert!(err.to_string().contains("project root must be absolute"));
    }

    #[test]
    fn project_view_marks_missing_roots_without_mutating_registry_entry() {
        let value = project_view(ProjectEntry {
            local_id: "prj_1".into(),
            name: "Missing".into(),
            root: "/definitely/missing/ryeos/project".into(),
            added_at: "2026-05-30T00:00:00Z".into(),
            tags: vec![],
        });
        assert_eq!(value["exists"], false);
        assert_eq!(value["root"], "/definitely/missing/ryeos/project");
    }
}
