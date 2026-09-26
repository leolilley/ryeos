//! One-way, exact guest base-installation intent beneath an external Worker.
//!
//! The record is written before the mutable base copy. Recovery only checks
//! the exact retained intent and import; it cannot repeat installation or
//! grant supervisor launch. The eventual guest owner must separately journal
//! launch, enforce input writer exclusion and settle the enclosing scope.

use std::ffi::OsStr;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::ExternalGuestInputProjection;
use ryeos_external_execution_contract::staging_package::{GuestImportContext, GuestImportTicket};
use serde::{Deserialize, Serialize};

use crate::guest_staging::{
    GuestStageIdentity, TicketedGuestImport, recover_ticketed_guest_import,
    stage_ticketed_uploaded_guest_package,
};

const OWNER_DIRECTORY: &str = "guest-import-owner";
const OWNER_RECORD_NAME: &str = "occurrence-owner.json";
const CANDIDATE_RUNTIME_DIRECTORY: &str = "candidate-runtime";
const RECORD_NAME: &str = "guest-base-install-intent.json";
const STAGE_MARKER_NAME: &str = "guest-base-install-owner.json";
const MAX_RECORD_BYTES: u64 = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct GuestBaseInstallIntentIdentity {
    schema: u32,
    journal_directory: lillux::PinnedDirectoryIdentity,
    record_file: lillux::PinnedRegularFileIdentity,
    record_sha256: String,
    stage_marker_file: lillux::PinnedRegularFileIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestBaseInstallMarker {
    schema: u32,
    journal_directory: lillux::PinnedDirectoryIdentity,
    record_file: lillux::PinnedRegularFileIdentity,
    record_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestOccurrenceOwnerRecord {
    schema: u32,
    ticket_sha256: String,
    occurrence_id: String,
    occurrence_directory: lillux::PinnedDirectoryIdentity,
    owner_directory: lillux::PinnedDirectoryIdentity,
}

/// Fixed, create-only owner below the independently selected occurrence root.
/// It is created before an uploaded byte can be imported. A failed stage or
/// process crash leaves this occurrence fenced against a second import.
pub struct GuestOccurrenceOwner {
    occurrence: lillux::PinnedDirectory,
    root: lillux::PinnedDirectory,
    _lock: lillux::PinnedDirectoryLock,
    ticket: GuestImportTicket,
    record: GuestOccurrenceOwnerRecord,
}

pub struct StagedGuestOccurrence {
    owner: GuestOccurrenceOwner,
    imported: TicketedGuestImport,
}

/// Retains both the exact owner and stage after base installation. This is
/// not supervisor adoption, writer exclusion, or a Ready claim.
pub struct InstalledGuestBase {
    _owner: GuestOccurrenceOwner,
    _imported: TicketedGuestImport,
    _runtime: lillux::PinnedDirectory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestOccurrenceRecoveryPhase {
    /// An owner was committed before import, but no install intent exists.
    /// An upload or stage may have happened; neither can be replayed here.
    ImportUncertain,
    /// Installation intent is committed. The base copy may be absent,
    /// partial, or complete; this observation does not grant another copy.
    InstallationUncertain,
}

pub struct RecoveredGuestOccurrence {
    _root: lillux::PinnedDirectory,
    _lock: lillux::PinnedDirectoryLock,
    _imported: Option<TicketedGuestImport>,
    _runtime: Option<lillux::PinnedDirectory>,
    phase: GuestOccurrenceRecoveryPhase,
}

impl RecoveredGuestOccurrence {
    pub fn phase(&self) -> &GuestOccurrenceRecoveryPhase {
        &self.phase
    }
}

/// Point-read the fixed owner child beneath the retained occurrence. Neither
/// phase exposes import, installation, supervisor launch, or Ready authority.
/// In particular, a crash before the owner record or stage intent is complete
/// remains quarantined rather than guessed from directory contents.
pub fn recover_guest_occurrence(
    occurrence: &lillux::PinnedDirectory,
    ticket: &GuestImportTicket,
    context: &GuestImportContext<'_>,
    inputs: &ExternalGuestInputProjection,
) -> Result<RecoveredGuestOccurrence> {
    occurrence.require_owner_private_directory()?;
    ticket.staging_expected(context, inputs)?;
    let root = occurrence
        .open_child_directory(OsStr::new(OWNER_DIRECTORY))?
        .context("guest occurrence owner is absent")?;
    root.require_owner_private_directory()?;
    let lock = root
        .try_lock_exclusive()?
        .context("guest occurrence owner remains live")?;
    lock.ensure_protects(&root)?;
    let owner_file = root
        .open_pinned_regular(OsStr::new(OWNER_RECORD_NAME), false)?
        .context("guest occurrence owner record is absent")?;
    ensure!(
        owner_file.permission_mode()? == 0o600,
        "guest occurrence owner record mode changed"
    );
    let owner_observation = owner_file.observation()?;
    ensure!(
        owner_observation.size() <= MAX_RECORD_BYTES,
        "guest occurrence owner record exceeds bound"
    );
    let owner_bytes = owner_file.read_stable_bounded(&owner_observation, MAX_RECORD_BYTES)?;
    let owner: GuestOccurrenceOwnerRecord = serde_json::from_slice(&owner_bytes)?;
    ensure!(
        canonical_owner_record(&owner)? == owner_bytes
            && owner.schema == 1
            && owner.ticket_sha256 == digest_ticket(ticket)?
            && owner.occurrence_id == context.occurrence_id
            && owner.occurrence_directory == occurrence.identity()?
            && owner.owner_directory == root.identity()?,
        "guest occurrence owner differs from retained placement"
    );
    let Some(record) = root.open_pinned_regular(OsStr::new(RECORD_NAME), false)? else {
        return Ok(RecoveredGuestOccurrence {
            _root: root,
            _lock: lock,
            _imported: None,
            _runtime: None,
            phase: GuestOccurrenceRecoveryPhase::ImportUncertain,
        });
    };
    ensure!(
        record.permission_mode()? == 0o600,
        "guest installation intent mode changed"
    );
    let observed = record.observation()?;
    ensure!(
        observed.size() <= MAX_RECORD_BYTES,
        "guest installation intent exceeds bound"
    );
    let bytes = record.read_stable_bounded(&observed, MAX_RECORD_BYTES)?;
    let intent: GuestBaseInstallIntent = serde_json::from_slice(&bytes)?;
    ensure!(
        canonical_record(&intent)? == bytes
            && intent.schema == 1
            && intent.ticket_sha256 == owner.ticket_sha256
            && intent.journal_directory == root.identity()?,
        "guest installation intent differs from occurrence owner"
    );
    let runtime = occurrence
        .open_child_directory(OsStr::new(CANDIDATE_RUNTIME_DIRECTORY))?
        .context("committed guest installation has no retained runtime")?;
    runtime.require_owner_private_directory()?;
    root.require_disjoint_directory_tree(&runtime)?;
    let imported = recover_ticketed_guest_import(&root, &intent.stage, ticket, context, inputs)?;
    runtime.require_disjoint_directory_tree(imported.root())?;
    let marker = imported
        .root()
        .open_pinned_regular(OsStr::new(STAGE_MARKER_NAME), false)?
        .context("guest stage installation marker is absent")?;
    let identity = GuestBaseInstallIntentIdentity {
        schema: 1,
        journal_directory: root.identity()?,
        record_file: lillux::pinned_regular_file_identity(&record.try_clone_descriptor()?)?,
        record_sha256: lillux::sha256_hex(&bytes),
        stage_marker_file: lillux::pinned_regular_file_identity(&marker.try_clone_descriptor()?)?,
    };
    verify_committed_intent(&imported, context, inputs, &root, &runtime, &identity)?;
    Ok(RecoveredGuestOccurrence {
        _root: root,
        _lock: lock,
        _imported: Some(imported),
        _runtime: Some(runtime),
        phase: GuestOccurrenceRecoveryPhase::InstallationUncertain,
    })
}

impl GuestOccurrenceOwner {
    /// Reserve the exact occurrence before importing an uploaded package.
    /// An incumbent child, even an incomplete one, is never adopted here.
    pub fn begin(
        occurrence: &lillux::PinnedDirectory,
        ticket: &GuestImportTicket,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
    ) -> Result<Self> {
        occurrence.require_owner_private_directory()?;
        ticket.staging_expected(context, inputs)?;
        let root = occurrence.create_child(OsStr::new(OWNER_DIRECTORY), 0o700)?;
        let lock = root
            .try_lock_exclusive()?
            .context("new guest occurrence owner is already locked")?;
        lock.ensure_protects(&root)?;
        let record = GuestOccurrenceOwnerRecord {
            schema: 1,
            ticket_sha256: digest_ticket(ticket)?,
            occurrence_id: context.occurrence_id.to_owned(),
            occurrence_directory: occurrence.identity()?,
            owner_directory: root.identity()?,
        };
        let bytes = canonical_owner_record(&record)?;
        root.atomic_create_regular(OsStr::new(OWNER_RECORD_NAME), &bytes, 0o600)?
            .context("guest occurrence owner record already exists")?;
        Ok(Self {
            occurrence: occurrence.try_clone()?,
            root,
            _lock: lock,
            ticket: ticket.clone(),
            record,
        })
    }

    fn recheck(
        &self,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
    ) -> Result<()> {
        self._lock.ensure_protects(&self.root)?;
        self.ticket.staging_expected(context, inputs)?;
        ensure!(
            self.record.schema == 1
                && self.record.ticket_sha256 == digest_ticket(&self.ticket)?
                && self.record.occurrence_id == context.occurrence_id
                && self.record.occurrence_directory == self.occurrence.identity()?
                && self.record.owner_directory == self.root.identity()?,
            "guest occurrence owner differs from retained authority"
        );
        ensure!(
            self.occurrence
                .open_child_directory(OsStr::new(OWNER_DIRECTORY))?
                .context("guest occurrence owner disappeared")?
                .identity()?
                == self.root.identity()?,
            "guest occurrence owner namespace changed inode"
        );
        let record = self
            .root
            .open_pinned_regular(OsStr::new(OWNER_RECORD_NAME), false)?
            .context("guest occurrence owner record is absent")?;
        ensure!(
            record.permission_mode()? == 0o600,
            "guest occurrence owner record mode changed"
        );
        let observed = record.observation()?;
        ensure!(
            observed.size() <= MAX_RECORD_BYTES,
            "guest occurrence owner record exceeds bound"
        );
        ensure!(
            record.read_stable_bounded(&observed, MAX_RECORD_BYTES)?
                == canonical_owner_record(&self.record)?,
            "guest occurrence owner record changed bytes"
        );
        Ok(())
    }

    /// Consumes the pre-upload owner. Failure leaves the create-only owner
    /// record in place and exposes no retry method.
    pub fn stage_uploaded_once(
        self,
        upload: &lillux::PinnedRegularFile,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<StagedGuestOccurrence> {
        self.recheck(context, inputs)?;
        ensure!(
            self.root.entry_names()? == vec![std::ffi::OsString::from(OWNER_RECORD_NAME)],
            "guest occurrence already has staged or ambient content"
        );
        let imported = stage_ticketed_uploaded_guest_package(
            upload,
            &self.root,
            &self.ticket,
            context,
            inputs,
            deadline,
        )?;
        Ok(StagedGuestOccurrence {
            owner: self,
            imported,
        })
    }
}

impl StagedGuestOccurrence {
    /// The owner journal already fences this occurrence against re-staging.
    /// Commit exact stage/runtime intent, then copy the verified base once.
    pub fn install_base_once(
        self,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
    ) -> Result<InstalledGuestBase> {
        self.owner.recheck(context, inputs)?;
        let runtime = self
            .owner
            .occurrence
            .create_child(OsStr::new(CANDIDATE_RUNTIME_DIRECTORY), 0o700)?;
        runtime.require_owner_private_directory()?;
        self.owner.root.require_disjoint_directory_tree(&runtime)?;
        let prepared = prepare_guest_base_install_locked(
            &self.imported,
            context,
            inputs,
            self.owner.root.try_clone()?,
            &runtime,
            self.owner._lock.clone(),
        )?;
        prepared.install_once()?;
        Ok(InstalledGuestBase {
            _owner: self.owner,
            _imported: self.imported,
            _runtime: runtime,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestBaseInstallIntent {
    schema: u32,
    ticket_sha256: String,
    stage: GuestStageIdentity,
    journal_directory: lillux::PinnedDirectoryIdentity,
    candidate_runtime: lillux::PinnedDirectoryIdentity,
}

/// The only local path that may perform the copy after create-only intent.
/// Dropping this value does not erase the intent or grant a retry.
struct PreparedGuestBaseInstall<'a> {
    imported: &'a TicketedGuestImport,
    context: &'a GuestImportContext<'a>,
    inputs: &'a ExternalGuestInputProjection,
    runtime: &'a lillux::PinnedDirectory,
    journal: lillux::PinnedDirectory,
    _lock: lillux::PinnedDirectoryLock,
    identity: GuestBaseInstallIntentIdentity,
}

impl PreparedGuestBaseInstall<'_> {
    /// Consuming the prepared value is process-local one-shot. The durable
    /// create-only intent prevents another prepare/recovery from rerunning it.
    pub fn install_once(self) -> Result<()> {
        self._lock.ensure_protects(&self.journal)?;
        verify_committed_intent(
            self.imported,
            self.context,
            self.inputs,
            &self.journal,
            self.runtime,
            &self.identity,
        )?;
        self.imported
            .install_base_into(self.context, self.inputs, self.runtime)
    }
}

fn prepare_guest_base_install_locked<'a>(
    imported: &'a TicketedGuestImport,
    context: &'a GuestImportContext<'a>,
    inputs: &'a ExternalGuestInputProjection,
    journal: lillux::PinnedDirectory,
    runtime: &'a lillux::PinnedDirectory,
    lock: lillux::PinnedDirectoryLock,
) -> Result<PreparedGuestBaseInstall<'a>> {
    lock.ensure_protects(&journal)?;
    ensure!(
        journal
            .open_pinned_regular(OsStr::new(RECORD_NAME), false)?
            .is_none(),
        "guest installation intent already exists"
    );
    imported.recheck_for_adoption(context, inputs)?;
    let intent = GuestBaseInstallIntent {
        schema: 1,
        ticket_sha256: ticket_digest(imported)?,
        stage: imported.stage_identity()?,
        journal_directory: journal.identity()?,
        candidate_runtime: runtime.identity()?,
    };
    let bytes = canonical_record(&intent)?;
    let record = journal
        .atomic_create_regular(OsStr::new(RECORD_NAME), &bytes, 0o600)?
        .context("guest installation intent already exists")?;
    let marker = GuestBaseInstallMarker {
        schema: 1,
        journal_directory: intent.journal_directory,
        record_file: lillux::pinned_regular_file_identity(&record)?,
        record_sha256: lillux::sha256_hex(&bytes),
    };
    let marker_bytes = canonical_marker(&marker)?;
    let marker_file = imported
        .root()
        .atomic_create_regular(OsStr::new(STAGE_MARKER_NAME), &marker_bytes, 0o600)?
        .context("ticketed guest stage already has an installation owner")?;
    let identity = GuestBaseInstallIntentIdentity {
        schema: 1,
        journal_directory: marker.journal_directory,
        record_file: marker.record_file,
        record_sha256: marker.record_sha256,
        stage_marker_file: lillux::pinned_regular_file_identity(&marker_file)?,
    };
    Ok(PreparedGuestBaseInstall {
        imported,
        context,
        inputs,
        runtime,
        journal,
        _lock: lock,
        identity,
    })
}

fn verify_committed_intent(
    imported: &TicketedGuestImport,
    context: &GuestImportContext<'_>,
    inputs: &ExternalGuestInputProjection,
    journal: &lillux::PinnedDirectory,
    runtime: &lillux::PinnedDirectory,
    identity: &GuestBaseInstallIntentIdentity,
) -> Result<GuestBaseInstallIntent> {
    let marker = imported
        .root()
        .open_pinned_regular(OsStr::new(STAGE_MARKER_NAME), false)?
        .context("guest stage installation owner marker is absent")?;
    ensure!(
        lillux::pinned_regular_file_identity(&marker.try_clone_descriptor()?)?
            == identity.stage_marker_file
            && marker.permission_mode()? == 0o600,
        "guest stage installation owner marker inode or mode changed"
    );
    let marker_observation = marker.observation()?;
    ensure!(
        marker_observation.size() <= MAX_RECORD_BYTES,
        "guest stage installation owner marker exceeds bound"
    );
    let marker_bytes = marker.read_stable_bounded(&marker_observation, MAX_RECORD_BYTES)?;
    let expected_marker = GuestBaseInstallMarker {
        schema: 1,
        journal_directory: identity.journal_directory,
        record_file: identity.record_file,
        record_sha256: identity.record_sha256.clone(),
    };
    ensure!(
        marker_bytes == canonical_marker(&expected_marker)?,
        "guest stage installation owner marker differs from retained intent"
    );
    let record = journal
        .open_pinned_regular(OsStr::new(RECORD_NAME), false)?
        .context("guest installation intent is absent")?;
    ensure!(
        lillux::pinned_regular_file_identity(&record.try_clone_descriptor()?)?
            == identity.record_file
            && record.permission_mode()? == 0o600,
        "guest installation intent inode or mode changed"
    );
    let observed = record.observation()?;
    ensure!(
        observed.size() <= MAX_RECORD_BYTES,
        "guest installation intent exceeds bound"
    );
    let bytes = record.read_stable_bounded(&observed, MAX_RECORD_BYTES)?;
    ensure!(
        lillux::sha256_hex(&bytes) == identity.record_sha256,
        "guest installation intent changed bytes"
    );
    let intent: GuestBaseInstallIntent = serde_json::from_slice(&bytes)?;
    ensure!(
        canonical_record(&intent)? == bytes
            && intent.schema == 1
            && intent.journal_directory == identity.journal_directory
            && intent.stage == imported.stage_identity()?
            && intent.ticket_sha256 == ticket_digest(imported)?
            && intent.candidate_runtime == runtime.identity()?,
        "guest installation intent differs from retained authority"
    );
    imported.recheck_for_adoption(context, inputs)?;
    Ok(intent)
}

fn ticket_digest(imported: &TicketedGuestImport) -> Result<String> {
    digest_ticket(imported.ticket())
}

fn digest_ticket(ticket: &GuestImportTicket) -> Result<String> {
    Ok(lillux::sha256_hex(
        lillux::canonical_json(&serde_json::to_value(ticket)?)?.as_bytes(),
    ))
}

fn canonical_owner_record(record: &GuestOccurrenceOwnerRecord) -> Result<Vec<u8>> {
    let bytes = lillux::canonical_json(&serde_json::to_value(record)?)?.into_bytes();
    ensure!(
        bytes.len() as u64 <= MAX_RECORD_BYTES,
        "guest occurrence owner record exceeds bound"
    );
    Ok(bytes)
}

fn canonical_record(intent: &GuestBaseInstallIntent) -> Result<Vec<u8>> {
    let bytes = lillux::canonical_json(&serde_json::to_value(intent)?)?.into_bytes();
    ensure!(
        bytes.len() as u64 <= MAX_RECORD_BYTES,
        "guest installation intent exceeds bound"
    );
    Ok(bytes)
}

fn canonical_marker(marker: &GuestBaseInstallMarker) -> Result<Vec<u8>> {
    let bytes = lillux::canonical_json(&serde_json::to_value(marker)?)?.into_bytes();
    ensure!(
        bytes.len() as u64 <= MAX_RECORD_BYTES,
        "guest installation owner marker exceeds bound"
    );
    Ok(bytes)
}
