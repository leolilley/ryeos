//! Runtime-owned path policy for the signing surfaces.
//!
//! Bundle publishing and operator signing both walk `.ai/` trees, but they have
//! different ownership boundaries. In a project tree, node runtime state and
//! signing secrets are not authoring source. In a bundle tree, declarative
//! node configuration is authoring source and must be signed by the bundle
//! publisher, while secret keys remain categorically excluded.
//!
//! Both boundaries reuse the sync-policy classifier in
//! `ryeos_state::project_sync`; no path list is duplicated here.

use std::path::Path;

use ryeos_engine::AI_DIR;
use ryeos_state::project_sync::{classify_project_ai_path, ProjectAiPathClass};

/// True when a `.ai/`-relative path is node runtime state or a signing secret,
/// and therefore never a signable authoring source.
///
/// Reuses `classify_project_ai_path`'s code-enforced floor:
/// - `NodeOwned` — `.ai/state`, `.ai/node/{schedules,routes,bundles}`;
/// - `NeverDeploySecret` — `.ai/config/keys/signing`, node identity/auth/vault.
///
/// The floor is exhaustive: every runtime writer emits under `.ai/state/`
/// (per-thread transcripts in `.ai/state/threads/<id>/`, graph-run transcripts
/// in `.ai/state/graphs/<id>/`), so nothing writes under `.ai/knowledge/` at
/// runtime and the whole `.ai/knowledge` surface stays authorable.
///
/// The `ignore` matcher is deliberately unused: runtime ownership is a floor,
/// independent of any project ignore file.
pub fn is_runtime_owned_ai_path(ai_rel_path: &str) -> bool {
    matches!(
        classify_project_ai_path(ai_rel_path, None),
        ProjectAiPathClass::NodeOwned { .. } | ProjectAiPathClass::NeverDeploySecret { .. }
    )
}

/// True when a `.ai/`-relative path is secret key material that must never be
/// signed or published as an item.
///
/// Bundle-owned declarative node configuration under `.ai/node/` is authoring
/// source and must be re-signed by the bundle publisher. This narrower policy
/// is therefore used by bundle publication, while project/operator signing
/// continues to use [`is_runtime_owned_ai_path`] to exclude node-owned state.
pub fn is_never_signable_secret_ai_path(ai_rel_path: &str) -> bool {
    matches!(
        classify_project_ai_path(ai_rel_path, None),
        ProjectAiPathClass::NeverDeploySecret { .. }
    )
}

/// As [`is_runtime_owned_ai_path`], but taking an absolute file path plus the
/// `.ai/` root it lives under. Reconstructs the `.ai/<...>` relative form the
/// floor classifier expects. Returns `false` when `file_path` is not under
/// `ai_root` (the caller's walk guarantees it is; this is defensive).
pub fn is_runtime_owned_file(file_path: &Path, ai_root: &Path) -> bool {
    let Ok(rel) = file_path.strip_prefix(ai_root) else {
        return false;
    };
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    is_runtime_owned_ai_path(&format!("{AI_DIR}/{rel_str}"))
}

/// Absolute-path form of [`is_never_signable_secret_ai_path`].
pub fn is_never_signable_secret_file(file_path: &Path, ai_root: &Path) -> bool {
    let Ok(rel) = file_path.strip_prefix(ai_root) else {
        return false;
    };
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    is_never_signable_secret_ai_path(&format!("{AI_DIR}/{rel_str}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_owned_and_secret_paths_are_runtime_owned() {
        for p in [
            ".ai/state/threads/abc.json",
            ".ai/node/schedules/nightly.yaml",
            ".ai/node/routes/inbound.yaml",
            ".ai/node/bundles/core/manifest.yaml",
            ".ai/config/keys/signing/private_key.pem",
        ] {
            assert!(is_runtime_owned_ai_path(p), "expected runtime-owned: {p}");
        }
    }

    #[test]
    fn authoring_surfaces_are_not_runtime_owned() {
        for p in [
            ".ai/directives/hello.md",
            ".ai/tools/ryeos/core/sign.yaml",
            ".ai/graphs/app/flow.yaml",
            ".ai/knowledge/app/notes.md",
            ".ai/config/keys/trusted/publisher.pem",
        ] {
            assert!(!is_runtime_owned_ai_path(p), "expected signable: {p}");
        }
    }

    #[test]
    fn bundle_node_config_is_authoring_source_but_secrets_never_are() {
        for path in [
            ".ai/node/routes/execute.yaml",
            ".ai/node/schedules/nightly.yaml",
        ] {
            assert!(is_runtime_owned_ai_path(path));
            assert!(!is_never_signable_secret_ai_path(path));
        }
        assert!(!is_never_signable_secret_ai_path(
            ".ai/node/commands/start.yaml"
        ));

        for path in [
            ".ai/config/keys/signing/private_key.pem",
            ".ai/node/identity/private_key.pem",
            ".ai/node/auth/private_key.pem",
            ".ai/node/vault/private_key.pem",
        ] {
            assert!(is_never_signable_secret_ai_path(path));
        }
    }

    #[test]
    fn runtime_transcript_output_is_node_owned_state() {
        // Runtime-emitted thread/graph transcripts live under `.ai/state/`,
        // which the `NodeOwned` floor covers — excluded from signing.
        assert!(is_runtime_owned_ai_path(
            ".ai/state/threads/T-abc/transcript.md"
        ));
        assert!(is_runtime_owned_ai_path(
            ".ai/state/threads/T-abc/capabilities.md"
        ));
        assert!(is_runtime_owned_ai_path(".ai/state/graphs/flow/gr-1.md"));
    }

    #[test]
    fn knowledge_surface_is_entirely_signable() {
        // Nothing writes under `.ai/knowledge/` at runtime, so the whole
        // surface is authored source — including subtrees named `state`.
        assert!(!is_runtime_owned_ai_path(".ai/knowledge/state/notes.md"));
        assert!(!is_runtime_owned_ai_path(".ai/knowledge/state.md"));
        assert!(!is_runtime_owned_ai_path(".ai/knowledge/stateful/notes.md"));
    }

    #[test]
    fn absolute_file_form_maps_through_ai_root() {
        let ai_root = Path::new("/bundle/source/.ai");
        assert!(is_runtime_owned_file(
            Path::new("/bundle/source/.ai/node/schedules/x.yaml"),
            ai_root,
        ));
        assert!(!is_runtime_owned_file(
            Path::new("/bundle/source/.ai/directives/x.md"),
            ai_root,
        ));
        // Outside the root → defensively not runtime-owned.
        assert!(!is_runtime_owned_file(
            Path::new("/elsewhere/.ai/state/x.json"),
            ai_root,
        ));
    }
}
