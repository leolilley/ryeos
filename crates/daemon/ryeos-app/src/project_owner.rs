//! The node's answer to "whose project namespace is this key acting in".
//!
//! Thin wrappers over the engine trust store's ownership resolver, which
//! reads only the node's own signed signer registry and its key-event chain.
//! Every site that needs a project owner from an authenticated key, or
//! checks that an acting key may act for a sealed owner, comes through here
//! (or straight to the trust store's `owner_for`/`speaks_for`). Nothing here
//! reads a request field, a project tree, a remote result or a bundle.
//!
//! A principal decides project ownership only. Grants, scopes, sessions and
//! thread reads stay per key.

use anyhow::{Result, anyhow};
use ryeos_engine::trust::{ResolvedOwner, TrustStore};
use ryeos_state::head_advance::HeadAdvancer;
use ryeos_state::principal_id::PrincipalId;

/// The owner `key` acts for on this node, from `trust` (which must be the
/// node registry store). A retired or revoked key is refused with
/// `owner_key_not_active`.
pub fn owner_for_in(trust: &TrustStore, key: &str) -> Result<ResolvedOwner> {
    trust.owner_for(key).map_err(|refusal| anyhow!("{refusal}"))
}

/// [`owner_for_in`] against the running node's registry.
pub fn owner_for(state: &crate::state::AppState, key: &str) -> Result<ResolvedOwner> {
    owner_for_in(&state.engine.trust_store, key)
}

/// The owning principal only.
pub fn owner_principal(state: &crate::state::AppState, key: &str) -> Result<PrincipalId> {
    owner_for(state, key).map(|owner| owner.principal)
}

/// Whether `key` may act for `owner` on this node, checked fresh against the
/// node registry: an advance of `owner`'s head, a resume, fold-back, session
/// reuse or UI revalidation of a run sealed with `owner`'s project.
pub fn speaks_for_in(trust: &TrustStore, key: &str, owner: &PrincipalId) -> bool {
    trust.speaks_for(key, owner)
}

/// [`speaks_for_in`] against the running node's registry.
pub fn speaks_for(state: &crate::state::AppState, key: &str, owner: &PrincipalId) -> bool {
    speaks_for_in(&state.engine.trust_store, key, owner)
}

/// Refuse unless `key` speaks for `owner`, naming both.
pub fn require_speaks_for_in(trust: &TrustStore, key: &str, owner: &PrincipalId) -> Result<()> {
    if speaks_for_in(trust, key, owner) {
        return Ok(());
    }
    let actual = match trust.owner_for(key) {
        Ok(resolved) => resolved.principal.to_string(),
        Err(refusal) => refusal.to_string(),
    };
    Err(anyhow!(
        "owner_not_spoken_for: key {key} does not act for project owner {owner} on this node \
         (it resolves to {actual})"
    ))
}

/// [`require_speaks_for_in`] against the running node's registry.
pub fn require_speaks_for(
    state: &crate::state::AppState,
    key: &str,
    owner: &PrincipalId,
) -> Result<()> {
    require_speaks_for_in(&state.engine.trust_store, key, owner)
}

/// The record of `key` advancing a head owned by `head_owner`, after
/// checking on this node that `key` acts for that owner. This is the only
/// way a head advance site obtains a [`HeadAdvancer`].
pub fn head_advancer_in(
    trust: &TrustStore,
    key: &str,
    head_owner: &PrincipalId,
) -> Result<HeadAdvancer> {
    let resolved = owner_for_in(trust, key)?;
    if &resolved.principal != head_owner {
        return Err(anyhow!(
            "owner_not_spoken_for: key {key} does not act for project owner {head_owner} on \
             this node (it resolves to {})",
            resolved.principal
        ));
    }
    let hex = key.strip_prefix("fp:").unwrap_or(key);
    Ok(HeadAdvancer {
        key: format!("fp:{hex}"),
        via_root: match resolved.via {
            ryeos_engine::trust::OwnerVia::Solo => None,
            ryeos_engine::trust::OwnerVia::Lineage { root } => Some(root),
        },
    })
}

/// [`head_advancer_in`] against the running node's registry.
pub fn head_advancer(
    state: &crate::state::AppState,
    key: &str,
    head_owner: &PrincipalId,
) -> Result<HeadAdvancer> {
    head_advancer_in(&state.engine.trust_store, key, head_owner)
}
