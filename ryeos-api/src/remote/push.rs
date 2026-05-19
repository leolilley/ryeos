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
    let system_space_dir = &state.config.system_space_dir;

    // Fail-fast guards: prevent the common footgun of running
    // `ryeos remote execute` from $HOME or some other catch-all
    // directory and silently ingest-walking thousands of unrelated
    // files. The push step recursively walks `project_path`; if
    // that's `$HOME` or contains the daemon's system space, the
    // walk takes minutes-to-hours and never produces a meaningful
    // snapshot. Detect both cases up front.
    refuse_walking_root(project_path, system_space_dir)?;

    // 1. Ingest project directory into local CAS using remote's ignore rules.
    let local_cas_root = system_space_dir.join(ryeos_engine::AI_DIR).join("state").join("objects");
    let local_cas = CasStore::new(local_cas_root.clone());

    let mut items: HashMap<String, String> = HashMap::new();
    ingest_for_push(&local_cas, &local_cas_root, project_path, project_path, &mut items, remote_ignore)?;

    // 2. Build manifest
    let manifest = SourceManifest { item_source_hashes: items };
    let manifest_hash = local_cas.store_object(&manifest.to_value())?;

    // 3. Build snapshot
    let snapshot = ryeos_state::objects::ProjectSnapshot {
        project_manifest_hash: manifest_hash.clone(),
        user_manifest_hash: None,
        parent_hashes: Vec::new(),
        created_at: lillux::time::iso8601_now(),
        source: "push".to_string(),
    };
    let snapshot_hash = local_cas.store_object(&snapshot.to_value())?;

    // 4. Collect all object hashes we need the remote to have
    let mut all_hashes: Vec<String> = Vec::new();
    for (_rel_path, obj_hash) in &manifest.item_source_hashes {
        all_hashes.push(obj_hash.clone());
        // The item source object also contains a blob reference
        if let Ok(Some(item_obj)) = local_cas.get_object(obj_hash) {
            if let Some(blob_hash) = item_obj.get("content_blob_hash").and_then(|v| v.as_str()) {
                all_hashes.push(blob_hash.to_string());
            }
        }
    }
    all_hashes.push(manifest_hash.clone());
    all_hashes.push(snapshot_hash.clone());
    all_hashes.sort();
    all_hashes.dedup();

    // 5. Check which hashes the remote already has
    let has_resp = client.objects_has(&all_hashes).await?;
    let missing: Vec<String> = has_resp.missing;

    // 6. Upload missing blobs and objects
    let blobs_uploaded = if !missing.is_empty() {
        let mut blobs = Vec::new();
        let mut objects = Vec::new();

        for hash in &missing {
            // Try blob first
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

    let blobs_skipped = all_hashes.len() - missing.len();

    // 7. Call push-head
    client.push_head(project_path_for_ref, &snapshot_hash).await?;

    let manifest_entries = manifest.item_source_hashes.len();
    Ok(PushResult {
        snapshot_hash,
        manifest_hash,
        manifest,
        manifest_entries,
        blobs_uploaded,
        blobs_skipped,
    })
}

/// Reject project paths that would walk the entire home directory
/// or contain the daemon's own system space.
///
/// Returns an error describing why and how to fix it. The error
/// message names the offending path so the operator can copy-paste a
/// corrected `-p` flag.
fn refuse_walking_root(project_path: &Path, system_space_dir: &Path) -> Result<()> {
    // Canonicalise both so symlinks and `.` aren't false negatives.
    // If canonicalisation fails (e.g. path doesn't exist), skip the
    // check — the upstream walk will fail with its own clearer error.
    let proj = match project_path.canonicalize() {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };
    let sys = system_space_dir
        .canonicalize()
        .unwrap_or_else(|_| system_space_dir.to_path_buf());

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

    if proj == sys || proj.starts_with(&sys) || sys.starts_with(&proj) {
        anyhow::bail!(
            "refusing to push {} — that path overlaps the daemon's \
             system space ({}). Pushing the daemon's own state to a \
             remote would corrupt both nodes. Re-run from a project \
             directory outside the daemon state tree.",
            proj.display(),
            sys.display(),
        );
    }

    Ok(())
}

/// Walk a project directory and ingest files for push.
fn ingest_for_push(
    cas: &CasStore,
    cas_root: &Path,
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
            ingest_for_push(cas, cas_root, root, &path, items, ignore)?;
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
mod refuse_walking_root_tests {
    use super::refuse_walking_root;
    use tempfile::TempDir;

    #[test]
    fn ordinary_project_dir_outside_home_passes() {
        // A tempdir under /tmp is neither $HOME nor inside the daemon
        // system space — must pass.
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
        let old_home = std::env::var_os("HOME");
        // SAFETY: tests in this module are not parallel within the
        // crate by default; the HOME env mutation is restored on
        // return. (cargo nextest runs tests in separate processes,
        // so cross-test interference is also bounded.)
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
    fn project_path_inside_system_space_is_refused() {
        let sys = TempDir::new().unwrap();
        let inside = sys.path().join("inner");
        std::fs::create_dir_all(&inside).unwrap();
        let err = refuse_walking_root(&inside, sys.path())
            .expect_err("paths inside system space must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("system space"),
            "error must mention system space, got: {msg}"
        );
    }

    #[test]
    fn project_path_containing_system_space_is_refused() {
        // The inverse: project is the parent that contains the
        // system_space_dir. Walking it would still hit the daemon
        // state.
        let proj = TempDir::new().unwrap();
        let sys = proj.path().join("daemon-state");
        std::fs::create_dir_all(&sys).unwrap();
        let err = refuse_walking_root(proj.path(), &sys)
            .expect_err("paths containing system space must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("system space"), "got: {msg}");
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
}
