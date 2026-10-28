//! Stopped-node re-anchoring of the capacity account to another filesystem.
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

use std::ffi::OsStr;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use lillux::crypto::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use lillux::{PinnedDirectory, PinnedDirectoryIdentity, PinnedRegularFileIdentity};
use ryeos_node_runtime::state_lock::StateLock;
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
        effective = effective.moved_by(&body, &document_digest);
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
        require_owned_log(&log)?;
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
    let Some(log) = directory.open_pinned_regular(OsStr::new(REANCHOR_EVIDENCE_LOG), false)? else {
        return Ok(Vec::new());
    };
    require_owned_log(&log)?;
    let observation = log.observation()?;
    let raw = log.read_stable_bounded(&observation, REANCHOR_EVIDENCE_LOG_BOUND as u64)?;
    raw.split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice(line)
                .map_err(|error| anyhow::anyhow!("re-anchor evidence entry is unreadable: {error}"))
        })
        .collect()
}

fn require_owned_log(log: &lillux::PinnedRegularFile) -> Result<()> {
    log.require_effective_user_owned().with_context(|| {
        format!(
            "re-anchor evidence log {REANCHOR_EVIDENCE_LOG} is refused: it must be owned by the account running this command and not group/other writable"
        )
    })
}

fn append_evidence(directory: &PinnedDirectory, evidence: &CapacityReanchorEvidence) -> Result<()> {
    let existing = directory.open_pinned_regular(OsStr::new(REANCHOR_EVIDENCE_LOG), false)?;
    let mut body = match &existing {
        Some(log) => {
            require_owned_log(log)?;
            let observation = log.observation()?;
            log.read_stable_bounded(&observation, REANCHOR_EVIDENCE_LOG_BOUND as u64)?
        }
        None => Vec::new(),
    };
    let mut line = serde_json::to_vec(evidence)?;
    line.push(b'\n');
    ensure!(
        line.len() <= REANCHOR_EVIDENCE_LOG_BOUND,
        "re-anchor evidence entry exceeds the log bound"
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
        OsStr::new(REANCHOR_EVIDENCE_LOG),
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
                schema: 6,
                issuance_nonce: "a".repeat(32),
                allowance: units,
                incarnation: NativeIncarnationEnvelope {
                    units,
                    maximum_occupancy_ms: 3,
                    cleanup_occupancy_ms: 1,
                    rollover_drain_margin_ms: 1,
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
}
