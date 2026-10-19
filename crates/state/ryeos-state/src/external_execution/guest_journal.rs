//! Protected single-binding journal owned by an external candidate supervisor.
//!
//! Fresh creation durably records launch intent before a native launcher is
//! prepared. Reopen is deliberately recovery-only: it cannot claim or finish
//! applications and cannot manufacture a launcher. Actual side effects occur
//! while an opaque one-shot application token holds the same SQLite writer
//! gate used by sticky revocation.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use lillux_sqlite::{
    OriginalNativeBoundMode, OriginalNativeConnection, OriginalNativeConnectionTerminal,
    OriginalNativeDestinationAuthority, OriginalNativeDomain, OriginalNativeFactory,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::export::{ValidatedCandidateRetention, validated_retained_candidate_root_sets};
use super::journal::{self, ApplicationClaim, JournalOwner};
use super::native_journal::{self, NativeJournalConnection};
use super::{
    AuthenticatedExecutionFrame, ChannelDirection, ExecutionChannelBinding,
    ExecutionChannelPayload, ExternalExecutionMode, NativeNamespaceExit,
    NativeQualificationScopeSettlement, NativeQualificationScopeSettlementObservation,
    NativeWriterExclusionMechanism, SignedExecutionFrame,
};
use crate::operational::RetainTerminal;
use crate::sqlite_schema::SchemaSqlDefinition;
use crate::{
    DurableCasPublicationKey, DurableCasUploadStage, DurableExternalCandidateReceipt,
    PinnedStateAuthority,
};

fn require_q_program(
    program: &super::qualification_session::AdmittedExternalQualificationSessionProgram,
    binding: &ExecutionChannelBinding,
) -> Result<()> {
    program.validate()?;
    binding.validate()?;
    ensure!(
        matches!(
            binding.execution_mode,
            ExternalExecutionMode::QualificationStructuredSession {}
        ) && program.digest()? == binding.candidate_program_digest
            && program.runtime_manifest_hash == binding.supervisor_runtime_hash,
        "Q settlement changed its admitted program"
    );
    Ok(())
}

const DATABASE_NAME: &str = "external-candidate.sqlite3";
const APPLICATION_ID: i32 = 0x5259_4547; // RYEG
const SCHEMA_EPOCH: i64 = 13;

const OWNER_SQL: &str = r#"
CREATE TABLE external_guest_meta (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    schema_epoch INTEGER NOT NULL CHECK(schema_epoch=13),
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
CREATE TABLE external_guest_runtime_applied (
    binding_digest TEXT PRIMARY KEY,
    launcher_occurrence_digest TEXT NOT NULL,
    candidate_program_digest TEXT NOT NULL,
    receipt_digest TEXT NOT NULL,
    receipt_json TEXT NOT NULL,
    FOREIGN KEY(binding_digest) REFERENCES external_guest_launcher_ready(binding_digest)
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
    output_capture_hash TEXT,
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
    output_capture_hash TEXT,
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
CREATE TRIGGER external_guest_runtime_applied_no_update
BEFORE UPDATE ON external_guest_runtime_applied
BEGIN SELECT RAISE(ABORT, 'external guest applied runtime is immutable'); END;
CREATE TRIGGER external_guest_runtime_applied_no_delete
BEFORE DELETE ON external_guest_runtime_applied
BEGIN SELECT RAISE(ABORT, 'external guest applied runtime cannot be deleted'); END;
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

// Qualification journals are fresh and mode-specific. Preserve the exact
// ordinary schema for recovery of existing structured/direct occurrences.
const QUALIFICATION_SETTLEMENT_SQL: &str = r#"
CREATE TABLE external_guest_qualification_settlement (
    binding_digest TEXT PRIMARY KEY,
    completion_frame_digest TEXT NOT NULL UNIQUE,
    observation_json TEXT NOT NULL,
    program_json TEXT NOT NULL,
    durable_stage_id TEXT,
    FOREIGN KEY(binding_digest) REFERENCES external_execution_channel(binding_digest)
);
CREATE TRIGGER external_guest_qualification_settlement_no_update
BEFORE UPDATE ON external_guest_qualification_settlement
BEGIN SELECT RAISE(ABORT, 'external guest qualification settlement is immutable'); END;
CREATE TRIGGER external_guest_qualification_settlement_no_delete
BEFORE DELETE ON external_guest_qualification_settlement
BEGIN SELECT RAISE(ABORT, 'external guest qualification settlement cannot be deleted'); END;
"#;

fn complete_schema(mode: ExternalExecutionMode) -> String {
    if matches!(
        mode,
        ExternalExecutionMode::QualificationStructuredSession {}
    ) {
        format!(
            "{}\n{}\n{}",
            journal::CHANNEL_SQL,
            OWNER_SQL,
            QUALIFICATION_SETTLEMENT_SQL
        )
    } else {
        format!("{}\n{}", journal::CHANNEL_SQL, OWNER_SQL)
    }
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

fn retained_launcher_occurrence(
    conn: &Connection,
    binding_digest: &str,
) -> Result<LauncherOccurrenceEvidence> {
    let (process_json, launcher_artifact_digest, channel_challenge_digest): (
        String,
        String,
        String,
    ) = conn.query_row(
        "SELECT process_identity_json,launcher_artifact_digest,channel_challenge_digest
         FROM external_guest_launcher_occurrence WHERE binding_digest=?1",
        [binding_digest],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let process_identity: lillux::ExactProcessIdentity = serde_json::from_str(&process_json)?;
    ensure!(
        process_json == lillux::canonical_json(&serde_json::to_value(&process_identity)?)?,
        "guest launcher process identity is noncanonical"
    );
    LauncherOccurrenceEvidence::from_held_launcher(
        process_identity,
        &launcher_artifact_digest,
        &channel_challenge_digest,
    )
}

fn retained_launcher_occurrence_digest(conn: &Connection, binding_digest: &str) -> Result<String> {
    retained_launcher_occurrence(conn, binding_digest)?.digest()
}

struct GuestStore {
    conn: NativeJournalConnection,
    /// Complete schema SQL of this binding's mode, captured from a closed
    /// native reference whose terminal is already in the host's census.
    schema_reference: SchemaSqlDefinition,
    directory: Arc<lillux::PinnedDirectory>,
    _lifetime_lock: lillux::PinnedDirectoryLock,
    /// The reservation's own descriptor on the main inode. It is closed only
    /// after the connection closed: closing another descriptor on that inode
    /// earlier would drop the locks SQLite holds on it.
    reservation_pin: Option<File>,
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

/// Exact empty guest-store inode reserved before the outer supervisor records
/// launch intent. The retained identity can be anchored independently before
/// this value initializes any guest journal state. It is process-local and has
/// no reopen path: a crash after outer launch intent is recovery-only and must
/// never manufacture another launcher.
pub struct ReservedGuestJournal {
    directory: lillux::PinnedDirectory,
    lifetime_lock: lillux::PinnedDirectoryLock,
    database_file: File,
    bootstrap_digest: String,
    state_runtime_identity_json: String,
    binding: ExecutionChannelBinding,
    store_identity: GuestJournalStoreIdentity,
    factory: Arc<OriginalNativeFactory>,
    destination: Arc<dyn OriginalNativeDestinationAuthority>,
}

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

/// Retained journal testimony about a held launcher occurrence and readiness.
/// State authenticates immutable rows and owner custody, not the process or
/// handshake described by them. The live executor must independently join
/// these records to its actual process and authenticated channel. This is
/// neither executable authority nor qualification testimony.
#[derive(Debug)]
pub struct RetainedLauncherReady {
    binding_digest: String,
    occurrence: LauncherOccurrenceEvidence,
    occurrence_digest: String,
    handshake_transcript_digest: String,
}

impl RetainedLauncherReady {
    pub fn binding_digest(&self) -> &str {
        &self.binding_digest
    }
    pub fn process_identity(&self) -> &lillux::ExactProcessIdentity {
        &self.occurrence.process_identity
    }
    pub fn retained_launcher_artifact_digest(&self) -> &str {
        &self.occurrence.launcher_artifact_digest
    }
    pub fn channel_challenge_digest(&self) -> &str {
        &self.occurrence.channel_challenge_digest
    }
    pub fn occurrence_digest(&self) -> &str {
        &self.occurrence_digest
    }
    pub fn handshake_transcript_digest(&self) -> &str {
        &self.handshake_transcript_digest
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
    pub output_capture_hash: Option<String>,
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

impl GuestJournalStoreIdentity {
    pub fn bootstrap_digest(&self) -> &str {
        &self.bootstrap_digest
    }

    pub fn binding_digest(&self) -> &str {
        &self.binding_digest
    }

    pub fn digest(&self) -> Result<String> {
        ensure!(
            self.schema == 1,
            "unsupported guest journal identity schema"
        );
        hash(&self.journal_nonce, "guest journal nonce")?;
        hash(&self.bootstrap_digest, "external bootstrap digest")?;
        hash(&self.binding_digest, "external binding digest")?;
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&serde_json::to_value(self)?)?.as_bytes(),
        ))
    }
}

impl ReservedGuestJournal {
    pub fn store_identity(&self) -> &GuestJournalStoreIdentity {
        &self.store_identity
    }

    pub fn bootstrap_digest(&self) -> &str {
        &self.bootstrap_digest
    }

    pub fn binding(&self) -> &ExecutionChannelBinding {
        &self.binding
    }

    /// Initialize the already-anchored exact inode. This can be consumed only
    /// by the process that reserved it; recovery never reopens this authority.
    /// The reserved main is still zero-length, so the native `Existing` open
    /// stages it into WAL before its first write, and the bound main inode
    /// must be the one the reservation pinned and anchored. The schema
    /// reference closes into `retain` before the journal's connection opens.
    pub fn initialize(self, retain: RetainTerminal<'_>) -> Result<PreparedGuestJournal> {
        let Self {
            directory,
            lifetime_lock,
            database_file,
            bootstrap_digest,
            state_runtime_identity_json,
            binding,
            store_identity,
            factory,
            destination,
        } = self;
        ensure_reserved_file(&directory, &database_file)?;
        ensure!(
            lillux::observe_open_regular_file(&database_file)?.size() == 0,
            "reserved external guest database is not empty"
        );
        ensure!(
            directory.entry_names()? == [OsString::from(DATABASE_NAME)],
            "reserved external guest directory gained an ambient entry"
        );
        let reserved = lillux::pinned_regular_file_identity(&database_file)?;
        ensure!(
            reserved == store_identity.database_identity,
            "reserved external guest database changed its anchored inode"
        );
        // SAFETY: this journal holds the directory's exclusive lifetime lock
        // for the whole capture; the reserving caller's factory and
        // destination come from the process's one funded native host.
        let schema_reference = unsafe {
            native_journal::capture_schema_reference(
                OriginalNativeDomain::ExternalGuestJournalSchemaReference,
                &complete_schema(binding.execution_mode),
                &factory,
                Arc::clone(&destination),
                retain,
            )
        }?;
        let directory = Arc::new(directory);
        // SAFETY: this journal holds the directory's exclusive lifetime lock
        // for the connection's whole life; the reserving caller's factory and
        // destination come from the process's one funded native host.
        let conn = NativeJournalConnection::Live(unsafe {
            open_native(
                Arc::clone(&directory),
                OriginalNativeBoundMode::Existing,
                OriginalNativeDomain::ExternalGuestJournal,
                &factory,
                destination,
            )
        }?);
        let initialized = (|| -> Result<()> {
            ensure!(
                conn.main_identity()? == reserved,
                "external guest native keeper bound an inode other than the reservation"
            );
            let binding_digest = binding.digest()?;
            let binding_json = lillux::canonical_json(&serde_json::to_value(&binding)?)?;
            let directory_identity_json =
                lillux::canonical_json(&serde_json::to_value(directory.identity()?)?)?;
            let database_identity_json =
                lillux::canonical_json(&serde_json::to_value(store_identity.database_identity)?)?;
            conn.with_sql(|conn| {
                configure(conn)?;
                // Configuring reads the store, which creates its WAL
                // sidecars; their directory entries become durable before
                // the first commit appends to `-wal`.
                directory.sync()?;
                conn.execute_batch(&complete_schema(binding.execution_mode))?;
                conn.pragma_update(None, "application_id", APPLICATION_ID)?;
                conn.pragma_update(None, "user_version", SCHEMA_EPOCH)?;
                let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
                tx.execute(
                    "INSERT INTO external_guest_meta VALUES(1,?1,?2,?3,?4,?5,?6,?7,'launch_intent')",
                    params![
                        SCHEMA_EPOCH,
                        &bootstrap_digest,
                        &binding_digest,
                        &store_identity.journal_nonce,
                        &directory_identity_json,
                        &database_identity_json,
                        &state_runtime_identity_json
                    ],
                )?;
                tx.execute(
                    "INSERT INTO external_execution_channel VALUES(?1,?2,?3,'prepared',NULL,NULL,NULL,NULL)",
                    params![&binding.placement_thread_id, &binding_digest, &binding_json],
                )?;
                tx.commit()?;
                Ok(())
            })?;
            Ok(())
        })();
        if let Err(error) = initialized {
            let error = conn.refuse(error);
            drop((lifetime_lock, database_file));
            return Err(error);
        }
        let store = GuestStore {
            conn,
            schema_reference,
            directory,
            _lifetime_lock: lifetime_lock,
            reservation_pin: Some(database_file),
            bootstrap_digest,
            state_runtime_identity_json,
            binding,
            store_identity,
        };
        if let Err(error) = store.validate() {
            return Err(store.refuse(error));
        }
        Ok(PreparedGuestJournal(store))
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

    /// Reserve and initialize in one step; the schema reference closes into
    /// `retain` before the journal's connection opens.
    pub fn create(
        directory: lillux::PinnedDirectory,
        authority: &PinnedStateAuthority,
        bootstrap_digest: &str,
        binding: ExecutionChannelBinding,
        factory: &Arc<OriginalNativeFactory>,
        destination: Arc<dyn OriginalNativeDestinationAuthority>,
        retain: RetainTerminal<'_>,
    ) -> Result<Self> {
        Self::reserve(
            directory,
            authority,
            bootstrap_digest,
            binding,
            factory,
            destination,
        )?
        .initialize(retain)
    }

    /// Close this journal's native connection; a refusal keeps the journal.
    pub fn close(
        self,
    ) -> std::result::Result<OriginalNativeConnectionTerminal, (Self, anyhow::Error)> {
        self.0
            .close()
            .map_err(|(store, error)| (Self(store), error))
    }

    /// Reserve the exact empty database inode so an independent outer owner
    /// can durably retain [`GuestJournalStoreIdentity`] before launch intent.
    /// The native factory and destination are kept for [`ReservedGuestJournal::initialize`];
    /// no ticket is taken until then.
    pub fn reserve(
        directory: lillux::PinnedDirectory,
        authority: &PinnedStateAuthority,
        bootstrap_digest: &str,
        binding: ExecutionChannelBinding,
        factory: &Arc<OriginalNativeFactory>,
        destination: Arc<dyn OriginalNativeDestinationAuthority>,
    ) -> Result<ReservedGuestJournal> {
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
        let binding_digest = binding.digest()?;
        let state_runtime_identity_json = lillux::canonical_json(&serde_json::to_value(
            authority.runtime_directory().identity()?,
        )?)?;
        let journal_nonce = lillux::sha256_hex(&lillux::crypto::generate_random_bytes::<32>());
        let database_identity = lillux::pinned_regular_file_identity(&database_file)?;
        let store_identity = GuestJournalStoreIdentity {
            schema: 1,
            journal_nonce,
            directory_identity: directory.identity()?,
            database_identity,
            bootstrap_digest: bootstrap_digest.to_owned(),
            binding_digest,
            state_runtime_identity_json: state_runtime_identity_json.clone(),
        };
        database_file.sync_all()?;
        directory.sync()?;
        store_identity.digest()?;
        Ok(ReservedGuestJournal {
            directory,
            lifetime_lock,
            database_file,
            bootstrap_digest: bootstrap_digest.to_owned(),
            state_runtime_identity_json,
            binding,
            store_identity,
            factory: Arc::clone(factory),
            destination,
        })
    }

    /// Persist one exact held launcher occurrence before releasing it. A
    /// refusal consumes this stage: its connection closes and the terminal
    /// rides the returned error.
    pub fn bind_launcher(self, evidence: LauncherOccurrenceEvidence) -> Result<BoundGuestJournal> {
        let bound = (|| -> Result<()> {
            self.0.require_database_identity()?;
            self.0.conn.with_sql(|conn| {
                let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
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
                Ok(())
            })?;
            self.0.validate()
        })();
        match bound {
            Ok(()) => Ok(BoundGuestJournal(self.0)),
            Err(error) => Err(self.0.refuse(error)),
        }
    }
}

impl BoundGuestJournal {
    /// Persist readiness only after the live executor has authenticated the
    /// exact held occurrence over its inherited deadline-bounded channel.
    /// A refusal consumes this stage: its connection closes and the terminal
    /// rides the returned error.
    pub fn mark_launcher_ready(
        self,
        ready: AuthenticatedLauncherReady,
    ) -> Result<LiveGuestJournal> {
        let marked = (|| -> Result<()> {
            self.0.require_database_identity()?;
            self.0.conn.with_sql(|conn| {
                let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
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
                Ok(())
            })?;
            self.0.validate()
        })();
        match marked {
            Ok(()) => Ok(LiveGuestJournal(self.0)),
            Err(error) => Err(self.0.refuse(error)),
        }
    }

    /// Close this journal's native connection; a refusal keeps the journal.
    pub fn close(
        self,
    ) -> std::result::Result<OriginalNativeConnectionTerminal, (Self, anyhow::Error)> {
        self.0
            .close()
            .map_err(|(store, error)| (Self(store), error))
    }
}

impl RecoveredGuestJournal {
    /// Reopen the retained guest journal under its outer anchor. Its schema
    /// reference closes into `retain` before the journal's own connection
    /// opens. A refusal raised after the native open carries the closed
    /// terminal in its error.
    pub fn open(
        directory: lillux::PinnedDirectory,
        authority: &PinnedStateAuthority,
        store_identity: &GuestJournalStoreIdentity,
        bootstrap_digest: &str,
        binding: &ExecutionChannelBinding,
        factory: &Arc<OriginalNativeFactory>,
        destination: Arc<dyn OriginalNativeDestinationAuthority>,
        retain: RetainTerminal<'_>,
    ) -> Result<Self> {
        let store = GuestStore::open_existing(
            directory,
            authority,
            store_identity,
            bootstrap_digest,
            binding,
            factory,
            destination,
            retain,
        )?;
        if let Err(error) = store.validate_retained_content(authority) {
            return Err(store.refuse(error));
        }
        Ok(Self(store))
    }

    /// Close this journal's native connection; a refusal keeps the journal.
    pub fn close(
        self,
    ) -> std::result::Result<OriginalNativeConnectionTerminal, (Self, anyhow::Error)> {
        self.0
            .close()
            .map_err(|(store, error)| (Self(store), error))
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
    /// Close this journal's native connection; a refusal keeps the journal.
    pub fn close(
        self,
    ) -> std::result::Result<OriginalNativeConnectionTerminal, (Self, anyhow::Error)> {
        self.0
            .close()
            .map_err(|(store, error)| (Self(store), error))
    }

    /// Read immutable occurrence and readiness testimony under the same
    /// pinned journal and live owner. Process and channel corroboration
    /// belongs to the executor; these records alone do not prove execution.
    pub fn retained_launcher_ready(&self) -> Result<RetainedLauncherReady> {
        self.0.validate()?;
        let binding_digest = self.0.binding.digest()?;
        let (occurrence, handshake_transcript_digest) = self.0.conn.with_sql(|conn| {
            let lifecycle: String = conn.query_row(
                "SELECT lifecycle FROM external_guest_meta WHERE singleton=1 AND binding_digest=?1",
                [&binding_digest],
                |row| row.get(0),
            )?;
            ensure!(
                lifecycle == "ready",
                "launcher observation lacks authenticated readiness"
            );
            let occurrence = retained_launcher_occurrence(conn, &binding_digest)?;
            let handshake_transcript_digest: String = conn.query_row(
                "SELECT handshake_transcript_digest FROM external_guest_launcher_ready WHERE binding_digest=?1",
                [&binding_digest], |row| row.get(0),
            )?;
            Ok((occurrence, handshake_transcript_digest))
        })?;
        let occurrence_digest = occurrence.digest()?;
        hash(
            &handshake_transcript_digest,
            "observed launcher handshake transcript",
        )?;
        Ok(RetainedLauncherReady {
            binding_digest,
            occurrence,
            occurrence_digest,
            handshake_transcript_digest,
        })
    }

    pub fn binding(&self) -> &ExecutionChannelBinding {
        &self.0.binding
    }

    pub fn has_applied_release(&self) -> Result<bool> {
        self.0.require_database_identity()?;
        let binding_digest = self.0.binding.digest()?;
        self.0.conn.with_sql(|conn| {
            Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM external_execution_frame
             WHERE binding_digest=?1 AND direction='owner_to_supervisor'
               AND application='applied'
               AND json_extract(frame_json,'$.frame.payload.kind')='release')",
                [binding_digest],
                |row| row.get(0),
            )?)
        })
    }

    /// Retain the sole exact native pre-exec observation after a completed
    /// release. This is local occurrence evidence, not a signed controller
    /// projection or a qualification claim. An ambiguous commit is reconciled
    /// by reading the same row; a different receipt can never replace it.
    pub fn record_applied_runtime(
        &self,
        launcher_occurrence_digest: &str,
        receipt: &lillux::LinuxSandboxAppliedLaunchReceipt,
    ) -> Result<bool> {
        hash(
            launcher_occurrence_digest,
            "applied launcher occurrence digest",
        )?;
        ensure!(
            receipt.owned_child_pid > 0
                && receipt.namespace_pid == 1
                && receipt.effective_uid == 1
                && receipt.effective_gid == 1
                && receipt.no_new_privs
                && receipt.seccomp_mode == 2
                && receipt.post_release_mount_view.schema == 1
                && receipt.post_release_mount_view.mount_count > 0,
            "guest applied runtime receipt lacks native controls"
        );
        self.0.require_database_identity()?;
        let binding_digest = self.0.binding.digest()?;
        let receipt_json = lillux::canonical_json(&serde_json::to_value(receipt)?)?;
        let receipt_digest = lillux::sha256_hex(receipt_json.as_bytes());
        self.0.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            self.0.owner().require_owner(&tx, &self.0.binding)?;
            ensure!(
                retained_launcher_occurrence_digest(&tx, &binding_digest)?
                    == launcher_occurrence_digest,
                "guest applied runtime changed exact launcher occurrence"
            );
            let release_applied: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='owner_to_supervisor'
                   AND application='applied'
                   AND json_extract(frame_json,'$.frame.payload.kind')='release')",
                [&binding_digest],
                |row| row.get(0),
            )?;
            ensure!(
                release_applied,
                "guest applied runtime precedes durable release"
            );
            let previous: Option<(String, String, String, String)> = tx
                .query_row(
                    "SELECT launcher_occurrence_digest,candidate_program_digest,
                            receipt_digest,receipt_json
                     FROM external_guest_runtime_applied WHERE binding_digest=?1",
                    [&binding_digest],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;
            if let Some(previous) = previous {
                ensure!(
                    previous
                        == (
                            launcher_occurrence_digest.to_owned(),
                            self.0.binding.candidate_program_digest.clone(),
                            receipt_digest,
                            receipt_json,
                        ),
                    "guest applied runtime differs from retained native evidence"
                );
                tx.commit()?;
                return Ok(false);
            }
            tx.execute(
                "INSERT INTO external_guest_runtime_applied VALUES(?1,?2,?3,?4,?5)",
                params![
                    binding_digest,
                    launcher_occurrence_digest,
                    self.0.binding.candidate_program_digest,
                    receipt_digest,
                    receipt_json,
                ],
            )?;
            tx.commit()?;
            Ok(true)
        })
    }

    /// Project only the already-committed native occurrence row into one
    /// authenticated supervisor frame. A lost response re-reads the same
    /// retained frame; it never polls or relaunches the executable again.
    pub fn ensure_runtime_applied_frame(
        &self,
        signing_key: &lillux::crypto::SigningKey,
    ) -> Result<Option<AuthenticatedExecutionFrame>> {
        self.0.require_database_identity()?;
        let binding_digest = self.0.binding.digest()?;
        self.0.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            self.0.owner().require_owner(&tx, &self.0.binding)?;
            let (occurrence, program, receipt_json): (String, String, String) = tx.query_row(
                "SELECT launcher_occurrence_digest,candidate_program_digest,receipt_json
                 FROM external_guest_runtime_applied WHERE binding_digest=?1",
                [&binding_digest],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            ensure!(
                occurrence == retained_launcher_occurrence_digest(&tx, &binding_digest)?
                    && program == self.0.binding.candidate_program_digest,
                "runtime-applied projection lost its native occurrence"
            );
            let payload = ExecutionChannelPayload::RuntimeApplied {
                launcher_occurrence_digest: occurrence,
                candidate_program_digest: program,
                receipt: serde_json::from_str(&receipt_json)?,
            };
            let mut statement = tx.prepare(
                "SELECT frame_json FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='supervisor_to_owner'
                   AND json_extract(frame_json,'$.frame.payload.kind')='runtime_applied'
                 LIMIT 2",
            )?;
            let existing = statement
                .query_map([&binding_digest], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            ensure!(
                existing.len() <= 1,
                "guest authored duplicate runtime-applied frames"
            );
            if let Some(wire) = existing.into_iter().next() {
                let frame = SignedExecutionFrame::decode_and_verify(
                    wire.as_bytes(),
                    &self.0.binding,
                    self.0.binding.issued_at_ms,
                )?;
                ensure!(
                    frame.frame().payload == payload,
                    "guest runtime-applied frame differs from retained native row"
                );
                tx.commit()?;
                return Ok(None);
            }
            let frame = journal::author_frame(
                &tx,
                &self.0.owner(),
                &self.0.binding.placement_thread_id,
                ChannelDirection::SupervisorToOwner,
                signing_key,
                payload,
            )?;
            tx.commit()?;
            Ok(Some(frame))
        })
    }

    /// Retain the one Q namespace reap reported by the authenticated held
    /// launcher. This follows the claimed command and precedes its finish; a
    /// lost launcher response without this row remains claimed/unknown.
    pub fn record_qualification_scope_settlement(
        &self,
        authority: &PinnedStateAuthority,
        program: &super::qualification_session::AdmittedExternalQualificationSessionProgram,
        completion: &AuthenticatedExecutionFrame,
        launcher_occurrence_digest: &str,
        exit: NativeNamespaceExit,
        capture: Option<(&[u8], &str, u32)>,
    ) -> Result<NativeQualificationScopeSettlement> {
        require_q_program(program, &self.0.binding)?;
        ensure!(
            self.0.state_runtime_identity_json
                == lillux::canonical_json(&serde_json::to_value(
                    authority.runtime_directory().identity()?
                )?)?,
            "Q fixture CAS authority changed"
        );
        if let Some(retained) =
            self.qualification_scope_settlement_for_completion(authority, program, completion)?
        {
            ensure!(
                retained.observation.launcher_occurrence_digest == launcher_occurrence_digest
                    && retained.observation.exit == exit,
                "Q settlement replay changed native outcome"
            );
            match (&retained.fixture_capture, capture, program.fixture()) {
                (None, None, None) => {}
                (Some(reference), Some((bytes, blob_hash, mode)), Some(fixture)) => {
                    ensure!(
                        bytes.len() as u64 <= fixture.max_bytes
                            && mode == crate::objects::ProjectFile::REGULAR_MODE
                            && reference.blob_hash == blob_hash
                            && reference.blob_hash == lillux::sha256_hex(bytes)
                            && reference.size_bytes == bytes.len() as u64,
                        "Q settlement replay changed captured bytes"
                    );
                }
                _ => anyhow::bail!("Q settlement replay changed fixture presence"),
            }
            return Ok(retained);
        }
        let guard = authority.acquire_shared_guard()?;
        self.0.require_database_identity()?;
        ensure!(
            matches!(
                self.0.binding.execution_mode,
                ExternalExecutionMode::QualificationStructuredSession {}
            ),
            "native qualification settlement requires Q mode"
        );
        let ExecutionChannelPayload::QualificationComplete {
            completion_request_digest,
        } = &completion.frame().payload
        else {
            anyhow::bail!("native qualification settlement has no completion command")
        };
        let binding_digest = self.0.binding.digest()?;
        self.0.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            self.0.owner().require_owner(&tx, &self.0.binding)?;
            ensure!(
                !journal::revoked(&tx, &binding_digest)?
                    && completion.frame().binding_digest == binding_digest
                    && completion.frame().direction == ChannelDirection::OwnerToSupervisor
                    && retained_launcher_occurrence_digest(&tx, &binding_digest)?
                        == launcher_occurrence_digest,
                "native qualification settlement changed its live launcher authority"
            );
            let (applied_occurrence, receipt_digest): (String, String) = tx.query_row(
                "SELECT launcher_occurrence_digest,receipt_digest
                 FROM external_guest_runtime_applied WHERE binding_digest=?1",
                [&binding_digest],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let application: String = tx.query_row(
                "SELECT application FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='owner_to_supervisor'
                   AND frame_digest=?2
                   AND json_extract(frame_json,'$.frame.payload.kind')='qualification_complete'",
                params![&binding_digest, completion.digest()],
                |row| row.get(0),
            )?;
            ensure!(
                applied_occurrence == launcher_occurrence_digest && application == "claimed",
                "native qualification settlement lacks its applied runtime and claimed command"
            );
            let observation = NativeQualificationScopeSettlementObservation {
                schema: 1,
                binding_digest: binding_digest.clone(),
                launcher_occurrence_digest: applied_occurrence,
                applied_runtime_receipt_digest: receipt_digest,
                completion_frame_digest: completion.digest().to_owned(),
                completion_request_digest: completion_request_digest.clone(),
                mechanism: NativeWriterExclusionMechanism::NamespaceInitReaped,
                exit,
            };
            observation.validate(&self.0.binding)?;
            let (fixture_capture, fixture_content, stage_id) = match (program.fixture(), capture) {
                (None, None) => (None, None, None),
                (Some(fixture), Some((bytes, blob_hash, mode))) => {
                    use crate::objects::qualification_fixture::{
                        Q_FIXTURE_CAPTURE_KIND, Q_FIXTURE_CAPTURE_SCHEMA, QFixtureCaptureMetadata,
                        QFixtureCaptureReference, q_fixture_contract_digest,
                    };
                    ensure!(
                        bytes.len() as u64 <= fixture.max_bytes && bytes.len() as u64 <= 65536,
                        "native Q fixture bytes exceed their bound"
                    );
                    let metadata = QFixtureCaptureMetadata {
                        kind: Q_FIXTURE_CAPTURE_KIND.into(),
                        schema: Q_FIXTURE_CAPTURE_SCHEMA,
                        fixture_contract_digest: q_fixture_contract_digest(fixture)?,
                        native_settlement: observation.clone(),
                        blob_hash: blob_hash.into(),
                        size_bytes: bytes.len() as u64,
                        normalized_mode: mode,
                    };
                    let reference = QFixtureCaptureReference {
                        metadata_hash: metadata.digest()?,
                        blob_hash: blob_hash.into(),
                        size_bytes: bytes.len() as u64,
                    };
                    reference.verify_for(&metadata, fixture, &self.0.binding, &observation, bytes)?;
                    let content =
                        crate::objects::qualification_fixture::QFixtureCaptureContent::from_bytes(
                            metadata.clone(),
                            bytes,
                        )?;
                    let key = DurableCasPublicationKey::qualification_fixture_occurrence(
                        &binding_digest,
                        completion.digest(),
                    )?;
                    let mut stage = authority
                        .require_recovery()?
                        .begin_durable_cas_upload_admitted(
                            &guard,
                            &self.0.bootstrap_digest,
                            "qualification-fixture",
                            &key,
                            None,
                        )?;
                    let cas = authority.cas_store()?;
                    ensure!(
                        stage.store_blob(&guard, &cas, bytes)? == reference.blob_hash
                            && stage.store_object(&guard, &cas, &metadata.to_value()?)?
                                == reference.metadata_hash,
                        "Q fixture CAS write changed its exact identity"
                    );
                    let receipt = stage.retain_qualification_fixture(&guard, &reference)?;
                    (Some(reference), Some(content), Some(receipt.staging_id))
                }
                _ => anyhow::bail!("native Q fixture presence differs from admitted program"),
            };
            let observation = NativeQualificationScopeSettlement {
                schema: 3,
                observation,
                fixture_capture,
                fixture_content,
            };
            observation.validate(&self.0.binding)?;
            let json = lillux::canonical_json(&serde_json::to_value(&observation)?)?;
            let program_json = lillux::canonical_json(&serde_json::to_value(program)?)?;
            tx.execute(
                "INSERT OR IGNORE INTO external_guest_qualification_settlement VALUES(?1,?2,?3,?4,?5)",
                params![
                    &binding_digest,
                    completion.digest(),
                    &json,
                    &program_json,
                    &stage_id
                ],
            )?;
            let retained: (String, String, Option<String>) = tx.query_row(
                "SELECT observation_json,program_json,durable_stage_id FROM external_guest_qualification_settlement
                 WHERE binding_digest=?1 AND completion_frame_digest=?2",
                params![&binding_digest, completion.digest()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            ensure!(
                retained == (json, program_json, stage_id),
                "native qualification settlement changed its durable row or CAS custody"
            );
            tx.commit()?;
            Ok(observation)
        })
    }

    pub fn qualification_scope_settlement_for_completion(
        &self,
        authority: &PinnedStateAuthority,
        program: &super::qualification_session::AdmittedExternalQualificationSessionProgram,
        completion: &AuthenticatedExecutionFrame,
    ) -> Result<Option<NativeQualificationScopeSettlement>> {
        require_q_program(program, &self.0.binding)?;
        self.0.validate()?;
        self.0.validate_retained_content(authority)?;
        self.0.require_database_identity()?;
        ensure!(
            matches!(
                self.0.binding.execution_mode,
                ExternalExecutionMode::QualificationStructuredSession {}
            ),
            "native qualification settlement requires Q mode"
        );
        self.0.conn.with_sql(|conn| {
        let raw: Option<String> = conn
            .query_row(
                "SELECT observation_json FROM external_guest_qualification_settlement
             WHERE binding_digest=?1 AND completion_frame_digest=?2",
                params![self.0.binding.digest()?, completion.digest()],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|raw| {
            let retained_program: String = conn.query_row(
                "SELECT program_json FROM external_guest_qualification_settlement WHERE binding_digest=?1 AND completion_frame_digest=?2",
                params![self.0.binding.digest()?, completion.digest()], |row| row.get(0))?;
            ensure!(retained_program == lillux::canonical_json(&serde_json::to_value(program)?)?, "Q settlement reader changed admitted program");
            let observation: NativeQualificationScopeSettlement =
                serde_json::from_str(&raw)?;
            observation.validate(&self.0.binding)?;
            let (occurrence, receipt_digest): (String, String) = conn.query_row(
                "SELECT launcher_occurrence_digest,receipt_digest
                 FROM external_guest_runtime_applied WHERE binding_digest=?1",
                [self.0.binding.digest()?],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            ensure!(
                raw == lillux::canonical_json(&serde_json::to_value(&observation)?)?
                    && observation.observation.completion_frame_digest == completion.digest()
                    && observation.observation.launcher_occurrence_digest == occurrence
                    && observation.observation.applied_runtime_receipt_digest == receipt_digest
                    && occurrence
                        == retained_launcher_occurrence_digest(
                            conn,
                            &self.0.binding.digest()?,
                        )?
                    && matches!(&completion.frame().payload,
                        ExecutionChannelPayload::QualificationComplete { completion_request_digest }
                        if completion_request_digest == &observation.observation.completion_request_digest),
                "retained qualification settlement changed its completion authority"
            );
            Ok(observation)
        })
        .transpose()
        })
    }

    pub fn reconcile_qualification_scope_settlement_application(
        &self,
        authority: &PinnedStateAuthority,
        program: &super::qualification_session::AdmittedExternalQualificationSessionProgram,
        completion: &AuthenticatedExecutionFrame,
    ) -> Result<GuestApplicationAck> {
        self.qualification_scope_settlement_for_completion(authority, program, completion)?
            .context("claimed qualification completion has no native settlement row")?;
        self.0.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            journal::finish_application(
                &tx,
                &self.0.owner(),
                &self.0.binding.placement_thread_id,
                ChannelDirection::OwnerToSupervisor,
                completion.frame().sequence,
                completion.digest(),
            )?;
            tx.commit()?;
            Ok(GuestApplicationAck {
                direction: ChannelDirection::OwnerToSupervisor,
                sequence: completion.frame().sequence,
                frame_digest: completion.digest().to_owned(),
            })
        })
    }

    /// Sign only the exact row after its initiating command is durably applied.
    pub fn ensure_qualification_scope_settled_frame(
        &self,
        authority: &PinnedStateAuthority,
        program: &super::qualification_session::AdmittedExternalQualificationSessionProgram,
        signing_key: &lillux::crypto::SigningKey,
        completion: &AuthenticatedExecutionFrame,
    ) -> Result<Option<AuthenticatedExecutionFrame>> {
        let observation = self
            .qualification_scope_settlement_for_completion(authority, program, completion)?
            .context("qualification scope frame has no native settlement row")?;
        self.0.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            self.0.owner().require_owner(&tx, &self.0.binding)?;
            let applied: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='owner_to_supervisor'
                   AND frame_digest=?2 AND application='applied'
                   AND json_extract(frame_json,'$.frame.payload.kind')='qualification_complete')",
                params![self.0.binding.digest()?, completion.digest()],
                |row| row.get(0),
            )?;
            ensure!(
                applied,
                "qualification native frame precedes applied completion"
            );
            let payload = ExecutionChannelPayload::QualificationScopeSettled { observation };
            let mut statement = tx.prepare(
                "SELECT frame_json FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='supervisor_to_owner'
                   AND json_extract(frame_json,'$.frame.payload.kind')='qualification_scope_settled'
                 LIMIT 2",
            )?;
            let frames = statement
                .query_map([self.0.binding.digest()?], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            ensure!(
                frames.len() <= 1,
                "qualification native settlement has duplicate frames"
            );
            if let Some(wire) = frames.into_iter().next() {
                let frame = SignedExecutionFrame::decode_and_verify(
                    wire.as_bytes(),
                    &self.0.binding,
                    self.0.binding.issued_at_ms,
                )?;
                ensure!(
                    frame.frame().payload == payload,
                    "qualification native frame changed its durable settlement row"
                );
                tx.commit()?;
                return Ok(None);
            }
            ensure!(
                !journal::revoked(&tx, &self.0.binding.digest()?)?,
                "revoked qualification execution cannot gain normal native settlement"
            );
            let frame = journal::author_frame(
                &tx,
                &self.0.owner(),
                &self.0.binding.placement_thread_id,
                ChannelDirection::SupervisorToOwner,
                signing_key,
                payload,
            )?;
            tx.commit()?;
            Ok(Some(frame))
        })
    }

    pub fn native_capture_for_quiesce(
        &self,
        quiesce_frame_digest: &str,
    ) -> Result<Option<DurableNativeCandidateCapture>> {
        hash(quiesce_frame_digest, "quiesce frame digest")?;
        self.0.require_database_identity()?;
        let binding_digest = self.0.binding.digest()?;
        self.0.conn.with_sql(|conn| {
            Ok(conn
            .query_row(
                "SELECT quiesce_frame_digest,occurrence_digest,durable_stage_id,
                        snapshot_hash,output_capture_hash,completion_request_digest,writer_exclusion_evidence_hash
                 FROM external_guest_native_capture
                 WHERE binding_digest=?1 AND quiesce_frame_digest=?2",
                params![binding_digest, quiesce_frame_digest],
                |row| {
                    Ok(DurableNativeCandidateCapture {
                        quiesce_frame_digest: row.get(0)?,
                        occurrence_digest: row.get(1)?,
                        durable_stage_id: row.get(2)?,
                        snapshot_hash: row.get(3)?,
                        output_capture_hash: row.get(4)?,
                        completion_request_digest: row.get(5)?,
                        writer_exclusion_evidence_hash: row.get(6)?,
                    })
                },
            )
            .optional()?)
        })
    }

    /// Whether this exact guest has durably retained a complete sealed export.
    /// Transport position is intentionally irrelevant: chunks may still be
    /// draining after native writer exclusion and CAS retention completed.
    pub fn has_retained_export(&self) -> Result<bool> {
        self.0.require_database_identity()?;
        self.0.conn.with_sql(|conn| {
            self.0.owner().require_owner(conn, &self.0.binding)?;
            Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM external_guest_export_retention
                 WHERE binding_digest=?1)",
                params![self.0.binding.digest()?],
                |row| row.get(0),
            )?)
        })
    }

    pub fn supervisor_export_for_capture(
        &self,
        capture: &DurableNativeCandidateCapture,
    ) -> Result<Option<AuthenticatedExecutionFrame>> {
        self.0.require_database_identity()?;
        let wire: Option<String> = self.0.conn.with_sql(|conn| {
            Ok(conn
                .query_row(
                    "SELECT frame_json FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='supervisor_to_owner'
                 AND json_extract(frame_json,'$.frame.payload.kind')='export_sealed'
                 AND json_extract(frame_json,'$.frame.payload.candidate_snapshot_hash')=?2
                 AND json_extract(frame_json,'$.frame.payload.candidate_output_capture_hash') IS ?3
                 AND json_extract(frame_json,'$.frame.payload.completion_request_digest')=?4
                 AND json_extract(frame_json,'$.frame.payload.writer_exclusion_evidence_hash')=?5",
                    params![
                        self.0.binding.digest()?,
                        capture.snapshot_hash,
                        capture.output_capture_hash,
                        capture.completion_request_digest,
                        capture.writer_exclusion_evidence_hash
                    ],
                    |row| row.get(0),
                )
                .optional()?)
        })?;
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
        self.0.require_database_identity()?;
        let retained = self
            .native_capture_for_quiesce(&capture.quiesce_frame_digest)?
            .context("native capture reconciliation has no durable evidence")?;
        ensure!(
            retained == *capture,
            "native capture reconciliation changed durable evidence"
        );
        self.0.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
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
        })
    }

    pub fn record_frame(&self, wire: &[u8]) -> Result<bool> {
        self.0.require_database_identity()?;
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
        self.0.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let result = journal::append_frame(
                &tx,
                &self.0.owner(),
                &self.0.binding.placement_thread_id,
                wire,
            )?;
            tx.commit()?;
            Ok(result)
        })
    }

    /// Retain, reconcile and complete one controller acknowledgement as an
    /// atomic no-effect application. This must be used only for received
    /// acknowledgements; locally authored frames remain pending remote proof.
    pub fn record_owner_acknowledgement(&self, wire: &[u8]) -> Result<bool> {
        self.0.require_database_identity()?;
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
        self.0.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
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
        })
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
        ensure!(
            !matches!(
                payload,
                ExecutionChannelPayload::RuntimeApplied { .. }
                    | ExecutionChannelPayload::QualificationScopeSettled { .. }
            ),
            "native projection requires its exact retained row authoring path"
        );
        self.0.require_database_identity()?;
        self.0.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
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
        })
    }

    /// Author one ordered supervisor batch atomically. Candidate export uses
    /// this boundary so a late frame-count, encoded-byte, signing, or storage
    /// failure cannot retain a prefix without its exact sealed terminator.
    pub fn author_supervisor_frames(
        &self,
        signing_key: &lillux::crypto::SigningKey,
        payloads: impl IntoIterator<Item = ExecutionChannelPayload>,
    ) -> Result<Vec<AuthenticatedExecutionFrame>> {
        self.0.require_database_identity()?;
        self.0.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let mut frames = Vec::new();
            for payload in payloads {
                ensure!(
                    !matches!(
                        payload,
                        ExecutionChannelPayload::RuntimeApplied { .. }
                            | ExecutionChannelPayload::QualificationScopeSettled { .. }
                    ),
                    "native projection requires its exact retained row authoring path"
                );
                frames.push(journal::author_frame(
                    &tx,
                    &self.0.owner(),
                    &self.0.binding.placement_thread_id,
                    ChannelDirection::SupervisorToOwner,
                    signing_key,
                    payload,
                )?);
            }
            ensure!(
                !frames.is_empty(),
                "external supervisor frame batch is empty"
            );
            tx.commit()?;
            Ok(frames)
        })
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
        self.0.require_database_identity()?;
        self.0.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
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
        })
    }

    pub fn pending_supervisor_transport_frames(
        &self,
        frame_limit: usize,
        byte_limit: usize,
    ) -> Result<Vec<journal::PendingExecutionFrame>> {
        self.0.require_database_identity()?;
        self.0.conn.with_sql(|conn| {
            journal::pending_transport_frames(
                conn,
                &self.0.owner(),
                &self.0.binding.placement_thread_id,
                ChannelDirection::SupervisorToOwner,
                frame_limit,
                byte_limit,
            )
        })
    }

    pub fn pending_supervisor_transport_frames_after(
        &self,
        after_sequence: u64,
        frame_limit: usize,
        byte_limit: usize,
    ) -> Result<Vec<journal::PendingExecutionFrame>> {
        self.0.require_database_identity()?;
        self.0.conn.with_sql(|conn| {
            journal::pending_transport_frames_after(
                conn,
                &self.0.owner(),
                &self.0.binding.placement_thread_id,
                ChannelDirection::SupervisorToOwner,
                after_sequence,
                frame_limit,
                byte_limit,
            )
        })
    }

    pub fn claim(
        &self,
        direction: ChannelDirection,
        sequence: u64,
        digest: &str,
    ) -> Result<GuestApplicationClaim> {
        self.0.require_database_identity()?;
        self.0.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
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
        self.0.require_database_identity()?;
        self.0.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
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
        })
    }

    pub fn apply_terminal_revocation<T>(
        &self,
        token: GuestTerminalApplicationToken,
        stop: impl FnOnce(&AuthenticatedExecutionFrame) -> Result<T>,
    ) -> Result<(T, PerformedGuestTerminalApplication)> {
        self.0.require_database_identity()?;
        self.0.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
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
        })
    }

    pub fn finish_terminal_revocation(
        &self,
        performed: PerformedGuestTerminalApplication,
    ) -> Result<GuestApplicationAck> {
        self.0.require_database_identity()?;
        let frame = performed.0;
        self.0.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
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
        })
    }

    /// Open the writer transaction that gates one claimed application. The
    /// caller checked the database identity before entering its SQL scope.
    fn application_transaction<'c>(
        &self,
        conn: &'c Connection,
        frame: &AuthenticatedExecutionFrame,
    ) -> Result<Transaction<'c>> {
        ensure!(
            frame.frame().binding_digest == self.0.binding.digest()?,
            "guest application gate changed channel authority"
        );
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
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
        self.0.require_database_identity()?;
        let value = self.0.conn.with_sql(|conn| {
            let tx = self.application_transaction(conn, &token.frame)?;
            let value = action(&token.frame)?;
            tx.commit()?;
            Ok(value)
        })?;
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
        self.0.require_database_identity()?;
        let written = self.0.conn.with_sql(|conn| {
            let tx = self.application_transaction(conn, &token.frame)?;
            let written = action(&token.frame, &bytes[token.protocol_offset..])?;
            ensure!(
                written > 0 && written <= bytes.len() - token.protocol_offset,
                "protocol application reported an invalid bounded write"
            );
            tx.commit()?;
            Ok(written)
        })?;
        token.protocol_offset += written;
        if token.protocol_offset == bytes.len() {
            Ok(GuestProtocolApplication::Performed(
                PerformedGuestApplication(token.frame),
            ))
        } else {
            Ok(GuestProtocolApplication::Pending(token))
        }
    }

    pub fn finish(&self, performed: PerformedGuestApplication) -> Result<GuestApplicationAck> {
        self.0.require_database_identity()?;
        let frame = performed.0;
        self.0.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
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
        self.0.require_database_identity()?;
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
                        candidate_output_capture_hash,
                        completion_request_digest,
                        writer_exclusion_evidence_hash,
                    } if candidate_snapshot_hash == content.snapshot_hash()
                        && candidate_output_capture_hash.as_deref()
                            == content.output_capture_hash()
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
        self.0.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            self.0.owner().require_owner(&tx, &self.0.binding)?;
            let matches_frame: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='supervisor_to_owner'
                 AND frame_digest=?2
                 AND json_extract(frame_json,'$.frame.payload.kind')='export_sealed'
                 AND json_extract(frame_json,'$.frame.payload.candidate_snapshot_hash')=?3
                 AND json_extract(frame_json,'$.frame.payload.candidate_output_capture_hash') IS ?4
                 AND json_extract(frame_json,'$.frame.payload.completion_request_digest')=?5
                 AND json_extract(frame_json,'$.frame.payload.writer_exclusion_evidence_hash')=?6)",
                params![
                    content.channel_binding_digest(),
                    sealed_frame_digest,
                    content.snapshot_hash(),
                    content.output_capture_hash(),
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
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    content.channel_binding_digest(),
                    sealed_frame_digest,
                    occurrence_digest,
                    receipt.staging_id(),
                    content.snapshot_hash(),
                    content.output_capture_hash(),
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
                    content.output_capture_hash(),
                    content.completion_request_digest(),
                    content.claimed_writer_exclusion_evidence_hash(),
                )?;
                let exact: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM external_guest_export_retention
                     WHERE binding_digest=?1 AND sealed_frame_digest=?2
                     AND occurrence_digest=?3 AND durable_receipt_id=?4
                     AND snapshot_hash=?5 AND output_capture_hash IS ?6
                     AND completion_request_digest=?7
                     AND writer_exclusion_evidence_hash=?8)",
                    params![
                        content.channel_binding_digest(),
                        sealed_frame_digest,
                        occurrence_digest,
                        receipt.staging_id(),
                        content.snapshot_hash(),
                        content.output_capture_hash(),
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
        })
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
        self.0.require_database_identity()?;
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
        if let Some(output_capture_hash) = &capture.output_capture_hash {
            hash(output_capture_hash, "candidate output capture hash")?;
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
            capture.output_capture_hash.as_deref(),
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
        self.0.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
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
                "INSERT OR IGNORE INTO external_guest_native_capture VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    self.0.binding.digest()?,
                    capture.quiesce_frame_digest,
                    capture.occurrence_digest,
                    capture.durable_stage_id,
                    capture.snapshot_hash,
                    capture.output_capture_hash,
                    capture.completion_request_digest,
                    capture.writer_exclusion_evidence_hash
                ],
            )?;
            if changed == 0 {
                let exact: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM external_guest_native_capture
                     WHERE binding_digest=?1 AND quiesce_frame_digest=?2
                     AND occurrence_digest=?3 AND durable_stage_id=?4
                     AND snapshot_hash=?5 AND output_capture_hash IS ?6
                     AND completion_request_digest=?7
                     AND writer_exclusion_evidence_hash=?8)",
                    params![
                        self.0.binding.digest()?,
                        capture.quiesce_frame_digest,
                        capture.occurrence_digest,
                        capture.durable_stage_id,
                        capture.snapshot_hash,
                        capture.output_capture_hash,
                        capture.completion_request_digest,
                        capture.writer_exclusion_evidence_hash
                    ],
                    |row| row.get(0),
                )?;
                ensure!(exact, "native capture conflicts with its durable record");
            }
            tx.commit()?;
            Ok(())
        })
    }
}

impl GuestStore {
    fn open_existing(
        directory: lillux::PinnedDirectory,
        authority: &PinnedStateAuthority,
        store_identity: &GuestJournalStoreIdentity,
        bootstrap_digest: &str,
        binding: &ExecutionChannelBinding,
        factory: &Arc<OriginalNativeFactory>,
        destination: Arc<dyn OriginalNativeDestinationAuthority>,
        retain: RetainTerminal<'_>,
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
        // WAL sidecars a crash left behind are admitted: the native keeper
        // pins them and SQLite replays their committed frames. A rollback
        // journal is refused.
        native_journal::require_wal_entries(&directory, DATABASE_NAME, "external guest store")?;
        {
            // Metadata check before any SQLite effect: this descriptor is
            // closed before the native keeper takes its own. A reserved inode
            // that was never initialized is refused here, so recovery never
            // stages or writes it.
            let database_file = directory
                .open_regular(OsStr::new(DATABASE_NAME), false)?
                .context("external guest database is absent")?;
            ensure!(
                lillux::matches_pinned_regular_file_identity(
                    &database_file,
                    store_identity.database_identity
                )?,
                "external guest database changed its outer inode identity"
            );
            ensure!(
                lillux::observe_open_regular_file(&database_file)?.size() > 0,
                "external guest database was reserved but never initialized"
            );
        }
        let state_runtime_identity_json = lillux::canonical_json(&serde_json::to_value(
            authority.runtime_directory().identity()?,
        )?)?;
        ensure!(
            store_identity.state_runtime_identity_json == state_runtime_identity_json,
            "external guest store changed receiver runtime authority"
        );
        // SAFETY: this journal holds the directory's exclusive lifetime lock
        // for the whole capture; the caller's factory and destination come
        // from the process's one funded native host.
        let schema_reference = unsafe {
            native_journal::capture_schema_reference(
                OriginalNativeDomain::ExternalGuestJournalSchemaReference,
                &complete_schema(binding.execution_mode),
                factory,
                Arc::clone(&destination),
                retain,
            )
        }?;
        let directory = Arc::new(directory);
        // SAFETY: this journal holds the directory's exclusive lifetime lock
        // for the connection's whole life; the caller's factory and
        // destination come from the process's one funded native host.
        let conn = NativeJournalConnection::Live(unsafe {
            open_native(
                Arc::clone(&directory),
                OriginalNativeBoundMode::Existing,
                OriginalNativeDomain::ExternalGuestJournal,
                factory,
                destination,
            )
        }?);
        let configured = (|| -> Result<()> {
            ensure!(
                conn.main_identity()? == store_identity.database_identity,
                "external guest database changed its outer inode identity"
            );
            conn.with_sql(configure)?;
            directory.sync()?;
            Ok(())
        })();
        if let Err(error) = configured {
            return Err(conn.refuse(error));
        }
        let store = Self {
            conn,
            schema_reference,
            directory,
            _lifetime_lock: lifetime_lock,
            reservation_pin: None,
            bootstrap_digest: bootstrap_digest.to_owned(),
            state_runtime_identity_json,
            binding: binding.clone(),
            store_identity: store_identity.clone(),
        };
        if let Err(error) = store.validate() {
            return Err(store.refuse(error));
        }
        Ok(store)
    }

    /// The native keeper still binds the database name to the anchored inode.
    fn require_database_identity(&self) -> Result<()> {
        ensure!(
            self.conn.main_identity()? == self.store_identity.database_identity,
            "external guest database identity changed"
        );
        Ok(())
    }

    fn close(self) -> std::result::Result<OriginalNativeConnectionTerminal, (Self, anyhow::Error)> {
        let Self {
            conn,
            schema_reference,
            directory,
            _lifetime_lock,
            reservation_pin,
            bootstrap_digest,
            state_runtime_identity_json,
            binding,
            store_identity,
        } = self;
        // The reservation pin and the lifetime lock are released only after
        // the connection closed.
        match conn.close() {
            Ok(terminal) => Ok(terminal),
            Err((conn, error)) => Err((
                Self {
                    conn,
                    schema_reference,
                    directory,
                    _lifetime_lock,
                    reservation_pin,
                    bootstrap_digest,
                    state_runtime_identity_json,
                    binding,
                    store_identity,
                },
                error,
            )),
        }
    }

    /// Close this store because `error` refused it; the terminal, or the
    /// refused close owner, rides the returned error. The reservation pin and
    /// the lifetime lock are released only after the close.
    fn refuse(self, error: anyhow::Error) -> anyhow::Error {
        let Self {
            conn,
            schema_reference,
            directory,
            _lifetime_lock,
            reservation_pin,
            bootstrap_digest,
            state_runtime_identity_json,
            binding,
            store_identity,
        } = self;
        let error = conn.refuse(error);
        drop((
            schema_reference,
            directory,
            _lifetime_lock,
            reservation_pin,
            bootstrap_digest,
            state_runtime_identity_json,
            binding,
            store_identity,
        ));
        error
    }

    fn owner(&self) -> GuestOwner<'_> {
        GuestOwner {
            bootstrap_digest: &self.bootstrap_digest,
        }
    }

    fn validate(&self) -> Result<()> {
        self.require_database_identity()?;
        self.conn.with_sql(|conn| self.validate_sql(conn))
    }

    fn validate_sql(&self, conn: &Connection) -> Result<()> {
        let app_id: i32 = conn.query_row("PRAGMA application_id", [], |row| row.get(0))?;
        let epoch: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        ensure!(
            app_id == APPLICATION_ID && epoch == SCHEMA_EPOCH,
            "external guest database has predecessor or foreign identity"
        );
        self.schema_reference
            .require_matches(conn, &self.directory.path().join(DATABASE_NAME))?;
        self.owner().require_owner(conn, &self.binding)?;
        let (retained_nonce, retained_directory, retained_database, retained_runtime): (
            String,
            String,
            String,
            String,
        ) = conn.query_row(
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
        let lifecycle: String = conn.query_row(
            "SELECT lifecycle FROM external_guest_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        let occurrence_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM external_guest_launcher_occurrence",
            [],
            |row| row.get(0),
        )?;
        let ready_count: i64 = conn.query_row(
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
        let applied: Option<(String, String, String, String)> = conn
            .query_row(
                "SELECT launcher_occurrence_digest,candidate_program_digest,
                        receipt_digest,receipt_json
                 FROM external_guest_runtime_applied WHERE binding_digest=?1",
                [self.binding.digest()?],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if let Some((occurrence, program, digest, receipt_json)) = &applied {
            let receipt: lillux::LinuxSandboxAppliedLaunchReceipt =
                serde_json::from_str(receipt_json)?;
            let release_applied: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='owner_to_supervisor'
                   AND application='applied'
                   AND json_extract(frame_json,'$.frame.payload.kind')='release')",
                [self.binding.digest()?],
                |row| row.get(0),
            )?;
            ensure!(
                lifecycle == "ready"
                    && ready_count == 1
                    && release_applied
                    && occurrence
                        == &retained_launcher_occurrence_digest(conn, &self.binding.digest()?,)?
                    && program == &self.binding.candidate_program_digest
                    && digest == &lillux::sha256_hex(receipt_json.as_bytes())
                    && receipt_json == &lillux::canonical_json(&serde_json::to_value(&receipt)?)?
                    && receipt.owned_child_pid > 0
                    && receipt.namespace_pid == 1
                    && receipt.effective_uid == 1
                    && receipt.effective_gid == 1
                    && receipt.no_new_privs
                    && receipt.seccomp_mode == 2
                    && receipt.post_release_mount_view.schema == 1
                    && receipt.post_release_mount_view.mount_count > 0,
                "external guest applied runtime lost exact occurrence evidence"
            );
        }
        let mut applied_frames = conn.prepare(
            "SELECT frame_json FROM external_execution_frame
             WHERE binding_digest=?1 AND direction='supervisor_to_owner'
               AND json_extract(frame_json,'$.frame.payload.kind')='runtime_applied'
             LIMIT 2",
        )?;
        let applied_frames = applied_frames
            .query_map([self.binding.digest()?], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(
            applied_frames.len() <= 1,
            "guest retained duplicate runtime-applied frames"
        );
        if let Some(wire) = applied_frames.first() {
            let Some((occurrence, program, _, receipt_json)) = &applied else {
                anyhow::bail!("guest runtime-applied frame has no native occurrence row")
            };
            let frame = SignedExecutionFrame::decode_and_verify(
                wire.as_bytes(),
                &self.binding,
                self.binding.issued_at_ms,
            )?;
            ensure!(
                frame.frame().payload
                    == (ExecutionChannelPayload::RuntimeApplied {
                        launcher_occurrence_digest: occurrence.clone(),
                        candidate_program_digest: program.clone(),
                        receipt: serde_json::from_str(receipt_json)?,
                    }),
                "guest runtime-applied frame differs from native row"
            );
        }
        if matches!(
            self.binding.execution_mode,
            ExternalExecutionMode::QualificationStructuredSession {}
        ) {
            let binding_digest = self.binding.digest()?;
            let retained: Option<(String, String)> = conn
                .query_row(
                    "SELECT completion_frame_digest,observation_json
                 FROM external_guest_qualification_settlement WHERE binding_digest=?1",
                    [&binding_digest],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((completion_digest, json)) = retained {
                let observation: NativeQualificationScopeSettlement = serde_json::from_str(&json)?;
                observation.validate(&self.binding)?;
                let (applied_occurrence, _, receipt_digest, _): (String, String, String, String) =
                    applied
                        .as_ref()
                        .context("qualification settlement lost applied runtime")?
                        .clone();
                ensure!(
                    json == lillux::canonical_json(&serde_json::to_value(&observation)?)?
                        && observation.observation.completion_frame_digest == completion_digest
                        && observation.observation.launcher_occurrence_digest == applied_occurrence
                        && observation.observation.applied_runtime_receipt_digest == receipt_digest,
                    "qualification settlement changed its native occurrence or applied runtime"
                );
                let (command_wire, application): (String, String) = conn.query_row(
                    "SELECT frame_json,application FROM external_execution_frame
                     WHERE binding_digest=?1 AND direction='owner_to_supervisor'
                       AND frame_digest=?2
                       AND json_extract(frame_json,'$.frame.payload.kind')='qualification_complete'",
                    params![&binding_digest, &completion_digest],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let command = SignedExecutionFrame::decode_and_verify(
                    command_wire.as_bytes(),
                    &self.binding,
                    self.binding.issued_at_ms,
                )?;
                ensure!(
                    matches!(&command.frame().payload,
                        ExecutionChannelPayload::QualificationComplete { completion_request_digest }
                        if completion_request_digest == &observation.observation.completion_request_digest)
                        && matches!(application.as_str(), "claimed" | "applied"),
                    "qualification settlement changed its initiating command"
                );
                let signed: Vec<String> = conn.prepare(
                    "SELECT frame_json FROM external_execution_frame
                     WHERE binding_digest=?1 AND direction='supervisor_to_owner'
                       AND json_extract(frame_json,'$.frame.payload.kind')='qualification_scope_settled'
                     LIMIT 2",
                )?.query_map([&binding_digest], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                ensure!(
                    signed.len() <= 1,
                    "qualification settlement has duplicate signed frames"
                );
                if let Some(wire) = signed.first() {
                    let frame = SignedExecutionFrame::decode_and_verify(
                        wire.as_bytes(),
                        &self.binding,
                        self.binding.issued_at_ms,
                    )?;
                    ensure!(
                        application == "applied"
                            && frame.frame().payload
                                == ExecutionChannelPayload::QualificationScopeSettled {
                                    observation
                                },
                        "qualification signed settlement differs from native row"
                    );
                }
            } else {
                let signed: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM external_execution_frame
                     WHERE binding_digest=?1 AND direction='supervisor_to_owner'
                       AND json_extract(frame_json,'$.frame.payload.kind')='qualification_scope_settled')",
                    [&binding_digest], |row| row.get(0),
                )?;
                ensure!(!signed, "qualification settlement frame has no native row");
            }
        }
        journal::validate_channels(conn, &self.owner())?;
        Ok(())
    }

    fn validate_retained_content(&self, authority: &PinnedStateAuthority) -> Result<()> {
        self.conn
            .with_sql(|conn| self.validate_retained_content_sql(conn, authority))
    }

    fn validate_retained_content_sql(
        &self,
        conn: &Connection,
        authority: &PinnedStateAuthority,
    ) -> Result<()> {
        ensure!(
            self.state_runtime_identity_json
                == lillux::canonical_json(&serde_json::to_value(
                    authority.runtime_directory().identity()?
                )?)?,
            "external guest recovery changed receiver CAS authority"
        );
        let guard = authority.acquire_shared_guard()?;
        // Only the qualification mode's schema declares the settlement table;
        // every other mode's exact schema has none, so it retains no rows.
        let q_rows = if matches!(
            self.binding.execution_mode,
            ExternalExecutionMode::QualificationStructuredSession {}
        ) {
            conn.prepare(
                "SELECT observation_json,program_json,durable_stage_id FROM external_guest_qualification_settlement WHERE binding_digest=?1")?
                .query_map([self.binding.digest()?], |row| Ok((row.get::<_,String>(0)?, row.get::<_,String>(1)?, row.get::<_,Option<String>>(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        for (raw, program_json, stage_id) in q_rows {
            use crate::objects::qualification_fixture::QFixtureCaptureMetadata;
            let settlement: NativeQualificationScopeSettlement = serde_json::from_str(&raw)?;
            settlement.validate(&self.binding)?;
            let program: super::qualification_session::AdmittedExternalQualificationSessionProgram =
                serde_json::from_str(&program_json)?;
            require_q_program(&program, &self.binding)?;
            ensure!(
                program_json == lillux::canonical_json(&serde_json::to_value(&program)?)?
                    && raw == lillux::canonical_json(&serde_json::to_value(&settlement)?)?,
                "Q settlement retained noncanonical authority"
            );
            match (
                program.fixture(),
                settlement.fixture_capture.as_ref(),
                settlement.fixture_content.as_ref(),
                stage_id.as_deref(),
            ) {
                (None, None, None, None) => {}
                (Some(fixture), Some(reference), Some(content), Some(stage_id)) => {
                    let key = DurableCasPublicationKey::qualification_fixture_occurrence(
                        &self.binding.digest()?,
                        &settlement.observation.completion_frame_digest,
                    )?;
                    let stage = authority
                        .require_recovery()?
                        .open_durable_cas_upload_admitted(
                            &guard,
                            stage_id,
                            &self.bootstrap_digest,
                        )?;
                    stage.ensure_publication_contract(&key, None)?;
                    let receipt = stage.qualification_fixture_receipt()?;
                    ensure!(
                        receipt.metadata_hash == reference.metadata_hash
                            && receipt.blob_hash == reference.blob_hash,
                        "Q fixture lost its exact durable CAS custody"
                    );
                    let cas = authority.cas_store()?;
                    let value = cas
                        .get_object_bounded(&reference.metadata_hash, 4096)?
                        .context("Q fixture metadata is absent")?;
                    let metadata = QFixtureCaptureMetadata::from_value(&value)?;
                    let (file, size) = cas
                        .open_blob(&reference.blob_hash)?
                        .context("Q fixture blob is absent")?;
                    ensure!(
                        size == reference.size_bytes && size <= fixture.max_bytes,
                        "Q fixture retained blob size changed"
                    );
                    let bytes = lillux::secure_fs::read_open_regular_file_exact_bounded(
                        file,
                        size,
                        fixture.max_bytes,
                    )?;
                    reference.verify_for(
                        &metadata,
                        fixture,
                        &self.binding,
                        &settlement.observation,
                        &bytes,
                    )?;
                    ensure!(
                        content.metadata == metadata
                            && content.verify_for(
                                reference,
                                fixture,
                                &self.binding,
                                &settlement.observation
                            )? == bytes,
                        "Q settlement content differs from its authenticated retained CAS bytes"
                    );
                }
                _ => {
                    anyhow::bail!("Q fixture retained program, reference and stage presence differ")
                }
            }
        }
        let mut captures = conn.prepare(
            "SELECT quiesce_frame_digest,occurrence_digest,durable_stage_id,
                    snapshot_hash,output_capture_hash,completion_request_digest,writer_exclusion_evidence_hash
             FROM external_guest_native_capture ORDER BY binding_digest",
        )?;
        let captures = captures
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (quiesce, occurrence, stage_id, snapshot, output_capture, completion, evidence) in
            captures
        {
            let application: Option<String> = conn
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
                output_capture.as_deref(),
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
        let mut rows = conn.prepare(
            "SELECT sealed_frame_digest,occurrence_digest,durable_receipt_id,
                    snapshot_hash,output_capture_hash,completion_request_digest,writer_exclusion_evidence_hash
             FROM external_guest_export_retention ORDER BY binding_digest",
        )?;
        let retained = rows
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (sealed, occurrence, receipt_id, snapshot, output_capture, completion, evidence) in
            retained
        {
            let (objects, blobs) = validated_retained_candidate_root_sets(
                authority,
                &guard,
                &self.binding,
                &snapshot,
                output_capture.as_deref(),
                &completion,
                &evidence,
            )?;
            let matches_frame: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM external_execution_frame
                 WHERE binding_digest=?1 AND direction='supervisor_to_owner'
                 AND frame_digest=?2
                 AND json_extract(frame_json,'$.frame.payload.kind')='export_sealed'
                 AND json_extract(frame_json,'$.frame.payload.candidate_snapshot_hash')=?3
                 AND json_extract(frame_json,'$.frame.payload.candidate_output_capture_hash') IS ?4
                 AND json_extract(frame_json,'$.frame.payload.completion_request_digest')=?5
                 AND json_extract(frame_json,'$.frame.payload.writer_exclusion_evidence_hash')=?6)",
                params![
                    self.binding.digest()?,
                    sealed,
                    snapshot,
                    output_capture,
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
        self.require_database_identity()?;
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
        let newly_recorded = self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let newly_recorded = journal::record_revocation(
                &tx,
                &self.owner(),
                &self.binding.placement_thread_id,
                wire,
            )?;
            tx.commit()?;
            Ok(newly_recorded)
        })?;
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
        if let Err(error) = self.require_database_identity() {
            return RevocationAppendOutcome::Blocked {
                revocation_committed: true,
                error,
            };
        }
        let result = self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let appended = journal::append_frame(
                &tx,
                &self.owner(),
                &self.binding.placement_thread_id,
                &committed.wire,
            )?;
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
        binding: &ExecutionChannelBinding,
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
        if let ExecutionChannelPayload::RuntimeApplied {
            launcher_occurrence_digest,
            candidate_program_digest,
            receipt,
        } = payload
        {
            let retained: Option<(String, String, String, String)> = conn
                .query_row(
                    "SELECT launcher_occurrence_digest,candidate_program_digest,
                            receipt_digest,receipt_json
                     FROM external_guest_runtime_applied WHERE binding_digest=?1",
                    [binding.digest()?],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;
            let Some((occurrence, program, digest, receipt_json)) = retained else {
                anyhow::bail!("guest runtime-applied frame has no native occurrence row")
            };
            ensure!(
                occurrence == *launcher_occurrence_digest
                    && program == *candidate_program_digest
                    && program == binding.candidate_program_digest
                    && digest == lillux::sha256_hex(receipt_json.as_bytes())
                    && receipt_json == lillux::canonical_json(&serde_json::to_value(receipt)?)?,
                "guest runtime-applied frame differs from native occurrence row"
            );
        }
        if matches!(
            payload,
            ExecutionChannelPayload::QualificationComplete { .. }
        ) {
            let admitted: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM external_guest_runtime_applied a
                 WHERE a.binding_digest=?1
                   AND EXISTS(SELECT 1 FROM external_execution_frame f
                     WHERE f.binding_digest=a.binding_digest AND f.direction='owner_to_supervisor'
                       AND f.application='applied'
                       AND json_extract(f.frame_json,'$.frame.payload.kind')='release')
                   AND EXISTS(SELECT 1 FROM external_execution_frame f
                     WHERE f.binding_digest=a.binding_digest AND f.direction='supervisor_to_owner'
                       AND f.application='applied'
                       AND json_extract(f.frame_json,'$.frame.payload.kind')='runtime_applied')
                   AND EXISTS(SELECT 1 FROM external_execution_frame f
                     WHERE f.binding_digest=a.binding_digest AND f.direction='supervisor_to_owner'
                       AND f.application='applied'
                       AND json_extract(f.frame_json,'$.frame.payload.kind')='protocol_eof'))",
                [binding.digest()?],
                |row| row.get(0),
            )?;
            ensure!(
                admitted,
                "qualification completion lacks applied release, runtime and protocol EOF"
            );
        }
        if let ExecutionChannelPayload::QualificationScopeSettled { observation } = payload {
            let retained: Option<String> = conn
                .query_row(
                    "SELECT observation_json FROM external_guest_qualification_settlement
                 WHERE binding_digest=?1 AND completion_frame_digest=?2",
                    params![
                        binding.digest()?,
                        &observation.observation.completion_frame_digest
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            let expected = lillux::canonical_json(&serde_json::to_value(observation)?)?;
            ensure!(
                retained.as_deref() == Some(expected.as_str()),
                "qualification scope frame has no exact durable native row"
            );
        }
        Ok(())
    }

    fn permits_execution_input_transport(
        &self,
        conn: &Connection,
        _binding: &ExecutionChannelBinding,
    ) -> Result<bool> {
        let lifecycle: String = conn.query_row(
            "SELECT lifecycle FROM external_guest_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        Ok(lifecycle == "ready")
    }

    fn require_export_retention(
        &self,
        conn: &Connection,
        binding: &ExecutionChannelBinding,
        frame_digest: &str,
        candidate_snapshot_hash: &str,
        candidate_output_capture_hash: Option<&str>,
        completion_request_digest: &str,
        writer_exclusion_evidence_hash: &str,
    ) -> Result<()> {
        let retained: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_guest_export_retention
             WHERE binding_digest=?1 AND sealed_frame_digest=?2 AND snapshot_hash=?3
             AND output_capture_hash IS ?4
             AND completion_request_digest=?5 AND writer_exclusion_evidence_hash=?6)",
            params![
                binding.digest()?,
                frame_digest,
                candidate_snapshot_hash,
                candidate_output_capture_hash,
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

/// WAL, `synchronous=FULL` and foreign keys, each read back. The crash
/// argument for this contract is in [`super::native_journal`].
fn configure(conn: &Connection) -> Result<()> {
    native_journal::configure(conn, "external guest journal")
}

/// Open the guest database on one bound ticket of `factory`.
///
/// # Safety
/// The caller holds the exclusive lifetime lock of `directory` for as long as
/// the returned connection lives, and `factory` and `destination` come from
/// the one native host funded for this supervisor process.
unsafe fn open_native(
    directory: Arc<lillux::PinnedDirectory>,
    mode: OriginalNativeBoundMode,
    domain: OriginalNativeDomain,
    factory: &Arc<OriginalNativeFactory>,
    destination: Arc<dyn OriginalNativeDestinationAuthority>,
) -> Result<OriginalNativeConnection> {
    // SAFETY: forwarded from this function's contract.
    unsafe {
        native_journal::open_bound(
            directory,
            OsStr::new(DATABASE_NAME),
            mode,
            domain,
            factory,
            destination,
        )
    }
}

/// The reservation's pinned descriptor still names the database entry. Runs
/// before any SQLite connection exists on that inode.
fn ensure_reserved_file(directory: &lillux::PinnedDirectory, expected: &File) -> Result<()> {
    let current = directory
        .open_regular(OsStr::new(DATABASE_NAME), false)?
        .context("external guest database disappeared")?;
    ensure!(
        lillux::same_open_file_identity(expected, &current)?,
        "external guest database identity changed"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;
    use crate::external_execution::native_journal::testing::{
        census, refusal, retire, root as journal_root,
    };
    use crate::external_execution::{
        ExecutionFrame, NativeNamespaceExit, NativeWriterExclusionMechanism,
        NativeWriterExclusionObservation, SignedExecutionFrame,
    };
    use crate::objects::{ProjectFile, ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree};
    use crate::test_support::shared_test_host;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use lillux::crypto::SigningKey;
    use lillux_sqlite::test_host::TestNativeHost;

    fn binding() -> (ExecutionChannelBinding, SigningKey, SigningKey) {
        let owner = lillux::crypto::generate_signing_key();
        let supervisor = lillux::crypto::generate_signing_key();
        let now = lillux::time::timestamp_millis();
        (
            ExecutionChannelBinding {
                schema: crate::external_execution::EXECUTION_CHANNEL_BINDING_SCHEMA,
                execution_mode:
                    ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {},
                placement_thread_id: "T-guest".into(),
                allocation_request_digest: "a".repeat(64),
                occurrence_id: "occurrence-guest".into(),
                admitted_capsule_hash: "b".repeat(64),
                base_snapshot_hash: "c".repeat(64),
                execution_binding_hash: "d".repeat(64),
                supervisor_runtime_hash: "e".repeat(64),
                candidate_program_digest: "0".repeat(64),
                channel_nonce: "f".repeat(64),
                owner_public_key: STANDARD.encode(owner.verifying_key().as_bytes()),
                supervisor_public_key: STANDARD.encode(supervisor.verifying_key().as_bytes()),
                issued_at_ms: now - 1_000,
                execution_deadline_ms: now + 60_000,
                expires_at_ms: now + 120_000,
                candidate_export_max_bytes: 512 * 1024,
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
        host: &TestNativeHost,
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
        live_store_for_binding(host, root, authority, binding())
    }

    fn live_store_for_binding(
        host: &TestNativeHost,
        root: &tempfile::TempDir,
        authority: &PinnedStateAuthority,
        (binding, owner, supervisor): (ExecutionChannelBinding, SigningKey, SigningKey),
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
        let bootstrap = "9".repeat(64);
        let prepared = PreparedGuestJournal::create(
            directory,
            authority,
            &bootstrap,
            binding.clone(),
            host.factory(),
            host.destination(),
            &census(&host),
        )
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
                boot: None,
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

    #[test]
    fn launcher_readiness_observation_reads_exact_immutable_live_journal() {
        let host = shared_test_host();
        let (_state_root, authority) = state_authority();
        let root = tempfile::tempdir().unwrap();
        let (live, binding, _, _, _, _) = live_store(&host, &root, &authority);
        let observed = live.retained_launcher_ready().unwrap();
        assert_eq!(observed.binding_digest(), binding.digest().unwrap());
        assert_eq!(observed.process_identity().target_pid, 100);
        assert_eq!(observed.process_identity().target_start_time_ticks, 200);
        assert_eq!(observed.retained_launcher_artifact_digest(), "7".repeat(64));
        assert_eq!(observed.channel_challenge_digest(), "6".repeat(64));
        assert_eq!(observed.handshake_transcript_digest(), "5".repeat(64));
        assert_eq!(
            observed.occurrence_digest(),
            live.0
                .conn
                .with_sql(|conn| retained_launcher_occurrence_digest(
                    conn,
                    &binding.digest().unwrap()
                ))
                .unwrap()
        );
        assert!(
            live.0
                .conn
                .with_sql(|conn| Ok(conn
                    .execute(
                        "UPDATE external_guest_launcher_ready SET handshake_transcript_digest=?1",
                        [&"8".repeat(64)],
                    )
                    .is_err()))
                .unwrap()
        );
        assert!(
            live.0
                .conn
                .with_sql(|conn| Ok(conn
                    .execute("DELETE FROM external_guest_launcher_occurrence", [],)
                    .is_err()))
                .unwrap()
        );
        assert_eq!(
            live.retained_launcher_ready().unwrap().occurrence_digest(),
            observed.occurrence_digest()
        );
        retire(&host, live.close());
    }

    fn state_authority() -> (tempfile::TempDir, PinnedStateAuthority) {
        let root = tempfile::tempdir().unwrap();
        let db = crate::test_support::TestState::open(
            root.path(),
            std::sync::Arc::new(crate::TrustStore::new()),
        )
        .unwrap();
        let authority = db.pinned_authority().unwrap();
        drop(db);
        (root, authority)
    }

    fn recover(
        host: &TestNativeHost,
        directory: lillux::PinnedDirectory,
        authority: &PinnedStateAuthority,
        store_identity: &GuestJournalStoreIdentity,
        bootstrap_digest: &str,
        binding: &ExecutionChannelBinding,
    ) -> Result<RecoveredGuestJournal> {
        RecoveredGuestJournal::open(
            directory,
            authority,
            store_identity,
            bootstrap_digest,
            binding,
            host.factory(),
            host.destination(),
            &census(&host),
        )
    }

    /// Reopen, validate and close the retained journal; a refusal settles
    /// into the census and returns its text as the error.
    fn recover_checked(
        host: &TestNativeHost,
        root: &std::path::Path,
        authority: &PinnedStateAuthority,
        store_identity: &GuestJournalStoreIdentity,
        bootstrap_digest: &str,
        binding: &ExecutionChannelBinding,
    ) -> Result<()> {
        let directory = lillux::PinnedDirectory::open(root)?
            .ok_or_else(|| anyhow::anyhow!("guest journal root absent"))?;
        let recovered = match recover(
            host,
            directory,
            authority,
            store_identity,
            bootstrap_digest,
            binding,
        ) {
            Ok(recovered) => recovered,
            Err(error) => anyhow::bail!(refusal(host, Err::<(), _>(error))),
        };
        let checked = recovered.validate();
        retire(host, recovered.close());
        checked
    }

    fn qualification_fixture_program()
    -> super::super::qualification_session::AdmittedExternalQualificationSessionProgram {
        use super::super::admission::test_support::{
            fixture_qualification_use, fixture_requirement,
        };
        use super::super::qualification_session::{
            AdmittedExternalQualificationSessionProgram, QUALIFICATION_SESSION_PROGRAM_SCHEMA,
        };
        use crate::external_content::products::qualification::ProductQualificationConsumerRuntimeMemberIdentity;
        use ryeos_external_execution_contract::qualification_fixture::{
            Q_FIXTURE_CONTRACT_SCHEMA, Q_FIXTURE_MOUNT_DESTINATION, QFixtureContract,
        };
        let requirement = fixture_requirement();
        let program = AdmittedExternalQualificationSessionProgram {
            schema: QUALIFICATION_SESSION_PROGRAM_SCHEMA,
            qualification_purpose_digest: "7".repeat(64),
            consumer_content_digest: "8".repeat(64),
            qualification_use: fixture_qualification_use(&requirement).unwrap(),
            runtime_manifest_kind: crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
            runtime_content_kind: crate::objects::ExternalContentKind::Tree,
            runtime_manifest_hash: "e".repeat(64),
            runtime_member: ProductQualificationConsumerRuntimeMemberIdentity {
                product_declaration_id: requirement.runtime_product_declaration_id.clone(),
                relative_path: requirement.runtime_recipe.executable_relative_path.clone(),
                executable_sha256: "6".repeat(64),
            },
            runtime_recipe_digest: requirement.runtime_recipe.digest().unwrap(),
            requirement,
            fixture: Some(QFixtureContract {
                schema: Q_FIXTURE_CONTRACT_SCHEMA,
                mount_destination: Q_FIXTURE_MOUNT_DESTINATION.into(),
                leaf: "result.txt".into(),
                initial_content_b64: STANDARD.encode(b"before\n"),
                initial_blob_hash: lillux::sha256_hex(b"before\n"),
                max_bytes: 64,
            }),
        };
        program.validate().unwrap();
        program
    }

    #[test]
    fn qualification_fixture_custody_survives_interrupted_completion_and_cold_reopen() {
        let host = shared_test_host();
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let program = qualification_fixture_program();
        let (mut binding, owner, supervisor) = binding();
        binding.execution_mode = ExternalExecutionMode::QualificationStructuredSession {};
        binding.candidate_export_max_bytes = 0;
        binding.candidate_program_digest = program.digest().unwrap();
        binding.validate().unwrap();
        let (live, binding, owner, supervisor, bootstrap, store_identity) =
            live_store_for_binding(&host, &root, &authority, (binding, owner, supervisor));
        let occurrence = live
            .0
            .conn
            .with_sql(|conn| retained_launcher_occurrence_digest(conn, &binding.digest().unwrap()))
            .unwrap();
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
            panic!("fresh release must be claimable")
        };
        let (_, performed) = live.apply_once(token, |_| Ok(())).unwrap();
        live.finish(performed).unwrap();
        // Journal input data exercises custody; it does not manufacture an opaque
        // native namespace-reap proof or test native filesystem capture itself.
        let receipt = lillux::LinuxSandboxAppliedLaunchReceipt {
            owned_child_pid: 101,
            namespace_pid: 1,
            effective_uid: 1,
            effective_gid: 1,
            no_new_privs: true,
            seccomp_mode: 2,
            executable_sha256: [1; 32],
            argv_sha256: [2; 32],
            environment_sha256: [3; 32],
            cwd_sha256: [4; 32],
            post_release_mount_view: lillux::LinuxSandboxMountPreparationCommitments {
                schema: 1,
                mount_count: 2,
                destination_access_sha256: [5; 32],
            },
        };
        live.record_applied_runtime(&occurrence, &receipt).unwrap();
        let applied = live
            .ensure_runtime_applied_frame(&supervisor)
            .unwrap()
            .unwrap();
        let GuestApplicationClaim::New(token) = live
            .claim(
                ChannelDirection::SupervisorToOwner,
                applied.frame().sequence,
                applied.digest(),
            )
            .unwrap()
        else {
            panic!("runtime projection must be claimable")
        };
        let (_, performed) = live.apply_once(token, |_| Ok(())).unwrap();
        live.finish(performed).unwrap();
        let eof = live
            .author_supervisor_frame(&supervisor, ExecutionChannelPayload::ProtocolEof)
            .unwrap();
        let GuestApplicationClaim::New(token) = live
            .claim(
                ChannelDirection::SupervisorToOwner,
                eof.frame().sequence,
                eof.digest(),
            )
            .unwrap()
        else {
            panic!("EOF must be claimable")
        };
        let (_, performed) = live.apply_once(token, |_| Ok(())).unwrap();
        live.finish(performed).unwrap();
        let (completion_wire, _) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            2,
            Some(release_digest.clone()),
            eof.frame().sequence,
            ExecutionChannelPayload::QualificationComplete {
                completion_request_digest: "1".repeat(64),
            },
        );
        let completion = SignedExecutionFrame::decode_and_verify(
            &completion_wire,
            &binding,
            binding.issued_at_ms,
        )
        .unwrap();
        let GuestApplicationClaim::New(token) = live.record_and_claim(&completion_wire).unwrap()
        else {
            panic!("completion must have one effect opportunity")
        };
        let effects = std::cell::Cell::new(0);
        let (_, performed) = live
            .apply_once(token, |_| {
                effects.set(effects.get() + 1);
                Ok(())
            })
            .unwrap();
        let bytes = b"after\n";
        let blob_hash = lillux::sha256_hex(bytes);
        let capture = Some((
            bytes.as_slice(),
            blob_hash.as_str(),
            ProjectFile::REGULAR_MODE,
        ));
        assert!(
            live.record_qualification_scope_settlement(
                &authority,
                &program,
                &completion,
                &occurrence,
                NativeNamespaceExit::Code(0),
                None,
            )
            .is_err()
        );
        let settlement = live
            .record_qualification_scope_settlement(
                &authority,
                &program,
                &completion,
                &occurrence,
                NativeNamespaceExit::Code(0),
                capture,
            )
            .unwrap();
        let row = || {
            live.0.conn.with_sql(|conn| Ok(conn.query_row(
            "SELECT observation_json,program_json,durable_stage_id FROM external_guest_qualification_settlement WHERE binding_digest=?1",
            [binding.digest().unwrap()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
        )?)).unwrap()
        };
        let original_row = row();
        let reference = settlement.fixture_capture.as_ref().unwrap();
        {
            let guard = authority.acquire_shared_guard().unwrap();
            let stage = authority
                .require_recovery()
                .unwrap()
                .open_durable_cas_upload_admitted(&guard, &original_row.2, &bootstrap)
                .unwrap();
            stage
                .ensure_publication_contract(
                    &DurableCasPublicationKey::qualification_fixture_occurrence(
                        &binding.digest().unwrap(),
                        completion.digest(),
                    )
                    .unwrap(),
                    None,
                )
                .unwrap();
            let retained = stage.qualification_fixture_receipt().unwrap();
            assert_eq!(retained.metadata_hash, reference.metadata_hash);
            assert_eq!(retained.blob_hash, reference.blob_hash);
        }
        // Crash prefix: the capture and nonexpiring roots exist, but Complete is
        // still claimed. Signing must wait; recovery finishes without recapture.
        assert!(
            live.ensure_qualification_scope_settled_frame(
                &authority,
                &program,
                &supervisor,
                &completion,
            )
            .is_err()
        );
        drop(performed);
        assert_eq!(
            live.record_qualification_scope_settlement(
                &authority,
                &program,
                &completion,
                &occurrence,
                NativeNamespaceExit::Code(0),
                capture,
            )
            .unwrap(),
            settlement
        );
        assert_eq!(row(), original_row);
        let mut changed_program = program.clone();
        changed_program.fixture.as_mut().unwrap().leaf = "different.txt".into();
        changed_program.validate().unwrap();
        assert!(
            live.qualification_scope_settlement_for_completion(
                &authority,
                &changed_program,
                &completion,
            )
            .is_err()
        );
        let changed_bytes = b"AFTER\n";
        assert!(
            live.record_qualification_scope_settlement(
                &authority,
                &program,
                &completion,
                &occurrence,
                NativeNamespaceExit::Code(0),
                Some((
                    changed_bytes.as_slice(),
                    &lillux::sha256_hex(changed_bytes),
                    ProjectFile::REGULAR_MODE
                )),
            )
            .is_err()
        );
        let (changed_wire, _) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            2,
            Some(release_digest),
            eof.frame().sequence,
            ExecutionChannelPayload::QualificationComplete {
                completion_request_digest: "2".repeat(64),
            },
        );
        let changed_completion =
            SignedExecutionFrame::decode_and_verify(&changed_wire, &binding, binding.issued_at_ms)
                .unwrap();
        assert!(
            live.record_qualification_scope_settlement(
                &authority,
                &program,
                &changed_completion,
                &occurrence,
                NativeNamespaceExit::Code(0),
                capture,
            )
            .is_err()
        );
        assert_eq!(row(), original_row);
        live.reconcile_qualification_scope_settlement_application(
            &authority,
            &program,
            &completion,
        )
        .unwrap();
        assert_eq!(effects.get(), 1);
        assert!(matches!(
            live.record_and_claim(&completion_wire).unwrap(),
            GuestApplicationClaim::AlreadyApplied
        ));
        let signed = live
            .ensure_qualification_scope_settled_frame(
                &authority,
                &program,
                &supervisor,
                &completion,
            )
            .unwrap()
            .unwrap();
        assert!(matches!(&signed.frame().payload,
            ExecutionChannelPayload::QualificationScopeSettled { observation } if observation == &settlement));
        assert!(
            live.ensure_qualification_scope_settled_frame(
                &authority,
                &program,
                &supervisor,
                &completion,
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(row(), original_row);
        retire(&host, live.close());
        let reopen = || {
            recover_checked(
                &host,
                root.path(),
                &authority,
                &store_identity,
                &bootstrap,
                &binding,
            )
        };
        reopen().unwrap();
        let cas = authority.cas_store().unwrap();
        for path in [
            lillux::cas::shard_path(cas.root(), "objects", &reference.metadata_hash, ".json"),
            lillux::cas::shard_path(cas.root(), "blobs", &reference.blob_hash, ""),
        ] {
            let original = std::fs::read(&path).unwrap();
            let absent = path.with_extension("saved-for-custody-test");
            std::fs::rename(&path, &absent).unwrap();
            assert!(
                reopen().is_err(),
                "cold reopen must require every retained CAS root"
            );
            std::fs::rename(&absent, &path).unwrap();
            reopen().unwrap();
            let mut changed = original.clone();
            let byte = changed.iter_mut().find(|byte| **byte == b'a').unwrap();
            *byte = b'b';
            std::fs::write(&path, &changed).unwrap();
            assert!(
                reopen().is_err(),
                "same-size CAS substitution must be refused"
            );
            std::fs::write(&path, &original).unwrap();
            reopen().unwrap();
        }
    }

    #[test]
    fn applied_runtime_requires_released_exact_occurrence_and_reopens_immutably() {
        let host = shared_test_host();
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, owner, supervisor, bootstrap, store_identity) =
            live_store(&host, &root, &authority);
        let occurrence = live
            .0
            .conn
            .with_sql(|conn| retained_launcher_occurrence_digest(conn, &binding.digest().unwrap()))
            .unwrap();
        let receipt = lillux::LinuxSandboxAppliedLaunchReceipt {
            owned_child_pid: 101,
            namespace_pid: 1,
            effective_uid: 1,
            effective_gid: 1,
            no_new_privs: true,
            seccomp_mode: 2,
            executable_sha256: [1; 32],
            argv_sha256: [2; 32],
            environment_sha256: [3; 32],
            cwd_sha256: [4; 32],
            post_release_mount_view: lillux::LinuxSandboxMountPreparationCommitments {
                schema: 1,
                mount_count: 1,
                destination_access_sha256: [5; 32],
            },
        };
        assert!(!live.has_applied_release().unwrap());
        assert!(live.record_applied_runtime(&occurrence, &receipt).is_err());
        assert!(live.ensure_runtime_applied_frame(&supervisor).is_err());
        let (release, _) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        let GuestApplicationClaim::New(token) = live.record_and_claim(&release).unwrap() else {
            panic!("release must have one exact application")
        };
        let (_, performed) = live.apply_once(token, |_| Ok(())).unwrap();
        live.finish(performed).unwrap();
        assert!(live.has_applied_release().unwrap());
        assert!(
            live.record_applied_runtime(&"a".repeat(64), &receipt)
                .is_err()
        );
        assert!(live.record_applied_runtime(&occurrence, &receipt).unwrap());
        assert!(!live.record_applied_runtime(&occurrence, &receipt).unwrap());
        let mut changed = receipt.clone();
        changed.environment_sha256[0] ^= 1;
        assert!(live.record_applied_runtime(&occurrence, &changed).is_err());
        let frame = live
            .ensure_runtime_applied_frame(&supervisor)
            .unwrap()
            .expect("first projection must author one frame");
        assert!(matches!(
            &frame.frame().payload,
            ExecutionChannelPayload::RuntimeApplied {
                launcher_occurrence_digest,
                candidate_program_digest,
                receipt: observed,
            } if launcher_occurrence_digest == &occurrence
                && candidate_program_digest == &binding.candidate_program_digest
                && observed == &receipt
        ));
        assert!(
            live.author_supervisor_frame(&supervisor, frame.frame().payload.clone())
                .is_err()
        );
        let GuestApplicationClaim::New(token) = live
            .claim(
                ChannelDirection::SupervisorToOwner,
                frame.frame().sequence,
                frame.digest(),
            )
            .unwrap()
        else {
            panic!("runtime-applied projection must be claimable once")
        };
        let (_, performed) = live.apply_once(token, |_| Ok(())).unwrap();
        live.finish(performed).unwrap();
        assert!(
            live.ensure_runtime_applied_frame(&supervisor)
                .unwrap()
                .is_none()
        );
        for statement in [
            "UPDATE external_guest_runtime_applied SET receipt_digest='other'",
            "DELETE FROM external_guest_runtime_applied",
        ] {
            assert!(
                live.0
                    .conn
                    .with_sql(|conn| Ok(conn.execute(statement, []).is_err()))
                    .unwrap()
            );
        }
        live.validate().unwrap();
        retire(&host, live.close());
        let recovered = recover(
            &host,
            lillux::PinnedDirectory::open(root.path()).unwrap().unwrap(),
            &authority,
            &store_identity,
            &bootstrap,
            &binding,
        )
        .unwrap();
        recovered.validate().unwrap();
        retire(&host, recovered.close());
    }

    #[test]
    fn fresh_intent_is_single_owner_and_reopen_is_recovery_only() {
        let host = shared_test_host();
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, _owner, _supervisor, bootstrap, store_identity) =
            live_store(&host, &root, &authority);
        let competing = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        refusal(
            &host,
            recover(
                &host,
                competing,
                &authority,
                &store_identity,
                &bootstrap,
                &binding,
            ),
        );
        live.validate().unwrap();
        retire(&host, live.close());
        let recovered = recover(
            &host,
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
        retire(&host, recovered.close());
        let (_other_state_root, other_authority) = state_authority();
        refusal(
            &host,
            recover(
                &host,
                lillux::PinnedDirectory::open(root.path()).unwrap().unwrap(),
                &other_authority,
                &store_identity,
                &bootstrap,
                &binding,
            ),
        );
        refusal(
            &host,
            recover(
                &host,
                lillux::PinnedDirectory::open(root.path()).unwrap().unwrap(),
                &authority,
                &store_identity,
                &"8".repeat(64),
                &binding,
            ),
        );
    }

    #[test]
    fn outer_owner_can_anchor_exact_guest_inode_before_initialization() {
        let host = shared_test_host();
        let root = tempfile::tempdir().unwrap();
        let directory = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        directory.tighten_owner_private_directory().unwrap();
        let (_state_root, authority) = state_authority();
        let (binding, _owner, _supervisor) = binding();
        let bootstrap = "9".repeat(64);
        let reserved = PreparedGuestJournal::reserve(
            directory,
            &authority,
            &bootstrap,
            binding.clone(),
            host.factory(),
            host.destination(),
        )
        .unwrap();
        let outer_anchor = reserved.store_identity().clone();
        outer_anchor.digest().unwrap();
        assert_eq!(reserved.bootstrap_digest(), bootstrap);
        assert_eq!(reserved.binding(), &binding);
        let prepared = reserved.initialize(&census(&host)).unwrap();
        assert_eq!(prepared.store_identity(), &outer_anchor);
        prepared.0.validate().unwrap();
        retire(&host, prepared.close());
    }

    #[test]
    fn abandoned_guest_reservation_cannot_be_reinitialized_or_reopened_live() {
        let host = shared_test_host();
        let root = tempfile::tempdir().unwrap();
        let directory = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        directory.tighten_owner_private_directory().unwrap();
        let (_state_root, authority) = state_authority();
        let (binding, _owner, _supervisor) = binding();
        let bootstrap = "9".repeat(64);
        let reserved = PreparedGuestJournal::reserve(
            directory,
            &authority,
            &bootstrap,
            binding.clone(),
            host.factory(),
            host.destination(),
        )
        .unwrap();
        let outer_anchor = reserved.store_identity().clone();
        drop(reserved);
        let reopened = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        refusal(
            &host,
            PreparedGuestJournal::create(
                reopened.try_clone().unwrap(),
                &authority,
                &bootstrap,
                binding.clone(),
                host.factory(),
                host.destination(),
                &census(&host),
            ),
        );
        refusal(
            &host,
            recover(
                &host,
                reopened,
                &authority,
                &outer_anchor,
                &bootstrap,
                &binding,
            ),
        );
    }

    #[test]
    fn guest_reservation_refuses_mutation_and_competing_owner_before_initialization() {
        let host = shared_test_host();
        enum Mutation {
            ReplaceInode,
            WriteBytes,
            AmbientEntry,
            CompetingOwner,
        }

        let (_state_root, authority) = state_authority();
        for mutation in [
            Mutation::ReplaceInode,
            Mutation::WriteBytes,
            Mutation::AmbientEntry,
            Mutation::CompetingOwner,
        ] {
            let root = tempfile::tempdir().unwrap();
            let directory = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
            directory.tighten_owner_private_directory().unwrap();
            let (binding, _owner, _supervisor) = binding();
            let bootstrap = "9".repeat(64);
            let reserved = PreparedGuestJournal::reserve(
                directory,
                &authority,
                &bootstrap,
                binding.clone(),
                host.factory(),
                host.destination(),
            )
            .unwrap();

            match mutation {
                Mutation::ReplaceInode => {
                    let replacement = root.path().join("replacement");
                    std::fs::File::create(&replacement)
                        .unwrap()
                        .sync_all()
                        .unwrap();
                    std::fs::rename(replacement, root.path().join(DATABASE_NAME)).unwrap();
                    refusal(&host, reserved.initialize(&census(&host)));
                }
                Mutation::WriteBytes => {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(root.path().join(DATABASE_NAME))
                        .unwrap()
                        .write_all(b"not empty")
                        .unwrap();
                    refusal(&host, reserved.initialize(&census(&host)));
                }
                Mutation::AmbientEntry => {
                    std::fs::File::create(root.path().join("ambient")).unwrap();
                    refusal(&host, reserved.initialize(&census(&host)));
                }
                Mutation::CompetingOwner => {
                    let competing = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
                    let error = match PreparedGuestJournal::reserve(
                        competing,
                        &authority,
                        &bootstrap,
                        binding,
                        host.factory(),
                        host.destination(),
                    ) {
                        Ok(_) => panic!("competing guest owner acquired reservation"),
                        Err(error) => format!("{error:#}"),
                    };
                    assert!(
                        error.contains("external guest directory already has a live owner"),
                        "competing reservation failed at the wrong boundary: {error}"
                    );
                    retire(&host, reserved.initialize(&census(&host)).unwrap().close());
                }
            }
        }
    }

    #[test]
    fn sticky_revocation_wins_before_the_native_side_effect_gate() {
        let host = shared_test_host();
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, owner, _supervisor, _bootstrap, _store_identity) =
            live_store(&host, &root, &authority);
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
        retire(&host, live.close());
    }

    #[test]
    fn writer_gate_linearizes_a_second_connection_revocation() {
        let host = shared_test_host();
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, owner, _supervisor, _bootstrap, _store_identity) =
            live_store(&host, &root, &authority);
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
        // A second connection on its own native ticket, opened before the
        // gate so its open takes no ticket inside the gated SQL scope.
        let second = match host.open_bound(
            journal_root(root.path()),
            OsStr::new(DATABASE_NAME),
            OriginalNativeBoundMode::Existing,
            OriginalNativeDomain::ExternalGuestJournal,
        ) {
            Ok(second) => second,
            Err(failure) => {
                let error = failure.to_string();
                host.retire_open_failure(failure);
                panic!("second guest connection refused: {error}")
            }
        };
        let (_, release_performed) = live
            .apply_once(release_token, |_| {
                second.with_sql(|conn| {
                    let error = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
                        .unwrap_err();
                    assert!(
                        matches!(
                            error.sqlite_error_code(),
                            Some(rusqlite::ErrorCode::DatabaseBusy)
                        ),
                        "revocation must deterministically observe the native-action writer gate"
                    );
                    Ok(())
                })
            })
            .unwrap();
        // The journal's own connection is a live peer on the same inode, so
        // the second connection's descriptors stay retained until that peer
        // closes: its close is pending, and is retried after the journal's.
        let second_pending = match second.close() {
            Ok(_) => panic!("a second connection closed while its peer still held the inode"),
            Err(pending) => pending,
        };
        live.finish(release_performed).unwrap();
        assert!(live.record_frame(&cancel).unwrap());
        assert!(matches!(
            live.claim(ChannelDirection::OwnerToSupervisor, 2, &cancel_digest)
                .unwrap(),
            GuestApplicationClaim::New(_)
        ));
        live.validate().unwrap();
        retire(&host, live.close());
        match second_pending.retry() {
            Ok(closed) => host.retain_terminal(OriginalNativeConnectionTerminal::Closed(closed)),
            Err(failure) => panic!("second guest connection close retained: {failure}"),
        }
    }

    #[test]
    fn supervisor_authoring_uses_retained_frontiers_and_exact_role_key() {
        let host = shared_test_host();
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, owner, supervisor, _bootstrap, _store_identity) =
            live_store(&host, &root, &authority);
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
        retire(&host, live.close());
    }

    #[test]
    fn supervisor_batch_budget_failure_retains_no_partial_prefix() {
        let host = shared_test_host();
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, owner, supervisor, _bootstrap, _store_identity) =
            live_store(&host, &root, &authority);
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
        let payloads = (0..binding.max_frames).map(|_| ExecutionChannelPayload::ProtocolBytes {
            bytes_base64: STANDARD.encode(b"bounded-export-frame"),
        });
        assert!(
            live.author_supervisor_frames(&supervisor, payloads)
                .is_err(),
            "ready plus a full frame-budget batch must exceed the bound"
        );

        let after = live
            .author_supervisor_frame(
                &supervisor,
                ExecutionChannelPayload::ProtocolBytes {
                    bytes_base64: STANDARD.encode(b"after-rollback"),
                },
            )
            .unwrap();
        let (_, ready_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            1,
            None,
            0,
            ExecutionChannelPayload::Ready {
                supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                base_snapshot_hash: binding.base_snapshot_hash.clone(),
                boot: None,
            },
        );
        assert_eq!(after.frame().sequence, 2);
        assert_eq!(
            after.frame().previous_frame_digest.as_deref(),
            Some(ready_digest.as_str())
        );
        live.validate().unwrap();
        retire(&host, live.close());
    }

    #[test]
    fn native_capture_is_rooted_and_reopen_validates_exact_quiesce_coordinates() {
        let host = shared_test_host();
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
        let prepared = PreparedGuestJournal::create(
            directory,
            &authority,
            &bootstrap,
            binding.clone(),
            host.factory(),
            host.destination(),
            &census(&host),
        )
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
                    boot: None,
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
            None,
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
            output_capture_hash: None,
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
        retire(&host, live.close());
        recover_checked(
            &host,
            root.path(),
            &authority,
            &store_identity,
            &bootstrap,
            &binding,
        )
        .unwrap();
    }

    #[test]
    fn recovery_refuses_schema_drift_without_repair() {
        let host = shared_test_host();
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, _owner, _supervisor, bootstrap, store_identity) =
            live_store(&host, &root, &authority);
        retire(&host, live.close());
        host.with_existing(
            journal_root(root.path()),
            OsStr::new(DATABASE_NAME),
            OriginalNativeDomain::ExternalGuestJournal,
            |conn| {
                conn.execute_batch("CREATE TABLE ambient_drift(value TEXT);")?;
                Ok(())
            },
        );
        refusal(
            &host,
            recover(
                &host,
                lillux::PinnedDirectory::open(root.path()).unwrap().unwrap(),
                &authority,
                &store_identity,
                &bootstrap,
                &binding,
            ),
        );
    }

    #[test]
    fn recovery_refuses_a_byte_copy_replacement_database_inode() {
        let host = shared_test_host();
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, _owner, _supervisor, bootstrap, store_identity) =
            live_store(&host, &root, &authority);
        retire(&host, live.close());
        let database = root.path().join(DATABASE_NAME);
        let replacement = root.path().join("replacement.sqlite3");
        std::fs::copy(&database, &replacement).unwrap();
        std::fs::rename(&replacement, &database).unwrap();
        let error = refusal(
            &host,
            recover(
                &host,
                lillux::PinnedDirectory::open(root.path()).unwrap().unwrap(),
                &authority,
                &store_identity,
                &bootstrap,
                &binding,
            ),
        );
        assert!(
            error.contains("outer inode identity"),
            "replacement database was not diagnosed: {error}"
        );
    }

    #[test]
    fn live_database_replacement_between_claim_and_effect_is_refused() {
        const TEST: &str = concat!(
            module_path!(),
            "::live_database_replacement_between_claim_and_effect_is_refused"
        );
        // Replacing a live database poisons its native keeper, and no close
        // settles a poisoned keeper. The scenario therefore runs in its own
        // child process and ends with it; this process checks what it saw.
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert!(
                outcome.contains("bound database was replaced"),
                "live replacement was not diagnosed: {outcome}"
            );
            assert!(
                outcome.contains("external journal native close retained"),
                "the poisoned journal's close was not refused: {outcome}"
            );
            return;
        }
        let host = shared_test_host();
        let root = tempfile::tempdir().unwrap();
        let (state_root, authority) = state_authority();
        let (live, binding, owner, _supervisor, _bootstrap, _store_identity) =
            live_store(&host, &root, &authority);
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
        let Err(error) = live.apply_once(token, |_| Ok(())) else {
            panic!("an effect on a replaced live database was admitted")
        };
        let refusal = format!("{error:#}");
        // Every ticket but the poisoned journal's reached the census.
        let (issued, recorded) = host.ticket_census();
        assert_eq!(
            issued as usize,
            recorded + 1,
            "only the poisoned journal may remain outside the census"
        );
        // The poisoned keeper refuses its close, and the journal keeps it.
        let (live, close) = match live.close() {
            Ok(terminal) => {
                host.retain_terminal(terminal);
                panic!("a poisoned journal keeper closed");
            }
            Err(retained) => retained,
        };
        lillux_sqlite::test_host::end_isolated_process(
            (live, authority, state_root, root, host),
            &format!("refusal: {refusal}; close: {close:#}"),
        )
    }

    #[test]
    fn partial_protocol_write_is_overtaken_by_sticky_cancellation() {
        let host = shared_test_host();
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, owner, _supervisor, _bootstrap, _store_identity) =
            live_store(&host, &root, &authority);
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
        retire(&host, live.close());
    }

    #[test]
    fn revocation_commit_survives_a_transcript_gap() {
        let host = shared_test_host();
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, owner, supervisor, bootstrap, store_identity) =
            live_store(&host, &root, &authority);
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
        retire(&host, live.close());
        let recovered = recover(
            &host,
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
        retire(&host, recovered.close());
    }

    #[test]
    fn terminal_stop_is_durably_one_shot() {
        let host = shared_test_host();
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, owner, _supervisor, _bootstrap, _store_identity) =
            live_store(&host, &root, &authority);
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
        retire(&host, live.close());
    }

    #[test]
    fn failed_terminal_stop_remains_claimed_unknown() {
        let host = shared_test_host();
        let root = tempfile::tempdir().unwrap();
        let (_state_root, authority) = state_authority();
        let (live, binding, owner, _supervisor, _bootstrap, _store_identity) =
            live_store(&host, &root, &authority);
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
        retire(&host, live.close());
    }
}
