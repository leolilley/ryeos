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

use anyhow::{Context as _, Result};
use lillux::{
    PinnedDirectory, PinnedEntryType, PrefixDirectoryCoverage, PrefixWalkBudget,
    PrefixWalkDecision, PrefixWalkDirectory, PrefixWalkExhaustion,
};
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
/// Kernel work available to one files.list call: directory opens, names read
/// and entries inspected.
const MAX_LIST_WORK: usize = 65_536;
/// Maximum file-space atlas entries returned from a recursive tree call.
const MAX_TREE_ENTRIES: usize = 3_000;
/// Kernel work available to one files.tree call, counted as for files.list.
const MAX_TREE_WORK: usize = 131_072;
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

/// Open `relative` below `root` one normal component at a time. The returned
/// path is rebuilt from exactly the components opened, so callers report and
/// match policy against the directory actually reached rather than the raw
/// request text.
fn open_relative_directory(
    root: &PinnedDirectory,
    relative: &str,
) -> Result<(PinnedDirectory, PathBuf)> {
    let mut directory = root.try_clone()?;
    let mut opened = PathBuf::new();
    for component in Path::new(relative).components() {
        let Component::Normal(name) = component else {
            anyhow::bail!("directory path is not a normalized relative path");
        };
        directory = directory
            .open_child_directory(name)?
            .ok_or_else(|| anyhow::anyhow!("directory path does not exist"))?;
        opened.push(name);
    }
    Ok((directory, opened))
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
    let (directory, _) = open_relative_directory(&allowed_root, &req.path)
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;

    let listing = tokio::task::spawn_blocking(move || {
        list_directory(&directory, MAX_LIST_ENTRIES, MAX_LIST_WORK)
    })
    .await
    .context("files.list directory worker stopped")??;

    Ok(serde_json::json!({
        "root": req.root,
        "path": req.path,
        "truncated": listing.truncated,
        "coverage": listing.coverage,
        "entries": listing.entries,
    }))
}

struct DirectoryListing {
    entries: Vec<Value>,
    /// True unless every immediate entry was reported from an unchanged
    /// directory.
    truncated: bool,
    coverage: Value,
}

/// List the immediate entries of one pinned directory as a bounded prefix.
/// Child directories are reported but never opened.
fn list_directory(
    directory: &PinnedDirectory,
    max_entries: usize,
    max_work: usize,
) -> Result<DirectoryListing> {
    let walk = directory.walk_prefix_no_follow(
        PrefixWalkBudget {
            max_depth: 1,
            max_entries,
            max_work,
        },
        |_, _| PrefixWalkDecision::Include,
    )?;
    let mut entries: Vec<Value> = walk
        .entries
        .iter()
        .map(|entry| {
            let mut value = serde_json::json!({
                "name": entry.metadata.name.to_string_lossy(),
                "is_dir": entry.metadata.entry_type == PinnedEntryType::Directory,
            });
            if let Some(size) = entry.size {
                value["size"] = serde_json::json!(size);
            }
            value
        })
        .collect();

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

    let root = walk.root();
    Ok(DirectoryListing {
        entries,
        truncated: walk.exhausted.is_some() || !root.is_complete(),
        coverage: serde_json::json!({
            "exhausted": exhaustion_label(walk.exhausted),
            "work_used": walk.work_used,
            "max_work": max_work,
            "directory": directory_coverage(root),
        }),
    })
}

fn coverage_label(coverage: PrefixDirectoryCoverage) -> &'static str {
    match coverage {
        PrefixDirectoryCoverage::Complete => "complete",
        PrefixDirectoryCoverage::NamePrefix => "name_prefix",
        PrefixDirectoryCoverage::StreamPrefix => "stream_prefix",
        PrefixDirectoryCoverage::DepthLimit => "depth_limit",
        PrefixDirectoryCoverage::NotVisited => "not_visited",
        PrefixDirectoryCoverage::Replaced => "replaced",
        PrefixDirectoryCoverage::Failed => "failed",
    }
}

fn exhaustion_label(exhausted: Option<PrefixWalkExhaustion>) -> Option<&'static str> {
    exhausted.map(|exhausted| match exhausted {
        PrefixWalkExhaustion::Entries => "entries",
        PrefixWalkExhaustion::Work => "work",
    })
}

fn directory_coverage(directory: &PrefixWalkDirectory) -> Value {
    serde_json::json!({
        "coverage": coverage_label(directory.coverage),
        "names_read": directory.names_read,
        "reported": directory.reported,
        "excluded": directory.excluded,
        "vanished": directory.vanished,
        "failed_entries": directory.failed_entries,
        "changed": directory.changed,
        "failure": directory.failure,
    })
}

fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
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
    let (directory, relative_directory) = open_relative_directory(&allowed_root, &req.path)
        .map_err(|e| HandlerError::BadRequest(e.to_string()))?;

    let max_depth = req.max_depth.clamp(1, MAX_TREE_DEPTH);
    let max_entries = req.max_entries.clamp(1, MAX_TREE_ENTRIES);
    let ignore = state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::ingest_ignore::CompiledIngestIgnorePolicy>()?
        .matcher
        .clone();
    let policy_relative = match req.root.as_str() {
        "project" => relative_directory.clone(),
        "project_ai" => Path::new(".ai").join(&relative_directory),
        _ => unreachable!("root type was validated above"),
    };
    let mut tree = tokio::task::spawn_blocking(move || {
        collect_tree_entries(
            &directory,
            &relative_directory,
            &policy_relative,
            max_depth,
            max_entries,
            MAX_TREE_WORK,
            &ignore,
        )
    })
    .await
    .context("files.tree directory worker stopped")??;
    tree.entries.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(serde_json::json!({
        "schema_version": "ryeos.ui.file_space.v1",
        "root": req.root,
        "path": req.path,
        "max_depth": max_depth,
        "max_entries": max_entries,
        "truncated": tree.truncated,
        "coverage": tree.coverage,
        "watchable": false,
        "supports_expand": true,
        "ignore_mode": "node_policy",
        "entries": tree.entries,
    }))
}

struct FileTree {
    entries: Vec<FileSpaceEntry>,
    /// True unless every included entry at every depth was reported from
    /// unchanged directories. Directories left unopened at the depth limit
    /// make a tree truncated.
    truncated: bool,
    coverage: Value,
}

/// Collect a bounded, ignore-filtered prefix of the tree below `directory`.
/// Every directory that is not completely covered is listed in the coverage
/// with the reason, so callers can expand or retry exactly those paths.
fn collect_tree_entries(
    directory: &PinnedDirectory,
    relative_directory: &Path,
    policy_relative_directory: &Path,
    max_depth: usize,
    max_entries: usize,
    max_work: usize,
    ignore: &ryeos_app::ignore::IgnoreMatcher,
) -> Result<FileTree> {
    let walk = directory.walk_prefix_no_follow(
        PrefixWalkBudget {
            max_depth,
            max_entries,
            max_work,
        },
        |relative, _| {
            if ignore.is_ignored(&slash_path(&policy_relative_directory.join(relative))) {
                PrefixWalkDecision::Exclude
            } else {
                PrefixWalkDecision::Include
            }
        },
    )?;
    let below = |relative: &Path| {
        if relative.as_os_str().is_empty() {
            relative_directory.to_path_buf()
        } else {
            relative_directory.join(relative)
        }
    };
    let entries = walk
        .entries
        .iter()
        .map(|entry| FileSpaceEntry {
            path: slash_path(&below(entry.relative_path.as_path())),
            name: entry.metadata.name.to_string_lossy().into_owned(),
            is_dir: entry.metadata.entry_type == PinnedEntryType::Directory,
            size: entry.size,
            modified: None,
        })
        .collect();
    let incomplete_directories: Vec<Value> = walk
        .directories
        .iter()
        .filter(|directory| !directory.is_complete())
        .map(|directory| {
            let mut value = directory_coverage(directory);
            let path = slash_path(&below(directory.relative_path.as_path()));
            value["path"] = serde_json::json!(path);
            value
        })
        .collect();
    let complete = walk.is_complete();
    Ok(FileTree {
        entries,
        truncated: !complete,
        coverage: serde_json::json!({
            "complete": complete,
            "exhausted": exhaustion_label(walk.exhausted),
            "work_used": walk.work_used,
            "max_work": max_work,
            "incomplete_directories": incomplete_directories,
        }),
    })
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
    let buf = file.read_prefix(MAX_READ_BYTES)?;
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

    fn ignore_matcher(patterns: &[&str]) -> ryeos_app::ignore::IgnoreMatcher {
        ryeos_app::ignore::IgnoreMatcher::from_config(&ryeos_app::ignore::IgnoreConfig {
            patterns: patterns.iter().map(|pattern| pattern.to_string()).collect(),
        })
        .expect("compile ignore patterns")
    }

    fn pinned(path: &Path) -> PinnedDirectory {
        PinnedDirectory::open(path)
            .expect("pin directory")
            .expect("directory exists")
    }

    fn tree_paths(tree: &FileTree) -> Vec<String> {
        tree.entries.iter().map(|entry| entry.path.clone()).collect()
    }

    #[test]
    fn tree_fills_its_entry_budget_from_an_oversized_directory() {
        // Ten names against a budget of three: the tree reports three entries
        // instead of refusing the directory or stopping after the first one.
        let temporary = tempfile::tempdir().expect("temporary root");
        for index in 0..10 {
            std::fs::write(temporary.path().join(format!("f{index}")), b"").expect("write");
        }

        let tree = collect_tree_entries(
            &pinned(temporary.path()),
            Path::new(""),
            Path::new(""),
            4,
            3,
            1_000,
            &ignore_matcher(&[]),
        )
        .expect("tree");

        assert_eq!(tree_paths(&tree), ["f0", "f1", "f2"]);
        assert!(tree.truncated);
        assert_eq!(tree.coverage["exhausted"], "entries");
        assert_eq!(
            tree.coverage["incomplete_directories"][0]["coverage"],
            "name_prefix"
        );
    }

    #[test]
    fn tree_descends_and_continues_after_a_nested_directory() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir_all(temporary.path().join("a/b")).expect("create tree");
        std::fs::write(temporary.path().join("a/b/leaf"), b"leaf").expect("write leaf");
        std::fs::write(temporary.path().join("a/x"), b"").expect("write x");
        std::fs::write(temporary.path().join("z"), b"").expect("write z");

        let tree = collect_tree_entries(
            &pinned(temporary.path()),
            Path::new("sub"),
            Path::new("sub"),
            4,
            100,
            1_000,
            &ignore_matcher(&[]),
        )
        .expect("tree");

        assert_eq!(
            tree_paths(&tree),
            ["sub/a", "sub/a/b", "sub/a/b/leaf", "sub/a/x", "sub/z"]
        );
        assert_eq!(tree.entries[2].size, Some(4));
        assert!(!tree.truncated);
        assert_eq!(tree.coverage["complete"], true);
        assert_eq!(tree.coverage["exhausted"], Value::Null);
    }

    #[test]
    fn tree_marks_directories_left_at_the_depth_limit() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir_all(temporary.path().join("d/e")).expect("create tree");
        std::fs::write(temporary.path().join("d/e/deep"), b"").expect("write deep");

        let tree = collect_tree_entries(
            &pinned(temporary.path()),
            Path::new(""),
            Path::new(""),
            2,
            100,
            1_000,
            &ignore_matcher(&[]),
        )
        .expect("tree");

        assert_eq!(tree_paths(&tree), ["d", "d/e"]);
        assert!(tree.truncated);
        assert_eq!(tree.coverage["exhausted"], Value::Null);
        let incomplete = tree.coverage["incomplete_directories"]
            .as_array()
            .expect("incomplete directories");
        assert_eq!(incomplete.len(), 1);
        assert_eq!(incomplete[0]["path"], "d/e");
        assert_eq!(incomplete[0]["coverage"], "depth_limit");
    }

    #[test]
    fn tree_applies_ignore_policy_on_the_policy_relative_path() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir_all(temporary.path().join("target/debug")).expect("create target");
        std::fs::create_dir(temporary.path().join("remotes")).expect("create remotes");
        std::fs::write(temporary.path().join("kept"), b"").expect("write kept");

        // The walk root is the project's `.ai/config` directory.
        let tree = collect_tree_entries(
            &pinned(temporary.path()),
            Path::new("config"),
            Path::new(".ai/config"),
            4,
            1,
            1_000,
            &ignore_matcher(&["target/", "/.ai/config/remotes/"]),
        )
        .expect("tree");

        assert_eq!(tree_paths(&tree), ["config/kept"]);
        assert!(!tree.truncated);
    }

    #[test]
    fn tree_reports_work_exhaustion_honestly() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("d")).expect("create d");
        std::fs::write(temporary.path().join("d/inner"), b"").expect("write inner");

        // One open, one name and one inspection leave nothing to open `d`.
        let tree = collect_tree_entries(
            &pinned(temporary.path()),
            Path::new(""),
            Path::new(""),
            4,
            100,
            3,
            &ignore_matcher(&[]),
        )
        .expect("tree");

        assert_eq!(tree_paths(&tree), ["d"]);
        assert!(tree.truncated);
        assert_eq!(tree.coverage["exhausted"], "work");
        assert_eq!(
            tree.coverage["incomplete_directories"][0]["coverage"],
            "not_visited"
        );
    }

    #[test]
    fn list_reports_a_bounded_prefix_of_an_oversized_directory() {
        let temporary = tempfile::tempdir().expect("temporary root");
        for index in 0..5 {
            std::fs::write(temporary.path().join(format!("f{index}")), b"").expect("write");
        }

        let listing =
            list_directory(&pinned(temporary.path()), 3, 1_000).expect("bounded listing");

        assert_eq!(listing.entries.len(), 3);
        assert!(listing.truncated);
        assert_eq!(listing.coverage["exhausted"], "entries");
        assert_eq!(listing.coverage["directory"]["coverage"], "name_prefix");
        assert_eq!(listing.coverage["directory"]["names_read"], 5);
    }

    #[test]
    fn list_of_a_directory_with_subdirectories_is_complete() {
        // Child directories are reported, not opened, and do not make a
        // one-level listing incomplete.
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("b-dir")).expect("create directory");
        std::fs::write(temporary.path().join("b-dir/inner"), b"").expect("write inner");
        std::fs::write(temporary.path().join("a-file"), b"abc").expect("write file");

        let listing = list_directory(&pinned(temporary.path()), 3, 1_000).expect("listing");

        assert!(!listing.truncated);
        assert_eq!(listing.entries[0]["name"], "b-dir");
        assert_eq!(listing.entries[0]["is_dir"], true);
        assert_eq!(listing.entries[1]["name"], "a-file");
        assert_eq!(listing.entries[1]["size"], 3);
        assert_eq!(listing.coverage["directory"]["coverage"], "complete");
    }

    #[test]
    fn relative_directory_path_is_rebuilt_from_the_components_opened() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir_all(temporary.path().join("config/x")).expect("create tree");
        let root = pinned(temporary.path());

        let (_, current) =
            open_relative_directory(&root, "config/.").expect("open with trailing dot");
        assert_eq!(current, Path::new("config"));

        let (directory, doubled) =
            open_relative_directory(&root, "config//x").expect("open with doubled separator");
        assert_eq!(doubled, Path::new("config/x"));

        std::fs::write(temporary.path().join("config/x/leaf"), b"").expect("write leaf");
        let tree = collect_tree_entries(
            &directory,
            &doubled,
            &Path::new(".ai").join(&doubled),
            4,
            100,
            1_000,
            &ignore_matcher(&["/.ai/config/x/leaf/"]),
        )
        .expect("tree");
        assert!(tree.entries.is_empty());
        assert_eq!(tree.coverage["complete"], true);

        assert!(open_relative_directory(&root, "./config").is_err());
        assert!(open_relative_directory(&root, "config/../config").is_err());
    }
}
