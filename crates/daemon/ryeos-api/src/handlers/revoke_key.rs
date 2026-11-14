//! `revoke-key` — withdraw an authorized-key grant and leave evidence.
//!
//! Only the principal that granted a key, or the node's configured operator,
//! may revoke it; the configured operator's own grant is revoked offline only.
//! Revocation writes a node-signed tombstone naming the grant's digest before
//! it removes the grant file, so a restored copy of the file stays refused.
//! A new grant for the same key has a new digest and is allowed.
//!
//! Revocation is the compromise response, so by default it also:
//! - revokes every grant the revoked key (transitively) granted, since
//!   delegation was bounded by the granter (`keep_delegates` opts out; the
//!   delegates it left are listed);
//! - pauses every schedule pinned to a revoked grant;
//! - cancels every live thread whose covering authority is a revoked grant,
//!   through the thread cancel path without its ownership check
//!   (`leave_running` opts out).
//!
//! Expiry is different: a lapsed grant leaves running work alone.

use std::sync::Arc;

use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_node_runtime::identity::{
    AuthorizedKeyError, AuthorizedKeyRevokeError, GrantFileState, RevokedAuthorizedKey,
};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Fingerprint (64 lowercase hex) of the grant to revoke.
    pub fingerprint: String,
    /// Why the grant is revoked, recorded in the tombstone.
    pub reason: String,
    /// Leave the revoked grants' live threads running.
    #[serde(default)]
    pub leave_running: bool,
    /// Revoke only the named grant, not the grants it granted.
    #[serde(default)]
    pub keep_delegates: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct RevokedGrant {
    pub fingerprint: String,
    pub grant_digest: String,
    pub revoked_at: String,
    pub revoked_by: String,
    pub reason: String,
    /// The grant was already revoked; this call only confirmed it.
    pub already_revoked: bool,
    /// The revoked key this grant was cascaded from; absent for the named
    /// grant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cascaded_from: Option<String>,
}

impl From<&RevokedAuthorizedKey> for RevokedGrant {
    fn from(grant: &RevokedAuthorizedKey) -> Self {
        Self {
            fingerprint: grant.fingerprint.clone(),
            grant_digest: grant.grant_digest.clone(),
            revoked_at: grant.revoked_at.clone(),
            revoked_by: grant.revoked_by.clone(),
            reason: grant.reason.clone(),
            already_revoked: grant.already_revoked,
            cascaded_from: grant.cascaded_from.clone(),
        }
    }
}

#[derive(Debug, Default, serde::Serialize)]
pub struct LiveWork {
    /// `leave_running` was asked: nothing was cancelled.
    pub left_running: bool,
    /// Threads cancelled because their covering authority was revoked.
    pub cancelled: Vec<String>,
    /// Threads that could not be cancelled, with why. The revocation itself
    /// stands; cancel these with `threads.cancel` as their owner.
    pub failed: Vec<CancelFailure>,
}

#[derive(Debug, serde::Serialize)]
pub struct CancelFailure {
    pub thread_id: String,
    pub error: String,
}

#[derive(serde::Serialize)]
pub struct Response {
    /// The grant that was named.
    pub fingerprint: String,
    /// Every grant revoked: the named one first, then cascaded delegates.
    pub revoked: Vec<RevokedGrant>,
    /// With `keep_delegates`: the grants the revoked key granted that were
    /// left in place.
    pub delegates_kept: Vec<String>,
    pub schedules: super::grant_schedules::ScheduleAuthorityChanges,
    pub live_work: LiveWork,
}

fn bare(principal: &str) -> &str {
    principal.strip_prefix("fp:").unwrap_or(principal)
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    ctx.require_verified()?;
    let fingerprint = req.fingerprint.trim().to_owned();
    if fingerprint.len() != 64
        || !fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(HandlerError::BadRequest(
            "fingerprint must be a lowercase SHA-256 digest".into(),
        ));
    }
    let operator = ryeos_node_runtime::identity::NodeIdentity::load(
        &state.config.operator_signing_key_path,
    )
    .map_err(|error| HandlerError::Internal(format!("load configured operator: {error:#}")))?;
    if fingerprint == operator.fingerprint() {
        return Err(super::grant_terms::revocation_refusal(
            &AuthorizedKeyRevokeError::OperatorGrantIsOfflineOnly,
        ));
    }

    let auth_dir = state.config.authorized_keys_dir.clone();
    let revoker_is_operator =
        ryeos_app::operator_authority::require_local_configured_operator(&state, &ctx).is_ok();
    let refuse_stranger = || {
        super::grant_terms::revocation_refusal(&AuthorizedKeyRevokeError::NotGranterOrOperator {
            fingerprint: fingerprint.clone(),
        })
    };
    if !revoker_is_operator {
        // The grant is read without judging expiry: revoking a lapsed grant
        // is allowed. A grant that is already tombstoned or gone can only be
        // finished by the operator.
        let existing = match ryeos_node_runtime::identity::load_verified_authorized_key_unjudged(
            &fingerprint,
            &auth_dir,
            &state.identity,
        ) {
            Ok(existing) => existing,
            Err(error)
                if matches!(
                    error.downcast_ref::<AuthorizedKeyError>(),
                    Some(AuthorizedKeyError::Revoked { .. })
                ) =>
            {
                None
            }
            Err(error) => return Err(HandlerError::Internal(format!("{error:#}"))),
        };
        match existing {
            Some(existing) if existing.was_granted_by(&ctx.fingerprint) => {}
            Some(_) | None => return Err(refuse_stranger()),
        }
    }

    let protected = vec![operator.fingerprint().to_owned()];
    let delegates_kept = if req.keep_delegates {
        ryeos_node_runtime::identity::inspect_authorized_keys(&auth_dir, &state.identity)
            .map_err(|error| HandlerError::Internal(format!("{error:#}")))?
            .into_iter()
            .filter_map(|entry| match entry.state {
                GrantFileState::Current(grant)
                    if grant.was_granted_by(&fingerprint) && grant.fingerprint != fingerprint =>
                {
                    Some(grant.fingerprint)
                }
                _ => None,
            })
            .collect()
    } else {
        Vec::new()
    };
    let revoked = ryeos_node_runtime::identity::revoke_authorized_key_cascade(
        &auth_dir,
        &fingerprint,
        &req.reason,
        bare(&ctx.fingerprint),
        req.keep_delegates,
        &protected,
        &state.identity,
    )
    .map_err(
        |error| match error.downcast_ref::<AuthorizedKeyRevokeError>() {
            Some(refusal) => super::grant_terms::revocation_refusal(refusal),
            None => HandlerError::Internal(format!("{error:#}")),
        },
    )?;
    let digests: Vec<String> = revoked
        .iter()
        .map(|grant| grant.grant_digest.clone())
        .collect();

    // The tombstones stand from here: fires and requests under the revoked
    // grants are already refused. Pause the schedules, then stop live work.
    let schedules = super::grant_schedules::pause_for_revocation(&state, &digests)
        .await
        .map_err(|error| HandlerError::Structured {
            code: "grant_revoked_schedules_not_paused".to_owned(),
            status: 500,
            body: serde_json::json!({
                "error": format!(
                    "grant {fingerprint} was revoked, but its schedules could not be paused: \
                     {error:#}. Their next fire is refused and pauses them; pause each now with \
                     `ryeos scheduler pause --schedule-id <id>`"
                ),
                "error_code": "grant_revoked_schedules_not_paused",
            }),
        })?;
    let live_work = if req.leave_running {
        LiveWork {
            left_running: true,
            ..LiveWork::default()
        }
    } else {
        cancel_live_work(&state, &digests).await?
    };

    let response = Response {
        fingerprint,
        revoked: revoked.iter().map(RevokedGrant::from).collect(),
        delegates_kept,
        schedules,
        live_work,
    };
    serde_json::to_value(response).map_err(|error| HandlerError::Internal(error.to_string()))
}

/// Cancel every live thread whose covering authority is one of `digests`,
/// through the thread cancel path without its ownership check.
async fn cancel_live_work(state: &Arc<AppState>, digests: &[String]) -> HandlerResult<LiveWork> {
    let mut work = LiveWork::default();
    for digest in digests {
        let thread_ids = state
            .state_store
            .live_thread_ids_under_grant(digest)
            .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
        for thread_id in thread_ids {
            // Cancelling an earlier thread cancels its descendants, which may
            // be in this list already finished.
            let live = state
                .state_store
                .get_thread(&thread_id)
                .map_err(|error| HandlerError::Internal(format!("{error:#}")))?
                .is_some_and(|thread| !ryeos_app::state_store::is_terminal_status(&thread.status));
            if !live {
                continue;
            }
            match super::threads_cancel::cancel_as_node_action(&thread_id, Arc::clone(state)).await
            {
                Ok(_) => work.cancelled.push(thread_id),
                Err(error) => work.failed.push(CancelFailure {
                    thread_id,
                    error: format!("{error:?}"),
                }),
            }
        }
    }
    Ok(work)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:identity/revoke-key",
    endpoint: "identity.revoke-key",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.identity/revoke-key"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use ryeos_node_runtime::identity::{
        AuthorizedBy, AuthorizedKeyPrincipalClass, GrantExpiry, NodeIdentity,
    };

    const SCOPE: &str = "ryeos.execute.service.vault/list";

    fn granter() -> String {
        format!("fp:{}", "a".repeat(64))
    }

    fn issue(state: &AppState, seed: u8, granted_by: &str) -> String {
        let key = lillux::crypto::SigningKey::from_bytes(&[seed; 32]);
        let public = key.verifying_key();
        let fingerprint = lillux::signature::compute_fingerprint(&public);
        ryeos_node_runtime::identity::create_authorized_key_toml(
            &state.config.authorized_keys_dir,
            &fingerprint,
            &base64::engine::general_purpose::STANDARD.encode(public.as_bytes()),
            &[SCOPE.to_owned()],
            "grantee",
            granted_by,
            "2026-01-01T00:00:00Z",
            &ryeos_node_runtime::identity::GrantTerms {
                expires_at: GrantExpiry::At(4_000_000_000),
                authorized_by: AuthorizedBy::principal_request(granted_by, None),
            },
            state.identity.signing_key(),
        )
        .unwrap();
        fingerprint
    }

    fn fixture() -> (Box<dyn std::any::Any>, Arc<AppState>) {
        let (tmp, owner, state) = crate::test_support::build_test_state();
        (Box::new((owner, tmp)), Arc::new(state))
    }

    fn request(fingerprint: &str) -> Request {
        Request {
            fingerprint: fingerprint.to_owned(),
            reason: "key compromised".to_owned(),
            leave_running: false,
            keep_delegates: false,
        }
    }

    fn context(principal: String) -> HandlerContext {
        HandlerContext::new_with_authority(
            principal,
            vec![SCOPE.to_owned()],
            true,
            Some(AuthorizedKeyPrincipalClass::LocalClient),
            None,
        )
        .with_signed_request_digest(Some("d".repeat(64)))
    }

    fn refusal_code(error: HandlerError) -> String {
        match error {
            HandlerError::Structured { code, .. } => code,
            other => panic!("expected a typed refusal, got {other:?}"),
        }
    }

    fn grant_exists(state: &AppState, fingerprint: &str) -> bool {
        state
            .config
            .authorized_keys_dir
            .join(format!("{fingerprint}.toml"))
            .exists()
    }

    #[tokio::test]
    async fn revoke_by_granter_allowed() {
        let (_guard, state) = fixture();
        let grantee = issue(&state, 31, &granter());
        let response = handle(request(&grantee), context(granter()), state.clone())
            .await
            .unwrap();
        assert!(!grant_exists(&state, &grantee));
        assert_eq!(response["revoked"][0]["fingerprint"], grantee.as_str());
        assert_eq!(response["revoked"][0]["reason"], "key compromised");
        assert_eq!(response["live_work"]["left_running"], false);
        assert_eq!(response["live_work"]["cancelled"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn revoke_by_stranger_refused() {
        let (_guard, state) = fixture();
        let grantee = issue(&state, 31, &granter());
        let stranger = format!("fp:{}", "e".repeat(64));
        for principal in [format!("fp:{grantee}"), stranger] {
            let error = handle(request(&grantee), context(principal), state.clone())
                .await
                .unwrap_err();
            assert_eq!(refusal_code(error), "grant_revocation_not_permitted");
        }
        assert!(grant_exists(&state, &grantee), "a refusal removes nothing");
    }

    #[tokio::test]
    async fn operator_grant_revoke_is_offline_only() {
        let (_guard, state) = fixture();
        let operator = NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
        for principal in [operator.principal_id(), granter()] {
            let error = handle(
                request(operator.fingerprint()),
                context(principal),
                state.clone(),
            )
            .await
            .unwrap_err();
            assert_eq!(refusal_code(error), "grant_is_operator");
        }
    }

    #[tokio::test]
    async fn cascade_revokes_delegates_and_lists_them() {
        let (_guard, state) = fixture();
        let middle = issue(&state, 31, &granter());
        let delegate = issue(&state, 32, &format!("fp:{middle}"));
        let unrelated = issue(&state, 33, &granter());
        let response = handle(request(&middle), context(granter()), state.clone())
            .await
            .unwrap();
        let revoked: Vec<&str> = response["revoked"]
            .as_array()
            .unwrap()
            .iter()
            .map(|grant| grant["fingerprint"].as_str().unwrap())
            .collect();
        assert_eq!(revoked, vec![middle.as_str(), delegate.as_str()]);
        assert_eq!(
            response["revoked"][1]["cascaded_from"],
            middle.as_str(),
            "the receipt names what each delegate was cascaded from"
        );
        assert!(grant_exists(&state, &unrelated));

        // keep_delegates lists what it left.
        let middle = issue(&state, 34, &granter());
        let kept = issue(&state, 35, &format!("fp:{middle}"));
        let response = handle(
            Request {
                keep_delegates: true,
                ..request(&middle)
            },
            context(granter()),
            state.clone(),
        )
        .await
        .unwrap();
        assert_eq!(response["revoked"].as_array().unwrap().len(), 1);
        assert_eq!(response["delegates_kept"], serde_json::json!([kept]));
        assert!(grant_exists(&state, &kept));
    }

    #[tokio::test]
    async fn revoke_converges_on_rerun_for_the_operator() {
        let (_guard, state) = fixture();
        let grantee = issue(&state, 31, &granter());
        let operator = NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
        for _ in 0..2 {
            let response = handle(
                request(&grantee),
                context(operator.principal_id()),
                state.clone(),
            )
            .await
            .unwrap();
            assert_eq!(response["revoked"][0]["fingerprint"], grantee.as_str());
        }
        let error = handle(
            request(&"f".repeat(64)),
            context(operator.principal_id()),
            state.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(refusal_code(error), "grant_not_found");
    }
}
