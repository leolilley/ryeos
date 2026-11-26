//! Linking a key to a principal's owner, and withdrawing a link: the effect
//! of the `node.key.link` and `node.key.unlink` offline operations.
//!
//! The record and its resolver live in `ryeos_engine::trust` (`key_link`).
//! This module is the writer's rules: who may be linked to whom, checked
//! against the node's own signer registry before the node pins the pair.

use std::path::Path;

use anyhow::{Result, bail, ensure};
use ryeos_engine::trust::{
    KeyLinkLedger, OwnerVia, SignedKeyLink, SignerRole, SignerStatus, StoredKeyLink, TrustStore,
    key_links_dir, revoke_key_link, write_key_link,
};
use ryeos_node_runtime::identity::NodeIdentity;
use serde::Serialize;

/// How far a link's `created_at` may differ from the node's clock, in
/// seconds. A link is made now, so it is neither far in the past nor future.
pub const MAX_LINK_CLOCK_SKEW_SECS: u64 = 600;

/// What linking reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LinkOutcome {
    pub id: String,
    pub principal: String,
    pub key: String,
    pub linked_by: String,
    pub not_after: String,
    /// False when the same link was already pinned.
    pub newly_linked: bool,
}

/// What unlinking reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnlinkOutcome {
    /// The ids tombstoned by this call.
    pub revoked: Vec<String>,
    /// Ids that already had a tombstone.
    pub already_revoked: Vec<String>,
}

/// Which links to withdraw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnlinkSelector {
    /// One link, by id.
    Id(String),
    /// Every link a key signed (`fp:<hex>`), for a compromised linking key.
    SignedBy(String),
}

/// Pin `link` if it passes the node's rules. `now_secs` is the node's clock.
///
/// The rules, each with its own code:
/// * both signatures verify and the shape is canonical (`key_link_malformed`,
///   `key_link_bad_signature`);
/// * the linking key acts for the principal on this node and is active
///   (`key_link_linker_not_owner`);
/// * the principal is not the node key's or the authoring key's, which own
///   nothing (`key_link_principal_is_node_key`, `key_link_principal_is_authoring_key`);
/// * the added key is not the node key or an authoring key
///   (`key_link_key_is_node_or_authoring`);
/// * the added key has no succession lineage and is not already joined to
///   another principal (`key_link_key_has_lineage`, `key_link_key_has_owner`);
/// * the link starts now and ends in the future (`key_link_not_current`).
pub fn link_key(
    app_root: &Path,
    node: &NodeIdentity,
    trust: &TrustStore,
    link: SignedKeyLink,
    now_secs: u64,
) -> Result<LinkOutcome> {
    link.verify()?;
    let body = &link.body;
    let principal = body.principal_id()?;
    let key = body.key_fingerprint()?.to_owned();
    let linked_by = body.linked_by_fingerprint()?.to_owned();

    // The linking key must be an active key of the principal.
    let owner = trust.owner_for(&linked_by).map_err(|refusal| {
        anyhow::anyhow!("key_link_linker_not_owner: the linking key owns nothing here: {refusal}")
    })?;
    ensure!(
        owner.principal == principal && !matches!(owner.via, OwnerVia::Linked { .. }),
        "key_link_linker_not_owner: {} does not act for {principal} through its own record, so it \
         cannot vouch for another key",
        body.linked_by
    );

    // Never the node's or the authoring key's principal.
    let node_owner = trust
        .owner_for(node.fingerprint())
        .map(|resolved| resolved.principal)
        .ok();
    ensure!(
        node_owner.as_ref() != Some(&principal),
        "key_link_principal_is_node_key: the node key signs on the node's behalf and a key \
         cannot be joined to it"
    );
    let authoring_owns = trust
        .records()
        .filter(|record| record.role == SignerRole::Authoring)
        .filter_map(|record| {
            trust.succession().and_then(|graph| {
                graph
                    .signing_principal_of(&record.fingerprint)
                    .ok()
                    .flatten()
            })
        })
        .any(|owner| owner == principal);
    ensure!(
        !authoring_owns,
        "key_link_principal_is_authoring_key: the authoring key signs runtime drafts and a key \
         cannot be joined to it"
    );

    // The added key is never the node key or an authoring-role key: a linked
    // one would speak for the operator.
    ensure!(
        key != node.fingerprint()
            && !trust
                .records()
                .any(|record| record.role == SignerRole::Authoring && record.fingerprint == key),
        "key_link_key_is_node_or_authoring: {} is the node key or an authoring key, which \
         cannot be joined to a principal",
        body.key
    );

    // The added key: no lineage, and no other owner.
    if let Some(graph) = trust.succession()
        && let Some(node_record) = graph.node(&key)
    {
        ensure!(
            node_record.status == SignerStatus::Active,
            "key_link_key_not_active: {} is {} on this node",
            body.key,
            node_record.status.as_str()
        );
        ensure!(
            node_record.predecessor.is_none() && graph.successors(&key).next().is_none(),
            "key_link_key_has_lineage: {} belongs to a key-succession lineage, and a key has \
             one owner",
            body.key
        );
    }
    let id = link.id()?;
    let already_pinned = KeyLinkLedger::load(app_root, node.verifying_key())
        .map_err(|error| anyhow::anyhow!("{error}"))?
        .links
        .iter()
        .any(|stored| stored.id == id);
    match trust.owner_for_at(&key, now_secs) {
        Ok(current) => {
            let joined_here = current.principal == principal
                && matches!(current.via, OwnerVia::Linked { .. } | OwnerVia::Solo);
            let own_solo = matches!(current.via, OwnerVia::Solo);
            ensure!(
                own_solo || joined_here || already_pinned,
                "key_link_key_has_owner: {} already acts for {}",
                body.key,
                current.principal
            );
        }
        Err(refusal) => bail!("key_link_key_has_owner: {refusal}"),
    }

    // The link is made now.
    let (created, not_after) = body.times()?;
    ensure!(
        created.abs_diff(now_secs) <= MAX_LINK_CLOCK_SKEW_SECS && not_after > now_secs,
        "key_link_not_current: a link is made now and must end in the future (created_at \
         {} is not within {MAX_LINK_CLOCK_SKEW_SECS} s of this node's clock, or not_after {} \
         has passed)",
        body.created_at,
        body.not_after
    );

    let outcome = LinkOutcome {
        id: id.clone(),
        principal: body.principal.clone(),
        key: body.key.clone(),
        linked_by: body.linked_by.clone(),
        not_after: body.not_after.clone(),
        newly_linked: !already_pinned,
    };
    let stored = StoredKeyLink::new(link, lillux::time::iso8601_from_unix_secs(now_secs))?;
    write_key_link(app_root, node.signing_key(), &stored)?;
    Ok(outcome)
}

/// Withdraw links by writing id-keyed tombstones. `revoked_by` is
/// `fp:<operator key>`. Idempotent: a link already tombstoned is reported,
/// not rewritten.
pub fn unlink_keys(
    app_root: &Path,
    node: &NodeIdentity,
    selector: &UnlinkSelector,
    revoked_by: &str,
    reason: &str,
    now_secs: u64,
) -> Result<UnlinkOutcome> {
    let ledger = KeyLinkLedger::load(app_root, node.verifying_key())
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let ids: Vec<String> = match selector {
        UnlinkSelector::Id(id) => {
            ensure!(
                ledger.links.iter().any(|stored| &stored.id == id),
                "key_link_unknown: no pinned link has id {id} ({})",
                key_links_dir(app_root).display()
            );
            vec![id.clone()]
        }
        UnlinkSelector::SignedBy(signer) => {
            let ids: Vec<String> = ledger
                .links
                .iter()
                .filter(|stored| &stored.link.body.linked_by == signer)
                .map(|stored| stored.id.clone())
                .collect();
            ensure!(
                !ids.is_empty(),
                "key_link_unknown: no pinned link was signed by {signer}"
            );
            ids
        }
    };
    let now = lillux::time::iso8601_from_unix_secs(now_secs);
    let mut outcome = UnlinkOutcome {
        revoked: Vec::new(),
        already_revoked: Vec::new(),
    };
    for id in ids {
        if ledger.is_revoked(&id) {
            outcome.already_revoked.push(id);
            continue;
        }
        revoke_key_link(app_root, node.signing_key(), &id, revoked_by, reason, &now)?;
        outcome.revoked.push(id);
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use lillux::crypto::SigningKey;
    use ryeos_engine::trust::{
        KeyLinkBody, SignerRecord, SignerRole, SignerScope, SignerStatus, TrustEvent,
        TrustEventKind,
    };

    const NOW: u64 = 1_800_000_000;

    fn key(byte: u8) -> SigningKey {
        SigningKey::from_bytes(&[byte; 32])
    }

    fn fp(key: &SigningKey) -> String {
        format!(
            "fp:{}",
            lillux::signature::compute_fingerprint(&key.verifying_key())
        )
    }

    struct Rig {
        app_root: tempfile::TempDir,
        node: NodeIdentity,
    }

    fn rig() -> Rig {
        let app_root = tempfile::TempDir::new().unwrap();
        let node = NodeIdentity::create(&app_root.path().join("node.key")).unwrap();
        Rig { app_root, node }
    }

    fn operator_record(key: &SigningKey, status: SignerStatus) -> SignerRecord {
        SignerRecord::new(
            &key.verifying_key(),
            "operator",
            SignerRole::Operator,
            SignerScope::operator_default(),
            status,
        )
    }

    fn store(rig: &Rig, records: Vec<SignerRecord>, chain: &[TrustEvent]) -> TrustStore {
        TrustStore::from_registry(*rig.node.verifying_key(), records)
            .unwrap()
            .with_succession_chain(chain)
    }

    fn link_body(
        trust: &TrustStore,
        by: &SigningKey,
        added: &SigningKey,
        created: u64,
        lifetime: u64,
        nonce: u8,
    ) -> KeyLinkBody {
        let encode = |key: &SigningKey| {
            base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes())
        };
        KeyLinkBody {
            schema: ryeos_engine::trust::KEY_LINK_SCHEMA.to_owned(),
            nonce: format!("{nonce:02x}").repeat(16),
            principal: trust.owner_for(&fp(by)).unwrap().principal.to_string(),
            key: fp(added),
            key_public: encode(added),
            linked_by: fp(by),
            linked_by_public: encode(by),
            created_at: lillux::time::iso8601_from_unix_secs(created),
            not_after: lillux::time::iso8601_from_unix_secs(created + lifetime),
            request_digest: "ee".repeat(32),
        }
    }

    fn signed(
        trust: &TrustStore,
        by: &SigningKey,
        added: &SigningKey,
        lifetime: u64,
        nonce: u8,
    ) -> SignedKeyLink {
        SignedKeyLink::sign(link_body(trust, by, added, NOW, lifetime, nonce), by, added).unwrap()
    }

    #[test]
    fn the_operator_links_another_machines_key_and_the_node_then_resolves_it_to_the_operator() {
        let rig = rig();
        let (operator, laptop) = (key(1), key(2));
        let trust = store(
            &rig,
            vec![operator_record(&operator, SignerStatus::Active)],
            &[],
        );
        let outcome = link_key(
            rig.app_root.path(),
            &rig.node,
            &trust,
            signed(&trust, &operator, &laptop, 86_400, 1),
            NOW,
        )
        .unwrap();
        assert!(outcome.newly_linked);

        // The next start reads the ledger: the laptop key now speaks for the
        // operator's owner, and only through the link.
        let links = KeyLinkLedger::load(rig.app_root.path(), rig.node.verifying_key())
            .unwrap()
            .entries()
            .unwrap();
        let after = store(
            &rig,
            vec![operator_record(&operator, SignerStatus::Active)],
            &[],
        )
        .with_key_links(links);
        let owner = after.owner_for_at(&fp(&laptop), NOW + 10).unwrap();
        assert_eq!(
            owner.principal,
            after.owner_for(&fp(&operator)).unwrap().principal
        );
        assert!(matches!(owner.via, OwnerVia::Linked { .. }));
        // Pinning the same link again changes nothing.
        let again = link_key(
            rig.app_root.path(),
            &rig.node,
            &after,
            signed(&after, &operator, &laptop, 86_400, 1),
            NOW,
        )
        .unwrap();
        assert!(!again.newly_linked);
    }

    #[test]
    fn a_key_that_does_not_act_for_the_principal_cannot_vouch_for_another() {
        let rig = rig();
        let (operator, stranger, laptop) = (key(1), key(5), key(2));
        let trust = store(
            &rig,
            vec![operator_record(&operator, SignerStatus::Active)],
            &[],
        );
        // The stranger signs a link into the operator's principal.
        let mut body = link_body(&trust, &stranger, &laptop, NOW, 86_400, 2);
        body.principal = trust
            .owner_for(&fp(&operator))
            .unwrap()
            .principal
            .to_string();
        let link = SignedKeyLink::sign(body, &stranger, &laptop).unwrap();
        let error = link_key(rig.app_root.path(), &rig.node, &trust, link, NOW).unwrap_err();
        assert!(
            format!("{error:#}").contains("key_link_linker_not_owner"),
            "{error:#}"
        );
    }

    #[test]
    fn a_retired_linking_key_cannot_vouch_and_a_key_with_a_lineage_cannot_be_linked() {
        let rig = rig();
        let (old, successor, laptop) = (key(1), key(2), key(3));
        let records = vec![
            operator_record(&old, SignerStatus::Retired),
            operator_record(&successor, SignerStatus::Active),
        ];
        let chain = [TrustEvent {
            schema: "x".into(),
            seq: 1,
            previous: None,
            kind: TrustEventKind::Succeeded,
            signer: lillux::signature::compute_fingerprint(&successor.verifying_key()),
            record_digest: String::new(),
            request_digest: String::new(),
            operator: String::new(),
            predecessor: Some(lillux::signature::compute_fingerprint(&old.verifying_key())),
        }];
        let trust = store(&rig, records, &chain);
        // The retired key owns nothing, so it cannot vouch.
        let mut body = link_body(&trust, &successor, &laptop, NOW, 86_400, 3);
        body.linked_by = fp(&old);
        body.linked_by_public =
            base64::engine::general_purpose::STANDARD.encode(old.verifying_key().to_bytes());
        let link = SignedKeyLink::sign(body, &old, &laptop).unwrap();
        assert!(link_key(rig.app_root.path(), &rig.node, &trust, link, NOW).is_err());

        // The successor has a lineage: it cannot itself be the added key.
        let other = key(7);
        let trust2 = store(
            &rig,
            vec![
                operator_record(&old, SignerStatus::Retired),
                operator_record(&successor, SignerStatus::Active),
                operator_record(&other, SignerStatus::Active),
            ],
            &chain,
        );
        let link = signed(&trust2, &other, &successor, 86_400, 4);
        let error = link_key(rig.app_root.path(), &rig.node, &trust2, link, NOW).unwrap_err();
        assert!(
            format!("{error:#}").contains("key_link_key_has_lineage"),
            "{error:#}"
        );
    }

    #[test]
    fn a_key_joined_to_one_principal_is_not_joined_to_another() {
        let rig = rig();
        let (first, second, laptop) = (key(1), key(4), key(2));
        let records = vec![
            operator_record(&first, SignerStatus::Active),
            operator_record(&second, SignerStatus::Active),
        ];
        let trust = store(&rig, records.clone(), &[]);
        link_key(
            rig.app_root.path(),
            &rig.node,
            &trust,
            signed(&trust, &first, &laptop, 86_400, 5),
            NOW,
        )
        .unwrap();
        let links = KeyLinkLedger::load(rig.app_root.path(), rig.node.verifying_key())
            .unwrap()
            .entries()
            .unwrap();
        let trust = store(&rig, records, &[]).with_key_links(links);
        let error = link_key(
            rig.app_root.path(),
            &rig.node,
            &trust,
            signed(&trust, &second, &laptop, 86_400, 6),
            NOW,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("key_link_key_has_owner"),
            "{error:#}"
        );
    }

    #[test]
    fn a_link_that_is_not_current_is_refused() {
        let rig = rig();
        let (operator, laptop) = (key(1), key(2));
        let trust = store(
            &rig,
            vec![operator_record(&operator, SignerStatus::Active)],
            &[],
        );
        // Made a day ago and already over; and made next year.
        for (created, lifetime, nonce) in
            [(NOW - 200_000, 86_400, 7), (NOW + 40_000_000, 86_400, 8)]
        {
            let link = SignedKeyLink::sign(
                link_body(&trust, &operator, &laptop, created, lifetime, nonce),
                &operator,
                &laptop,
            )
            .unwrap();
            let error = link_key(rig.app_root.path(), &rig.node, &trust, link, NOW).unwrap_err();
            assert!(
                format!("{error:#}").contains("key_link_not_current"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn the_node_key_and_an_authoring_key_cannot_be_the_added_key() {
        let rig = rig();
        let (operator, authoring) = (key(1), key(7));
        let trust = store(
            &rig,
            vec![
                operator_record(&operator, SignerStatus::Active),
                SignerRecord::new(
                    &authoring.verifying_key(),
                    "authoring",
                    SignerRole::Authoring,
                    SignerScope::operator_default(),
                    SignerStatus::Active,
                ),
            ],
            &[],
        );
        for (added, nonce) in [(rig.node.signing_key(), 11), (&authoring, 12)] {
            let link = signed(&trust, &operator, added, 86_400, nonce);
            let error = link_key(rig.app_root.path(), &rig.node, &trust, link, NOW).unwrap_err();
            assert!(
                format!("{error:#}").contains("key_link_key_is_node_or_authoring"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn unlinking_by_signer_withdraws_every_link_that_key_signed() {
        let rig = rig();
        let (operator, laptop, phone) = (key(1), key(2), key(6));
        let trust = store(
            &rig,
            vec![operator_record(&operator, SignerStatus::Active)],
            &[],
        );
        for (added, nonce) in [(&laptop, 9), (&phone, 10)] {
            link_key(
                rig.app_root.path(),
                &rig.node,
                &trust,
                signed(&trust, &operator, added, 86_400, nonce),
                NOW,
            )
            .unwrap();
        }
        let outcome = unlink_keys(
            rig.app_root.path(),
            &rig.node,
            &UnlinkSelector::SignedBy(fp(&operator)),
            &fp(&operator),
            "operator key compromised",
            NOW + 5,
        )
        .unwrap();
        assert_eq!(outcome.revoked.len(), 2);
        let ledger = KeyLinkLedger::load(rig.app_root.path(), rig.node.verifying_key()).unwrap();
        assert!(ledger.entries().unwrap().is_empty());

        // Again: nothing newly revoked.
        let outcome = unlink_keys(
            rig.app_root.path(),
            &rig.node,
            &UnlinkSelector::SignedBy(fp(&operator)),
            &fp(&operator),
            "again",
            NOW + 6,
        )
        .unwrap();
        assert!(outcome.revoked.is_empty());
        assert_eq!(outcome.already_revoked.len(), 2);
    }
}
