//! Which principal a key speaks for on this node, decided only from the
//! node's own signed signer registry.
//!
//! Two parts:
//! - [`SuccessionGraph`]: the key-succession graph read from the node-signed
//!   records and key-event chain. It states facts (each key's role, status
//!   and the predecessor its `succeeded` event names) and decides nothing.
//!   Every consumer of key succession reads this one graph.
//! - The ownership policy ([`SuccessionGraph::ownership_of`]): a key's owner
//!   is the solo principal of the root of its backward succession chain.
//!   A key whose record is `retired` or `revoked` owns nothing and speaks
//!   for nothing. Revocation is per key: successors recorded before a
//!   revocation keep the principal, because a succession can only be
//!   recorded while the old key is not revoked, needs the operator's signed
//!   request, and the event log order is node-signed. A key with no record
//!   (a stranger, a remote pusher, a browser key, the node key) speaks for
//!   its own solo principal.
//!
//! Capacity reads the same graph with its own, different policy (a revoked
//! link cuts everything after it, because there the question is whether the
//! issuer's signatures verify). The policies are separate functions over
//! the one graph.
//!
//! Inputs are only the loaded node-signed records and event chain. Nothing
//! here takes a request field, a project tree, a remote result, a bundle or
//! an unsigned file. A principal standing never authorizes anything except
//! project ownership: grants, scopes, sessions and thread reads stay per
//! key.

use std::collections::BTreeMap;

use ryeos_state::principal_id::PrincipalId;
use serde::{Deserialize, Serialize};

use super::registry::{SignerRecord, SignerRole, SignerStatus, TrustEvent, TrustEventKind};

/// One key in the succession graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuccessionNode {
    pub fingerprint: String,
    pub role: SignerRole,
    pub status: SignerStatus,
    /// The key whose succession statement admitted this one, from its
    /// `succeeded` event. `None` for a key pinned any other way.
    pub predecessor: Option<String>,
}

/// The node's key-succession graph.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SuccessionGraph {
    nodes: BTreeMap<String, SuccessionNode>,
    /// Successor fingerprints per predecessor, in event-chain order.
    successors: BTreeMap<String, Vec<String>>,
}

/// How a key's owner was resolved. Evidence only: nothing decides on it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OwnerVia {
    /// The key is the root of its own principal.
    Solo,
    /// The key descends by recorded succession from `root`.
    Lineage { root: String },
}

/// A key's owning principal on this node.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedOwner {
    pub principal: PrincipalId,
    pub via: OwnerVia,
}

/// Why a key has no owner on this node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerRefusal {
    /// The key's record is retired or revoked: it no longer acts.
    KeyNotActive { status: SignerStatus },
    /// The value is not a key fingerprint.
    NotAFingerprint(String),
    /// The store has no node registry to resolve against (a store rebuilt
    /// from a launch envelope or built in memory).
    Unavailable,
    /// The succession chain loops through these keys. Valid operations
    /// cannot produce a cycle, so the registry is damaged.
    ChainCorrupt { keys: Vec<String> },
}

impl OwnerRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::KeyNotActive { .. } => "owner_key_not_active",
            Self::NotAFingerprint(_) => "owner_not_a_key_fingerprint",
            Self::Unavailable => "owner_resolution_unavailable",
            Self::ChainCorrupt { .. } => "owner_chain_corrupt",
        }
    }
}

impl std::fmt::Display for OwnerRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::KeyNotActive { status } => write!(
                formatter,
                "owner_key_not_active: the key is {} on this node and owns no project namespace",
                status.as_str()
            ),
            Self::NotAFingerprint(detail) => {
                write!(formatter, "owner_not_a_key_fingerprint: {detail}")
            }
            Self::Unavailable => formatter.write_str(
                "owner_resolution_unavailable: this trust store holds no node registry to resolve a project owner",
            ),
            Self::ChainCorrupt { keys } => write!(
                formatter,
                "owner_chain_corrupt: the key succession chain loops through {}; the signer \
                 registry is damaged; restore it from a backup of the node state",
                keys.join(" -> ")
            ),
        }
    }
}

impl std::error::Error for OwnerRefusal {}

impl SuccessionGraph {
    /// Build the graph from verified records and the verified event chain.
    /// A `succeeded` event names its predecessor; each key has at most one
    /// (the registry writer skips a key that already has a record). If the
    /// chain ever held two for one key, the first in chain order is kept.
    pub fn build(records: &[SignerRecord], chain: &[TrustEvent]) -> Self {
        let mut nodes: BTreeMap<String, SuccessionNode> = records
            .iter()
            .map(|record| {
                (
                    record.fingerprint.clone(),
                    SuccessionNode {
                        fingerprint: record.fingerprint.clone(),
                        role: record.role,
                        status: record.status,
                        predecessor: None,
                    },
                )
            })
            .collect();
        let mut successors: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for event in chain {
            if event.kind != TrustEventKind::Succeeded {
                continue;
            }
            let Some(predecessor) = event.predecessor.as_ref() else {
                continue;
            };
            let Some(node) = nodes.get_mut(&event.signer) else {
                continue;
            };
            if node.predecessor.is_some() || predecessor == &event.signer {
                continue;
            }
            node.predecessor = Some(predecessor.clone());
            successors
                .entry(predecessor.clone())
                .or_default()
                .push(event.signer.clone());
        }
        Self { nodes, successors }
    }

    /// The key's node, when the registry holds a record for it.
    pub fn node(&self, fingerprint: &str) -> Option<&SuccessionNode> {
        self.nodes.get(fingerprint)
    }

    /// The keys whose succession events name `fingerprint` as predecessor,
    /// in event-chain order.
    pub fn successors(&self, fingerprint: &str) -> impl Iterator<Item = &SuccessionNode> {
        self.successors
            .get(fingerprint)
            .into_iter()
            .flatten()
            .filter_map(|successor| self.nodes.get(successor))
    }

    /// The root of `fingerprint`'s backward chain: follow predecessors until
    /// a key with none (or one the registry holds no record for). A chain
    /// that loops is refused as `owner_chain_corrupt`, naming the keys.
    pub fn root_of(&self, fingerprint: &str) -> Result<String, OwnerRefusal> {
        let mut current = fingerprint.to_owned();
        let mut seen = vec![current.clone()];
        while let Some(predecessor) = self
            .nodes
            .get(&current)
            .and_then(|node| node.predecessor.clone())
        {
            if seen.contains(&predecessor) {
                seen.push(predecessor);
                return Err(OwnerRefusal::ChainCorrupt { keys: seen });
            }
            seen.push(predecessor.clone());
            current = predecessor;
        }
        Ok(current)
    }

    /// The ownership policy. `fingerprint` is `fp:<64 lowercase hex>`
    /// without the prefix (the registry's spelling).
    pub fn ownership_of(&self, fingerprint: &str) -> Result<ResolvedOwner, OwnerRefusal> {
        let solo = |key: &str| {
            PrincipalId::solo(&format!("fp:{key}"))
                .map_err(|error| OwnerRefusal::NotAFingerprint(format!("{error:#}")))
        };
        let Some(node) = self.nodes.get(fingerprint) else {
            return Ok(ResolvedOwner {
                principal: solo(fingerprint)?,
                via: OwnerVia::Solo,
            });
        };
        if node.status != SignerStatus::Active {
            return Err(OwnerRefusal::KeyNotActive {
                status: node.status,
            });
        }
        let root = self.root_of(fingerprint)?;
        let via = if root == fingerprint {
            OwnerVia::Solo
        } else {
            OwnerVia::Lineage { root: root.clone() }
        };
        Ok(ResolvedOwner {
            principal: solo(&root)?,
            via,
        })
    }

    /// The principal a signature by `fingerprint` speaks for when it is
    /// verified: the solo principal of its chain root, for an active or
    /// retired key (verification follows "retired verifies, revoked does
    /// not"); `Ok(None)` for a revoked key or one with no record. For judging
    /// signed items (`own:` scopes) and stored statements (endorsements),
    /// never for acting: acting goes through [`Self::ownership_of`].
    pub fn signing_principal_of(
        &self,
        fingerprint: &str,
    ) -> Result<Option<PrincipalId>, OwnerRefusal> {
        let Some(node) = self.nodes.get(fingerprint) else {
            return Ok(None);
        };
        if node.status == SignerStatus::Revoked {
            return Ok(None);
        }
        let root = self.root_of(fingerprint)?;
        PrincipalId::solo(&format!("fp:{root}"))
            .map(Some)
            .map_err(|error| OwnerRefusal::NotAFingerprint(format!("{error:#}")))
    }

    /// Keys whose succession chain is corrupt (loops), for doctor.
    pub fn corrupt_chains(&self) -> Vec<OwnerRefusal> {
        self.nodes
            .keys()
            .filter_map(|key| self.root_of(key).err())
            .collect()
    }

    /// Every principal some key in this registry roots, with its root key
    /// and the currently active keys that speak for it. For doctor and
    /// `trust list`, so an operator can read `pid:v1:` scopes.
    pub fn principals(&self) -> Vec<PrincipalSummary> {
        let mut by_root: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for node in self.nodes.values() {
            let Ok(root) = self.root_of(&node.fingerprint) else {
                continue;
            };
            let entry = by_root.entry(root).or_default();
            if node.status == SignerStatus::Active {
                entry.push(node.fingerprint.clone());
            }
        }
        by_root
            .into_iter()
            .filter_map(|(root, current_keys)| {
                Some(PrincipalSummary {
                    principal: PrincipalId::solo(&format!("fp:{root}")).ok()?,
                    root_key: root,
                    current_keys,
                })
            })
            .collect()
    }
}

/// One principal as the registry sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrincipalSummary {
    pub principal: PrincipalId,
    pub root_key: String,
    pub current_keys: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authority::trust::registry::SignerScope;

    fn fp(byte: u8) -> String {
        format!("{byte:02x}").repeat(32)
    }

    fn record(byte: u8, role: SignerRole, status: SignerStatus) -> SignerRecord {
        let key = lillux::crypto::SigningKey::from_bytes(&[byte; 32]).verifying_key();
        let mut record = SignerRecord::new(
            &key,
            &format!("k{byte}"),
            role,
            SignerScope::operator_default(),
            status,
        );
        record.fingerprint = fp(byte);
        record
    }

    fn succeeded(seq: u64, signer: u8, predecessor: u8) -> TrustEvent {
        TrustEvent {
            schema: "x".into(),
            seq,
            previous: None,
            kind: TrustEventKind::Succeeded,
            signer: fp(signer),
            record_digest: String::new(),
            request_digest: String::new(),
            operator: String::new(),
            predecessor: Some(fp(predecessor)),
        }
    }

    fn solo(byte: u8) -> PrincipalId {
        PrincipalId::solo(&format!("fp:{}", fp(byte))).unwrap()
    }

    #[test]
    fn a_key_with_no_record_speaks_for_its_own_solo_principal() {
        let graph = SuccessionGraph::build(&[], &[]);
        let owner = graph.ownership_of(&fp(9)).unwrap();
        assert_eq!(owner.principal, solo(9));
        assert_eq!(owner.via, OwnerVia::Solo);
    }

    #[test]
    fn a_rotated_successor_keeps_the_root_principal_and_the_retired_root_owns_nothing() {
        let records = [
            record(1, SignerRole::Operator, SignerStatus::Retired),
            record(2, SignerRole::Operator, SignerStatus::Active),
        ];
        let graph = SuccessionGraph::build(&records, &[succeeded(1, 2, 1)]);
        let owner = graph.ownership_of(&fp(2)).unwrap();
        assert_eq!(owner.principal, solo(1));
        assert_eq!(owner.via, OwnerVia::Lineage { root: fp(1) });
        assert_eq!(
            graph.ownership_of(&fp(1)).unwrap_err(),
            OwnerRefusal::KeyNotActive {
                status: SignerStatus::Retired
            }
        );
    }

    #[test]
    fn revoking_a_retired_root_keeps_the_successor_principal() {
        let records = [
            record(1, SignerRole::Operator, SignerStatus::Revoked),
            record(2, SignerRole::Operator, SignerStatus::Active),
        ];
        let graph = SuccessionGraph::build(&records, &[succeeded(1, 2, 1)]);
        assert_eq!(graph.ownership_of(&fp(2)).unwrap().principal, solo(1));
        assert_eq!(
            graph.ownership_of(&fp(1)).unwrap_err().code(),
            "owner_key_not_active"
        );
    }

    #[test]
    fn a_revoked_successor_owns_nothing() {
        let records = [
            record(1, SignerRole::Operator, SignerStatus::Retired),
            record(2, SignerRole::Operator, SignerStatus::Revoked),
            record(3, SignerRole::Operator, SignerStatus::Active),
        ];
        let graph = SuccessionGraph::build(&records, &[succeeded(1, 2, 1), succeeded(2, 3, 2)]);
        assert!(graph.ownership_of(&fp(2)).is_err());
        assert_eq!(graph.ownership_of(&fp(3)).unwrap().principal, solo(1));
    }

    #[test]
    fn branching_successors_both_resolve_to_the_root() {
        let records = [
            record(1, SignerRole::Operator, SignerStatus::Retired),
            record(2, SignerRole::Operator, SignerStatus::Active),
            record(3, SignerRole::Operator, SignerStatus::Active),
        ];
        let graph = SuccessionGraph::build(&records, &[succeeded(1, 2, 1), succeeded(2, 3, 1)]);
        assert_eq!(graph.ownership_of(&fp(2)).unwrap().principal, solo(1));
        assert_eq!(graph.ownership_of(&fp(3)).unwrap().principal, solo(1));
        let successors: Vec<_> = graph
            .successors(&fp(1))
            .map(|node| node.fingerprint.clone())
            .collect();
        assert_eq!(successors, [fp(2), fp(3)]);
    }

    #[test]
    fn continuity_covers_every_signer_role() {
        for role in [
            SignerRole::Operator,
            SignerRole::Publisher,
            SignerRole::ProjectAuthor,
            SignerRole::Authoring,
        ] {
            let records = [
                record(1, role, SignerStatus::Retired),
                record(2, role, SignerStatus::Active),
            ];
            let graph = SuccessionGraph::build(&records, &[succeeded(1, 2, 1)]);
            assert_eq!(graph.ownership_of(&fp(2)).unwrap().principal, solo(1));
        }
    }

    #[test]
    fn a_cycle_in_a_corrupt_chain_is_refused_by_name() {
        let records = [
            record(1, SignerRole::Operator, SignerStatus::Active),
            record(2, SignerRole::Operator, SignerStatus::Active),
        ];
        // Each key names the other; the first event per key wins, so 2 has
        // predecessor 1 and 1 has predecessor 2.
        let graph = SuccessionGraph::build(&records, &[succeeded(1, 2, 1), succeeded(2, 1, 2)]);
        // A loop means a damaged registry: refused, naming the keys.
        for key in [fp(1), fp(2)] {
            assert_eq!(
                graph.ownership_of(&key).unwrap_err().code(),
                "owner_chain_corrupt"
            );
            assert_eq!(
                graph.signing_principal_of(&key).unwrap_err().code(),
                "owner_chain_corrupt"
            );
        }
        assert_eq!(graph.corrupt_chains().len(), 2);
        assert!(graph.principals().is_empty());
    }

    #[test]
    fn a_retired_key_still_stands_behind_its_statements_and_a_revoked_one_does_not() {
        let records = [
            record(1, SignerRole::Operator, SignerStatus::Retired),
            record(2, SignerRole::Operator, SignerStatus::Revoked),
            record(3, SignerRole::Operator, SignerStatus::Active),
        ];
        let graph = SuccessionGraph::build(&records, &[succeeded(1, 2, 1), succeeded(2, 3, 2)]);
        assert_eq!(graph.signing_principal_of(&fp(1)).unwrap(), Some(solo(1)));
        assert_eq!(graph.signing_principal_of(&fp(2)).unwrap(), None);
        assert_eq!(graph.signing_principal_of(&fp(3)).unwrap(), Some(solo(1)));
        assert_eq!(graph.signing_principal_of(&fp(9)).unwrap(), None);
        // Acting is separate: the retired key still owns nothing.
        assert!(graph.ownership_of(&fp(1)).is_err());
    }

    #[test]
    fn principals_list_root_and_current_keys() {
        let records = [
            record(1, SignerRole::Operator, SignerStatus::Retired),
            record(2, SignerRole::Operator, SignerStatus::Active),
            record(5, SignerRole::Publisher, SignerStatus::Active),
        ];
        let graph = SuccessionGraph::build(&records, &[succeeded(1, 2, 1)]);
        let principals = graph.principals();
        assert_eq!(principals.len(), 2);
        let operator = principals.iter().find(|p| p.root_key == fp(1)).unwrap();
        assert_eq!(operator.principal, solo(1));
        assert_eq!(operator.current_keys, [fp(2)]);
    }
}
