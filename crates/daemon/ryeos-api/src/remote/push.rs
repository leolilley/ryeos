//! CAS push pipeline for remote nodes.
//!
//! Handles the ingest-locally → upload-blobs → push-head pipeline
//! for pushing project content to a remote node.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use base64::Engine as _;

use lillux::cas::{sha256_hex, CasStore};
use ryeos_state::ignore::IgnoreMatcher;
use ryeos_state::objects::SourceManifest;
use ryeos_state::project_sync::{ProjectSyncScope, PROJECT_AI_SURFACES};

use crate::remote::client::{BlobUpload, RemoteClient};
use ryeos_app::state::AppState;

/// Result of pushing a project to a remote.
#[derive(Debug)]
pub struct PushResult {
    pub snapshot_hash: String,
    pub manifest_hash: String,
    /// The exact pushed manifest — needed by pull_results() for
    /// conflict detection (can't recompute later; workspace may drift).
    pub manifest: SourceManifest,
    pub manifest_entries: usize,
    pub blobs_uploaded: usize,
    pub blobs_skipped: usize,
    /// Operator manifest hash, if the operator's app root had any
    /// content under the sync allow-list. Used by pull_results() as
    /// the symmetric base for operator-side diff/apply.
    pub user_manifest_hash: Option<String>,
    /// The exact pushed user manifest. None when the operator's user
    /// space is empty / absent.
    pub user_manifest: Option<SourceManifest>,
}

/// Build, upload, and stage an AI-only project snapshot.
///
/// Unlike [`push_project`], this is allow-list based and project-only:
/// it does not ingest app files and never includes a user manifest.
///
/// `remote_ignore`, when supplied, is applied to every candidate path
/// under the allow-list so files the remote would later reject (e.g.
/// `__pycache__/`, `*.pyc`) are dropped client-side instead of
/// blowing up at `/push-head`.
pub async fn push_project_ai_only(
    client: &RemoteClient,
    state: &Arc<AppState>,
    local_project_path: &Path,
    remote_project_path_for_ref: &str,
    remote_ignore: Option<&IgnoreMatcher>,
) -> Result<PushResult> {
    let app_root = &state.config.app_root;
    refuse_walking_root(local_project_path, app_root)?;

    let local_cas_root = app_root
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join("objects");
    let local_cas = CasStore::new(local_cas_root);

    let mut items: HashMap<String, String> = HashMap::new();
    ingest_project_ai_for_push(&local_cas, local_project_path, &mut items, remote_ignore)?;

    let manifest = SourceManifest {
        item_source_hashes: items,
    };
    ryeos_state::project_sync::validate_project_manifest_paths(
        &manifest,
        ProjectSyncScope::AiOnly,
        remote_ignore,
    )?;
    let manifest_hash = local_cas.store_object(&manifest.to_value())?;

    let snapshot = ryeos_state::objects::ProjectSnapshot {
        project_manifest_hash: manifest_hash.clone(),
        user_manifest_hash: None,
        message: None,
        project_sync_scope: ProjectSyncScope::AiOnly,
        parent_hashes: Vec::new(),
        created_at: lillux::time::iso8601_now(),
        source: "remote_ai_sync".to_string(),
    };
    let snapshot_hash = local_cas.store_object(&snapshot.to_value())?;

    let all_hashes = collect_snapshot_hashes(
        &local_cas,
        &manifest,
        None,
        None,
        &manifest_hash,
        &snapshot_hash,
    );
    let (blobs_uploaded, blobs_skipped) = upload_missing(client, &local_cas, &all_hashes).await?;

    client
        .push_head(remote_project_path_for_ref, &snapshot_hash)
        .await?;

    let manifest_entries = manifest.item_source_hashes.len();
    Ok(PushResult {
        snapshot_hash,
        manifest_hash,
        manifest,
        manifest_entries,
        blobs_uploaded,
        blobs_skipped,
        user_manifest_hash: None,
        user_manifest: None,
    })
}

/// Push a project directory to a remote node.
///
/// 1. Apply the remote's ingest ignore rules to build the manifest
/// 2. Ingest locally into CAS
/// 3. Build manifest + snapshot
/// 4. Check which blobs the remote already has
/// 5. Upload missing blobs + manifest + snapshot
/// 6. Call push-head to write the HEAD ref
///
/// The `remote_ignore` matcher is the **only** ignore policy used: the
/// manifest is built using the remote's rules so that the pushed content
/// matches what the remote would accept during ingest. Callers must
/// resolve ignore rules before calling this function.
pub async fn push_project(
    client: &RemoteClient,
    state: &Arc<AppState>,
    project_path: &Path,
    project_path_for_ref: &str,
    remote_ignore: &IgnoreMatcher,
) -> Result<PushResult> {
    let app_root = &state.config.app_root;

    // Fail-fast guards: prevent the common footgun of running
    // `ryeos remote execute` from $HOME or some other catch-all
    // directory and silently ingest-walking thousands of unrelated
    // files. The push step recursively walks `project_path`; if
    // that's `$HOME` or contains the daemon's app root, the
    // walk takes minutes-to-hours and never produces a meaningful
    // snapshot. Detect both cases up front.
    refuse_walking_root(project_path, app_root)?;

    // 1. Ingest project directory into local CAS using remote's ignore rules.
    let local_cas_root = app_root
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join("objects");
    let local_cas = CasStore::new(local_cas_root);

    let mut items: HashMap<String, String> = HashMap::new();
    ingest_for_push(
        &local_cas,
        project_path,
        project_path,
        &mut items,
        remote_ignore,
    )?;

    // 2. Build project manifest
    let manifest = SourceManifest {
        item_source_hashes: items,
    };
    let manifest_hash = local_cas.store_object(&manifest.to_value())?;

    // 3. Build snapshot
    let snapshot = ryeos_state::objects::ProjectSnapshot {
        project_manifest_hash: manifest_hash.clone(),
        user_manifest_hash: None,
        message: None,
        project_sync_scope: ryeos_state::project_sync::ProjectSyncScope::FullProject,
        parent_hashes: Vec::new(),
        created_at: lillux::time::iso8601_now(),
        source: "push".to_string(),
    };
    let snapshot_hash = local_cas.store_object(&snapshot.to_value())?;

    // 4. Collect all object hashes we need the remote to have
    let all_hashes = collect_snapshot_hashes(
        &local_cas,
        &manifest,
        None,
        None,
        &manifest_hash,
        &snapshot_hash,
    );

    let (blobs_uploaded, blobs_skipped) = upload_missing(client, &local_cas, &all_hashes).await?;

    // 7. Call push-head
    client
        .push_head(project_path_for_ref, &snapshot_hash)
        .await?;

    let manifest_entries = manifest.item_source_hashes.len();
    Ok(PushResult {
        snapshot_hash,
        manifest_hash,
        manifest,
        manifest_entries,
        blobs_uploaded,
        blobs_skipped,
        user_manifest_hash: None,
        user_manifest: None,
    })
}

pub(crate) fn collect_snapshot_hashes(
    cas: &CasStore,
    manifest: &SourceManifest,
    user_manifest: Option<&SourceManifest>,
    user_manifest_hash: Option<&str>,
    manifest_hash: &str,
    snapshot_hash: &str,
) -> Vec<String> {
    let mut all_hashes: Vec<String> = Vec::new();
    collect_manifest_hashes(cas, manifest, &mut all_hashes);
    if let Some(um) = user_manifest {
        collect_manifest_hashes(cas, um, &mut all_hashes);
    }
    if let Some(umh) = user_manifest_hash {
        all_hashes.push(umh.to_string());
    }
    all_hashes.push(manifest_hash.to_string());
    all_hashes.push(snapshot_hash.to_string());
    all_hashes.sort();
    all_hashes.dedup();
    all_hashes
}

fn collect_manifest_hashes(
    cas: &CasStore,
    manifest: &SourceManifest,
    all_hashes: &mut Vec<String>,
) {
    for obj_hash in manifest.item_source_hashes.values() {
        all_hashes.push(obj_hash.clone());
        if let Ok(Some(item_obj)) = cas.get_object(obj_hash) {
            if let Some(blob_hash) = item_obj.get("content_blob_hash").and_then(|v| v.as_str()) {
                all_hashes.push(blob_hash.to_string());
            }
        }
    }
}

pub(crate) async fn upload_missing(
    client: &RemoteClient,
    local_cas: &CasStore,
    all_hashes: &[String],
) -> Result<(usize, usize)> {
    let has_resp = client.objects_has(all_hashes).await?;
    let skipped = has_resp.found.len();
    let missing: Vec<String> = has_resp.missing;

    let uploaded = if !missing.is_empty() {
        let mut blobs = Vec::new();
        let mut objects = Vec::new();

        for hash in &missing {
            if let Ok(Some(data)) = local_cas.get_blob(hash) {
                let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
                blobs.push(BlobUpload { data: encoded });
            } else if let Ok(Some(value)) = local_cas.get_object(hash) {
                objects.push(value);
            }
        }

        if !blobs.is_empty() || !objects.is_empty() {
            client.objects_put(&blobs, &objects).await?;
        }
        blobs.len() + objects.len()
    } else {
        0
    };

    Ok((uploaded, skipped))
}

/// Reject project paths that would walk the entire home directory
/// or contain the daemon's own app root.
///
/// Returns an error describing why and how to fix it. The error
/// message names the offending path so the operator can copy-paste a
/// corrected `-p` flag.
fn refuse_walking_root(project_path: &Path, app_root: &Path) -> Result<()> {
    // Canonicalise both so symlinks and `.` aren't false negatives.
    // If canonicalisation fails (e.g. path doesn't exist), skip the
    // check — the upstream walk will fail with its own clearer error.
    let proj = match project_path.canonicalize() {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };
    let app_root_canon = app_root
        .canonicalize()
        .unwrap_or_else(|_| app_root.to_path_buf());

    // Reject filebundle root
    if proj.parent().is_none() {
        anyhow::bail!(
            "refusing to push filebundle root '/'. \
             `remote execute` recursively ingests the project; \
             walking '/' would ingest the entire filesystem. \
             Re-run from inside a project directory, or pass \
             `-p <project-dir>` explicitly.",
        );
    }

    // $HOME comparison via env var to avoid a new dependency. The
    // env var is stable on every platform ryeos targets (Linux,
    // macOS). If $HOME is unset we just skip this check.
    if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
        let home = home.canonicalize().unwrap_or(home);
        if proj == home {
            anyhow::bail!(
                "refusing to push {} — that's your home directory. \
                 `remote execute` recursively ingests the project; \
                 running it from $HOME would walk every file you own. \
                 Re-run from inside a project directory, or pass \
                 `-p <project-dir>` explicitly.",
                proj.display(),
            );
        }
    }

    if proj == app_root_canon
        || proj.starts_with(&app_root_canon)
        || app_root_canon.starts_with(&proj)
    {
        anyhow::bail!(
            "refusing to push {} — that path overlaps the daemon's \
             app root ({}). Pushing the daemon's own state to a \
             remote would corrupt both nodes. Re-run from a project \
             directory outside the daemon state tree.",
            proj.display(),
            app_root_canon.display(),
        );
    }

    Ok(())
}

/// Walk only managed project AI roots and ingest regular files.
/// Symlinked allow-list roots and symlinks inside those roots are skipped.
fn ingest_project_ai_for_push(
    cas: &CasStore,
    project_root: &Path,
    items: &mut HashMap<String, String>,
    remote_ignore: Option<&IgnoreMatcher>,
) -> Result<()> {
    for surface in PROJECT_AI_SURFACES
        .iter()
        .filter(|surface| surface.materialize_to_project)
    {
        let abs = project_root.join(surface.root);
        match std::fs::symlink_metadata(&abs) {
            Ok(md) if md.file_type().is_symlink() => {
                tracing::warn!(path = %abs.display(), "skipping symlinked project AI sync root");
                continue;
            }
            Ok(md) if md.is_dir() => {
                ingest_project_ai_dir_for_push(cas, project_root, &abs, items, remote_ignore)?
            }
            _ => continue,
        }
    }
    Ok(())
}

fn ingest_project_ai_dir_for_push(
    cas: &CasStore,
    project_root: &Path,
    dir: &Path,
    items: &mut HashMap<String, String>,
    remote_ignore: Option<&IgnoreMatcher>,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            tracing::warn!(path = %entry.path().display(), "skipping symlink in project AI ingest");
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            if let Some(matcher) = remote_ignore {
                let rel_dir = path
                    .strip_prefix(project_root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if matcher.is_ignored(&rel_dir) {
                    continue;
                }
            }
            ingest_project_ai_dir_for_push(cas, project_root, &path, items, remote_ignore)?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(project_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if let Some(matcher) = remote_ignore {
                if matcher.is_ignored(&rel) {
                    continue;
                }
            }
            let bytes = std::fs::read(&path)?;
            let blob_hash = cas.store_blob(&bytes)?;
            let integrity = sha256_hex(&bytes);
            let item_source = ryeos_state::objects::ItemSource {
                item_ref: rel.clone(),
                content_blob_hash: blob_hash,
                integrity,
                signature_info: None,
                mode: executable_mode(&path),
            };
            let obj_hash = cas.store_object(&item_source.to_value())?;
            items.insert(rel, obj_hash);
        }
    }
    Ok(())
}

fn executable_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .ok()
            .map(|m| m.permissions().mode())
            .filter(|m| m & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Walk a project directory and ingest files for push.
fn ingest_for_push(
    cas: &CasStore,
    root: &Path,
    dir: &Path,
    items: &mut HashMap<String, String>,
    ignore: &IgnoreMatcher,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        // Skip state/ directory
        if rel.starts_with("state/") || rel == "state" {
            continue;
        }

        // Apply ignore rules
        if ignore.is_ignored(&rel) {
            continue;
        }

        if path.is_dir() {
            ingest_for_push(cas, root, &path, items, ignore)?;
        } else if path.is_file() {
            let bytes = std::fs::read(&path)?;
            let blob_hash = cas.store_blob(&bytes)?;
            let integrity = sha256_hex(&bytes);

            let item_source = ryeos_state::objects::ItemSource {
                item_ref: rel.clone(),
                content_blob_hash: blob_hash,
                integrity,
                signature_info: None,
                mode: None,
            };
            let obj_hash = cas.store_object(&item_source.to_value())?;
            items.insert(rel, obj_hash);
        }
    }
    Ok(())
}

#[cfg(test)]
mod project_ai_ingest_tests {
    use super::ingest_project_ai_for_push;
    use lillux::cas::CasStore;
    use std::collections::HashMap;
    use tempfile::TempDir;

    #[test]
    fn project_ai_ingest_includes_schedule_declarations_not_node_schedule_specs() {
        let project = TempDir::new().unwrap();
        let cas_dir = TempDir::new().unwrap();
        let cas = CasStore::new(cas_dir.path().to_path_buf());
        std::fs::create_dir_all(project.path().join(".ai/config/schedules")).unwrap();
        std::fs::create_dir_all(project.path().join(".ai/node/schedules")).unwrap();
        std::fs::write(
            project.path().join(".ai/config/schedules/snap-track.yaml"),
            "schedule intent",
        )
        .unwrap();
        std::fs::write(
            project.path().join(".ai/node/schedules/runtime.yaml"),
            "runtime state",
        )
        .unwrap();

        let mut items = HashMap::new();
        ingest_project_ai_for_push(&cas, project.path(), &mut items, None).unwrap();

        assert!(items.contains_key(".ai/config/schedules/snap-track.yaml"));
        assert!(!items.contains_key(".ai/node/schedules/runtime.yaml"));
    }
}

#[cfg(test)]
mod refuse_walking_root_tests {
    use super::refuse_walking_root;
    use tempfile::TempDir;

    /// Tests in this module mutate `$HOME`; serialize those mutations.
    static HOME_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn ordinary_project_dir_outside_home_passes() {
        // A tempdir under /tmp is neither $HOME nor inside the daemon
        // app root — must pass.
        let proj = TempDir::new().unwrap();
        let sys = TempDir::new().unwrap();
        refuse_walking_root(proj.path(), sys.path()).expect("ordinary dir must pass");
    }

    #[test]
    fn project_path_equal_to_home_is_refused() {
        let proj = TempDir::new().unwrap();
        let sys = TempDir::new().unwrap();
        // Spoof $HOME to point at the project dir; the function
        // must catch that and refuse with a clear $HOME message.
        // SAFETY: serialised via HOME_ENV_LOCK to prevent concurrent
        // HOME-mutating tests in this module from racing.
        let _guard = HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", proj.path());
        }
        let result = refuse_walking_root(proj.path(), sys.path());
        unsafe {
            match old_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        let err = result.expect_err("home dir must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("home directory"),
            "error must mention 'home directory', got: {msg}"
        );
    }

    #[test]
    fn project_path_inside_app_root_is_refused() {
        let sys = TempDir::new().unwrap();
        let inside = sys.path().join("inner");
        std::fs::create_dir_all(&inside).unwrap();
        let err = refuse_walking_root(&inside, sys.path())
            .expect_err("paths inside app root must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("app root"),
            "error must mention app root, got: {msg}"
        );
    }

    #[test]
    fn project_path_containing_app_root_is_refused() {
        // The inverse: project is the parent that contains the
        // app_root. Walking it would still hit the daemon
        // state.
        let proj = TempDir::new().unwrap();
        let sys = proj.path().join("daemon-state");
        std::fs::create_dir_all(&sys).unwrap();
        let err = refuse_walking_root(proj.path(), &sys)
            .expect_err("paths containing app root must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("app root"), "got: {msg}");
    }

    #[test]
    fn nonexistent_path_does_not_short_circuit() {
        // Canonicalisation fails for nonexistent paths; the guard
        // must skip rather than spuriously refuse — the downstream
        // walk will produce a clearer "not found" error.
        let sys = TempDir::new().unwrap();
        let missing = std::path::Path::new("/this/does/not/exist/anywhere");
        refuse_walking_root(missing, sys.path())
            .expect("missing path must pass the guard (fail elsewhere)");
    }

    // ── Item 9: missing refuse_walking_root coverage ──

    #[test]
    fn filesystem_root_is_refused() {
        // Walking '/' would ingest the entire filesystem. Must hard error.
        let sys = TempDir::new().unwrap();
        let err = refuse_walking_root(std::path::Path::new("/"), sys.path())
            .expect_err("filebundle root must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("filebundle root") || msg.contains("'/'"),
            "error must mention filebundle root, got: {msg}"
        );
    }
}

#[cfg(test)]
mod ingest_symlink_tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn ingest_project_ai_walks_only_managed_roots() {
        let cas_dir = TempDir::new().unwrap();
        let cas = CasStore::new(cas_dir.path().to_path_buf());
        let project = TempDir::new().unwrap();

        std::fs::create_dir_all(project.path().join(".ai/directives")).unwrap();
        std::fs::create_dir_all(project.path().join(".ai/state")).unwrap();
        std::fs::create_dir_all(project.path().join("src")).unwrap();
        std::fs::write(project.path().join(".ai/directives/ok.md"), "ok").unwrap();
        std::fs::write(project.path().join(".ai/state/runtime.sqlite3"), "state").unwrap();
        std::fs::write(project.path().join("src/index.ts"), "app").unwrap();

        let mut items = HashMap::new();
        ingest_project_ai_for_push(&cas, project.path(), &mut items, None).unwrap();

        assert!(items.contains_key(".ai/directives/ok.md"));
        assert!(
            !items.keys().any(|k| k.contains("runtime.sqlite3")),
            "state must not be ingested: {items:?}"
        );
        assert!(
            !items.keys().any(|k| k.starts_with("src/")),
            "app code must not be ingested: {items:?}"
        );
    }

    #[test]
    fn ingest_project_ai_skips_symlinks_and_preserves_exec_mode() {
        #[cfg(not(unix))]
        {
            return;
        }

        use std::os::unix::fs::PermissionsExt;

        let cas_dir = TempDir::new().unwrap();
        let cas = CasStore::new(cas_dir.path().to_path_buf());
        let project = TempDir::new().unwrap();
        let tools = project.path().join(".ai/tools");
        std::fs::create_dir_all(&tools).unwrap();
        let tool = tools.join("run.sh");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), tools.join("leak.sh")).unwrap();

        let mut items = HashMap::new();
        ingest_project_ai_for_push(&cas, project.path(), &mut items, None).unwrap();
        assert!(items.contains_key(".ai/tools/run.sh"));
        assert!(!items.contains_key(".ai/tools/leak.sh"));

        let obj_hash = items.get(".ai/tools/run.sh").unwrap();
        let obj = cas.get_object(obj_hash).unwrap().unwrap();
        let item = ryeos_state::objects::ItemSource::from_value(&obj).unwrap();
        assert_eq!(item.mode.map(|m| m & 0o777), Some(0o755));
    }
}
