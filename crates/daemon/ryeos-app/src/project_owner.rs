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

/// The owner a root admission stamps into its `thread_created` event: the
/// principal `requested_by` acts for, from `trust`. `Ok(None)` only when the
/// admission names no acting key (a node-started root), which stays unstamped
/// until its source (a schedule's registrant, a recovered run) supplies an
/// owner; the stamping build refuses an unstamped birth outright (rulings
/// 1.59), so that is the interim, not the end. An acting key whose owner
/// cannot be resolved (a store without the registry, a corrupt chain, a
/// retired or revoked key) refuses the admission with the resolver's own
/// refusal: a root is never admitted unstamped because the lookup failed.
pub fn admitting_owner_in(
    trust: &TrustStore,
    requested_by: Option<&str>,
) -> Result<Option<PrincipalId>> {
    let Some(key) = requested_by else {
        return Ok(None);
    };
    owner_for_in(trust, key).map(|owner| Some(owner.principal))
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

/// Whether `key` may act for the owner of `project` on this node: the owner
/// the project's own identity names, judged by the node registry as
/// [`speaks_for`] judges it. A reader that filters rows by project ownership
/// asks this and never compares an owner with a key itself.
pub fn key_owns_project(
    state: &crate::state::AppState,
    key: &str,
    project: &ryeos_state::project_identity::ProjectIdentity,
) -> bool {
    speaks_for(state, key, project.owner())
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
            // A linked key advanced the head for the principal it was joined to;
            // the link id is evidence, not a root key.
            ryeos_engine::trust::OwnerVia::Linked { .. } => None,
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

/// Refuse unless the owner this node resolves for `key` is exactly
/// `expected` (when a caller asserted one). A remote client sends the owner
/// it recorded at bind; this compares, before anything is read or advanced,
/// and never uses the assertion as the owner or to select one.
pub fn require_expected_owner_in(
    trust: &TrustStore,
    key: &str,
    expected: Option<&PrincipalId>,
) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let resolved = owner_for_in(trust, key)?;
    if &resolved.principal != expected {
        return Err(anyhow!(
            "remote_owner_changed: this node resolves key {key} to project owner {} but the \
             caller expected {expected}; the key was rotated, re-delegated or re-pinned since \
             the project was bound. Re-bind it with `ryeos remote bind-project` after checking \
             why",
            resolved.principal
        ));
    }
    Ok(())
}

/// [`require_expected_owner_in`] against the running node's registry.
pub fn require_expected_owner(
    state: &crate::state::AppState,
    key: &str,
    expected: Option<&PrincipalId>,
) -> Result<()> {
    require_expected_owner_in(&state.engine.trust_store, key, expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;
    use ryeos_engine::trust::{
        SignerRecord, SignerRole, SignerScope, SignerStatus, TrustEvent, TrustEventKind,
        compute_fingerprint,
    };

    fn record(key: &SigningKey, status: SignerStatus) -> SignerRecord {
        SignerRecord::new(
            &key.verifying_key(),
            "owner-test",
            SignerRole::ProjectAuthor,
            SignerScope::authoring_default(),
            status,
        )
    }

    fn solo(fingerprint: &str) -> PrincipalId {
        ryeos_engine::trust::SuccessionGraph::default()
            .ownership_of(fingerprint)
            .unwrap()
            .principal
    }

    #[test]
    fn expected_owner_is_compared_with_the_resolver_never_adopted() {
        let node = SigningKey::from_bytes(&[1u8; 32]).verifying_key();
        let root = SigningKey::from_bytes(&[2u8; 32]);
        let successor = SigningKey::from_bytes(&[3u8; 32]);
        let root_fp = compute_fingerprint(&root.verifying_key());
        let successor_fp = compute_fingerprint(&successor.verifying_key());
        let succeeded = TrustEvent {
            schema: "x".into(),
            seq: 1,
            previous: None,
            kind: TrustEventKind::Succeeded,
            signer: successor_fp.clone(),
            record_digest: String::new(),
            request_digest: String::new(),
            operator: String::new(),
            predecessor: Some(root_fp.clone()),
        };
        let trust = TrustStore::from_registry(
            node,
            vec![
                record(&root, SignerStatus::Retired),
                record(&successor, SignerStatus::Active),
            ],
        )
        .unwrap()
        .with_succession_chain(std::slice::from_ref(&succeeded));

        // No assertion: nothing is compared.
        require_expected_owner_in(&trust, &successor_fp, None).unwrap();
        // The rotated successor still acts for the root's principal.
        require_expected_owner_in(&trust, &successor_fp, Some(&solo(&root_fp))).unwrap();
        // The successor's own solo principal is not what it resolves to.
        let refused = require_expected_owner_in(&trust, &successor_fp, Some(&solo(&successor_fp)))
            .unwrap_err();
        assert!(
            format!("{refused:#}").starts_with("remote_owner_changed"),
            "{refused:#}"
        );
    }

    #[test]
    fn a_root_admission_takes_the_owner_the_acting_key_resolves_to() {
        let node = SigningKey::from_bytes(&[1u8; 32]).verifying_key();
        let root = SigningKey::from_bytes(&[2u8; 32]);
        let successor = SigningKey::from_bytes(&[3u8; 32]);
        let root_fp = compute_fingerprint(&root.verifying_key());
        let successor_fp = compute_fingerprint(&successor.verifying_key());
        let succeeded = TrustEvent {
            schema: "x".into(),
            seq: 1,
            previous: None,
            kind: TrustEventKind::Succeeded,
            signer: successor_fp.clone(),
            record_digest: String::new(),
            request_digest: String::new(),
            operator: String::new(),
            predecessor: Some(root_fp.clone()),
        };
        let trust = TrustStore::from_registry(
            node,
            vec![
                record(&root, SignerStatus::Retired),
                record(&successor, SignerStatus::Active),
            ],
        )
        .unwrap()
        .with_succession_chain(std::slice::from_ref(&succeeded));
        // The rotated successor stamps its chain root's principal, not its own.
        assert_eq!(
            admitting_owner_in(&trust, Some(&successor_fp)).unwrap(),
            Some(solo(&root_fp))
        );
        // No acting key: nothing to stamp. A key the registry has no record of
        // acts for its own solo principal, as the resolver says everywhere.
        assert_eq!(admitting_owner_in(&trust, None).unwrap(), None);
        let stranger = "f".repeat(64);
        assert_eq!(
            admitting_owner_in(&trust, Some(&stranger)).unwrap(),
            Some(solo(&stranger))
        );
        // A store that never loaded the registry cannot resolve an acting key:
        // the admission is refused with the resolver's refusal, never admitted
        // unstamped.
        let refused = admitting_owner_in(&TrustStore::empty(), Some(&successor_fp)).unwrap_err();
        assert_eq!(
            format!("{refused:#}"),
            TrustStore::empty()
                .owner_for(&successor_fp)
                .unwrap_err()
                .to_string()
        );
        // A retired key does not admit either (owner_key_not_active).
        let retired = admitting_owner_in(&trust, Some(&root_fp)).unwrap_err();
        assert!(
            format!("{retired:#}").contains("owner_key_not_active"),
            "{retired:#}"
        );
    }

    /// A store that never loaded the node's registry cannot say whose
    /// project a key acts in: it refuses, never falls back to a solo owner.
    #[test]
    fn a_store_without_the_node_registry_refuses_owner_resolution() {
        let node = SigningKey::from_bytes(&[1u8; 32]).verifying_key();
        let key = SigningKey::from_bytes(&[5u8; 32]);
        let fp = compute_fingerprint(&key.verifying_key());
        let owner = solo(&fp);
        for trust in [
            TrustStore::empty(),
            TrustStore::from_registry(node, vec![record(&key, SignerStatus::Active)]).unwrap(),
        ] {
            let refused = owner_for_in(&trust, &fp).unwrap_err();
            assert!(
                format!("{refused:#}").starts_with("owner_resolution_unavailable"),
                "{refused:#}"
            );
            let refused = head_advancer_in(&trust, &fp, &owner).unwrap_err();
            assert!(
                format!("{refused:#}").starts_with("owner_resolution_unavailable"),
                "{refused:#}"
            );
            assert!(!speaks_for_in(&trust, &fp, &owner));
        }
    }

    #[test]
    fn a_key_with_no_succession_matches_only_its_own_principal() {
        let node = SigningKey::from_bytes(&[1u8; 32]).verifying_key();
        let key = SigningKey::from_bytes(&[4u8; 32]);
        let fp = compute_fingerprint(&key.verifying_key());
        let trust = TrustStore::from_registry(node, vec![record(&key, SignerStatus::Active)])
            .unwrap()
            .with_succession_chain(&[]);
        require_expected_owner_in(&trust, &fp, Some(&solo(&fp))).unwrap();
        let other: PrincipalId = format!("pid:v1:{}", "a".repeat(64)).parse().unwrap();
        let refused = require_expected_owner_in(&trust, &fp, Some(&other)).unwrap_err();
        assert!(
            format!("{refused:#}").starts_with("remote_owner_changed"),
            "{refused:#}"
        );
    }
}
