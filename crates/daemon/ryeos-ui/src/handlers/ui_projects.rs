//! RyeOS UI local project registry and user RyeOS UI config handlers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::{HandlerError, parse_request};
use ryeos_app::principal::{
    HostedPrincipalResolver, LOCAL_PRINCIPAL_ID, LockedPrincipalStore, PrincipalStore,
};
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

use crate::seat_auth::require_seat_caller;
use crate::state::get_ui_state;

const PROJECTS_VERSION: u32 = 1;
const RYEOS_UI_CONFIG_VERSION: u32 = 1;
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
pub struct RyeOsConfigFile {
    pub version: u32,
    pub theme: String,
    pub landing_view: String,
    pub default_open_mode: String,
}

impl Default for RyeOsConfigFile {
    fn default() -> Self {
        Self {
            version: RYEOS_UI_CONFIG_VERSION,
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
    pub default_open_mode: Option<String>,
}

pub async fn handle_projects_list(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let caller = require_seat_caller(&ctx, &state)?;
    let project_path = string_param(&params, "project_path");
    let current_project = project_path.as_deref().or_else(|| caller.project_root());
    let store = resolve_principal_store(&ctx, &state)?;
    let projects = store.load_projects()?;
    let mut rows = projects.projects;
    if let Some(current) = current_project {
        if !rows
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
    if require_seat_caller(&ctx, &state)?.read_only() {
        return Err(HandlerError::Forbidden("session is read-only".into()).into());
    }
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
    if require_seat_caller(&ctx, &state)?.read_only() {
        return Err(HandlerError::Forbidden("session is read-only".into()).into());
    }
    let req: ForgetProjectRequest = parse_request(params)?;
    if req.local_id.is_none() && req.root.is_none() {
        return Err(HandlerError::BadRequest("local_id or root is required".into()).into());
    }
    let root = match (req.local_id.as_deref(), req.root.as_deref()) {
        (Some(_), _) => None,
        (None, Some(root)) => Some(project_root_locator_for_forget(root)?),
        (None, None) => None,
    };

    let store = locked_principal_store(&ctx, &state).await?;
    let mut projects = store.load_projects()?;
    let before = projects.projects.len();
    projects.projects.retain(|p| {
        if let Some(local_id) = req.local_id.as_deref() {
            p.local_id != local_id
        } else {
            root.as_deref().is_none_or(|r| r != p.root)
        }
    });
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
    let caller = require_seat_caller(&ctx, &state)?;
    if caller.read_only() {
        return Err(HandlerError::Forbidden("session is read-only".into()).into());
    }
    let req: OpenProjectRequest = parse_request(params)?;
    let current_project = caller.project_root().map(str::to_string);
    let store = locked_principal_store(&ctx, &state).await?;
    let projects = store.load_projects()?;
    let project = if req.local_id == "current" {
        let root = current_project.ok_or(HandlerError::NotFound)?;
        let path = PathBuf::from(&root);
        ProjectEntry {
            local_id: "current".to_string(),
            name: inferred_project_name(&path),
            root,
            added_at: String::new(),
            tags: Vec::new(),
        }
    } else {
        projects
            .projects
            .into_iter()
            .find(|p| p.local_id == req.local_id)
            .ok_or(HandlerError::NotFound)?
    };

    let canonical = canonical_project_root(&project.root)?;
    let root = canonical.display().to_string();
    let recent = if project.local_id == "current" {
        RecentFile::default()
    } else {
        store.touch_recent_project(&project.local_id)?
    };

    let session = if let Some(session_id) = session_id_from_context(&ctx) {
        let updated_session = get_ui_state(&state)
            .ok_or_else(|| HandlerError::Internal("UiState not set".into()))?
            .browser_sessions
            .set_project_root(session_id, Some(root.clone()))
            .ok_or(HandlerError::Forbidden("session expired or invalid".into()))?;
        json!({
            "session_id": updated_session.session_id,
            "project_root": updated_session.project_root,
            "read_only": updated_session.read_only,
        })
    } else {
        json!({
            "session_id": "",
            "project_root": root.clone(),
            "read_only": false,
        })
    };

    Ok(json!({
        "project": project_view(
            ProjectEntry { root: root.clone(), ..project.clone() },
            Some(&root),
            project.local_id != "current"
        ),
        "session": session,
        "recent": recent.recent_projects,
    }))
}

pub async fn handle_recent_touch(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    if require_seat_caller(&ctx, &state)?.read_only() {
        return Err(HandlerError::Forbidden("session is read-only".into()).into());
    }
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
    Ok(json!(config))
}

pub async fn handle_config_update(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    if require_seat_caller(&ctx, &state)?.read_only() {
        return Err(HandlerError::Forbidden("session is read-only".into()).into());
    }
    let req: UpdateConfigRequest = parse_request(params)?;
    if let Some(theme) = req.theme.as_deref() {
        validate_choice("theme", theme, &["system", "light", "dark"])?;
    }
    if let Some(landing_view) = req.landing_view.as_deref() {
        validate_choice("landing_view", landing_view, &["projects"])?;
    }
    if let Some(default_open_mode) = req.default_open_mode.as_deref() {
        validate_choice(
            "default_open_mode",
            default_open_mode,
            &["normal", "read_only"],
        )?;
    }
    let store = locked_principal_store(&ctx, &state).await?;
    let mut config = store.load_ui_config()?;
    if let Some(theme) = req.theme {
        config.theme = theme;
    }
    if let Some(landing_view) = req.landing_view {
        config.landing_view = landing_view;
    }
    if let Some(default_open_mode) = req.default_open_mode {
        config.default_open_mode = default_open_mode;
    }
    store.write_ui_config(&config)?;
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
        ensure_version("ryeos-ui.yaml", config.version, RYEOS_UI_CONFIG_VERSION)?;
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
        ensure_version("ryeos-ui.yaml", config.version, RYEOS_UI_CONFIG_VERSION)?;
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
    if let Some(user_principal_id) = session_user_principal_id(ctx, state)? {
        let resolver = HostedPrincipalResolver::for_app_root(&state.config.app_root);
        return PrincipalStore::resolve_with(&resolver, &user_principal_id);
    }
    PrincipalStore::resolve_principal(LOCAL_PRINCIPAL_ID)
}

async fn locked_principal_store(
    ctx: &HandlerContext,
    state: &AppState,
) -> Result<LockedPrincipalStore> {
    if let Some(user_principal_id) = session_user_principal_id(ctx, state)? {
        let resolver = HostedPrincipalResolver::for_app_root(&state.config.app_root);
        return PrincipalStore::locked_with(&resolver, &user_principal_id).await;
    }
    PrincipalStore::locked_principal(LOCAL_PRINCIPAL_ID).await
}

fn session_user_principal_id(ctx: &HandlerContext, state: &AppState) -> Result<Option<String>> {
    let Some(session_id) = session_id_from_context(ctx) else {
        return Ok(None);
    };
    let session = get_ui_state(state)
        .ok_or_else(|| HandlerError::Internal("UiState not set".into()))?
        .browser_sessions
        .get_session(session_id)
        .ok_or(HandlerError::Forbidden("session expired or invalid".into()))?;
    Ok(session.user_principal_id)
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

fn string_param(params: &Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
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
