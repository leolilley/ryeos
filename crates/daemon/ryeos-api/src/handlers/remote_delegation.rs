//! Remote delegation services.
//!
//! On the source node a local principal signs a bounded delegation for one
//! remote (`remote/delegate`), lists the delegations stored for it
//! (`remote/delegation-list`) and revokes one (`remote/delegation-revoke`). On
//! the receiving node `identity/revoke-delegation` records a tombstone for one
//! delegation digest, which the node's request verification then refuses.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::handler_context::HandlerContext;
use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;
use crate::remote::client::{RemoteClient, delegator_fingerprint};
use crate::remote::config;
use ryeos_app::handler_error::delegation_handler_error;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_node_runtime::remote_delegation::{
    self as delegation_lib, DelegationError, DelegationParties, MAX_DELEGATION_LIFETIME_SECS,
    SignedDelegation,
};

fn default_remote() -> String {
    "default".to_string()
}

fn delegations_dir(state: &AppState) -> std::path::PathBuf {
    ryeos_engine::roots::RuntimeRoot::new(state.config.app_root.clone()).delegations_dir()
}

fn caller_fingerprint(ctx: &HandlerContext) -> HandlerResult<String> {
    ctx.require_verified()?;
    delegator_fingerprint(ctx).map_err(|error| delegation_handler_error(&error))
}

fn load_remote(state: &AppState, name: &str) -> HandlerResult<config::RemoteConfig> {
    let remotes = config::load_remotes(&state.config.app_root).map_err(|error| {
        config::typed_load_error_or(error, |error| {
            HandlerError::Internal(format!("load remotes: {error:#}"))
        })
    })?;
    config::get_remote(&remotes, name)
        .map_err(|error| HandlerError::BadRequest(format!("remote '{name}': {error:#}")))
}

// ---------------------------------------------------------------------------
// remote/delegate
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegateRequest {
    #[serde(default = "default_remote")]
    pub remote: String,
    /// The delegation, signed by the caller with the caller's own key. Absent:
    /// the response carries the facts the caller needs to build one.
    #[serde(default)]
    pub delegation: Option<SignedDelegation>,
}

pub async fn delegate(
    req: DelegateRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    let fingerprint = caller_fingerprint(&ctx)?;
    let remote = load_remote(&state, &req.remote)?;
    // A delegation's audience is a remote this node dials; an inbound-only pin
    // has no outbound path to delegate for.
    remote.outbound_url()?;
    let own_site = state.threads.site_id().to_owned();
    let own_node = state.identity.principal_id();
    let Some(signed) = req.delegation else {
        return Ok(json!({
            "remote": remote.name,
            "template": {
                "delegator": format!("fp:{fingerprint}"),
                "delegate_site": own_site,
                "delegate_node": own_node,
                "audience_site": remote.site_id,
                "audience_node": remote.principal_id,
            },
            "max_lifetime_secs": MAX_DELEGATION_LIFETIME_SECS,
        }));
    };

    if signed.body.delegator != format!("fp:{fingerprint}") {
        return Err(HandlerError::Forbidden(
            "a delegation is accepted only from the principal that signed it".into(),
        ));
    }
    // The signature must verify under the caller's own key as this node
    // admitted it.
    let grant = ryeos_node_runtime::identity::load_verified_authorized_key(
        &fingerprint,
        &state.config.authorized_keys_dir,
        &state.identity,
    )
    .map_err(|error| HandlerError::Internal(format!("{error:#}")))?
    .ok_or_else(|| HandlerError::Forbidden("the caller's grant is not on this node".into()))?;
    signed
        .verify(&grant.public_key)
        .map_err(|error| delegation_handler_error(&error))?;
    let now = lillux::time::unix_secs_now();
    signed
        .validate_at(
            now,
            DelegationParties {
                delegate_site: &own_site,
                delegate_node: &own_node,
                audience_site: &remote.site_id,
                audience_node: &remote.principal_id,
            },
        )
        .map_err(|error| delegation_handler_error(&error))?;
    if signed.body.not_after > now.saturating_add(MAX_DELEGATION_LIFETIME_SECS) {
        return Err(HandlerError::BadRequest(format!(
            "a delegation may last at most {MAX_DELEGATION_LIFETIME_SECS} seconds; sign one with \
             an earlier not_after"
        )));
    }
    ryeos_node_runtime::identity::reject_offline_scopes(&signed.body.scopes)
        .map_err(|error| HandlerError::BadRequest(format!("{error:#}")))?;

    let digest = delegation_lib::store_delegation(
        &delegations_dir(&state),
        &remote.name,
        &signed,
        &state.identity,
        now,
    )
    .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
    Ok(json!({
        "remote": remote.name,
        "delegation_digest": digest,
        "delegator": signed.body.delegator,
        "scopes": signed.body.scopes,
        "not_after": signed.body.not_after,
    }))
}

pub const DELEGATE: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/delegate",
    endpoint: "remote.delegate",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/delegate"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: DelegateRequest = crate::handler_error::parse_request(params)?;
            delegate(req, ctx, state).await.map_err(Into::into)
        })
    },
};

// ---------------------------------------------------------------------------
// remote/delegation-list
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListRequest {}

pub async fn list(
    _req: ListRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    let fingerprint = caller_fingerprint(&ctx)?;
    let delegations =
        delegation_lib::list_delegations(&delegations_dir(&state), &fingerprint, &state.identity)
            .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
    Ok(json!({
        "delegator": format!("fp:{fingerprint}"),
        "now": lillux::time::unix_secs_now(),
        "delegations": delegations,
    }))
}

pub const LIST: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/delegation-list",
    endpoint: "remote.delegation-list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/delegation-list"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ListRequest = crate::handler_error::parse_request(params)?;
            list(req, ctx, state).await.map_err(Into::into)
        })
    },
};

// ---------------------------------------------------------------------------
// remote/delegation-revoke
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeRequest {
    #[serde(default = "default_remote")]
    pub remote: String,
    /// Delete only the local record and leave the remote's ledger alone.
    #[serde(default)]
    pub local_only: bool,
}

pub async fn revoke(
    req: RevokeRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    let fingerprint = caller_fingerprint(&ctx)?;
    let remote = load_remote(&state, &req.remote)?;
    let delegation = delegation_lib::load_delegation(
        &delegations_dir(&state),
        &fingerprint,
        &remote.name,
        &state.identity,
    )
    .map_err(|error| HandlerError::Internal(format!("{error:#}")))?
    .ok_or_else(|| {
        delegation_handler_error(&DelegationError::Missing {
            delegator: format!("fp:{fingerprint}"),
            remote: remote.name.clone(),
        })
    })?;
    let digest = delegation
        .digest()
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;

    // A delegation that has already lapsed cannot be used, so the remote needs
    // no tombstone for it, and presenting it would be refused anyway.
    let lapsed = lillux::time::unix_secs_now() > delegation.body.not_after;
    let mut remote_tombstoned = false;
    if !req.local_only && !lapsed {
        let client = RemoteClient::from_remote_cfg_as_delegated(&state, &remote, &ctx)
            .map_err(crate::handlers::remote_push::outbound_refusal)?;
        client
            .revoke_delegation(&delegation.body.delegator, &digest)
            .await
            .map_err(|error| {
                crate::remote::client::map_remote_call_error(
                    error,
                    format!(
                        "the remote did not record the revocation; the local delegation was \
                         kept. If the delegation does not lend `ryeos.execute.service.identity/revoke-delegation`, \
                         ask the remote operator to run `ryeos delegation tombstone --delegator {} --delegation-digest {digest}`, \
                         or revoke locally only with `--local-only`",
                        delegation.body.delegator
                    ),
                )
            })?;
        remote_tombstoned = true;
    }
    let removed =
        delegation_lib::remove_delegation(&delegations_dir(&state), &fingerprint, &remote.name)
            .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
    Ok(json!({
        "remote": remote.name,
        "delegation_digest": digest,
        "remote_tombstoned": remote_tombstoned,
        "already_expired": lapsed,
        "removed_local": removed,
    }))
}

pub const REVOKE: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/delegation-revoke",
    endpoint: "remote.delegation-revoke",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/delegation-revoke"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: RevokeRequest = crate::handler_error::parse_request(params)?;
            revoke(req, ctx, state).await.map_err(Into::into)
        })
    },
};

// ---------------------------------------------------------------------------
// identity/revoke-delegation (the receiving node's ledger)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TombstoneRequest {
    /// `fp:<fingerprint>` of the principal that signed the delegation.
    pub delegator: String,
    /// Digest of the delegation to revoke.
    pub delegation_digest: String,
}

pub async fn tombstone(
    req: TombstoneRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    ctx.require_verified()?;
    if !lillux::valid_hash(&req.delegation_digest) {
        return Err(HandlerError::BadRequest(
            "delegation_digest must be a lowercase SHA-256 digest".into(),
        ));
    }
    let is_delegator = ctx.fingerprint == req.delegator;
    let is_operator =
        ryeos_app::operator_authority::require_local_configured_operator(&state, &ctx).is_ok();
    if !is_delegator && !is_operator {
        return Err(HandlerError::Forbidden(
            "only the principal that signed a delegation, or this node's operator, may revoke it"
                .into(),
        ));
    }
    let revocations = ryeos_engine::roots::RuntimeRoot::new(state.config.app_root.clone())
        .delegation_revocations_dir();
    let tombstone = delegation_lib::write_tombstone(
        &revocations,
        &req.delegation_digest,
        &req.delegator,
        &ctx.fingerprint,
        ctx.delegated_via.clone(),
        ctx.signed_request_digest.clone(),
        &state.identity,
        lillux::time::unix_secs_now(),
    )
    .map_err(|error| HandlerError::BadRequest(format!("{error:#}")))?;
    Ok(json!({
        "delegation_digest": tombstone.delegation_digest,
        "delegator": tombstone.delegator,
        "revoked_at": tombstone.revoked_at,
        "revoked_by": tombstone.revoked_by,
        "delegated_via": tombstone.delegated_via,
    }))
}

pub const TOMBSTONE: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:identity/revoke-delegation",
    endpoint: "identity.revoke-delegation",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.identity/revoke-delegation"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: TombstoneRequest = crate::handler_error::parse_request(params)?;
            tombstone(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn pinned_remote(inbound_only: bool) -> config::RemoteConfig {
        let key = lillux::crypto::SigningKey::from_bytes(&[91u8; 32]);
        config::RemoteConfig {
            name: "nat".to_owned(),
            url: (!inbound_only).then(|| "https://nat.example".to_owned()),
            reachability: if inbound_only {
                config::Reachability::InboundOnly
            } else {
                config::Reachability::Outbound
            },
            principal_id: format!(
                "fp:{}",
                lillux::signature::compute_fingerprint(&key.verifying_key())
            ),
            signing_key: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes())
            ),
            site_id: "site:nat".to_owned(),
            vault_fingerprint: "vault".to_owned(),
            ingest_ignore: None,
            project_bindings: Default::default(),
        }
    }

    fn caller() -> HandlerContext {
        HandlerContext::new_with_authority(
            format!("fp:{}", "ab".repeat(32)),
            vec!["*".to_owned()],
            true,
            Some(ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass::LocalClient),
            None,
        )
    }

    #[tokio::test]
    async fn delegating_for_an_inbound_only_pin_is_refused() {
        let (_tmp, _store, state) = crate::test_support::build_test_state();
        let node = state.identity.signing_key().clone();
        config::write_remote_pin(
            &state.config.app_root,
            &node,
            &pinned_remote(true),
            config::PinSource::DescriptorWithoutContact,
            None,
        )
        .unwrap();
        let state = Arc::new(state);
        let error = delegate(
            DelegateRequest {
                remote: "nat".to_owned(),
                delegation: None,
            },
            caller(),
            state,
        )
        .await
        .expect_err("an inbound-only pin has no outbound path to delegate for");
        match error {
            HandlerError::Structured { code, .. } => {
                assert_eq!(code, config::REMOTE_INBOUND_ONLY)
            }
            other => panic!("unexpected refusal {other:?}"),
        }
    }

    #[tokio::test]
    async fn delegating_for_an_outbound_pin_returns_the_signing_facts() {
        let (_tmp, _store, state) = crate::test_support::build_test_state();
        let node = state.identity.signing_key().clone();
        config::write_remote_pin(
            &state.config.app_root,
            &node,
            &pinned_remote(false),
            config::PinSource::DescriptorWithoutContact,
            None,
        )
        .unwrap();
        let answer = delegate(
            DelegateRequest {
                remote: "nat".to_owned(),
                delegation: None,
            },
            caller(),
            Arc::new(state),
        )
        .await
        .unwrap();
        assert_eq!(answer["template"]["audience_site"], "site:nat");
    }
}
