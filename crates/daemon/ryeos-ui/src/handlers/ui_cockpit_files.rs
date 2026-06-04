//! `ui.cockpit.files.list` and `ui.cockpit.files.read` — safe scoped
//! file browsing for the cockpit.
//!
//! All file access is constrained to allowed roots derived from the
//! browser session's project path. No arbitrary absolute path reads.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

use crate::state::get_ui_state;

/// Maximum file read size (256 KiB).
const MAX_READ_BYTES: usize = 256 * 1024;
/// Maximum directory entries returned from a single files.list call.
const MAX_LIST_ENTRIES: usize = 2_000;

fn session_id_from_context(ctx: &HandlerContext) -> Option<String> {
    ctx.fingerprint.strip_prefix("session:").map(String::from)
}

/// Resolve the allowed root for a given root type + session project.
fn resolve_allowed_root(root_type: &str, project_path: Option<&str>) -> Result<PathBuf> {
    match root_type {
        "project" => project_path
            .map(PathBuf::from)
            .context("no project bound to this session"),
        "project_ai" => project_path
            .map(|p| PathBuf::from(p).join(".ai"))
            .context("no project bound to this session"),
        _ => anyhow::bail!(
            "unknown root type '{}': allowed roots are 'project' and 'project_ai'",
            root_type
        ),
    }
}

/// Canonicalize `requested` and verify it stays under `root`.
/// Returns the canonical path on success.
fn safe_path(root: &Path, requested: &str) -> Result<PathBuf> {
    let root_canonical = root.canonicalize().context("allowed root does not exist")?;

    // Join and canonicalize to resolve any `..` traversal.
    let joined = root_canonical.join(requested);
    let canonical = joined.canonicalize().context("path does not exist")?;

    // Verify the canonical path starts with the root.
    if !canonical.starts_with(&root_canonical) {
        anyhow::bail!("path escapes allowed root");
    }

    Ok(canonical)
}

// ── files.list ────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesListRequest {
    pub root: String,
    #[serde(default)]
    pub path: String,
}

pub async fn handle_files_list(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let session_id = session_id_from_context(&ctx)
        .ok_or_else(|| HandlerError::Forbidden("browser session required".into()))?;

    let session = get_ui_state(&state)
        .expect("UiState not set")
        .browser_sessions
        .get_session(&session_id)
        .ok_or(HandlerError::Forbidden("session expired or invalid".into()))?;

    let req: FilesListRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;

    let allowed_root = resolve_allowed_root(&req.root, session.project_root.as_deref())
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;

    let safe =
        safe_path(&allowed_root, &req.path).map_err(|e| HandlerError::BadRequest(e.to_string()))?;

    if !safe.is_dir() {
        return Err(HandlerError::BadRequest("path is not a directory".into()).into());
    }

    let mut entries: Vec<Value> = Vec::new();
    let dir = std::fs::read_dir(&safe)?;
    for entry in dir.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let metadata = std::fs::symlink_metadata(&path).ok();
        let is_dir = metadata.as_ref().is_some_and(|meta| meta.is_dir());

        let mut entry_val = serde_json::json!({
            "name": name,
            "is_dir": is_dir,
        });

        if let Some(meta) = metadata {
            entry_val["size"] = serde_json::json!(meta.len());
            if let Ok(modified) = meta.modified() {
                entry_val["modified"] = serde_json::json!(format!("{:?}", modified));
            }
        }

        entries.push(entry_val);

        if entries.len() >= MAX_LIST_ENTRIES {
            break;
        }
    }

    entries.sort_by(|a, b| {
        // Directories first, then alphabetical
        let a_dir = a["is_dir"].as_bool().unwrap_or(false);
        let b_dir = b["is_dir"].as_bool().unwrap_or(false);
        if a_dir != b_dir {
            return if a_dir {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        a["name"].as_str().cmp(&b["name"].as_str())
    });

    Ok(serde_json::json!({
        "root": req.root,
        "path": req.path,
        "truncated": entries.len() >= MAX_LIST_ENTRIES,
        "entries": entries,
    }))
}

// ── files.read ────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesReadRequest {
    pub root: String,
    pub path: String,
}

pub async fn handle_files_read(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let session_id = session_id_from_context(&ctx)
        .ok_or_else(|| HandlerError::Forbidden("browser session required".into()))?;

    let session = get_ui_state(&state)
        .expect("UiState not set")
        .browser_sessions
        .get_session(&session_id)
        .ok_or(HandlerError::Forbidden("session expired or invalid".into()))?;

    let req: FilesReadRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;

    let allowed_root = resolve_allowed_root(&req.root, session.project_root.as_deref())
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;

    let safe =
        safe_path(&allowed_root, &req.path).map_err(|e| HandlerError::BadRequest(e.to_string()))?;

    if !safe.is_file() {
        return Err(HandlerError::BadRequest("path is not a file".into()).into());
    }

    let metadata = std::fs::metadata(&safe)?;
    let size = metadata.len() as usize;
    let truncated = size > MAX_READ_BYTES;

    use std::io::Read;
    let file = std::fs::File::open(&safe)?;
    let mut buf = Vec::with_capacity(std::cmp::min(size, MAX_READ_BYTES));
    file.take(MAX_READ_BYTES as u64).read_to_end(&mut buf)?;
    let content = String::from_utf8_lossy(&buf).into_owned();

    Ok(serde_json::json!({
        "root": req.root,
        "path": req.path,
        "size": size,
        "truncated": truncated,
        "content": content,
    }))
}

// ── Descriptors ────────────────────────────────────────────────────

pub const FILES_LIST_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/cockpit/files/list",
    endpoint: "ui.cockpit.files.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_files_list(params, ctx, state).await })
    },
};

pub const STUDIO_FILES_LIST_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/studio/files/list",
    endpoint: "ui.studio.files.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_files_list(params, ctx, state).await })
    },
};

pub const FILES_READ_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/cockpit/files/read",
    endpoint: "ui.cockpit.files.read",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_files_read(params, ctx, state).await })
    },
};

pub const STUDIO_FILES_READ_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/studio/files/read",
    endpoint: "ui.studio.files.read",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_files_read(params, ctx, state).await })
    },
};
