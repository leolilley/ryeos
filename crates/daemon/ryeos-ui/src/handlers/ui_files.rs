//! `ui.ryeos.files.list` and `ui.ryeos.files.read` — safe scoped
//! file browsing for the ryeos-ui.
//!
//! All file access is constrained to allowed roots derived from the
//! browser session's project path. No arbitrary absolute path reads.
//! Browser renderers reach these services only through compiled binding
//! coordinates. The direct HTTP route is a signed-operator lane; either lane
//! is converted to a pinned project directory before any traversal.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use lillux::{PinnedDirectory, PinnedEntryType};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

/// Maximum file read size (256 KiB).
const MAX_READ_BYTES: usize = 256 * 1024;
/// Maximum directory entries returned from a single files.list call.
const MAX_LIST_ENTRIES: usize = 2_000;
/// Maximum file-space atlas entries returned from a recursive tree call.
const MAX_TREE_ENTRIES: usize = 3_000;
/// Maximum recursive depth for file-space tree snapshots.
const MAX_TREE_DEPTH: usize = 12;

fn resolve_allowed_root(
    root_type: &str,
    caller: &crate::seat_auth::SeatCaller,
    operator_project_path: Option<&str>,
) -> Result<PinnedDirectory> {
    let project = match caller.project_access()? {
        Some(project) => project.try_clone_directory()?,
        None => {
            let path = operator_project_path
                .ok_or_else(|| anyhow::anyhow!("no project bound to this invocation"))?;
            PinnedDirectory::open(Path::new(path))?
                .ok_or_else(|| anyhow::anyhow!("operator-selected project does not exist"))?
        }
    };
    match root_type {
        "project" => Ok(project),
        "project_ai" => project
            .open_child_directory(std::ffi::OsStr::new(".ai"))?
            .ok_or_else(|| anyhow::anyhow!("project .ai directory does not exist")),
        _ => anyhow::bail!(
            "unknown root type '{}': allowed roots are 'project' and 'project_ai'",
            root_type
        ),
    }
}

fn open_relative_directory(root: &PinnedDirectory, relative: &str) -> Result<PinnedDirectory> {
    let mut directory = root.try_clone()?;
    for component in Path::new(relative).components() {
        let Component::Normal(name) = component else {
            anyhow::bail!("directory path is not a normalized relative path");
        };
        directory = directory
            .open_child_directory(name)?
            .ok_or_else(|| anyhow::anyhow!("directory path does not exist"))?;
    }
    Ok(directory)
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
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let operator_project_path = params
        .get("project_path")
        .and_then(Value::as_str)
        .map(String::from);
    let mut params = params;
    if let Some(map) = params.as_object_mut() {
        map.remove("project_path");
    }

    let req: FilesListRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;

    let allowed_root = resolve_allowed_root(&req.root, &caller, operator_project_path.as_deref())
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;
    let directory = open_relative_directory(&allowed_root, &req.path)
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;

    let mut entries: Vec<Value> = Vec::new();
    let observed = directory.entries_no_follow_bounded(MAX_LIST_ENTRIES + 1)?;
    let truncated = observed.len() > MAX_LIST_ENTRIES;
    for entry in observed.into_iter().take(MAX_LIST_ENTRIES) {
        let name = entry.name.to_string_lossy().into_owned();
        let is_dir = entry.entry_type == PinnedEntryType::Directory;

        let mut entry_val = serde_json::json!({
            "name": name,
            "is_dir": is_dir,
        });
        if entry.entry_type == PinnedEntryType::Regular
            && let Some(file) = directory.open_pinned_regular(&entry.name, false)?
        {
            entry_val["size"] = serde_json::json!(file.size()?);
        }
        entries.push(entry_val);
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
        "truncated": truncated,
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
    modified: Option<u64>,
}

pub async fn handle_files_tree(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let operator_project_path = params
        .get("project_path")
        .and_then(Value::as_str)
        .map(String::from);
    let mut params = params;
    if let Some(map) = params.as_object_mut() {
        map.remove("project_path");
    }

    let req: FilesTreeRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;

    let allowed_root = resolve_allowed_root(&req.root, &caller, operator_project_path.as_deref())
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;
    let directory = open_relative_directory(&allowed_root, &req.path)
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;

    let max_depth = req.max_depth.clamp(1, MAX_TREE_DEPTH);
    let max_entries = req.max_entries.clamp(1, MAX_TREE_ENTRIES);
    let ignore = &state
        .node_policy
        .require::<ryeos_app::node_policy::sections::ingest_ignore::CompiledIngestIgnorePolicy>()?
        .matcher;
    let mut entries = Vec::new();
    let mut truncated = false;
    let policy_relative = match req.root.as_str() {
        "project" => PathBuf::from(&req.path),
        "project_ai" => Path::new(".ai").join(&req.path),
        _ => unreachable!("root type was validated above"),
    };
    collect_tree_entries(
        &directory,
        Path::new(&req.path),
        &policy_relative,
        0,
        max_depth,
        max_entries,
        ignore,
        &mut entries,
        &mut truncated,
    )?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(serde_json::json!({
        "schema_version": "ryeos.ui.file_space.v1",
        "root": req.root,
        "path": req.path,
        "max_depth": max_depth,
        "max_entries": max_entries,
        "truncated": truncated,
        "watchable": false,
        "supports_expand": true,
        "ignore_mode": "node_policy",
        "entries": entries,
    }))
}

fn collect_tree_entries(
    directory: &PinnedDirectory,
    relative_directory: &Path,
    policy_relative_directory: &Path,
    depth: usize,
    max_depth: usize,
    max_entries: usize,
    ignore: &ryeos_app::ignore::IgnoreMatcher,
    out: &mut Vec<FileSpaceEntry>,
    truncated: &mut bool,
) -> Result<()> {
    if *truncated || depth >= max_depth {
        return Ok(());
    }

    let remaining = max_entries.saturating_sub(out.len());
    let entries = directory.entries_no_follow_bounded(remaining.saturating_add(1))?;
    if entries.len() > remaining {
        *truncated = true;
    }
    for entry in entries.into_iter().take(remaining) {
        if out.len() >= max_entries {
            *truncated = true;
            break;
        }
        let name = entry.name.to_string_lossy().into_owned();
        let relative = relative_directory.join(&entry.name);
        let policy_relative = policy_relative_directory.join(&entry.name);
        let relative_text = relative.to_string_lossy().replace('\\', "/");
        if ignore.is_ignored(&policy_relative.to_string_lossy().replace('\\', "/")) {
            continue;
        }
        let is_dir = entry.entry_type == PinnedEntryType::Directory;
        let size = if entry.entry_type == PinnedEntryType::Regular {
            directory
                .open_pinned_regular(&entry.name, false)?
                .map(|file| file.size())
                .transpose()?
        } else {
            None
        };
        out.push(FileSpaceEntry {
            path: relative_text,
            name,
            is_dir,
            size,
            modified: None,
        });
        if is_dir {
            let child = directory
                .open_child_directory(&entry.name)?
                .ok_or_else(|| anyhow::anyhow!("directory changed during file-tree traversal"))?;
            collect_tree_entries(
                &child,
                &relative,
                &policy_relative,
                depth + 1,
                max_depth,
                max_entries,
                ignore,
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
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let operator_project_path = params
        .get("project_path")
        .and_then(Value::as_str)
        .map(String::from);
    let mut params = params;
    if let Some(map) = params.as_object_mut() {
        map.remove("project_path");
    }

    let req: FilesReadRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;

    let allowed_root = resolve_allowed_root(&req.root, &caller, operator_project_path.as_deref())
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;
    let file = allowed_root
        .open_pinned_regular_descendant(Path::new(&req.path), false)?
        .ok_or_else(|| HandlerError::BadRequest("path is not a regular file".into()))?;
    let size = file.size()?;
    let truncated = size > MAX_READ_BYTES as u64;
    let mut buf = file.read_bounded(MAX_READ_BYTES as u64 + 1)?;
    buf.truncate(MAX_READ_BYTES);
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
    service_ref: "service:ui/ryeos-ui/files/list",
    endpoint: "ui.ryeos.files.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_files_list(params, ctx, state).await })
    },
};

pub const FILES_READ_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/files/read",
    endpoint: "ui.ryeos.files.read",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_files_read(params, ctx, state).await })
    },
};

pub const FILES_TREE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/files/tree",
    endpoint: "ui.ryeos.files.tree",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_files_tree(params, ctx, state).await })
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn workspace_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("workspace root")
            .to_path_buf()
    }

    #[test]
    fn bundled_file_routes_require_a_signed_principal() {
        // Browser renderers use compiled binding coordinates. These direct
        // routes are the explicit operator lane and therefore require a
        // signed caller; their project root is pinned before traversal.
        let routes = [
            "bundles/ryeos-ui/.ai/node/routes/ui/ryeos-ui/files-list.yaml",
            "bundles/ryeos-ui/.ai/node/routes/ui/ryeos-ui/files-read.yaml",
            "bundles/ryeos-ui/.ai/node/routes/ui/ryeos-ui/files-tree.yaml",
        ];
        let service_refs = [
            "service:ui/ryeos-ui/files/list",
            "service:ui/ryeos-ui/files/read",
            "service:ui/ryeos-ui/files/tree",
        ];

        for (route, service_ref) in routes.into_iter().zip(service_refs) {
            let contents = std::fs::read_to_string(workspace_root().join(route))
                .unwrap_or_else(|err| panic!("read {route}: {err}"));
            let yaml: Value = serde_yaml::from_str(&contents)
                .unwrap_or_else(|err| panic!("parse {route}: {err}"));

            assert_eq!(yaml["auth"], "ryeos_signed", "{route}");
            assert_eq!(yaml["response"]["source"], service_ref, "{route}");
        }
    }

    #[test]
    fn retained_project_descriptor_cannot_be_redirected_by_path_replacement() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let original = temporary.path().join("project");
        let moved = temporary.path().join("project-moved");
        std::fs::create_dir(&original).expect("create original project");
        std::fs::write(original.join("proof.txt"), b"original").expect("write original file");
        let pinned = PinnedDirectory::open(&original)
            .expect("pin original project")
            .expect("original project exists");

        std::fs::rename(&original, &moved).expect("replace project path");
        std::fs::create_dir(&original).expect("create replacement project");
        std::fs::write(original.join("proof.txt"), b"replacement").expect("write replacement");

        let file = pinned
            .open_pinned_regular_descendant(Path::new("proof.txt"), false)
            .expect("descriptor-relative lookup")
            .expect("original file remains reachable");
        assert_eq!(
            file.read_bounded(32).expect("read pinned file"),
            b"original"
        );
        assert!(open_relative_directory(&pinned, "../project/proof.txt").is_err());
    }
}
