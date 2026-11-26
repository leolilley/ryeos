//! Who may run a live project at a path. One function, called by every site
//! that takes a project path from a request and turns it into a live project.
//!
//! The node's admitted operator names any directory the node-internal guard
//! allows, as before. Any other key may run a live project only at a canonical
//! root that an owner the key speaks for has registered on this node
//! (`project_registry`), compared exactly: never a subdirectory, never an
//! ancestor. Every way that can fail for such a key (a relative path, a path
//! that does not exist or cannot be opened, a node-internal root, an
//! unregistered root, a registry that cannot be read) is the one refusal,
//! [`LiveProjectPathRefusal::NotRegisteredToCaller`], so the answer is no
//! oracle for what exists on the host.

use std::path::{Path, PathBuf};

use crate::state::AppState;

/// Typed refusal code of [`LiveProjectPathRefusal::NotRegisteredToCaller`].
pub const LIVE_PROJECT_NOT_REGISTERED_TO_CALLER: &str = "live_project_not_registered_to_caller";

/// Why a requested live project path was not admitted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LiveProjectPathRefusal {
    /// A key that is not the admitted operator named a path that is not a
    /// registered checkout of an owner it speaks for. The body is one fixed
    /// text for every cause.
    #[error(
        "live_project_not_registered_to_caller: a live project path must be a checkout \
         registered to an owner this key speaks for"
    )]
    NotRegisteredToCaller,
    /// The operator named a path that cannot be used. Carries the text the
    /// sites gave before this function existed, so the operator's answers do
    /// not change.
    #[error("{0}")]
    BadPath(String),
    /// The operator named a node-internal root.
    #[error("{0}")]
    NodeInternal(String),
}

/// A live project path that was admitted: the canonical root and the pinned
/// directory that was checked. Use the pinned directory, not a second
/// resolution of the path.
#[derive(Debug)]
pub struct AdmittedLiveProject {
    pub canonical: PathBuf,
    pub directory: lillux::PinnedDirectory,
}

/// Admit `requested` as a live project for `key`. `operator` is whether the
/// key is the node's admitted operator, judged by the caller from its request
/// context with `operator_authority::require_admitted_operator`.
pub fn admit_live_project_path(
    state: &AppState,
    key: &str,
    operator: bool,
    requested: &Path,
) -> Result<AdmittedLiveProject, LiveProjectPathRefusal> {
    use LiveProjectPathRefusal::{BadPath, NodeInternal, NotRegisteredToCaller};

    if !requested.is_absolute() {
        return Err(if operator {
            BadPath("project_path must be absolute".to_owned())
        } else {
            NotRegisteredToCaller
        });
    }
    let canonical = match std::fs::canonicalize(requested) {
        Ok(canonical) => canonical,
        Err(error) => {
            return Err(if operator {
                BadPath(format!(
                    "live project_path '{}' cannot be resolved for the selected execution policy: {error}",
                    requested.display()
                ))
            } else {
                NotRegisteredToCaller
            });
        }
    };
    if let Err(refusal) = crate::project_root_guard::refuse_node_internal_project_root_for_app(
        &canonical,
        &state.config.app_root,
    ) {
        return Err(if operator {
            NodeInternal(refusal.to_string())
        } else {
            NotRegisteredToCaller
        });
    }
    if !operator && !registered_to_key(state, key, &canonical) {
        return Err(NotRegisteredToCaller);
    }
    let directory = match lillux::PinnedDirectory::open(&canonical) {
        Ok(Some(directory)) => directory,
        Ok(None) => {
            return Err(if operator {
                BadPath(format!(
                    "live project_path '{}' is not a directory",
                    canonical.display()
                ))
            } else {
                NotRegisteredToCaller
            });
        }
        Err(error) => {
            return Err(if operator {
                BadPath(format!(
                    "live project_path '{}' cannot be opened: {error:#}",
                    canonical.display()
                ))
            } else {
                NotRegisteredToCaller
            });
        }
    };
    Ok(AdmittedLiveProject {
        canonical,
        directory,
    })
}

/// Whether an owner `key` acts for has registered exactly `canonical`. Read
/// live on every call. A key the node cannot resolve to an owner, a retired
/// or revoked key, and a registry that cannot be read are all "no".
fn registered_to_key(state: &AppState, key: &str, canonical: &Path) -> bool {
    let Ok(owner) = state.engine.trust_store.owner_for(key) else {
        return false;
    };
    match crate::project_registry::load_registrations(
        &state.config.app_root,
        state.identity.verifying_key(),
    ) {
        Ok(registrations) => registrations.iter().any(|loaded| {
            loaded.registration.identity.owner() == &owner.principal
                && loaded
                    .registration
                    .paths
                    .iter()
                    .any(|path| path == canonical)
        }),
        Err(error) => {
            tracing::warn!(
                error = %format!("{error:#}"),
                "project registrations unreadable; no live project path is admitted to a non-operator key"
            );
            false
        }
    }
}
