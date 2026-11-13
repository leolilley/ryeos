//! `renew-key` — re-sign an authorized-key grant with a new expiry.
//!
//! Only the principal that granted a key, or the node's configured operator,
//! may renew it: a grantee cannot extend its own grant. Renewal keeps the key,
//! class, origin, scopes, label, granting authority and creation time, and
//! replaces the expiry and the recorded provenance with the renewing request.
//! A lapsed grant can be renewed; the configured operator's grant never lapses
//! and is refused.

use std::sync::Arc;

use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_node_runtime::identity::{AuthorizedKeyRenewError, GrantTerms};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Fingerprint (64 lowercase hex) of the grant to renew.
    pub fingerprint: String,
    /// Requested new lifetime in seconds, counted from now. Absent: the
    /// class maximum from the node's authorization policy. Above it: refused.
    #[serde(default)]
    pub lifetime_secs: Option<u64>,
}

#[derive(serde::Serialize)]
pub struct Response {
    pub fingerprint: String,
    pub label: String,
    pub principal_class: String,
    pub scopes: Vec<String>,
    pub previous_expires_at: String,
    pub expires_at: String,
    pub authorized_by: ryeos_node_runtime::identity::AuthorizedBy,
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
    let auth_dir = state.config.authorized_keys_dir.clone();
    // The grant is read without judging its expiry: renewing a lapsed grant is
    // the point.
    let existing = ryeos_node_runtime::identity::load_verified_authorized_key_unjudged(
        &fingerprint,
        &auth_dir,
        &state.identity,
    )
    .map_err(|error| HandlerError::Internal(format!("{error:#}")))?
    .ok_or_else(|| {
        super::grant_terms::renewal_refusal(&AuthorizedKeyRenewError::NotFound {
            fingerprint: fingerprint.clone(),
        })
    })?;

    let renewer_is_operator =
        ryeos_app::operator_authority::require_local_configured_operator(&state, &ctx).is_ok();
    if !renewer_is_operator {
        if !existing.was_granted_by(&ctx.fingerprint) {
            return Err(super::grant_terms::renewal_refusal(
                &AuthorizedKeyRenewError::NotGranterOrOperator {
                    fingerprint: fingerprint.clone(),
                },
            ));
        }
        // A granter whose own authority has narrowed since cannot renew what it
        // no longer covers.
        for scope in &existing.scopes {
            let covered = state
                .authorizer
                .authorize(
                    &ctx.scopes,
                    &ryeos_runtime::authorizer::AuthorizationPolicy::require(scope.as_str()),
                )
                .is_ok();
            if !covered {
                return Err(super::grant_terms::renewal_refusal(
                    &AuthorizedKeyRenewError::ScopesNoLongerCovered {
                        fingerprint: fingerprint.clone(),
                        scope: scope.clone(),
                    },
                ));
            }
        }
    }

    let terms: GrantTerms = super::grant_terms::issuance_terms(
        &state,
        existing.principal_class,
        req.lifetime_secs,
        super::grant_terms::caller_authorized_by(&ctx),
    )?;
    let renewed = ryeos_node_runtime::identity::renew_authorized_key_toml(
        &auth_dir,
        &fingerprint,
        &ctx.fingerprint,
        renewer_is_operator,
        &terms,
        &state.identity,
    )
    .map_err(
        |error| match error.downcast_ref::<AuthorizedKeyRenewError>() {
            Some(refusal) => super::grant_terms::renewal_refusal(refusal),
            None => HandlerError::Internal(format!("{error:#}")),
        },
    )?;

    let response = Response {
        fingerprint,
        label: renewed.label,
        principal_class: renewed.principal_class.as_str().to_owned(),
        scopes: renewed.scopes,
        previous_expires_at: super::grant_terms::expiry_text(renewed.previous_expires_at),
        expires_at: super::grant_terms::expiry_text(renewed.expires_at),
        authorized_by: terms.authorized_by,
    };
    serde_json::to_value(response).map_err(|error| HandlerError::Internal(error.to_string()))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:identity/renew-key",
    endpoint: "identity.renew-key",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.identity/renew-key"],
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

    /// A node state holding one lapsed local-client grant that `granter()`
    /// issued. Returns the state and the grantee's fingerprint.
    fn lapsed_grant() -> (Box<dyn std::any::Any>, Arc<AppState>, String) {
        let (tmp, owner, state) = crate::test_support::build_test_state();
        // The fixture directories and store owner live as long as the guard.
        let guard: Box<dyn std::any::Any> = Box::new((owner, tmp));
        let grantee = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
        let public = grantee.verifying_key();
        let fingerprint = lillux::signature::compute_fingerprint(&public);
        ryeos_node_runtime::identity::create_authorized_key_toml(
            &state.config.authorized_keys_dir,
            &fingerprint,
            &base64::engine::general_purpose::STANDARD.encode(public.as_bytes()),
            &[SCOPE.to_owned()],
            "grantee",
            &granter(),
            "2026-01-01T00:00:00Z",
            &ryeos_node_runtime::identity::GrantTerms {
                expires_at: GrantExpiry::At(1_000),
                authorized_by: AuthorizedBy::principal_request(&granter(), None),
            },
            state.identity.signing_key(),
        )
        .unwrap();
        (guard, Arc::new(state), fingerprint)
    }

    fn request(fingerprint: &str) -> Request {
        Request {
            fingerprint: fingerprint.to_owned(),
            lifetime_secs: None,
        }
    }

    fn context(principal: String, scopes: &[&str]) -> HandlerContext {
        HandlerContext::new_with_authority(
            principal,
            scopes.iter().map(|scope| (*scope).to_owned()).collect(),
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

    #[tokio::test]
    async fn renew_by_granter_extends_a_lapsed_grant_and_records_the_renewer() {
        let (_guard, state, fingerprint) = lapsed_grant();
        let response = handle(
            request(&fingerprint),
            context(granter(), &[SCOPE]),
            state.clone(),
        )
        .await
        .unwrap();
        assert_eq!(response["previous_expires_at"], "1970-01-01T00:16:40Z");
        let grant = ryeos_node_runtime::identity::load_verified_authorized_key(
            &fingerprint,
            &state.config.authorized_keys_dir,
            &state.identity,
        )
        .unwrap()
        .expect("a renewed grant authenticates again");
        assert_eq!(grant.granted_by, granter());
        assert_eq!(grant.authorized_by.principal, granter());
        assert_eq!(
            grant.authorized_by.request_digest.as_deref(),
            Some("d".repeat(64).as_str())
        );
    }

    #[tokio::test]
    async fn renew_is_refused_for_the_grantee_and_for_other_principals() {
        let (_guard, state, fingerprint) = lapsed_grant();
        let stranger = format!("fp:{}", "e".repeat(64));
        for principal in [format!("fp:{fingerprint}"), stranger] {
            let error = handle(
                request(&fingerprint),
                context(principal, &[SCOPE]),
                state.clone(),
            )
            .await
            .unwrap_err();
            assert_eq!(refusal_code(error), "grant_renewal_not_permitted");
        }
        assert!(
            ryeos_node_runtime::identity::load_verified_authorized_key(
                &fingerprint,
                &state.config.authorized_keys_dir,
                &state.identity,
            )
            .is_err(),
            "a refused renewal leaves the grant lapsed"
        );
    }

    #[tokio::test]
    async fn renew_by_the_granter_refuses_scopes_it_no_longer_holds() {
        let (_guard, state, fingerprint) = lapsed_grant();
        let error = handle(
            request(&fingerprint),
            context(granter(), &["ryeos.execute.service.vault/set"]),
            state.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(refusal_code(error), "grant_renewal_scopes_not_covered");
    }

    #[tokio::test]
    async fn the_configured_operator_renews_any_grant() {
        let (_guard, state, fingerprint) = lapsed_grant();
        let operator = NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
        handle(
            request(&fingerprint),
            context(operator.principal_id(), &[]),
            state.clone(),
        )
        .await
        .unwrap();
        assert!(
            ryeos_node_runtime::identity::load_verified_authorized_key(
                &fingerprint,
                &state.config.authorized_keys_dir,
                &state.identity,
            )
            .unwrap()
            .is_some()
        );
    }

    #[tokio::test]
    async fn renew_names_a_missing_grant_and_over_maximum_lifetimes() {
        let (_guard, state, fingerprint) = lapsed_grant();
        let error = handle(
            request(&"f".repeat(64)),
            context(granter(), &[SCOPE]),
            state.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(refusal_code(error), "grant_not_found");
        let maximum =
            ryeos_node_runtime::node_policy::sections::authorization::NodeAuthorizationPolicy::standard()
                .max_lifetime_secs
                .local_client;
        let error = handle(
            Request {
                fingerprint,
                lifetime_secs: Some(maximum + 1),
            },
            context(granter(), &[SCOPE]),
            state.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(refusal_code(error), "grant_lifetime_exceeds_policy");
    }
}
