//! User-space sync allow-list and path validation.
//!
//! User space (`~/.ryeos/.ai/`) is a mix of items, config, and
//! local-only state. To prevent accidental exfiltration of secrets
//! across the wire, sync is **explicit allow-list** (default-deny).
//! Everything listed in [`USER_SPACE_SYNC_DIRS`] participates in
//! cross-node sync; everything else stays local.
//!
//! Trust pins (`config/keys/trusted/`) are special — they cross the
//! wire as part of a user manifest but are extracted into a
//! request-scoped overlay on the remote, never merged into the
//! remote's persistent trust store. See [`USER_TRUST_SYNC_DIR`].
//!
//! Hard local-only (NEVER cross the wire):
//! - `config/keys/signing/` — private signing key
//! - `.env` — operator secrets
//! - `state/` — local cache
//! - `node/identity/` — node-local identity
//! - `node/vault/` — sealed secrets bound to local X25519
//! - `node/auth/` — authorized-key store
//! - `node/bundles/` — absolute-path bundle registrations

use std::path::{Component, Path};

use anyhow::{anyhow, Result};

use crate::objects::SourceManifest;

/// Subdirectories of a user-space `.ai/` that participate in
/// cross-node sync. Anything not in this set is local-only.
///
/// All entries are honoured by the per-request engine overlay on the
/// remote — handler descriptors, parsers, kind schemas, and verbs
/// all resolve from the materialised user root, not the
/// remote's global engine.
pub const USER_SPACE_SYNC_DIRS: &[&str] = &[
    "directives",
    "tools",
    "knowledge",
    "parsers",
    "handlers",
    "protocols",
    "node/engine/kinds",
    "node/verbs",
];

/// Trust pins are also pushed but handled separately: they go into a
/// *request-scoped* trust overlay on the remote, never merged into
/// the remote's persistent trust store.
pub const USER_TRUST_SYNC_DIR: &str = "config/keys/trusted";

/// Validate that every path in a user manifest is within the allowed
/// sync directories and contains no traversal attacks.
///
/// Uses path-component matching, not substring/prefix checks:
/// - `..` is rejected only when it appears as `Component::ParentDir`
///   (so a benign filename like `foo..bar` passes)
/// - allow-list prefix is matched component-by-component (so
///   `config/keys/trustedness/...` does NOT match `config/keys/trusted`)
pub fn validate_user_manifest_paths(manifest: &SourceManifest) -> Result<()> {
    // Pre-split allow-list dirs into component vectors for component-wise
    // prefix matching. We allocate once per call (called only from the
    // push_head validation path, not in any hot loop).
    let allowed_prefixes: Vec<Vec<&str>> = USER_SPACE_SYNC_DIRS
        .iter()
        .chain(std::iter::once(&USER_TRUST_SYNC_DIR))
        .map(|d| d.split('/').collect())
        .collect();

    for rel_path in manifest.item_source_hashes.keys() {
        // Defensive: paths shouldn't have NUL bytes.
        if rel_path.contains('\0') {
            return Err(anyhow!(
                "user manifest contains NUL byte in path '{}'",
                rel_path
            ));
        }

        let path = Path::new(rel_path);
        let components: Vec<Component> = path.components().collect();

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

        let normal_segments: Vec<&str> = components
            .iter()
            .filter_map(|c| match c {
                Component::Normal(s) => s.to_str(),
                _ => None,
            })
            .collect();

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

/// Check whether `rel_path` (relative to a user `.ai/` root) belongs
/// to the trust-pin sub-section of a user manifest. Used by the
/// remote to split a single materialised user manifest into "items"
/// vs "trust pins" without writing the pins to disk under the temp
/// user root's `config/keys/trusted/`.
pub fn is_trust_pin_path(rel_path: &str) -> bool {
    let pin_prefix: Vec<&str> = USER_TRUST_SYNC_DIR.split('/').collect();
    let segments: Vec<&str> = Path::new(rel_path)
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    segments.len() > pin_prefix.len()
        && segments
            .iter()
            .zip(pin_prefix.iter())
            .all(|(seg, pref)| seg == pref)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn manifest_with(paths: &[&str]) -> SourceManifest {
        SourceManifest {
            item_source_hashes: paths
                .iter()
                .map(|p| ((*p).to_string(), "h".to_string()))
                .collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn accepts_allowed_paths() {
        let m = manifest_with(&[
            "directives/my/refactor.md",
            "tools/my/script.py",
            "knowledge/my/notes.md",
            "config/keys/trusted/abc.toml",
            "node/engine/kinds/my.kind-schema.yaml",
            "node/verbs/my.yaml",
        ]);
        validate_user_manifest_paths(&m).expect("allowed paths must pass");
    }

    #[test]
    fn rejects_absolute_path() {
        let m = manifest_with(&["/etc/passwd"]);
        assert!(validate_user_manifest_paths(&m)
            .unwrap_err()
            .to_string()
            .contains("absolute path"));
    }

    #[test]
    fn rejects_dotdot_component() {
        let m = manifest_with(&["knowledge/../../etc/passwd"]);
        assert!(validate_user_manifest_paths(&m)
            .unwrap_err()
            .to_string()
            .contains("'..'"));
    }

    #[test]
    fn accepts_benign_double_dot_in_filename() {
        let m = manifest_with(&["knowledge/my/foo..bar.md"]);
        validate_user_manifest_paths(&m).expect("benign '..' in filename must pass");
    }

    #[test]
    fn rejects_lookalike_trust_prefix() {
        let m = manifest_with(&["config/keys/trustedness/abc.toml"]);
        assert!(validate_user_manifest_paths(&m)
            .unwrap_err()
            .to_string()
            .contains("outside allowed"));
    }

    #[test]
    fn rejects_disallowed_signing_keys() {
        let m = manifest_with(&["config/keys/signing/private_key.pem"]);
        assert!(validate_user_manifest_paths(&m)
            .unwrap_err()
            .to_string()
            .contains("outside allowed"));
    }

    #[test]
    fn is_trust_pin_path_matches_under_trust_dir() {
        assert!(is_trust_pin_path("config/keys/trusted/abc.toml"));
        assert!(is_trust_pin_path("config/keys/trusted/nested/abc.toml"));
        assert!(!is_trust_pin_path("config/keys/signing/key.pem"));
        assert!(!is_trust_pin_path("directives/my/refactor.md"));
        assert!(!is_trust_pin_path("config/keys/trustedness/abc.toml"));
    }
}
