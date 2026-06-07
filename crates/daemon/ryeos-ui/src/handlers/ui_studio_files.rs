//! `ui.studio.files.list` and `ui.studio.files.read` — safe scoped
//! file browsing for the studio.
//!
//! All file access is constrained to allowed roots derived from the
//! browser session's project path. No arbitrary absolute path reads.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
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
/// Maximum file-space atlas entries returned from a recursive tree call.
const MAX_TREE_ENTRIES: usize = 3_000;
/// Maximum recursive depth for file-space tree snapshots.
const MAX_TREE_DEPTH: usize = 12;

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

// ── files.tree ────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesTreeRequest {
    pub root: String,
    #[serde(default)]
    pub path: String,
    #[serde(default = "default_tree_depth")]
    pub max_depth: usize,
    #[serde(default = "default_tree_entries")]
    pub max_entries: usize,
}

#[derive(Debug, Serialize)]
struct FileSpaceEntry {
    path: String,
    name: String,
    is_dir: bool,
    size: Option<u64>,
    modified: Option<String>,
}

pub async fn handle_files_tree(
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

    let req: FilesTreeRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;

    let allowed_root = resolve_allowed_root(&req.root, session.project_root.as_deref())
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;
    let root_canonical = allowed_root
        .canonicalize()
        .map_err(|e| HandlerError::BadRequest(format!("allowed root does not exist: {e}")))?;
    let safe =
        safe_path(&allowed_root, &req.path).map_err(|e| HandlerError::BadRequest(e.to_string()))?;
    if !safe.is_dir() {
        return Err(HandlerError::BadRequest("path is not a directory".into()).into());
    }

    let max_depth = req.max_depth.clamp(1, MAX_TREE_DEPTH);
    let max_entries = req.max_entries.clamp(1, MAX_TREE_ENTRIES);
    let mut entries = Vec::new();
    let mut truncated = false;
    collect_tree_entries(
        &root_canonical,
        &safe,
        0,
        max_depth,
        max_entries,
        &mut entries,
        &mut truncated,
    )?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(serde_json::json!({
        "schema_version": "studio.file_space.v1",
        "root": req.root,
        "path": req.path,
        "max_depth": max_depth,
        "max_entries": max_entries,
        "truncated": truncated,
        "watchable": false,
        "supports_expand": true,
        "ignore_mode": "built_in",
        "entries": entries,
    }))
}

fn collect_tree_entries(
    root: &Path,
    dir: &Path,
    depth: usize,
    max_depth: usize,
    max_entries: usize,
    out: &mut Vec<FileSpaceEntry>,
    truncated: &mut bool,
) -> Result<()> {
    if *truncated || depth >= max_depth {
        return Ok(());
    }

    let Ok(read_dir) = std::fs::read_dir(dir) else {
        *truncated = true;
        return Ok(());
    };
    let mut entries = read_dir.filter_map(Result::ok).collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        if out.len() >= max_entries {
            *truncated = true;
            break;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if should_skip_tree_entry(&name) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let is_dir = metadata.is_dir();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        out.push(FileSpaceEntry {
            path: rel,
            name,
            is_dir,
            size: (!is_dir).then_some(metadata.len()),
            modified: metadata
                .modified()
                .ok()
                .map(|modified| format!("{:?}", modified)),
        });
        if is_dir && !metadata.file_type().is_symlink() {
            collect_tree_entries(
                root,
                &path,
                depth + 1,
                max_depth,
                max_entries,
                out,
                truncated,
            )?;
        }
        if *truncated {
            break;
        }
    }
    Ok(())
}

fn should_skip_tree_entry(name: &str) -> bool {
    matches!(
        name,
        ".git" | "target" | "node_modules" | "dist" | "build" | ".next" | ".cache"
    )
}

fn default_tree_depth() -> usize {
    8
}

fn default_tree_entries() -> usize {
    MAX_TREE_ENTRIES
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
    service_ref: "service:ui/studio/files/list",
    endpoint: "ui.studio.files.list",
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
    service_ref: "service:ui/studio/files/read",
    endpoint: "ui.studio.files.read",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_files_read(params, ctx, state).await })
    },
};

pub const FILES_TREE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/studio/files/tree",
    endpoint: "ui.studio.files.tree",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_files_tree(params, ctx, state).await })
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
