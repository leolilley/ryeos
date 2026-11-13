//! Stopped-node amendments of the capacity account's issuance: re-anchoring
//! it to another filesystem, and replacing the policy it runs under. Both
//! are operator-signed documents in one chain over the original issuance.
//!
//! A durable identity names the filesystem's own identity plus the inode, so a
//! volume re-attached under another device number needs nothing. Moving the
//! data to a different filesystem is a different act: every recorded identity
//! then names a place the node no longer is. That move must be explicit and
//! signed. The operator key signs one document that names the exact prior
//! binding (by digest), the original issuance the account records still carry,
//! and the newly measured root, runtime, account-directory and account
//! identities. Boot accepts the account at the new place only through that
//! document: the re-signed initialization completion embeds a binding that
//! names the document's digest, and the document must verify under the pinned
//! operator key and agree with the binding in every identity.
//!
//! Documents are published under content-addressed names
//! (`native-capacity-reanchor.<digest>.json`) and never overwritten, so an
//! interrupted re-anchor cannot destroy the document the sealed completion
//! still names. Each document names the previous one and the exact binding it
//! replaces, and carries the account's epoch, debit sequence and anchor digest
//! at the time; a document that steps back from the one before it is refused.
//! Restoring a whole older volume (rolling the account back to an earlier
//! image) is the separate rollback class and is out of scope here.
//!
//! The command needs the node stopped (the state lock), refuses while an
//! incarnation debit is outstanding, never touches the account or anchor
//! records (they keep their original issuance digest), and records evidence
//! like re-issuance does.
//!
//! A policy amendment is the same document with the places unchanged and a
//! `policy` that replaces the one in force. The issuance pins the policy it
//! was granted under, so without it no `native_capacity` policy generation
//! other than the issued one can ever start the node; with it the operator
//! applies the new generation and the account continues under it, epoch and
//! debit sequence unchanged. The account's current image must already fit
//! the new policy's bounds, because every later start validates that image
//! under the generation's policy; an image that does not fit is refused
//! before any write, and an operator re-issuance opens a fresh image that
//! will.

use std::ffi::OsStr;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use lillux::crypto::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use lillux::{PinnedDirectory, PinnedDirectoryIdentity, PinnedRegularFileIdentity};
use ryeos_node_runtime::node_policy::sections::native_capacity::NodeNativeCapacityPolicy;
use ryeos_node_runtime::state_lock::StateLock;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use super::reissue::StoppedCapacityAccount;
use super::{NativeCapacityAccountBinding, digest, hex_digest};

const DOCUMENT_PREFIX: &str = "native-capacity-reanchor.";
const DOCUMENT_SUFFIX: &str = ".json";
const REANCHOR_DOCUMENT_BOUND: u64 = 16 * 1024;
const REANCHOR_SCHEMA: &str = "ryeos/native-capacity-reanchor/v2";
/// Longest chain of documents a node will walk.
const MAX_CHAIN: usize = 64;
/// Bound on directory entries scanned for documents.
const MAX_DIRECTORY_ENTRIES: usize = 4096;
/// Evidence of every re-anchor an operator performed, oldest first.
pub const REANCHOR_EVIDENCE_LOG: &str = "native-capacity-reanchors.jsonl";
const REANCHOR_EVIDENCE_LOG_BOUND: usize = 256 * 1024;
/// Evidence of every policy amendment an operator applied to this node's
/// capacity account, oldest first.
pub const POLICY_AMENDMENT_EVIDENCE_LOG: &str = "native-capacity-policy-amendments.jsonl";
const REASON_BOUND_CHARS: usize = 1024;

/// What a re-anchored binding records of the move: the original issuance the
/// account and anchor still carry, and the digest of the signed document that
/// authorises the new identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BindingReanchor {
    document_digest: String,
    issuance_digest: String,
}

impl BindingReanchor {
    pub(super) fn original_issuance(&self) -> Result<[u8; 32]> {
        parse_digest(&self.issuance_digest)
    }
}

fn parse_digest(value: &str) -> Result<[u8; 32]> {
    ensure!(digest(value), "re-anchor digest is not canonical");
    let mut out = [0_u8; 32];
    for (slot, pair) in out.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
        *slot = u8::from_str_radix(std::str::from_utf8(pair)?, 16)?;
    }
    Ok(out)
}

/// Where the account stood when the operator signed a move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountPosition {
    pub(crate) epoch: u64,
    pub(crate) sequence: u64,
    pub(crate) anchor_digest: String,
}

/// What the operator signed to move one capacity account to a new place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReanchorBody {
    schema: String,
    operator_fingerprint: String,
    node_fingerprint: String,
    /// Digest of the original issuance, which the account records carry.
    issuance_digest: String,
    /// Digest of the canonical JSON of the exact binding this replaces.
    prior_binding_digest: String,
    /// The document this one follows; absent for the first re-anchor.
    prior_document_digest: Option<String>,
    /// The account's epoch, debit sequence and anchor image when signed. A
    /// later document may not carry a lower epoch or sequence.
    epoch: u64,
    sequence: u64,
    anchor_digest: String,
    root_identity: PinnedDirectoryIdentity,
    runtime_identity: PinnedDirectoryIdentity,
    account_directory_identity: PinnedDirectoryIdentity,
    account_identity: PinnedRegularFileIdentity,
    /// The policy this document puts in force in place of the one before
    /// it; absent, the policy in force stays. A document that moves nothing
    /// and replaces no policy amends nothing and is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    policy: Option<NodeNativeCapacityPolicy>,
    reason: String,
    anchored_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReanchorDocument {
    body: ReanchorBody,
    signature: String,
}

fn document_name(document_digest: &str) -> String {
    format!("{DOCUMENT_PREFIX}{document_digest}{DOCUMENT_SUFFIX}")
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex_digest(&sha2::Sha256::digest(bytes).into())
}

/// Parse and verify a re-anchor document against the node and operator it
/// must name. The boot check and the command share this.
fn verify_document(
    bytes: &[u8],
    operator: &VerifyingKey,
    operator_fingerprint: &str,
    node_fingerprint: &str,
) -> Result<ReanchorBody> {
    ensure!(
        u64::try_from(bytes.len())? <= REANCHOR_DOCUMENT_BOUND,
        "capacity re-anchor document exceeds its bound"
    );
    let document: ReanchorDocument = serde_json::from_slice(bytes)?;
    let body = document.body;
    ensure!(
        body.schema == REANCHOR_SCHEMA,
        "foreign capacity re-anchor schema"
    );
    ensure!(
        body.operator_fingerprint == operator_fingerprint,
        "capacity re-anchor was signed for another operator"
    );
    ensure!(
        body.node_fingerprint == node_fingerprint,
        "capacity re-anchor belongs to another node"
    );
    ensure!(
        digest(&body.issuance_digest)
            && digest(&body.prior_binding_digest)
            && digest(&body.anchor_digest)
            && body.prior_document_digest.as_deref().is_none_or(digest),
        "capacity re-anchor digest is not canonical"
    );
    ensure!(body.epoch >= 1, "capacity re-anchor names no epoch");
    ensure!(
        body.reason.chars().count() <= REASON_BOUND_CHARS,
        "capacity re-anchor reason exceeds its bound"
    );
    if let Some(policy) = &body.policy {
        policy
            .validate()
            .context("capacity amendment names an invalid policy")?;
    }
    let encoded = document
        .signature
        .strip_prefix("ed25519:")
        .context("capacity re-anchor signature encoding invalid")?;
    let signature =
        Signature::from_slice(&base64::engine::general_purpose::STANDARD.decode(encoded)?)?;
    operator.verify(
        lillux::canonical_json(&serde_json::to_value(&body)?)?.as_bytes(),
        &signature,
    )?;
    Ok(body)
}

/// One published document's bytes, owned by the running account, which must
/// hash to the digest its name carries.
fn read_named(onboarding: &PinnedDirectory, document_digest: &str) -> Result<Option<Vec<u8>>> {
    ensure!(digest(document_digest), "re-anchor digest is not canonical");
    let name = document_name(document_digest);
    let Some(file) = onboarding.open_pinned_regular(OsStr::new(&name), false)? else {
        return Ok(None);
    };
    file.require_effective_user_owned()?;
    let observation = file.observation()?;
    let bytes = file.read_stable_bounded(&observation, REANCHOR_DOCUMENT_BOUND)?;
    ensure!(
        sha256_hex(&bytes) == document_digest,
        "capacity re-anchor document does not hash to its name"
    );
    Ok(Some(bytes))
}

/// Digests of the documents published in `onboarding`, by name.
fn published_digests(onboarding: &PinnedDirectory) -> Result<Vec<String>> {
    let mut digests = Vec::new();
    for name in onboarding.entry_names_bounded(MAX_DIRECTORY_ENTRIES)? {
        let Some(name) = name.to_str() else { continue };
        if let Some(candidate) = name
            .strip_prefix(DOCUMENT_PREFIX)
            .and_then(|rest| rest.strip_suffix(DOCUMENT_SUFFIX))
            && digest(candidate)
        {
            digests.push(candidate.to_owned());
        }
    }
    digests.sort();
    Ok(digests)
}

fn onboarding(app_root: &Path) -> Result<PinnedDirectory> {
    PinnedDirectory::open(&app_root.join(".ai/config/onboarding"))?
        .context("operator onboarding absent")
}

/// The binding the operator-signed issuance recorded at the original place.
fn issued_binding(
    onboarding: &PinnedDirectory,
    operator: &VerifyingKey,
    operator_fingerprint: &str,
) -> Result<NativeCapacityAccountBinding> {
    let file = onboarding
        .open_pinned_regular(OsStr::new("native-capacity-issuance.json"), false)?
        .context("capacity issuance document absent")?;
    file.require_effective_user_owned()?;
    let observation = file.observation()?;
    let bytes = file.read_stable_bounded(&observation, 16 * 1024)?;
    let document: super::IssuanceDocument = serde_json::from_slice(&bytes)?;
    ensure!(
        document.body.schema == "ryeos/native-capacity-issuance/v3"
            && document.body.operator_fingerprint == operator_fingerprint,
        "capacity issuance is foreign"
    );
    let encoded = document
        .signature
        .strip_prefix("ed25519:")
        .context("capacity signature encoding invalid")?;
    let signature =
        Signature::from_slice(&base64::engine::general_purpose::STANDARD.decode(encoded)?)?;
    operator.verify(
        lillux::canonical_json(&serde_json::to_value(&document.body)?)?.as_bytes(),
        &signature,
    )?;
    Ok(document.body.binding)
}

impl NativeCapacityAccountBinding {
    /// This binding moved to the places `body` names, under document
    /// `document_digest`. `original` supplies the issuance the account keeps.
    fn moved_by(&self, body: &ReanchorBody, document_digest: &str) -> Self {
        Self {
            root_identity: body.root_identity,
            runtime_identity: body.runtime_identity,
            account_directory_identity: body.account_directory_identity,
            account_identity: body.account_identity,
            policy: body.policy.clone().unwrap_or_else(|| self.policy.clone()),
            reanchor: Some(BindingReanchor {
                document_digest: document_digest.to_owned(),
                issuance_digest: body.issuance_digest.clone(),
            }),
            ..self.clone()
        }
    }
}

/// A verified chain of documents ending at one tip.
struct Chain {
    /// The binding the tip authorises.
    effective: NativeCapacityAccountBinding,
    tip: ReanchorBody,
    length: usize,
}

/// Walk back from `tip_digest` through the documents' `prior_document_digest`
/// links, then check forward from the original binding: every document is
/// signed by the operator for this node and issuance, names exactly the
/// binding the previous one produced, and never steps back in epoch or debit
/// sequence.
fn verify_chain(
    onboarding: &PinnedDirectory,
    original: &NativeCapacityAccountBinding,
    tip_digest: &str,
    operator: &VerifyingKey,
    operator_fingerprint: &str,
) -> Result<Chain> {
    let mut walk = Vec::new();
    let mut next = Some(tip_digest.to_owned());
    while let Some(current) = next {
        ensure!(
            walk.len() < MAX_CHAIN,
            "capacity re-anchor chain is too long"
        );
        let bytes = read_named(onboarding, &current)?
            .with_context(|| format!("capacity re-anchor document {current} is absent"))?;
        let body = verify_document(
            &bytes,
            operator,
            operator_fingerprint,
            &original.node_fingerprint,
        )?;
        next = body.prior_document_digest.clone();
        walk.push((current, body));
    }
    walk.reverse();
    let issuance = hex_digest(&original.issuance_digest()?);
    let mut effective = original.clone();
    let mut previous: Option<(u64, u64)> = None;
    let length = walk.len();
    let mut last = None;
    for (document_digest, body) in walk {
        ensure!(
            body.issuance_digest == issuance,
            "capacity re-anchor names another issuance"
        );
        ensure!(
            body.prior_binding_digest == effective.binding_digest()?,
            "capacity re-anchor does not replace the binding before it"
        );
        if let Some((epoch, sequence)) = previous {
            ensure!(
                body.epoch >= epoch && body.sequence >= sequence,
                "capacity re-anchor steps back in epoch or debit sequence; restoring an older \
                 volume is out of scope"
            );
        }
        previous = Some((body.epoch, body.sequence));
        let amended = effective.moved_by(&body, &document_digest);
        ensure!(
            !amended.same_places(&effective) || amended.policy != effective.policy,
            "capacity amendment moves nothing and replaces no policy"
        );
        effective = amended;
        last = Some(body);
    }
    Ok(Chain {
        effective,
        tip: last.context("capacity re-anchor chain is empty")?,
        length,
    })
}

/// The longest valid chain among the published documents, ties broken by the
/// later debit sequence. An invalid or stale document is not a tip; the one
/// a sealed completion names is verified on its own at boot.
fn best_chain(
    onboarding: &PinnedDirectory,
    original: &NativeCapacityAccountBinding,
    operator: &VerifyingKey,
    operator_fingerprint: &str,
) -> Result<Option<(String, Chain)>> {
    let mut best: Option<(String, Chain)> = None;
    for candidate in published_digests(onboarding)? {
        let Ok(chain) = verify_chain(
            onboarding,
            original,
            &candidate,
            operator,
            operator_fingerprint,
        ) else {
            continue;
        };
        let better = best.as_ref().is_none_or(|(_, current)| {
            (chain.length, chain.tip.sequence) > (current.length, current.tip.sequence)
        });
        if better {
            best = Some((candidate, chain));
        }
    }
    Ok(best)
}

impl NativeCapacityAccountBinding {
    /// The binding as the best published re-anchor chain, when there is one,
    /// moves it. Used where an operator-signed issuance is loaded to be
    /// resealed: the issuance document keeps its original identities, the
    /// chain supersedes them.
    pub(crate) fn with_current_reanchor(
        self,
        app_root: &Path,
        operator: &VerifyingKey,
        operator_fingerprint: &str,
    ) -> Result<Self> {
        let onboarding = onboarding(app_root)?;
        Ok(
            match best_chain(&onboarding, &self, operator, operator_fingerprint)? {
                Some((_, chain)) => chain.effective,
                None => self,
            },
        )
    }

    /// Boot's check of a re-anchored binding: the document it names and every
    /// document before it must verify under the pinned operator key, chain
    /// back to the operator-signed issuance without stepping back, and
    /// produce exactly this binding. A binding that was never re-anchored
    /// passes through to the ordinary install check alone.
    pub(crate) fn require_reanchor_document(
        &self,
        app_root: &Path,
        operator: &VerifyingKey,
        operator_fingerprint: &str,
    ) -> Result<()> {
        let Some(marker) = &self.reanchor else {
            return Ok(());
        };
        let onboarding = onboarding(app_root)?;
        let original = issued_binding(&onboarding, operator, operator_fingerprint)?;
        let chain = verify_chain(
            &onboarding,
            &original,
            &marker.document_digest,
            operator,
            operator_fingerprint,
        )?;
        ensure!(
            chain.effective == *self,
            "capacity re-anchor chain disagrees with the binding it authorises"
        );
        Ok(())
    }

    /// This binding's identities as they are measured at `app_root` now, the
    /// issuance anchor unchanged.
    fn measured_at(&self, app_root: &Path) -> Result<Self> {
        let directory = PinnedDirectory::open(&app_root.join(".ai/node/native-capacity"))?
            .context("capacity directory absent")?;
        let account = directory
            .open_pinned_regular(OsStr::new("account"), false)?
            .context("original capacity account is absent")?;
        Ok(Self {
            root_identity: PinnedDirectory::open(app_root)?
                .context("capacity root absent")?
                .identity()?,
            runtime_identity: PinnedDirectory::open(&app_root.join(".ai/state"))?
                .context("capacity runtime absent")?
                .identity()?,
            account_directory_identity: directory.identity()?,
            account_identity: account.identity()?,
            ..self.clone()
        })
    }

    fn binding_digest(&self) -> Result<String> {
        let canonical = lillux::canonical_json(&serde_json::to_value(self)?)?;
        Ok(sha256_hex(canonical.as_bytes()))
    }

    fn same_places(&self, other: &Self) -> bool {
        self.root_identity == other.root_identity
            && self.runtime_identity == other.runtime_identity
            && self.account_directory_identity == other.account_directory_identity
            && self.account_identity == other.account_identity
    }
}

/// One operator re-anchor on a stopped node.
pub struct ReanchorRequest<'a> {
    pub app_root: &'a Path,
    /// Held for the whole operation: a running daemon holds the same lock.
    pub state_lock: &'a StateLock,
    /// The operator key that signed the issuance; any other key is foreign.
    pub operator_key: &'a SigningKey,
    pub reason: &'a str,
    /// The command name recorded in the evidence log.
    pub command: &'a str,
}

/// What the command changed, or found already changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReanchorReport {
    /// The signed document now authorising this place; absent when the node
    /// was found anchored where it is and was never moved.
    pub document_digest: Option<String>,
    pub prior_binding_digest: Option<String>,
    /// The recorded binding already names this place: nothing was written.
    pub already_applied: bool,
}

/// One line of the bounded evidence log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityReanchorEvidence {
    pub document_digest: String,
    pub prior_binding_digest: String,
    pub reason: String,
    pub recorded_at: String,
    pub actor: lillux::process_control::ControllerAccount,
    pub command: String,
}

/// Anchor a stopped node's capacity account at the place it now is. Rerunning
/// after any interruption converges: the signed document is rewritten from
/// the same measurements and the completion resealed, and a node whose
/// recorded binding already names this place reports `already_applied`.
pub fn reanchor_capacity(request: ReanchorRequest<'_>) -> Result<ReanchorReport> {
    let ReanchorRequest {
        app_root,
        state_lock,
        operator_key,
        reason,
        command,
    } = request;
    state_lock.ensure_protects_app_root(app_root)?;
    let actor = lillux::process_control::ControllerAccount::current().map_err(|error| {
        anyhow::anyhow!(
            "capacity re-anchor refused before any write: the acting account cannot be recorded as its evidence actor ({error}); run it as the node account"
        )
    })?;
    ensure!(
        reason.chars().count() <= REASON_BOUND_CHARS,
        "capacity re-anchor reason exceeds its bound"
    );
    let report = crate::init::verify_init_completion_before_capacity_install(app_root)?
        .context("re-anchoring requires completed initialization")?;
    let operator_fingerprint =
        ryeos_engine::trust::compute_fingerprint(&operator_key.verifying_key());
    ensure!(
        operator_fingerprint == report.operator_fingerprint,
        "operator key differs from the one that initialized this node"
    );
    let prior = report.capacity_account;
    let measured = prior.measured_at(app_root)?;
    let onboarding = onboarding(app_root)?;
    let state =
        PinnedDirectory::open(&app_root.join(".ai/state"))?.context("capacity runtime absent")?;
    if let Some(log) = state.open_pinned_regular(OsStr::new(REANCHOR_EVIDENCE_LOG), false)? {
        require_owned_log(&log, REANCHOR_EVIDENCE_LOG, "re-anchor")?;
    }
    let operator = operator_key.verifying_key();
    if prior.same_places(&measured) {
        // A rerun after the seal but before the evidence line owes the line.
        if let Some(marker) = &prior.reanchor {
            let bytes = read_named(&onboarding, &marker.document_digest)?
                .context("the document the binding names is absent")?;
            let body = verify_document(
                &bytes,
                &operator,
                &operator_fingerprint,
                &report.node_fingerprint,
            )?;
            append_evidence_once(
                &state,
                &CapacityReanchorEvidence {
                    document_digest: marker.document_digest.clone(),
                    prior_binding_digest: body.prior_binding_digest,
                    reason: body.reason,
                    recorded_at: lillux::time::iso8601_now(),
                    actor,
                    command: command.to_owned(),
                },
            )?;
        }
        return Ok(ReanchorReport {
            document_digest: prior
                .reanchor
                .as_ref()
                .map(|marker| marker.document_digest.clone()),
            prior_binding_digest: None,
            already_applied: true,
        });
    }
    let policy = prior.policy.clone();
    // The records must be this account's, at the place now measured, and
    // carry no debit: nothing outstanding can be attributed across a move.
    measured.require_installed(app_root, &report.node_fingerprint, &policy)?;
    let issuance = prior.issuance_digest()?;
    let directory = PinnedDirectory::open(&app_root.join(".ai/node/native-capacity"))?
        .context("capacity directory absent")?;
    let mut owner = StoppedCapacityAccount::open(
        directory,
        measured.account_identity,
        onboarding.try_clone()?,
        state.try_clone()?,
        issuance,
        &policy,
    )?;
    let position = owner.require_no_outstanding_debit(&policy)?;
    state_lock.ensure_protects_app_root(app_root)?;

    let prior_document_digest = prior
        .reanchor
        .as_ref()
        .map(|marker| marker.document_digest.clone());
    if let Some(previous) = &prior_document_digest {
        let bytes = read_named(&onboarding, previous)?
            .context("the document the binding names is absent")?;
        let previous = verify_document(
            &bytes,
            &operator,
            &operator_fingerprint,
            &report.node_fingerprint,
        )?;
        ensure!(
            position.epoch >= previous.epoch && position.sequence >= previous.sequence,
            "the account stands before the previous re-anchor; restoring an older volume is out of scope"
        );
    }
    let prior_binding_digest = prior.binding_digest()?;
    let body = ReanchorBody {
        schema: REANCHOR_SCHEMA.to_owned(),
        operator_fingerprint: operator_fingerprint.clone(),
        node_fingerprint: report.node_fingerprint.clone(),
        issuance_digest: hex_digest(&issuance),
        prior_binding_digest: prior_binding_digest.clone(),
        prior_document_digest,
        epoch: position.epoch,
        sequence: position.sequence,
        anchor_digest: position.anchor_digest,
        root_identity: measured.root_identity,
        runtime_identity: measured.runtime_identity,
        account_directory_identity: measured.account_directory_identity,
        account_identity: measured.account_identity,
        policy: None,
        reason: reason.to_owned(),
        anchored_at: lillux::time::iso8601_now(),
    };
    // An interrupted run leaves a document no completion names yet; a rerun
    // measures the same facts and adopts it instead of publishing another.
    let adopted = published_digests(&onboarding)?
        .into_iter()
        .filter_map(|candidate| {
            let bytes = read_named(&onboarding, &candidate).ok()??;
            let existing = verify_document(
                &bytes,
                &operator,
                &operator_fingerprint,
                &report.node_fingerprint,
            )
            .ok()?;
            body.same_facts(&existing).then_some(candidate)
        })
        .next();
    let document_digest = match adopted {
        Some(digest) => digest,
        None => {
            let canonical = lillux::canonical_json(&serde_json::to_value(&body)?)?;
            let signature = operator_key.sign(canonical.as_bytes());
            let document = ReanchorDocument {
                body,
                signature: format!(
                    "ed25519:{}",
                    base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
                ),
            };
            let document_bytes = serde_json::to_vec(&document)?;
            ensure!(
                u64::try_from(document_bytes.len())? <= REANCHOR_DOCUMENT_BOUND,
                "capacity re-anchor document exceeds its bound"
            );
            let document_digest = sha256_hex(&document_bytes);
            // Content-addressed and create-only: nothing already published,
            // least of all the document the sealed completion names, is
            // overwritten.
            onboarding.atomic_write_pinned_if_same(
                OsStr::new(&document_name(&document_digest)),
                None,
                &document_bytes,
                0o600,
                crate::NODE_TEMP_PURPOSE,
            )?;
            onboarding.sync()?;
            document_digest
        }
    };
    // The completion now embeds the re-anchored binding and names the
    // document; until it is resealed boot still honours the previous one.
    crate::init::seal_current_init_completion(app_root, None)?;
    append_evidence_once(
        &state,
        &CapacityReanchorEvidence {
            document_digest: document_digest.clone(),
            prior_binding_digest: prior_binding_digest.clone(),
            reason: reason.to_owned(),
            recorded_at: lillux::time::iso8601_now(),
            actor,
            command: command.to_owned(),
        },
    )?;
    Ok(ReanchorReport {
        document_digest: Some(document_digest),
        prior_binding_digest: Some(prior_binding_digest),
        already_applied: false,
    })
}

/// One operator policy amendment on a stopped node: the capacity account
/// continues under `policy`, which the accompanying `native_capacity`
/// policy generation puts in force.
pub struct CapacityPolicyAmendmentRequest<'a> {
    pub app_root: &'a Path,
    /// Held by the command for the whole operation: a running daemon holds
    /// the same lock, so acquiring it is the proof that the node is stopped.
    pub state_lock: &'a StateLock,
    /// The operator key that signed the issuance; any other key is foreign.
    pub operator_key: &'a SigningKey,
    pub policy: &'a NodeNativeCapacityPolicy,
    pub reason: &'a str,
    /// The command name recorded in the evidence log.
    pub command: &'a str,
}

/// What the amendment changed, or found already in force.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityPolicyAmendmentReport {
    /// The amendment document in force for this policy.
    pub document_digest: Option<String>,
    /// Digest of the binding the amendment replaced; absent when the policy
    /// was already in force.
    pub prior_binding_digest: Option<String>,
    /// Digest of the canonical JSON of the policy now in force.
    pub policy_digest: String,
    pub already_applied: bool,
    /// The dead incarnation whose debit was settled before the amendment,
    /// as the next start would have settled it; absent when none was owed.
    pub settled_incarnation: Option<String>,
}

/// One line of the bounded policy-amendment evidence log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityPolicyAmendmentEvidence {
    pub document_digest: String,
    pub prior_binding_digest: String,
    pub policy_digest: String,
    pub reason: String,
    pub recorded_at: String,
    pub actor: lillux::process_control::ControllerAccount,
    pub command: String,
}

/// The authenticated operator and node an amendment acts for.
pub(super) struct AmendmentIdentity<'a> {
    pub(super) operator_key: &'a SigningKey,
    pub(super) operator_fingerprint: String,
    pub(super) node_fingerprint: String,
}

/// The directories an amendment reads and writes: the account's own
/// directory, the operator onboarding directory the documents live in, and
/// the state directory that holds the evidence log.
pub(super) struct AmendmentPlaces {
    pub(super) directory: PinnedDirectory,
    pub(super) onboarding: PinnedDirectory,
    pub(super) state: PinnedDirectory,
}

/// Put `policy` in force for a stopped node's capacity account by publishing
/// one amendment document over the issuance chain. The account and anchor
/// records are untouched: epoch and debit sequence continue, and they keep
/// the original issuance digest.
///
/// The caller publishes the matching `native_capacity` policy generation and
/// reseals the initialization completion; until both have happened the node
/// refuses to start, and rerunning the same operation converges. The prior
/// binding is read from the issuance and its published chain, not from the
/// completion record, because the generation may already have been
/// published when a run is repeated and the completion is then stale.
pub fn amend_capacity_policy(
    request: CapacityPolicyAmendmentRequest<'_>,
) -> Result<CapacityPolicyAmendmentReport> {
    prepare_capacity_policy_amendment(request)?.apply()
}

/// Check every precondition of [`amend_capacity_policy`] and decide what it
/// would write, writing nothing. A caller that must refuse before its own
/// first change prepares here, and applies only once its own checks pass.
pub fn prepare_capacity_policy_amendment(
    request: CapacityPolicyAmendmentRequest<'_>,
) -> Result<PreparedCapacityPolicyAmendment<'_>> {
    let CapacityPolicyAmendmentRequest {
        app_root,
        state_lock,
        operator_key,
        policy,
        reason,
        command,
    } = request;
    state_lock.ensure_protects_app_root(app_root)?;
    let actor = lillux::process_control::ControllerAccount::current().map_err(|error| {
        anyhow::anyhow!(
            "capacity policy amendment refused before any write: the acting account cannot be recorded as its evidence actor ({error}); run it as the node account"
        )
    })?;
    let operator = operator_key.verifying_key();
    let operator_fingerprint = ryeos_engine::trust::compute_fingerprint(&operator);
    let node_identity_path = app_root
        .join(ryeos_engine::AI_DIR)
        .join("node/identity/public-identity.json");
    let node_fingerprint =
        ryeos_node_runtime::identity::NodeIdentity::load_public_identity(&node_identity_path)
            .context("load pinned public node identity for the capacity policy amendment")?
            .verified_fingerprint()?;
    let onboarding = onboarding(app_root)?;
    let original = issued_binding(&onboarding, &operator, &operator_fingerprint)?;
    original.require_node(&node_fingerprint)?;
    let prior = match best_chain(&onboarding, &original, &operator, &operator_fingerprint)? {
        Some((_, chain)) => chain.effective,
        None => original,
    };
    // The records must be this account's, at the place the binding names,
    // under the policy in force now.
    prior.require_installed(app_root, &node_fingerprint, &prior.policy)?;
    let places = AmendmentPlaces {
        directory: PinnedDirectory::open(&app_root.join(".ai/node/native-capacity"))?
            .context("capacity directory absent")?,
        onboarding,
        state: PinnedDirectory::open(&app_root.join(".ai/state"))?
            .context("capacity runtime absent")?,
    };
    state_lock.ensure_protects_app_root(app_root)?;
    let identity = AmendmentIdentity {
        operator_key,
        operator_fingerprint,
        node_fingerprint,
    };
    let plan = plan_policy_at(&places, &prior, policy, &identity, reason)?;
    Ok(PreparedCapacityPolicyAmendment {
        places,
        identity,
        actor,
        command,
        plan,
    })
}

/// A policy amendment whose preconditions all held when it was prepared;
/// [`PreparedCapacityPolicyAmendment::apply`] performs its writes.
pub struct PreparedCapacityPolicyAmendment<'a> {
    places: AmendmentPlaces,
    identity: AmendmentIdentity<'a>,
    actor: lillux::process_control::ControllerAccount,
    command: &'a str,
    plan: PlannedPolicyAmendment,
}

impl PreparedCapacityPolicyAmendment<'_> {
    /// Whether the policy is or will be in force through a policy amendment
    /// document: false only when the binding in force already carries the
    /// policy without one, as the original issuance or after a plain move.
    pub fn involves_amendment(&self) -> bool {
        match &self.plan.outcome {
            PolicyAmendmentOutcome::InForce { owed, .. } => owed.is_some(),
            PolicyAmendmentOutcome::Amend { .. } | PolicyAmendmentOutcome::SettleFirst { .. } => {
                true
            }
        }
    }

    /// Publish the amendment document and its evidence line, or only the
    /// line a document published by an interrupted run still owes.
    pub fn apply(self) -> Result<CapacityPolicyAmendmentReport> {
        self.plan
            .write(&self.places, &self.identity, self.actor, self.command)
    }
}

/// [`amend_capacity_policy`] below the app-root plumbing: `prior` is the
/// binding in force (the issuance as amended by its chain) and `places` are
/// its directories.
#[cfg(test)]
pub(super) fn amend_policy_at(
    places: AmendmentPlaces,
    prior: &NativeCapacityAccountBinding,
    policy: &NodeNativeCapacityPolicy,
    identity: &AmendmentIdentity<'_>,
    actor: lillux::process_control::ControllerAccount,
    reason: &str,
    command: &str,
) -> Result<CapacityPolicyAmendmentReport> {
    plan_policy_at(&places, prior, policy, identity, reason)?
        .write(&places, identity, actor, command)
}

/// What an amendment writes, decided with every precondition checked.
struct PlannedPolicyAmendment {
    policy_digest: String,
    outcome: PolicyAmendmentOutcome,
}

enum PolicyAmendmentOutcome {
    /// The policy is already in force. `owed` names the policy document that
    /// carries it, whose evidence line is appended unless already recorded
    /// (a run interrupted after the document owes it); it is absent when no
    /// policy document carries the policy.
    InForce {
        document_digest: Option<String>,
        owed: Option<OwedPolicyEvidence>,
    },
    /// A new document replaces `prior_binding_digest`'s policy.
    Amend {
        body: Box<ReanchorBody>,
        prior_binding_digest: String,
        reason: String,
    },
    /// The account carries a dead incarnation's debit. A node stopped by its
    /// host without a clean shutdown always does, and a node whose new binary
    /// cannot start under the old generation can never settle it by
    /// restarting. The write settles it exactly as that start would, then
    /// plans again from the settled position, whose anchor the document names.
    SettleFirst {
        prior: Box<NativeCapacityAccountBinding>,
        policy: Box<NodeNativeCapacityPolicy>,
        reason: String,
    },
}

struct OwedPolicyEvidence {
    document_digest: String,
    prior_binding_digest: String,
    reason: String,
}

/// Every check an amendment makes before its first write: the policy and
/// reason bounds, the evidence log it must extend, and, when the policy is
/// not yet in force, that the account carries no outstanding debit, or only a
/// provably dead incarnation's (settled by the write, as a start would), and
/// that its current image fits the new policy.
fn plan_policy_at(
    places: &AmendmentPlaces,
    prior: &NativeCapacityAccountBinding,
    policy: &NodeNativeCapacityPolicy,
    identity: &AmendmentIdentity<'_>,
    reason: &str,
) -> Result<PlannedPolicyAmendment> {
    policy.validate()?;
    ensure!(
        reason.chars().count() <= REASON_BOUND_CHARS,
        "capacity policy amendment reason exceeds its bound"
    );
    let policy_digest =
        sha256_hex(lillux::canonical_json(&serde_json::to_value(policy)?)?.as_bytes());
    // The log this run must end by extending is checked before the first
    // write, so a refused log never leaves an amendment without evidence.
    if let Some(log) = places
        .state
        .open_pinned_regular(OsStr::new(POLICY_AMENDMENT_EVIDENCE_LOG), false)?
    {
        require_owned_log(&log, POLICY_AMENDMENT_EVIDENCE_LOG, "policy amendment")?;
    }
    let operator = identity.operator_key.verifying_key();
    if prior.policy == *policy {
        // Already in force. A rerun after the document but before the
        // evidence line owes the line, and only when the document in force
        // is a policy amendment rather than a move.
        let document_digest = prior
            .reanchor
            .as_ref()
            .map(|marker| marker.document_digest.clone());
        let owed = match &document_digest {
            Some(document_digest) => {
                let bytes = read_named(&places.onboarding, document_digest)?
                    .context("the document the binding names is absent")?;
                let body = verify_document(
                    &bytes,
                    &operator,
                    &identity.operator_fingerprint,
                    &identity.node_fingerprint,
                )?;
                body.policy.is_some().then(|| OwedPolicyEvidence {
                    document_digest: document_digest.clone(),
                    prior_binding_digest: body.prior_binding_digest,
                    reason: body.reason,
                })
            }
            None => None,
        };
        return Ok(PlannedPolicyAmendment {
            policy_digest,
            outcome: PolicyAmendmentOutcome::InForce {
                document_digest,
                owed,
            },
        });
    }
    let issuance = prior.issuance_digest()?;
    ensure!(
        places.directory.identity()? == prior.account_directory_identity,
        "capacity directory differs from the binding in force"
    );
    let mut owner = StoppedCapacityAccount::open(
        places.directory.try_clone()?,
        prior.account_identity,
        places.onboarding.try_clone()?,
        places.state.try_clone()?,
        issuance,
        &prior.policy,
    )?;
    // Every later start validates the account's current image under the
    // generation's policy, so the image must already fit the new one.
    let mut fit = StoppedCapacityAccount::open(
        places.directory.try_clone()?,
        prior.account_identity,
        places.onboarding.try_clone()?,
        places.state.try_clone()?,
        issuance,
        policy,
    )?;
    if owner.owes_cold_settlement(&prior.policy)? {
        // The settled image, recovery charge included, must fit the new
        // policy too, or the re-plan after the settlement write would refuse.
        fit.owes_cold_settlement(policy).context(
            "the capacity account, once its dead incarnation is settled, does not fit the new \
             policy; open a fresh epoch with `ryeos node capacity reissue` first, then apply the policy",
        )?;
        return Ok(PlannedPolicyAmendment {
            policy_digest,
            outcome: PolicyAmendmentOutcome::SettleFirst {
                prior: Box::new(prior.clone()),
                policy: Box::new(policy.clone()),
                reason: reason.to_owned(),
            },
        });
    }
    let position = owner.require_no_outstanding_debit(&prior.policy)?;
    fit.require_no_outstanding_debit(policy).context(
        "the capacity account's current image does not fit the new policy; open a fresh epoch \
         with `ryeos node capacity reissue` first, then apply the policy",
    )?;
    let prior_binding_digest = prior.binding_digest()?;
    let body = ReanchorBody {
        schema: REANCHOR_SCHEMA.to_owned(),
        operator_fingerprint: identity.operator_fingerprint.clone(),
        node_fingerprint: identity.node_fingerprint.clone(),
        issuance_digest: hex_digest(&issuance),
        prior_binding_digest: prior_binding_digest.clone(),
        prior_document_digest: prior
            .reanchor
            .as_ref()
            .map(|marker| marker.document_digest.clone()),
        epoch: position.epoch,
        sequence: position.sequence,
        anchor_digest: position.anchor_digest,
        root_identity: prior.root_identity,
        runtime_identity: prior.runtime_identity,
        account_directory_identity: prior.account_directory_identity,
        account_identity: prior.account_identity,
        policy: Some(policy.clone()),
        reason: reason.to_owned(),
        anchored_at: lillux::time::iso8601_now(),
    };
    Ok(PlannedPolicyAmendment {
        policy_digest,
        outcome: PolicyAmendmentOutcome::Amend {
            body: Box::new(body),
            prior_binding_digest,
            reason: reason.to_owned(),
        },
    })
}

impl PlannedPolicyAmendment {
    fn write(
        self,
        places: &AmendmentPlaces,
        identity: &AmendmentIdentity<'_>,
        actor: lillux::process_control::ControllerAccount,
        command: &str,
    ) -> Result<CapacityPolicyAmendmentReport> {
        let Self {
            policy_digest,
            outcome,
        } = self;
        match outcome {
            PolicyAmendmentOutcome::InForce {
                document_digest,
                owed,
            } => {
                if let Some(owed) = owed {
                    append_policy_evidence_once(
                        &places.state,
                        &CapacityPolicyAmendmentEvidence {
                            document_digest: owed.document_digest,
                            prior_binding_digest: owed.prior_binding_digest,
                            policy_digest: policy_digest.clone(),
                            reason: owed.reason,
                            recorded_at: lillux::time::iso8601_now(),
                            actor,
                            command: command.to_owned(),
                        },
                    )?;
                }
                Ok(CapacityPolicyAmendmentReport {
                    document_digest,
                    prior_binding_digest: None,
                    policy_digest,
                    already_applied: true,
                    settled_incarnation: None,
                })
            }
            PolicyAmendmentOutcome::Amend {
                body,
                prior_binding_digest,
                reason,
            } => {
                let document_digest = publish_or_adopt(&places.onboarding, *body, identity)?;
                append_policy_evidence_once(
                    &places.state,
                    &CapacityPolicyAmendmentEvidence {
                        document_digest: document_digest.clone(),
                        prior_binding_digest: prior_binding_digest.clone(),
                        policy_digest: policy_digest.clone(),
                        reason,
                        recorded_at: lillux::time::iso8601_now(),
                        actor,
                        command: command.to_owned(),
                    },
                )?;
                Ok(CapacityPolicyAmendmentReport {
                    document_digest: Some(document_digest),
                    prior_binding_digest: Some(prior_binding_digest),
                    policy_digest,
                    already_applied: false,
                    settled_incarnation: None,
                })
            }
            PolicyAmendmentOutcome::SettleFirst {
                prior,
                policy,
                reason,
            } => {
                let settled = StoppedCapacityAccount::open(
                    places.directory.try_clone()?,
                    prior.account_identity,
                    places.onboarding.try_clone()?,
                    places.state.try_clone()?,
                    prior.issuance_digest()?,
                    &prior.policy,
                )?
                .settle_cold(&prior.policy)?;
                let replanned = plan_policy_at(places, &prior, &policy, identity, &reason)?;
                ensure!(
                    !matches!(
                        replanned.outcome,
                        PolicyAmendmentOutcome::SettleFirst { .. }
                    ),
                    "the capacity account still carries a debit after its dead-incarnation settlement"
                );
                let mut report = replanned.write(places, identity, actor, command)?;
                report.settled_incarnation = settled;
                Ok(report)
            }
        }
    }
}

/// Publish `body` as a content-addressed document, or adopt a published one
/// that signs the same facts: an interrupted run leaves a document no
/// completion names yet, and a rerun must not publish another.
fn publish_or_adopt(
    onboarding: &PinnedDirectory,
    body: ReanchorBody,
    identity: &AmendmentIdentity<'_>,
) -> Result<String> {
    let operator = identity.operator_key.verifying_key();
    let adopted = published_digests(onboarding)?
        .into_iter()
        .filter_map(|candidate| {
            let bytes = read_named(onboarding, &candidate).ok()??;
            let existing = verify_document(
                &bytes,
                &operator,
                &identity.operator_fingerprint,
                &identity.node_fingerprint,
            )
            .ok()?;
            body.same_facts(&existing).then_some(candidate)
        })
        .next();
    if let Some(digest) = adopted {
        return Ok(digest);
    }
    let canonical = lillux::canonical_json(&serde_json::to_value(&body)?)?;
    let signature = identity.operator_key.sign(canonical.as_bytes());
    let document = ReanchorDocument {
        body,
        signature: format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
        ),
    };
    let document_bytes = serde_json::to_vec(&document)?;
    ensure!(
        u64::try_from(document_bytes.len())? <= REANCHOR_DOCUMENT_BOUND,
        "capacity amendment document exceeds its bound"
    );
    let document_digest = sha256_hex(&document_bytes);
    // Content-addressed and create-only: nothing already published, least
    // of all the document a sealed completion names, is overwritten.
    onboarding.atomic_write_pinned_if_same(
        OsStr::new(&document_name(&document_digest)),
        None,
        &document_bytes,
        0o600,
        crate::NODE_TEMP_PURPOSE,
    )?;
    onboarding.sync()?;
    Ok(document_digest)
}

/// A rerun after an interruption between the document and the log append
/// owes exactly one line; a rerun after a complete run owes none.
fn append_policy_evidence_once(
    directory: &PinnedDirectory,
    evidence: &CapacityPolicyAmendmentEvidence,
) -> Result<()> {
    let recorded: Vec<CapacityPolicyAmendmentEvidence> =
        read_log(directory, POLICY_AMENDMENT_EVIDENCE_LOG, "policy amendment")?;
    if recorded
        .iter()
        .any(|entry| entry.document_digest == evidence.document_digest)
    {
        return Ok(());
    }
    append_log(
        directory,
        POLICY_AMENDMENT_EVIDENCE_LOG,
        "policy amendment",
        evidence,
    )
}

impl ReanchorBody {
    /// Whether `other` signs the same facts: everything but the free text and
    /// the time.
    fn same_facts(&self, other: &Self) -> bool {
        self.issuance_digest == other.issuance_digest
            && self.prior_binding_digest == other.prior_binding_digest
            && self.prior_document_digest == other.prior_document_digest
            && self.epoch == other.epoch
            && self.sequence == other.sequence
            && self.anchor_digest == other.anchor_digest
            && self.root_identity == other.root_identity
            && self.runtime_identity == other.runtime_identity
            && self.account_directory_identity == other.account_directory_identity
            && self.account_identity == other.account_identity
            && self.policy == other.policy
    }
}

/// A rerun after an interruption between the seal and the log append owes
/// exactly one line; a rerun after a complete run owes none.
fn append_evidence_once(
    directory: &PinnedDirectory,
    evidence: &CapacityReanchorEvidence,
) -> Result<()> {
    if read_evidence(directory)?
        .iter()
        .any(|entry| entry.document_digest == evidence.document_digest)
    {
        return Ok(());
    }
    append_evidence(directory, evidence)
}

/// Every recorded re-anchor, oldest first. An unreadable line is an error.
pub fn read_capacity_reanchors(state_dir: &Path) -> Result<Vec<CapacityReanchorEvidence>> {
    let Some(directory) = PinnedDirectory::open(state_dir)? else {
        return Ok(Vec::new());
    };
    read_evidence(&directory)
}

fn read_evidence(directory: &PinnedDirectory) -> Result<Vec<CapacityReanchorEvidence>> {
    read_log(directory, REANCHOR_EVIDENCE_LOG, "re-anchor")
}

fn append_evidence(directory: &PinnedDirectory, evidence: &CapacityReanchorEvidence) -> Result<()> {
    append_log(directory, REANCHOR_EVIDENCE_LOG, "re-anchor", evidence)
}

/// Every recorded policy amendment, oldest first. An unreadable line is an
/// error.
pub fn read_capacity_policy_amendments(
    state_dir: &Path,
) -> Result<Vec<CapacityPolicyAmendmentEvidence>> {
    let Some(directory) = PinnedDirectory::open(state_dir)? else {
        return Ok(Vec::new());
    };
    read_log(
        &directory,
        POLICY_AMENDMENT_EVIDENCE_LOG,
        "policy amendment",
    )
}

/// An evidence log is the node account's own record: owned by the account
/// running the command and writable by no group or other. A log another
/// account owns or can write is refused, never read as evidence and never
/// extended.
fn require_owned_log(log: &lillux::PinnedRegularFile, name: &str, what: &str) -> Result<()> {
    log.require_effective_user_owned().with_context(|| {
        format!(
            "{what} evidence log {name} is refused: it must be owned by the account running this command and not group/other writable"
        )
    })
}

fn read_log<T: DeserializeOwned>(
    directory: &PinnedDirectory,
    name: &str,
    what: &str,
) -> Result<Vec<T>> {
    let Some(log) = directory.open_pinned_regular(OsStr::new(name), false)? else {
        return Ok(Vec::new());
    };
    require_owned_log(&log, name, what)?;
    let observation = log.observation()?;
    let raw = log.read_stable_bounded(&observation, REANCHOR_EVIDENCE_LOG_BOUND as u64)?;
    raw.split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice(line)
                .map_err(|error| anyhow::anyhow!("{what} evidence entry is unreadable: {error}"))
        })
        .collect()
}

/// Append one line by pinned compare-and-write, dropping the oldest whole
/// lines only when the log would exceed its bound.
fn append_log<T: Serialize>(
    directory: &PinnedDirectory,
    name: &str,
    what: &str,
    evidence: &T,
) -> Result<()> {
    let existing = directory.open_pinned_regular(OsStr::new(name), false)?;
    let mut body = match &existing {
        Some(log) => {
            require_owned_log(log, name, what)?;
            let observation = log.observation()?;
            log.read_stable_bounded(&observation, REANCHOR_EVIDENCE_LOG_BOUND as u64)?
        }
        None => Vec::new(),
    };
    let mut line = serde_json::to_vec(evidence)?;
    line.push(b'\n');
    ensure!(
        line.len() <= REANCHOR_EVIDENCE_LOG_BOUND,
        "{what} evidence entry exceeds the log bound"
    );
    while body.len() + line.len() > REANCHOR_EVIDENCE_LOG_BOUND {
        match body.iter().position(|byte| *byte == b'\n') {
            Some(end) => {
                body.drain(..=end);
            }
            None => body.clear(),
        }
    }
    body.extend_from_slice(&line);
    directory.atomic_write_pinned_if_same(
        OsStr::new(name),
        existing.as_ref(),
        &body,
        0o600,
        crate::NODE_TEMP_PURPOSE,
    )?;
    directory.sync()?;
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod controls {
    //! Document-level controls for the re-anchor chain. The binding is a
    //! synthetic issuance and the keys are deterministic; nothing here is
    //! admission.

    use super::*;
    use ryeos_node_runtime::node_policy::sections::native_capacity::{
        NativeCapacityUnits, NativeIncarnationEnvelope, NativeRecoveryEnvelope,
        NativeRenewalEnvelope, NodeNativeCapacityPolicy,
    };

    fn directory_id(device: u64, inode: u64) -> PinnedDirectoryIdentity {
        serde_json::from_value(
            serde_json::json!({"filesystem": {"device": device}, "inode": inode}),
        )
        .unwrap()
    }

    fn file_id(device: u64, inode: u64) -> PinnedRegularFileIdentity {
        serde_json::from_value(
            serde_json::json!({"filesystem": {"device": device}, "inode": inode}),
        )
        .unwrap()
    }

    fn original() -> NativeCapacityAccountBinding {
        let units = NativeCapacityUnits {
            storage_bytes: 1 << 20,
            host_control_storage_bytes: 4096,
            allocator_steps: 1000,
            connection_tickets: 4,
            cleanup_steps: 8,
        };
        NativeCapacityAccountBinding {
            schema: 6,
            node_fingerprint: "f".repeat(64),
            root_identity: directory_id(1, 10),
            runtime_identity: directory_id(1, 11),
            account_directory_identity: directory_id(1, 12),
            account_identity: file_id(1, 13),
            anchor_protocol: super::super::anchor::PROTOCOL.to_owned(),
            reanchor: None,
            policy: NodeNativeCapacityPolicy {
                schema: 7,
                issuance_nonce: "a".repeat(32),
                allowance: units,
                incarnation: NativeIncarnationEnvelope {
                    units,
                    maximum_occupancy_ms: 3,
                    cleanup_occupancy_ms: 1,
                    rollover_drain_margin_ms: 1,
                    rollover_work_margin_steps: Some(1),
                    host_release_steps: 2,
                    outside_cleanup_steps: 1,
                },
                recovery: NativeRecoveryEnvelope {
                    maximum_attempts: 32,
                    work_per_attempt: 65_536,
                    control_storage_bytes: 262_144,
                    maximum_duration_ms: 120_000,
                },
                renewal: NativeRenewalEnvelope {
                    at_orderly_rollover: false,
                    operator_reissue: true,
                    maximum_epochs: None,
                },
            },
        }
    }

    struct Fixture {
        _temporary: tempfile::TempDir,
        onboarding: PinnedDirectory,
        operator: SigningKey,
        operator_fingerprint: String,
        original: NativeCapacityAccountBinding,
    }

    impl Fixture {
        fn new() -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let onboarding = PinnedDirectory::open(temporary.path()).unwrap().unwrap();
            let operator = SigningKey::from_bytes(&[7; 32]);
            Self {
                _temporary: temporary,
                onboarding,
                operator_fingerprint: ryeos_engine::trust::compute_fingerprint(
                    &operator.verifying_key(),
                ),
                operator,
                original: original(),
            }
        }

        /// The facts of a move to places numbered `place`, following
        /// `previous` (the binding and document it replaces).
        fn body(
            &self,
            previous: &NativeCapacityAccountBinding,
            previous_document: Option<&str>,
            place: u64,
            epoch: u64,
            sequence: u64,
        ) -> ReanchorBody {
            ReanchorBody {
                schema: REANCHOR_SCHEMA.to_owned(),
                operator_fingerprint: self.operator_fingerprint.clone(),
                node_fingerprint: self.original.node_fingerprint.clone(),
                issuance_digest: hex_digest(&self.original.issuance_digest().unwrap()),
                prior_binding_digest: previous.binding_digest().unwrap(),
                prior_document_digest: previous_document.map(str::to_owned),
                epoch,
                sequence,
                anchor_digest: "c".repeat(64),
                root_identity: directory_id(place, 10),
                runtime_identity: directory_id(place, 11),
                account_directory_identity: directory_id(place, 12),
                account_identity: file_id(place, 13),
                policy: None,
                reason: "controls".to_owned(),
                anchored_at: "fixture".to_owned(),
            }
        }

        fn sign_with(key: &SigningKey, body: ReanchorBody) -> Vec<u8> {
            let canonical = lillux::canonical_json(&serde_json::to_value(&body).unwrap()).unwrap();
            let signature = key.sign(canonical.as_bytes());
            serde_json::to_vec(&ReanchorDocument {
                body,
                signature: format!(
                    "ed25519:{}",
                    base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
                ),
            })
            .unwrap()
        }

        fn publish_bytes(&self, bytes: &[u8]) -> String {
            let digest = sha256_hex(bytes);
            self.onboarding
                .atomic_write_pinned_if_same(
                    OsStr::new(&document_name(&digest)),
                    None,
                    bytes,
                    0o600,
                    crate::NODE_TEMP_PURPOSE,
                )
                .unwrap();
            digest
        }

        fn publish(&self, body: ReanchorBody) -> String {
            self.publish_bytes(&Self::sign_with(&self.operator, body))
        }

        fn chain(&self, tip: &str) -> Result<Chain> {
            verify_chain(
                &self.onboarding,
                &self.original,
                tip,
                &self.operator.verifying_key(),
                &self.operator_fingerprint,
            )
        }

        fn best(&self) -> Option<(String, Chain)> {
            best_chain(
                &self.onboarding,
                &self.original,
                &self.operator.verifying_key(),
                &self.operator_fingerprint,
            )
            .unwrap()
        }
    }

    #[test]
    fn an_interrupted_second_reanchor_leaves_the_first_bootable_and_converges() {
        let f = Fixture::new();
        let first = f.publish(f.body(&f.original, None, 2, 1, 4));
        let after_first = f.chain(&first).unwrap().effective;
        // The second run published its document, then died before the seal:
        // the completion still names the first, which must still verify.
        let second_body = f.body(&after_first, Some(&first), 3, 1, 4);
        let second = f.publish(second_body.clone());
        assert_ne!(first, second, "documents are never overwritten");
        assert_eq!(f.chain(&first).unwrap().effective, after_first);
        // A rerun measures the same facts, adopts the stray document, and the
        // reseal's best chain is the second.
        let rerun = ReanchorBody {
            reason: "rerun".to_owned(),
            anchored_at: "later".to_owned(),
            ..second_body.clone()
        };
        assert!(rerun.same_facts(&second_body));
        let (tip, chain) = f.best().expect("a valid chain");
        assert_eq!(tip, second);
        assert_eq!(chain.length, 2);
        assert_eq!(
            chain.effective.reanchor.as_ref().unwrap().document_digest,
            second
        );
    }

    #[test]
    fn a_foreign_operator_is_refused_and_never_a_tip() {
        let f = Fixture::new();
        let foreign = SigningKey::from_bytes(&[9; 32]);
        let body = f.body(&f.original, None, 2, 1, 0);
        let digest = f.publish_bytes(&Fixture::sign_with(&foreign, body));
        let error = f.chain(&digest).err().expect("signed by another key");
        assert!(format!("{error:#}").to_lowercase().contains("signature"));
        assert!(f.best().is_none());
        // Claiming the operator's fingerprint does not help.
        let other = ReanchorBody {
            operator_fingerprint: "e".repeat(64),
            ..f.body(&f.original, None, 2, 1, 0)
        };
        let digest = f.publish(other);
        assert!(f.chain(&digest).is_err());
    }

    #[test]
    fn a_document_for_another_node_is_refused() {
        let f = Fixture::new();
        let body = ReanchorBody {
            node_fingerprint: "d".repeat(64),
            ..f.body(&f.original, None, 2, 1, 0)
        };
        let digest = f.publish(body);
        let error = f.chain(&digest).err().expect("another node");
        assert!(format!("{error:#}").contains("another node"), "{error:#}");
    }

    #[test]
    fn a_tampered_body_field_or_name_is_refused() {
        let f = Fixture::new();
        let bytes = Fixture::sign_with(&f.operator, f.body(&f.original, None, 2, 1, 0));
        let digest = f.publish_bytes(&bytes);
        // A body field changed after signing no longer verifies.
        let mut tampered: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        tampered["body"]["sequence"] = 99.into();
        let error = verify_document(
            &serde_json::to_vec(&tampered).unwrap(),
            &f.operator.verifying_key(),
            &f.operator_fingerprint,
            &f.original.node_fingerprint,
        )
        .expect_err("tampered field");
        assert!(format!("{error:#}").to_lowercase().contains("signature"));
        // Bytes that do not hash to the name they sit under are not that
        // document.
        let name = document_name(&digest);
        let path = f._temporary.path().join(&name);
        std::fs::write(&path, b"{}").unwrap();
        assert!(read_named(&f.onboarding, &digest).is_err());
    }

    #[test]
    fn a_stale_or_stepping_back_document_is_refused() {
        let f = Fixture::new();
        let first = f.publish(f.body(&f.original, None, 2, 2, 10));
        let after_first = f.chain(&first).unwrap().effective;
        // Names the original binding although the first already replaced it.
        let stale = f.publish(f.body(&f.original, Some(&first), 3, 2, 10));
        let error = f.chain(&stale).err().expect("stale prior binding");
        assert!(
            format!("{error:#}").contains("does not replace the binding before it"),
            "{error:#}"
        );
        // A lower debit sequence or epoch than the document before it.
        for (epoch, sequence) in [(2, 9), (1, 10)] {
            let back = f.publish(f.body(&after_first, Some(&first), 3, epoch, sequence));
            let error = f.chain(&back).err().expect("steps back");
            assert!(format!("{error:#}").contains("steps back"), "{error:#}");
        }
        // The only tip left is the first.
        assert_eq!(f.best().unwrap().0, first);
        // Another issuance entirely.
        let foreign = ReanchorBody {
            issuance_digest: "9".repeat(64),
            ..f.body(&f.original, None, 4, 1, 0)
        };
        let digest = f.publish(foreign);
        assert!(f.chain(&digest).is_err());
    }

    #[test]
    fn a_policy_amendment_replaces_the_policy_and_keeps_the_places_and_issuance() {
        let f = Fixture::new();
        let mut amended_policy = f.original.policy.clone();
        amended_policy.incarnation.rollover_work_margin_steps = amended_policy
            .incarnation
            .rollover_work_margin_steps
            .map(|margin| margin + 1);
        // A document that neither moves nor replaces the policy amends
        // nothing and is no tip.
        let idle = f.publish(f.body(&f.original, None, 1, 1, 0));
        let error = f.chain(&idle).err().expect("amends nothing");
        assert!(format!("{error:#}").contains("moves nothing"), "{error:#}");
        assert!(f.best().is_none());
        let amendment = f.publish(ReanchorBody {
            policy: Some(amended_policy.clone()),
            ..f.body(&f.original, None, 1, 1, 0)
        });
        let chain = f.chain(&amendment).unwrap();
        assert_eq!(chain.effective.policy, amended_policy);
        assert!(chain.effective.same_places(&f.original));
        assert_eq!(
            chain.effective.issuance_digest().unwrap(),
            f.original.issuance_digest().unwrap()
        );
        assert_eq!(f.best().unwrap().0, amendment);
        // A later move keeps the amended policy; a later amendment over the
        // moved binding keeps the move.
        let moved = f.publish(f.body(&chain.effective, Some(&amendment), 2, 1, 0));
        let after_move = f.chain(&moved).unwrap().effective;
        assert_eq!(after_move.policy, amended_policy);
        assert_eq!(after_move.root_identity, directory_id(2, 10));
        let mut again = amended_policy.clone();
        again.incarnation.rollover_work_margin_steps = again
            .incarnation
            .rollover_work_margin_steps
            .map(|margin| margin + 1);
        let second = f.publish(ReanchorBody {
            policy: Some(again.clone()),
            ..f.body(&after_move, Some(&moved), 2, 1, 0)
        });
        let final_binding = f.chain(&second).unwrap().effective;
        assert_eq!(final_binding.policy, again);
        assert_eq!(final_binding.root_identity, directory_id(2, 10));
        // An invalid policy is refused as a document.
        let mut invalid = again.clone();
        invalid.issuance_nonce = "short".to_owned();
        let bad = f.publish(ReanchorBody {
            policy: Some(invalid),
            ..f.body(&final_binding, Some(&second), 2, 1, 0)
        });
        assert!(f.chain(&bad).is_err());
    }

    /// A real account and anchor on disk under the controls binding, so the
    /// amendment reads and fits them the way the command does.
    struct AccountFixture {
        _temporary: tempfile::TempDir,
        places: AmendmentPlaces,
        onboarding: PinnedDirectory,
        state: PinnedDirectory,
        binding: NativeCapacityAccountBinding,
        operator: SigningKey,
        operator_fingerprint: String,
    }

    impl AccountFixture {
        fn new() -> Self {
            use super::super::redo::AccountRedo;
            use super::super::{CapacityRecord, RECORD_SCHEMA, encode, genesis_consumed};
            let temporary = tempfile::tempdir().unwrap();
            for name in [
                "native-capacity",
                "onboarding",
                "state",
                "root",
                "root/.ai/state",
            ] {
                std::fs::create_dir_all(temporary.path().join(name)).unwrap();
            }
            let directory = PinnedDirectory::open(&temporary.path().join("native-capacity"))
                .unwrap()
                .unwrap();
            directory.tighten_owner_private_directory().unwrap();
            let onboarding = PinnedDirectory::open(&temporary.path().join("onboarding"))
                .unwrap()
                .unwrap();
            let state = PinnedDirectory::open(&temporary.path().join("state"))
                .unwrap()
                .unwrap();
            let (policy, _, _) = super::super::redo::controls::fixture();
            let account = directory
                .atomic_create_pinned_regular(
                    OsStr::new("account"),
                    &[0; super::super::RECORD_BYTES],
                    0o600,
                    crate::NODE_TEMP_PURPOSE,
                )
                .unwrap()
                .unwrap();
            let binding = NativeCapacityAccountBinding {
                root_identity: PinnedDirectory::open(&temporary.path().join("root"))
                    .unwrap()
                    .unwrap()
                    .identity()
                    .unwrap(),
                runtime_identity: PinnedDirectory::open(&temporary.path().join("root/.ai/state"))
                    .unwrap()
                    .unwrap()
                    .identity()
                    .unwrap(),
                account_directory_identity: directory.identity().unwrap(),
                account_identity: account.identity().unwrap(),
                policy,
                ..original()
            };
            let genesis = CapacityRecord {
                schema: RECORD_SCHEMA,
                epoch: 1,
                issuance_digest: binding.issuance_digest().unwrap(),
                sequence: 0,
                consumed: genesis_consumed(),
                outstanding: None,
            };
            let redo = AccountRedo::genesis(genesis, &binding.policy).unwrap();
            let bytes = encode(&genesis).unwrap();
            let mut scratch = [0; super::super::RECORD_BYTES];
            account
                .overwrite_fixed_if_matches(
                    &[0; super::super::RECORD_BYTES][..bytes.len()],
                    &bytes,
                    &mut scratch[..bytes.len()],
                    lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(10)),
                )
                .unwrap();
            directory
                .atomic_create_pinned_regular(
                    OsStr::new("anchor"),
                    &redo.encode(&binding.policy).unwrap(),
                    0o600,
                    crate::NODE_TEMP_PURPOSE,
                )
                .unwrap()
                .unwrap();
            let operator = SigningKey::from_bytes(&[7; 32]);
            Self {
                places: AmendmentPlaces {
                    directory,
                    onboarding: onboarding.try_clone().unwrap(),
                    state: state.try_clone().unwrap(),
                },
                _temporary: temporary,
                onboarding,
                state,
                operator_fingerprint: ryeos_engine::trust::compute_fingerprint(
                    &operator.verifying_key(),
                ),
                binding,
                operator,
            }
        }

        fn amend(
            &self,
            prior: &NativeCapacityAccountBinding,
            policy: &NodeNativeCapacityPolicy,
        ) -> Result<CapacityPolicyAmendmentReport> {
            let places = AmendmentPlaces {
                directory: self.places.directory.try_clone().unwrap(),
                onboarding: self.places.onboarding.try_clone().unwrap(),
                state: self.places.state.try_clone().unwrap(),
            };
            amend_policy_at(
                places,
                prior,
                policy,
                &AmendmentIdentity {
                    operator_key: &self.operator,
                    operator_fingerprint: self.operator_fingerprint.clone(),
                    node_fingerprint: self.binding.node_fingerprint.clone(),
                },
                lillux::process_control::ControllerAccount::current().unwrap(),
                "controls",
                "ryeos node policy-apply native_capacity",
            )
        }

        fn in_force(&self) -> NativeCapacityAccountBinding {
            best_chain(
                &self.onboarding,
                &self.binding,
                &self.operator.verifying_key(),
                &self.operator_fingerprint,
            )
            .unwrap()
            .map(|(_, chain)| chain.effective)
            .unwrap_or_else(|| self.binding.clone())
        }

        fn evidence(&self) -> Vec<CapacityPolicyAmendmentEvidence> {
            read_log(
                &self.state,
                POLICY_AMENDMENT_EVIDENCE_LOG,
                "policy amendment",
            )
            .unwrap()
        }
    }

    #[test]
    fn amending_the_policy_publishes_one_document_and_converges_on_rerun() {
        let f = AccountFixture::new();
        let mut amended = f.binding.policy.clone();
        amended.incarnation.rollover_work_margin_steps = amended
            .incarnation
            .rollover_work_margin_steps
            .map(|margin| margin + 1);
        let report = f.amend(&f.binding, &amended).unwrap();
        assert!(!report.already_applied);
        let document = report.document_digest.clone().expect("a document");
        assert_eq!(
            report.prior_binding_digest.as_deref(),
            Some(f.binding.binding_digest().unwrap().as_str())
        );
        let in_force = f.in_force();
        assert_eq!(in_force.policy, amended);
        assert_eq!(
            in_force.reanchor.as_ref().unwrap().document_digest,
            document
        );
        assert_eq!(
            in_force.issuance_digest().unwrap(),
            f.binding.issuance_digest().unwrap()
        );
        assert_eq!(f.evidence().len(), 1);
        assert_eq!(f.evidence()[0].policy_digest, report.policy_digest);
        // The records are untouched: the anchor still loads under the old
        // policy at epoch 1, sequence 0.
        let mut owner = StoppedCapacityAccount::open(
            f.places.directory.try_clone().unwrap(),
            f.binding.account_identity,
            f.onboarding.try_clone().unwrap(),
            f.state.try_clone().unwrap(),
            f.binding.issuance_digest().unwrap(),
            &f.binding.policy,
        )
        .unwrap();
        let position = owner
            .require_no_outstanding_debit(&f.binding.policy)
            .unwrap();
        assert_eq!((position.epoch, position.sequence), (1, 0));
        // A rerun with the policy already in force publishes nothing and
        // owes no second line.
        let rerun = f.amend(&in_force, &amended).unwrap();
        assert!(rerun.already_applied);
        assert_eq!(rerun.document_digest.as_deref(), Some(document.as_str()));
        assert_eq!(f.evidence().len(), 1);
        assert_eq!(published_digests(&f.onboarding).unwrap().len(), 1);
        // A second amendment chains on the first.
        let mut again = amended.clone();
        again.incarnation.rollover_work_margin_steps = again
            .incarnation
            .rollover_work_margin_steps
            .map(|margin| margin + 1);
        let second = f.amend(&in_force, &again).unwrap();
        assert!(!second.already_applied);
        assert_eq!(f.in_force().policy, again);
        assert_eq!(published_digests(&f.onboarding).unwrap().len(), 2);
        assert_eq!(f.evidence().len(), 2);
    }

    #[test]
    fn a_policy_the_current_image_does_not_fit_is_refused_before_any_write() {
        let f = AccountFixture::new();
        let mut too_small = f.binding.policy.clone();
        // Valid in itself (above the control-storage floor), but genesis
        // already consumed more storage than this allowance grants, so the
        // current image cannot be held under it.
        too_small.allowance.storage_bytes = 70_000;
        too_small.incarnation.units.storage_bytes = 70_000;
        too_small.validate().unwrap();
        let error = f.amend(&f.binding, &too_small).unwrap_err();
        assert!(
            format!("{error:#}").contains("does not fit the new policy"),
            "{error:#}"
        );
        assert!(published_digests(&f.onboarding).unwrap().is_empty());
        assert!(f.evidence().is_empty());
        assert_eq!(f.in_force(), f.binding);
    }
}
