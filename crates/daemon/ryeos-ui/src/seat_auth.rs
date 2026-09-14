//! Seat caller authentication for ryeos-ui services.
//!
//! Signed UI sources are invoked either by a signed client or through an exact
//! entry in a daemon-compiled browser-session binding. A browser cookie is
//! transport identity only: it cannot call source services directly and
//! thereby bypass the compiled source coordinate and parameter contract.

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;

use crate::browser_session::BrowserSession;
use crate::state::get_ui_state;

tokio::task_local! {
    /// Daemon-retained authority for one compiled UI dispatch. This sideband
    /// is scoped by `ui.invocations.dispatch`; it is neither serialized into
    /// a request nor reconstructible from a browser cookie.
    static COMPILED_UI_SESSION: BrowserSession;
}

pub async fn with_compiled_ui_session<F>(session: BrowserSession, future: F) -> F::Output
where
    F: std::future::Future,
{
    COMPILED_UI_SESSION.scope(session, future).await
}

pub fn compiled_ui_session() -> Option<BrowserSession> {
    COMPILED_UI_SESSION.try_with(Clone::clone).ok()
}

pub enum SeatCaller {
    Session(BrowserSession),
    Operator { fingerprint: String },
}

impl SeatCaller {
    /// Exact project path derived from the retained directory descriptor.
    /// UI handlers must use this for project-aware work; the session's
    /// `project_root` string is a display projection and must not be reopened
    /// as authority.
    pub fn project_path(&self) -> Result<Option<std::path::PathBuf>, HandlerError> {
        Ok(self
            .project_directory()?
            .map(|authority| authority.descriptor_path())
            .transpose()
            .map_err(|error| HandlerError::Internal(error.to_string()))?)
    }

    /// Clone the retained project directory authority without resolving its
    /// diagnostic pathname. File-serving handlers must keep traversal rooted
    /// in this handle rather than converting it to a path and reopening it.
    pub fn project_directory(&self) -> Result<Option<lillux::PinnedDirectory>, HandlerError> {
        let Self::Session(session) = self else {
            return Ok(None);
        };
        session
            .project_authority
            .as_ref()
            .map(|authority| {
                authority
                    .ensure_path_binding()
                    .map_err(|_| HandlerError::Forbidden("project authority changed".into()))?;
                authority
                    .try_clone()
                    .map_err(|error| HandlerError::Internal(error.to_string()))
            })
            .transpose()
    }
}

/// Require the verified principal restored by signed execution or compiled
/// binding dispatch. Raw browser-session principals are deliberately refused.
pub fn require_seat_caller(
    ctx: &HandlerContext,
    state: &AppState,
) -> Result<SeatCaller, HandlerError> {
    if let Some(session) = compiled_ui_session() {
        return Ok(SeatCaller::Session(session));
    }
    if let Some(session_id) = ctx.fingerprint.strip_prefix("session:") {
        let valid = get_ui_state(state)
            .ok_or_else(|| HandlerError::Internal("UiState not set".into()))?
            .browser_sessions
            .get_session(session_id)
            .is_some();
        return Err(HandlerError::Forbidden(if valid {
            "browser data access requires a compiled binding coordinate".into()
        } else {
            "session expired or invalid".into()
        }));
    }
    if ctx.verified && !ctx.fingerprint.is_empty() {
        return Ok(SeatCaller::Operator {
            fingerprint: ctx.fingerprint.clone(),
        });
    }
    Err(HandlerError::Forbidden(
        "browser session or verified operator required".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use ryeos_api::surface_views::EffectiveUiItemIdentity;
    use ryeos_engine::resolution::{EffectiveDefinitionDigest, TrustClass};

    fn session() -> BrowserSession {
        let now = Instant::now();
        BrowserSession {
            session_id: "session-sideband".into(),
            created_at: now,
            expires_at: now + Duration::from_secs(30),
            compiled_binding: Arc::new(crate::compiled_binding::SessionCompiledUiBinding {
                binding_digest: "11".repeat(32),
                posture: crate::compiled_binding::EffectiveUiPosture::ObservationOnly,
                binding: crate::compiled_binding::CompiledUiBinding {
                    contract_revision: crate::UI_BINDING_CONTRACT_REVISION.into(),
                    principal_id: "fp:test".into(),
                    project_root: None,
                    request_engine_generation_identity: "generation:test".into(),
                    node_policy_generation_digest: "22".repeat(32),
                    surface: EffectiveUiItemIdentity {
                        canonical_ref: "surface:ryeos/ui/base-observe".into(),
                        effective_definition_digest: EffectiveDefinitionDigest::parse(
                            "33".repeat(32),
                        )
                        .expect("fixture digest"),
                        effective_trust_class: TrustClass::TrustedBundle,
                    },
                    views: Default::default(),
                    sources: Default::default(),
                    affordances: Default::default(),
                    surface_route: None,
                    attenuated: Vec::new(),
                },
            }),
            effective_surface: serde_json::json!({}),
            granted_caps: Vec::new(),
            project_root: None,
            surface_ref: "surface:ryeos/ui/base-observe".into(),
            user_principal_id: Some("fp:user".into()),
            project_authority: None,
        }
    }

    #[tokio::test]
    async fn compiled_session_authority_exists_only_inside_dispatch_scope() {
        assert!(compiled_ui_session().is_none());
        with_compiled_ui_session(session(), async {
            let retained = compiled_ui_session().expect("retained dispatch authority");
            assert_eq!(retained.session_id, "session-sideband");
            assert_eq!(retained.user_principal_id.as_deref(), Some("fp:user"));
        })
        .await;
        assert!(compiled_ui_session().is_none());
    }
}
