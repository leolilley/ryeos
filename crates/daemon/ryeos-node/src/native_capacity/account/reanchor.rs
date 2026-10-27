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

pub(crate) const REANCHOR_DOCUMENT_NAME: &str = "native-capacity-reanchor.json";
const REANCHOR_DOCUMENT_BOUND: u64 = 16 * 1024;
const REANCHOR_SCHEMA: &str = "ryeos/native-capacity-reanchor/v1";
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
        digest(&body.issuance_digest) && digest(&body.prior_binding_digest),
        "capacity re-anchor digest is not canonical"
    );
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

/// The published document's bytes, owned by the running account.
fn read_document(onboarding: &PinnedDirectory) -> Result<Option<Vec<u8>>> {
    let Some(file) = onboarding.open_pinned_regular(OsStr::new(REANCHOR_DOCUMENT_NAME), false)?
    else {
        return Ok(None);
    };
    file.require_effective_user_owned()?;
    let observation = file.observation()?;
    Ok(Some(file.read_stable_bounded(
        &observation,
        REANCHOR_DOCUMENT_BOUND,
    )?))
}

fn onboarding(app_root: &Path) -> Result<PinnedDirectory> {
    PinnedDirectory::open(&app_root.join(".ai/config/onboarding"))?
        .context("operator onboarding absent")
}

impl NativeCapacityAccountBinding {
    /// The binding as the published re-anchor document, when there is one,
    /// moves it. Used where an operator-signed issuance is loaded to be
    /// resealed: the issuance document keeps its original identities, the
    /// re-anchor document supersedes them.
    pub(crate) fn with_current_reanchor(
        self,
        app_root: &Path,
        operator: &VerifyingKey,
        operator_fingerprint: &str,
    ) -> Result<Self> {
        let Some(bytes) = read_document(&onboarding(app_root)?)? else {
            return Ok(self);
        };
        let body = verify_document(
            &bytes,
            operator,
            operator_fingerprint,
            &self.node_fingerprint,
        )?;
        ensure!(
            body.issuance_digest == hex_digest(&self.issuance_digest()?),
            "capacity re-anchor names another issuance"
        );
        Ok(Self {
            root_identity: body.root_identity,
            runtime_identity: body.runtime_identity,
            account_directory_identity: body.account_directory_identity,
            account_identity: body.account_identity,
            reanchor: Some(BindingReanchor {
                document_digest: hex_digest(&sha2::Sha256::digest(&bytes).into()),
                issuance_digest: body.issuance_digest,
            }),
            ..self
        })
    }

    /// Boot's check of a re-anchored binding: the signed document it names
    /// must be the published one, verify under the pinned operator key, and
    /// agree with this binding in every identity. A binding that was never
    /// re-anchored passes through to the ordinary install check alone.
    pub(crate) fn require_reanchor_document(
        &self,
        app_root: &Path,
        operator: &VerifyingKey,
        operator_fingerprint: &str,
    ) -> Result<()> {
        let Some(marker) = &self.reanchor else {
            return Ok(());
        };
        let bytes = read_document(&onboarding(app_root)?)?
            .context("capacity binding was re-anchored but its signed document is absent")?;
        ensure!(
            hex_digest(&sha2::Sha256::digest(&bytes).into()) == marker.document_digest,
            "capacity re-anchor document is not the one the binding names"
        );
        let body = verify_document(
            &bytes,
            operator,
            operator_fingerprint,
            &self.node_fingerprint,
        )?;
        ensure!(
            body.issuance_digest == marker.issuance_digest
                && body.root_identity == self.root_identity
                && body.runtime_identity == self.runtime_identity
                && body.account_directory_identity == self.account_directory_identity
                && body.account_identity == self.account_identity,
            "capacity re-anchor document disagrees with the binding it authorises"
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
        Ok(hex_digest(
            &sha2::Sha256::digest(canonical.as_bytes()).into(),
        ))
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
    if prior.same_places(&measured) {
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
    let onboarding = onboarding(app_root)?;
    let state =
        PinnedDirectory::open(&app_root.join(".ai/state"))?.context("capacity runtime absent")?;
    let mut owner = StoppedCapacityAccount::open(
        directory,
        measured.account_identity,
        onboarding.try_clone()?,
        state.try_clone()?,
        issuance,
        &policy,
    )?;
    owner.require_no_outstanding_debit(&policy)?;
    state_lock.ensure_protects_app_root(app_root)?;

    if let Some(log) = state.open_pinned_regular(OsStr::new(REANCHOR_EVIDENCE_LOG), false)? {
        require_owned_log(&log)?;
    }
    let prior_binding_digest = prior.binding_digest()?;
    let body = ReanchorBody {
        schema: REANCHOR_SCHEMA.to_owned(),
        operator_fingerprint,
        node_fingerprint: report.node_fingerprint,
        issuance_digest: hex_digest(&issuance),
        prior_binding_digest: prior_binding_digest.clone(),
        root_identity: measured.root_identity,
        runtime_identity: measured.runtime_identity,
        account_directory_identity: measured.account_directory_identity,
        account_identity: measured.account_identity,
        reason: reason.to_owned(),
        anchored_at: lillux::time::iso8601_now(),
    };
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
    let existing = onboarding.open_pinned_regular(OsStr::new(REANCHOR_DOCUMENT_NAME), false)?;
    onboarding.atomic_write_pinned_if_same(
        OsStr::new(REANCHOR_DOCUMENT_NAME),
        existing.as_ref(),
        &document_bytes,
        0o600,
        crate::NODE_TEMP_PURPOSE,
    )?;
    onboarding.sync()?;
    // The completion now embeds the re-anchored binding and names the
    // document; until it is resealed boot still refuses the old places.
    crate::init::seal_current_init_completion(app_root, None)?;

    let document_digest = hex_digest(&sha2::Sha256::digest(&document_bytes).into());
    append_evidence(
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

/// Every recorded re-anchor, oldest first. An unreadable line is an error.
pub fn read_capacity_reanchors(state_dir: &Path) -> Result<Vec<CapacityReanchorEvidence>> {
    let Some(directory) = PinnedDirectory::open(state_dir)? else {
        return Ok(Vec::new());
    };
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
