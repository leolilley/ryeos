//! `push-head` — write a principal-scoped project HEAD ref.
//!
//! Called after the client has uploaded all blobs + manifest + snapshot
//! via the CAS routes. Validates the snapshot chain and writes the
//! HEAD ref scoped to the caller's fingerprint.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::Value;

use ryeos_executor::executor::ServiceAvailability;
use crate::registry::ServiceDescriptor;
use crate::handler_context::HandlerContext;
use ryeos_app::state::AppState;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Project path (used to derive the project hash).
    pub project_path: String,
    /// CAS hash of the `ProjectSnapshot` to point HEAD at.
    pub snapshot_hash: String,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let cas_root = state.state_store.cas_root()?;
    let refs_root = state.state_store.refs_root()?;
    let cas = lillux::cas::CasStore::new(cas_root.clone());

    // Caller identity used for principal-scoped storage — must be verified.
    ctx.require_verified().map_err(|e| anyhow::anyhow!(e))?;

    // 1. Validate snapshot exists in CAS
    let snap_obj = cas
        .get_object(&req.snapshot_hash)?
        .ok_or_else(|| anyhow!("snapshot {} not found in CAS", req.snapshot_hash))?;
    let snapshot = ryeos_state::objects::ProjectSnapshot::from_value(&snap_obj)?;

    // 2. Validate manifest exists in CAS
    let manifest_obj = cas
        .get_object(&snapshot.project_manifest_hash)?
        .ok_or_else(|| anyhow!(
            "manifest {} not found in CAS (referenced by snapshot {})",
            snapshot.project_manifest_hash, req.snapshot_hash
        ))?;
    let manifest = ryeos_state::objects::SourceManifest::from_value(&manifest_obj)?;

    // 3. Validate manifest entries don't contain ignored paths
    let ignore = &state.ignore_matcher;
    for rel_path in manifest.item_source_hashes.keys() {
        if ignore.is_ignored(rel_path) {
            return Err(anyhow!(
                "manifest contains ignored path '{}'; remove it from the push",
                rel_path
            ));
        }
    }

    // 3b. Validate user manifest paths (if present).
    // When user_manifest_hash is populated (§2), validate all paths
    // against the allow-list and reject absolute/..-traversal paths.
    if let Some(ref user_manifest_hash) = snapshot.user_manifest_hash {
        let user_manifest_obj = cas
            .get_object(user_manifest_hash)?
            .ok_or_else(|| anyhow!(
                "user manifest {} not found in CAS (referenced by snapshot {})",
                user_manifest_hash, req.snapshot_hash
            ))?;
        let user_manifest = ryeos_state::objects::SourceManifest::from_value(&user_manifest_obj)?;
        validate_user_manifest_paths(&user_manifest)?;
    }

    // 4. Compute principal-scoped project key.
    //
    // §0a: project_ref must be canonical. Both push HEAD writes (here)
    // and execute HEAD reads (project_source.rs) go through
    // `canonical_project_ref` — the single source of truth — so both
    // sides agree on the hash key. The `NO_PROJECT_SENTINEL` is the
    // only path string that bypasses canonicalize (per-principal scope
    // for --no-project mode).
    let canonical_project_path =
        ryeos_executor::execution::project_source::canonical_project_ref(&req.project_path)
            .map_err(|e| anyhow!("push_head: {}", e))?;
    let principal_key = ryeos_state::refs::principal_storage_key(&ctx.fingerprint);
    let project_hash = lillux::cas::sha256_hex(canonical_project_path.as_bytes());

    // 5. Write the HEAD ref (with CAS compare-and-swap if HEAD already exists)
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    let _permit = state.write_barrier.try_acquire()
        .map_err(|e| anyhow!("cannot acquire CAS write permit: {e}"))?;

    // If a HEAD already exists for this principal+project, advance it with CAS.
    // Otherwise, write a new ref.
    match state.state_store.with_state_db(|db| {
        db.read_project_head(principal_key, &project_hash)
    })? {
        Some(current_hash) => {
            // Advance with conflict detection
            ryeos_state::refs::advance_project_head_ref(
                &refs_root,
                principal_key,
                &project_hash,
                &req.snapshot_hash,
                &current_hash,
                &signer,
            )?;
        }
        None => {
            // First push — write new ref
            ryeos_state::refs::write_project_head_ref(
                &refs_root,
                principal_key,
                &project_hash,
                &req.snapshot_hash,
                &signer,
            )?;
        }
    }

    tracing::info!(
        principal_key = %principal_key,
        project_hash = %project_hash,
        snapshot_hash = %req.snapshot_hash,
        manifest_entries = manifest.item_source_hashes.len(),
        "push-head: wrote project HEAD ref"
    );

    Ok(serde_json::json!({
        "principal_key": principal_key,
        "project_hash": project_hash,
        "snapshot_hash": req.snapshot_hash,
        "manifest_entries": manifest.item_source_hashes.len(),
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:system/push-head",
    endpoint: "system.push-head",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.push.head"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)?;
            handle(req, ctx, state).await
        })
    },
};

/// Subdirectories of a user-space `.ai/` that participate in
/// cross-node sync. Anything not in this set is local-only.
pub const USER_SPACE_SYNC_DIRS: &[&str] = &[
    "directives",
    "tools",
    "knowledge",
    "parsers",
    "handlers",
    "protocols",
    "node/engine/kinds",
    "node/verbs",
    "node/aliases",
];

/// Trust pins are also pushed but handled separately (request-scoped).
pub const USER_TRUST_SYNC_DIR: &str = "config/keys/trusted";

/// Validate that all paths in a user manifest are within the
/// allowed sync directories and don't contain traversal attacks.
///
/// Uses proper path-component matching, not substring/prefix checks:
/// - `..` is rejected only when it appears as a Component::ParentDir
///   (so a benign filename like `foo..bar` passes)
/// - allow-list prefix is matched component-by-component (so
///   `config/keys/trustedness/...` does NOT match `config/keys/trusted`)
fn validate_user_manifest_paths(manifest: &ryeos_state::objects::SourceManifest) -> Result<()> {
    use std::path::{Component, Path};

    // Pre-split allow-list dirs into component vectors for component-wise prefix matching.
    let allowed_prefixes: Vec<Vec<&str>> = USER_SPACE_SYNC_DIRS
        .iter()
        .chain(std::iter::once(&USER_TRUST_SYNC_DIR))
        .map(|d| d.split('/').collect())
        .collect();

    for rel_path in manifest.item_source_hashes.keys() {
        // Reject NUL bytes (defensive — paths shouldn't have these)
        if rel_path.contains('\0') {
            return Err(anyhow!(
                "user manifest contains NUL byte in path '{}'",
                rel_path
            ));
        }

        let path = Path::new(rel_path);
        let components: Vec<Component> = path.components().collect();

        // Reject absolute / root / prefix components and parent-dir traversal
        for comp in &components {
            match comp {
                Component::RootDir | Component::Prefix(_) => {
                    return Err(anyhow!(
                        "user manifest contains absolute path '{}'; only relative paths allowed",
                        rel_path
                    ));
                }
                Component::ParentDir => {
                    return Err(anyhow!(
                        "user manifest contains '..' component in path '{}'; traversal not allowed",
                        rel_path
                    ));
                }
                _ => {}
            }
        }

        // Extract normal (non-CurDir, non-ParentDir) component names for prefix matching.
        let normal_segments: Vec<&str> = components
            .iter()
            .filter_map(|c| match c {
                Component::Normal(s) => s.to_str(),
                _ => None,
            })
            .collect();

        // Verify path is component-wise under one of the allowed prefixes.
        let allowed = allowed_prefixes.iter().any(|prefix| {
            normal_segments.len() > prefix.len()
                && normal_segments
                    .iter()
                    .zip(prefix.iter())
                    .all(|(seg, pref)| seg == pref)
        });
        if !allowed {
            return Err(anyhow!(
                "user manifest contains path '{}' outside allowed sync directories {:?}",
                rel_path,
                USER_SPACE_SYNC_DIRS
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_manifest_accepts_allowed_paths() {
        let manifest = ryeos_state::objects::SourceManifest {
            item_source_hashes: vec![
                ("directives/my/refactor.md".to_string(), "hash1".to_string()),
                ("tools/my/script.py".to_string(), "hash2".to_string()),
                ("knowledge/my/notes.md".to_string(), "hash3".to_string()),
                ("config/keys/trusted/abc.toml".to_string(), "hash4".to_string()),
            ].into_iter().collect(),
        };
        validate_user_manifest_paths(&manifest).expect("allowed paths must pass");
    }

    #[test]
    fn user_manifest_rejects_absolute_path() {
        let manifest = ryeos_state::objects::SourceManifest {
            item_source_hashes: vec![
                ("/etc/passwd".to_string(), "hash".to_string()),
            ].into_iter().collect(),
        };
        let err = validate_user_manifest_paths(&manifest).unwrap_err();
        assert!(err.to_string().contains("absolute path"));
    }

    #[test]
    fn user_manifest_rejects_dotdot_traversal() {
        let manifest = ryeos_state::objects::SourceManifest {
            item_source_hashes: vec![
                ("knowledge/../../../etc/passwd".to_string(), "hash".to_string()),
            ].into_iter().collect(),
        };
        let err = validate_user_manifest_paths(&manifest).unwrap_err();
        assert!(err.to_string().contains(".."));
    }

    #[test]
    fn user_manifest_rejects_disallowed_path() {
        let manifest = ryeos_state::objects::SourceManifest {
            item_source_hashes: vec![
                ("config/keys/signing/private_key.pem".to_string(), "hash".to_string()),
            ].into_iter().collect(),
        };
        let err = validate_user_manifest_paths(&manifest).unwrap_err();
        assert!(err.to_string().contains("outside allowed"));
    }

    // ── Item 4: allow-list bugs ──

    #[test]
    fn user_manifest_accepts_benign_double_dot_in_filename() {
        // "foo..bar" is a legal filename; only `..` as a path *component*
        // is traversal. The old substring check incorrectly rejected this.
        let manifest = ryeos_state::objects::SourceManifest {
            item_source_hashes: vec![
                ("knowledge/my/foo..bar.md".to_string(), "hash".to_string()),
            ].into_iter().collect(),
        };
        validate_user_manifest_paths(&manifest)
            .expect("benign '..' in filename must be accepted");
    }

    #[test]
    fn user_manifest_rejects_dotdot_as_path_component() {
        let manifest = ryeos_state::objects::SourceManifest {
            item_source_hashes: vec![
                ("knowledge/foo/../bar.md".to_string(), "hash".to_string()),
            ].into_iter().collect(),
        };
        let err = validate_user_manifest_paths(&manifest).unwrap_err();
        assert!(err.to_string().contains("'..' component"));
    }

    #[test]
    fn user_manifest_rejects_lookalike_trust_prefix() {
        // `config/keys/trustedness/...` must NOT match `config/keys/trusted`
        // (old starts_with check accepted this).
        let manifest = ryeos_state::objects::SourceManifest {
            item_source_hashes: vec![
                ("config/keys/trustedness/abc.toml".to_string(), "hash".to_string()),
            ].into_iter().collect(),
        };
        let err = validate_user_manifest_paths(&manifest).unwrap_err();
        assert!(err.to_string().contains("outside allowed"));
    }

    #[test]
    fn user_manifest_rejects_lookalike_sync_dir_prefix() {
        // `directives_legacy/...` must NOT match `directives` allow-list entry.
        let manifest = ryeos_state::objects::SourceManifest {
            item_source_hashes: vec![
                ("directives_legacy/foo.md".to_string(), "hash".to_string()),
            ].into_iter().collect(),
        };
        let err = validate_user_manifest_paths(&manifest).unwrap_err();
        assert!(err.to_string().contains("outside allowed"));
    }

    #[test]
    fn user_manifest_rejects_bare_allowlist_dir_with_no_payload() {
        // The bare directory name with no file under it is also rejected
        // (we require strictly *under* the allow-listed prefix).
        let manifest = ryeos_state::objects::SourceManifest {
            item_source_hashes: vec![
                ("directives".to_string(), "hash".to_string()),
            ].into_iter().collect(),
        };
        let err = validate_user_manifest_paths(&manifest).unwrap_err();
        assert!(err.to_string().contains("outside allowed"));
    }
}
