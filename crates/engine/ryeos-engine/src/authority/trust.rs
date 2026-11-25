//! Item signer trust store and signature verification.
//!
//! The trust store is built from the node-owned signer registry
//! (`<app_root>/.ai/node/trust/`, see [`registry`]) plus the node key, which
//! is the implicit root of trust. Trust is node state: it is never read from
//! a project, a bundle, or an operator config directory, and the only way to
//! add a signer is a node-signed registry record written by the trust
//! operations.
//!
//! Every signer carries a role and a scope (spaces, kinds, projects).
//! [`verify_item_signature`] judges the scope wherever a trust class is
//! formed: a signature from a pinned key outside its scope classifies as
//! `Untrusted` in project space and is refused with a typed
//! `signer_out_of_scope` error in bundle and node space. A `revoked` signer
//! verifies nothing; a `retired` signer still verifies existing items.
//!
//! Verifies Ed25519 item signatures and computes content hashes over
//! the post-signature content.
//!
//! The trust store does NOT share trust policy with daemon request auth or
//! node auth; those are distinct trust domains.

use ryeos_state::principal_id::PrincipalId;
use ryeos_state::project_identity::{LogicalProject, LogicalProjectAbsence};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use lillux::crypto::{Signature, Verifier, VerifyingKey};

mod endorsement;
mod ownership;
mod publication;
mod registry;
pub use endorsement::*;
pub use ownership::*;
pub use publication::*;
pub use registry::*;

use crate::contracts::{
    ResolvedItem, SignatureEnvelope, SignatureHeader, SignerFingerprint, TrustClass, VerifiedItem,
};
use crate::error::EngineError;

// ── Publisher trust document (PUBLISHER_TRUST.toml) ─────────────────

/// Canonical schema for `PUBLISHER_TRUST.toml` — the universal trust
/// artifact emitted by `ryeos publish` and consumed by `ryeos init
/// --trust-file` and `ryeos trust pin --from`.
///
/// ```toml
/// public_key = "ed25519:<base64>"   # required
/// fingerprint = "<sha256-hex>"      # required, MUST match public_key
/// owner = "<label>"                 # required, informational
/// ```
///
/// Strict parsing: `deny_unknown_fields`. One parser shared by all
/// consumers (init, trust pin, publish emit).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PublisherTrustDoc {
    /// Ed25519 public key in `ed25519:<base64>` format.
    pub public_key: String,
    /// SHA-256 hex fingerprint of the raw 32-byte public key.
    pub fingerprint: String,
    /// Informational owner label (e.g. "ryeos-official", "RyeOS Development").
    pub owner: String,
}

impl PublisherTrustDoc {
    /// Parse a `PUBLISHER_TRUST.toml` from text. Validates that the
    /// fingerprint matches the public key bytes.
    pub fn parse(text: &str) -> Result<Self, EngineError> {
        let doc: PublisherTrustDoc = toml::from_str(text)
            .map_err(|e| EngineError::Internal(format!("invalid PUBLISHER_TRUST.toml: {e}")))?;
        doc.validate()?;
        Ok(doc)
    }

    /// Validate that the fingerprint matches the public key.
    pub fn validate(&self) -> Result<(), EngineError> {
        let vk = self.decode_verifying_key()?;
        let computed = compute_fingerprint(&vk);
        if computed != self.fingerprint {
            return Err(EngineError::Internal(format!(
                "PUBLISHER_TRUST.toml self-inconsistent: declared fingerprint {} \
                 but public_key bytes hash to {}",
                self.fingerprint, computed
            )));
        }
        Ok(())
    }

    /// Decode the `ed25519:<base64>` public key into a `VerifyingKey`.
    pub fn decode_verifying_key(&self) -> Result<VerifyingKey, EngineError> {
        let b64 = self.public_key.strip_prefix("ed25519:").ok_or_else(|| {
            EngineError::Internal("public_key must be ed25519:<base64> format".to_string())
        })?;
        let bytes: Vec<u8> = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| EngineError::Internal(format!("invalid base64 in public_key: {e}")))?;
        let len = bytes.len();
        let arr: [u8; 32] = bytes.try_into().map_err(|_| {
            EngineError::Internal(format!("public_key must decode to 32 bytes, got {len}"))
        })?;
        VerifyingKey::from_bytes(&arr)
            .map_err(|e| EngineError::Internal(format!("invalid Ed25519 public key: {e}")))
    }

    /// Render to TOML text suitable for writing to disk.
    pub fn to_toml(&self) -> String {
        format!(
            "public_key = \"{}\"\nfingerprint = \"{}\"\nowner = \"{}\"\n",
            self.public_key, self.fingerprint, self.owner,
        )
    }
}

// ── Node document signers ───────────────────────────────────────────

/// Stable refusal code: a node document is signed by a key other than the
/// one key allowed to sign that kind of node state.
pub const NODE_DOCUMENT_WRONG_SIGNER: &str = "node_document_wrong_signer";

/// A node document (node policy, schedules, node config, bundle
/// registrations, the init completion record) signed by a key that may not
/// sign it. Node state verifies against exactly one signer, never against
/// every pinned key: a pinned third-party publisher key must not be able to
/// sign node state.
#[derive(Debug, Clone, thiserror::Error)]
#[error(
    "{NODE_DOCUMENT_WRONG_SIGNER}: {path} is signed by {signer}, but only the {role} key {expected} may sign it; {cure}"
)]
pub struct NodeDocumentWrongSigner {
    pub path: String,
    pub signer: String,
    pub expected: String,
    /// `node` or `operator`.
    pub role: &'static str,
    pub cure: String,
}

/// The cure named for node-key documents: they are written only by the node.
pub const NODE_DOCUMENT_CURE: &str = "this is node state that only the node itself writes: remove the file and recreate it through the node's own command";

/// Refuse a node document whose signature line names any signer but
/// `expected`. Callers then verify the signature against
/// [`TrustStore::restricted_to`]`(expected)`, never the whole store.
pub fn require_document_signer(
    path: &Path,
    signer: &str,
    expected: &str,
    role: &'static str,
    cure: &str,
) -> Result<(), NodeDocumentWrongSigner> {
    if signer == expected {
        return Ok(());
    }
    Err(NodeDocumentWrongSigner {
        path: path.display().to_string(),
        signer: signer.to_string(),
        expected: expected.to_string(),
        role,
        cure: cure.to_string(),
    })
}

// ── Trusted signer entry ────────────────────────────────────────────

/// A trusted signer loaded from the trust store.
#[derive(Debug, Clone)]
pub struct TrustedSigner {
    pub fingerprint: String,
    pub verifying_key: VerifyingKey,
    pub label: Option<String>,
}

// ── Signature context ───────────────────────────────────────────────

/// Where a signature was found. Scope is judged against this wherever a
/// trust class is formed.
#[derive(Debug, Clone, Copy)]
pub struct SignerContext<'a> {
    pub space: SignerSpace,
    /// Item kind, or a fixed document name for signed non-item documents
    /// (for example `bundle_manifest`).
    pub kind: &'a str,
    /// The owner+name project the item belongs to, as this node has
    /// established it. Never a path: an absent identity matches no scoped
    /// signer.
    pub project: &'a LogicalProject,
}

/// The project of bundle- and node-space contexts.
pub static NO_PROJECT: LogicalProject = LogicalProject::Absent(LogicalProjectAbsence::Projectless);

impl<'a> SignerContext<'a> {
    pub fn new(space: impl Into<SignerSpace>, kind: &'a str, project: &'a LogicalProject) -> Self {
        Self {
            space: space.into(),
            kind,
            project,
        }
    }

    /// A bundle-space item or document.
    pub fn bundle(kind: &'a str) -> Self {
        Self::new(SignerSpace::Bundle, kind, &NO_PROJECT)
    }

    /// A node-space item or document.
    pub fn node(kind: &'a str) -> Self {
        Self::new(SignerSpace::Node, kind, &NO_PROJECT)
    }

    /// A project-space item.
    pub fn project(kind: &'a str, project: &'a LogicalProject) -> Self {
        Self::new(SignerSpace::Project, kind, project)
    }
}

/// Scope of the implicit node root: node state only. The node key never
/// signs project items; runtime-authored ones carry the authoring key.
fn node_root_scope() -> SignerScope {
    SignerScope {
        spaces: vec![SignerSpace::Node],
        kinds: vec![ALL_KINDS.to_string()],
        projects: Vec::new(),
    }
}

enum ScopeDecision {
    Admitted,
    Revoked { label: String },
    OutOfScope { label: String, scope: SignerScope },
}

// ── Trust store ─────────────────────────────────────────────────────

/// Item signer trust store.
///
/// Maps signer fingerprints to their Ed25519 verifying keys and, for
/// registry signers, to their node-signed [`SignerRecord`].
///
/// This is the **item signer** trust domain only. Distinct from
/// principal auth and node trust.
#[derive(Debug, Clone)]
pub struct TrustStore {
    /// Keys that can verify signatures: the node root plus every `active`
    /// or `retired` registry signer.
    signers: HashMap<String, TrustedSigner>,
    /// Registry records in every status (revoked ones included).
    records: BTreeMap<String, SignerRecord>,
    /// The node key, the implicit root of trust. Absent for in-memory
    /// stores built from raw signers.
    node_key: Option<VerifyingKey>,
    /// Scopes of signers that arrived without a registry record (the signer
    /// list a runtime receives in its launch envelope).
    entry_scopes: HashMap<String, SignerScope>,
    /// Endorsements that make authoring-key items trusted. Empty for stores
    /// built from raw signers or a launch envelope.
    endorsements: endorsement::EndorsementView,
    /// Set on a store rebuilt from a launch envelope: the launch's frozen
    /// endorsement snapshot, which fixes the project identity and names the
    /// signers whose items it decides.
    launch_endorsements: Option<LaunchEndorsements>,
    /// The node's key-succession graph, from which project ownership is
    /// resolved. Present only on a store loaded from the node registry with
    /// its event chain; every other store refuses ownership questions.
    succession: Option<std::sync::Arc<SuccessionGraph>>,
}

#[derive(Debug, Clone)]
struct LaunchEndorsements {
    project: ryeos_state::project_identity::ProjectIdentity,
    authoring_signers: std::collections::BTreeSet<String>,
}

impl TrustStore {
    /// Create an empty trust store (no signers trusted).
    pub fn empty() -> Self {
        Self {
            signers: HashMap::new(),
            records: BTreeMap::new(),
            node_key: None,
            entry_scopes: HashMap::new(),
            endorsements: endorsement::EndorsementView::default(),
            launch_endorsements: None,
            succession: None,
        }
    }

    /// Create an unscoped trust store from a pre-built list of signers.
    ///
    /// For callers that already hold verified keys by other means (tests,
    /// tools verifying against one explicit key). Every signer is valid in
    /// every space and for every kind and project. Node-backed stores come
    /// from [`TrustStore::load_node_registry`].
    pub fn from_signers(signers: Vec<TrustedSigner>) -> Self {
        let map = signers
            .into_iter()
            .map(|s| (s.fingerprint.clone(), s))
            .collect();
        Self {
            signers: map,
            records: BTreeMap::new(),
            node_key: None,
            entry_scopes: HashMap::new(),
            endorsements: endorsement::EndorsementView::default(),
            launch_endorsements: None,
            succession: None,
        }
    }

    /// The scoped signer list a runtime receives: the node root plus every
    /// `active` or `retired` registry signer, sorted by fingerprint.
    pub fn signer_entries(&self) -> Vec<TrustedSignerEntry> {
        let mut entries = Vec::with_capacity(self.signers.len());
        for signer in self.signers.values() {
            let scope = match self.records.get(&signer.fingerprint) {
                Some(record) => record.scope.clone(),
                None if self.node_fingerprint().as_deref() == Some(signer.fingerprint.as_str()) => {
                    node_root_scope()
                }
                None => match self.entry_scopes.get(&signer.fingerprint) {
                    Some(scope) => scope.clone(),
                    None => SignerScope::everywhere(),
                },
            };
            entries.push(TrustedSignerEntry {
                fingerprint: signer.fingerprint.clone(),
                public_key: format!(
                    "ed25519:{}",
                    base64::engine::general_purpose::STANDARD
                        .encode(signer.verifying_key.as_bytes())
                ),
                label: signer.label.clone().unwrap_or_default(),
                scope,
                endorsements: None,
            });
        }
        entries.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
        entries
    }

    /// Rebuild a store from the signer list carried in a launch envelope.
    /// Every entry's key must hash to its fingerprint and its scope must be
    /// structurally valid; scope is judged exactly as in the daemon.
    pub fn from_signer_entries(entries: &[TrustedSignerEntry]) -> Result<Self, TrustError> {
        let mut signers = HashMap::new();
        let mut entry_scopes = HashMap::new();
        for entry in entries {
            let refusal = |reason: String| TrustError::RecordTampered {
                path: PathBuf::from(format!("{}.yaml", entry.fingerprint)),
                reason,
            };
            let encoded = entry
                .public_key
                .strip_prefix("ed25519:")
                .ok_or_else(|| refusal("public_key must be ed25519:<base64>".to_string()))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|e| refusal(format!("public_key is not valid base64: {e}")))?;
            let array: [u8; 32] = bytes
                .try_into()
                .map_err(|_| refusal("public_key must decode to 32 bytes".to_string()))?;
            let key = VerifyingKey::from_bytes(&array)
                .map_err(|e| refusal(format!("public_key is not an Ed25519 key: {e}")))?;
            if compute_fingerprint(&key) != entry.fingerprint {
                return Err(refusal(
                    "public_key does not hash to the declared fingerprint".to_string(),
                ));
            }
            entry.scope.validate().map_err(refusal)?;
            signers.insert(
                entry.fingerprint.clone(),
                TrustedSigner {
                    fingerprint: entry.fingerprint.clone(),
                    verifying_key: key,
                    label: Some(entry.label.clone()),
                },
            );
            entry_scopes.insert(entry.fingerprint.clone(), entry.scope.clone());
        }
        let mut store = Self {
            signers,
            records: BTreeMap::new(),
            node_key: None,
            entry_scopes,
            endorsements: endorsement::EndorsementView::default(),
            launch_endorsements: None,
            succession: None,
        };
        store.apply_entry_endorsements(entries)?;
        Ok(store)
    }

    /// A launch's signer list carries, on each authoring-role signer, the
    /// endorsements the request froze. Every such entry must carry the same
    /// snapshot; the store then judges those signers' items by it.
    fn apply_entry_endorsements(
        &mut self,
        entries: &[TrustedSignerEntry],
    ) -> Result<(), TrustError> {
        let invalid = |reason: String| TrustError::EndorsementRecordInvalid {
            path: PathBuf::from("launch-endorsements"),
            reason,
        };
        let mut snapshot: Option<&EndorsementSnapshot> = None;
        let mut authoring_signers = std::collections::BTreeSet::new();
        for entry in entries {
            let Some(carried) = &entry.endorsements else {
                continue;
            };
            if snapshot.is_some_and(|first| first != carried) {
                return Err(invalid(
                    "the signer list carries different endorsement snapshots".to_string(),
                ));
            }
            snapshot = Some(carried);
            authoring_signers.insert(entry.fingerprint.clone());
        }
        let Some(snapshot) = snapshot else {
            return Ok(());
        };
        let index = EndorsementIndex::from_snapshot(snapshot, |fingerprint| {
            self.signers
                .get(fingerprint)
                .map(|signer| signer.verifying_key)
        })
        .map_err(invalid)?;
        self.endorsements = endorsement::EndorsementView::frozen(index);
        self.launch_endorsements = Some(LaunchEndorsements {
            project: snapshot.project.clone(),
            authoring_signers,
        });
        Ok(())
    }

    /// Build a store from the node key and verified registry records.
    pub fn from_registry(
        node_key: VerifyingKey,
        records: Vec<SignerRecord>,
    ) -> Result<Self, TrustError> {
        let node_fingerprint = compute_fingerprint(&node_key);
        let mut signers = HashMap::new();
        signers.insert(
            node_fingerprint.clone(),
            TrustedSigner {
                fingerprint: node_fingerprint.clone(),
                verifying_key: node_key,
                label: Some("node".to_string()),
            },
        );
        let mut by_fingerprint = BTreeMap::new();
        for record in records {
            if record.fingerprint == node_fingerprint {
                return Err(TrustError::RecordNameMismatch {
                    path: PathBuf::from(record.file_name()),
                    reason: "describes the node key, which is the implicit root and has no record"
                        .to_string(),
                });
            }
            let verifying_key =
                record
                    .verifying_key()
                    .map_err(|reason| TrustError::RecordNameMismatch {
                        path: PathBuf::from(record.file_name()),
                        reason,
                    })?;
            if record.role == SignerRole::Authoring
                && record.status != SignerStatus::Revoked
                && record.scope != SignerScope::authoring_default()
            {
                return Err(TrustError::AuthoringScopeRetired {
                    fingerprint: record.fingerprint.clone(),
                    label: record.label.clone(),
                    scope: record.scope.describe(),
                });
            }
            if record.status != SignerStatus::Revoked {
                signers.insert(
                    record.fingerprint.clone(),
                    TrustedSigner {
                        fingerprint: record.fingerprint.clone(),
                        verifying_key,
                        label: Some(record.label.clone()),
                    },
                );
            }
            by_fingerprint.insert(record.fingerprint.clone(), record);
        }
        Ok(Self {
            signers,
            records: by_fingerprint,
            node_key: Some(node_key),
            entry_scopes: HashMap::new(),
            endorsements: endorsement::EndorsementView::default(),
            launch_endorsements: None,
            succession: None,
        })
    }

    /// Load the node's signer registry from `<app_root>/.ai/node/trust/signers/`.
    ///
    /// The node key comes from the node's public identity. Every record must
    /// verify under it; an unsigned, self-signed, cross-signed, edited,
    /// misnamed or unknown-schema record is a typed refusal. A missing
    /// registry directory is an empty registry (only the node key trusted).
    pub fn load_node_registry(app_root: &Path) -> Result<Self, TrustError> {
        let node_key = load_node_verifying_key(app_root)?;
        let records = load_signer_records(&signers_dir(app_root), &node_key)?;
        let chain = load_event_chain(&events_dir(app_root), &node_key)?;
        let mut store = Self::from_registry(node_key, records)?.with_succession_chain(&chain);
        let ledger = EndorsementLedger::load(app_root, &node_key)?;
        let index = EndorsementIndex::build(&ledger, &store)?;
        tracing::debug!(
            records = store.records.len(),
            endorsements = index.len(),
            "loaded node signer registry"
        );
        store.endorsements = endorsement::EndorsementView::live(index);
        Ok(store)
    }

    /// Which kinds a standing endorsement may name under this store. Unset
    /// until [`Self::with_standing_kinds`] is called, so a store that was
    /// never paired with a kind registry honours no standing endorsement.
    pub fn standing_gate(&self) -> &endorsement::StandingGate {
        self.endorsements.gate()
    }

    /// This store, paired with the kinds whose signed schema allows a standing
    /// endorsement. The pairing belongs to the returned value, so an engine
    /// generation that owns this store and its kind registry moves them
    /// together: no endorsement is judged by another generation's registry.
    pub fn with_standing_kinds(mut self, kinds: impl IntoIterator<Item = String>) -> Self {
        self.endorsements = self
            .endorsements
            .with_gate(endorsement::StandingGate::of_kinds(kinds));
        self
    }

    /// The stored endorsements that cover nothing under this store's gate,
    /// by digest, with why.
    pub fn endorsement_inert(&self) -> Vec<(String, endorsement::InertReason)> {
        self.endorsements.current().inert(self.endorsements.gate())
    }

    /// The endorsements in force, as of now. This is the stored index; use
    /// [`Self::endorsement_inert`] for the ones that cover nothing.
    pub fn endorsement_index(&self) -> std::sync::Arc<EndorsementIndex> {
        self.endorsements.current()
    }

    /// A copy of this store whose endorsement index never changes. A request
    /// takes one so every verification it makes sees the same endorsements,
    /// even when one is recorded or revoked while it runs.
    pub fn snapshot(&self) -> Self {
        let mut copy = self.clone();
        copy.endorsements = self.endorsements.snapshot();
        copy
    }

    /// Re-read the endorsement store and replace the index under a live store,
    /// so a running node follows a recorded or revoked endorsement without a
    /// restart. Returns the new index identity. A store that is not live (a
    /// snapshot, or one built from raw signers) is not changed.
    pub fn refresh_endorsements(&self, app_root: &Path) -> Result<String, TrustError> {
        let node_key = self.node_key.ok_or_else(|| TrustError::Unreadable {
            path: endorsements_dir(app_root),
            reason: "the store has no node key, so it holds no endorsements".to_string(),
        })?;
        let ledger = EndorsementLedger::load(app_root, &node_key)?;
        let index = EndorsementIndex::build(&ledger, self)?;
        let identity = index.identity(self.endorsements.gate(), lillux::time::unix_secs_now());
        self.endorsements.replace(index);
        Ok(identity)
    }

    /// The endorsements a launch of `logical_project` carries to its runtime,
    /// taken from this store's index (a request's frozen snapshot), or `None`
    /// when the project states no identity or nothing applies. The identity
    /// is the same one [`Self::endorsed`] matches on.
    pub fn launch_endorsements(
        &self,
        logical_project: &LogicalProject,
    ) -> Option<EndorsementSnapshot> {
        self.endorsements
            .current()
            .snapshot_for(self.endorsements.gate(), logical_project.identity()?)
    }

    /// The scoped signer list a launch carries: this store's entries, with the
    /// endorsements `frozen` holds for `logical_project` attached to each
    /// authoring-role signer. Config loaders and runtimes built from the list
    /// judge those signers' items by the endorsements, and the list's digest
    /// (which every config dependency proof and cache key records) changes
    /// whenever the endorsements do.
    pub fn launch_signer_entries(
        &self,
        frozen: &TrustStore,
        logical_project: &LogicalProject,
    ) -> Vec<TrustedSignerEntry> {
        let mut entries = self.signer_entries();
        // Scoped entries are judged here, against the identity the launch
        // sealed, so a runtime that never sees that identity judges the same:
        // a scope naming this launch's project is unscoped for the launch, and
        // one that does not loses project space.
        let identity = logical_project.identity();
        let owners: HashMap<String, Option<PrincipalId>> = entries
            .iter()
            .map(|entry| {
                (
                    entry.fingerprint.clone(),
                    self.signing_principal_of(&entry.fingerprint),
                )
            })
            .collect();
        entries.retain_mut(|entry| {
            if entry.scope.projects.is_empty()
                || !entry.scope.spaces.contains(&SignerSpace::Project)
            {
                return true;
            }
            let named = identity.is_some_and(|identity| {
                entry.scope.projects.iter().any(|project| {
                    project.names(
                        identity,
                        owners.get(&entry.fingerprint).and_then(Option::as_ref),
                    )
                })
            });
            entry.scope.projects.clear();
            if !named {
                entry
                    .scope
                    .spaces
                    .retain(|space| *space != SignerSpace::Project);
            }
            !entry.scope.spaces.is_empty()
        });
        let Some(snapshot) = frozen.launch_endorsements(logical_project) else {
            return entries;
        };
        for entry in &mut entries {
            let authoring = self.records.get(&entry.fingerprint).is_some_and(|record| {
                record.role == SignerRole::Authoring && record.status != SignerStatus::Revoked
            });
            if authoring {
                entry.endorsements = Some(snapshot.clone());
            }
        }
        entries
    }

    /// This store with the key-succession graph built from its records and
    /// `chain` (the verified node-signed key-event chain). Only a store with
    /// a graph answers ownership questions.
    pub fn with_succession_chain(mut self, chain: &[TrustEvent]) -> Self {
        let records: Vec<SignerRecord> = self.records.values().cloned().collect();
        self.succession = Some(std::sync::Arc::new(SuccessionGraph::build(&records, chain)));
        self
    }

    /// The node's key-succession graph, when this store holds one.
    pub fn succession(&self) -> Option<&SuccessionGraph> {
        self.succession.as_deref()
    }

    /// The principal whose project namespace `key` acts in on this node.
    /// `key` is a key fingerprint, `fp:<64 lowercase hex>` or bare hex. A
    /// retired or revoked key has none (`owner_key_not_active`). This and
    /// [`Self::speaks_for`] are the only ways to obtain an owner from a key.
    pub fn owner_for(&self, key: &str) -> Result<ResolvedOwner, OwnerRefusal> {
        let graph = self
            .succession
            .as_deref()
            .ok_or(OwnerRefusal::Unavailable)?;
        graph.ownership_of(key.strip_prefix("fp:").unwrap_or(key))
    }

    /// The principal a signature by `key` speaks for when verified: its
    /// chain root's solo principal for an active or retired key; `None` for
    /// a revoked key, a key with no record, a corrupt chain, or a store with
    /// no registry graph. For judging signed items and stored statements,
    /// never for acting.
    pub fn signing_principal_of(&self, key: &str) -> Option<PrincipalId> {
        self.succession
            .as_deref()?
            .signing_principal_of(key.strip_prefix("fp:").unwrap_or(key))
            .ok()
            .flatten()
    }

    /// Whether `key` speaks for `principal` on this node. Decides project
    /// ownership only; never a grant, scope, session or thread read.
    pub fn speaks_for(&self, key: &str, principal: &PrincipalId) -> bool {
        self.owner_for(key)
            .is_ok_and(|owner| &owner.principal == principal)
    }

    /// A copy of this store holding exactly `index`, frozen. For stores built
    /// in memory and for tests.
    pub fn with_endorsement_index(mut self, index: EndorsementIndex) -> Self {
        self.endorsements = endorsement::EndorsementView::frozen(index);
        self
    }

    /// Check whether a fingerprint can verify signatures (active, retired,
    /// or the node root). Revoked signers are not trusted.
    pub fn is_trusted(&self, fingerprint: &str) -> bool {
        self.signers.contains_key(fingerprint)
    }

    /// Look up a signer's verifying key. Revoked signers are absent.
    pub fn get(&self, fingerprint: &str) -> Option<&TrustedSigner> {
        self.signers.get(fingerprint)
    }

    /// The registry record for a signer in any status. The node root has none.
    pub fn signer(&self, fingerprint: &str) -> Option<&SignerRecord> {
        self.records.get(fingerprint)
    }

    /// Every registry record in fingerprint order, revoked ones included.
    pub fn records(&self) -> impl Iterator<Item = &SignerRecord> {
        self.records.values()
    }

    /// Fingerprint of the implicit node root, when the store has one.
    pub fn node_fingerprint(&self) -> Option<String> {
        self.node_key.as_ref().map(compute_fingerprint)
    }

    /// The node key, the root of trust, when the store has one.
    pub fn node_verifying_key(&self) -> Option<&VerifyingKey> {
        self.node_key.as_ref()
    }

    /// A store holding only `fingerprint`'s signer (empty when it is not
    /// trusted here). Node documents verify against this, not the whole
    /// store. The signer keeps its registry record and scope.
    pub fn restricted_to(&self, fingerprint: &str) -> Self {
        let signers: HashMap<String, TrustedSigner> = self
            .signers
            .get(fingerprint)
            .map(|signer| (signer.fingerprint.clone(), signer.clone()))
            .into_iter()
            .collect();
        let node_key = self
            .node_key
            .filter(|key| compute_fingerprint(key) == fingerprint);
        Self {
            records: self
                .records
                .get(fingerprint)
                .map(|record| (fingerprint.to_string(), record.clone()))
                .into_iter()
                .collect(),
            entry_scopes: self
                .entry_scopes
                .get(fingerprint)
                .map(|scope| (fingerprint.to_string(), scope.clone()))
                .into_iter()
                .collect(),
            signers,
            node_key,
            endorsements: endorsement::EndorsementView::default(),
            launch_endorsements: None,
            succession: None,
        }
    }

    /// Every trusted signer, in no particular order.
    pub fn signers(&self) -> impl Iterator<Item = &TrustedSigner> {
        self.signers.values()
    }

    /// Number of keys that can verify signatures.
    pub fn len(&self) -> usize {
        self.signers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.signers.is_empty()
    }

    /// Registry digest over the records (every status). Changes with any
    /// pin, scope change, retirement or revocation. This is the fact an
    /// offline trust request pins.
    pub fn digest(&self) -> String {
        registry_digest(self.records.values())
    }

    /// Stable identity for the exact signer set used by a request snapshot.
    ///
    /// Includes public-key bytes, labels and every record's scope and
    /// status, so a scope change cannot alias a prior trust state.
    pub fn fingerprint(&self) -> String {
        let mut signers = self.signers.values().collect::<Vec<_>>();
        signers.sort_by(|left, right| left.fingerprint.cmp(&right.fingerprint));
        let mut bytes = Vec::new();
        for signer in signers {
            append_fingerprint_field(&mut bytes, signer.fingerprint.as_bytes());
            append_fingerprint_field(&mut bytes, signer.verifying_key.as_bytes());
            append_fingerprint_field(&mut bytes, signer.label.as_deref().unwrap_or("").as_bytes());
        }
        append_fingerprint_field(&mut bytes, self.digest().as_bytes());
        append_fingerprint_field(
            &mut bytes,
            trusted_signers_digest(&self.signer_entries()).as_bytes(),
        );
        append_fingerprint_field(
            &mut bytes,
            self.endorsements
                .current()
                .identity(self.endorsements.gate(), lillux::time::unix_secs_now())
                .as_bytes(),
        );
        lillux::cas::sha256_hex(&bytes)
    }

    pub(crate) fn estimated_size_bytes(&self) -> usize {
        self.signers
            .values()
            .map(|signer| {
                signer.fingerprint.capacity()
                    + signer.label.as_ref().map(String::capacity).unwrap_or(0)
                    + signer.verifying_key.as_bytes().len()
            })
            .sum::<usize>()
            + self.records.len() * 512
    }

    fn scope_decision(
        &self,
        fingerprint: &str,
        context: &SignerContext<'_>,
        content_digest: &str,
    ) -> ScopeDecision {
        if let Some(launch) = &self.launch_endorsements
            && launch.authoring_signers.contains(fingerprint)
        {
            if self.endorsed(context, content_digest) {
                return ScopeDecision::Admitted;
            }
            return ScopeDecision::OutOfScope {
                label: self
                    .signers
                    .get(fingerprint)
                    .and_then(|signer| signer.label.clone())
                    .unwrap_or_default(),
                scope: self
                    .entry_scopes
                    .get(fingerprint)
                    .cloned()
                    .unwrap_or_else(SignerScope::authoring_default),
            };
        }
        if let Some(record) = self.records.get(fingerprint) {
            if record.status == SignerStatus::Revoked {
                return ScopeDecision::Revoked {
                    label: record.label.clone(),
                };
            }
            // The authoring key's registry scope never decides trust. What it
            // signed is trusted only when a live endorsement covers the item's
            // exact digest, or its kind within the project.
            if record.role == SignerRole::Authoring {
                if self.endorsed(context, content_digest) {
                    return ScopeDecision::Admitted;
                }
                return ScopeDecision::OutOfScope {
                    label: record.label.clone(),
                    scope: record.scope.clone(),
                };
            }
            // Verification follows "retired verifies, revoked does not", so
            // an `own:` scope is judged on the signing principal, not on
            // whether the key may still act.
            let owner = self.signing_principal_of(fingerprint);
            if record
                .scope
                .admits(context.space, context.kind, context.project, owner.as_ref())
            {
                return ScopeDecision::Admitted;
            }
            return ScopeDecision::OutOfScope {
                label: record.label.clone(),
                scope: record.scope.clone(),
            };
        }
        if self.node_key.as_ref().map(compute_fingerprint).as_deref() == Some(fingerprint) {
            let scope = node_root_scope();
            if scope.admits(context.space, context.kind, context.project, None) {
                return ScopeDecision::Admitted;
            }
            return ScopeDecision::OutOfScope {
                label: "node".to_string(),
                scope,
            };
        }
        if let Some(scope) = self.entry_scopes.get(fingerprint) {
            // A launch's signer list is already resolved against the
            // launch's identity (see `launch_signer_entries`): no entry
            // keeps an `own:` scope, so nothing here needs an owner.
            if scope.admits(context.space, context.kind, context.project, None) {
                return ScopeDecision::Admitted;
            }
            return ScopeDecision::OutOfScope {
                label: self
                    .signers
                    .get(fingerprint)
                    .and_then(|signer| signer.label.clone())
                    .unwrap_or_default(),
                scope: scope.clone(),
            };
        }
        // Unscoped in-memory signer.
        ScopeDecision::Admitted
    }
}

impl TrustStore {
    /// Whether a live endorsement covers an item signed in `context` whose
    /// signed content has digest `content_digest`. Only project items qualify.
    fn endorsed(&self, context: &SignerContext<'_>, content_digest: &str) -> bool {
        if context.space != SignerSpace::Project {
            return false;
        }
        // A store rebuilt from a launch judges only that launch's project,
        // whose identity the daemon sealed into the frozen snapshot.
        if let Some(launch) = &self.launch_endorsements {
            return self.endorsements.current().covers(
                self.endorsements.gate(),
                &launch.project,
                context.kind,
                content_digest,
                lillux::time::unix_secs_now(),
            );
        }
        // Otherwise endorsements name a project by its owner+name identity;
        // an absent identity is never endorsed.
        let Some(identity) = context.project.identity() else {
            return false;
        };
        self.endorsements.current().covers(
            self.endorsements.gate(),
            identity,
            context.kind,
            content_digest,
            lillux::time::unix_secs_now(),
        )
    }
}

fn append_fingerprint_field(bytes: &mut Vec<u8>, field: &[u8]) {
    bytes.extend_from_slice(&(field.len() as u64).to_le_bytes());
    bytes.extend_from_slice(field);
}

// ── Fingerprint computation ─────────────────────────────────────────

/// Compute the SHA-256 hex fingerprint of an Ed25519 public key.
///
/// This matches the fingerprint computation in `crates/bin/daemon/src/identity.rs`.
pub fn compute_fingerprint(key: &VerifyingKey) -> String {
    lillux::signature::compute_fingerprint(key)
}

// ── Content hash after signature line ───────────────────────────────

/// Compute the content hash over file bytes AFTER the signature line.
///
/// The signature line itself is excluded from the hash. This is what
/// the signer hashed when creating the signature.
///
/// Returns the SHA-256 hex digest of the post-signature content, and
/// the byte offset where the content starts (after the signature line's
/// trailing newline).
pub fn content_hash_after_signature(content: &str, envelope: &SignatureEnvelope) -> Option<String> {
    lillux::signature::content_hash_after_signature(
        content,
        &envelope.prefix,
        envelope.suffix.as_deref(),
        envelope.after_shebang,
    )
}

pub fn strip_signature_lines(content: &str) -> String {
    lillux::signature::strip_signature_lines(content)
}

// ── Signature verification ──────────────────────────────────────────

/// Verify a node-signed document against the node key alone.
///
/// Node documents (policy generations, bindings, configuration) are trusted
/// only when the node key signed them; no pinned publisher, operator or
/// project key may stand in.
pub fn verify_node_signed_document(
    content: &str,
    header: &SignatureHeader,
    envelope: &SignatureEnvelope,
    node_key: &VerifyingKey,
) -> Result<(), EngineError> {
    let node_fingerprint = compute_fingerprint(node_key);
    if header.signer_fingerprint != node_fingerprint {
        return Err(EngineError::SignatureVerificationFailed {
            canonical_ref: String::new(),
            reason: format!(
                "node document is signed by {}, not by this node's key {node_fingerprint}",
                header.signer_fingerprint
            ),
        });
    }
    let actual_hash = content_hash_after_signature(content, envelope).ok_or_else(|| {
        EngineError::SignatureVerificationFailed {
            canonical_ref: String::new(),
            reason: "could not locate signature line in content".into(),
        }
    })?;
    if actual_hash != header.content_hash {
        return Err(EngineError::ContentHashMismatch {
            canonical_ref: String::new(),
            expected: header.content_hash.clone(),
            actual: actual_hash,
        });
    }
    if !lillux::signature::verify_signature(&header.content_hash, &header.signature_b64, node_key) {
        return Err(EngineError::SignatureVerificationFailed {
            canonical_ref: String::new(),
            reason: "Ed25519 signature verification failed".into(),
        });
    }
    Ok(())
}

/// Verify an item's Ed25519 signature.
///
/// Steps:
/// 1. Recompute content hash over bytes after the signature line
/// 2. Compare with the content_hash in the signature header
/// 3. Verify the Ed25519 signature over the content_hash string
/// 4. Judge the signer's scope against `context`
///
/// The signature is over the content_hash hex string (as bytes), matching
/// the pattern in `crates/bin/daemon/src/auth.rs` and `crates/bin/daemon/src/identity.rs`.
#[tracing::instrument(
    name = "engine:trust_verify",
    skip(content, trust_store, context),
    fields(signer = ?header.signer_fingerprint)
)]
pub fn verify_item_signature(
    content: &str,
    header: &SignatureHeader,
    envelope: &SignatureEnvelope,
    trust_store: &TrustStore,
    context: &SignerContext<'_>,
) -> Result<(TrustClass, Option<SignerFingerprint>), EngineError> {
    // Step 1: Recompute content hash over post-signature content
    let actual_hash = content_hash_after_signature(content, envelope).ok_or_else(|| {
        EngineError::SignatureVerificationFailed {
            canonical_ref: String::new(), // caller fills this in
            reason: "could not locate signature line in content".into(),
        }
    })?;

    verify_item_signature_with_hash(&actual_hash, header, trust_store, context)
}

/// Like [`verify_item_signature`], but takes the content hash the
/// caller already recomputed over the post-signature content, so the
/// content is not hashed a second time. The hash must come from
/// [`content_hash_after_signature`] on the same content — passing the
/// header's own hash would turn the integrity check into a tautology.
pub fn verify_item_signature_with_hash(
    actual_hash: &str,
    header: &SignatureHeader,
    trust_store: &TrustStore,
    context: &SignerContext<'_>,
) -> Result<(TrustClass, Option<SignerFingerprint>), EngineError> {
    tracing::trace!(actual_hash = %actual_hash, header_hash = %header.content_hash, "comparing content hashes");

    // Step 2: Compare content hashes
    if actual_hash != header.content_hash {
        return Err(EngineError::ContentHashMismatch {
            canonical_ref: String::new(),
            expected: header.content_hash.clone(),
            actual: actual_hash.to_owned(),
        });
    }

    // Step 3: Decode and verify the Ed25519 signature
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(&header.signature_b64)
        .map_err(|e| EngineError::SignatureVerificationFailed {
            canonical_ref: String::new(),
            reason: format!("invalid base64 in signature: {e}"),
        })?;

    let signature = Signature::from_slice(&sig_bytes).map_err(|e| {
        EngineError::SignatureVerificationFailed {
            canonical_ref: String::new(),
            reason: format!("invalid Ed25519 signature bytes: {e}"),
        }
    })?;

    let fingerprint = &header.signer_fingerprint;

    // A revoked signer verifies nothing, whatever the item claims.
    if let Some(record) = trust_store.signer(fingerprint)
        && record.status == SignerStatus::Revoked
    {
        return Err(TrustError::SignerRevoked {
            fingerprint: fingerprint.clone(),
            label: record.label.clone(),
        }
        .into());
    }

    // Look up signer in trust store
    match trust_store.get(fingerprint) {
        Some(signer) => {
            // Verify: signature is over the content_hash hex string
            signer
                .verifying_key
                .verify(header.content_hash.as_bytes(), &signature)
                .map_err(|_| EngineError::SignatureVerificationFailed {
                    canonical_ref: String::new(),
                    reason: "Ed25519 signature verification failed".into(),
                })?;

            // Step 4: judge scope where the trust class forms.
            match trust_store.scope_decision(fingerprint, context, actual_hash) {
                ScopeDecision::Admitted => {
                    tracing::trace!(fingerprint = %fingerprint, trust_class = "trusted", "signature verified");
                    Ok((
                        TrustClass::Trusted,
                        Some(SignerFingerprint(fingerprint.clone())),
                    ))
                }
                ScopeDecision::Revoked { label } => Err(TrustError::SignerRevoked {
                    fingerprint: fingerprint.clone(),
                    label,
                }
                .into()),
                ScopeDecision::OutOfScope { label, scope } => {
                    if context.space == SignerSpace::Project {
                        tracing::trace!(fingerprint = %fingerprint, trust_class = "untrusted", "signer outside scope in project space");
                        Ok((
                            TrustClass::Untrusted,
                            Some(SignerFingerprint(fingerprint.clone())),
                        ))
                    } else {
                        Err(TrustError::SignerOutOfScope {
                            fingerprint: fingerprint.clone(),
                            label,
                            scope: scope.describe(),
                            kind: context.kind.to_string(),
                            space: context.space.as_str().to_string(),
                            project: context
                                .project
                                .identity()
                                .map(|identity| format!(" in project {identity}"))
                                .unwrap_or_default(),
                        }
                        .into())
                    }
                }
            }
        }
        None => {
            // Signer not in trust store — we can't verify the signature
            // cryptographically, but the content hash check passed.
            // Mark as Untrusted.
            tracing::trace!(fingerprint = %fingerprint, trust_class = "untrusted", "signer not in trust store");
            Ok((
                TrustClass::Untrusted,
                Some(SignerFingerprint(fingerprint.clone())),
            ))
        }
    }
}

/// Full item verification: takes a ResolvedItem, reads its content,
/// and produces a VerifiedItem.
#[tracing::instrument(
    name = "engine:verify_item",
    skip(item, trust_store),
    fields(canonical_ref = %item.canonical_ref)
)]
pub fn verify_resolved_item(
    item: ResolvedItem,
    trust_store: &TrustStore,
) -> Result<VerifiedItem, EngineError> {
    let content = std::fs::read_to_string(&item.source_path).map_err(|e| {
        EngineError::Internal(format!(
            "failed to read {} for verification: {e}",
            item.source_path.display()
        ))
    })?;
    verify_resolved_item_content(item, &content, trust_store)
}

/// Verify a resolved item against exact source bytes already opened by the
/// engine.
///
/// This keeps the read/hash/verify sequence single-pass for static
/// verification caching. It is crate-visible only: callers outside the engine
/// cannot pair fabricated resolution metadata with arbitrary bytes.
pub(crate) fn verify_resolved_item_content(
    item: ResolvedItem,
    content: &str,
    trust_store: &TrustStore,
) -> Result<VerifiedItem, EngineError> {
    let actual_content_hash = crate::item_resolution::content_hash(content);
    verify_resolved_item_content_with_hash(item, content, &actual_content_hash, trust_store)
}

/// Verify bytes whose whole-file digest was already computed by the engine.
///
/// The supplied digest is not caller authority: it is compared with the
/// resolver's admitted digest before signature verification. Keeping this
/// helper crate-private prevents another crate from substituting unproven
/// bytes while avoiding a second SHA-256 pass in the static-evidence cache.
pub(crate) fn verify_resolved_item_content_with_hash(
    item: ResolvedItem,
    content: &str,
    actual_content_hash: &str,
    trust_store: &TrustStore,
) -> Result<VerifiedItem, EngineError> {
    if actual_content_hash != item.content_hash {
        return Err(EngineError::ContentHashMismatch {
            canonical_ref: item.canonical_ref.to_string(),
            expected: item.content_hash.clone(),
            actual: actual_content_hash.to_owned(),
        });
    }
    tracing::debug!(
        item_ref = %item.canonical_ref,
        has_signature = item.signature_header.is_some(),
        "verifying resolved item"
    );

    match &item.signature_header {
        Some(header) => {
            let context = SignerContext::new(item.source_space, &item.kind, &item.logical_project);
            let (trust_class, signer) = verify_item_signature(
                content,
                header,
                &item.source_format.signature,
                trust_store,
                &context,
            )
            .map_err(|e| patch_canonical_ref(e, &item.canonical_ref.to_string()))?;

            Ok(VerifiedItem {
                resolved: item,
                signer,
                trust_class,
                pinned_version: None,
            })
        }
        None => {
            // No signature — Unsigned
            Ok(VerifiedItem {
                resolved: item,
                signer: None,
                trust_class: TrustClass::Unsigned,
                pinned_version: None,
            })
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Patch the canonical_ref field into verification errors that were
/// created with an empty string placeholder.
pub(crate) fn patch_canonical_ref(err: EngineError, canonical_ref: &str) -> EngineError {
    match err {
        EngineError::SignatureVerificationFailed { reason, .. } => {
            EngineError::SignatureVerificationFailed {
                canonical_ref: canonical_ref.to_owned(),
                reason,
            }
        }
        EngineError::ContentHashMismatch {
            expected, actual, ..
        } => EngineError::ContentHashMismatch {
            canonical_ref: canonical_ref.to_owned(),
            expected,
            actual,
        },
        other => other,
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::SignatureEnvelope;
    use lillux::crypto::{Signer, SigningKey};

    fn sha256_hex(data: &[u8]) -> String {
        lillux::cas::sha256_hex(data)
    }

    fn hash_prefix_envelope() -> SignatureEnvelope {
        SignatureEnvelope {
            prefix: "#".to_owned(),
            suffix: None,
            after_shebang: false,
        }
    }

    fn html_envelope() -> SignatureEnvelope {
        SignatureEnvelope {
            prefix: "<!--".to_owned(),
            suffix: Some("-->".to_owned()),
            after_shebang: false,
        }
    }

    fn shebang_envelope() -> SignatureEnvelope {
        SignatureEnvelope {
            prefix: "#".to_owned(),
            suffix: None,
            after_shebang: true,
        }
    }

    /// Generate a signing key pair and return (signing_key, verifying_key, fingerprint).
    fn gen_key() -> (SigningKey, VerifyingKey, String) {
        let signing_key = SigningKey::from_bytes(&[42u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let fingerprint = compute_fingerprint(&verifying_key);
        (signing_key, verifying_key, fingerprint)
    }

    /// Generate a second, distinct key pair.
    fn gen_key2() -> (SigningKey, VerifyingKey, String) {
        let signing_key = SigningKey::from_bytes(&[99u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let fingerprint = compute_fingerprint(&verifying_key);
        (signing_key, verifying_key, fingerprint)
    }

    fn gen_key3() -> (SigningKey, VerifyingKey, String) {
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let fingerprint = compute_fingerprint(&verifying_key);
        (signing_key, verifying_key, fingerprint)
    }

    /// Build a properly signed file content string.
    fn build_signed_content(
        body: &str,
        signing_key: &SigningKey,
        fingerprint: &str,
        envelope: &SignatureEnvelope,
    ) -> String {
        let content_hash = sha256_hex(body.as_bytes());
        let signature: Signature = signing_key.sign(content_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        let timestamp = "2026-04-10T00:00:00Z";

        let payload = format!("ryeos:signed:{timestamp}:{content_hash}:{sig_b64}:{fingerprint}");

        match &envelope.suffix {
            Some(suffix) => format!("{} {payload} {suffix}\n{body}", envelope.prefix),
            None => format!("{} {payload}\n{body}", envelope.prefix),
        }
    }

    /// Build a properly signed file with shebang.
    fn build_signed_content_with_shebang(
        body: &str,
        signing_key: &SigningKey,
        fingerprint: &str,
        shebang: &str,
    ) -> String {
        let envelope = shebang_envelope();
        // The signature line is line 2, below the shebang; the content hash
        // covers every byte except that line, shebang included.
        let content_hash = sha256_hex(format!("{shebang}\n{body}").as_bytes());
        let signature: Signature = signing_key.sign(content_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        let timestamp = "2026-04-10T00:00:00Z";

        let payload = format!("ryeos:signed:{timestamp}:{content_hash}:{sig_b64}:{fingerprint}");
        format!("{shebang}\n{} {payload}\n{body}", envelope.prefix)
    }

    fn bundle_ctx() -> SignerContext<'static> {
        SignerContext::bundle("tool")
    }

    fn record_for(
        vk: &VerifyingKey,
        role: SignerRole,
        spaces: Vec<SignerSpace>,
        kinds: &[&str],
        projects: &[&str],
        status: SignerStatus,
    ) -> SignerRecord {
        let mut record = SignerRecord::new(
            vk,
            "test-signer",
            role,
            SignerScope {
                spaces,
                kinds: kinds.iter().map(|k| k.to_string()).collect(),
                projects: projects.iter().map(|p| p.parse().unwrap()).collect(),
            },
            status,
        );
        record.event = "e".repeat(64);
        record
    }

    const PROJECT_OWNER: &str =
        "pid:v1:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// A stated project owned by [`PROJECT_OWNER`], one per test name.
    fn stated_project(name: &str) -> &'static LogicalProject {
        fn stated(name: &str) -> LogicalProject {
            LogicalProject::Stated(format!("{PROJECT_OWNER}/{name}").parse().unwrap())
        }
        static P: std::sync::LazyLock<LogicalProject> = std::sync::LazyLock::new(|| stated("p"));
        static ALPHA: std::sync::LazyLock<LogicalProject> =
            std::sync::LazyLock::new(|| stated("alpha"));
        static BETA: std::sync::LazyLock<LogicalProject> =
            std::sync::LazyLock::new(|| stated("beta"));
        static LISTED: std::sync::LazyLock<LogicalProject> =
            std::sync::LazyLock::new(|| stated("listed"));
        static OTHER: std::sync::LazyLock<LogicalProject> =
            std::sync::LazyLock::new(|| stated("other"));
        match name {
            "p" => &P,
            "alpha" => &ALPHA,
            "beta" => &BETA,
            "listed" => &LISTED,
            "other" => &OTHER,
            other => panic!("no fixture project named {other}"),
        }
    }

    fn absent_project() -> &'static LogicalProject {
        &NO_STATED_PROJECT
    }

    static NO_STATED_PROJECT: LogicalProject =
        LogicalProject::Absent(LogicalProjectAbsence::NotStated);

    fn publisher_record(vk: &VerifyingKey, status: SignerStatus) -> SignerRecord {
        record_for(
            vk,
            SignerRole::Publisher,
            vec![SignerSpace::Bundle],
            &["*"],
            &[],
            status,
        )
    }

    fn write_identity(app_root: &Path, node_vk: &VerifyingKey) {
        let dir = app_root.join(".ai/node/identity");
        std::fs::create_dir_all(&dir).unwrap();
        let doc = serde_json::json!({
            "kind": "identity/v1",
            "principal_id": format!("fp:{}", compute_fingerprint(node_vk)),
            "signing_key": format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(node_vk.as_bytes())
            ),
        });
        std::fs::write(dir.join("public-identity.json"), doc.to_string()).unwrap();
    }

    fn write_record(app_root: &Path, record: &SignerRecord, signer: &SigningKey) -> PathBuf {
        let dir = signers_dir(app_root);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(record.file_name());
        std::fs::write(&path, record.render_signed(signer).unwrap()).unwrap();
        path
    }

    fn signed_with_header(
        sk: &SigningKey,
        fp: &str,
    ) -> (String, SignatureEnvelope, SignatureHeader) {
        let envelope = hash_prefix_envelope();
        let content = build_signed_content("print('x')\n", sk, fp, &envelope);
        let header = crate::item_resolution::parse_signature_header(&content, &envelope).unwrap();
        (content, envelope, header)
    }

    // ── TrustStore tests ────────────────────────────────────────────

    #[test]
    fn empty_trust_store() {
        let store = TrustStore::empty();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert!(!store.is_trusted("anything"));
    }

    #[test]
    fn trust_store_from_signers() {
        let (_, vk, fp) = gen_key();
        let store = TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp.clone(),
            verifying_key: vk,
            label: Some("test".into()),
        }]);
        assert_eq!(store.len(), 1);
        assert!(store.is_trusted(&fp));
        assert!(!store.is_trusted("other"));
    }

    #[test]
    fn registry_record_signed_by_node_is_trusted() {
        let (node_sk, node_vk, node_fp) = gen_key();
        let (_, pub_vk, pub_fp) = gen_key2();
        let root = tempfile::tempdir().unwrap();
        write_identity(root.path(), &node_vk);
        write_record(
            root.path(),
            &publisher_record(&pub_vk, SignerStatus::Active),
            &node_sk,
        );

        let store = TrustStore::load_node_registry(root.path()).unwrap();
        assert!(store.is_trusted(&pub_fp));
        assert!(store.is_trusted(&node_fp), "node key is the implicit root");
        assert_eq!(store.signer(&pub_fp).unwrap().role, SignerRole::Publisher);
        assert!(store.signer(&node_fp).is_none(), "node root has no record");
    }

    #[test]
    fn missing_registry_directory_trusts_only_the_node_key() {
        let (_, node_vk, node_fp) = gen_key();
        let root = tempfile::tempdir().unwrap();
        write_identity(root.path(), &node_vk);
        let store = TrustStore::load_node_registry(root.path()).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.is_trusted(&node_fp));
    }

    #[test]
    fn unsigned_record_is_refused_with_code() {
        let (_, node_vk, _) = gen_key();
        let (_, pub_vk, _) = gen_key2();
        let root = tempfile::tempdir().unwrap();
        write_identity(root.path(), &node_vk);
        let record = publisher_record(&pub_vk, SignerStatus::Active);
        let dir = signers_dir(root.path());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(record.file_name());
        std::fs::write(&path, serde_yaml::to_string(&record).unwrap()).unwrap();

        let err = TrustStore::load_node_registry(root.path()).unwrap_err();
        assert_eq!(err.code(), "trust_record_unsigned");
        assert!(err.to_string().contains(&path.display().to_string()));
    }

    #[test]
    fn self_signed_record_is_refused() {
        let (_, node_vk, _) = gen_key();
        let (pub_sk, pub_vk, _) = gen_key2();
        let root = tempfile::tempdir().unwrap();
        write_identity(root.path(), &node_vk);
        write_record(
            root.path(),
            &publisher_record(&pub_vk, SignerStatus::Active),
            &pub_sk,
        );
        let err = TrustStore::load_node_registry(root.path()).unwrap_err();
        assert_eq!(err.code(), "trust_record_wrong_signer");
    }

    #[test]
    fn record_signed_by_other_pinned_key_is_refused() {
        let (node_sk, node_vk, _) = gen_key();
        let (other_sk, other_vk, _) = gen_key2();
        let (_, pub_vk, _) = gen_key3();
        let root = tempfile::tempdir().unwrap();
        write_identity(root.path(), &node_vk);
        let other = record_for(
            &other_vk,
            SignerRole::Operator,
            vec![SignerSpace::Project, SignerSpace::Node],
            &["*"],
            &[],
            SignerStatus::Active,
        );
        write_record(root.path(), &other, &node_sk);
        write_record(
            root.path(),
            &publisher_record(&pub_vk, SignerStatus::Active),
            &other_sk,
        );
        let err = TrustStore::load_node_registry(root.path()).unwrap_err();
        assert_eq!(err.code(), "trust_record_wrong_signer");
    }

    #[test]
    fn edited_record_is_refused_as_tampered() {
        let (node_sk, node_vk, _) = gen_key();
        let (_, pub_vk, _) = gen_key2();
        let root = tempfile::tempdir().unwrap();
        write_identity(root.path(), &node_vk);
        let path = write_record(
            root.path(),
            &publisher_record(&pub_vk, SignerStatus::Active),
            &node_sk,
        );
        let edited = std::fs::read_to_string(&path)
            .unwrap()
            .replace("publisher", "operator");
        std::fs::write(&path, edited).unwrap();
        let err = TrustStore::load_node_registry(root.path()).unwrap_err();
        assert_eq!(err.code(), "trust_record_tampered");
    }

    #[test]
    fn raw_pub_file_in_registry_is_refused() {
        let (_, node_vk, _) = gen_key();
        let (_, pub_vk, pub_fp) = gen_key2();
        let root = tempfile::tempdir().unwrap();
        write_identity(root.path(), &node_vk);
        let dir = signers_dir(root.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{pub_fp}.pub")),
            base64::engine::general_purpose::STANDARD.encode(pub_vk.as_bytes()),
        )
        .unwrap();
        let err = TrustStore::load_node_registry(root.path()).unwrap_err();
        assert_eq!(err.code(), "trust_record_name_mismatch");
    }

    #[test]
    fn record_name_must_match_fingerprint() {
        let (node_sk, node_vk, _) = gen_key();
        let (_, pub_vk, _) = gen_key2();
        let root = tempfile::tempdir().unwrap();
        write_identity(root.path(), &node_vk);
        let record = publisher_record(&pub_vk, SignerStatus::Active);
        let dir = signers_dir(root.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{}.yaml", "0".repeat(64))),
            record.render_signed(&node_sk).unwrap(),
        )
        .unwrap();
        let err = TrustStore::load_node_registry(root.path()).unwrap_err();
        assert_eq!(err.code(), "trust_record_name_mismatch");
    }

    #[test]
    fn unknown_schema_is_refused() {
        let (node_sk, node_vk, _) = gen_key();
        let (_, pub_vk, _) = gen_key2();
        let root = tempfile::tempdir().unwrap();
        write_identity(root.path(), &node_vk);
        let mut record = publisher_record(&pub_vk, SignerStatus::Active);
        record.schema = "ryeos/trust-signer/v0".to_string();
        write_record(root.path(), &record, &node_sk);
        // Only that signer is unusable: the registry still loads, without it,
        // and the refusal is reported by name.
        let store = TrustStore::load_node_registry(root.path()).unwrap();
        assert!(store.signer(&record.fingerprint).is_none());
        let (records, refused) =
            load_signer_records_reporting(&signers_dir(root.path()), &node_vk).unwrap();
        assert!(records.is_empty());
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].code(), "trust_record_schema_unsupported");
    }

    #[test]
    fn a_v1_record_without_project_entries_reads_and_one_with_entries_is_refused_alone() {
        let (node_sk, node_vk, _) = gen_key();
        let (_, publisher_vk, _) = gen_key2();
        let author_vk = SigningKey::from_bytes(&[77; 32]).verifying_key();
        let root = tempfile::tempdir().unwrap();
        write_identity(root.path(), &node_vk);
        let mut unscoped = publisher_record(&publisher_vk, SignerStatus::Active);
        unscoped.schema = SIGNER_RECORD_SCHEMA_V1.to_string();
        write_record(root.path(), &unscoped, &node_sk);
        let mut scoped = record_for(
            &author_vk,
            SignerRole::ProjectAuthor,
            vec![SignerSpace::Project],
            &["*"],
            &["own:alpha"],
            SignerStatus::Active,
        );
        scoped.schema = SIGNER_RECORD_SCHEMA_V1.to_string();
        write_record(root.path(), &scoped, &node_sk);
        let (records, refused) =
            load_signer_records_reporting(&signers_dir(root.path()), &node_vk).unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.fingerprint.as_str())
                .collect::<Vec<_>>(),
            vec![unscoped.fingerprint.as_str()]
        );
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].code(), "trust_record_schema_unsupported");
        assert!(refused[0].to_string().contains(&scoped.file_name()));
    }

    #[test]
    fn publisher_scope_bundle_only_makes_project_item_untrusted() {
        let (sk, vk, fp) = gen_key();
        let (_, node_vk, _) = gen_key2();
        let store =
            TrustStore::from_registry(node_vk, vec![publisher_record(&vk, SignerStatus::Active)])
                .unwrap();
        let (content, envelope, header) = signed_with_header(&sk, &fp);

        let (bundle_class, _) =
            verify_item_signature(&content, &header, &envelope, &store, &bundle_ctx()).unwrap();
        assert_eq!(bundle_class, TrustClass::Trusted);

        let ctx = SignerContext::project("tool", stated_project("p"));
        let (project_class, _) =
            verify_item_signature(&content, &header, &envelope, &store, &ctx).unwrap();
        assert_eq!(project_class, TrustClass::Untrusted);
    }

    #[test]
    fn out_of_scope_signer_in_bundle_space_is_refused_with_code() {
        let (sk, vk, fp) = gen_key();
        let (_, node_vk, _) = gen_key2();
        let record = record_for(
            &vk,
            SignerRole::Operator,
            vec![SignerSpace::Project, SignerSpace::Node],
            &["*"],
            &[],
            SignerStatus::Active,
        );
        let store = TrustStore::from_registry(node_vk, vec![record]).unwrap();
        let (content, envelope, header) = signed_with_header(&sk, &fp);
        let err =
            verify_item_signature(&content, &header, &envelope, &store, &bundle_ctx()).unwrap_err();
        let EngineError::Trust(trust) = &err else {
            panic!("expected typed trust error, got {err:?}");
        };
        assert_eq!(trust.code(), "signer_out_of_scope");
        let text = err.to_string();
        assert!(text.contains(&fp));
        assert!(text.contains("test-signer"));
        assert!(text.contains("ryeos trust scope"));
    }

    #[test]
    fn kind_scope_limits_signed_kinds() {
        let (sk, vk, fp) = gen_key();
        let (_, node_vk, _) = gen_key2();
        let record = record_for(
            &vk,
            SignerRole::ProjectAuthor,
            vec![SignerSpace::Project],
            &["knowledge"],
            &[],
            SignerStatus::Active,
        );
        let store = TrustStore::from_registry(node_vk, vec![record]).unwrap();
        let (content, envelope, header) = signed_with_header(&sk, &fp);
        let ok = SignerContext::project("knowledge", absent_project());
        let (class, _) = verify_item_signature(&content, &header, &envelope, &store, &ok).unwrap();
        assert_eq!(class, TrustClass::Trusted);
        let bad = SignerContext::project("tool", absent_project());
        let (class, _) = verify_item_signature(&content, &header, &envelope, &store, &bad).unwrap();
        assert_eq!(class, TrustClass::Untrusted);
    }

    #[test]
    fn authoring_signer_with_empty_scope_is_untrusted() {
        let (sk, vk, fp) = gen_key();
        let (_, node_vk, _) = gen_key2();
        let record = SignerRecord::new(
            &vk,
            "authoring",
            SignerRole::Authoring,
            SignerScope::authoring_default(),
            SignerStatus::Active,
        );
        assert!(record.scope.kinds.is_empty());
        record.scope.validate().unwrap();
        let store = TrustStore::from_registry(node_vk, vec![record]).unwrap();
        let (content, envelope, header) = signed_with_header(&sk, &fp);
        for kind in ["knowledge", "directive", "tool"] {
            let ctx = SignerContext::project(kind, stated_project("p"));
            let (class, _) =
                verify_item_signature(&content, &header, &envelope, &store, &ctx).unwrap();
            assert_eq!(class, TrustClass::Untrusted, "{kind}");
        }
    }

    /// A launch's signer list for a project with no endorsement attaches no
    /// snapshot; an authoring-role entry without one means nothing is
    /// endorsed, so every loader built from it refuses the key's items.
    #[test]
    fn authoring_entry_without_an_endorsement_snapshot_is_untrusted_in_project_space() {
        let (sk, vk, fp) = gen_key();
        let (_, node_vk, _) = gen_key2();
        let record = SignerRecord::new(
            &vk,
            "authoring",
            SignerRole::Authoring,
            SignerScope::authoring_default(),
            SignerStatus::Active,
        );
        let live = TrustStore::from_registry(node_vk, vec![record]).unwrap();
        let entries = live.launch_signer_entries(&live, stated_project("p"));
        assert!(entries.iter().all(|entry| entry.endorsements.is_none()));
        let store = TrustStore::from_signer_entries(&entries).unwrap();
        let (content, envelope, header) = signed_with_header(&sk, &fp);
        for kind in ["knowledge", "directive", "tool", "config"] {
            for root in [stated_project("p"), absent_project()] {
                let ctx = SignerContext::project(kind, root);
                let (class, _) =
                    verify_item_signature(&content, &header, &envelope, &store, &ctx).unwrap();
                assert_eq!(class, TrustClass::Untrusted, "{kind} {root}");
            }
        }
    }

    #[test]
    fn authoring_record_with_a_widened_scope_is_refused_at_load() {
        let (_, vk, fp) = gen_key();
        let (_, node_vk, _) = gen_key2();
        let record = record_for(
            &vk,
            SignerRole::Authoring,
            vec![SignerSpace::Project],
            &["knowledge"],
            &[],
            SignerStatus::Active,
        );
        let error = TrustStore::from_registry(node_vk, vec![record]).unwrap_err();
        assert_eq!(error.code(), "trust_authoring_scope_retired");
        let text = error.to_string();
        assert!(text.contains(&fp), "{text}");
        assert!(text.contains("ryeos trust scope"), "{text}");
        assert!(text.contains("ryeos endorse"), "{text}");
    }

    #[test]
    fn a_revoked_authoring_record_with_a_widened_scope_still_loads() {
        let (_, vk, _) = gen_key();
        let (_, node_vk, _) = gen_key2();
        let record = record_for(
            &vk,
            SignerRole::Authoring,
            vec![SignerSpace::Project],
            &["knowledge"],
            &[],
            SignerStatus::Revoked,
        );
        TrustStore::from_registry(node_vk, vec![record]).unwrap();
    }

    #[test]
    fn node_key_signed_project_item_is_out_of_scope() {
        // The signing key is the registry's node root.
        let (sk, vk, fp) = gen_key();
        let store = TrustStore::from_registry(vk, Vec::new()).unwrap();
        let (content, envelope, header) = signed_with_header(&sk, &fp);
        let project = SignerContext::project("knowledge", stated_project("p"));
        let (class, _) =
            verify_item_signature(&content, &header, &envelope, &store, &project).unwrap();
        assert_eq!(class, TrustClass::Untrusted);
        let node = SignerContext::node("node");
        let (class, _) =
            verify_item_signature(&content, &header, &envelope, &store, &node).unwrap();
        assert_eq!(class, TrustClass::Trusted);
    }

    #[test]
    fn project_author_scope_limits_to_listed_project() {
        let (sk, vk, fp) = gen_key();
        let (_, node_vk, _) = gen_key2();
        let record = record_for(
            &vk,
            SignerRole::ProjectAuthor,
            vec![SignerSpace::Project],
            &["*"],
            &["pid:v1:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/listed"],
            SignerStatus::Active,
        );
        let store = TrustStore::from_registry(node_vk, vec![record]).unwrap();
        let (content, envelope, header) = signed_with_header(&sk, &fp);

        let listed = SignerContext::project("tool", stated_project("listed"));
        let (class, _) =
            verify_item_signature(&content, &header, &envelope, &store, &listed).unwrap();
        assert_eq!(class, TrustClass::Trusted);
        let other = SignerContext::project("tool", stated_project("other"));
        let (class, _) =
            verify_item_signature(&content, &header, &envelope, &store, &other).unwrap();
        assert_eq!(class, TrustClass::Untrusted);
        let none = SignerContext::project("tool", absent_project());
        let (class, _) =
            verify_item_signature(&content, &header, &envelope, &store, &none).unwrap();
        assert_eq!(class, TrustClass::Untrusted);
    }

    #[test]
    fn scoped_entries_match_only_a_stated_identity_and_own_binds_the_signers_principal() {
        let owner_text = format!("pid:v1:{}", "a".repeat(64));
        let other_text = format!("pid:v1:{}", "b".repeat(64));
        let owner: PrincipalId = owner_text.parse().unwrap();
        let signer = Some(&owner);
        let (signer_text, other) = (owner_text.as_str(), other_text.as_str());
        let stated = |owner: &str, name: &str| {
            LogicalProject::Stated(format!("{owner}/{name}").parse().unwrap())
        };
        let scoped = SignerScope {
            spaces: vec![SignerSpace::Project],
            kinds: vec!["*".to_owned()],
            projects: vec!["own:demo".parse().unwrap()],
        };
        let space = SignerSpace::Project;
        assert!(scoped.admits(space, "tool", &stated(signer_text, "demo"), signer));
        // Another owner's project of the same name is never matched.
        assert!(!scoped.admits(space, "tool", &stated(other, "demo"), signer));
        assert!(!scoped.admits(space, "tool", &stated(signer_text, "other"), signer));
        // A key that speaks for no principal (retired, revoked) matches no
        // `own:` entry.
        assert!(!scoped.admits(space, "tool", &stated(signer_text, "demo"), None));
        // A scoped signer never matches an absent identity.
        for reason in [
            LogicalProjectAbsence::NotRegistered,
            LogicalProjectAbsence::ForeignHeadNotHeld,
            LogicalProjectAbsence::NotStated,
        ] {
            assert!(!scoped.admits(space, "tool", &LogicalProject::Absent(reason), signer));
        }
        // An unscoped signer admits every project context, absent included.
        let unscoped = SignerScope {
            projects: Vec::new(),
            ..scoped.clone()
        };
        assert!(unscoped.admits(
            space,
            "tool",
            &LogicalProject::Absent(LogicalProjectAbsence::NotRegistered),
            signer
        ));
        // A path is not a scope.
        assert!("/work/listed".parse::<ProjectScopeEntry>().is_err());
        assert!(format!("{other}/demo").parse::<ProjectScopeEntry>().is_ok());
        // A key fingerprint is never an owner.
        assert!(
            format!("fp:{}/demo", "b".repeat(64))
                .parse::<ProjectScopeEntry>()
                .is_err()
        );
    }

    /// The verdict for a signer outside its scope: untrusted, or refused as
    /// out of scope. Any other refusal is a broken fixture.
    fn assert_untrusted_or_out_of_scope<E: std::fmt::Debug>(verdict: Result<TrustClass, E>) {
        match verdict {
            Ok(class) => assert_eq!(class, TrustClass::Untrusted),
            Err(error) => {
                let text = format!("{error:?}");
                assert!(text.contains("SignerOutOfScope"), "{text}");
            }
        }
    }

    #[test]
    fn a_project_named_core_and_the_installed_core_bundle_are_judged_apart() {
        // A project whose manifest says `name: core` is pid:v1:<owner>/core.
        // The installed bundle `core` is judged in bundle space by its
        // publisher. Neither trust ever stands in for the other, and nothing
        // compares the two names: a project identity is a principal plus a
        // name, never a bundle name.
        let (author_sk, author_vk, author_fp) = gen_key();
        let (_, node_vk, _) = gen_key2();
        let publisher_sk = SigningKey::from_bytes(&[11u8; 32]);
        let publisher_vk = publisher_sk.verifying_key();
        let publisher_fp = compute_fingerprint(&publisher_vk);
        let core_project = format!("{PROJECT_OWNER}/core");
        let store = TrustStore::from_registry(
            node_vk,
            vec![
                record_for(
                    &author_vk,
                    SignerRole::ProjectAuthor,
                    vec![SignerSpace::Project],
                    &["*"],
                    &[core_project.as_str()],
                    SignerStatus::Active,
                ),
                publisher_record(&publisher_vk, SignerStatus::Active),
            ],
        )
        .unwrap()
        .with_succession_chain(&[]);
        let project = LogicalProject::Stated(core_project.parse().unwrap());
        let in_project = SignerContext::project("tool", &project);

        // The project author is trusted in its project, never for the bundle.
        let (content, envelope, header) = signed_with_header(&author_sk, &author_fp);
        let (class, _) =
            verify_item_signature(&content, &header, &envelope, &store, &in_project).unwrap();
        assert_eq!(class, TrustClass::Trusted);
        // Outside its scope the signer is refused, never trusted.
        let refused = verify_item_signature(&content, &header, &envelope, &store, &bundle_ctx())
            .map(|(class, _)| class);
        assert_untrusted_or_out_of_scope(refused);

        // The core bundle's publisher is trusted in bundle space, never as
        // the project named core.
        let (content, envelope, header) = signed_with_header(&publisher_sk, &publisher_fp);
        let (class, _) =
            verify_item_signature(&content, &header, &envelope, &store, &bundle_ctx()).unwrap();
        assert_eq!(class, TrustClass::Trusted);
        let refused = verify_item_signature(&content, &header, &envelope, &store, &in_project)
            .map(|(class, _)| class);
        assert_untrusted_or_out_of_scope(refused);

        // `core` alone is not a project identity, so a bundle name can never
        // be read as one.
        assert!(
            "core"
                .parse::<ryeos_state::project_identity::ProjectIdentity>()
                .is_err()
        );
    }

    #[test]
    fn own_scope_verification_follows_retired_verifies_and_revoked_does_not() {
        let (root_sk, root_vk, root_fp) = gen_key();
        let (_, node_vk, _) = gen_key2();
        let successor_vk = SigningKey::from_bytes(&[7u8; 32]).verifying_key();
        let successor_fp = compute_fingerprint(&successor_vk);
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
        let store_with = |root_status: SignerStatus| {
            TrustStore::from_registry(
                node_vk,
                vec![
                    record_for(
                        &root_vk,
                        SignerRole::ProjectAuthor,
                        vec![SignerSpace::Project],
                        &["*"],
                        &["own:demo"],
                        root_status,
                    ),
                    record_for(
                        &successor_vk,
                        SignerRole::ProjectAuthor,
                        vec![SignerSpace::Project],
                        &["*"],
                        &["own:demo"],
                        SignerStatus::Active,
                    ),
                ],
            )
            .unwrap()
            .with_succession_chain(std::slice::from_ref(&succeeded))
        };
        let retired = store_with(SignerStatus::Retired);
        let principal = retired.signing_principal_of(&root_fp).unwrap();
        assert_eq!(
            retired.signing_principal_of(&successor_fp),
            Some(principal.clone())
        );
        let demo = LogicalProject::Stated(format!("{principal}/demo").parse().unwrap());
        let (content, envelope, header) = signed_with_header(&root_sk, &root_fp);
        // A retired key's own:-scoped items still verify after rotation.
        let ctx = SignerContext::project("tool", &demo);
        let (class, _) =
            verify_item_signature(&content, &header, &envelope, &retired, &ctx).unwrap();
        assert_eq!(class, TrustClass::Trusted);
        // Acting is separate: the retired key owns nothing.
        assert_eq!(
            retired.owner_for(&root_fp).unwrap_err().code(),
            "owner_key_not_active"
        );
        assert!(!retired.speaks_for(&root_fp, &principal));
        assert!(retired.speaks_for(&successor_fp, &principal));
        // A revoked key's items verify nowhere, and it has no signing
        // principal for own:.
        let revoked = store_with(SignerStatus::Revoked);
        assert_eq!(revoked.signing_principal_of(&root_fp), None);
        assert!(verify_item_signature(&content, &header, &envelope, &revoked, &ctx).is_err());
    }

    #[test]
    fn launch_entries_resolve_scoped_signers_per_launch_identity_only() {
        let (sk, vk, fp) = gen_key();
        let (_, node_vk, _) = gen_key2();
        let alpha = format!("{PROJECT_OWNER}/alpha");
        let record = record_for(
            &vk,
            SignerRole::ProjectAuthor,
            vec![SignerSpace::Project, SignerSpace::Bundle],
            &["*"],
            &[alpha.as_str()],
            SignerStatus::Active,
        );
        let store = TrustStore::from_registry(node_vk, vec![record]).unwrap();
        let entry_for = |project: &LogicalProject| {
            store
                .launch_signer_entries(&store, project)
                .into_iter()
                .find(|entry| entry.fingerprint == fp)
        };
        // A launch of the scoped project carries the signer unscoped for it.
        let own = entry_for(stated_project("alpha")).unwrap();
        assert!(own.scope.projects.is_empty());
        assert!(own.scope.spaces.contains(&SignerSpace::Project));
        // A child or second launch into another project gets no widened entry:
        // the signer loses project space there, keeping only its other spaces.
        for other in [stated_project("beta"), absent_project()] {
            let entry = entry_for(other).unwrap();
            assert!(
                !entry.scope.spaces.contains(&SignerSpace::Project),
                "{other}"
            );
            assert_eq!(entry.scope.spaces, vec![SignerSpace::Bundle]);
            // A runtime rebuilt from that entry never trusts the signer's
            // project items, whatever project context it judges.
            let rebuilt = TrustStore::from_signer_entries(&[entry]).unwrap();
            let (content, envelope, header) = signed_with_header(&sk, &fp);
            let ctx = SignerContext::project("tool", stated_project("alpha"));
            let (class, _) =
                verify_item_signature(&content, &header, &envelope, &rebuilt, &ctx).unwrap();
            assert_eq!(class, TrustClass::Untrusted, "{other}");
        }
        // The resolution is computed afresh for every launch from the store,
        // not cached: the same store still widens for alpha afterwards.
        assert!(
            entry_for(stated_project("alpha"))
                .unwrap()
                .scope
                .projects
                .is_empty()
        );
    }

    #[test]
    fn revoked_signer_does_not_verify() {
        let (sk, vk, fp) = gen_key();
        let (_, node_vk, _) = gen_key2();
        let store =
            TrustStore::from_registry(node_vk, vec![publisher_record(&vk, SignerStatus::Revoked)])
                .unwrap();
        assert!(!store.is_trusted(&fp));
        let (content, envelope, header) = signed_with_header(&sk, &fp);
        let err =
            verify_item_signature(&content, &header, &envelope, &store, &bundle_ctx()).unwrap_err();
        assert!(matches!(
            err,
            EngineError::Trust(TrustError::SignerRevoked { .. })
        ));
    }

    #[test]
    fn retired_signer_still_verifies() {
        let (sk, vk, fp) = gen_key();
        let (_, node_vk, _) = gen_key2();
        let store =
            TrustStore::from_registry(node_vk, vec![publisher_record(&vk, SignerStatus::Retired)])
                .unwrap();
        let (content, envelope, header) = signed_with_header(&sk, &fp);
        let (class, _) =
            verify_item_signature(&content, &header, &envelope, &store, &bundle_ctx()).unwrap();
        assert_eq!(class, TrustClass::Trusted);
    }

    #[test]
    fn registry_digest_changes_with_status() {
        let (_, vk, _) = gen_key();
        let (_, node_vk, _) = gen_key2();
        let active =
            TrustStore::from_registry(node_vk, vec![publisher_record(&vk, SignerStatus::Active)])
                .unwrap();
        let retired =
            TrustStore::from_registry(node_vk, vec![publisher_record(&vk, SignerStatus::Retired)])
                .unwrap();
        assert_ne!(active.digest(), retired.digest());
        assert_ne!(active.fingerprint(), retired.fingerprint());
    }

    #[test]
    fn node_documents_verify_only_under_the_node_key() {
        let (node_sk, node_vk, node_fp) = gen_key();
        let (pub_sk, _, pub_fp) = gen_key2();
        let envelope = hash_prefix_envelope();
        let signed = build_signed_content("a: 1\n", &node_sk, &node_fp, &envelope);
        let header = crate::item_resolution::parse_signature_header(&signed, &envelope).unwrap();
        verify_node_signed_document(&signed, &header, &envelope, &node_vk).unwrap();

        let foreign = build_signed_content("a: 1\n", &pub_sk, &pub_fp, &envelope);
        let header = crate::item_resolution::parse_signature_header(&foreign, &envelope).unwrap();
        assert!(verify_node_signed_document(&foreign, &header, &envelope, &node_vk).is_err());
    }

    #[test]
    fn event_chain_detects_gap_and_links() {
        let (node_sk, node_vk, _) = gen_key();
        let root = tempfile::tempdir().unwrap();
        let dir = events_dir(root.path());
        std::fs::create_dir_all(&dir).unwrap();
        let first = TrustEvent {
            schema: TRUST_EVENT_SCHEMA.to_string(),
            seq: 1,
            previous: None,
            kind: TrustEventKind::Init,
            signer: "a".repeat(64),
            record_digest: "b".repeat(64),
            request_digest: "c".repeat(64),
            operator: "d".repeat(64),
            predecessor: None,
        };
        let second = TrustEvent {
            seq: 2,
            previous: Some(first.digest()),
            kind: TrustEventKind::Pinned,
            ..first.clone()
        };
        for event in [&first, &second] {
            std::fs::write(
                dir.join(event.file_name()),
                event.render_signed(&node_sk).unwrap(),
            )
            .unwrap();
        }
        let chain = load_event_chain(&dir, &node_vk).unwrap();
        assert_eq!(chain.len(), 2);

        std::fs::remove_file(dir.join(first.file_name())).unwrap();
        let err = load_event_chain(&dir, &node_vk).unwrap_err();
        assert_eq!(err.code(), "trust_event_chain_broken");
    }

    #[test]
    fn event_chain_timed_returns_signing_times_and_first_pin_time() {
        let (node_sk, node_vk, _) = gen_key();
        let root = tempfile::tempdir().unwrap();
        let dir = events_dir(root.path());
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |seq: u64, previous: Option<String>, kind, signer: &str| TrustEvent {
            schema: TRUST_EVENT_SCHEMA.to_string(),
            seq,
            previous,
            kind,
            signer: signer.to_string(),
            record_digest: "b".repeat(64),
            request_digest: "c".repeat(64),
            operator: "d".repeat(64),
            predecessor: None,
        };
        let key = "a".repeat(64);
        let other = "e".repeat(64);
        let e1 = mk(1, None, TrustEventKind::Pinned, &key);
        let e2 = mk(2, Some(e1.digest()), TrustEventKind::Pinned, &other);
        let e3 = mk(3, Some(e2.digest()), TrustEventKind::Retired, &key);
        let e4 = mk(4, Some(e3.digest()), TrustEventKind::Pinned, &key);
        let times = [
            "2026-01-01T00:00:00Z",
            "2026-02-02T00:00:00Z",
            "2026-03-03T00:00:00Z",
            "2026-04-04T00:00:00Z",
        ];
        for (event, at) in [&e1, &e2, &e3, &e4].into_iter().zip(times) {
            let body = serde_yaml::to_string(event).unwrap();
            std::fs::write(
                dir.join(event.file_name()),
                lillux::signature::sign_content_at(&body, &node_sk, "#", None, at),
            )
            .unwrap();
        }
        let chain = load_event_chain_timed(&dir, &node_vk).unwrap();
        let got: Vec<&str> = chain.iter().map(|(_, at)| at.as_str()).collect();
        assert_eq!(got, times);
        // Retired then pinned again: the key has been trusted since its first pin.
        assert_eq!(key_pinned_at(&chain, &key).as_deref(), Some(times[0]));
        assert_eq!(key_pinned_at(&chain, &other).as_deref(), Some(times[1]));
        assert_eq!(key_pinned_at(&chain, "unknown"), None);
    }

    #[test]
    fn event_chain_timed_refuses_an_event_without_a_signing_time() {
        let (node_sk, node_vk, _) = gen_key();
        let root = tempfile::tempdir().unwrap();
        let dir = events_dir(root.path());
        std::fs::create_dir_all(&dir).unwrap();
        let event = TrustEvent {
            schema: TRUST_EVENT_SCHEMA.to_string(),
            seq: 1,
            previous: None,
            kind: TrustEventKind::Init,
            signer: "a".repeat(64),
            record_digest: "b".repeat(64),
            request_digest: "c".repeat(64),
            operator: "d".repeat(64),
            predecessor: None,
        };
        let body = serde_yaml::to_string(&event).unwrap();
        std::fs::write(
            dir.join(event.file_name()),
            lillux::signature::sign_content_at(&body, &node_sk, "#", None, ""),
        )
        .unwrap();
        assert!(load_event_chain_timed(&dir, &node_vk).is_err());
    }

    #[test]
    fn succession_requires_old_key_signature() {
        let (old_sk, old_vk, _) = gen_key();
        let (other_sk, _, _) = gen_key2();
        let (_, new_vk, _) = gen_key3();
        let old_record = publisher_record(&old_vk, SignerStatus::Active);
        let statement =
            SuccessionStatement::new(&old_record.fingerprint, &new_vk, "2026-11-13T00:00:00Z");
        let good = statement.render_signed(&old_sk).unwrap();
        SuccessionStatement::parse_and_verify(&good, &old_record).unwrap();
        let forged = statement.render_signed(&other_sk).unwrap();
        assert!(SuccessionStatement::parse_and_verify(&forged, &old_record).is_err());
    }

    // ── Content-hash tests ──────────────────────────────────────────

    #[test]
    fn content_hash_after_sig_hash_prefix() {
        let content = "# ryeos:signed:2026-04-10T00:00:00Z:HASH:SIG:FP\nprint('hello')\n";
        let envelope = hash_prefix_envelope();

        let hash = content_hash_after_signature(content, &envelope).unwrap();
        let expected = sha256_hex(b"print('hello')\n");
        assert_eq!(hash, expected);
    }

    #[test]
    fn content_hash_after_sig_html_prefix() {
        let content = "<!-- ryeos:signed:2026-04-10T00:00:00Z:HASH:SIG:FP -->\n# Directive\n";
        let envelope = html_envelope();

        let hash = content_hash_after_signature(content, &envelope).unwrap();
        let expected = sha256_hex(b"# Directive\n");
        assert_eq!(hash, expected);
    }

    #[test]
    fn content_hash_after_sig_with_shebang() {
        let content = "#!/usr/bin/env python3\n# ryeos:signed:2026-04-10T00:00:00Z:HASH:SIG:FP\nprint('hi')\n";
        let envelope = shebang_envelope();

        let hash = content_hash_after_signature(content, &envelope).unwrap();
        let expected = sha256_hex(b"#!/usr/bin/env python3\nprint('hi')\n");
        assert_eq!(hash, expected);
    }

    #[test]
    fn content_hash_no_signature_line() {
        let content = "print('hello')\n";
        let envelope = hash_prefix_envelope();

        assert!(content_hash_after_signature(content, &envelope).is_none());
    }

    // ── Signature verification tests ────────────────────────────────

    fn unscoped_store(vk: VerifyingKey, fp: &str) -> TrustStore {
        TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp.to_string(),
            verifying_key: vk,
            label: None,
        }])
    }

    #[test]
    fn verify_valid_signature_trusted() {
        let (sk, vk, fp) = gen_key();
        let (content, envelope, header) = signed_with_header(&sk, &fp);
        let store = unscoped_store(vk, &fp);

        let (trust_class, signer) =
            verify_item_signature(&content, &header, &envelope, &store, &bundle_ctx()).unwrap();
        assert_eq!(trust_class, TrustClass::Trusted);
        assert_eq!(signer.unwrap().0, fp);
    }

    #[test]
    fn verify_valid_signature_untrusted_signer() {
        let (sk, _, fp) = gen_key();
        let (content, envelope, header) = signed_with_header(&sk, &fp);

        // Empty trust store — signer not trusted
        let store = TrustStore::empty();

        let (trust_class, signer) =
            verify_item_signature(&content, &header, &envelope, &store, &bundle_ctx()).unwrap();
        assert_eq!(trust_class, TrustClass::Untrusted);
        assert_eq!(signer.unwrap().0, fp);
    }

    #[test]
    fn verify_tampered_content_fails() {
        let (sk, vk, fp) = gen_key();
        let (mut content, envelope, header) = signed_with_header(&sk, &fp);

        // Tamper with the body
        content.push_str("# injected malicious code\n");

        let store = unscoped_store(vk, &fp);

        let err =
            verify_item_signature(&content, &header, &envelope, &store, &bundle_ctx()).unwrap_err();
        assert!(
            matches!(err, EngineError::ContentHashMismatch { .. }),
            "expected ContentHashMismatch, got: {err:?}"
        );
    }

    #[test]
    fn verify_wrong_signer_key_fails() {
        let (sk, _, fp) = gen_key();
        let (_, vk2, _) = gen_key2();
        let (content, envelope, header) = signed_with_header(&sk, &fp);

        // Trust store has the fingerprint but mapped to a DIFFERENT key
        let store = unscoped_store(vk2, &fp);

        let err =
            verify_item_signature(&content, &header, &envelope, &store, &bundle_ctx()).unwrap_err();
        assert!(
            matches!(err, EngineError::SignatureVerificationFailed { .. }),
            "expected SignatureVerificationFailed, got: {err:?}"
        );
    }

    #[test]
    fn verify_html_envelope_signature() {
        let (sk, vk, fp) = gen_key();
        let body = "# My Directive\n\nSome content.\n";
        let envelope = html_envelope();
        let content = build_signed_content(body, &sk, &fp, &envelope);

        let header = crate::item_resolution::parse_signature_header(&content, &envelope).unwrap();
        let store = unscoped_store(vk, &fp);

        let (trust_class, signer) =
            verify_item_signature(&content, &header, &envelope, &store, &bundle_ctx()).unwrap();
        assert_eq!(trust_class, TrustClass::Trusted);
        assert_eq!(signer.unwrap().0, fp);
    }

    #[test]
    fn verify_shebang_envelope_signature() {
        let (sk, vk, fp) = gen_key();
        let body = "print('hello')\n";
        let content = build_signed_content_with_shebang(body, &sk, &fp, "#!/usr/bin/env python3");

        let envelope = shebang_envelope();
        let header = crate::item_resolution::parse_signature_header(&content, &envelope).unwrap();
        let store = unscoped_store(vk, &fp);

        let (trust_class, signer) =
            verify_item_signature(&content, &header, &envelope, &store, &bundle_ctx()).unwrap();
        assert_eq!(trust_class, TrustClass::Trusted);
        assert_eq!(signer.unwrap().0, fp);
    }

    #[test]
    fn fingerprint_matches_identity_rs_computation() {
        // The fingerprint should be SHA-256 of the raw 32-byte public key
        let (_, vk, fp) = gen_key();
        let expected = sha256_hex(vk.as_bytes());
        assert_eq!(fp, expected);
    }
}
