//! Protected single-binding journal owned by an external candidate supervisor.
//!
//! Fresh creation durably records launch intent before a native launcher is
//! prepared. Reopen is deliberately recovery-only: it cannot claim or finish
//! applications and cannot manufacture a launcher. Actual side effects occur
//! while an opaque one-shot application token holds the same SQLite writer
//! gate used by sticky revocation.

use std::ffi::{OsStr, OsString};
use std::fs::File;

use anyhow::{Context as _, Result, ensure};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

use super::export::{ValidatedCandidateRetention, validated_retained_candidate_root_sets};
use super::journal::{self, ApplicationClaim, JournalOwner};
use super::{
    AuthenticatedExecutionFrame, ChannelDirection, ExecutionChannelBinding,
    ExecutionChannelPayload, SignedExecutionFrame,
};
use crate::{
    DurableCasPublicationKey, DurableCasUploadStage, DurableExternalCandidateReceipt,
    PinnedStateAuthority,
};

const DATABASE_NAME: &str = "external-candidate.sqlite3";
const APPLICATION_ID: i32 = 0x5259_4547; // RYEG
const SCHEMA_EPOCH: i64 = 5;

const OWNER_SQL: &str = r#"
CREATE TABLE external_guest_meta (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    schema_epoch INTEGER NOT NULL CHECK(schema_epoch=5),
    bootstrap_digest TEXT NOT NULL,
    binding_digest TEXT NOT NULL UNIQUE,
    journal_nonce TEXT NOT NULL UNIQUE,
    directory_identity_json TEXT NOT NULL,
    database_identity_json TEXT NOT NULL,
    state_runtime_identity_json TEXT NOT NULL,
    lifecycle TEXT NOT NULL CHECK(lifecycle IN ('launch_intent','launcher_bound','ready'))
);
CREATE TABLE external_guest_launcher_occurrence (
    binding_digest TEXT PRIMARY KEY,
    process_identity_json TEXT NOT NULL,
    launcher_artifact_digest TEXT NOT NULL,
    channel_challenge_digest TEXT NOT NULL,
    FOREIGN KEY(binding_digest) REFERENCES external_execution_channel(binding_digest)
);
CREATE TABLE external_guest_launcher_ready (
    binding_digest TEXT PRIMARY KEY,
    handshake_transcript_digest TEXT NOT NULL,
    FOREIGN KEY(binding_digest) REFERENCES external_guest_launcher_occurrence(binding_digest)
);
CREATE TABLE external_guest_terminal_application (
    binding_digest TEXT PRIMARY KEY,
    frame_digest TEXT NOT NULL UNIQUE,
    application TEXT NOT NULL CHECK(application IN ('claimed','applied')),
    FOREIGN KEY(binding_digest) REFERENCES external_execution_channel(binding_digest)
);
CREATE TABLE external_guest_export_retention (
    binding_digest TEXT PRIMARY KEY,
    sealed_frame_digest TEXT NOT NULL UNIQUE,
    occurrence_digest TEXT NOT NULL UNIQUE,
    durable_receipt_id TEXT NOT NULL UNIQUE,
    snapshot_hash TEXT NOT NULL,
    completion_request_digest TEXT NOT NULL,
    writer_exclusion_evidence_hash TEXT NOT NULL,
    FOREIGN KEY(binding_digest) REFERENCES external_execution_channel(binding_digest)
);
CREATE TABLE external_guest_native_capture (
    binding_digest TEXT PRIMARY KEY,
    quiesce_frame_digest TEXT NOT NULL UNIQUE,
    occurrence_digest TEXT NOT NULL UNIQUE,
    durable_stage_id TEXT NOT NULL UNIQUE,
    snapshot_hash TEXT NOT NULL,
    completion_request_digest TEXT NOT NULL,
    writer_exclusion_evidence_hash TEXT NOT NULL,
    FOREIGN KEY(binding_digest) REFERENCES external_execution_channel(binding_digest)
);
CREATE TRIGGER external_guest_meta_singleton
BEFORE INSERT ON external_guest_meta
WHEN NEW.singleton!=1 OR EXISTS(SELECT 1 FROM external_guest_meta)
BEGIN SELECT RAISE(ABORT, 'external guest metadata is a singleton'); END;
CREATE TRIGGER external_guest_meta_no_delete
BEFORE DELETE ON external_guest_meta
BEGIN SELECT RAISE(ABORT, 'external guest metadata cannot be deleted'); END;
CREATE TRIGGER external_guest_meta_transition
BEFORE UPDATE ON external_guest_meta
WHEN NEW.singleton!=OLD.singleton OR NEW.schema_epoch!=OLD.schema_epoch
 OR NEW.bootstrap_digest!=OLD.bootstrap_digest OR NEW.binding_digest!=OLD.binding_digest
 OR NEW.journal_nonce!=OLD.journal_nonce
 OR NEW.directory_identity_json!=OLD.directory_identity_json
 OR NEW.database_identity_json!=OLD.database_identity_json
 OR NEW.state_runtime_identity_json!=OLD.state_runtime_identity_json
 OR NOT ((OLD.lifecycle='launch_intent' AND NEW.lifecycle='launcher_bound')
      OR (OLD.lifecycle='launcher_bound' AND NEW.lifecycle='ready'))
BEGIN SELECT RAISE(ABORT, 'external guest metadata cannot be rewritten'); END;
CREATE TRIGGER external_guest_launcher_occurrence_no_update
BEFORE UPDATE ON external_guest_launcher_occurrence
BEGIN SELECT RAISE(ABORT, 'external guest launcher occurrence is immutable'); END;
CREATE TRIGGER external_guest_launcher_occurrence_no_delete
BEFORE DELETE ON external_guest_launcher_occurrence
BEGIN SELECT RAISE(ABORT, 'external guest launcher occurrence cannot be deleted'); END;
CREATE TRIGGER external_guest_launcher_ready_no_update
BEFORE UPDATE ON external_guest_launcher_ready
BEGIN SELECT RAISE(ABORT, 'external guest launcher readiness is immutable'); END;
CREATE TRIGGER external_guest_launcher_ready_no_delete
BEFORE DELETE ON external_guest_launcher_ready
BEGIN SELECT RAISE(ABORT, 'external guest launcher readiness cannot be deleted'); END;
CREATE TRIGGER external_guest_terminal_application_immutable
BEFORE UPDATE ON external_guest_terminal_application
WHEN NEW.binding_digest!=OLD.binding_digest OR NEW.frame_digest!=OLD.frame_digest
 OR NOT (OLD.application='claimed' AND NEW.application='applied')
BEGIN SELECT RAISE(ABORT, 'external guest terminal application cannot be rewritten'); END;
CREATE TRIGGER external_guest_terminal_application_no_delete
BEFORE DELETE ON external_guest_terminal_application
BEGIN SELECT RAISE(ABORT, 'external guest terminal application cannot be deleted'); END;
CREATE TRIGGER external_guest_single_channel
BEFORE INSERT ON external_execution_channel
WHEN EXISTS(SELECT 1 FROM external_execution_channel)
 OR NEW.binding_digest!=(SELECT binding_digest FROM external_guest_meta WHERE singleton=1)
BEGIN SELECT RAISE(ABORT, 'external guest store admits one exact channel'); END;
CREATE TRIGGER external_guest_channel_no_delete
BEFORE DELETE ON external_execution_channel
BEGIN SELECT RAISE(ABORT, 'external guest channel cannot be deleted'); END;
CREATE TRIGGER external_guest_frame_no_delete
BEFORE DELETE ON external_execution_frame
BEGIN SELECT RAISE(ABORT, 'external guest transcript cannot be deleted'); END;
CREATE TRIGGER external_guest_retention_no_update
BEFORE UPDATE ON external_guest_export_retention
BEGIN SELECT RAISE(ABORT, 'external guest export retention is immutable'); END;
CREATE TRIGGER external_guest_retention_no_delete
BEFORE DELETE ON external_guest_export_retention
BEGIN SELECT RAISE(ABORT, 'external guest export retention requires disposition'); END;
CREATE TRIGGER external_guest_native_capture_no_update
BEFORE UPDATE ON external_guest_native_capture
BEGIN SELECT RAISE(ABORT, 'external guest native capture is immutable'); END;
CREATE TRIGGER external_guest_native_capture_no_delete
BEFORE DELETE ON external_guest_native_capture
BEGIN SELECT RAISE(ABORT, 'external guest native capture requires reconciliation'); END;
"#;

fn complete_schema() -> String {
    format!("{}\n{}", journal::CHANNEL_SQL, OWNER_SQL)
}

fn hash(value: &str, label: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{label} is not a canonical SHA-256 digest"
    );
    Ok(())
}

struct GuestStore {
    conn: Connection,
    directory: lillux::PinnedDirectory,
    _lifetime_lock: lillux::PinnedDirectoryLock,
    database_file: File,
    bootstrap_digest: String,
    state_runtime_identity_json: String,
    binding: ExecutionChannelBinding,
    store_identity: GuestJournalStoreIdentity,
}

/// Outer-owner anchor for an exact guest journal inode. This value is retained
/// beside allocation/recovery authority, never reconstructed from the database
/// it authenticates.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestJournalStoreIdentity {
    schema: u32,
    journal_nonce: String,
    directory_identity: lillux::PinnedDirectoryIdentity,
    database_identity: lillux::PinnedRegularFileIdentity,
    bootstrap_digest: String,
    binding_digest: String,
    state_runtime_identity_json: String,
}

/// Fresh launch intent. This type exists before native preparation and can
/// become live exactly once after the dedicated launcher is bound.
pub struct PreparedGuestJournal(GuestStore);

/// Launch intent joined durably to one exact held process and inherited local
/// channel challenge, but not yet authenticated as ready.
pub struct BoundGuestJournal(GuestStore);

/// Journal authority held only by the live protected supervisor actor.
pub struct LiveGuestJournal(GuestStore);

/// Reopened evidence and revocation authority. It intentionally has no path
/// back to [`LiveGuestJournal`] and exposes no claim, finish or retention API.
pub struct RecoveredGuestJournal(GuestStore);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LauncherOccurrenceEvidence {
    process_identity: lillux::ExactProcessIdentity,
    launcher_artifact_digest: String,
    channel_challenge_digest: String,
}

impl LauncherOccurrenceEvidence {
    pub fn from_held_launcher(
        process_identity: lillux::ExactProcessIdentity,
        launcher_artifact_digest: &str,
        channel_challenge_digest: &str,
    ) -> Result<Self> {
        hash(launcher_artifact_digest, "launcher artifact digest")?;
        hash(
            channel_challenge_digest,
            "launcher channel challenge digest",
        )?;
        Ok(Self {
            process_identity,
            launcher_artifact_digest: launcher_artifact_digest.to_owned(),
            channel_challenge_digest: channel_challenge_digest.to_owned(),
        })
    }

    pub fn digest(&self) -> Result<String> {
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&serde_json::to_value(self)?)?.as_bytes(),
        ))
    }
}

pub struct AuthenticatedLauncherReady {
    handshake_transcript_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableNativeCandidateCapture {
    pub quiesce_frame_digest: String,
    pub occurrence_digest: String,
    pub durable_stage_id: String,
    pub snapshot_hash: String,
    pub completion_request_digest: String,
    pub writer_exclusion_evidence_hash: String,
}

impl AuthenticatedLauncherReady {
    /// The executor may construct this only after the exact inherited channel
    /// completes its bounded challenge/binding handshake.
    pub fn from_handshake_transcript(handshake_transcript_digest: &str) -> Result<Self> {
        hash(
            handshake_transcript_digest,
            "launcher handshake transcript digest",
        )?;
        Ok(Self {
            handshake_transcript_digest: handshake_transcript_digest.to_owned(),
        })
    }
}

/// Only a fresh durable claim can mint this process-local execution authority.
/// It is deliberately neither cloneable nor serializable.
pub struct GuestApplicationToken {
    frame: AuthenticatedExecutionFrame,
    protocol_offset: usize,
}

impl GuestApplicationToken {
    pub fn frame_digest(&self) -> &str {
        self.frame.digest()
    }
}

pub enum GuestApplicationClaim {
    New(GuestApplicationToken),
    AlreadyClaimed,
    AlreadyApplied,
    Revoked,
}

/// A bounded native effect completed while its durable writer gate was held.
/// Durable application finish consumes this value.
pub struct PerformedGuestApplication(AuthenticatedExecutionFrame);

pub enum GuestProtocolApplication {
    Pending(GuestApplicationToken),
    Performed(PerformedGuestApplication),
}

pub struct GuestApplicationAck {
    direction: ChannelDirection,
    sequence: u64,
    frame_digest: String,
}

pub struct CommittedGuestRevocation {
    frame: AuthenticatedExecutionFrame,
    wire: Vec<u8>,
    newly_recorded: bool,
}

/// Only a fresh durable terminal claim can mint this process-local stop
/// authority. A crash after this token is minted is an uncertain stop and can
/// never mint another token.
pub struct GuestTerminalApplicationToken(AuthenticatedExecutionFrame);

pub enum GuestTerminalApplicationClaim {
    New(GuestTerminalApplicationToken),
    AlreadyClaimed,
    AlreadyApplied,
}

pub struct PerformedGuestTerminalApplication(AuthenticatedExecutionFrame);

impl CommittedGuestRevocation {
    pub fn frame_digest(&self) -> &str {
        self.frame.digest()
    }

    pub fn newly_recorded(&self) -> bool {
        self.newly_recorded
    }

    pub fn direction(&self) -> ChannelDirection {
        self.frame.frame().direction
    }

    pub fn sequence(&self) -> u64 {
        self.frame.frame().sequence
    }
}

pub enum RevocationAppendOutcome {
    Appended,
    AlreadyPresent,
    Blocked {
        revocation_committed: bool,
        error: anyhow::Error,
    },
}

impl GuestApplicationAck {
    pub fn direction(&self) -> ChannelDirection {
        self.direction
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn frame_digest(&self) -> &str {
        &self.frame_digest
    }
}

impl PreparedGuestJournal {
    pub fn binding(&self) -> &ExecutionChannelBinding {
        &self.0.binding
    }

    pub fn store_identity(&self) -> &GuestJournalStoreIdentity {
        &self.0.store_identity
    }

    pub fn bootstrap_digest(&self) -> &str {
        &self.0.bootstrap_digest
    }

    pub fn create(
        directory: lillux::PinnedDirectory,
        authority: &PinnedStateAuthority,
        bootstrap_digest: &str,
        binding: ExecutionChannelBinding,
    ) -> Result<Self> {
        hash(bootstrap_digest, "external bootstrap digest")?;
        binding.validate()?;
        directory.require_owner_private_directory()?;
        let lifetime_lock = directory
            .try_lock_exclusive()?
            .context("external guest directory already has a live owner")?;
        lifetime_lock.ensure_protects(&directory)?;
        ensure!(
            directory.entry_names()?.is_empty(),
            "external guest store directory is not fresh"
        );
        let database_file =
            directory.open_regular_create(OsStr::new(DATABASE_NAME), true, true, 0o600)?;
        directory.sync()?;
        let conn = open_exact(&directory, &database_file)?;
        configure(&conn)?;
        let schema = complete_schema();
        conn.execute_batch(&schema)?;
        conn.pragma_update(None, "application_id", APPLICATION_ID)?;
        conn.pragma_update(None, "user_version", SCHEMA_EPOCH)?;
        let binding_digest = binding.digest()?;
        let binding_json = lillux::canonical_json(&serde_json::to_value(&binding)?)?;
        let directory_identity_json =
            lillux::canonical_json(&serde_json::to_value(directory.identity()?)?)?;
        let state_runtime_identity_json = lillux::canonical_json(&serde_json::to_value(
            authority.runtime_directory().identity()?,
        )?)?;
        let journal_nonce = lillux::sha256_hex(&lillux::crypto::generate_random_bytes::<32>());
        let database_identity = lillux::pinned_regular_file_identity(&database_file)?;
        let database_identity_json =
            lillux::canonical_json(&serde_json::to_value(database_identity)?)?;
        let store_identity = GuestJournalStoreIdentity {
            schema: 1,
            journal_nonce: journal_nonce.clone(),
            directory_identity: directory.identity()?,
            database_identity,
            bootstrap_digest: bootstrap_digest.to_owned(),
            binding_digest: binding_digest.clone(),
            state_runtime_identity_json: state_runtime_identity_json.clone(),
        };
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO external_guest_meta VALUES(1,?1,?2,?3,?4,?5,?6,?7,'launch_intent')",
            params![
                SCHEMA_EPOCH,
                bootstrap_digest,
                binding_digest,
                journal_nonce,
                directory_identity_json,
                database_identity_json,
                state_runtime_identity_json
            ],
        )?;
        tx.execute(
            "INSERT INTO external_execution_channel VALUES(?1,?2,?3,'prepared',NULL,NULL,NULL)",
            params![binding.placement_thread_id, binding_digest, binding_json],
        )?;
        tx.commit()?;
        database_file.sync_all()?;
        directory.sync()?;
        let store = GuestStore {
            conn,
            directory,
            _lifetime_lock: lifetime_lock,
            database_file,
            bootstrap_digest: bootstrap_digest.to_owned(),
            state_runtime_identity_json,
            binding,
            store_identity,
        };
        store.validate()?;
        Ok(Self(store))
    }

    /// Persist one exact held launcher occurrence before releasing it.
    pub fn bind_launcher(self, evidence: LauncherOccurrenceEvidence) -> Result<BoundGuestJournal> {
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        let tx = Transaction::new_unchecked(&self.0.conn, TransactionBehavior::Immediate)?;
        self.0.owner().require_owner(&tx, &self.0.binding)?;
        tx.execute(
            "INSERT INTO external_guest_launcher_occurrence VALUES(?1,?2,?3,?4)",
            params![
                self.0.binding.digest()?,
                lillux::canonical_json(&serde_json::to_value(evidence.process_identity)?)?,
                evidence.launcher_artifact_digest,
                evidence.channel_challenge_digest
            ],
        )?;
        let changed = tx.execute(
            "UPDATE external_guest_meta SET lifecycle='launcher_bound'
             WHERE singleton=1 AND lifecycle='launch_intent'",
            [],
        )?;
        ensure!(changed == 1, "external guest launch intent is not fresh");
        tx.commit()?;
        self.0.database_file.sync_all()?;
        self.0.validate()?;
        Ok(BoundGuestJournal(self.0))
    }
}

impl BoundGuestJournal {
    /// Persist readiness only after the live executor has authenticated the
    /// exact held occurrence over its inherited deadline-bounded channel.
    pub fn mark_launcher_ready(
        self,
        ready: AuthenticatedLauncherReady,
    ) -> Result<LiveGuestJournal> {
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        let tx = Transaction::new_unchecked(&self.0.conn, TransactionBehavior::Immediate)?;
        self.0.owner().require_owner(&tx, &self.0.binding)?;
        tx.execute(
            "INSERT INTO external_guest_launcher_ready VALUES(?1,?2)",
            params![self.0.binding.digest()?, ready.handshake_transcript_digest],
        )?;
        let changed = tx.execute(
            "UPDATE external_guest_meta SET lifecycle='ready'
             WHERE singleton=1 AND lifecycle='launcher_bound'",
            [],
        )?;
        ensure!(
            changed == 1,
            "external guest launcher occurrence is not bound"
        );
        tx.commit()?;
        self.0.database_file.sync_all()?;
        self.0.validate()?;
        Ok(LiveGuestJournal(self.0))
    }
}

impl RecoveredGuestJournal {
    pub fn open(
        directory: lillux::PinnedDirectory,
        authority: &PinnedStateAuthority,
        store_identity: &GuestJournalStoreIdentity,
        bootstrap_digest: &str,
        binding: &ExecutionChannelBinding,
    ) -> Result<Self> {
        let store = GuestStore::open_existing(
            directory,
            authority,
            store_identity,
            bootstrap_digest,
            binding,
        )?;
        store.validate_retained_content(authority)?;
        Ok(Self(store))
    }

    pub fn binding(&self) -> &ExecutionChannelBinding {
        &self.0.binding
    }

    /// Persist cancellation independently before attempting contiguous append.
    /// Recovery cannot apply the cancellation to a recreated launcher; cleanup
    /// remains a separate independently verified obligation.
    pub fn commit_terminal_revocation(&self, wire: &[u8]) -> Result<CommittedGuestRevocation> {
        self.0.commit_terminal_revocation(wire)
    }

    pub fn try_append_terminal_observation(
        &self,
        committed: &CommittedGuestRevocation,
    ) -> RevocationAppendOutcome {
        self.0.try_append_terminal_observation(committed)
    }

    pub fn validate(&self) -> Result<()> {
        self.0.validate()
    }
}

impl LiveGuestJournal {
    pub fn binding(&self) -> &ExecutionChannelBinding {
        &self.0.binding
    }

    pub fn native_capture_for_quiesce(
        &self,
        quiesce_frame_digest: &str,
    ) -> Result<Option<DurableNativeCandidateCapture>> {
        hash(quiesce_frame_digest, "quiesce frame digest")?;
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        self.0
            .conn
            .query_row(
                "SELECT quiesce_frame_digest,occurrence_digest,durable_stage_id,
                        snapshot_hash,completion_request_digest,writer_exclusion_evidence_hash
                 FROM external_guest_native_capture
                 WHERE binding_digest=?1 AND quiesce_frame_digest=?2",
                params![self.0.binding.digest()?, quiesce_frame_digest],
                |row| {
                    Ok(DurableNativeCandidateCapture {
                        quiesce_frame_digest: row.get(0)?,
                        occurrence_digest: row.get(1)?,
                        durable_stage_id: row.get(2)?,
                        snapshot_hash: row.get(3)?,
                        completion_request_digest: row.get(4)?,
                        writer_exclusion_evidence_hash: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn supervisor_export_for_capture(
        &self,
        capture: &DurableNativeCandidateCapture,
    ) -> Result<Option<AuthenticatedExecutionFrame>> {
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        let wire: Option<String> = self
            .0
            .conn
            .query_row(
                "SELECT frame_json FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='supervisor_to_owner'
                 AND json_extract(frame_json,'$.frame.payload.kind')='export_sealed'
                 AND json_extract(frame_json,'$.frame.payload.candidate_snapshot_hash')=?2
                 AND json_extract(frame_json,'$.frame.payload.completion_request_digest')=?3
                 AND json_extract(frame_json,'$.frame.payload.writer_exclusion_evidence_hash')=?4",
                params![
                    self.0.binding.digest()?,
                    capture.snapshot_hash,
                    capture.completion_request_digest,
                    capture.writer_exclusion_evidence_hash
                ],
                |row| row.get(0),
            )
            .optional()?;
        wire.map(|wire| {
            SignedExecutionFrame::decode_and_verify(
                wire.as_bytes(),
                &self.0.binding,
                self.0.binding.issued_at_ms,
            )
        })
        .transpose()
    }

    pub fn reconcile_native_capture_application(
        &self,
        capture: &DurableNativeCandidateCapture,
    ) -> Result<GuestApplicationAck> {
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        let retained = self
            .native_capture_for_quiesce(&capture.quiesce_frame_digest)?
            .context("native capture reconciliation has no durable evidence")?;
        ensure!(
            retained == *capture,
            "native capture reconciliation changed durable evidence"
        );
        let tx = Transaction::new_unchecked(&self.0.conn, TransactionBehavior::Immediate)?;
        let (sequence, wire): (i64, String) = tx.query_row(
            "SELECT sequence,frame_json FROM external_execution_frame
             WHERE binding_digest=?1 AND direction='owner_to_supervisor'
             AND frame_digest=?2
             AND json_extract(frame_json,'$.frame.payload.kind')='quiesce'",
            params![self.0.binding.digest()?, capture.quiesce_frame_digest],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let frame = SignedExecutionFrame::decode_and_verify(
            wire.as_bytes(),
            &self.0.binding,
            self.0.binding.issued_at_ms,
        )?;
        ensure!(
            matches!(
                &frame.frame().payload,
                ExecutionChannelPayload::Quiesce { completion_request_digest }
                    if completion_request_digest == &capture.completion_request_digest
            ),
            "native capture reconciliation changed completion authority"
        );
        journal::finish_application(
            &tx,
            &self.0.owner(),
            &self.0.binding.placement_thread_id,
            ChannelDirection::OwnerToSupervisor,
            u64::try_from(sequence)?,
            &capture.quiesce_frame_digest,
        )?;
        tx.commit()?;
        Ok(GuestApplicationAck {
            direction: ChannelDirection::OwnerToSupervisor,
            sequence: u64::try_from(sequence)?,
            frame_digest: capture.quiesce_frame_digest.clone(),
        })
    }

    pub fn record_frame(&self, wire: &[u8]) -> Result<bool> {
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        let verified = SignedExecutionFrame::decode_and_verify(
            wire,
            &self.0.binding,
            lillux::time::timestamp_millis(),
        )?;
        if matches!(verified.frame().payload, ExecutionChannelPayload::Cancel) {
            let committed = self.0.commit_terminal_revocation(wire)?;
            return match self.0.try_append_terminal_observation(&committed) {
                RevocationAppendOutcome::Appended => Ok(true),
                RevocationAppendOutcome::AlreadyPresent => Ok(false),
                RevocationAppendOutcome::Blocked { error, .. } => Err(error.context(
                    "external revocation committed but its transcript observation is blocked",
                )),
            };
        }
        let tx = Transaction::new_unchecked(&self.0.conn, TransactionBehavior::Immediate)?;
        let result = journal::append_frame(
            &tx,
            &self.0.owner(),
            &self.0.binding.placement_thread_id,
            wire,
        )?;
        tx.commit()?;
        Ok(result)
    }

    /// Retain, reconcile and complete one controller acknowledgement as an
    /// atomic no-effect application. This must be used only for received
    /// acknowledgements; locally authored frames remain pending remote proof.
    pub fn record_owner_acknowledgement(&self, wire: &[u8]) -> Result<bool> {
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        let verified = SignedExecutionFrame::decode_and_verify(
            wire,
            &self.0.binding,
            lillux::time::timestamp_millis(),
        )?;
        ensure!(
            verified.frame().direction == ChannelDirection::OwnerToSupervisor
                && matches!(
                    verified.frame().payload,
                    ExecutionChannelPayload::Acknowledge { .. }
                ),
            "guest acknowledgement ingress requires an owner acknowledgement"
        );
        let tx = Transaction::new_unchecked(&self.0.conn, TransactionBehavior::Immediate)?;
        let inserted = journal::append_frame(
            &tx,
            &self.0.owner(),
            &self.0.binding.placement_thread_id,
            wire,
        )?;
        journal::apply_received_acknowledgement(
            &tx,
            &self.0.owner(),
            &self.0.binding.placement_thread_id,
            verified.frame().direction,
            verified.frame().sequence,
            verified.digest(),
        )?;
        tx.commit()?;
        Ok(inserted)
    }

    /// Author one exact supervisor observation from the retained directional
    /// frontiers and append it in the same SQLite writer transaction. A commit
    /// error is uncertain publication and must never be retried by guessing a
    /// successor; recovery reads the journal instead.
    pub fn author_supervisor_frame(
        &self,
        signing_key: &lillux::crypto::SigningKey,
        payload: ExecutionChannelPayload,
    ) -> Result<AuthenticatedExecutionFrame> {
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        let tx = Transaction::new_unchecked(&self.0.conn, TransactionBehavior::Immediate)?;
        let verified = journal::author_frame(
            &tx,
            &self.0.owner(),
            &self.0.binding.placement_thread_id,
            ChannelDirection::SupervisorToOwner,
            signing_key,
            payload,
        )?;
        tx.commit()?;
        Ok(verified)
    }

    /// Recover or author a signed supervisor acknowledgement of the peer
    /// frame's current durable guest application state. Unlike node ingress,
    /// the outbound supervisor may acknowledge an owner acknowledgement once
    /// so its cumulative frontier can advance and that exact frame can serve
    /// as the reconnect poll.
    pub fn ensure_supervisor_acknowledgement(
        &self,
        signing_key: &lillux::crypto::SigningKey,
        peer_sequence: u64,
        peer_digest: &str,
    ) -> Result<Option<AuthenticatedExecutionFrame>> {
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        let tx = Transaction::new_unchecked(&self.0.conn, TransactionBehavior::Immediate)?;
        let binding_digest = self.0.binding.digest()?;
        let contiguous: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_execution_frame
             WHERE binding_digest=?1 AND direction='owner_to_supervisor'
               AND sequence=?2 AND frame_digest=?3)",
            params![binding_digest, i64::try_from(peer_sequence)?, peer_digest],
            |row| row.get(0),
        )?;
        let frame = if contiguous {
            journal::ensure_application_acknowledgement(
                &tx,
                &self.0.owner(),
                &self.0.binding.placement_thread_id,
                ChannelDirection::SupervisorToOwner,
                peer_sequence,
                peer_digest,
                signing_key,
                true,
            )?
        } else {
            let terminal: (String, String, String) = tx.query_row(
                "SELECT r.frame_digest,r.frame_json,t.application
                 FROM external_execution_revocation r
                 JOIN external_guest_terminal_application t
                   ON t.binding_digest=r.binding_digest
                  AND t.frame_digest=r.frame_digest
                 WHERE r.binding_digest=?1",
                [&binding_digest],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            let retained = SignedExecutionFrame::decode_and_verify(
                terminal.1.as_bytes(),
                &self.0.binding,
                self.0.binding.issued_at_ms,
            )?;
            ensure!(
                terminal.0 == peer_digest
                    && retained.digest() == peer_digest
                    && retained.frame().sequence == peer_sequence
                    && retained.frame().direction == ChannelDirection::OwnerToSupervisor
                    && matches!(retained.frame().payload, ExecutionChannelPayload::Cancel),
                "guest terminal acknowledgement changed sticky revocation"
            );
            let application = match terminal.2.as_str() {
                "claimed" => crate::external_execution::ExecutionFrameApplication::Claimed,
                "applied" => crate::external_execution::ExecutionFrameApplication::Applied,
                _ => anyhow::bail!("guest terminal application has an invalid state"),
            };
            journal::ensure_application_acknowledgement_for_state(
                &tx,
                &self.0.owner(),
                &self.0.binding.placement_thread_id,
                ChannelDirection::SupervisorToOwner,
                peer_sequence,
                peer_digest,
                application,
                signing_key,
            )?
        };
        tx.commit()?;
        Ok(frame)
    }

    pub fn pending_supervisor_transport_frames(
        &self,
        frame_limit: usize,
        byte_limit: usize,
    ) -> Result<Vec<journal::PendingExecutionFrame>> {
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        journal::pending_transport_frames(
            &self.0.conn,
            &self.0.owner(),
            &self.0.binding.placement_thread_id,
            ChannelDirection::SupervisorToOwner,
            frame_limit,
            byte_limit,
        )
    }

    pub fn claim(
        &self,
        direction: ChannelDirection,
        sequence: u64,
        digest: &str,
    ) -> Result<GuestApplicationClaim> {
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        let tx = Transaction::new_unchecked(&self.0.conn, TransactionBehavior::Immediate)?;
        let result = journal::claim_application(
            &tx,
            &self.0.owner(),
            &self.0.binding.placement_thread_id,
            direction,
            sequence,
            digest,
        )?;
        tx.commit()?;
        Ok(match result {
            ApplicationClaim::New(frame) => GuestApplicationClaim::New(GuestApplicationToken {
                frame,
                protocol_offset: 0,
            }),
            ApplicationClaim::AlreadyClaimed => GuestApplicationClaim::AlreadyClaimed,
            ApplicationClaim::AlreadyApplied => GuestApplicationClaim::AlreadyApplied,
            ApplicationClaim::Revoked => GuestApplicationClaim::Revoked,
        })
    }

    /// Retain and immediately claim one ordinary authenticated frame without
    /// asking the transport owner to reconstruct its signed coordinates.
    /// Cancellation uses the separate sticky-revocation API.
    pub fn record_and_claim(&self, wire: &[u8]) -> Result<GuestApplicationClaim> {
        let verified = SignedExecutionFrame::decode_and_verify(
            wire,
            &self.0.binding,
            lillux::time::timestamp_millis(),
        )?;
        ensure!(
            !matches!(verified.frame().payload, ExecutionChannelPayload::Cancel),
            "guest cancellation requires sticky revocation before append"
        );
        self.record_frame(wire)?;
        self.claim(
            verified.frame().direction,
            verified.frame().sequence,
            verified.digest(),
        )
    }

    /// Commit sticky cancellation independently of contiguous transcript
    /// append. This token alone can authorize the one live launcher stop.
    pub fn commit_terminal_revocation(&self, wire: &[u8]) -> Result<CommittedGuestRevocation> {
        self.0.commit_terminal_revocation(wire)
    }

    pub fn try_append_terminal_observation(
        &self,
        committed: &CommittedGuestRevocation,
    ) -> RevocationAppendOutcome {
        self.0.try_append_terminal_observation(committed)
    }

    pub fn claim_terminal_revocation(
        &self,
        committed: &CommittedGuestRevocation,
    ) -> Result<GuestTerminalApplicationClaim> {
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        let tx = Transaction::new_unchecked(&self.0.conn, TransactionBehavior::Immediate)?;
        self.0.owner().require_owner(&tx, &self.0.binding)?;
        let retained: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_execution_revocation
             WHERE binding_digest=?1 AND frame_digest=?2 AND frame_json=?3)",
            params![
                self.0.binding.digest()?,
                committed.frame.digest(),
                committed.frame.canonical()
            ],
            |row| row.get(0),
        )?;
        ensure!(retained, "guest cancellation lost its sticky authority");
        let existing: Option<String> = tx
            .query_row(
                "SELECT application FROM external_guest_terminal_application
                 WHERE binding_digest=?1 AND frame_digest=?2",
                params![self.0.binding.digest()?, committed.frame.digest()],
                |row| row.get(0),
            )
            .optional()?;
        let claim = match existing.as_deref() {
            Some("claimed") => GuestTerminalApplicationClaim::AlreadyClaimed,
            Some("applied") => GuestTerminalApplicationClaim::AlreadyApplied,
            Some(_) => anyhow::bail!("guest terminal application has an invalid state"),
            None => {
                tx.execute(
                    "INSERT INTO external_guest_terminal_application VALUES(?1,?2,'claimed')",
                    params![self.0.binding.digest()?, committed.frame.digest()],
                )?;
                let frame = SignedExecutionFrame::decode_and_verify(
                    &committed.wire,
                    &self.0.binding,
                    lillux::time::timestamp_millis(),
                )?;
                GuestTerminalApplicationClaim::New(GuestTerminalApplicationToken(frame))
            }
        };
        tx.commit()?;
        Ok(claim)
    }

    pub fn apply_terminal_revocation<T>(
        &self,
        token: GuestTerminalApplicationToken,
        stop: impl FnOnce(&AuthenticatedExecutionFrame) -> Result<T>,
    ) -> Result<(T, PerformedGuestTerminalApplication)> {
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        let tx = Transaction::new_unchecked(&self.0.conn, TransactionBehavior::Immediate)?;
        self.0.owner().require_owner(&tx, &self.0.binding)?;
        let state: String = tx.query_row(
            "SELECT application FROM external_guest_terminal_application
             WHERE binding_digest=?1 AND frame_digest=?2",
            params![self.0.binding.digest()?, token.0.digest()],
            |row| row.get(0),
        )?;
        ensure!(
            state == "claimed",
            "guest terminal stop has no exact durable claim"
        );
        let value = stop(&token.0)?;
        tx.commit()?;
        Ok((value, PerformedGuestTerminalApplication(token.0)))
    }

    pub fn finish_terminal_revocation(
        &self,
        performed: PerformedGuestTerminalApplication,
    ) -> Result<GuestApplicationAck> {
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        let frame = performed.0;
        let tx = Transaction::new_unchecked(&self.0.conn, TransactionBehavior::Immediate)?;
        self.0.owner().require_owner(&tx, &self.0.binding)?;
        let changed = tx.execute(
            "UPDATE external_guest_terminal_application SET application='applied'
             WHERE binding_digest=?1 AND frame_digest=?2 AND application='claimed'",
            params![self.0.binding.digest()?, frame.digest()],
        )?;
        ensure!(changed == 1, "guest terminal stop is not freshly performed");
        tx.commit()?;
        Ok(GuestApplicationAck {
            direction: frame.frame().direction,
            sequence: frame.frame().sequence,
            frame_digest: frame.digest().to_owned(),
        })
    }

    fn application_transaction(
        &self,
        frame: &AuthenticatedExecutionFrame,
    ) -> Result<Transaction<'_>> {
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        ensure!(
            frame.frame().binding_digest == self.0.binding.digest()?,
            "guest application gate changed channel authority"
        );
        let tx = Transaction::new_unchecked(&self.0.conn, TransactionBehavior::Immediate)?;
        self.0.owner().require_owner(&tx, &self.0.binding)?;
        let application: String = tx.query_row(
            "SELECT application FROM external_execution_frame
             WHERE binding_digest=?1 AND direction=?2 AND sequence=?3 AND frame_digest=?4",
            params![
                self.0.binding.digest()?,
                frame.frame().direction.as_str(),
                i64::try_from(frame.frame().sequence)?,
                frame.digest()
            ],
            |row| row.get(0),
        )?;
        ensure!(
            application == "claimed",
            "guest action has no exact durable claim"
        );
        let revoked = journal::revoked(&tx, &self.0.binding.digest()?)?;
        if matches!(frame.frame().payload, ExecutionChannelPayload::Cancel) {
            ensure!(
                revoked,
                "guest cancellation side effect precedes sticky revocation"
            );
        } else if frame.frame().direction == ChannelDirection::OwnerToSupervisor {
            ensure!(
                !revoked,
                "external execution was revoked before side effect"
            );
        } else if revoked {
            ensure!(
                matches!(
                    frame.frame().payload,
                    ExecutionChannelPayload::Stopped { .. }
                ),
                "revoked external execution admits only its terminal stop observation"
            );
        }
        Ok(tx)
    }

    /// Run one non-streaming bounded native action while its exact durable
    /// claim excludes revocation. Returning an error consumes the token: the
    /// caller must reconcile the claimed/unknown action rather than retry it.
    pub fn apply_once<T>(
        &self,
        token: GuestApplicationToken,
        action: impl FnOnce(&AuthenticatedExecutionFrame) -> Result<T>,
    ) -> Result<(T, PerformedGuestApplication)> {
        ensure!(
            !matches!(
                token.frame.frame().payload,
                ExecutionChannelPayload::ProtocolBytes { .. }
            ),
            "protocol bytes require bounded partial application"
        );
        let tx = self.application_transaction(&token.frame)?;
        let value = action(&token.frame)?;
        tx.commit()?;
        Ok((value, PerformedGuestApplication(token.frame)))
    }

    /// Apply one bounded prefix of protocol input. Each successful partial
    /// write releases the writer transaction and returns the same opaque token
    /// with an advanced process-local offset; cancellation is rechecked before
    /// the next prefix. Any uncertain write consumes the token.
    pub fn apply_protocol_chunk(
        &self,
        mut token: GuestApplicationToken,
        action: impl FnOnce(&AuthenticatedExecutionFrame, &[u8]) -> Result<usize>,
    ) -> Result<GuestProtocolApplication> {
        let bytes = token.frame.protocol_bytes()?;
        ensure!(
            token.protocol_offset < bytes.len(),
            "protocol application has no remaining bytes"
        );
        let tx = self.application_transaction(&token.frame)?;
        let written = action(&token.frame, &bytes[token.protocol_offset..])?;
        ensure!(
            written > 0 && written <= bytes.len() - token.protocol_offset,
            "protocol application reported an invalid bounded write"
        );
        token.protocol_offset += written;
        tx.commit()?;
        if token.protocol_offset == bytes.len() {
            Ok(GuestProtocolApplication::Performed(
                PerformedGuestApplication(token.frame),
            ))
        } else {
            Ok(GuestProtocolApplication::Pending(token))
        }
    }

    pub fn finish(&self, performed: PerformedGuestApplication) -> Result<GuestApplicationAck> {
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        let frame = performed.0;
        let tx = Transaction::new_unchecked(&self.0.conn, TransactionBehavior::Immediate)?;
        journal::finish_application(
            &tx,
            &self.0.owner(),
            &self.0.binding.placement_thread_id,
            frame.frame().direction,
            frame.frame().sequence,
            frame.digest(),
        )?;
        tx.commit()?;
        Ok(GuestApplicationAck {
            direction: frame.frame().direction,
            sequence: frame.frame().sequence,
            frame_digest: frame.digest().to_owned(),
        })
    }

    /// Install exact receiver-CAS retention while the validating guard remains
    /// held. The corresponding authenticated sealed frame must already exist.
    pub fn retain_export(
        &self,
        authority: &PinnedStateAuthority,
        retained: &ValidatedCandidateRetention<'_>,
        sealed: &AuthenticatedExecutionFrame,
        occurrence_digest: &str,
        occurrence_stage: &mut DurableCasUploadStage,
    ) -> Result<DurableExternalCandidateReceipt> {
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        hash(occurrence_digest, "external launcher occurrence digest")?;
        let sealed_frame_digest = sealed.digest();
        hash(sealed_frame_digest, "sealed export frame digest")?;
        let content = retained.content_for_store(authority)?;
        ensure!(
            self.0.state_runtime_identity_json
                == lillux::canonical_json(&serde_json::to_value(
                    authority.runtime_directory().identity()?
                )?)?,
            "guest export retention changed receiver CAS authority"
        );
        ensure!(
            content.channel_binding_digest() == self.0.binding.digest()?,
            "guest export retention changed channel"
        );
        ensure!(
            sealed.frame().direction == ChannelDirection::SupervisorToOwner
                && sealed.frame().binding_digest == self.0.binding.digest()?
                && matches!(
                    &sealed.frame().payload,
                    ExecutionChannelPayload::ExportSealed {
                        candidate_snapshot_hash,
                        completion_request_digest,
                        writer_exclusion_evidence_hash,
                    } if candidate_snapshot_hash == content.snapshot_hash()
                        && completion_request_digest == content.completion_request_digest()
                        && writer_exclusion_evidence_hash
                            == content.claimed_writer_exclusion_evidence_hash()
                ),
            "guest retention changed authenticated sealed export authority"
        );
        let publication_key = DurableCasPublicationKey::external_candidate_occurrence(
            content.channel_binding_digest(),
            occurrence_digest,
        )?;
        ensure!(
            occurrence_stage.owner_principal() == self.0.bootstrap_digest,
            "external-candidate occurrence root changed bootstrap owner"
        );
        occurrence_stage.ensure_publication_contract(&publication_key, None)?;
        // Central recovery roots publish before the guest row. A crash between
        // these commits is conservative and exactly reconcilable: the already
        // retained receipt may create only its matching missing guest row.
        let receipt = if occurrence_stage.admitted_target_hash().is_some() {
            let receipt = occurrence_stage.external_candidate_receipt()?;
            let (objects, blobs) = retained.retained_root_sets(authority)?;
            ensure!(
                receipt.owner_principal() == self.0.bootstrap_digest
                    && receipt.publication_key() == &publication_key
                    && receipt.snapshot_hash() == content.snapshot_hash()
                    && receipt.object_hashes() == &objects
                    && receipt.blob_hashes() == &blobs
                    && receipt.large_object_hashes().is_empty(),
                "existing external-candidate receipt changed retained content"
            );
            receipt
        } else {
            retained.retain_external_candidate_occurrence(authority, occurrence_stage)?
        };
        let tx = Transaction::new_unchecked(&self.0.conn, TransactionBehavior::Immediate)?;
        self.0.owner().require_owner(&tx, &self.0.binding)?;
        let matches_frame: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_execution_frame
             WHERE binding_digest=?1 AND direction='supervisor_to_owner'
             AND frame_digest=?2
             AND json_extract(frame_json,'$.frame.payload.kind')='export_sealed'
             AND json_extract(frame_json,'$.frame.payload.candidate_snapshot_hash')=?3
             AND json_extract(frame_json,'$.frame.payload.completion_request_digest')=?4
             AND json_extract(frame_json,'$.frame.payload.writer_exclusion_evidence_hash')=?5)",
            params![
                content.channel_binding_digest(),
                sealed_frame_digest,
                content.snapshot_hash(),
                content.completion_request_digest(),
                content.claimed_writer_exclusion_evidence_hash()
            ],
            |row| row.get(0),
        )?;
        ensure!(
            matches_frame,
            "guest retention has no exact sealed export frame"
        );
        let changed = tx.execute(
            "INSERT OR IGNORE INTO external_guest_export_retention
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                content.channel_binding_digest(),
                sealed_frame_digest,
                occurrence_digest,
                receipt.staging_id(),
                content.snapshot_hash(),
                content.completion_request_digest(),
                content.claimed_writer_exclusion_evidence_hash()
            ],
        )?;
        if changed == 0 {
            self.0.owner().require_export_retention(
                &tx,
                &self.0.binding,
                sealed_frame_digest,
                content.snapshot_hash(),
                content.completion_request_digest(),
                content.claimed_writer_exclusion_evidence_hash(),
            )?;
            let exact: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM external_guest_export_retention
                 WHERE binding_digest=?1 AND sealed_frame_digest=?2
                 AND occurrence_digest=?3 AND durable_receipt_id=?4
                 AND snapshot_hash=?5 AND completion_request_digest=?6
                 AND writer_exclusion_evidence_hash=?7)",
                params![
                    content.channel_binding_digest(),
                    sealed_frame_digest,
                    occurrence_digest,
                    receipt.staging_id(),
                    content.snapshot_hash(),
                    content.completion_request_digest(),
                    content.claimed_writer_exclusion_evidence_hash()
                ],
                |row| row.get(0),
            )?;
            ensure!(
                exact,
                "guest export retention conflicts with central receipt"
            );
        }
        tx.commit()?;
        Ok(receipt)
    }

    pub fn validate(&self) -> Result<()> {
        self.0.validate()
    }

    /// Join the exact native capture response to its authenticated quiesce
    /// command and already-durable occurrence roots. This does not seal or
    /// publish the candidate; it makes a launcher response restart-auditable.
    pub fn record_native_capture(
        &self,
        authority: &PinnedStateAuthority,
        quiesce: &AuthenticatedExecutionFrame,
        capture: &DurableNativeCandidateCapture,
    ) -> Result<()> {
        ensure_same_file(&self.0.directory, &self.0.database_file)?;
        for (value, label) in [
            (&capture.quiesce_frame_digest, "quiesce frame digest"),
            (&capture.occurrence_digest, "launcher occurrence digest"),
            (&capture.snapshot_hash, "candidate snapshot hash"),
            (
                &capture.completion_request_digest,
                "completion request digest",
            ),
            (
                &capture.writer_exclusion_evidence_hash,
                "writer exclusion evidence hash",
            ),
        ] {
            hash(value, label)?;
        }
        ensure!(
            quiesce.digest() == capture.quiesce_frame_digest
                && quiesce.frame().binding_digest == self.0.binding.digest()?
                && quiesce.frame().direction == ChannelDirection::OwnerToSupervisor
                && matches!(
                    &quiesce.frame().payload,
                    ExecutionChannelPayload::Quiesce { completion_request_digest }
                        if completion_request_digest == &capture.completion_request_digest
                ),
            "native capture changed its authenticated quiesce authority"
        );
        ensure!(
            self.0.state_runtime_identity_json
                == lillux::canonical_json(&serde_json::to_value(
                    authority.runtime_directory().identity()?
                )?)?,
            "native capture changed receiver CAS authority"
        );
        let guard = authority.acquire_shared_guard()?;
        let (objects, blobs) = validated_retained_candidate_root_sets(
            authority,
            &guard,
            &self.0.binding,
            &capture.snapshot_hash,
            &capture.completion_request_digest,
            &capture.writer_exclusion_evidence_hash,
        )?;
        let publication_key = DurableCasPublicationKey::external_candidate_occurrence(
            &self.0.binding.digest()?,
            &capture.occurrence_digest,
        )?;
        let stage = authority
            .require_recovery()?
            .open_durable_cas_upload_admitted(
                &guard,
                &capture.durable_stage_id,
                &self.0.bootstrap_digest,
            )?;
        stage.ensure_publication_contract(&publication_key, None)?;
        ensure!(
            stage
                .admitted_target_hash()
                .is_none_or(|target| target == capture.snapshot_hash)
                && stage.protected_object_hashes() == &objects
                && stage.protected_blob_hashes() == &blobs
                && stage.protected_large_object_hashes().is_empty(),
            "native capture changed its exact durable occurrence roots"
        );
        let tx = Transaction::new_unchecked(&self.0.conn, TransactionBehavior::Immediate)?;
        self.0.owner().require_owner(&tx, &self.0.binding)?;
        let application: String = tx.query_row(
            "SELECT application FROM external_execution_frame
             WHERE binding_digest=?1 AND direction='owner_to_supervisor'
             AND frame_digest=?2
             AND json_extract(frame_json,'$.frame.payload.kind')='quiesce'",
            params![self.0.binding.digest()?, capture.quiesce_frame_digest],
            |row| row.get(0),
        )?;
        ensure!(
            matches!(application.as_str(), "claimed" | "applied"),
            "native capture has no exact claimed quiesce application"
        );
        let changed = tx.execute(
            "INSERT OR IGNORE INTO external_guest_native_capture VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                self.0.binding.digest()?,
                capture.quiesce_frame_digest,
                capture.occurrence_digest,
                capture.durable_stage_id,
                capture.snapshot_hash,
                capture.completion_request_digest,
                capture.writer_exclusion_evidence_hash
            ],
        )?;
        if changed == 0 {
            let exact: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM external_guest_native_capture
                 WHERE binding_digest=?1 AND quiesce_frame_digest=?2
                 AND occurrence_digest=?3 AND durable_stage_id=?4
                 AND snapshot_hash=?5 AND completion_request_digest=?6
                 AND writer_exclusion_evidence_hash=?7)",
                params![
                    self.0.binding.digest()?,
                    capture.quiesce_frame_digest,
                    capture.occurrence_digest,
                    capture.durable_stage_id,
                    capture.snapshot_hash,
                    capture.completion_request_digest,
                    capture.writer_exclusion_evidence_hash
                ],
                |row| row.get(0),
            )?;
            ensure!(exact, "native capture conflicts with its durable record");
        }
        tx.commit()?;
        Ok(())
    }
}

impl GuestStore {
    fn open_existing(
        directory: lillux::PinnedDirectory,
        authority: &PinnedStateAuthority,
        store_identity: &GuestJournalStoreIdentity,
        bootstrap_digest: &str,
        binding: &ExecutionChannelBinding,
    ) -> Result<Self> {
        hash(bootstrap_digest, "external bootstrap digest")?;
        binding.validate()?;
        directory.require_owner_private_directory()?;
        ensure!(
            store_identity.schema == 1
                && store_identity.bootstrap_digest == bootstrap_digest
                && store_identity.binding_digest == binding.digest()?
                && store_identity.directory_identity == directory.identity()?,
            "external guest store changed its outer recovery identity"
        );
        let lifetime_lock = directory
            .try_lock_exclusive()?
            .context("external guest directory still has a live owner")?;
        lifetime_lock.ensure_protects(&directory)?;
        for name in directory.entry_names()? {
            ensure!(
                name == OsStr::new(DATABASE_NAME) || name == journal_name(),
                "external guest store has an ambient entry: {}",
                directory.path().join(name).display()
            );
        }
        for name in [wal_name(), shm_name()] {
            ensure!(
                directory.open_entry(&name, false)?.is_none(),
                "external guest rollback journal store has an unexpected sidecar"
            );
        }
        // A crash may leave an exact rollback journal. Pinning it before SQLite
        // opens prevents a substituted special entry; SQLite may consume it.
        let _rollback = directory.open_regular(&journal_name(), false)?;
        let database_file = directory
            .open_regular(OsStr::new(DATABASE_NAME), true)?
            .context("external guest database is absent")?;
        ensure!(
            lillux::matches_pinned_regular_file_identity(
                &database_file,
                store_identity.database_identity
            )?,
            "external guest database changed its outer inode identity"
        );
        let conn = open_exact(&directory, &database_file)?;
        configure(&conn)?;
        let state_runtime_identity_json = lillux::canonical_json(&serde_json::to_value(
            authority.runtime_directory().identity()?,
        )?)?;
        ensure!(
            store_identity.state_runtime_identity_json == state_runtime_identity_json,
            "external guest store changed receiver runtime authority"
        );
        let store = Self {
            conn,
            directory,
            _lifetime_lock: lifetime_lock,
            database_file,
            bootstrap_digest: bootstrap_digest.to_owned(),
            state_runtime_identity_json,
            binding: binding.clone(),
            store_identity: store_identity.clone(),
        };
        store.validate()?;
        Ok(store)
    }

    fn owner(&self) -> GuestOwner<'_> {
        GuestOwner {
            bootstrap_digest: &self.bootstrap_digest,
        }
    }

    fn validate(&self) -> Result<()> {
        ensure_same_file(&self.directory, &self.database_file)?;
        let app_id: i32 = self
            .conn
            .query_row("PRAGMA application_id", [], |row| row.get(0))?;
        let epoch: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        ensure!(
            app_id == APPLICATION_ID && epoch == SCHEMA_EPOCH,
            "external guest database has predecessor or foreign identity"
        );
        crate::sqlite_schema::assert_complete_schema_sql(
            &self.conn,
            &complete_schema(),
            &self.directory.path().join(DATABASE_NAME),
        )?;
        self.owner().require_owner(&self.conn, &self.binding)?;
        let (retained_nonce, retained_directory, retained_database, retained_runtime): (
            String,
            String,
            String,
            String,
        ) = self.conn.query_row(
            "SELECT journal_nonce,directory_identity_json,database_identity_json,
                    state_runtime_identity_json
             FROM external_guest_meta WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        ensure!(
            retained_directory
                == lillux::canonical_json(&serde_json::to_value(self.directory.identity()?)?)?,
            "external guest directory identity changed"
        );
        ensure!(
            retained_nonce == self.store_identity.journal_nonce
                && retained_database
                    == lillux::canonical_json(&serde_json::to_value(
                        self.store_identity.database_identity
                    )?)?,
            "external guest database lost its outer recovery anchor"
        );
        ensure!(
            retained_runtime == self.state_runtime_identity_json,
            "external guest receiver CAS identity changed"
        );
        let lifecycle: String = self.conn.query_row(
            "SELECT lifecycle FROM external_guest_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        let occurrence_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM external_guest_launcher_occurrence",
            [],
            |row| row.get(0),
        )?;
        let ready_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM external_guest_launcher_ready",
            [],
            |row| row.get(0),
        )?;
        ensure!(
            matches!(
                (lifecycle.as_str(), occurrence_count, ready_count),
                ("launch_intent", 0, 0) | ("launcher_bound", 1, 0) | ("ready", 1, 1)
            ),
            "external guest launcher lifecycle lost exact occurrence evidence"
        );
        journal::validate_channels(&self.conn, &self.owner())?;
        Ok(())
    }

    fn validate_retained_content(&self, authority: &PinnedStateAuthority) -> Result<()> {
        ensure!(
            self.state_runtime_identity_json
                == lillux::canonical_json(&serde_json::to_value(
                    authority.runtime_directory().identity()?
                )?)?,
            "external guest recovery changed receiver CAS authority"
        );
        let guard = authority.acquire_shared_guard()?;
        let mut captures = self.conn.prepare(
            "SELECT quiesce_frame_digest,occurrence_digest,durable_stage_id,
                    snapshot_hash,completion_request_digest,writer_exclusion_evidence_hash
             FROM external_guest_native_capture ORDER BY binding_digest",
        )?;
        let captures = captures
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (quiesce, occurrence, stage_id, snapshot, completion, evidence) in captures {
            let application: Option<String> = self
                .conn
                .query_row(
                    "SELECT application FROM external_execution_frame
                     WHERE binding_digest=?1 AND direction='owner_to_supervisor'
                     AND frame_digest=?2
                     AND json_extract(frame_json,'$.frame.payload.kind')='quiesce'
                     AND json_extract(frame_json,'$.frame.payload.completion_request_digest')=?3",
                    params![self.binding.digest()?, quiesce, completion],
                    |row| row.get(0),
                )
                .optional()?;
            ensure!(
                matches!(application.as_deref(), Some("claimed" | "applied")),
                "guest native capture lost its authenticated quiesce application"
            );
            let (objects, blobs) = validated_retained_candidate_root_sets(
                authority,
                &guard,
                &self.binding,
                &snapshot,
                &completion,
                &evidence,
            )?;
            let publication_key = DurableCasPublicationKey::external_candidate_occurrence(
                &self.binding.digest()?,
                &occurrence,
            )?;
            let stage = authority
                .require_recovery()?
                .open_durable_cas_upload_admitted(&guard, &stage_id, &self.bootstrap_digest)?;
            stage.ensure_publication_contract(&publication_key, None)?;
            ensure!(
                stage
                    .admitted_target_hash()
                    .is_none_or(|target| target == snapshot)
                    && stage.protected_object_hashes() == &objects
                    && stage.protected_blob_hashes() == &blobs
                    && stage.protected_large_object_hashes().is_empty(),
                "guest native capture changed its exact durable occurrence roots"
            );
        }
        let mut rows = self.conn.prepare(
            "SELECT sealed_frame_digest,occurrence_digest,durable_receipt_id,
                    snapshot_hash,completion_request_digest,writer_exclusion_evidence_hash
             FROM external_guest_export_retention ORDER BY binding_digest",
        )?;
        let retained = rows
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (sealed, occurrence, receipt_id, snapshot, completion, evidence) in retained {
            let (objects, blobs) = validated_retained_candidate_root_sets(
                authority,
                &guard,
                &self.binding,
                &snapshot,
                &completion,
                &evidence,
            )?;
            let matches_frame: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='supervisor_to_owner'
                 AND frame_digest=?2
                 AND json_extract(frame_json,'$.frame.payload.kind')='export_sealed'
                 AND json_extract(frame_json,'$.frame.payload.candidate_snapshot_hash')=?3
                 AND json_extract(frame_json,'$.frame.payload.completion_request_digest')=?4
                 AND json_extract(frame_json,'$.frame.payload.writer_exclusion_evidence_hash')=?5)",
                params![
                    self.binding.digest()?,
                    sealed,
                    snapshot,
                    completion,
                    evidence
                ],
                |row| row.get(0),
            )?;
            ensure!(
                matches_frame,
                "guest retention lost authenticated sealed frame"
            );
            let publication_key = DurableCasPublicationKey::external_candidate_occurrence(
                &self.binding.digest()?,
                &occurrence,
            )?;
            let stage = authority
                .require_recovery()?
                .open_durable_cas_upload_admitted(&guard, &receipt_id, &self.bootstrap_digest)?;
            stage.ensure_publication_contract(&publication_key, None)?;
            let receipt = stage.external_candidate_receipt()?;
            ensure!(
                receipt.staging_id() == receipt_id
                    && receipt.snapshot_hash() == snapshot
                    && receipt.object_hashes() == &objects
                    && receipt.blob_hashes() == &blobs
                    && receipt.large_object_hashes().is_empty(),
                "guest retention changed its exact central recovery roots"
            );
        }
        Ok(())
    }

    fn commit_terminal_revocation(&self, wire: &[u8]) -> Result<CommittedGuestRevocation> {
        ensure_same_file(&self.directory, &self.database_file)?;
        let frame = SignedExecutionFrame::decode_and_verify(
            wire,
            &self.binding,
            lillux::time::timestamp_millis(),
        )?;
        ensure!(
            frame.frame().direction == ChannelDirection::OwnerToSupervisor
                && matches!(frame.frame().payload, ExecutionChannelPayload::Cancel),
            "external terminal revocation is not an owner cancellation"
        );
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let newly_recorded = journal::record_revocation(
            &tx,
            &self.owner(),
            &self.binding.placement_thread_id,
            wire,
        )?;
        tx.commit()?;
        Ok(CommittedGuestRevocation {
            frame,
            wire: wire.to_vec(),
            newly_recorded,
        })
    }

    fn try_append_terminal_observation(
        &self,
        committed: &CommittedGuestRevocation,
    ) -> RevocationAppendOutcome {
        if let Err(error) = ensure_same_file(&self.directory, &self.database_file) {
            return RevocationAppendOutcome::Blocked {
                revocation_committed: true,
                error,
            };
        }
        let tx = match Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate) {
            Ok(tx) => tx,
            Err(error) => {
                return RevocationAppendOutcome::Blocked {
                    revocation_committed: true,
                    error: error.into(),
                };
            }
        };
        let result = journal::append_frame(
            &tx,
            &self.owner(),
            &self.binding.placement_thread_id,
            &committed.wire,
        )
        .and_then(|appended| {
            tx.commit()?;
            Ok(appended)
        });
        match result {
            Ok(true) => RevocationAppendOutcome::Appended,
            Ok(false) => RevocationAppendOutcome::AlreadyPresent,
            Err(error) => RevocationAppendOutcome::Blocked {
                revocation_committed: true,
                error,
            },
        }
    }
}

struct GuestOwner<'a> {
    bootstrap_digest: &'a str,
}

impl JournalOwner for GuestOwner<'_> {
    fn require_owner(&self, conn: &Connection, binding: &ExecutionChannelBinding) -> Result<()> {
        let retained: Option<(String, String)> = conn
            .query_row(
                "SELECT bootstrap_digest,binding_digest FROM external_guest_meta WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        ensure!(
            retained == Some((self.bootstrap_digest.to_owned(), binding.digest()?)),
            "external guest journal lost exact bootstrap owner"
        );
        Ok(())
    }

    fn authorize_frame(
        &self,
        conn: &Connection,
        _binding: &ExecutionChannelBinding,
        payload: &ExecutionChannelPayload,
    ) -> Result<()> {
        let lifecycle: String = conn.query_row(
            "SELECT lifecycle FROM external_guest_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        ensure!(
            lifecycle == "ready" || matches!(payload, ExecutionChannelPayload::Cancel),
            "external guest launcher is not ready"
        );
        Ok(())
    }

    fn require_export_retention(
        &self,
        conn: &Connection,
        binding: &ExecutionChannelBinding,
        frame_digest: &str,
        candidate_snapshot_hash: &str,
        completion_request_digest: &str,
        writer_exclusion_evidence_hash: &str,
    ) -> Result<()> {
        let retained: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_guest_export_retention
             WHERE binding_digest=?1 AND sealed_frame_digest=?2 AND snapshot_hash=?3
             AND completion_request_digest=?4 AND writer_exclusion_evidence_hash=?5)",
            params![
                binding.digest()?,
                frame_digest,
                candidate_snapshot_hash,
                completion_request_digest,
                writer_exclusion_evidence_hash
            ],
            |row| row.get(0),
        )?;
        ensure!(
            retained,
            "external guest sealed export is not durably retained"
        );
        Ok(())
    }

    fn out_of_band_application_state(
        &self,
        conn: &Connection,
        binding: &ExecutionChannelBinding,
        direction: ChannelDirection,
        sequence: u64,
        digest: &str,
    ) -> Result<Option<crate::external_execution::ExecutionFrameApplication>> {
        let retained: Option<(String, String)> = conn
            .query_row(
                "SELECT r.frame_json,COALESCE(t.application,'retained')
                 FROM external_execution_revocation r
                 LEFT JOIN external_guest_terminal_application t
                   ON t.binding_digest=r.binding_digest
                  AND t.frame_digest=r.frame_digest
                 WHERE r.binding_digest=?1 AND r.frame_digest=?2",
                params![binding.digest()?, digest],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((wire, application)) = retained else {
            return Ok(None);
        };
        let frame = SignedExecutionFrame::decode_and_verify(
            wire.as_bytes(),
            binding,
            binding.issued_at_ms,
        )?;
        ensure!(
            frame.frame().direction == direction
                && frame.frame().sequence == sequence
                && matches!(frame.frame().payload, ExecutionChannelPayload::Cancel),
            "guest terminal application changed sticky revocation"
        );
        Ok(Some(match application.as_str() {
            "retained" => crate::external_execution::ExecutionFrameApplication::Retained,
            "claimed" => crate::external_execution::ExecutionFrameApplication::Claimed,
            "applied" => crate::external_execution::ExecutionFrameApplication::Applied,
            _ => anyhow::bail!("guest terminal application has an invalid state"),
        }))
    }

    fn can_prove_pending_input_revoked(&self) -> bool {
        true
    }
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    let mode: String = conn.query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))?;
    ensure!(
        mode == "delete",
        "external guest journal mode is not rollback-delete"
    );
    Ok(())
}

fn open_exact(directory: &lillux::PinnedDirectory, expected: &File) -> Result<Connection> {
    let path = directory.descriptor_child_path(OsStr::new(DATABASE_NAME))?;
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open protected external guest database {}", path.display()))?;
    ensure_same_file(directory, expected)?;
    Ok(conn)
}

fn ensure_same_file(directory: &lillux::PinnedDirectory, expected: &File) -> Result<()> {
    let current = directory
        .open_regular(OsStr::new(DATABASE_NAME), false)?
        .context("external guest database disappeared")?;
    ensure!(
        lillux::same_open_file_identity(expected, &current)?,
        "external guest database identity changed"
    );
    Ok(())
}

fn wal_name() -> OsString {
    OsString::from(format!("{DATABASE_NAME}-wal"))
}

fn shm_name() -> OsString {
    OsString::from(format!("{DATABASE_NAME}-shm"))
}

fn journal_name() -> OsString {
    OsString::from(format!("{DATABASE_NAME}-journal"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_execution::{
        ExecutionFrame, NativeNamespaceExit, NativeWriterExclusionMechanism,
        NativeWriterExclusionObservation, SignedExecutionFrame,
    };
    use crate::objects::{ProjectFile, ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use lillux::crypto::SigningKey;

    fn binding() -> (ExecutionChannelBinding, SigningKey, SigningKey) {
        let owner = lillux::crypto::generate_signing_key();
        let supervisor = lillux::crypto::generate_signing_key();
        let now = lillux::time::timestamp_millis();
        (
            ExecutionChannelBinding {
                schema: 1,
                placement_thread_id: "T-guest".into(),
                allocation_request_digest: "a".repeat(64),
                occurrence_id: "occurrence-guest".into(),
                admitted_capsule_hash: "b".repeat(64),
                base_snapshot_hash: "c".repeat(64),
                execution_binding_hash: "d".repeat(64),
                supervisor_runtime_hash: "e".repeat(64),
                channel_nonce: "f".repeat(64),
                owner_public_key: STANDARD.encode(owner.verifying_key().as_bytes()),
                supervisor_public_key: STANDARD.encode(supervisor.verifying_key().as_bytes()),
                issued_at_ms: now - 1_000,
                execution_deadline_ms: now + 60_000,
                expires_at_ms: now + 120_000,
                max_frames: 100,
                max_bytes: 1024 * 1024,
            },
            owner,
            supervisor,
        )
    }

    fn wire(
        binding: &ExecutionChannelBinding,
        key: &SigningKey,
        direction: ChannelDirection,
        sequence: u64,
        previous_frame_digest: Option<String>,
        acknowledged_peer_sequence: u64,
        payload: ExecutionChannelPayload,
    ) -> (Vec<u8>, String) {
        let signed = SignedExecutionFrame::sign(
            ExecutionFrame {
                schema: 1,
                binding_digest: binding.digest().unwrap(),
                direction,
                sequence,
                previous_frame_digest,
                acknowledged_peer_sequence,
                payload,
            },
            binding,
            key,
        )
        .unwrap();
        let bytes = lillux::canonical_json(&serde_json::to_value(signed).unwrap())
            .unwrap()
            .into_bytes();
        let digest = lillux::sha256_hex(&bytes);
        (bytes, digest)
    }

    fn live_store(
        root: &tempfile::TempDir,
        authority: &PinnedStateAuthority,
    ) -> (
        LiveGuestJournal,
        ExecutionChannelBinding,
        SigningKey,
        SigningKey,
        String,
        GuestJournalStoreIdentity,
    ) {
        let directory = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        directory.tighten_owner_private_directory().unwrap();
        let (binding, owner, supervisor) = binding();
        let bootstrap = "9".repeat(64);
        let prepared =
            PreparedGuestJournal::create(directory, authority, &bootstrap, binding.clone())
                .unwrap();
        let store_identity = prepared.store_identity().clone();
        let occurrence = LauncherOccurrenceEvidence::from_held_launcher(
            lillux::ExactProcessIdentity {
                boot_id: "test-boot".into(),
                target_pid: 100,
                target_start_time_ticks: 200,
                group_leader_pid: 100,
                group_leader_start_time_ticks: 200,
            },
            &"7".repeat(64),
            &"6".repeat(64),
        )
        .unwrap();
        let ready = AuthenticatedLauncherReady::from_handshake_transcript(&"5".repeat(64)).unwrap();
        let live = prepared
            .bind_launcher(occurrence)
            .unwrap()
            .mark_launcher_ready(ready)
            .unwrap();
        let (ready, ready_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            1,
            None,
            0,
            ExecutionChannelPayload::Ready {
                supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                base_snapshot_hash: binding.base_snapshot_hash.clone(),
            },
        );
        assert!(live.record_frame(&ready).unwrap());
        let GuestApplicationClaim::New(ready_token) = live
            .claim(ChannelDirection::SupervisorToOwner, 1, &ready_digest)
            .unwrap()
        else {
            panic!("ready must retain its exact applied state")
        };
        let (_, ready_performed) = live.apply_once(ready_token, |_| Ok(())).unwrap();
        live.finish(ready_performed).unwrap();
        (live, binding, owner, supervisor, bootstrap, store_identity)
    }

    fn state_authority() -> (tempfile::TempDir, PinnedStateAuthority) {
        let root = tempfile::tempdir().unwrap();
        let db = crate::StateDb::open(root.path(), std::sync::Arc::new(crate::TrustStore::new()))
            .unwrap();
        let authority = db.pinned_authority().unwrap();
        drop(db);
        (root, authority)
    }

    #[test]
    fn fresh_intent_is_single_owner_and_reopen_is_recovery_only() {
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, _owner, _supervisor, bootstrap, store_identity) =
            live_store(&root, &authority);
        let competing = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        assert!(
            RecoveredGuestJournal::open(
                competing,
                &authority,
                &store_identity,
                &bootstrap,
                &binding
            )
            .is_err()
        );
        live.validate().unwrap();
        drop(live);
        let recovered = RecoveredGuestJournal::open(
            lillux::PinnedDirectory::open(root.path()).unwrap().unwrap(),
            &authority,
            &store_identity,
            &bootstrap,
            &binding,
        )
        .unwrap();
        recovered.validate().unwrap();
        assert_eq!(
            recovered.binding().digest().unwrap(),
            binding.digest().unwrap()
        );
        drop(recovered);
        let (_other_state_root, other_authority) = state_authority();
        assert!(
            RecoveredGuestJournal::open(
                lillux::PinnedDirectory::open(root.path()).unwrap().unwrap(),
                &other_authority,
                &store_identity,
                &bootstrap,
                &binding,
            )
            .is_err()
        );
        assert!(
            RecoveredGuestJournal::open(
                lillux::PinnedDirectory::open(root.path()).unwrap().unwrap(),
                &authority,
                &store_identity,
                &"8".repeat(64),
                &binding,
            )
            .is_err()
        );
    }

    #[test]
    fn sticky_revocation_wins_before_the_native_side_effect_gate() {
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, owner, _supervisor, _bootstrap, _store_identity) =
            live_store(&root, &authority);
        let (release, release_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        assert!(live.record_frame(&release).unwrap());
        let GuestApplicationClaim::New(release_token) = live
            .claim(ChannelDirection::OwnerToSupervisor, 1, &release_digest)
            .unwrap()
        else {
            panic!("release must be a new exact claim")
        };
        let (cancel, cancel_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            2,
            Some(release_digest),
            1,
            ExecutionChannelPayload::Cancel,
        );
        assert!(live.record_frame(&cancel).unwrap());
        assert!(live.apply_once(release_token, |_| Ok(())).is_err());
        let GuestApplicationClaim::New(cancel_token) = live
            .claim(ChannelDirection::OwnerToSupervisor, 2, &cancel_digest)
            .unwrap()
        else {
            panic!("cancel must be a new exact claim")
        };
        let (_, cancel_performed) = live.apply_once(cancel_token, |_| Ok(())).unwrap();
        assert_eq!(
            live.finish(cancel_performed).unwrap().frame_digest(),
            cancel_digest
        );
        live.validate().unwrap();
    }

    #[test]
    fn writer_gate_linearizes_a_second_connection_revocation() {
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, owner, _supervisor, _bootstrap, _store_identity) =
            live_store(&root, &authority);
        let (release, release_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        live.record_frame(&release).unwrap();
        let GuestApplicationClaim::New(release_token) = live
            .claim(ChannelDirection::OwnerToSupervisor, 1, &release_digest)
            .unwrap()
        else {
            panic!("release must be newly claimed")
        };
        let (cancel, cancel_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            2,
            Some(release_digest.clone()),
            1,
            ExecutionChannelPayload::Cancel,
        );
        let (_, release_performed) = live
            .apply_once(release_token, |_| {
                let conn = Connection::open(root.path().join(DATABASE_NAME))?;
                conn.busy_timeout(std::time::Duration::ZERO)?;
                let error =
                    Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap_err();
                assert!(
                    matches!(
                        error.sqlite_error_code(),
                        Some(rusqlite::ErrorCode::DatabaseBusy)
                    ),
                    "revocation must deterministically observe the native-action writer gate"
                );
                Ok(())
            })
            .unwrap();
        live.finish(release_performed).unwrap();
        assert!(live.record_frame(&cancel).unwrap());
        assert!(matches!(
            live.claim(ChannelDirection::OwnerToSupervisor, 2, &cancel_digest)
                .unwrap(),
            GuestApplicationClaim::New(_)
        ));
        live.validate().unwrap();
    }

    #[test]
    fn supervisor_authoring_uses_retained_frontiers_and_exact_role_key() {
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, owner, supervisor, _bootstrap, _store_identity) =
            live_store(&root, &authority);
        let (release, release_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        live.record_frame(&release).unwrap();
        let GuestApplicationClaim::New(token) = live
            .claim(ChannelDirection::OwnerToSupervisor, 1, &release_digest)
            .unwrap()
        else {
            panic!("release must be newly claimed")
        };
        let (_, performed) = live.apply_once(token, |_| Ok(())).unwrap();
        live.finish(performed).unwrap();

        assert!(
            live.author_supervisor_frame(
                &owner,
                ExecutionChannelPayload::Acknowledge {
                    peer_frame_sequence: 1,
                    peer_frame_digest: release_digest.clone(),
                    application: crate::external_execution::ExecutionFrameApplication::Applied,
                },
            )
            .is_err()
        );
        let observation = live
            .ensure_supervisor_acknowledgement(&supervisor, 1, &release_digest)
            .unwrap()
            .expect("applied owner input requires one exact acknowledgement");
        assert_eq!(observation.frame().sequence, 2);
        assert_eq!(observation.frame().acknowledged_peer_sequence, 1);
        assert!(observation.frame().previous_frame_digest.is_some());
        assert_eq!(
            live.ensure_supervisor_acknowledgement(&supervisor, 1, &release_digest)
                .unwrap()
                .unwrap()
                .digest(),
            observation.digest(),
            "recovery must return the retained signed acknowledgement"
        );
        let pending = live
            .pending_supervisor_transport_frames(4, 64 * 1024)
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].digest(), observation.digest());
        assert_eq!(pending[0].wire(), observation.canonical().as_bytes());

        let (owner_ack, owner_ack_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            2,
            Some(release_digest),
            observation.frame().sequence,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: observation.frame().sequence,
                peer_frame_digest: observation.digest().to_owned(),
                application: crate::external_execution::ExecutionFrameApplication::Retained,
            },
        );
        assert!(live.record_owner_acknowledgement(&owner_ack).unwrap());
        assert!(
            live.pending_supervisor_transport_frames(4, 64 * 1024)
                .unwrap()
                .is_empty(),
            "signed peer evidence must advance the transport frontier"
        );

        let ack_of_ack = live
            .ensure_supervisor_acknowledgement(&supervisor, 2, &owner_ack_digest)
            .unwrap()
            .expect("the supervisor may acknowledge an owner acknowledgement once");
        assert_eq!(ack_of_ack.frame().sequence, 3);
        assert_eq!(ack_of_ack.frame().acknowledged_peer_sequence, 2);
        assert!(matches!(
            &ack_of_ack.frame().payload,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: 2,
                peer_frame_digest,
                application:
                    crate::external_execution::ExecutionFrameApplication::Applied,
            } if peer_frame_digest == &owner_ack_digest
        ));
        live.validate().unwrap();
    }

    #[test]
    fn native_capture_is_rooted_and_reopen_validates_exact_quiesce_coordinates() {
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let cas = authority.cas_store().unwrap();
        let policy = ProjectSnapshotPolicy::new(
            crate::project_sync::ProjectSyncScope::FullProject,
            vec![],
            vec![],
            Default::default(),
        )
        .unwrap();
        let policy_hash = cas.store_object(&policy.to_value()).unwrap();
        let bytes = b"external-candidate-base";
        let file = ProjectFile {
            blob_hash: cas.store_blob(bytes).unwrap(),
            size: bytes.len() as u64,
            normalized_mode: ProjectFile::REGULAR_MODE,
        };
        let file_hash = cas.store_object(&file.to_value()).unwrap();
        let tree = ProjectTree {
            files: [("main.py".to_owned(), file_hash.clone())]
                .into_iter()
                .collect(),
        };
        let tree_hash = cas.store_object(&tree.to_value()).unwrap();
        let base = ProjectSnapshot {
            project_tree_hash: tree_hash.clone(),
            effective_policy_hash: policy_hash.clone(),
            parent_hashes: vec![],
            created_at: "2026-09-20T00:00:00Z".into(),
            message: None,
            source: "external-native-capture-test".into(),
        };
        let base_hash = cas.store_object(&base.to_value()).unwrap();
        let (mut binding, owner, supervisor) = binding();
        binding.base_snapshot_hash = base_hash.clone();
        let bootstrap = "9".repeat(64);
        let directory = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        directory.tighten_owner_private_directory().unwrap();
        let prepared =
            PreparedGuestJournal::create(directory, &authority, &bootstrap, binding.clone())
                .unwrap();
        let store_identity = prepared.store_identity().clone();
        let occurrence = LauncherOccurrenceEvidence::from_held_launcher(
            lillux::ExactProcessIdentity {
                boot_id: "native-capture-boot".into(),
                target_pid: 200,
                target_start_time_ticks: 300,
                group_leader_pid: 200,
                group_leader_start_time_ticks: 300,
            },
            &"7".repeat(64),
            &"6".repeat(64),
        )
        .unwrap();
        let occurrence_digest = occurrence.digest().unwrap();
        let live = prepared
            .bind_launcher(occurrence)
            .unwrap()
            .mark_launcher_ready(
                AuthenticatedLauncherReady::from_handshake_transcript(&"5".repeat(64)).unwrap(),
            )
            .unwrap();
        let ready = live
            .author_supervisor_frame(
                &supervisor,
                ExecutionChannelPayload::Ready {
                    supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                    base_snapshot_hash: binding.base_snapshot_hash.clone(),
                },
            )
            .unwrap();
        let GuestApplicationClaim::New(token) = live
            .claim(
                ChannelDirection::SupervisorToOwner,
                ready.frame().sequence,
                ready.digest(),
            )
            .unwrap()
        else {
            panic!("ready observation must be freshly claimable")
        };
        let (_, performed) = live.apply_once(token, |_| Ok(())).unwrap();
        live.finish(performed).unwrap();
        let (release, release_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        let GuestApplicationClaim::New(token) = live.record_and_claim(&release).unwrap() else {
            panic!("release must be freshly claimable")
        };
        let (_, performed) = live.apply_once(token, |_| Ok(())).unwrap();
        live.finish(performed).unwrap();
        let completion = "8".repeat(64);
        let (quiesce_wire, quiesce_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            2,
            Some(release_digest),
            1,
            ExecutionChannelPayload::Quiesce {
                completion_request_digest: completion.clone(),
            },
        );
        let quiesce = SignedExecutionFrame::decode_and_verify(
            &quiesce_wire,
            &binding,
            lillux::time::timestamp_millis(),
        )
        .unwrap();
        let GuestApplicationClaim::New(quiesce_token) =
            live.record_and_claim(&quiesce_wire).unwrap()
        else {
            panic!("quiesce must be freshly claimable")
        };

        let candidate = ProjectSnapshot {
            project_tree_hash: tree_hash,
            effective_policy_hash: policy_hash,
            parent_hashes: vec![base_hash],
            created_at: "2026-09-20T00:00:01Z".into(),
            message: None,
            source: "external_candidate_terminal_capture".into(),
        };
        let snapshot_hash = cas.store_object(&candidate.to_value()).unwrap();
        let observation = NativeWriterExclusionObservation {
            schema: 1,
            binding_digest: binding.digest().unwrap(),
            base_snapshot_hash: binding.base_snapshot_hash.clone(),
            completion_request_digest: completion.clone(),
            mechanism: NativeWriterExclusionMechanism::NamespaceInitReaped,
            exit: NativeNamespaceExit::Code(0),
        };
        let evidence_hash = cas
            .store_blob(
                lillux::canonical_json(&serde_json::to_value(observation).unwrap())
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let (objects, blobs) = validated_retained_candidate_root_sets(
            &authority,
            &guard,
            &binding,
            &snapshot_hash,
            &completion,
            &evidence_hash,
        )
        .unwrap();
        let publication_key = DurableCasPublicationKey::external_candidate_occurrence(
            &binding.digest().unwrap(),
            &occurrence_digest,
        )
        .unwrap();
        let mut stage = authority
            .require_recovery()
            .unwrap()
            .begin_durable_cas_upload_admitted(
                &guard,
                &bootstrap,
                "external-candidate-native-capture",
                &publication_key,
                None,
            )
            .unwrap();
        stage
            .protect_cas_closure(
                &guard,
                objects.iter().map(String::as_str),
                blobs.iter().map(String::as_str),
            )
            .unwrap();
        let stage_id = stage.staging_id().to_owned();
        drop(stage);
        drop(guard);
        let capture = DurableNativeCandidateCapture {
            quiesce_frame_digest: quiesce_digest,
            occurrence_digest,
            durable_stage_id: stage_id,
            snapshot_hash,
            completion_request_digest: completion,
            writer_exclusion_evidence_hash: evidence_hash,
        };
        let (_, performed) = live.apply_once(quiesce_token, |_| Ok(())).unwrap();
        live.record_native_capture(&authority, &quiesce, &capture)
            .unwrap();
        drop(performed);
        assert_eq!(
            live.reconcile_native_capture_application(&capture)
                .unwrap()
                .frame_digest(),
            capture.quiesce_frame_digest
        );
        drop(live);
        RecoveredGuestJournal::open(
            lillux::PinnedDirectory::open(root.path()).unwrap().unwrap(),
            &authority,
            &store_identity,
            &bootstrap,
            &binding,
        )
        .unwrap()
        .validate()
        .unwrap();
    }

    #[test]
    fn recovery_refuses_schema_drift_without_repair() {
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, _owner, _supervisor, bootstrap, store_identity) =
            live_store(&root, &authority);
        drop(live);
        let conn = Connection::open(root.path().join(DATABASE_NAME)).unwrap();
        conn.execute_batch("CREATE TABLE ambient_drift(value TEXT);")
            .unwrap();
        drop(conn);
        assert!(
            RecoveredGuestJournal::open(
                lillux::PinnedDirectory::open(root.path()).unwrap().unwrap(),
                &authority,
                &store_identity,
                &bootstrap,
                &binding,
            )
            .is_err()
        );
    }

    #[test]
    fn recovery_refuses_a_byte_copy_replacement_database_inode() {
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, _owner, _supervisor, bootstrap, store_identity) =
            live_store(&root, &authority);
        drop(live);
        let database = root.path().join(DATABASE_NAME);
        let replacement = root.path().join("replacement.sqlite3");
        std::fs::copy(&database, &replacement).unwrap();
        std::fs::rename(&replacement, &database).unwrap();
        assert!(
            RecoveredGuestJournal::open(
                lillux::PinnedDirectory::open(root.path()).unwrap().unwrap(),
                &authority,
                &store_identity,
                &bootstrap,
                &binding,
            )
            .err()
            .expect("replacement database must be refused")
            .to_string()
            .contains("outer inode identity")
        );
    }

    #[test]
    fn live_database_replacement_between_claim_and_effect_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, owner, _supervisor, _bootstrap, _store_identity) =
            live_store(&root, &authority);
        let (release, release_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        live.record_frame(&release).unwrap();
        let GuestApplicationClaim::New(token) = live
            .claim(ChannelDirection::OwnerToSupervisor, 1, &release_digest)
            .unwrap()
        else {
            panic!("release must be newly claimed")
        };
        assert!(matches!(
            live.claim(ChannelDirection::OwnerToSupervisor, 1, &release_digest)
                .unwrap(),
            GuestApplicationClaim::AlreadyClaimed
        ));
        let database = root.path().join(DATABASE_NAME);
        let replacement = root.path().join("replacement.sqlite3");
        std::fs::copy(&database, &replacement).unwrap();
        std::fs::rename(&replacement, &database).unwrap();
        assert!(live.apply_once(token, |_| Ok(())).is_err());
    }

    #[test]
    fn partial_protocol_write_is_overtaken_by_sticky_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, owner, _supervisor, _bootstrap, _store_identity) =
            live_store(&root, &authority);
        let (release, release_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        live.record_frame(&release).unwrap();
        let GuestApplicationClaim::New(release_token) = live
            .claim(ChannelDirection::OwnerToSupervisor, 1, &release_digest)
            .unwrap()
        else {
            panic!("release must be newly claimed")
        };
        let (_, release_performed) = live.apply_once(release_token, |_| Ok(())).unwrap();
        live.finish(release_performed).unwrap();
        let (input, input_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            2,
            Some(release_digest),
            1,
            ExecutionChannelPayload::ProtocolBytes {
                bytes_base64: STANDARD.encode(b"abc"),
            },
        );
        live.record_frame(&input).unwrap();
        let GuestApplicationClaim::New(token) = live
            .claim(ChannelDirection::OwnerToSupervisor, 2, &input_digest)
            .unwrap()
        else {
            panic!("protocol input must be newly claimed")
        };
        let mut written = Vec::new();
        let GuestProtocolApplication::Pending(token) = live
            .apply_protocol_chunk(token, |_, remaining| {
                written.push(remaining[0]);
                Ok(1)
            })
            .unwrap()
        else {
            panic!("one byte must leave protocol input pending")
        };
        let (cancel, _cancel_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            3,
            Some(input_digest),
            1,
            ExecutionChannelPayload::Cancel,
        );
        assert!(live.record_frame(&cancel).unwrap());
        assert!(
            live.apply_protocol_chunk(token, |_, remaining| {
                written.extend_from_slice(remaining);
                Ok(remaining.len())
            })
            .is_err()
        );
        assert_eq!(written, b"a");
    }

    #[test]
    fn revocation_commit_survives_a_transcript_gap() {
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, owner, supervisor, bootstrap, store_identity) =
            live_store(&root, &authority);
        let (cancel, cancel_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            2,
            Some("4".repeat(64)),
            1,
            ExecutionChannelPayload::Cancel,
        );
        let committed = live.commit_terminal_revocation(&cancel).unwrap();
        assert!(committed.newly_recorded());
        assert_eq!(committed.frame_digest(), cancel_digest);
        assert!(matches!(
            live.try_append_terminal_observation(&committed),
            RevocationAppendOutcome::Blocked {
                revocation_committed: true,
                ..
            }
        ));
        let GuestTerminalApplicationClaim::New(token) =
            live.claim_terminal_revocation(&committed).unwrap()
        else {
            panic!("gap-crossing cancellation must retain one terminal claim")
        };
        let (_, performed) = live.apply_terminal_revocation(token, |_| Ok(())).unwrap();
        live.finish_terminal_revocation(performed).unwrap();
        let acknowledgement = live
            .ensure_supervisor_acknowledgement(&supervisor, 2, &cancel_digest)
            .unwrap()
            .unwrap();
        assert_eq!(acknowledgement.frame().acknowledged_peer_sequence, 0);
        assert!(matches!(
            &acknowledgement.frame().payload,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: 2,
                peer_frame_digest,
                application: crate::external_execution::ExecutionFrameApplication::Applied,
            } if peer_frame_digest == &cancel_digest
        ));
        drop(live);
        let recovered = RecoveredGuestJournal::open(
            lillux::PinnedDirectory::open(root.path()).unwrap().unwrap(),
            &authority,
            &store_identity,
            &bootstrap,
            &binding,
        )
        .unwrap();
        let repeated = recovered.commit_terminal_revocation(&cancel).unwrap();
        assert!(!repeated.newly_recorded());
        assert!(matches!(
            recovered.try_append_terminal_observation(&repeated),
            RevocationAppendOutcome::Blocked {
                revocation_committed: true,
                ..
            }
        ));
    }

    #[test]
    fn terminal_stop_is_durably_one_shot() {
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, owner, _supervisor, _bootstrap, _store_identity) =
            live_store(&root, &authority);
        let (cancel, _) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            0,
            ExecutionChannelPayload::Cancel,
        );
        let committed = live.commit_terminal_revocation(&cancel).unwrap();
        assert!(matches!(
            live.try_append_terminal_observation(&committed),
            RevocationAppendOutcome::Appended
        ));
        let GuestTerminalApplicationClaim::New(token) =
            live.claim_terminal_revocation(&committed).unwrap()
        else {
            panic!("first terminal stop must acquire its durable claim")
        };
        let mut stops = 0;
        let (_, performed) = live
            .apply_terminal_revocation(token, |_| {
                stops += 1;
                Ok(())
            })
            .unwrap();
        let ack = live.finish_terminal_revocation(performed).unwrap();
        assert_eq!(ack.frame_digest(), committed.frame_digest());
        assert!(matches!(
            live.claim_terminal_revocation(&committed).unwrap(),
            GuestTerminalApplicationClaim::AlreadyApplied
        ));
        assert_eq!(stops, 1);
    }

    #[test]
    fn failed_terminal_stop_remains_claimed_unknown() {
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, owner, _supervisor, _bootstrap, _store_identity) =
            live_store(&root, &authority);
        let (cancel, _) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            0,
            ExecutionChannelPayload::Cancel,
        );
        let committed = live.commit_terminal_revocation(&cancel).unwrap();
        let GuestTerminalApplicationClaim::New(token) =
            live.claim_terminal_revocation(&committed).unwrap()
        else {
            panic!("first terminal stop must acquire its durable claim")
        };
        assert!(
            live.apply_terminal_revocation::<()>(token, |_| anyhow::bail!("lost response"))
                .is_err()
        );
        assert!(matches!(
            live.claim_terminal_revocation(&committed).unwrap(),
            GuestTerminalApplicationClaim::AlreadyClaimed
        ));
    }
}
