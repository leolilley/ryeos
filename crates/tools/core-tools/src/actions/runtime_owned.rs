//! Runtime-owned path policy for the signing surfaces.
//!
//! Bundle publishing and operator signing both walk `.ai/` trees. Some `.ai/`
//! subtrees are node runtime state or signing secrets, not authoring source:
//! a node writes them while it runs, and they must never be signed as items.
//! Signing them turns daemon state (schedules, routes, thread output, keys)
//! into a bulk of failed items and false namespace warnings.
//!
//! This is the single sync-policy floor (`ryeos_state::project_sync`) reused
//! for the signing surfaces. No path list is duplicated here — the policy
//! lives in exactly one place.

use std::path::Path;

use ryeos_engine::AI_DIR;
use ryeos_state::project_sync::{classify_project_ai_path, ProjectAiPathClass};

/// Runtime thread knowledge is emitted by the daemon under this subpath of the
/// otherwise-authorable `.ai/knowledge` surface. It is runtime output, not
/// authored source, so it is excluded from signing.
///
/// This is the one runtime-output subpath the sync-policy floor does not yet
/// cover (`.ai/knowledge` is a deployable surface). The clean end-state is
/// relocating the runtime writer under `.ai/state/` — node-owned by the floor,
/// invisible to every signing surface — at which point this constant becomes a
/// dead no-op. Until that writer moves, this declares the subpath as runtime
/// output so a bundle publish never signs daemon-emitted thread knowledge.
const RUNTIME_KNOWLEDGE_OUTPUT_PREFIX: &str = ".ai/knowledge/state";

/// True when a `.ai/`-relative path is node runtime state or a signing secret,
/// and therefore never a signable authoring source.
///
/// Reuses `classify_project_ai_path`'s code-enforced floor:
/// - `NodeOwned` — `.ai/state`, `.ai/node/{schedules,routes,bundles}`;
/// - `NeverDeploySecret` — `.ai/config/keys/signing`, node identity/auth/vault.
///
/// Plus the one runtime-output subpath the floor does not yet own
/// ([`RUNTIME_KNOWLEDGE_OUTPUT_PREFIX`]).
///
/// The `ignore` matcher is deliberately unused: runtime ownership is a floor,
/// independent of any project ignore file.
pub fn is_runtime_owned_ai_path(ai_rel_path: &str) -> bool {
    if ai_rel_path == RUNTIME_KNOWLEDGE_OUTPUT_PREFIX
        || ai_rel_path.starts_with(&format!("{RUNTIME_KNOWLEDGE_OUTPUT_PREFIX}/"))
    {
        return true;
    }
    matches!(
        classify_project_ai_path(ai_rel_path, None),
        ProjectAiPathClass::NodeOwned { .. } | ProjectAiPathClass::NeverDeploySecret { .. }
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
    fn runtime_thread_knowledge_output_is_runtime_owned() {
        // Daemon-emitted thread knowledge under `.ai/knowledge/state/` is
        // runtime output, not authored source — excluded from signing.
        assert!(is_runtime_owned_ai_path(".ai/knowledge/state/thread-xyz.md"));
        assert!(is_runtime_owned_ai_path(".ai/knowledge/state"));
        // A sibling authored knowledge item named `state.md` is NOT under the
        // runtime-output subpath and stays signable.
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
