//! Studio local project registry and user Studio config handlers.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

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

static USER_SPACE_YAML_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

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
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    ensure_read_session(&ctx, &state)?;
    let paths = UserSpacePaths::resolve()?;
    let projects = load_projects(&paths)?;
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
    let _guard = user_space_yaml_lock().lock().await;
    let req: AddProjectRequest = parse_request(params)?;
    let root = canonical_project_root(&req.root)?;
    let root_text = root.display().to_string();
    let paths = UserSpacePaths::resolve()?;
    let mut projects = load_projects(&paths)?;

    if let Some(existing) = projects.projects.iter_mut().find(|p| p.root == root_text) {
        if let Some(name) = req.name.filter(|s| !s.trim().is_empty()) {
            existing.name = name;
        }
        if let Some(tags) = req.tags {
            existing.tags = tags;
        }
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
        tags: req.tags.unwrap_or_default(),
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
    let _guard = user_space_yaml_lock().lock().await;
    let req: ForgetProjectRequest = parse_request(params)?;
    if req.local_id.is_none() && req.root.is_none() {
        return Err(HandlerError::BadRequest("local_id or root is required".into()).into());
    }
    let root = match (req.local_id.as_deref(), req.root.as_deref()) {
        (Some(_), _) => None,
        (None, Some(root)) => Some(project_root_locator_for_forget(root)?),
        (None, None) => None,
    };

    let paths = UserSpacePaths::resolve()?;
    let mut projects = load_projects(&paths)?;
    let before = projects.projects.len();
    projects.projects.retain(|p| {
        if let Some(local_id) = req.local_id.as_deref() {
            p.local_id != local_id
        } else {
            root.as_deref().is_none_or(|r| r != p.root)
        }
    });
    let removed = before - projects.projects.len();
    write_yaml_atomic(&paths.projects_config(), &projects)?;

    let mut recent = load_recent(&paths)?;
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
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    ensure_read_session(&ctx, &state)?;
    let req: ResolveProjectRequest = parse_request(params)?;
    let paths = UserSpacePaths::resolve()?;
    let projects = load_projects(&paths)?;
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
    let _guard = user_space_yaml_lock().lock().await;
    let req: TouchRecentRequest = parse_request(params)?;
    let paths = UserSpacePaths::resolve()?;
    let projects = load_projects(&paths)?;
    if !projects.projects.iter().any(|p| p.local_id == req.local_id) {
        return Err(HandlerError::NotFound.into());
    }

    let mut recent = load_recent(&paths)?;
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
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    ensure_read_session(&ctx, &state)?;
    let paths = UserSpacePaths::resolve()?;
    let recent = load_recent(&paths)?;
    Ok(json!(recent))
}

pub async fn handle_config_get(
    _params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    ensure_read_session(&ctx, &state)?;
    let paths = UserSpacePaths::resolve()?;
    let config = load_studio_config(&paths)?;
    Ok(json!(config))
}

pub async fn handle_config_update(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    ensure_writable_session(&ctx, &state)?;
    let _guard = user_space_yaml_lock().lock().await;
    let req: UpdateConfigRequest = parse_request(params)?;
    let paths = UserSpacePaths::resolve()?;
    let mut config = load_studio_config(&paths)?;
    if let Some(theme) = req.theme {
        validate_choice("theme", &theme, &["system", "light", "dark"])?;
        config.theme = theme;
    }
    if let Some(landing_view) = req.landing_view {
        validate_choice("landing_view", &landing_view, &["projects"])?;
        config.landing_view = landing_view;
    }
    if let Some(default_open_mode) = req.default_open_mode {
        validate_choice(
            "default_open_mode",
            &default_open_mode,
            &["normal", "read_only"],
        )?;
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

fn user_space_yaml_lock() -> &'static Mutex<()> {
    USER_SPACE_YAML_LOCK.get_or_init(|| Mutex::new(()))
}

fn load_projects(paths: &UserSpacePaths) -> Result<ProjectsFile> {
    let projects: ProjectsFile = read_yaml_or_default(&paths.projects_config())?;
    ensure_version("projects.yaml", projects.version, PROJECTS_VERSION)?;
    Ok(projects)
}

fn load_studio_config(paths: &UserSpacePaths) -> Result<StudioConfigFile> {
    let config: StudioConfigFile = read_yaml_or_default(&paths.studio_config())?;
    ensure_version("studio.yaml", config.version, STUDIO_CONFIG_VERSION)?;
    Ok(config)
}

fn load_recent(paths: &UserSpacePaths) -> Result<RecentFile> {
    let recent: RecentFile = read_yaml_or_default(&paths.studio_recent())?;
    ensure_version("recent.yaml", recent.version, RECENT_VERSION)?;
    Ok(recent)
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

fn ensure_read_session(ctx: &HandlerContext, state: &AppState) -> Result<()> {
    let session_id = session_id_from_context(ctx)
        .ok_or_else(|| HandlerError::Forbidden("browser session required".into()))?;
    get_ui_state(state)
        .ok_or_else(|| HandlerError::Internal("UiState not set".into()))?
        .browser_sessions
        .get_session(session_id)
        .ok_or(HandlerError::Forbidden("session expired or invalid".into()))?;
    Ok(())
}

fn ensure_writable_session(ctx: &HandlerContext, state: &AppState) -> Result<()> {
    let session_id = session_id_from_context(ctx)
        .ok_or_else(|| HandlerError::Forbidden("browser session required".into()))?;
    let session = get_ui_state(state)
        .ok_or_else(|| HandlerError::Internal("UiState not set".into()))?
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
    fn forget_root_locator_accepts_missing_absolute_paths() {
        let locator = project_root_locator_for_forget("/definitely/missing/ryeos/project")
            .expect("missing absolute path should still be usable for forget");
        assert_eq!(locator, "/definitely/missing/ryeos/project");
    }

    #[test]
    fn unsupported_versions_are_rejected() {
        let err = ensure_version("projects.yaml", 2, 1).unwrap_err();
        assert!(err
            .to_string()
            .contains("unsupported projects.yaml version 2"));
    }

    #[test]
    fn invalid_config_choices_are_rejected() {
        let err = validate_choice("theme", "neon", &["system", "light", "dark"]).unwrap_err();
        assert!(err.to_string().contains("invalid theme value 'neon'"));
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
