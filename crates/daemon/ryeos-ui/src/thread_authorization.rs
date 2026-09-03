//! Exact-thread authorization shared by RyeOS UI read services.
//!
//! Seat authentication establishes a caller lane. It does not prove that the
//! caller may observe an arbitrary thread ID. This module reads the signed
//! thread subject first and applies project/owner authority before any result,
//! event, artifact, child, capsule, or definition evidence is loaded.

use std::path::PathBuf;

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_app::state_store::AuthoritativeThreadSubject;
use ryeos_state::objects::ExecutionProjectAuthority;

use crate::seat_auth::SeatCaller;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExactThreadCaller {
    Session {
        canonical_project_root: Option<PathBuf>,
        principal_id: Option<String>,
    },
    Operator {
        principal_id: String,
    },
}

impl ExactThreadCaller {
    fn from_seat(caller: &SeatCaller) -> Self {
        match caller {
            SeatCaller::Session(session) => Self::Session {
                canonical_project_root: session
                    .project_root
                    .as_ref()
                    .and_then(|root| std::fs::canonicalize(root).ok()),
                principal_id: session.user_principal_id.clone(),
            },
            SeatCaller::Operator { fingerprint } => Self::Operator {
                principal_id: fingerprint.clone(),
            },
        }
    }

    fn principal_id(&self) -> Option<&str> {
        match self {
            Self::Session { principal_id, .. } => principal_id.as_deref(),
            Self::Operator { principal_id } => Some(principal_id),
        }
    }

    fn authorizes(&self, subject: &AuthoritativeThreadSubject) -> bool {
        match (&subject.project_authority, self) {
            (
                ExecutionProjectAuthority::LiveProject { canonical_root, .. },
                Self::Session {
                    canonical_project_root,
                    ..
                },
            ) => canonical_project_root.as_ref() == Some(canonical_root),
            (ExecutionProjectAuthority::LiveProject { .. }, Self::Operator { .. })
            | (ExecutionProjectAuthority::Projectless { .. }, _)
            | (ExecutionProjectAuthority::PinnedGeneration { .. }, _) => {
                self.principal_id() == subject.requested_by.as_deref()
            }
        }
    }
}

/// Authorize one or two exact thread subjects as one indistinguishable read.
/// Missing and unauthorized subjects both return `NotFound`.
pub(crate) fn authorize_exact_thread_subjects(
    ctx: &HandlerContext,
    state: &AppState,
    caller: &SeatCaller,
    thread_ids: &[&str],
) -> Result<Vec<AuthoritativeThreadSubject>, HandlerError> {
    if ctx.authenticated_origin_site_id.is_some() {
        return Err(HandlerError::NotFound);
    }

    let subjects = state
        .state_store
        .authoritative_thread_subjects(thread_ids)
        .map_err(|error| {
            tracing::error!(error = %error, "exact thread authority read failed");
            HandlerError::Internal("exact thread authority read failed".to_string())
        })?;
    if subjects.iter().any(Option::is_none) {
        return Err(HandlerError::NotFound);
    }

    let caller = ExactThreadCaller::from_seat(caller);
    let subjects = subjects.into_iter().flatten().collect::<Vec<_>>();
    if subjects.iter().all(|subject| caller.authorizes(subject)) {
        Ok(subjects)
    } else {
        Err(HandlerError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ryeos_state::objects::{
        ChildProjectAuthorityPolicy, EnvironmentAuthority, LiveAccessAuthority,
        LiveFilesystemConfinement, LiveProjectAccess, PinnedProjectRealization, ThreadStatus,
    };

    use super::*;

    fn subject(
        requested_by: Option<&str>,
        project_authority: ExecutionProjectAuthority,
    ) -> AuthoritativeThreadSubject {
        AuthoritativeThreadSubject {
            thread_id: "T-subject".to_string(),
            chain_root_id: "T-subject".to_string(),
            status: ThreadStatus::Running,
            requested_by: requested_by.map(str::to_string),
            project_authority,
            admitted_launch_capsule_hash: None,
        }
    }

    fn live(root: &str) -> ExecutionProjectAuthority {
        ExecutionProjectAuthority::LiveProject {
            authority_id: "authority".to_string(),
            authored_project_identity: "project".to_string(),
            canonical_root: PathBuf::from(root),
            live_access: LiveAccessAuthority {
                access: LiveProjectAccess::ReadOnly,
                authorized_write_namespaces: Vec::new(),
                confinement: LiveFilesystemConfinement::UnconfinedHost,
            },
            environment: EnvironmentAuthority::None,
            capability_ceiling: Vec::new(),
            child_policy: ChildProjectAuthorityPolicy::Inherit,
        }
    }

    fn pinned() -> ExecutionProjectAuthority {
        ExecutionProjectAuthority::PinnedGeneration {
            stable_project_identity: "project".to_string(),
            display_path: None,
            base_snapshot_hash: "a".repeat(64),
            snapshot_hash: "b".repeat(64),
            realization: PinnedProjectRealization::ReadOnly,
            environment: EnvironmentAuthority::None,
            capability_ceiling: Vec::new(),
            child_policy: ChildProjectAuthorityPolicy::Inherit,
        }
    }

    #[test]
    fn live_session_requires_the_same_canonical_project() {
        let caller = ExactThreadCaller::Session {
            canonical_project_root: Some(Path::new("/project/a").to_path_buf()),
            principal_id: Some("fp:owner".to_string()),
        };
        assert!(caller.authorizes(&subject(None, live("/project/a"))));
        assert!(!caller.authorizes(&subject(None, live("/project/b"))));
    }

    #[test]
    fn operator_requires_durable_ownership_for_live_subjects() {
        let owner = ExactThreadCaller::Operator {
            principal_id: "fp:owner".to_string(),
        };
        assert!(owner.authorizes(&subject(Some("fp:owner"), live("/project"))));
        assert!(!owner.authorizes(&subject(Some("fp:other"), live("/project"))));
    }

    #[test]
    fn pinned_and_projectless_sessions_require_durable_ownership() {
        let owner = ExactThreadCaller::Session {
            canonical_project_root: None,
            principal_id: Some("fp:owner".to_string()),
        };
        let anonymous = ExactThreadCaller::Session {
            canonical_project_root: None,
            principal_id: None,
        };
        assert!(owner.authorizes(&subject(Some("fp:owner"), pinned())));
        assert!(owner.authorizes(&subject(
            Some("fp:owner"),
            ExecutionProjectAuthority::PROJECTLESS,
        )));
        assert!(!anonymous.authorizes(&subject(
            Some("fp:owner"),
            ExecutionProjectAuthority::PROJECTLESS,
        )));
    }
}
