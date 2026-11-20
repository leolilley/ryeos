//! The node journal: a hash-chained, node-signed record of what happens to the
//! node itself and that nothing else records.
//!
//! Records live as files in a directory under the node state directory that
//! only the daemon writes; they are not in the runtime database. Each record
//! is signed with the node key (the daemon never holds an operator key) and
//! carries the digest of the one before it. Its time, `recorded_at`, is inside
//! the signed body: the time is part of what the signature vouches for.
//!
//! The chain, its files and its count-based rotation into a node-signed
//! checkpoint are the offline-operation ledger's, shared through
//! [`crate::signed_chain`]. The journal keeps the newest records and folds the
//! oldest into a checkpoint that carries the chain tip, so history is bounded
//! and a reader can say where it starts.
//!
//! The type vocabulary is closed mechanism names, never wording; a view turns
//! a type into text. `detail` is a bounded flat object of mechanical fields.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use lillux::crypto::{Signer, SigningKey, Verifier, VerifyingKey};
use ryeos_engine::roots::RuntimeRoot;
use ryeos_node_runtime::identity::NodeIdentity;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::lifecycle_marker::{LaunchRoute, UncleanExit};
use crate::offline_operation::contract::{
    CanonicalInstant, canonical_bytes, decode_signature, digest_of, encode_signature,
};
use crate::signed_chain::{Chain, ChainEntry};

pub const RECORD_SCHEMA: &str = "ryeos/node-journal-record/v1";
pub const CHECKPOINT_SCHEMA: &str = "ryeos/node-journal-checkpoint/v1";
/// A record file larger than this is refused unread and never written.
pub const MAX_RECORD_BYTES: u64 = 16 * 1024;
/// Records kept before rotation drops the oldest into a checkpoint.
pub const JOURNAL_ROTATE_AT: usize = 1024;
/// Records kept after a rotation.
pub const JOURNAL_KEEP: usize = 512;

/// Most fields a record's `detail` holds.
pub const MAX_DETAIL_FIELDS: usize = 16;
/// Longest `detail` key, in bytes.
pub const MAX_DETAIL_KEY_BYTES: usize = 64;
/// Longest string in `detail`, in bytes.
pub const MAX_DETAIL_STRING_BYTES: usize = 512;
/// Longest reference (`subject_ref`, `before_ref`, ...), in bytes.
pub const MAX_REF_BYTES: usize = 256;

const DIRECTORY: &str = "node-journal";

/// The closed set of things the journal records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalType {
    #[serde(rename = "node.started")]
    NodeStarted,
    #[serde(rename = "node.exited")]
    NodeExited,
    #[serde(rename = "node.exit_inferred_unclean")]
    NodeExitInferredUnclean,
}

impl JournalType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NodeStarted => "node.started",
            Self::NodeExited => "node.exited",
            Self::NodeExitInferredUnclean => "node.exit_inferred_unclean",
        }
    }
}

/// The signed content of one journal record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalBody {
    pub schema: String,
    /// Position in the chain, from 1.
    pub sequence: u64,
    /// Digest of the previous record's body, or the genesis head.
    pub previous: String,
    pub node_fingerprint: String,
    /// When the record was written. Inside the signed body on purpose: this
    /// time is the evidence, not a timestamp beside it.
    pub recorded_at: String,
    #[serde(rename = "type")]
    pub record_type: JournalType,
    pub subject_ref: Option<String>,
    pub before_ref: Option<String>,
    pub after_ref: Option<String>,
    /// The operator fingerprint when an authenticated request caused this, as
    /// the daemon attests it; null otherwise.
    pub actor: Option<String>,
    /// Who vouches for the record when that is not the node itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attested_by: Option<String>,
    pub detail: BTreeMap<String, Value>,
}

impl JournalBody {
    /// The bounds on what a record may hold.
    pub fn validate_shape(&self) -> Result<()> {
        ensure!(
            self.schema == RECORD_SCHEMA,
            "journal record schema mismatch"
        );
        CanonicalInstant::parse("recorded_at", &self.recorded_at)?;
        for (name, reference) in [
            ("subject_ref", &self.subject_ref),
            ("before_ref", &self.before_ref),
            ("after_ref", &self.after_ref),
            ("actor", &self.actor),
            ("attested_by", &self.attested_by),
        ] {
            if let Some(reference) = reference {
                ensure!(
                    !reference.is_empty() && reference.len() <= MAX_REF_BYTES,
                    "journal {name} must be 1 to {MAX_REF_BYTES} bytes"
                );
            }
        }
        ensure!(
            self.detail.len() <= MAX_DETAIL_FIELDS,
            "journal detail holds more than {MAX_DETAIL_FIELDS} fields"
        );
        for (key, value) in &self.detail {
            ensure!(
                !key.is_empty() && key.len() <= MAX_DETAIL_KEY_BYTES,
                "journal detail key must be 1 to {MAX_DETAIL_KEY_BYTES} bytes"
            );
            match value {
                Value::Null | Value::Bool(_) | Value::Number(_) => {}
                Value::String(text) => ensure!(
                    text.len() <= MAX_DETAIL_STRING_BYTES,
                    "journal detail `{key}` exceeds {MAX_DETAIL_STRING_BYTES} bytes"
                ),
                Value::Array(_) | Value::Object(_) => {
                    anyhow::bail!("journal detail `{key}` is not a flat mechanical value")
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedJournalRecord {
    pub body: JournalBody,
    /// Signed with the node key over the canonical body.
    pub signature: String,
}

impl SignedJournalRecord {
    pub fn sign(body: JournalBody, node_key: &SigningKey) -> Result<Self> {
        body.validate_shape()?;
        let signature = node_key.sign(&canonical_bytes(&body)?);
        let record = Self {
            body,
            signature: encode_signature(&signature),
        };
        ensure!(
            u64::try_from(serde_json::to_vec(&record)?.len())? <= MAX_RECORD_BYTES,
            "journal record exceeds {MAX_RECORD_BYTES} bytes"
        );
        Ok(record)
    }

    pub fn digest(&self) -> Result<String> {
        digest_of(&self.body)
    }

    pub fn verify(&self, node_key: &VerifyingKey) -> Result<()> {
        self.body.validate_shape()?;
        node_key
            .verify(
                &canonical_bytes(&self.body)?,
                &decode_signature(&self.signature)?,
            )
            .context("journal record signature does not verify")
    }
}

/// The head of a journal with no records. Binding the node's fingerprint makes
/// a head signed for one node meaningless on another.
pub fn genesis_journal_head(node_fingerprint: &str) -> String {
    lillux::sha256_hex(format!("ryeos/node-journal-genesis/v1:{node_fingerprint}").as_bytes())
}

impl ChainEntry for SignedJournalRecord {
    const NOUN: &'static str = "journal record";
    const LEDGER_NAME: &'static str = "node journal";
    const SCHEMA: &'static str = RECORD_SCHEMA;
    const CHECKPOINT_SCHEMA: &'static str = CHECKPOINT_SCHEMA;
    const MAX_BYTES: u64 = MAX_RECORD_BYTES;
    const ROTATE_AT: usize = JOURNAL_ROTATE_AT;
    const KEEP: usize = JOURNAL_KEEP;

    fn genesis_head(node_fingerprint: &str) -> String {
        genesis_journal_head(node_fingerprint)
    }

    fn sequence(&self) -> u64 {
        self.body.sequence
    }

    fn previous(&self) -> &str {
        &self.body.previous
    }

    fn node_fingerprint(&self) -> &str {
        &self.body.node_fingerprint
    }

    fn schema(&self) -> &str {
        &self.body.schema
    }

    fn recorded_at(&self) -> &str {
        &self.body.recorded_at
    }

    fn digest(&self) -> Result<String> {
        SignedJournalRecord::digest(self)
    }

    fn verify(&self, key: &VerifyingKey) -> Result<()> {
        SignedJournalRecord::verify(self, key)
    }
}

/// What a verified read of the journal found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalState {
    /// Digest of the last record (or the checkpoint's tip, or the genesis).
    pub head: String,
    pub last_sequence: u64,
    pub retained: usize,
    pub dropped_records: u64,
    pub oldest_recorded_at: Option<String>,
}

/// The retained records and where the history they hold starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRead {
    /// Oldest first.
    pub records: Vec<SignedJournalRecord>,
    /// When the oldest retained record was written, present exactly when
    /// rotation has dropped earlier ones: nothing before it is held.
    pub truncated_before: Option<String>,
    pub dropped_records: u64,
}

/// What the caller supplies for one record; the journal adds the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalDraft {
    pub record_type: JournalType,
    pub subject_ref: Option<String>,
    pub before_ref: Option<String>,
    pub after_ref: Option<String>,
    pub actor: Option<String>,
    pub attested_by: Option<String>,
    pub detail: BTreeMap<String, Value>,
}

impl JournalDraft {
    pub fn new(record_type: JournalType) -> Self {
        Self {
            record_type,
            subject_ref: None,
            before_ref: None,
            after_ref: None,
            actor: None,
            attested_by: None,
            detail: BTreeMap::new(),
        }
    }

    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.detail.insert(key.to_owned(), value.into());
        self
    }
}

/// Why the journal could not be read or written. The two are handled apart:
/// a chain that does not verify is a fact about the node's integrity, a write
/// that failed is an ordinary state-write failure.
#[derive(Debug)]
pub enum JournalError {
    /// The retained records are unreadable, unsigned, out of chain or not this
    /// node's. Nothing was written.
    Integrity(anyhow::Error),
    /// The directory could not be created or a record could not be written.
    Write(anyhow::Error),
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Integrity(error) => write!(formatter, "node journal does not verify: {error:#}"),
            Self::Write(error) => write!(formatter, "node journal write failed: {error:#}"),
        }
    }
}

impl std::error::Error for JournalError {}

/// The daemon's journal. Only the daemon process, holding the state lock,
/// writes it.
#[derive(Clone)]
pub struct NodeJournal {
    dir: PathBuf,
    node: NodeIdentity,
}

impl NodeJournal {
    pub fn for_app_root(app_root: &Path, node: NodeIdentity) -> Self {
        Self {
            dir: RuntimeRoot::new(app_root.to_path_buf())
                .state()
                .join(DIRECTORY),
            node,
        }
    }

    fn chain(&self) -> Chain<'_, SignedJournalRecord> {
        Chain::new(self.dir.clone(), &self.node)
    }

    fn state_of(tip: crate::signed_chain::ChainTip) -> JournalState {
        JournalState {
            head: tip.head,
            last_sequence: tip.last_sequence,
            retained: tip.retained,
            dropped_records: tip.dropped,
            oldest_recorded_at: tip.oldest_recorded_at,
        }
    }

    /// Read and verify the retained chain.
    pub fn load(&self) -> Result<JournalState> {
        Ok(Self::state_of(self.chain().walk(|_, _| {})?))
    }

    /// Verified records, oldest first, and where the held history starts.
    pub fn read(&self) -> Result<JournalRead> {
        let chain = self.chain();
        let tip = chain.walk(|_, _| {})?;
        let records = chain.recent(tip.last_sequence, usize::MAX)?;
        Ok(JournalRead {
            records,
            truncated_before: (tip.dropped > 0)
                .then(|| tip.oldest_recorded_at.clone())
                .flatten(),
            dropped_records: tip.dropped,
        })
    }

    fn create_dir(&self) -> Result<()> {
        lillux::PinnedDirectory::open_or_create(&self.dir)?.tighten_owner_private_directory()
    }

    fn append_after(
        &self,
        state: &JournalState,
        draft: JournalDraft,
    ) -> Result<(SignedJournalRecord, JournalState)> {
        let chain = self.chain();
        let sequence = state.last_sequence + 1;
        let record = SignedJournalRecord::sign(
            JournalBody {
                schema: RECORD_SCHEMA.to_owned(),
                sequence,
                previous: state.head.clone(),
                node_fingerprint: self.node.fingerprint().to_owned(),
                recorded_at: lillux::time::iso8601_now(),
                record_type: draft.record_type,
                subject_ref: draft.subject_ref,
                before_ref: draft.before_ref,
                after_ref: draft.after_ref,
                actor: draft.actor,
                attested_by: draft.attested_by,
                detail: draft.detail,
            },
            self.node.signing_key(),
        )?;
        chain.write_entry(sequence, &record)?;
        let mut next = state.clone();
        next.head = record.digest()?;
        next.last_sequence = sequence;
        next.retained += 1;
        if next.oldest_recorded_at.is_none() {
            next.oldest_recorded_at = Some(record.body.recorded_at.clone());
        }
        if next.retained > JOURNAL_ROTATE_AT {
            chain.rotate()?;
            let tip = chain.walk(|_, _| {})?;
            return Ok((record, Self::state_of(tip)));
        }
        Ok((record, next))
    }

    /// Append one record after verifying the chain it extends.
    pub fn append(&self, draft: JournalDraft) -> Result<SignedJournalRecord, JournalError> {
        self.create_dir().map_err(JournalError::Write)?;
        let state = self.load().map_err(JournalError::Integrity)?;
        self.append_after(&state, draft)
            .map(|(record, _)| record)
            .map_err(JournalError::Write)
    }

    /// Record a daemon start. When the previous run left a `running` marker
    /// whose process is gone, first record that run's inferred unclean exit,
    /// unless the journal already holds that exact run's (the same start time
    /// and pid): one lost run is one record, however many starts follow.
    pub fn record_start(
        &self,
        unclean: Option<&UncleanExit>,
        started: &StartFacts<'_>,
    ) -> Result<(), JournalError> {
        self.create_dir().map_err(JournalError::Write)?;
        let mut already = false;
        let tip = self
            .chain()
            .walk(|record, _| {
                if let Some(unclean) = unclean
                    && record.body.record_type == JournalType::NodeExitInferredUnclean
                    && record.body.detail.get("started_at").and_then(Value::as_str)
                        == Some(unclean.started_at.as_str())
                    && record.body.detail.get("pid").and_then(Value::as_u64)
                        == Some(u64::from(unclean.pid))
                {
                    already = true;
                }
            })
            .map_err(JournalError::Integrity)?;
        let mut state = Self::state_of(tip);
        if let Some(unclean) = unclean
            && !already
        {
            let (_, next) = self
                .append_after(&state, unclean_draft(unclean))
                .map_err(JournalError::Write)?;
            state = next;
        }
        self.append_after(&state, started_draft(started))
            .map_err(JournalError::Write)?;
        Ok(())
    }

    /// Record a handled exit.
    pub fn record_exit(&self, exit: &ExitFacts<'_>) -> Result<(), JournalError> {
        self.append(exited_draft(exit)).map(|_| ())
    }
}

/// What a start knows about itself.
pub struct StartFacts<'a> {
    pub version: &'a str,
    pub revision: Option<&'a str>,
    pub profile: Option<&'a str>,
    pub pid: u32,
    pub started_at: &'a str,
    pub launch_route: LaunchRoute,
}

/// What a handled exit knows.
pub struct ExitFacts<'a> {
    pub reason: &'a str,
    pub error: Option<&'a str>,
    pub pid: u32,
    pub started_at: &'a str,
    pub launch_route: LaunchRoute,
}

fn launch_route_name(route: LaunchRoute) -> &'static str {
    match route {
        LaunchRoute::Direct => "direct",
        LaunchRoute::Supervised => "supervised",
    }
}

fn bounded(text: &str) -> String {
    let mut end = text.len().min(MAX_DETAIL_STRING_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn started_draft(facts: &StartFacts<'_>) -> JournalDraft {
    let mut draft = JournalDraft::new(JournalType::NodeStarted)
        .with("version", bounded(facts.version))
        .with("pid", facts.pid)
        .with("started_at", facts.started_at)
        .with("launch_route", launch_route_name(facts.launch_route));
    if let Some(revision) = facts.revision {
        draft = draft.with("revision", bounded(revision));
    }
    if let Some(profile) = facts.profile {
        draft = draft.with("profile", bounded(profile));
    }
    draft
}

fn exited_draft(facts: &ExitFacts<'_>) -> JournalDraft {
    let clean = facts.error.is_none() && facts.reason != "startup_failed";
    let mut draft = JournalDraft::new(JournalType::NodeExited)
        .with("reason", bounded(facts.reason))
        .with("clean", clean)
        .with("pid", facts.pid)
        .with("started_at", facts.started_at)
        .with("launch_route", launch_route_name(facts.launch_route));
    if let Some(error) = facts.error {
        draft = draft.with("error", bounded(error.trim()));
    }
    draft
}

fn unclean_draft(unclean: &UncleanExit) -> JournalDraft {
    let mut draft = JournalDraft::new(JournalType::NodeExitInferredUnclean)
        .with("pid", unclean.pid)
        .with("started_at", unclean.started_at.as_str());
    if let Some(route) = unclean.launch_route {
        draft = draft.with("launch_route", launch_route_name(route));
    }
    draft
}

#[cfg(test)]
mod tests;
