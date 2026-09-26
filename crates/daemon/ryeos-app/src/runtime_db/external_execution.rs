//! Subordinate external allocation ownership, attached to an admitted execution.
//!
//! This is not a second session scheduler. Its key is the existing placement
//! thread. The local worker remains an ordinary local process. Persist contact
//! intention before calling an external allocator; an ambiguous call consumes
//! capacity indefinitely until an independently qualified reconciliation path
//! settles it. No TTL, local process death or caller boolean is cleanup proof.

use super::*;
use anyhow::ensure;

mod channel;
pub use channel::ExternalDirectOutput;
pub(crate) mod connector;
pub(crate) use channel::{
    ExternalCandidateImportClaim, ExternalCandidateImportTarget, ExternalProtocolOutputClaim,
    ExternalSupervisorExchange, RetainedExternalCandidateImport,
};

pub(super) const FIRST_EPOCH: u32 = 40;
pub const EXTERNAL_ALLOCATION_RESERVATION_SCHEMA: u32 = 5;
// The bounded direct projection is operational authority retained inline, not
// a new CAS root or caller-authored command. Metadata remains separately bounded.
const MAX_EXTERNAL_ALLOCATION_RESERVATION_BYTES: usize =
    ryeos_state::external_execution::admission::MAX_EXTERNAL_DIRECT_PROGRAM_BYTES + 8192;
pub(super) const GUARD_SQL: &str = r#"CREATE TABLE external_execution_guard (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    schema_version INTEGER NOT NULL CHECK (schema_version = 1),
    unsettled INTEGER NOT NULL CHECK (unsettled >= 0)
)"#;

pub(super) const JOURNAL_SQL: &str = r#"
CREATE TABLE external_execution_binding_generation (
    binding_hash TEXT PRIMARY KEY,
    capacity_owner TEXT NOT NULL,
    binding_json TEXT NOT NULL,
    retained_at_ms INTEGER NOT NULL
);
CREATE TRIGGER external_execution_binding_generation_no_update
BEFORE UPDATE ON external_execution_binding_generation
BEGIN SELECT RAISE(ABORT, 'retained external binding generation is immutable'); END;
CREATE TRIGGER external_execution_binding_generation_no_delete
BEFORE DELETE ON external_execution_binding_generation
BEGIN SELECT RAISE(ABORT, 'retained external binding generation requires explicit obligation-aware cleanup'); END;
CREATE TABLE external_execution_allocation (
    placement_thread_id TEXT PRIMARY KEY,
    capacity_owner TEXT NOT NULL,
    reservation_json TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN
        ('reserved','contact_pending','bound','quarantined','no_contact',
         'contacted_no_occurrence','terminated')),
    occurrence_json TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK ((phase IN ('reserved','contact_pending','no_contact','contacted_no_occurrence')
            AND occurrence_json IS NULL)
        OR phase = 'quarantined'
        OR (phase IN ('bound','terminated') AND occurrence_json IS NOT NULL))
);
CREATE INDEX idx_external_execution_capacity
    ON external_execution_allocation(capacity_owner, phase);
CREATE TABLE external_execution_import (
    binding_digest TEXT PRIMARY KEY,
    snapshot_hash TEXT NOT NULL,
    output_capture_hash TEXT,
    evidence_blob_hash TEXT NOT NULL,
    completion_request_digest TEXT NOT NULL,
    export_frame_digest TEXT NOT NULL
);

CREATE TABLE external_execution_connector (
    placement_thread_id TEXT PRIMARY KEY REFERENCES external_execution_allocation(placement_thread_id),
    channel_binding_digest TEXT NOT NULL,
    execution_binding_hash TEXT NOT NULL,
    connector_protocol TEXT NOT NULL,
    connector_artifact_hash TEXT NOT NULL,
    connector_artifact_bytes INTEGER NOT NULL CHECK (connector_artifact_bytes > 0),
    capability_generation TEXT NOT NULL,
    capability_hash TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('prepared','connected','closed')),
    peer_process_identity_json TEXT,
    peer_process_identity_digest TEXT,
    prepared_at_ms INTEGER NOT NULL,
    connected_at_ms INTEGER,
    closed_at_ms INTEGER,
    close_reason TEXT,
    CHECK (
        ((state='prepared' AND peer_process_identity_json IS NULL
              AND peer_process_identity_digest IS NULL AND connected_at_ms IS NULL
              AND closed_at_ms IS NULL AND close_reason IS NULL)
         OR (state='connected' AND peer_process_identity_json IS NOT NULL
              AND peer_process_identity_digest IS NOT NULL AND connected_at_ms IS NOT NULL
              AND closed_at_ms IS NULL AND close_reason IS NULL)
         OR (state='closed' AND closed_at_ms IS NOT NULL AND close_reason IS NOT NULL
              AND ((peer_process_identity_json IS NULL AND peer_process_identity_digest IS NULL
                    AND connected_at_ms IS NULL)
                OR (peer_process_identity_json IS NOT NULL
                    AND peer_process_identity_digest IS NOT NULL
                    AND connected_at_ms IS NOT NULL))))
        AND (connected_at_ms IS NULL OR connected_at_ms >= prepared_at_ms)
        AND (closed_at_ms IS NULL OR closed_at_ms >= prepared_at_ms)
        AND (connected_at_ms IS NULL OR closed_at_ms IS NULL
             OR closed_at_ms >= connected_at_ms)
    )
);
CREATE TRIGGER external_execution_connector_insert_guard
BEFORE INSERT ON external_execution_connector
WHEN NEW.state!='prepared'
 OR NOT EXISTS(SELECT 1 FROM external_execution_allocation a
    JOIN external_execution_channel c ON c.placement_thread_id=a.placement_thread_id
    WHERE a.placement_thread_id=NEW.placement_thread_id AND a.phase='bound'
      AND c.binding_digest=NEW.channel_binding_digest AND c.state='running'
      AND json_extract(c.binding_json,'$.execution_binding_hash')=NEW.execution_binding_hash)
 OR NOT EXISTS(SELECT 1 FROM external_execution_frame f
    WHERE f.binding_digest=NEW.channel_binding_digest
      AND f.direction='owner_to_supervisor'
      AND json_extract(f.frame_json,'$.frame.payload.kind')='release')
BEGIN SELECT RAISE(ABORT, 'external connector requires exact released channel authority'); END;
CREATE TRIGGER external_execution_connector_transition_guard
BEFORE UPDATE ON external_execution_connector
WHEN NEW.placement_thread_id != OLD.placement_thread_id
 OR NEW.channel_binding_digest != OLD.channel_binding_digest
 OR NEW.execution_binding_hash != OLD.execution_binding_hash
 OR NEW.connector_protocol != OLD.connector_protocol
 OR NEW.connector_artifact_hash != OLD.connector_artifact_hash
 OR NEW.connector_artifact_bytes != OLD.connector_artifact_bytes
 OR NEW.capability_generation != OLD.capability_generation
 OR NEW.capability_hash != OLD.capability_hash
 OR NEW.prepared_at_ms != OLD.prepared_at_ms
 OR (OLD.state='prepared' AND NEW.state='closed'
     AND (NEW.peer_process_identity_json IS NOT NULL
       OR NEW.peer_process_identity_digest IS NOT NULL
       OR NEW.connected_at_ms IS NOT NULL))
 OR (OLD.state='connected'
     AND (NEW.peer_process_identity_json IS NOT OLD.peer_process_identity_json
       OR NEW.peer_process_identity_digest IS NOT OLD.peer_process_identity_digest
       OR NEW.connected_at_ms IS NOT OLD.connected_at_ms))
 OR NOT ((OLD.state='prepared' AND NEW.state IN ('connected','closed'))
      OR (OLD.state='connected' AND NEW.state='closed'))
BEGIN SELECT RAISE(ABORT, 'external connector transition contradicts retained authority'); END;
CREATE TRIGGER external_execution_connector_no_delete
BEFORE DELETE ON external_execution_connector
BEGIN SELECT RAISE(ABORT, 'external connector occurrence is retained'); END;
CREATE TRIGGER external_execution_connector_settlement_guard
BEFORE UPDATE OF phase ON external_execution_allocation
WHEN NEW.phase IN ('no_contact','contacted_no_occurrence','terminated')
 AND EXISTS(SELECT 1 FROM external_execution_connector c
    WHERE c.placement_thread_id=OLD.placement_thread_id AND c.state!='closed')
BEGIN SELECT RAISE(ABORT, 'external allocation retains a live local connector'); END;

CREATE TABLE external_execution_no_occurrence (
    placement_thread_id TEXT PRIMARY KEY REFERENCES external_execution_allocation(placement_thread_id),
    evidence_json TEXT NOT NULL
);
CREATE TRIGGER external_execution_no_occurrence_insert_guard
BEFORE INSERT ON external_execution_no_occurrence
WHEN NOT EXISTS(SELECT 1 FROM external_execution_allocation a
    WHERE a.placement_thread_id=NEW.placement_thread_id
      AND a.phase IN ('contact_pending','quarantined')
      AND a.occurrence_json IS NULL)
BEGIN SELECT RAISE(ABORT, 'external no-occurrence evidence has no unresolved contact'); END;
CREATE TRIGGER external_execution_no_occurrence_immutable
BEFORE UPDATE ON external_execution_no_occurrence
BEGIN SELECT RAISE(ABORT, 'external no-occurrence evidence is immutable'); END;
CREATE TRIGGER external_execution_no_occurrence_no_delete
BEFORE DELETE ON external_execution_no_occurrence
BEGIN SELECT RAISE(ABORT, 'external no-occurrence evidence is retained'); END;

CREATE TABLE external_execution_supervisor_activation_intent (
    placement_thread_id TEXT PRIMARY KEY REFERENCES external_execution_allocation(placement_thread_id),
    intent_json TEXT NOT NULL
);
CREATE TRIGGER external_execution_supervisor_activation_intent_insert_guard
BEFORE INSERT ON external_execution_supervisor_activation_intent
WHEN NOT EXISTS(SELECT 1 FROM external_execution_allocation a
    WHERE a.placement_thread_id=NEW.placement_thread_id
      AND a.phase='bound' AND a.occurrence_json IS NOT NULL)
BEGIN SELECT RAISE(ABORT, 'external supervisor activation has no executable bound occurrence'); END;
CREATE TRIGGER external_execution_supervisor_activation_intent_immutable
BEFORE UPDATE ON external_execution_supervisor_activation_intent
BEGIN SELECT RAISE(ABORT, 'external supervisor activation intent is immutable'); END;
CREATE TRIGGER external_execution_supervisor_activation_intent_no_delete
BEFORE DELETE ON external_execution_supervisor_activation_intent
BEGIN SELECT RAISE(ABORT, 'external supervisor activation intent is retained'); END;

CREATE TABLE external_execution_supervisor_activation_observation (
    placement_thread_id TEXT PRIMARY KEY REFERENCES external_execution_allocation(placement_thread_id),
    observation_json TEXT NOT NULL
);
CREATE TRIGGER external_execution_supervisor_activation_observation_insert_guard
BEFORE INSERT ON external_execution_supervisor_activation_observation
WHEN NOT EXISTS(SELECT 1 FROM external_execution_allocation a
    JOIN external_execution_supervisor_activation_intent i
      ON i.placement_thread_id=a.placement_thread_id
    WHERE a.placement_thread_id=NEW.placement_thread_id
      AND a.phase IN ('bound','quarantined') AND a.occurrence_json IS NOT NULL)
BEGIN SELECT RAISE(ABORT, 'external supervisor activation observation has no durable intent'); END;
CREATE TRIGGER external_execution_supervisor_activation_observation_immutable
BEFORE UPDATE ON external_execution_supervisor_activation_observation
BEGIN SELECT RAISE(ABORT, 'external supervisor activation observation is immutable'); END;
CREATE TRIGGER external_execution_supervisor_activation_observation_no_delete
BEFORE DELETE ON external_execution_supervisor_activation_observation
BEGIN SELECT RAISE(ABORT, 'external supervisor activation observation is retained'); END;

CREATE TABLE external_execution_termination_intent (
    placement_thread_id TEXT PRIMARY KEY REFERENCES external_execution_allocation(placement_thread_id),
    intent_json TEXT NOT NULL
);
CREATE TRIGGER external_execution_termination_intent_insert_guard
BEFORE INSERT ON external_execution_termination_intent
WHEN NOT EXISTS(SELECT 1 FROM external_execution_allocation a
    WHERE a.placement_thread_id=NEW.placement_thread_id
      AND a.phase IN ('bound','quarantined')
      AND a.occurrence_json IS NOT NULL)
BEGIN SELECT RAISE(ABORT, 'external termination intent has no bound occurrence'); END;
CREATE TRIGGER external_execution_termination_intent_immutable
BEFORE UPDATE ON external_execution_termination_intent
BEGIN SELECT RAISE(ABORT, 'external termination intent is immutable'); END;
CREATE TRIGGER external_execution_termination_intent_no_delete
BEFORE DELETE ON external_execution_termination_intent
BEGIN SELECT RAISE(ABORT, 'external termination intent is retained'); END;

CREATE TABLE external_execution_terminal_observation (
    placement_thread_id TEXT PRIMARY KEY REFERENCES external_execution_allocation(placement_thread_id),
    observation_json TEXT NOT NULL
);
CREATE TRIGGER external_execution_terminal_observation_insert_guard
BEFORE INSERT ON external_execution_terminal_observation
WHEN NOT EXISTS(SELECT 1 FROM external_execution_allocation a
    JOIN external_execution_termination_intent i
      ON i.placement_thread_id=a.placement_thread_id
    WHERE a.placement_thread_id=NEW.placement_thread_id
      AND a.phase IN ('bound','quarantined')
      AND a.occurrence_json IS NOT NULL)
BEGIN SELECT RAISE(ABORT, 'external terminal evidence has no termination intent'); END;
CREATE TRIGGER external_execution_terminal_observation_immutable
BEFORE UPDATE ON external_execution_terminal_observation
BEGIN SELECT RAISE(ABORT, 'external terminal observation is immutable'); END;
CREATE TRIGGER external_execution_terminal_observation_no_delete
BEFORE DELETE ON external_execution_terminal_observation
BEGIN SELECT RAISE(ABORT, 'external terminal observation is retained'); END;


CREATE TRIGGER external_execution_import_no_update
BEFORE UPDATE ON external_execution_import
BEGIN SELECT RAISE(ABORT, 'retained external import is immutable'); END;
CREATE TRIGGER external_execution_import_no_delete
BEFORE DELETE ON external_execution_import
BEGIN SELECT RAISE(ABORT, 'external import requires explicit completion retention handoff'); END;

CREATE TRIGGER external_execution_channel_no_delete
BEFORE DELETE ON external_execution_channel
WHEN EXISTS(SELECT 1 FROM external_execution_allocation a
    WHERE a.placement_thread_id=OLD.placement_thread_id
      AND a.phase NOT IN ('no_contact','contacted_no_occurrence','terminated'))
BEGIN SELECT RAISE(ABORT, 'external execution retains its channel'); END;

CREATE TRIGGER external_execution_frame_no_delete
BEFORE DELETE ON external_execution_frame
WHEN EXISTS(SELECT 1 FROM external_execution_channel c
    JOIN external_execution_allocation a ON a.placement_thread_id=c.placement_thread_id
    WHERE c.binding_digest=OLD.binding_digest
      AND a.phase NOT IN ('no_contact','contacted_no_occurrence','terminated'))
BEGIN SELECT RAISE(ABORT, 'external execution retains its transcript'); END;
CREATE TRIGGER external_execution_insert_guard
AFTER INSERT ON external_execution_allocation
WHEN NEW.phase NOT IN ('no_contact','contacted_no_occurrence','terminated')
BEGIN
    UPDATE external_execution_guard SET unsettled=unsettled+1 WHERE singleton=1;
    SELECT CASE WHEN changes()!=1 THEN RAISE(ABORT, 'external execution guard absent') END;
END;
CREATE TRIGGER external_execution_update_guard
AFTER UPDATE OF phase ON external_execution_allocation
WHEN OLD.phase NOT IN ('no_contact','contacted_no_occurrence','terminated')
 AND NEW.phase IN ('no_contact','contacted_no_occurrence','terminated')
BEGIN
    UPDATE external_execution_guard SET unsettled=unsettled-1 WHERE singleton=1;
    SELECT CASE WHEN changes()!=1 THEN RAISE(ABORT, 'external execution guard absent') END;
END;
CREATE TRIGGER external_execution_no_reactivation
BEFORE UPDATE ON external_execution_allocation
WHEN OLD.phase IN ('no_contact','contacted_no_occurrence','terminated')
BEGIN SELECT RAISE(ABORT, 'settled external allocation is immutable'); END;
CREATE TRIGGER external_execution_settlement_evidence_guard
BEFORE UPDATE OF phase ON external_execution_allocation
WHEN (NEW.phase='contacted_no_occurrence' AND NOT EXISTS(
        SELECT 1 FROM external_execution_no_occurrence e
        WHERE e.placement_thread_id=OLD.placement_thread_id))
 OR (NEW.phase='terminated' AND NOT EXISTS(
        SELECT 1 FROM external_execution_terminal_observation e
        WHERE e.placement_thread_id=OLD.placement_thread_id))
BEGIN SELECT RAISE(ABORT, 'external settlement lacks independently retained evidence'); END;
CREATE TRIGGER external_execution_transition_guard
BEFORE UPDATE ON external_execution_allocation
WHEN NEW.placement_thread_id != OLD.placement_thread_id
 OR NEW.capacity_owner != OLD.capacity_owner
 OR NEW.reservation_json != OLD.reservation_json
 OR NOT (
    (OLD.phase='reserved' AND NEW.phase IN ('contact_pending','no_contact'))
    OR (OLD.phase='contact_pending' AND NEW.phase IN
        ('bound','quarantined','contacted_no_occurrence'))
    OR (OLD.phase='bound' AND NEW.phase IN ('quarantined','terminated'))
    OR (OLD.phase='quarantined' AND NEW.phase IN
        ('quarantined','contacted_no_occurrence','terminated'))
 )
BEGIN SELECT RAISE(ABORT, 'external allocation transition contradicts retained authority'); END;
CREATE TRIGGER external_execution_no_delete
BEFORE DELETE ON external_execution_allocation
WHEN OLD.phase NOT IN ('no_contact','contacted_no_occurrence','terminated')
BEGIN SELECT RAISE(ABORT, 'unsettled external execution cannot be deleted'); END;
CREATE TRIGGER external_execution_credential_release_guard
BEFORE UPDATE ON credential_profile
WHEN (NEW.lock_owner IS NOT OLD.lock_owner
      OR NEW.credential_generation != OLD.credential_generation
      OR NEW.profile_id IS NOT OLD.profile_id
      OR NEW.home_id IS NOT OLD.home_id
      OR NEW.owner_principal IS NOT OLD.owner_principal)
AND EXISTS (
    SELECT 1 FROM external_execution_allocation a
    JOIN dedicated_session s ON s.placement_thread_id=a.placement_thread_id
    WHERE s.credential_profile_id=OLD.profile_id
      AND a.phase NOT IN ('no_contact','contacted_no_occurrence','terminated')
)
BEGIN SELECT RAISE(ABORT, 'external execution cleanup retains credential ownership'); END;
CREATE TRIGGER external_execution_credential_delete_guard
BEFORE DELETE ON credential_profile
WHEN EXISTS (
    SELECT 1 FROM external_execution_allocation a
    JOIN dedicated_session s ON s.placement_thread_id=a.placement_thread_id
    WHERE s.credential_profile_id=OLD.profile_id
      AND a.phase NOT IN ('no_contact','contacted_no_occurrence','terminated')
)
BEGIN SELECT RAISE(ABORT, 'external execution retains its credential owner'); END;
CREATE TRIGGER external_execution_workspace_delete_guard
BEFORE DELETE ON execution_workspace
WHEN EXISTS (
    SELECT 1 FROM external_execution_allocation a
    JOIN dedicated_session s ON s.placement_thread_id=a.placement_thread_id
    WHERE s.workspace_id=OLD.workspace_id
      AND a.phase NOT IN ('no_contact','contacted_no_occurrence','terminated')
)
BEGIN SELECT RAISE(ABORT, 'external execution retains its workspace'); END;
CREATE TRIGGER external_execution_workspace_identity_guard
BEFORE UPDATE ON execution_workspace
WHEN EXISTS (
    SELECT 1 FROM external_execution_allocation a
    JOIN dedicated_session s ON s.placement_thread_id=a.placement_thread_id
    WHERE s.workspace_id=OLD.workspace_id
      AND a.phase NOT IN ('no_contact','contacted_no_occurrence','terminated')
)
AND (NEW.workspace_id IS NOT OLD.workspace_id
    OR NEW.thread_id IS NOT OLD.thread_id
    OR NEW.launch_owner IS NOT OLD.launch_owner
    OR NEW.base_snapshot IS NOT OLD.base_snapshot
    OR NEW.root_path IS NOT OLD.root_path
    OR NEW.frozen_snapshot_hash IS NOT OLD.frozen_snapshot_hash
    OR NEW.frozen_output_capture_hash IS NOT OLD.frozen_output_capture_hash
    OR NEW.state IN ('freezing','destroying','closing','closed'))
BEGIN SELECT RAISE(ABORT, 'external execution blocks local workspace settlement'); END;
CREATE TRIGGER external_execution_session_delete_guard
BEFORE DELETE ON dedicated_session
WHEN EXISTS (SELECT 1 FROM external_execution_allocation a
    WHERE a.placement_thread_id=OLD.placement_thread_id
      AND a.phase NOT IN ('no_contact','contacted_no_occurrence','terminated'))
BEGIN SELECT RAISE(ABORT, 'external execution retains its session owner'); END;
CREATE TRIGGER external_execution_session_identity_guard
BEFORE UPDATE ON dedicated_session
WHEN EXISTS (SELECT 1 FROM external_execution_allocation a
    WHERE a.placement_thread_id=OLD.placement_thread_id
      AND a.phase NOT IN ('no_contact','contacted_no_occurrence','terminated'))
AND (NEW.placement_thread_id IS NOT OLD.placement_thread_id
    OR NEW.admitted_capsule_hash IS NOT OLD.admitted_capsule_hash
    OR NEW.workspace_id IS NOT OLD.workspace_id
    OR NEW.worker_instance_id IS NOT OLD.worker_instance_id
    OR NEW.worker_boot_epoch IS NOT OLD.worker_boot_epoch
    OR NEW.credential_profile_id IS NOT OLD.credential_profile_id
    OR NEW.credential_generation IS NOT OLD.credential_generation
    OR NEW.candidate_snapshot_hash IS NOT OLD.candidate_snapshot_hash
    OR NEW.state IN ('freezing','frozen','verifying','qualifying','publish_ready',
        'publishing','discarding','terminal'))
BEGIN SELECT RAISE(ABORT, 'external execution blocks local completion and owner replacement'); END;
CREATE TRIGGER external_execution_direct_thread_delete_guard
BEFORE DELETE ON thread_runtime
WHEN EXISTS(SELECT 1 FROM external_execution_allocation a
    WHERE a.placement_thread_id=OLD.thread_id
      AND json_extract(a.reservation_json,'$.owner.kind')='direct_thread'
      AND a.phase NOT IN ('no_contact','contacted_no_occurrence','terminated'))
BEGIN SELECT RAISE(ABORT, 'external direct execution retains its thread owner'); END;
CREATE TRIGGER external_execution_direct_thread_identity_guard
BEFORE UPDATE ON thread_runtime
WHEN (NEW.thread_id IS NOT OLD.thread_id OR NEW.chain_root_id IS NOT OLD.chain_root_id)
 AND EXISTS(SELECT 1 FROM external_execution_allocation a
    WHERE a.placement_thread_id=OLD.thread_id
      AND json_extract(a.reservation_json,'$.owner.kind')='direct_thread'
      AND a.phase NOT IN ('no_contact','contacted_no_occurrence','terminated'))
BEGIN SELECT RAISE(ABORT, 'external direct execution retains its exact chain owner'); END;
"#;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalDedicatedSessionOwner {
    pub workspace_id: String,
    pub worker_instance_id: String,
    pub worker_boot_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExternalAllocationOwner {
    DedicatedSession(ExternalDedicatedSessionOwner),
    DirectThread {
        chain_root_id: String,
        launch_owner: LaunchOwner,
        program: ryeos_state::external_execution::admission::AdmittedExternalDirectProgram,
    },
}

impl ExternalAllocationOwner {
    pub fn dedicated_session(&self) -> Result<&ExternalDedicatedSessionOwner> {
        match self {
            Self::DedicatedSession(owner) => Ok(owner),
            Self::DirectThread { .. } => {
                bail!("external allocation has no dedicated-session owner")
            }
        }
    }

    fn validate(&self, placement: &str) -> Result<()> {
        match self {
            Self::DedicatedSession(owner) => {
                validate_bounded_runtime_text("external workspace", &owner.workspace_id, 256)?;
                validate_bounded_runtime_text("external worker", &owner.worker_instance_id, 256)?;
                ensure!(
                    (1..=i64::MAX as u64).contains(&owner.worker_boot_epoch),
                    "external worker epoch is invalid"
                );
            }
            Self::DirectThread {
                chain_root_id,
                launch_owner,
                program,
            } => {
                program.validate()?;
                validate_bounded_runtime_text("external direct chain", chain_root_id, 256)?;
                validate_bounded_runtime_text(
                    "external direct claim",
                    &launch_owner.unpredictable_nonce,
                    256,
                )?;
                validate_bounded_runtime_text(
                    "external direct daemon",
                    &launch_owner.daemon_generation_id,
                    256,
                )?;
                ensure!(
                    launch_owner.thread_id == placement
                        && (1..=i64::MAX as u64).contains(&launch_owner.monotonic_launch_epoch),
                    "external direct launch owner is invalid"
                );
            }
        }
        Ok(())
    }
}

/// Initial direct programs arrive only through the opaque app compiler proof.
/// The allocation adapter receives its bounded allocation-only projection, not
/// these retained arguments/input or the controller's protected credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalAllocationReservation {
    pub schema: u32,
    pub placement_thread_id: String,
    pub admitted_capsule_hash: String,
    pub owner: ExternalAllocationOwner,
    pub base_snapshot_hash: String,
    pub binding_hash: String,
    /// Stable protected capacity domain across binding/policy generations.
    pub capacity_owner: String,
    /// Deterministic app-private vault coordinate for the controller signer and
    /// one-use attachment capability.  This is an identity, never a secret.
    pub channel_authority_generation: String,
    /// Exact controller channel signer selected before allocator contact.
    pub channel_owner_public_key: String,
    /// Digest of the one-use attachment capability delivered only to the
    /// protected supervisor bootstrap.
    pub channel_bootstrap_capability_hash: String,
    pub request_digest: String,
    pub max_active: u16,
    pub timeout_seconds: u32,
    pub contact_deadline_ms: i64,
    /// First allocation reservation, after local guest-input preparation.
    pub startup_started_at_ms: i64,
    /// Non-renewing readiness expiry derived from the exact admitted capsule.
    pub startup_deadline_ms: i64,
}

/// Nonportable observation policy supplied by the existing lifecycle owner.
/// This is not durable identity: its consequence is committed with the exact
/// retained observation in the allocation's existing transaction.
#[derive(Clone, Copy)]
pub(crate) enum ExternalObservationTiming {
    Startup {
        deadline_exceeded: bool,
        live_deadline: lillux::time::MonotonicDeadline,
    },
    Cleanup,
}

impl ExternalObservationTiming {
    pub(crate) fn with_deadline_exceeded(self, observed: bool) -> Self {
        match self {
            Self::Startup {
                deadline_exceeded,
                live_deadline,
            } => Self::Startup {
                deadline_exceeded: deadline_exceeded || observed,
                live_deadline,
            },
            Self::Cleanup => Self::Cleanup,
        }
    }
}

fn fence_external_observation_timing(
    conn: &Connection,
    record: &ExternalAllocationRecord,
    timing: ExternalObservationTiming,
) -> Result<()> {
    let now = i64::try_from(lillux::time::timestamp_millis())?;
    let quarantine = match timing {
        ExternalObservationTiming::Cleanup => true,
        ExternalObservationTiming::Startup {
            deadline_exceeded,
            live_deadline,
        } => {
            deadline_exceeded
                || live_deadline.has_elapsed()
                || record.reservation.require_startup_time(now).is_err()
        }
    };
    if quarantine && !record.phase.is_settled() {
        conn.execute(
            "UPDATE external_execution_allocation SET phase='quarantined',updated_at_ms=?2
             WHERE placement_thread_id=?1",
            params![record.reservation.placement_thread_id, now],
        )?;
    }
    Ok(())
}

impl ExternalAllocationReservation {
    pub fn validate(&self) -> Result<()> {
        if self.schema != EXTERNAL_ALLOCATION_RESERVATION_SCHEMA
            || !(1..=64).contains(&self.max_active)
            || !(1..=3600).contains(&self.timeout_seconds)
            || self.contact_deadline_ms <= 0
            || self.startup_started_at_ms <= 0
            || !self
                .startup_deadline_ms
                .checked_sub(self.startup_started_at_ms)
                .is_some_and(|duration| (1..=600_000).contains(&duration))
        {
            bail!("external allocation reservation is outside its versioned bounds");
        }
        validate_bounded_runtime_text("external placement", &self.placement_thread_id, 256)?;
        self.owner.validate(&self.placement_thread_id)?;
        for hash in [
            &self.admitted_capsule_hash,
            &self.base_snapshot_hash,
            &self.binding_hash,
            &self.capacity_owner,
            &self.channel_authority_generation,
            &self.channel_bootstrap_capability_hash,
            &self.request_digest,
        ] {
            validate_sha256("external allocation identity", hash)?;
        }
        ryeos_state::external_execution::validate_channel_public_key(
            &self.channel_owner_public_key,
        )?;
        ensure!(
            lillux::canonical_json(&serde_json::to_value(self)?)?.len()
                <= MAX_EXTERNAL_ALLOCATION_RESERVATION_BYTES,
            "external allocation reservation exceeds its serialized bound"
        );
        Ok(())
    }

    pub(crate) fn validate_startup_budget(&self, ready_timeout_ms: u64) -> Result<()> {
        self.validate()?;
        if self
            .startup_deadline_ms
            .checked_sub(self.startup_started_at_ms)
            != Some(i64::try_from(ready_timeout_ms)?)
        {
            bail!("external startup reservation changed its admitted readiness budget");
        }
        Ok(())
    }

    pub(crate) fn require_startup_time(&self, now_ms: i64) -> Result<()> {
        self.validate()?;
        if now_ms < self.startup_started_at_ms || now_ms >= self.startup_deadline_ms {
            bail!("external startup readiness deadline expired or clock preceded its anchor");
        }
        Ok(())
    }

    pub(crate) fn startup_deadline(&self) -> Result<lillux::time::MonotonicDeadline> {
        // Durable recovery follows the existing wall-clock convention. This
        // projection does not claim monotonic continuity across a restart.
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        self.require_startup_time(now)?;
        Ok(lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_millis(u64::try_from(self.startup_deadline_ms - now)?),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalAllocationOccurrence {
    pub schema: u32,
    pub binding_hash: String,
    pub request_digest: String,
    pub occurrence_id: String,
    pub provider_observation_digest: String,
}

/// Authoritative adapter evidence that the exact allocation request produced
/// no occurrence. This is distinct from `NoContact`: provider contact happened,
/// but exact reconciliation proved that no cleanup obligation exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExternalNoOccurrenceEvidence {
    pub schema: u32,
    pub binding_hash: String,
    pub request_digest: String,
    pub provider_observation_digest: String,
}

impl ExternalNoOccurrenceEvidence {
    fn validate(&self, reservation: &ExternalAllocationReservation) -> Result<()> {
        if self.schema != 1
            || self.binding_hash != reservation.binding_hash
            || self.request_digest != reservation.request_digest
        {
            bail!("external no-occurrence evidence contradicts its reservation");
        }
        validate_sha256(
            "external no-occurrence observation",
            &self.provider_observation_digest,
        )
    }
}

/// Exact controller-authored supervisor start request, retained before the
/// lifecycle adapter is allowed to mutate the bound occurrence. Capability
/// plaintext and TLS certificate bytes remain outside this public journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExternalSupervisorActivationIntent {
    pub schema: u32,
    pub binding_hash: String,
    pub request_digest: String,
    pub occurrence_id: String,
    pub supervisor_runtime_hash: String,
    pub guest_input_identity: String,
    pub activation_request_digest: String,
    pub attachment_deadline_ms: i64,
    pub execution_timeout_seconds: u32,
    pub post_execution_timeout_seconds: u32,
    pub channel_max_bytes: u64,
    /// Package identity is retained in this same immutable row, so the
    /// supervisor-start contact claim can never outlive or change its upload.
    pub delivery: ExternalGuestPackageDeliveryCommitment,
}

/// Claimed identity of a locally re-imported package selected for the *same*
/// one-shot supervisor activation. Validation here does not prove package
/// provenance or persist this claim; the caller must supply the prepared
/// package and the retained activation/binding contract. This is a subordinate
/// delivery coordinate, not a second contact permit. Its hashes must never
/// enter activation_request_digest: the package manifest already commits it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExternalGuestPackageDeliveryCommitment {
    pub schema: u32,
    pub binding_hash: String,
    pub request_digest: String,
    pub occurrence_id: String,
    pub activation_request_digest: String,
    pub guest_input_identity: String,
    pub manifest_sha256: String,
    pub payload_sha256: String,
    pub regular_bytes: u64,
    pub framed_bytes: u64,
}

impl ExternalGuestPackageDeliveryCommitment {
    /// `activation` and `contract` must already have been validated against
    /// the exact retained reservation and signed binding. The package producer
    /// must separately prove these digests/lengths describe its pinned inode.
    pub(crate) fn validate_for(
        &self,
        activation: &ExternalSupervisorActivationIntent,
        contract: &crate::node_config::sections::external_execution::ExternalPlacementBackendContract,
    ) -> Result<()> {
        ensure!(
            self.schema == 1
                && self.binding_hash == activation.binding_hash
                && self.request_digest == activation.request_digest
                && self.occurrence_id == activation.occurrence_id
                && self.activation_request_digest == activation.activation_request_digest
                && self.guest_input_identity == activation.guest_input_identity,
            "external guest package delivery contradicts activation authority"
        );
        validate_sha256("external guest package manifest", &self.manifest_sha256)?;
        validate_sha256("external guest package payload", &self.payload_sha256)?;
        ensure!(
            self.regular_bytes > 0
                && self.regular_bytes <= contract.max_guest_package_regular_bytes
                && self.framed_bytes <= contract.max_guest_package_framed_bytes
                && self.framed_bytes >= self.regular_bytes.saturating_add(20),
            "external guest package delivery exceeds retained binding budget"
        );
        Ok(())
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn fixture_guest_package_delivery(
    binding_hash: &str,
    request_digest: &str,
    occurrence_id: &str,
    activation_request_digest: &str,
    guest_input_identity: &str,
) -> ExternalGuestPackageDeliveryCommitment {
    ExternalGuestPackageDeliveryCommitment {
        schema: 1,
        binding_hash: binding_hash.into(),
        request_digest: request_digest.into(),
        occurrence_id: occurrence_id.into(),
        activation_request_digest: activation_request_digest.into(),
        guest_input_identity: guest_input_identity.into(),
        manifest_sha256: "1".repeat(64),
        payload_sha256: "2".repeat(64),
        regular_bytes: 1,
        framed_bytes: 21,
    }
}

impl ExternalSupervisorActivationIntent {
    fn validate(
        &self,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
    ) -> Result<()> {
        if self.schema != 3
            || self.binding_hash != reservation.binding_hash
            || self.request_digest != reservation.request_digest
            || self.occurrence_id != occurrence.occurrence_id
            || self.execution_timeout_seconds != reservation.timeout_seconds
            || self.attachment_deadline_ms <= reservation.contact_deadline_ms
            || !(1..=900).contains(&self.post_execution_timeout_seconds)
            || !(1..=64 * 1024 * 1024).contains(&self.channel_max_bytes)
        {
            bail!("external supervisor activation intent contradicts its occurrence");
        }
        validate_sha256("external supervisor runtime", &self.supervisor_runtime_hash)?;
        validate_sha256("external guest input identity", &self.guest_input_identity)?;
        validate_sha256(
            "external supervisor activation request",
            &self.activation_request_digest,
        )
    }

    pub(crate) fn validate_contract(
        &self,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        contract: &crate::node_config::sections::external_execution::ExternalPlacementBackendContract,
    ) -> Result<()> {
        self.validate(reservation, occurrence)?;
        let exact_attachment_deadline = reservation
            .contact_deadline_ms
            .checked_add(i64::from(contract.observation_timeout_seconds) * 1_000)
            .context("external supervisor attachment deadline overflow")?;
        if self.supervisor_runtime_hash != reserved_runtime_manifest(reservation, contract)?
            || self.attachment_deadline_ms != exact_attachment_deadline
            || self.post_execution_timeout_seconds
                != contract
                    .observation_timeout_seconds
                    .checked_add(contract.cleanup_timeout_seconds)
                    .context("external supervisor post-execution timeout overflow")?
            || self.channel_max_bytes != contract.max_transfer_bytes.min(64 * 1024 * 1024)
        {
            bail!("external supervisor activation changed its retained binding contract");
        }
        self.delivery.validate_for(self, contract)?;
        ensure!(
            self.activation_request_digest
                == external_supervisor_activation_request_digest(
                    reservation,
                    occurrence,
                    contract,
                    self.attachment_deadline_ms,
                    self.post_execution_timeout_seconds,
                    self.channel_max_bytes,
                    &self.guest_input_identity,
                )?,
            "external supervisor activation request identity changed"
        );
        Ok(())
    }
}

/// The session runtime is selected by its protected binding. An ordinary
/// command's runtime is selected by its retained compiler output, never by a
/// worker field borrowed into a direct binding.
fn reserved_runtime_manifest<'a>(
    reservation: &'a ExternalAllocationReservation,
    contract: &'a crate::node_config::sections::external_execution::ExternalPlacementBackendContract,
) -> Result<&'a str> {
    use crate::node_config::sections::external_execution::ExternalWorkloadBinding;
    match &reservation.owner {
        ExternalAllocationOwner::DedicatedSession(_) => Ok(&contract
            .workload
            .structured_session()?
            .runtime_manifest_hash),
        ExternalAllocationOwner::DirectThread { program, .. } => {
            program.validate()?;
            ensure!(
                matches!(contract.workload, ExternalWorkloadBinding::DirectCommand {})
                    && contract.max_export_bytes == 0
                    && program.projection().endpoint_binding_digest == reservation.binding_hash
                    && program.projection().timeout_seconds
                        == u64::from(reservation.timeout_seconds)
                    && reservation.timeout_seconds <= contract.timeout_seconds,
                "direct activation changed its retained workload, binding or execution budget"
            );
            program.runtime_manifest_hash()
        }
    }
}

/// Reproduce the exact non-secret identity of one supervisor-start mutation.
/// The capability plaintext and TLS roots are deliberately absent: their
/// retained hashes already bind the corresponding protected generations.
pub(crate) fn external_supervisor_activation_request_digest(
    reservation: &ExternalAllocationReservation,
    occurrence: &ExternalAllocationOccurrence,
    contract: &crate::node_config::sections::external_execution::ExternalPlacementBackendContract,
    attachment_deadline_ms: i64,
    post_execution_timeout_seconds: u32,
    channel_max_bytes: u64,
    guest_input_identity: &str,
) -> Result<String> {
    validate_sha256("external guest input identity", guest_input_identity)?;
    let runtime_manifest = reserved_runtime_manifest(reservation, contract)?;
    if let ExternalAllocationOwner::DirectThread { program, .. } = &reservation.owner {
        ensure!(
            program.guest_input_identity() == guest_input_identity,
            "direct activation changed its compiled guest input identity"
        );
    }
    ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "domain":"ryeos.external-supervisor-activation.v1",
        "controller":&contract.controller_transport,
        "tls_root_bundle_digest":&contract.controller_transport.tls_root_bundle_digest,
        "placement_thread_id":&reservation.placement_thread_id,
        "occurrence_id":&occurrence.occurrence_id,
        "allocation_request_digest":&reservation.request_digest,
        "admitted_capsule_hash":&reservation.admitted_capsule_hash,
        "base_snapshot_hash":&reservation.base_snapshot_hash,
        "execution_binding_hash":&reservation.binding_hash,
        "supervisor_runtime_hash":runtime_manifest,
        "launcher_artifact_hash":&contract.launcher_artifact_hash,
        "owner_public_key":&reservation.channel_owner_public_key,
        "bootstrap_capability_hash":&reservation.channel_bootstrap_capability_hash,
        "attachment_deadline_ms":attachment_deadline_ms,
        "execution_timeout_seconds":reservation.timeout_seconds,
        "post_execution_timeout_seconds":post_execution_timeout_seconds,
        "channel_max_bytes":channel_max_bytes,
        "guest_input_identity":guest_input_identity,
    }))
}

/// Independently observed result for the exact supervisor-start request.
/// `not_started` is terminal only for activation; the allocated occurrence
/// still requires provider-terminal cleanup before capacity can be released.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExternalSupervisorActivationObservation {
    pub schema: u32,
    pub binding_hash: String,
    pub request_digest: String,
    pub occurrence_id: String,
    pub activation_request_digest: String,
    pub activation_state: String,
    pub provider_observation_digest: String,
}

impl ExternalSupervisorActivationObservation {
    fn validate(
        &self,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        intent: &ExternalSupervisorActivationIntent,
    ) -> Result<()> {
        if self.schema != 1
            || self.binding_hash != reservation.binding_hash
            || self.request_digest != reservation.request_digest
            || self.occurrence_id != occurrence.occurrence_id
            || self.activation_request_digest != intent.activation_request_digest
            || !matches!(self.activation_state.as_str(), "started" | "not_started")
        {
            bail!("external supervisor activation observation contradicts its intent");
        }
        validate_sha256(
            "external supervisor activation observation",
            &self.provider_observation_digest,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalSupervisorActivationRecord {
    pub intent: ExternalSupervisorActivationIntent,
    pub observation: Option<ExternalSupervisorActivationObservation>,
}

/// Durable request written before the one allowed termination mutation. The
/// request identity is derived by the controller and is never supplied by a
/// worker or provider response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExternalTerminationIntent {
    pub schema: u32,
    pub binding_hash: String,
    pub request_digest: String,
    pub occurrence_id: String,
    pub termination_request_digest: String,
}

impl ExternalTerminationIntent {
    fn validate(
        &self,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
    ) -> Result<()> {
        if self.schema != 1
            || self.binding_hash != reservation.binding_hash
            || self.request_digest != reservation.request_digest
            || self.occurrence_id != occurrence.occurrence_id
        {
            bail!("external termination intent contradicts its occurrence");
        }
        validate_sha256(
            "external termination request",
            &self.termination_request_digest,
        )
    }
}

/// Independent provider terminal fact. A termination request acknowledgement,
/// timeout, 404, local process exit, or caller assertion cannot construct it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExternalTerminalObservation {
    pub schema: u32,
    pub binding_hash: String,
    pub request_digest: String,
    pub occurrence_id: String,
    pub termination_request_digest: String,
    pub terminal_state: String,
    pub provider_observation_digest: String,
}

impl ExternalTerminalObservation {
    fn validate(
        &self,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        intent: &ExternalTerminationIntent,
    ) -> Result<()> {
        if self.schema != 1
            || self.binding_hash != reservation.binding_hash
            || self.request_digest != reservation.request_digest
            || self.occurrence_id != occurrence.occurrence_id
            || self.termination_request_digest != intent.termination_request_digest
            || self.terminal_state != "terminated"
        {
            bail!("external terminal observation contradicts its occurrence");
        }
        validate_sha256(
            "external terminal observation",
            &self.provider_observation_digest,
        )
    }
}

impl ExternalAllocationOccurrence {
    fn validate(&self, reservation: &ExternalAllocationReservation) -> Result<()> {
        if self.schema != 1
            || self.binding_hash != reservation.binding_hash
            || self.request_digest != reservation.request_digest
        {
            bail!("external occurrence contradicts its reserved authority");
        }
        validate_bounded_runtime_text("external occurrence", &self.occurrence_id, 512)?;
        validate_sha256(
            "external occurrence observation",
            &self.provider_observation_digest,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalAllocationPhase {
    Reserved,
    ContactPending,
    Bound,
    Quarantined,
    NoContact,
    ContactedNoOccurrence,
    Terminated,
}

impl ExternalAllocationPhase {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "reserved" => Ok(Self::Reserved),
            "contact_pending" => Ok(Self::ContactPending),
            "bound" => Ok(Self::Bound),
            "quarantined" => Ok(Self::Quarantined),
            "no_contact" => Ok(Self::NoContact),
            "contacted_no_occurrence" => Ok(Self::ContactedNoOccurrence),
            "terminated" => Ok(Self::Terminated),
            _ => bail!("external allocation phase is not current"),
        }
    }

    pub(crate) fn is_settled(self) -> bool {
        matches!(
            self,
            Self::NoContact | Self::ContactedNoOccurrence | Self::Terminated
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalAllocationRecord {
    pub reservation: ExternalAllocationReservation,
    pub phase: ExternalAllocationPhase,
    pub occurrence: Option<ExternalAllocationOccurrence>,
}

/// Exact result of the durable allocator-contact claim.  A caller may contact
/// the allocator only when it receives `Contact`; every other result carries
/// the current journal row and is observation/cleanup authority only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExternalAllocationContactClaim {
    Contact(ExternalAllocationRecord),
    Reconcile(ExternalAllocationRecord),
    Settled(ExternalAllocationRecord),
}

fn read(conn: &Connection, placement: &str) -> Result<Option<ExternalAllocationRecord>> {
    let raw: Option<(String, String, String, Option<String>)> = conn
        .query_row(
            "SELECT capacity_owner,reservation_json,phase,occurrence_json
         FROM external_execution_allocation WHERE placement_thread_id=?1",
            [placement],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    raw.map(|(capacity, reservation_json, phase, occurrence_json)| {
        if reservation_json.len() > MAX_EXTERNAL_ALLOCATION_RESERVATION_BYTES
            || occurrence_json.as_ref().is_some_and(|raw| raw.len() > 8192)
        {
            bail!("external allocation record exceeds its bound");
        }
        let reservation: ExternalAllocationReservation = serde_json::from_str(&reservation_json)?;
        reservation.validate()?;
        if reservation.placement_thread_id != placement
            || reservation.capacity_owner != capacity
            || lillux::canonical_json(&serde_json::to_value(&reservation)?)? != reservation_json
        {
            bail!("external allocation row contradicts its canonical reservation");
        }
        let phase = ExternalAllocationPhase::parse(&phase)?;
        let occurrence = occurrence_json
            .map(|raw| {
                let occurrence: ExternalAllocationOccurrence = serde_json::from_str(&raw)?;
                occurrence.validate(&reservation)?;
                if lillux::canonical_json(&serde_json::to_value(&occurrence)?)? != raw {
                    bail!("external occurrence is not canonical");
                }
                Ok::<_, anyhow::Error>(occurrence)
            })
            .transpose()?;
        match (phase, occurrence.is_some()) {
            (ExternalAllocationPhase::Bound, false)
            | (ExternalAllocationPhase::Terminated, false)
            | (
                ExternalAllocationPhase::Reserved
                | ExternalAllocationPhase::ContactPending
                | ExternalAllocationPhase::NoContact
                | ExternalAllocationPhase::ContactedNoOccurrence,
                true,
            ) => {
                bail!("external occurrence contradicts its phase");
            }
            _ => {}
        }
        Ok(ExternalAllocationRecord {
            reservation,
            phase,
            occurrence,
        })
    })
    .transpose()
}

fn read_canonical_evidence<T: serde::de::DeserializeOwned + Serialize>(
    conn: &Connection,
    table: &str,
    column: &str,
    placement: &str,
) -> Result<Option<T>> {
    // Table and column are private fixed literals at every call site.
    let sql = format!("SELECT {column} FROM {table} WHERE placement_thread_id=?1");
    let raw: Option<String> = conn
        .query_row(&sql, [placement], |row| row.get(0))
        .optional()?;
    raw.map(|raw| {
        if raw.len() > 8192 {
            bail!("external lifecycle evidence exceeds its bound");
        }
        let value: T = serde_json::from_str(&raw)?;
        if lillux::canonical_json(&serde_json::to_value(&value)?)? != raw {
            bail!("external lifecycle evidence is not canonical");
        }
        Ok(value)
    })
    .transpose()
}

fn validate_lifecycle_evidence(conn: &Connection, record: &ExternalAllocationRecord) -> Result<()> {
    let placement = &record.reservation.placement_thread_id;
    let no_occurrence: Option<ExternalNoOccurrenceEvidence> = read_canonical_evidence(
        conn,
        "external_execution_no_occurrence",
        "evidence_json",
        placement,
    )?;
    let termination: Option<ExternalTerminationIntent> = read_canonical_evidence(
        conn,
        "external_execution_termination_intent",
        "intent_json",
        placement,
    )?;
    let terminal: Option<ExternalTerminalObservation> = read_canonical_evidence(
        conn,
        "external_execution_terminal_observation",
        "observation_json",
        placement,
    )?;
    let activation: Option<ExternalSupervisorActivationIntent> = read_canonical_evidence(
        conn,
        "external_execution_supervisor_activation_intent",
        "intent_json",
        placement,
    )?;
    let activation_observation: Option<ExternalSupervisorActivationObservation> =
        read_canonical_evidence(
            conn,
            "external_execution_supervisor_activation_observation",
            "observation_json",
            placement,
        )?;
    if let Some(evidence) = &no_occurrence {
        evidence.validate(&record.reservation)?;
    }
    if let Some(intent) = &termination {
        intent.validate(
            &record.reservation,
            record
                .occurrence
                .as_ref()
                .context("external termination intent has no occurrence")?,
        )?;
    }
    if let Some(intent) = &activation {
        intent.validate(
            &record.reservation,
            record
                .occurrence
                .as_ref()
                .context("external supervisor activation intent has no occurrence")?,
        )?;
    }
    if let Some(observation) = &activation_observation {
        observation.validate(
            &record.reservation,
            record
                .occurrence
                .as_ref()
                .context("external supervisor activation observation has no occurrence")?,
            activation
                .as_ref()
                .context("external supervisor activation observation has no intent")?,
        )?;
    }
    if let Some(observation) = &terminal {
        observation.validate(
            &record.reservation,
            record
                .occurrence
                .as_ref()
                .context("external terminal evidence has no occurrence")?,
            termination
                .as_ref()
                .context("external terminal evidence has no termination intent")?,
        )?;
    }
    if termination.is_some()
        && !matches!(
            record.phase,
            ExternalAllocationPhase::Quarantined | ExternalAllocationPhase::Terminated
        )
    {
        bail!("external termination intent did not fence execution");
    }
    if activation.is_some()
        && !matches!(
            record.phase,
            ExternalAllocationPhase::Bound
                | ExternalAllocationPhase::Quarantined
                | ExternalAllocationPhase::Terminated
        )
    {
        bail!("external supervisor activation has no retained bound occurrence");
    }
    match record.phase {
        ExternalAllocationPhase::ContactedNoOccurrence => ensure!(
            no_occurrence.is_some()
                && activation.is_none()
                && activation_observation.is_none()
                && termination.is_none()
                && terminal.is_none(),
            "settled no-occurrence allocation lacks its exact evidence"
        ),
        ExternalAllocationPhase::Terminated => ensure!(
            no_occurrence.is_none() && termination.is_some() && terminal.is_some(),
            "settled terminal allocation lacks its exact evidence"
        ),
        _ => ensure!(
            no_occurrence.is_none() && terminal.is_none(),
            "unsettled allocation retained contradictory settlement evidence"
        ),
    }
    Ok(())
}

/// Stable, independently readable reset guard. Never decode version-specific
/// allocation/session rows to infer that remote obligations disappeared.
pub(super) fn read_guard(conn: &Connection) -> Result<i64> {
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='external_execution_guard'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if sql.as_deref() != Some(GUARD_SQL) {
        bail!("external execution guard is absent or has an unsupported contract");
    }
    let count: i64 =
        conn.query_row("SELECT COUNT(*) FROM external_execution_guard", [], |row| {
            row.get(0)
        })?;
    let (version, unsettled): (i64, i64) = conn.query_row(
        "SELECT schema_version, unsettled FROM external_execution_guard WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if count != 1 || version != 1 || unsettled < 0 {
        bail!("external execution guard is malformed");
    }
    Ok(unsettled)
}

pub(super) fn ensure_resettable(conn: &Connection, epoch: u32) -> Result<()> {
    if epoch >= FIRST_EPOCH && read_guard(conn)? != 0 {
        bail!(
            "execution-history reset retains unsettled external execution; local host death and elapsed TTL are not remote cleanup proof"
        );
    }
    Ok(())
}

fn read_retained_binding(
    conn: &Connection,
    binding_hash: &str,
) -> Result<
    Option<crate::node_config::sections::external_execution::RetainedExternalExecutionBinding>,
> {
    validate_sha256("external binding", binding_hash)?;
    let raw: Option<(String, String)> = conn
        .query_row(
            "SELECT capacity_owner,binding_json
             FROM external_execution_binding_generation WHERE binding_hash=?1",
            [binding_hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    raw.map(|(capacity_owner, binding_json)| {
        let retained: crate::node_config::sections::external_execution::RetainedExternalExecutionBinding =
            serde_json::from_str(&binding_json)
                .context("decode retained external binding generation")?;
        retained.validate()?;
        if retained.digest() != binding_hash
            || retained.capacity_owner() != capacity_owner
            || retained.canonical_json()? != binding_json
        {
            bail!("retained external binding generation changed");
        }
        Ok(retained)
    })
    .transpose()
}

fn require_supervisor_activation_allows_channel(
    conn: &Connection,
    record: &ExternalAllocationRecord,
    binding: &ryeos_state::external_execution::ExecutionChannelBinding,
    retained: &crate::node_config::sections::external_execution::RetainedExternalExecutionBinding,
) -> Result<()> {
    let occurrence = record
        .occurrence
        .as_ref()
        .context("external channel activation has no occurrence")?;
    let intent: ExternalSupervisorActivationIntent = read_canonical_evidence(
        conn,
        "external_execution_supervisor_activation_intent",
        "intent_json",
        &record.reservation.placement_thread_id,
    )?
    .context("external channel has no durable supervisor activation intent")?;
    intent.validate(&record.reservation, occurrence)?;
    intent.validate_contract(
        &record.reservation,
        occurrence,
        &retained.backend_contract(),
    )?;
    ensure!(
        binding.supervisor_runtime_hash == intent.supervisor_runtime_hash,
        "external channel changed its activated supervisor runtime"
    );
    if let Some(observation) = read_canonical_evidence::<ExternalSupervisorActivationObservation>(
        conn,
        "external_execution_supervisor_activation_observation",
        "observation_json",
        &record.reservation.placement_thread_id,
    )? {
        observation.validate(&record.reservation, occurrence, &intent)?;
        ensure!(
            observation.activation_state == "started",
            "external channel cannot attach after supervisor non-start evidence"
        );
    }
    Ok(())
}

pub(super) fn validate_current(conn: &Connection) -> Result<()> {
    let mut bindings = conn.prepare("SELECT binding_hash,capacity_owner,binding_json FROM external_execution_binding_generation")?;
    for row in bindings.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (digest, capacity, json) = row?;
        let retained: crate::node_config::sections::external_execution::RetainedExternalExecutionBinding =
            serde_json::from_str(&json).context("decode retained external binding generation")?;
        retained.validate()?;
        if retained.digest() != digest
            || retained.capacity_owner() != capacity
            || retained.canonical_json()? != json
        {
            bail!("retained external binding generation changed");
        }
    }
    let unsettled: i64 = conn.query_row(
        "SELECT COUNT(*) FROM external_execution_allocation
         WHERE phase NOT IN ('no_contact','contacted_no_occurrence','terminated')",
        [],
        |row| row.get(0),
    )?;
    if read_guard(conn)? != unsettled {
        bail!("external execution guard contradicts its allocation journal");
    }
    let mut statement =
        conn.prepare("SELECT placement_thread_id FROM external_execution_allocation")?;
    let placements = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for placement in placements {
        let record = read(conn, &placement)?.context("external allocation disappeared")?;
        if !record.phase.is_settled() {
            require_retained_owner(conn, &record.reservation)?;
        }
        let retained = read_retained_binding(conn, &record.reservation.binding_hash)?
            .context("external allocation lost its exact retained binding generation")?;
        if retained.capacity_owner() != record.reservation.capacity_owner {
            bail!("external allocation contradicts its retained binding generation");
        }
        retained.check_reservation_limits(
            record.reservation.max_active,
            record.reservation.timeout_seconds,
        )?;
        if let Some(intent) = read_canonical_evidence::<ExternalSupervisorActivationIntent>(
            conn,
            "external_execution_supervisor_activation_intent",
            "intent_json",
            &placement,
        )? {
            intent.validate_contract(
                &record.reservation,
                record
                    .occurrence
                    .as_ref()
                    .context("external supervisor activation lost its occurrence")?,
                &retained.backend_contract(),
            )?;
        }
        validate_lifecycle_evidence(conn, &record)?;
    }
    channel::validate_channels(conn)?;
    connector::validate_connectors(conn)?;
    Ok(())
}

fn require_session_owner(
    conn: &Connection,
    reservation: &ExternalAllocationReservation,
) -> Result<()> {
    let owner = reservation.owner.dedicated_session()?;
    let matched: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM dedicated_session s JOIN credential_profile p
           ON p.profile_id=s.credential_profile_id
         JOIN execution_workspace w ON w.workspace_id=s.workspace_id
         JOIN thread_launch_claim c ON c.thread_id=s.placement_thread_id
         WHERE s.placement_thread_id=?1 AND s.admitted_capsule_hash=?2 AND s.workspace_id=?3
           AND s.worker_instance_id=?4 AND s.worker_boot_epoch=?5
           AND w.thread_id=s.placement_thread_id AND w.base_snapshot=?6
           AND w.launch_owner=c.claimed_by
           AND p.credential_generation=s.credential_generation
           AND p.lock_owner=s.worker_instance_id)",
        params![
            reservation.placement_thread_id,
            reservation.admitted_capsule_hash,
            owner.workspace_id,
            owner.worker_instance_id,
            i64::try_from(owner.worker_boot_epoch)?,
            reservation.base_snapshot_hash
        ],
        |row| row.get(0),
    )?;
    if !matched {
        bail!("external allocation has no exact locked dedicated-session owner");
    }
    Ok(())
}

fn require_retained_owner(
    conn: &Connection,
    reservation: &ExternalAllocationReservation,
) -> Result<()> {
    match &reservation.owner {
        ExternalAllocationOwner::DedicatedSession(_) => require_session_owner(conn, reservation),
        ExternalAllocationOwner::DirectThread { chain_root_id, .. } => {
            // Original owner remains in the immutable reservation. Claim rotation
            // or cancellation cannot erase an occurrence's cleanup obligation.
            let matched: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM thread_runtime WHERE thread_id=?1 AND chain_root_id=?2)",
                params![reservation.placement_thread_id, chain_root_id], |row| row.get(0))?;
            ensure!(
                matched,
                "external direct allocation lost its retained thread owner"
            );
            Ok(())
        }
    }
}

fn require_contactable_direct_owner(
    conn: &Connection,
    reservation: &ExternalAllocationReservation,
) -> Result<()> {
    ensure!(
        direct_owner_is_contactable(conn, reservation)?,
        "external direct allocation has no exact current unstopped launch owner"
    );
    Ok(())
}

/// A negative authority observation is distinct from a corrupt reservation or
/// failed database read. Transport may suppress executable backlog on the former
/// without discarding retained evidence or hiding the latter.
fn direct_owner_is_contactable(
    conn: &Connection,
    reservation: &ExternalAllocationReservation,
) -> Result<bool> {
    let ExternalAllocationOwner::DirectThread {
        chain_root_id,
        launch_owner,
        ..
    } = &reservation.owner
    else {
        bail!("external direct admission requires a direct thread owner");
    };
    reservation.validate()?;
    let canonical_owner = lillux::canonical_json(&serde_json::to_value(launch_owner)?)?;
    let matched: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM thread_runtime r
           JOIN thread_launch_claim c ON c.thread_id=r.thread_id
           JOIN thread_launch_epoch e ON e.thread_id=r.thread_id
         WHERE r.thread_id=?1 AND r.chain_root_id=?2 AND r.stop_requested_at_ms IS NULL
           AND c.claimed_by=?3 AND c.claim_id=?4 AND e.last_epoch=?5)",
        params![
            reservation.placement_thread_id,
            chain_root_id,
            canonical_owner,
            launch_owner.unpredictable_nonce,
            i64::try_from(launch_owner.monotonic_launch_epoch)?
        ],
        |row| row.get(0),
    )?;
    Ok(matched)
}

fn require_new_owner(
    conn: &Connection,
    reservation: &ExternalAllocationReservation,
    proof: Option<&crate::state_store::VerifiedExternalDirectOwner>,
) -> Result<()> {
    match &reservation.owner {
        ExternalAllocationOwner::DedicatedSession(_) => {
            ensure!(
                proof.is_none(),
                "session allocation cannot consume direct thread proof"
            );
            require_session_owner(conn, reservation)?;
            require_contactable_session(conn, &reservation.placement_thread_id)
        }
        ExternalAllocationOwner::DirectThread { .. } => {
            ensure!(
                proof.is_some_and(|proof| proof.matches(reservation)),
                "external direct allocation requires verified born capsule authority"
            );
            require_contactable_direct_owner(conn, reservation)
        }
    }
}

/// Require the exact still-admitted session and its current root workspace.
///
/// `ready` is the pre-process boundary used by direct recovery fixtures.
/// `active` is the ordinary public WorkerExecution boundary: its managed
/// runtime has attached before the dedicated structured-session worker asks
/// the external provider for an occurrence. Both states retain the exact
/// launch owner; freezing and every later state close new provider contact.
fn require_contactable_session(conn: &Connection, placement: &str) -> Result<()> {
    let admitted: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM dedicated_session s
            JOIN execution_workspace w ON w.workspace_id=s.workspace_id
            JOIN thread_launch_claim c ON c.thread_id=s.placement_thread_id
          WHERE s.placement_thread_id=?1 AND s.state='admitted' AND s.send_boundary='none'
            AND w.thread_id=s.placement_thread_id
            AND w.launch_owner=c.claimed_by AND w.state IN ('ready','active'))",
        [placement],
        |row| row.get(0),
    )?;
    if !admitted {
        bail!(
            "external allocation requires an unreleased admitted session and contactable workspace"
        );
    }
    Ok(())
}

impl RuntimeDb {
    pub(super) fn unsettled_external_chain_count(&self, chain_root_id: &str) -> Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM external_execution_allocation a
             LEFT JOIN dedicated_session s ON s.placement_thread_id=a.placement_thread_id
             WHERE a.phase NOT IN ('no_contact','contacted_no_occurrence','terminated')
               AND ((json_extract(a.reservation_json,'$.owner.kind')='direct_thread'
                     AND json_extract(a.reservation_json,'$.owner.chain_root_id')=?1)
                 OR (json_extract(a.reservation_json,'$.owner.kind')='dedicated_session'
                     AND s.chain_root_id=?1))",
            [chain_root_id],
            |row| row.get(0),
        )?;
        u64::try_from(count).context("negative external allocation retention count")
    }

    pub fn external_allocation(&self, placement: &str) -> Result<Option<ExternalAllocationRecord>> {
        read(&self.conn, placement)
    }

    pub(crate) fn unsettled_external_allocation_placements(&self) -> Result<Vec<String>> {
        validate_current(&self.conn)?;
        let mut statement = self.conn.prepare(
            "SELECT placement_thread_id FROM external_execution_allocation
             WHERE phase NOT IN ('no_contact','contacted_no_occurrence','terminated')
             ORDER BY placement_thread_id",
        )?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub(crate) fn recoverable_external_cleanup_placements(&self) -> Result<Vec<String>> {
        validate_current(&self.conn)?;
        let mut statement = self.conn.prepare(
            "SELECT placement_thread_id FROM external_execution_allocation
             WHERE phase='quarantined' ORDER BY placement_thread_id",
        )?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub(crate) fn retained_external_binding(
        &self,
        binding_hash: &str,
    ) -> Result<
        Option<crate::node_config::sections::external_execution::RetainedExternalExecutionBinding>,
    > {
        read_retained_binding(&self.conn, binding_hash)
    }

    /// Idempotent reservation; no allocator may be contacted here. Unknown
    /// prior calls and quarantined occurrences retain capacity across restart.
    pub(crate) fn reserve_external_allocation(
        &self,
        reservation: &ExternalAllocationReservation,
        retained_binding: &crate::node_config::sections::external_execution::RetainedExternalExecutionBinding,
    ) -> Result<ExternalAllocationRecord> {
        self.reserve_external_allocation_with_verified_owner(reservation, retained_binding, None)
    }

    pub(crate) fn reserve_external_allocation_with_verified_owner(
        &self,
        reservation: &ExternalAllocationReservation,
        retained_binding: &crate::node_config::sections::external_execution::RetainedExternalExecutionBinding,
        proof: Option<&crate::state_store::VerifiedExternalDirectOwner>,
    ) -> Result<ExternalAllocationRecord> {
        reservation.validate()?;
        retained_binding.validate()?;
        retained_binding
            .check_reservation_limits(reservation.max_active, reservation.timeout_seconds)?;
        if retained_binding.digest() != reservation.binding_hash
            || retained_binding.capacity_owner() != reservation.capacity_owner
        {
            bail!("external allocation contradicts its retained binding generation");
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        validate_current(&tx)?;
        let binding_json = retained_binding.canonical_json()?;
        let prior: Option<(String, String)> = tx.query_row(
            "SELECT capacity_owner,binding_json FROM external_execution_binding_generation WHERE binding_hash=?1",
            [retained_binding.digest()], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
        match prior {
            Some((capacity, json))
                if capacity == retained_binding.capacity_owner() && json == binding_json => {}
            Some(_) => bail!("retained external binding generation replay changed"),
            None => {
                tx.execute(
                    "INSERT INTO external_execution_binding_generation VALUES(?1,?2,?3,?4)",
                    params![
                        retained_binding.digest(),
                        retained_binding.capacity_owner(),
                        binding_json,
                        i64::try_from(lillux::time::timestamp_millis())?
                    ],
                )?;
            }
        }
        if let Some(existing) = read(&tx, &reservation.placement_thread_id)? {
            if existing.reservation != *reservation {
                bail!("external allocation replay changed its exact reservation");
            }
            tx.commit()?;
            return Ok(existing);
        }
        require_new_owner(&tx, reservation, proof)?;
        if read_guard(&tx)? >= 256 {
            bail!("node external allocation ceiling reached");
        }
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        reservation.require_startup_time(now)?;
        if now >= reservation.contact_deadline_ms
            || reservation.contact_deadline_ms.saturating_sub(now) > 300_000
        {
            bail!("external allocation contact deadline has expired");
        }
        let (count, prior_limit): (i64, Option<i64>) = tx.query_row(
            "SELECT COUNT(*),MIN(json_extract(reservation_json,'$.max_active'))
             FROM external_execution_allocation WHERE capacity_owner=?1
               AND phase NOT IN ('no_contact','contacted_no_occurrence','terminated')",
            [&reservation.capacity_owner],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let limit = i64::from(reservation.max_active).min(prior_limit.unwrap_or(64));
        if count >= limit {
            bail!("external execution capacity remains reserved");
        }
        tx.execute(
            "INSERT INTO external_execution_allocation VALUES(?1,?2,?3,'reserved',NULL,?4,?4)",
            params![
                reservation.placement_thread_id,
                reservation.capacity_owner,
                lillux::canonical_json(&serde_json::to_value(reservation)?)?,
                now
            ],
        )?;
        let record =
            read(&tx, &reservation.placement_thread_id)?.context("reserved allocation missing")?;
        tx.commit()?;
        Ok(record)
    }

    /// Only the winner of this durable CAS may contact the allocator.  The
    /// returned record is read in the same transaction, so a losing caller
    /// never reconciles from the stale phase it observed before the claim.
    pub(crate) fn claim_external_allocation_contact(
        &self,
        placement: &str,
        request_digest: &str,
    ) -> Result<ExternalAllocationContactClaim> {
        self.claim_external_allocation_contact_with_verified_owner(placement, request_digest, None)
    }

    pub(crate) fn claim_external_allocation_contact_with_verified_owner(
        &self,
        placement: &str,
        request_digest: &str,
        proof: Option<&crate::state_store::VerifiedExternalDirectOwner>,
    ) -> Result<ExternalAllocationContactClaim> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, placement)?.context("external allocation was not reserved")?;
        if record.reservation.request_digest != request_digest {
            bail!("external allocation contact changed its request identity");
        }
        match record.phase {
            ExternalAllocationPhase::NoContact
            | ExternalAllocationPhase::ContactedNoOccurrence
            | ExternalAllocationPhase::Terminated => {
                tx.commit()?;
                return Ok(ExternalAllocationContactClaim::Settled(record));
            }
            _ => {}
        }
        require_retained_owner(&tx, &record.reservation)?;
        match record.phase {
            ExternalAllocationPhase::ContactPending
            | ExternalAllocationPhase::Bound
            | ExternalAllocationPhase::Quarantined => {
                tx.commit()?;
                return Ok(ExternalAllocationContactClaim::Reconcile(record));
            }
            ExternalAllocationPhase::Reserved => {}
            _ => unreachable!("settled external phase returned before owner validation"),
        }
        require_new_owner(&tx, &record.reservation, proof)?;
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        record.reservation.require_startup_time(now)?;
        if now >= record.reservation.contact_deadline_ms {
            bail!("external allocation contact deadline expired");
        }
        let changed = tx.execute(
            "UPDATE external_execution_allocation SET phase='contact_pending',updated_at_ms=?2
             WHERE placement_thread_id=?1 AND phase='reserved'",
            params![placement, now],
        )?;
        if changed != 1 {
            bail!("external allocation contact claim lost its durable CAS");
        }
        let current = read(&tx, placement)?
            .context("external allocation disappeared after its contact claim")?;
        if current.phase != ExternalAllocationPhase::ContactPending {
            bail!("external allocation contact claim did not retain its current phase");
        }
        tx.commit()?;
        Ok(ExternalAllocationContactClaim::Contact(current))
    }

    /// Bind the exact returned occurrence to the original pending contact.
    /// This is allocation observation only, not release or completion proof.
    pub(crate) fn bind_external_allocation(
        &self,
        placement: &str,
        occurrence: &ExternalAllocationOccurrence,
        timing: ExternalObservationTiming,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, placement)?.context("external allocation is absent")?;
        occurrence.validate(&record.reservation)?;
        if let Some(prior) = &record.occurrence {
            if prior != occurrence {
                bail!("external allocation occurrence changed");
            }
            fence_external_observation_timing(&tx, &record, timing)?;
            tx.commit()?;
            return Ok(());
        }
        if !matches!(
            record.phase,
            ExternalAllocationPhase::ContactPending | ExternalAllocationPhase::Quarantined
        ) {
            bail!("external occurrence arrived outside its durable contact");
        }
        let already_owned: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_execution_allocation
             WHERE placement_thread_id!=?1 AND capacity_owner=?2
               AND json_extract(occurrence_json,'$.occurrence_id')=?3)",
            params![
                placement,
                record.reservation.capacity_owner,
                occurrence.occurrence_id
            ],
            |row| row.get(0),
        )?;
        if already_owned {
            bail!("external occurrence already belongs to another placement");
        }
        // Late exact responses may identify cleanup after quarantine, but may
        // never reverse quarantine into execution permission.
        tx.execute(
            "UPDATE external_execution_allocation
             SET occurrence_json=?2,phase=CASE WHEN phase='quarantined' THEN phase ELSE 'bound' END,updated_at_ms=?3
             WHERE placement_thread_id=?1",
            params![placement, lillux::canonical_json(&serde_json::to_value(occurrence)?)?,
                i64::try_from(lillux::time::timestamp_millis())?],
        )?;
        fence_external_observation_timing(&tx, &record, timing)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn observe_external_lifecycle_pending(
        &self,
        placement: &str,
        timing: ExternalObservationTiming,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record =
            read(&tx, placement)?.context("external pending observation lost its allocation")?;
        ensure!(
            matches!(
                record.phase,
                ExternalAllocationPhase::ContactPending
                    | ExternalAllocationPhase::Bound
                    | ExternalAllocationPhase::Quarantined
            ),
            "external pending observation arrived outside contacted lifecycle"
        );
        fence_external_observation_timing(&tx, &record, timing)?;
        tx.commit()?;
        Ok(())
    }

    /// Settle a contacted allocation only from exact adapter testimony that
    /// the original request produced no occurrence. This is never inferred
    /// from timeout, list results, 404, or a locally absent process.
    pub(crate) fn settle_external_no_occurrence(
        &self,
        placement: &str,
        evidence: &ExternalNoOccurrenceEvidence,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, placement)?.context("external allocation is absent")?;
        evidence.validate(&record.reservation)?;
        if record.phase == ExternalAllocationPhase::ContactedNoOccurrence {
            let prior: Option<ExternalNoOccurrenceEvidence> = read_canonical_evidence(
                &tx,
                "external_execution_no_occurrence",
                "evidence_json",
                placement,
            )?;
            ensure!(
                prior.as_ref() == Some(evidence),
                "external no-occurrence evidence changed"
            );
            tx.commit()?;
            return Ok(());
        }
        ensure!(
            matches!(
                record.phase,
                ExternalAllocationPhase::ContactPending | ExternalAllocationPhase::Quarantined
            ) && record.occurrence.is_none(),
            "external no-occurrence evidence arrived outside unresolved contact"
        );
        tx.execute(
            "INSERT INTO external_execution_no_occurrence VALUES(?1,?2)",
            params![
                placement,
                lillux::canonical_json(&serde_json::to_value(evidence)?)?
            ],
        )?;
        tx.execute(
            "UPDATE external_execution_allocation
             SET phase='contacted_no_occurrence',updated_at_ms=?2
             WHERE placement_thread_id=?1",
            params![placement, i64::try_from(lillux::time::timestamp_millis())?],
        )?;
        let settled = read(&tx, placement)?.context("settled allocation disappeared")?;
        validate_lifecycle_evidence(&tx, &settled)?;
        tx.commit()?;
        Ok(())
    }

    /// Persist the exact supervisor-start mutation before adapter contact.
    /// `true` is the sole permission to issue it; an exact replay receives
    /// reconciliation authority only and must never start another supervisor.
    pub(crate) fn begin_external_supervisor_activation(
        &self,
        placement: &str,
        intent: &ExternalSupervisorActivationIntent,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, placement)?.context("external allocation is absent")?;
        let occurrence = record
            .occurrence
            .as_ref()
            .context("external supervisor activation requires an exact occurrence")?;
        intent.validate(&record.reservation, occurrence)?;
        let retained = read_retained_binding(&tx, &record.reservation.binding_hash)?
            .context("external supervisor activation lost its binding generation")?;
        intent.validate_contract(
            &record.reservation,
            occurrence,
            &retained.backend_contract(),
        )?;
        let prior: Option<ExternalSupervisorActivationIntent> = read_canonical_evidence(
            &tx,
            "external_execution_supervisor_activation_intent",
            "intent_json",
            placement,
        )?;
        if let Some(prior) = prior {
            ensure!(
                prior == *intent,
                "external supervisor activation intent changed"
            );
            tx.commit()?;
            return Ok(false);
        }
        ensure!(
            record.phase == ExternalAllocationPhase::Bound,
            "new external supervisor activation requires an executable bound occurrence"
        );
        channel::require_current_execution_owner(&tx, &record.reservation)?;
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        record.reservation.require_startup_time(now)?;
        ensure!(
            now < intent.attachment_deadline_ms,
            "external supervisor activation deadline expired before contact"
        );
        tx.execute(
            "INSERT INTO external_execution_supervisor_activation_intent VALUES(?1,?2)",
            params![
                placement,
                lillux::canonical_json(&serde_json::to_value(intent)?)?
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn external_supervisor_activation(
        &self,
        placement: &str,
    ) -> Result<Option<ExternalSupervisorActivationRecord>> {
        let record = read(&self.conn, placement)?.context("external allocation is absent")?;
        let Some(intent) = read_canonical_evidence::<ExternalSupervisorActivationIntent>(
            &self.conn,
            "external_execution_supervisor_activation_intent",
            "intent_json",
            placement,
        )?
        else {
            return Ok(None);
        };
        let occurrence = record
            .occurrence
            .as_ref()
            .context("external supervisor activation has no occurrence")?;
        intent.validate(&record.reservation, occurrence)?;
        let retained = read_retained_binding(&self.conn, &record.reservation.binding_hash)?
            .context("external supervisor activation lost its binding generation")?;
        intent.validate_contract(
            &record.reservation,
            occurrence,
            &retained.backend_contract(),
        )?;
        let observation = read_canonical_evidence::<ExternalSupervisorActivationObservation>(
            &self.conn,
            "external_execution_supervisor_activation_observation",
            "observation_json",
            placement,
        )?;
        if let Some(observation) = &observation {
            observation.validate(&record.reservation, occurrence, &intent)?;
        }
        Ok(Some(ExternalSupervisorActivationRecord {
            intent,
            observation,
        }))
    }

    pub(crate) fn settle_external_supervisor_activation(
        &self,
        placement: &str,
        observation: &ExternalSupervisorActivationObservation,
        timing: ExternalObservationTiming,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, placement)?.context("external allocation is absent")?;
        let occurrence = record
            .occurrence
            .as_ref()
            .context("external supervisor activation observation has no occurrence")?;
        let intent: ExternalSupervisorActivationIntent = read_canonical_evidence(
            &tx,
            "external_execution_supervisor_activation_intent",
            "intent_json",
            placement,
        )?
        .context("external supervisor activation observation has no durable intent")?;
        let retained = read_retained_binding(&tx, &record.reservation.binding_hash)?
            .context("external supervisor activation lost its binding generation")?;
        intent.validate_contract(
            &record.reservation,
            occurrence,
            &retained.backend_contract(),
        )?;
        observation.validate(&record.reservation, occurrence, &intent)?;
        ensure!(
            matches!(
                record.phase,
                ExternalAllocationPhase::Bound | ExternalAllocationPhase::Quarantined
            ),
            "external supervisor activation observation arrived after occurrence settlement"
        );
        let prior: Option<ExternalSupervisorActivationObservation> = read_canonical_evidence(
            &tx,
            "external_execution_supervisor_activation_observation",
            "observation_json",
            placement,
        )?;
        if let Some(prior) = prior {
            ensure!(
                prior == *observation,
                "external supervisor activation observation changed"
            );
            fence_external_observation_timing(&tx, &record, timing)?;
            tx.commit()?;
            return Ok(());
        }
        if observation.activation_state == "not_started" {
            let attached: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM external_execution_channel WHERE placement_thread_id=?1)",
                [placement],
                |row| row.get(0),
            )?;
            ensure!(
                !attached,
                "external supervisor cannot be observed not started after channel attachment"
            );
        }
        tx.execute(
            "INSERT INTO external_execution_supervisor_activation_observation VALUES(?1,?2)",
            params![
                placement,
                lillux::canonical_json(&serde_json::to_value(observation)?)?
            ],
        )?;
        fence_external_observation_timing(&tx, &record, timing)?;
        tx.commit()?;
        Ok(())
    }

    /// Persist the exact termination mutation intent. `true` is the unique
    /// process-local permission to issue that request; `false` is recovery
    /// authority only and must use observation/reconciliation.
    pub(crate) fn begin_external_termination(
        &self,
        placement: &str,
        intent: &ExternalTerminationIntent,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, placement)?.context("external allocation is absent")?;
        let occurrence = record
            .occurrence
            .as_ref()
            .context("external termination requires an exact occurrence")?;
        intent.validate(&record.reservation, occurrence)?;
        ensure!(
            matches!(
                record.phase,
                ExternalAllocationPhase::Bound
                    | ExternalAllocationPhase::Quarantined
                    | ExternalAllocationPhase::Terminated
            ),
            "external termination intent arrived outside a bound occurrence"
        );
        let prior: Option<ExternalTerminationIntent> = read_canonical_evidence(
            &tx,
            "external_execution_termination_intent",
            "intent_json",
            placement,
        )?;
        if let Some(prior) = prior {
            ensure!(prior == *intent, "external termination intent changed");
            tx.commit()?;
            return Ok(false);
        }
        ensure!(
            record.phase != ExternalAllocationPhase::Terminated,
            "settled external occurrence cannot gain a new termination intent"
        );
        let channel: Option<String> = tx
            .query_row(
                "SELECT binding_digest FROM external_execution_channel
                 WHERE placement_thread_id=?1",
                [placement],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(binding_digest) = channel {
            let revoked: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM external_execution_revocation
                 WHERE binding_digest=?1)",
                [binding_digest],
                |row| row.get(0),
            )?;
            if !revoked {
                ensure!(
                    record.phase == ExternalAllocationPhase::Bound,
                    "quarantined external execution cannot acquire a new normal settlement intent"
                );
                ensure!(
                    matches!(
                        record.reservation.owner,
                        ExternalAllocationOwner::DirectThread { .. }
                    ),
                    "external termination requires durable channel revocation"
                );
                require_contactable_direct_owner(&tx, &record.reservation)?;
                ensure!(
                    channel::complete_applied_direct_output_tx(&tx, placement)?.is_some(),
                    "normal external termination requires complete applied target observations"
                );
            }
        }
        tx.execute(
            "INSERT INTO external_execution_termination_intent VALUES(?1,?2)",
            params![
                placement,
                lillux::canonical_json(&serde_json::to_value(intent)?)?
            ],
        )?;
        if record.phase == ExternalAllocationPhase::Bound {
            tx.execute(
                "UPDATE external_execution_allocation
                 SET phase='quarantined',updated_at_ms=?2 WHERE placement_thread_id=?1",
                params![placement, i64::try_from(lillux::time::timestamp_millis())?],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Release external capacity only after exact terminal testimony joins the
    /// retained occurrence and the controller-authored termination intent.
    pub(crate) fn settle_external_terminal(
        &self,
        placement: &str,
        observation: &ExternalTerminalObservation,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, placement)?.context("external allocation is absent")?;
        let occurrence = record
            .occurrence
            .as_ref()
            .context("external terminal observation has no occurrence")?;
        let intent: ExternalTerminationIntent = read_canonical_evidence(
            &tx,
            "external_execution_termination_intent",
            "intent_json",
            placement,
        )?
        .context("external terminal observation has no durable termination intent")?;
        observation.validate(&record.reservation, occurrence, &intent)?;
        if record.phase == ExternalAllocationPhase::Terminated {
            let prior: Option<ExternalTerminalObservation> = read_canonical_evidence(
                &tx,
                "external_execution_terminal_observation",
                "observation_json",
                placement,
            )?;
            ensure!(
                prior.as_ref() == Some(observation),
                "external terminal observation changed"
            );
            tx.commit()?;
            return Ok(());
        }
        ensure!(
            matches!(
                record.phase,
                ExternalAllocationPhase::Bound | ExternalAllocationPhase::Quarantined
            ),
            "external terminal observation arrived outside a bound occurrence"
        );
        tx.execute(
            "INSERT INTO external_execution_terminal_observation VALUES(?1,?2)",
            params![
                placement,
                lillux::canonical_json(&serde_json::to_value(observation)?)?
            ],
        )?;
        tx.execute(
            "UPDATE external_execution_allocation
             SET phase='terminated',updated_at_ms=?2 WHERE placement_thread_id=?1",
            params![placement, i64::try_from(lillux::time::timestamp_millis())?],
        )?;
        let settled = read(&tx, placement)?.context("settled allocation disappeared")?;
        validate_lifecycle_evidence(&tx, &settled)?;
        tx.commit()?;
        Ok(())
    }

    /// Cancel without external contact, or conservatively quarantine a call
    /// that may have been accepted. No contacted phase can become no-contact.
    pub fn cancel_external_allocation(&self, placement: &str) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, placement)?.context("external allocation is absent")?;
        if record.phase.is_settled() {
            return Ok(());
        }
        tx.execute(
            "UPDATE external_execution_allocation SET phase=?2,updated_at_ms=?3 WHERE placement_thread_id=?1",
            params![placement, if record.phase == ExternalAllocationPhase::Reserved {"no_contact"} else {"quarantined"},
                i64::try_from(lillux::time::timestamp_millis())?],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn external_execution_cas_roots(&self) -> Result<Vec<String>> {
        validate_current(&self.conn)?;
        let mut statement = self.conn.prepare(
            "SELECT placement_thread_id FROM external_execution_allocation
             WHERE phase NOT IN ('no_contact','contacted_no_occurrence','terminated')",
        )?;
        let placements = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut roots = BTreeSet::new();
        for placement in placements {
            let record =
                read(&self.conn, &placement)?.context("external allocation root is absent")?;
            roots.insert(record.reservation.admitted_capsule_hash);
            roots.insert(record.reservation.base_snapshot_hash);
        }
        let mut statement = self
            .conn
            .prepare("SELECT snapshot_hash FROM external_execution_import")?;
        for root in statement.query_map([], |row| row.get::<_, String>(0))? {
            roots.insert(root?);
        }
        Ok(roots.into_iter().collect())
    }

    pub fn external_execution_blob_roots(&self) -> Result<Vec<String>> {
        validate_current(&self.conn)?;
        let mut statement = self
            .conn
            .prepare("SELECT evidence_blob_hash FROM external_execution_import")?;
        Ok(statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    /// Storage-owner fixture only: deliberately does not claim an admitted
    /// program, authoritative birth, or permission to contact an adapter.
    pub(crate) fn direct_owner_reservation(db: &RuntimeDb) -> ExternalAllocationReservation {
        let placement = "T-direct-owner";
        db.insert_thread_runtime(placement, placement).unwrap();
        db.claim_thread_launch(placement, "direct-claim", "daemon:direct-owner")
            .unwrap();
        let launch_owner = db.get_launch_claim(placement).unwrap().unwrap().owner;
        let binding = crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
        let now = i64::try_from(lillux::time::timestamp_millis()).unwrap();
        ExternalAllocationReservation {
            schema: EXTERNAL_ALLOCATION_RESERVATION_SCHEMA,
            placement_thread_id: placement.into(),
            admitted_capsule_hash: "a".repeat(64),
            owner: ExternalAllocationOwner::DirectThread {
                chain_root_id: placement.into(),
                launch_owner,
                program: crate::thread_lifecycle::external_direct_program_test_fixture(),
            },
            base_snapshot_hash: "b".repeat(64),
            binding_hash: binding.digest().into(),
            capacity_owner: binding.capacity_owner().into(),
            channel_authority_generation: "3".repeat(64),
            channel_owner_public_key: ryeos_state::external_execution::encode_channel_public_key(
                &lillux::crypto::SigningKey::from_bytes(&[19; 32]).verifying_key(),
            )
            .unwrap(),
            channel_bootstrap_capability_hash: "4".repeat(64),
            request_digest: "e".repeat(64),
            max_active: 1,
            timeout_seconds: 60,
            contact_deadline_ms: now + 30_000,
            startup_started_at_ms: now,
            startup_deadline_ms: now + 60_000,
        }
    }

    #[test]
    fn direct_program_retention_is_bounded_and_exact_across_database_reopen() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let mut reservation = direct_owner_reservation(&db);
        let ExternalAllocationOwner::DirectThread { program, .. } = &mut reservation.owner else {
            unreachable!()
        };
        let mut wire = serde_json::to_value(&*program).unwrap();
        wire["projection"]["arguments"] = serde_json::json!(["a".repeat(12_000)]);
        *program = serde_json::from_value(wire).unwrap();
        reservation.validate().unwrap();
        let encoded = lillux::canonical_json(&serde_json::to_value(&reservation).unwrap()).unwrap();
        assert!(encoded.len() > 8192 && encoded.len() < MAX_EXTERNAL_ALLOCATION_RESERVATION_BYTES);
        // Storage-only fixture, deliberately not a born compiler admission.
        retain_direct_owner_fixture(&db, &reservation, "contact_pending");
        drop(db);
        let reopened = RuntimeDb::open(&path).unwrap();
        assert_eq!(
            reopened
                .external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap()
                .reservation,
            reservation
        );
        let mut predecessor = reservation.clone();
        predecessor.schema = 4;
        assert!(predecessor.validate().is_err());
        let mut oversized = serde_json::to_value(&reservation).unwrap();
        oversized["owner"]["program"]["projection"]["arguments"] =
            serde_json::json!(["z".repeat(MAX_EXTERNAL_ALLOCATION_RESERVATION_BYTES)]);
        assert!(
            serde_json::from_value::<ExternalAllocationReservation>(oversized)
                .unwrap()
                .validate()
                .is_err()
        );
    }

    /// Model a retained storage boundary, not production admission. New direct
    /// admission remains closed until an ordinary program binding is installed.
    pub(crate) fn retain_direct_owner_fixture(
        db: &RuntimeDb,
        reservation: &ExternalAllocationReservation,
        phase: &str,
    ) {
        let binding = crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
        db.conn
            .execute(
                "INSERT INTO external_execution_binding_generation VALUES(?1,?2,?3,1)",
                params![
                    binding.digest(),
                    binding.capacity_owner(),
                    binding.canonical_json().unwrap()
                ],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO external_execution_allocation VALUES(?1,?2,?3,?4,NULL,1,1)",
                params![
                    reservation.placement_thread_id,
                    reservation.capacity_owner,
                    lillux::canonical_json(&serde_json::to_value(reservation).unwrap()).unwrap(),
                    phase
                ],
            )
            .unwrap();
    }

    #[test]
    fn direct_owner_is_closed_and_not_a_synthetic_session() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let reservation = direct_owner_reservation(&db);
        reservation.validate().unwrap();
        require_contactable_direct_owner(&db.conn, &reservation).unwrap();
        for table in [
            "dedicated_session",
            "execution_workspace",
            "credential_profile",
        ] {
            let count: i64 = db
                .conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0);
        }
        let value = serde_json::to_value(&reservation).unwrap();
        for mutation in ["missing", "mixed", "legacy"] {
            let mut changed = value.clone();
            match mutation {
                "missing" => {
                    changed.as_object_mut().unwrap().remove("owner");
                }
                "mixed" => changed["owner"]["workspace_id"] = "W-fake".into(),
                "legacy" => changed["worker_boot_epoch"] = 1.into(),
                _ => unreachable!(),
            }
            assert!(serde_json::from_value::<ExternalAllocationReservation>(changed).is_err());
        }
        assert!(
            reserve(&db, &reservation)
                .unwrap_err()
                .to_string()
                .contains("verified born capsule")
        );
        assert!(
            db.external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(read_guard(&db.conn).unwrap(), 0);
    }

    #[test]
    fn direct_contact_checks_exact_claim_chain_epoch_and_stop() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let original = direct_owner_reservation(&db);
        assert!(direct_owner_is_contactable(&db.conn, &original).unwrap());
        for mutation in ["chain", "nonce", "epoch", "daemon"] {
            let mut changed = original.clone();
            let ExternalAllocationOwner::DirectThread {
                chain_root_id,
                launch_owner,
                ..
            } = &mut changed.owner
            else {
                unreachable!()
            };
            match mutation {
                "chain" => *chain_root_id = "T-other".into(),
                "nonce" => launch_owner.unpredictable_nonce = "other".into(),
                "epoch" => launch_owner.monotonic_launch_epoch += 1,
                "daemon" => launch_owner.daemon_generation_id = "daemon:other".into(),
                _ => unreachable!(),
            }
            assert!(require_contactable_direct_owner(&db.conn, &changed).is_err());
            assert!(!direct_owner_is_contactable(&db.conn, &changed).unwrap());
        }
        db.request_thread_stop(&original.placement_thread_id, StopIntent::Cancel)
            .unwrap();
        assert!(require_contactable_direct_owner(&db.conn, &original).is_err());
        assert!(!direct_owner_is_contactable(&db.conn, &original).unwrap());
        // A failed authority query is not an ordinary closed transport gate.
        let missing_schema = Connection::open_in_memory().unwrap();
        assert!(direct_owner_is_contactable(&missing_schema, &original).is_err());
    }

    #[test]
    fn retained_direct_occurrence_survives_claim_rotation_for_cleanup_only() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let reservation = direct_owner_reservation(&db);
        retain_direct_owner_fixture(&db, &reservation, "contact_pending");
        db.release_thread_launch_claim(&reservation.placement_thread_id, "direct-claim")
            .unwrap();
        db.claim_thread_launch(&reservation.placement_thread_id, "new-claim", "daemon:new")
            .unwrap();
        db.request_thread_stop(&reservation.placement_thread_id, StopIntent::Cancel)
            .unwrap();
        validate_current(&db.conn).unwrap();
        assert!(require_contactable_direct_owner(&db.conn, &reservation).is_err());
        let ExternalAllocationContactClaim::Reconcile(retained) = db
            .claim_external_allocation_contact(
                &reservation.placement_thread_id,
                &reservation.request_digest,
            )
            .unwrap()
        else {
            panic!("retained contact was not reconcile-only")
        };
        assert_eq!(retained.reservation, reservation);
        db.release_thread_launch_claim(&reservation.placement_thread_id, "new-claim")
            .unwrap();
        let pins = db
            .inspect_chain_recovery_pins(
                &reservation.placement_thread_id,
                &[reservation.placement_thread_id.clone()],
            )
            .unwrap();
        assert_eq!(pins.external_allocation_obligations, 1);
        assert!(!pins.is_empty());
        assert!(
            db.conn
                .execute(
                    "DELETE FROM thread_runtime WHERE thread_id=?1",
                    [&reservation.placement_thread_id]
                )
                .is_err()
        );
        assert!(
            db.conn
                .execute(
                    "UPDATE thread_runtime SET chain_root_id='T-other' WHERE thread_id=?1",
                    [&reservation.placement_thread_id]
                )
                .is_err()
        );
        assert_eq!(
            db.external_execution_cas_roots().unwrap(),
            vec![
                reservation.admitted_capsule_hash.clone(),
                reservation.base_snapshot_hash.clone()
            ]
        );
    }

    #[test]
    fn direct_uncontacted_settlement_releases_retention_not_contact_authority() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let reservation = direct_owner_reservation(&db);
        retain_direct_owner_fixture(&db, &reservation, "reserved");
        assert!(
            db.claim_external_allocation_contact(
                &reservation.placement_thread_id,
                &reservation.request_digest
            )
            .is_err()
        );
        assert_eq!(
            db.external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap()
                .phase,
            ExternalAllocationPhase::Reserved
        );
        db.cancel_external_allocation(&reservation.placement_thread_id)
            .unwrap();
        assert_eq!(
            db.unsettled_external_chain_count(&reservation.placement_thread_id)
                .unwrap(),
            0
        );
        assert!(db.external_execution_cas_roots().unwrap().is_empty());
        assert_eq!(
            db.conn
                .execute(
                    "DELETE FROM thread_runtime WHERE thread_id=?1",
                    [&reservation.placement_thread_id]
                )
                .unwrap(),
            1
        );
    }

    fn test_observation_timing() -> ExternalObservationTiming {
        ExternalObservationTiming::Startup {
            deadline_exceeded: false,
            live_deadline: lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_secs(60),
            ),
        }
    }
    use super::*;

    pub(super) fn reservation(db: &RuntimeDb, suffix: &str) -> ExternalAllocationReservation {
        reservation_with_base(db, suffix, &"b".repeat(64))
    }

    pub(super) fn reservation_with_base(
        db: &RuntimeDb,
        suffix: &str,
        base_snapshot_hash: &str,
    ) -> ExternalAllocationReservation {
        let placement = format!("T-{suffix}");
        let workspace = format!("W-{suffix}");
        let worker = format!("worker-{suffix}");
        let profile = format!("P-{suffix}");
        db.create_credential_profile(NewCredentialProfile {
            profile_id: &profile,
            owner_principal: "fp:operator",
            home_id: &format!("home-{suffix}"),
        })
        .unwrap();
        db.acquire_credential_profile(&profile, "fp:operator", &worker)
            .unwrap();
        db.admit_dedicated_session(NewDedicatedSession {
            placement_thread_id: &placement,
            chain_root_id: "T-root",
            owner_principal: "fp:operator",
            admitted_capsule_hash: &"a".repeat(64),
            workspace_id: &workspace,
            candidate_required: true,
            candidate_disposition: DedicatedCandidateDisposition::OwnerDecision,
            credential_profile_id: &profile,
            credential_generation: 1,
            credential_lock_owner: &worker,
        })
        .unwrap();
        assert_eq!(
            db.claim_thread_launch(
                &placement,
                &format!("claim-{suffix}"),
                "daemon:external-execution-test",
            )
            .unwrap(),
            crate::runtime_db::LaunchClaimOutcome::Claimed,
        );
        let launch_owner = db
            .get_launch_claim(&placement)
            .unwrap()
            .expect("external execution fixture launch claim")
            .claimed_by;
        db.conn
            .execute(
                "INSERT INTO execution_workspace(workspace_id,thread_id,launch_owner,backend_id,
             base_snapshot,root_path,state,created_at_ms,updated_at_ms)
             VALUES(?1,?2,?3,'fixture',?4,'/fixture','ready',1,1)",
                params![workspace, placement, launch_owner, base_snapshot_hash],
            )
            .unwrap();
        let binding = crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
        let now = i64::try_from(lillux::time::timestamp_millis()).unwrap();
        ExternalAllocationReservation {
            schema: EXTERNAL_ALLOCATION_RESERVATION_SCHEMA,
            placement_thread_id: placement,
            admitted_capsule_hash: "a".repeat(64),
            owner: ExternalAllocationOwner::DedicatedSession(ExternalDedicatedSessionOwner {
                workspace_id: workspace,
                worker_instance_id: worker,
                worker_boot_epoch: 1,
            }),
            base_snapshot_hash: base_snapshot_hash.to_owned(),
            binding_hash: binding.digest().to_owned(),
            capacity_owner: binding.capacity_owner().to_owned(),
            channel_authority_generation: "3".repeat(64),
            channel_owner_public_key: ryeos_state::external_execution::encode_channel_public_key(
                &lillux::crypto::SigningKey::from_bytes(&[19; 32]).verifying_key(),
            )
            .unwrap(),
            channel_bootstrap_capability_hash: "4".repeat(64),
            request_digest: "e".repeat(64),
            max_active: 1,
            timeout_seconds: 60,
            startup_started_at_ms: now,
            startup_deadline_ms: now + 60_000,
            contact_deadline_ms: i64::try_from(lillux::time::timestamp_millis()).unwrap() + 60_000,
        }
    }

    pub(super) fn reserve(
        db: &RuntimeDb,
        reservation: &ExternalAllocationReservation,
    ) -> Result<ExternalAllocationRecord> {
        db.reserve_external_allocation(
            reservation,
            &crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture(),
        )
    }

    pub(super) fn activation_intent(
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
    ) -> ExternalSupervisorActivationIntent {
        let contract = crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture().backend_contract();
        let attachment_deadline_ms = reservation.contact_deadline_ms
            + i64::from(contract.observation_timeout_seconds) * 1_000;
        let post_execution_timeout_seconds =
            contract.observation_timeout_seconds + contract.cleanup_timeout_seconds;
        let channel_max_bytes = contract.max_transfer_bytes.min(64 * 1024 * 1024);
        let guest_input_identity = "9".repeat(64);
        let activation_request_digest = external_supervisor_activation_request_digest(
            reservation,
            occurrence,
            &contract,
            attachment_deadline_ms,
            post_execution_timeout_seconds,
            channel_max_bytes,
            &guest_input_identity,
        )
        .unwrap();
        let delivery = fixture_guest_package_delivery(
            &reservation.binding_hash,
            &reservation.request_digest,
            &occurrence.occurrence_id,
            &activation_request_digest,
            &guest_input_identity,
        );
        ExternalSupervisorActivationIntent {
            schema: 3,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            supervisor_runtime_hash: contract
                .workload
                .structured_session()
                .unwrap()
                .runtime_manifest_hash
                .clone(),
            guest_input_identity,
            activation_request_digest,
            attachment_deadline_ms,
            execution_timeout_seconds: reservation.timeout_seconds,
            post_execution_timeout_seconds,
            channel_max_bytes,
            delivery,
        }
    }

    #[test]
    fn late_lifecycle_evidence_and_quarantine_commit_atomically_and_survive_reopen() {
        for stage in [
            "allocation",
            "activation",
            "pending_allocation",
            "pending_activation",
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("runtime.sqlite3");
            let db = RuntimeDb::open(&path).unwrap();
            let reservation = reservation(&db, "atomic-late");
            let placement = &reservation.placement_thread_id;
            reserve(&db, &reservation).unwrap();
            db.claim_external_allocation_contact(placement, &reservation.request_digest)
                .unwrap();
            let occurrence = ExternalAllocationOccurrence {
                schema: 1,
                binding_hash: reservation.binding_hash.clone(),
                request_digest: reservation.request_digest.clone(),
                occurrence_id: "atomic-occurrence".into(),
                provider_observation_digest: "f".repeat(64),
            };
            if stage.contains("activation") {
                db.bind_external_allocation(placement, &occurrence, test_observation_timing())
                    .unwrap();
                db.begin_external_supervisor_activation(
                    placement,
                    &activation_intent(&reservation, &occurrence),
                )
                .unwrap();
            }
            let intent = activation_intent(&reservation, &occurrence);
            let observation = ExternalSupervisorActivationObservation {
                schema: 1,
                binding_hash: reservation.binding_hash.clone(),
                request_digest: reservation.request_digest.clone(),
                occurrence_id: occurrence.occurrence_id.clone(),
                activation_request_digest: intent.activation_request_digest.clone(),
                activation_state: "started".into(),
                provider_observation_digest: "7".repeat(64),
            };
            let timing = test_observation_timing().with_deadline_exceeded(true);
            let settle = |db: &RuntimeDb| match stage {
                "allocation" => db.bind_external_allocation(placement, &occurrence, timing),
                "activation" => {
                    db.settle_external_supervisor_activation(placement, &observation, timing)
                }
                _ => db.observe_external_lifecycle_pending(placement, timing),
            };
            // Deliberately fail the quarantine write. Its observation insert
            // must roll back too: there is no separately committed Bound or
            // Started state to recover after this transaction failure.
            db.conn.execute_batch("CREATE TRIGGER test_refuse_late_fence BEFORE UPDATE OF phase ON external_execution_allocation
                WHEN NEW.phase='quarantined' BEGIN SELECT RAISE(ABORT,'fixture quarantine failure'); END;").unwrap();
            assert!(settle(&db).is_err());
            let record = db.external_allocation(placement).unwrap().unwrap();
            assert_ne!(record.phase, ExternalAllocationPhase::Quarantined);
            if stage == "allocation" {
                assert!(record.occurrence.is_none());
            }
            if stage == "activation" {
                assert!(
                    db.external_supervisor_activation(placement)
                        .unwrap()
                        .unwrap()
                        .observation
                        .is_none()
                );
            }
            db.conn
                .execute_batch("DROP TRIGGER test_refuse_late_fence")
                .unwrap();
            reservation.startup_deadline().unwrap();
            settle(&db).unwrap();
            drop(db);
            let db = RuntimeDb::open(&path).unwrap();
            assert_eq!(
                db.external_allocation(placement).unwrap().unwrap().phase,
                ExternalAllocationPhase::Quarantined
            );
            if stage == "allocation" {
                assert_eq!(
                    db.external_allocation(placement)
                        .unwrap()
                        .unwrap()
                        .occurrence
                        .as_ref(),
                    Some(&occurrence)
                );
                assert!(
                    db.begin_external_supervisor_activation(placement, &intent)
                        .is_err()
                );
            }
            if stage == "activation" {
                assert_eq!(
                    db.external_supervisor_activation(placement)
                        .unwrap()
                        .unwrap()
                        .observation
                        .as_ref(),
                    Some(&observation)
                );
            }
            // Exact observation reentry cannot clear the committed fence.
            settle(&db).unwrap();
            assert_eq!(
                db.external_allocation(placement).unwrap().unwrap().phase,
                ExternalAllocationPhase::Quarantined
            );
        }
    }

    #[test]
    fn duplicate_evidence_still_fences_an_expired_live_cap_atomically() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let reservation = reservation(&db, "duplicate-late");
        let placement = &reservation.placement_thread_id;
        reserve(&db, &reservation).unwrap();
        db.claim_external_allocation_contact(placement, &reservation.request_digest)
            .unwrap();
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: "duplicate-occurrence".into(),
            provider_observation_digest: "f".repeat(64),
        };
        db.bind_external_allocation(placement, &occurrence, test_observation_timing())
            .unwrap();
        assert_eq!(
            db.external_allocation(placement).unwrap().unwrap().phase,
            ExternalAllocationPhase::Bound
        );
        reservation.startup_deadline().unwrap();
        db.bind_external_allocation(
            placement,
            &occurrence,
            ExternalObservationTiming::Startup {
                deadline_exceeded: false,
                live_deadline: lillux::time::MonotonicDeadline::after(lillux::time::Duration::ZERO),
            },
        )
        .unwrap();
        assert_eq!(
            db.external_allocation(placement).unwrap().unwrap().phase,
            ExternalAllocationPhase::Quarantined
        );
    }

    #[test]
    fn startup_reservation_is_strict_and_has_one_exact_capsule_budget() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let reserved = reservation(&db, "startup-shape");
        reserved.validate_startup_budget(60_000).unwrap();
        assert!(reserved.validate_startup_budget(60_001).is_err());
        let value = serde_json::to_value(&reserved).unwrap();
        for name in ["startup_started_at_ms", "startup_deadline_ms"] {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(name);
            assert!(serde_json::from_value::<ExternalAllocationReservation>(missing).is_err());
        }
        let mut unknown = value.clone();
        unknown["startup_timeout_ms"] = 60_000.into();
        assert!(serde_json::from_value::<ExternalAllocationReservation>(unknown).is_err());
        let mut predecessor = reserved.clone();
        predecessor.schema = 2;
        assert!(predecessor.validate().is_err());
        for (start, end) in [
            (0, 1),
            (10, 10),
            (10, 9),
            (1, 600_002),
            (i64::MAX, i64::MIN),
        ] {
            let mut invalid = reserved.clone();
            invalid.startup_started_at_ms = start;
            invalid.startup_deadline_ms = end;
            assert!(invalid.validate().is_err());
        }
        assert!(
            reserved
                .require_startup_time(reserved.startup_started_at_ms - 1)
                .is_err()
        );
        reserved
            .require_startup_time(reserved.startup_deadline_ms - 1)
            .unwrap();
        assert!(
            reserved
                .require_startup_time(reserved.startup_deadline_ms)
                .is_err()
        );
    }

    #[test]
    fn startup_anchor_survives_reopen_and_refuses_budget_renewal() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let reserved = reservation(&db, "startup-retained");
        reserve(&db, &reserved).unwrap();
        drop(db);
        let db = RuntimeDb::open(&path).unwrap();
        assert_eq!(reserve(&db, &reserved).unwrap().reservation, reserved);
        let mut renewed = reserved.clone();
        renewed.startup_started_at_ms += 1;
        renewed.startup_deadline_ms += 1;
        assert!(reserve(&db, &renewed).is_err());
        assert_eq!(
            db.external_allocation(&reserved.placement_thread_id)
                .unwrap()
                .unwrap()
                .reservation,
            reserved
        );
    }

    #[test]
    fn expired_startup_refuses_new_contact_but_preserves_uncertain_cleanup() {
        for contacted in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
            let mut reserved = reservation(&db, "startup-expiry");
            reserved.startup_deadline_ms = reserved.startup_started_at_ms + 250;
            reserve(&db, &reserved).unwrap();
            if contacted {
                assert!(matches!(
                    db.claim_external_allocation_contact(
                        &reserved.placement_thread_id,
                        &reserved.request_digest
                    )
                    .unwrap(),
                    ExternalAllocationContactClaim::Contact(_)
                ));
            }
            let now = i64::try_from(lillux::time::timestamp_millis()).unwrap();
            lillux::time::sleep(lillux::time::Duration::from_millis(
                u64::try_from((reserved.startup_deadline_ms - now).max(0)).unwrap() + 1,
            ));
            let result = db.claim_external_allocation_contact(
                &reserved.placement_thread_id,
                &reserved.request_digest,
            );
            if contacted {
                assert!(matches!(
                    result.unwrap(),
                    ExternalAllocationContactClaim::Reconcile(_)
                ));
            } else {
                assert!(result.is_err());
            }
            // Exact retained reservation can be read/recovered after expiry;
            // expiry never erases an uncertain allocator contact.
            assert_eq!(reserve(&db, &reserved).unwrap().reservation, reserved);
        }
    }

    #[test]
    fn external_contact_accepts_the_live_public_workspace_and_refuses_freeze() {
        let active_dir = tempfile::tempdir().unwrap();
        let active_db = RuntimeDb::open(&active_dir.path().join("runtime.sqlite3")).unwrap();
        let active = reservation(&active_db, "active");
        active_db
            .conn
            .execute(
                "UPDATE execution_workspace SET state='active' WHERE workspace_id=?1",
                [&active.owner.dedicated_session().unwrap().workspace_id],
            )
            .unwrap();
        assert_eq!(
            reserve(&active_db, &active).unwrap().phase,
            ExternalAllocationPhase::Reserved
        );

        let frozen_dir = tempfile::tempdir().unwrap();
        let frozen_db = RuntimeDb::open(&frozen_dir.path().join("runtime.sqlite3")).unwrap();
        let frozen = reservation(&frozen_db, "frozen");
        frozen_db
            .conn
            .execute(
                "UPDATE execution_workspace SET state='freezing' WHERE workspace_id=?1",
                [&frozen.owner.dedicated_session().unwrap().workspace_id],
            )
            .unwrap();
        assert!(reserve(&frozen_db, &frozen).is_err());
        assert!(frozen_db.external_allocation("T-frozen").unwrap().is_none());
    }

    #[test]
    fn quarantined_external_start_failure_retains_session_and_credential_fence() {
        let dir = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&dir.path().join("runtime.sqlite3")).unwrap();
        let reserved = reservation(&db, "failed-start");
        reserve(&db, &reserved).unwrap();
        db.claim_external_allocation_contact(
            &reserved.placement_thread_id,
            &reserved.request_digest,
        )
        .unwrap();
        db.cancel_external_allocation(&reserved.placement_thread_id)
            .unwrap();
        assert_eq!(
            db.external_allocation(&reserved.placement_thread_id)
                .unwrap()
                .unwrap()
                .phase,
            ExternalAllocationPhase::Quarantined
        );
        db.fail_dedicated_session_start(
            &reserved.placement_thread_id,
            "worker-failed-start",
            1,
            "guest package refused",
            false,
        )
        .unwrap();
        let session = db
            .dedicated_session(&reserved.placement_thread_id)
            .unwrap()
            .unwrap();
        assert_eq!(session.state, "outcome_unknown");
        assert!(db
            .fail_dedicated_session_start(
                &reserved.placement_thread_id,
                "worker-failed-start",
                1,
                "incorrect local-only cleanup claim",
                true,
            )
            .is_err());
        assert_eq!(
            db.credential_profile("P-failed-start")
                .unwrap()
                .unwrap()
                .lock_owner
                .as_deref(),
            Some("worker-failed-start")
        );
    }

    #[test]
    fn external_contact_is_claimed_once_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let reserved = reservation(&db, "one");
        assert_eq!(
            reserve(&db, &reserved).unwrap().phase,
            ExternalAllocationPhase::Reserved
        );
        assert_eq!(
            reserve(&db, &reserved).unwrap().phase,
            ExternalAllocationPhase::Reserved
        );
        assert!(matches!(
            db.claim_external_allocation_contact("T-one", &reserved.request_digest)
                .unwrap(),
            ExternalAllocationContactClaim::Contact(ExternalAllocationRecord {
                phase: ExternalAllocationPhase::ContactPending,
                ..
            })
        ));
        assert!(matches!(
            db.claim_external_allocation_contact("T-one", &reserved.request_digest)
                .unwrap(),
            ExternalAllocationContactClaim::Reconcile(ExternalAllocationRecord {
                phase: ExternalAllocationPhase::ContactPending,
                ..
            })
        ));
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reserved.binding_hash.clone(),
            request_digest: reserved.request_digest.clone(),
            occurrence_id: "fixture-occurrence".into(),
            provider_observation_digest: "f".repeat(64),
        };
        db.bind_external_allocation("T-one", &occurrence, test_observation_timing())
            .unwrap();
        assert!(matches!(
            db.claim_external_allocation_contact("T-one", &reserved.request_digest)
                .unwrap(),
            ExternalAllocationContactClaim::Reconcile(ExternalAllocationRecord {
                phase: ExternalAllocationPhase::Bound,
                occurrence: Some(ref current),
                ..
            }) if current == &occurrence
        ));
        assert!(db.discard_all_thread_history(true).is_err());
        drop(db);
        let db = RuntimeDb::open(&path).unwrap();
        let retained_json: String = db.conn.query_row(
            "SELECT binding_json FROM external_execution_binding_generation WHERE binding_hash=?1",
            [&reserved.binding_hash], |row| row.get(0)).unwrap();
        let retained: crate::node_config::sections::external_execution::RetainedExternalExecutionBinding =
            serde_json::from_str(&retained_json).unwrap();
        retained.validate().unwrap();
        assert!(matches!(
            db.claim_external_allocation_contact("T-one", &reserved.request_digest)
                .unwrap(),
            ExternalAllocationContactClaim::Reconcile(ExternalAllocationRecord {
                phase: ExternalAllocationPhase::Bound,
                ..
            })
        ));
        db.cancel_external_allocation("T-one").unwrap();
        assert_eq!(
            db.external_allocation("T-one").unwrap().unwrap().phase,
            ExternalAllocationPhase::Quarantined
        );
        assert_eq!(read_guard(&db.conn).unwrap(), 1);
        assert!(
            db.release_credential_profile("P-one", "worker-one")
                .is_err()
        );
        assert_eq!(
            db.external_execution_cas_roots().unwrap(),
            vec!["a".repeat(64), "b".repeat(64)]
        );
        let second = reservation(&db, "two");
        assert!(reserve(&db, &second).is_err());
    }

    #[test]
    fn guest_package_delivery_is_subordinate_to_exact_activation_and_signed_budget() {
        let dir = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&dir.path().join("runtime.sqlite3")).unwrap();
        let reservation = reservation(&db, "one");
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: "fixture-occurrence".into(),
            provider_observation_digest: "f".repeat(64),
        };
        let activation = activation_intent(&reservation, &occurrence);
        let contract = crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture().backend_contract();
        let delivery = ExternalGuestPackageDeliveryCommitment {
            schema: 1,
            binding_hash: activation.binding_hash.clone(),
            request_digest: activation.request_digest.clone(),
            occurrence_id: activation.occurrence_id.clone(),
            activation_request_digest: activation.activation_request_digest.clone(),
            guest_input_identity: activation.guest_input_identity.clone(),
            manifest_sha256: "1".repeat(64),
            payload_sha256: "2".repeat(64),
            regular_bytes: contract.max_guest_package_regular_bytes,
            framed_bytes: contract.max_guest_package_framed_bytes,
        };
        delivery.validate_for(&activation, &contract).unwrap();
        let mut changed = delivery.clone();
        changed.schema = 2;
        assert!(changed.validate_for(&activation, &contract).is_err());
        let mut changed = delivery.clone();
        changed.binding_hash = "4".repeat(64);
        assert!(changed.validate_for(&activation, &contract).is_err());
        let mut changed = delivery.clone();
        changed.request_digest = "5".repeat(64);
        assert!(changed.validate_for(&activation, &contract).is_err());
        let mut changed = delivery.clone();
        changed.occurrence_id = "other-occurrence".into();
        assert!(changed.validate_for(&activation, &contract).is_err());
        let mut changed = delivery.clone();
        changed.activation_request_digest = "3".repeat(64);
        assert!(changed.validate_for(&activation, &contract).is_err());
        let mut changed = delivery.clone();
        changed.guest_input_identity = "6".repeat(64);
        assert!(changed.validate_for(&activation, &contract).is_err());
        let mut changed = delivery.clone();
        changed.manifest_sha256 = "not-a-digest".into();
        assert!(changed.validate_for(&activation, &contract).is_err());
        let mut changed = delivery.clone();
        changed.payload_sha256 = "not-a-digest".into();
        assert!(changed.validate_for(&activation, &contract).is_err());
        let mut changed = delivery.clone();
        changed.regular_bytes = 0;
        assert!(changed.validate_for(&activation, &contract).is_err());
        let mut changed = delivery.clone();
        changed.regular_bytes += 1;
        assert!(changed.validate_for(&activation, &contract).is_err());
        let mut changed = delivery.clone();
        changed.framed_bytes = changed.regular_bytes + 19;
        assert!(changed.validate_for(&activation, &contract).is_err());
        let mut changed = delivery;
        changed.framed_bytes += 1;
        assert!(changed.validate_for(&activation, &contract).is_err());
    }

    #[test]
    fn supervisor_activation_requires_bound_occurrence_and_reconciles_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let reservation = reservation(&db, "one");
        reserve(&db, &reservation).unwrap();
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: "fixture-occurrence".into(),
            provider_observation_digest: "f".repeat(64),
        };
        let intent = activation_intent(&reservation, &occurrence);
        assert!(
            db.begin_external_supervisor_activation("T-one", &intent)
                .is_err()
        );
        db.claim_external_allocation_contact("T-one", &reservation.request_digest)
            .unwrap();
        db.bind_external_allocation("T-one", &occurrence, test_observation_timing())
            .unwrap();

        assert!(
            db.begin_external_supervisor_activation("T-one", &intent)
                .unwrap()
        );
        assert!(
            !db.begin_external_supervisor_activation("T-one", &intent)
                .unwrap()
        );
        let mut changed = intent.clone();
        changed.supervisor_runtime_hash = "6".repeat(64);
        assert!(
            db.begin_external_supervisor_activation("T-one", &changed)
                .is_err()
        );
        let mut changed = intent.clone();
        changed.delivery.payload_sha256 = "7".repeat(64);
        assert!(
            db.begin_external_supervisor_activation("T-one", &changed)
                .is_err()
        );
        let mut changed = intent.clone();
        changed.delivery.manifest_sha256 = "8".repeat(64);
        assert!(
            db.begin_external_supervisor_activation("T-one", &changed)
                .is_err()
        );
        drop(db);

        let db = RuntimeDb::open(&path).unwrap();
        assert!(
            !db.begin_external_supervisor_activation("T-one", &intent)
                .unwrap()
        );
        let mut changed = intent.clone();
        changed.delivery.payload_sha256 = "7".repeat(64);
        assert!(
            db.begin_external_supervisor_activation("T-one", &changed)
                .is_err()
        );
        let observation = ExternalSupervisorActivationObservation {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            activation_request_digest: intent.activation_request_digest.clone(),
            activation_state: "started".into(),
            provider_observation_digest: "7".repeat(64),
        };
        db.settle_external_supervisor_activation("T-one", &observation, test_observation_timing())
            .unwrap();
        db.settle_external_supervisor_activation("T-one", &observation, test_observation_timing())
            .unwrap();
        let retained = db.external_supervisor_activation("T-one").unwrap().unwrap();
        assert_eq!(retained.intent, intent);
        assert_eq!(retained.observation.as_ref(), Some(&observation));
        let mut changed = observation;
        changed.activation_state = "not_started".into();
        assert!(
            db.settle_external_supervisor_activation("T-one", &changed, test_observation_timing())
                .is_err()
        );
    }

    #[test]
    fn first_supervisor_activation_requires_a_live_launch_owner() {
        let dir = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&dir.path().join("runtime.sqlite3")).unwrap();
        let reservation = reservation(&db, "orphaned");
        reserve(&db, &reservation).unwrap();
        db.claim_external_allocation_contact(
            &reservation.placement_thread_id,
            &reservation.request_digest,
        )
        .unwrap();
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: "fixture-orphaned".into(),
            provider_observation_digest: "f".repeat(64),
        };
        db.bind_external_allocation(
            &reservation.placement_thread_id,
            &occurrence,
            test_observation_timing(),
        )
        .unwrap();
        db.conn
            .execute(
                "UPDATE execution_workspace SET state='orphaned' WHERE workspace_id=?1",
                [&reservation.owner.dedicated_session().unwrap().workspace_id],
            )
            .unwrap();
        let intent = activation_intent(&reservation, &occurrence);
        assert!(
            db.begin_external_supervisor_activation(&reservation.placement_thread_id, &intent)
                .is_err()
        );
        let retained: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM external_execution_supervisor_activation_intent WHERE placement_thread_id=?1",
                [&reservation.placement_thread_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, 0);
    }

    #[test]
    fn recovery_validation_recomputes_supervisor_activation_request_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let reservation = reservation(&db, "activation-digest");
        reserve(&db, &reservation).unwrap();
        db.claim_external_allocation_contact(
            &reservation.placement_thread_id,
            &reservation.request_digest,
        )
        .unwrap();
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: "fixture-activation-digest".into(),
            provider_observation_digest: "f".repeat(64),
        };
        db.bind_external_allocation(
            &reservation.placement_thread_id,
            &occurrence,
            test_observation_timing(),
        )
        .unwrap();
        let intent = activation_intent(&reservation, &occurrence);
        assert!(
            db.begin_external_supervisor_activation(&reservation.placement_thread_id, &intent)
                .unwrap()
        );
        let observation = ExternalSupervisorActivationObservation {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            activation_request_digest: intent.activation_request_digest.clone(),
            activation_state: "started".into(),
            provider_observation_digest: "7".repeat(64),
        };
        db.settle_external_supervisor_activation(
            &reservation.placement_thread_id,
            &observation,
            test_observation_timing(),
        )
        .unwrap();
        validate_current(&db.conn).unwrap();
        let mut replaced_intent = intent;
        replaced_intent.activation_request_digest = "6".repeat(64);
        // Keep the nested package commitment consistent with the forged row
        // so recovery reaches the independent activation-request recomputation.
        replaced_intent.delivery.activation_request_digest = "6".repeat(64);
        let mut replaced_observation = observation;
        replaced_observation.activation_request_digest = "6".repeat(64);
        db.conn
            .execute_batch(
                "DROP TRIGGER external_execution_supervisor_activation_intent_immutable;
                 DROP TRIGGER external_execution_supervisor_activation_observation_immutable;",
            )
            .unwrap();
        db.conn
            .execute(
                "UPDATE external_execution_supervisor_activation_intent SET intent_json=?2 WHERE placement_thread_id=?1",
                params![
                    reservation.placement_thread_id,
                    lillux::canonical_json(&serde_json::to_value(&replaced_intent).unwrap()).unwrap()
                ],
            )
            .unwrap();
        db.conn
            .execute(
                "UPDATE external_execution_supervisor_activation_observation SET observation_json=?2 WHERE placement_thread_id=?1",
                params![
                    reservation.placement_thread_id,
                    lillux::canonical_json(&serde_json::to_value(&replaced_observation).unwrap())
                        .unwrap()
                ],
            )
            .unwrap();
        let error = validate_current(&db.conn).unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("external supervisor activation request identity changed")
        );
    }

    #[test]
    fn contacted_no_occurrence_requires_exact_evidence_and_releases_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&dir.path().join("runtime.sqlite3")).unwrap();
        let first = reservation(&db, "one");
        reserve(&db, &first).unwrap();
        assert!(matches!(
            db.claim_external_allocation_contact("T-one", &first.request_digest)
                .unwrap(),
            ExternalAllocationContactClaim::Contact(_)
        ));
        let mut evidence = ExternalNoOccurrenceEvidence {
            schema: 1,
            binding_hash: first.binding_hash.clone(),
            request_digest: first.request_digest.clone(),
            provider_observation_digest: "f".repeat(64),
        };
        let mut wrong = evidence.clone();
        wrong.request_digest = "0".repeat(64);
        assert!(db.settle_external_no_occurrence("T-one", &wrong).is_err());
        db.settle_external_no_occurrence("T-one", &evidence)
            .unwrap();
        db.settle_external_no_occurrence("T-one", &evidence)
            .unwrap();
        assert_eq!(read_guard(&db.conn).unwrap(), 0);
        assert_eq!(
            db.external_allocation("T-one").unwrap().unwrap().phase,
            ExternalAllocationPhase::ContactedNoOccurrence
        );
        evidence.provider_observation_digest = "0".repeat(64);
        assert!(
            db.settle_external_no_occurrence("T-one", &evidence)
                .is_err()
        );

        let second = reservation(&db, "two");
        reserve(&db, &second).unwrap();
    }

    #[test]
    fn exact_terminal_observation_is_distinct_from_termination_intent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let reservation = reservation(&db, "one");
        reserve(&db, &reservation).unwrap();
        db.claim_external_allocation_contact("T-one", &reservation.request_digest)
            .unwrap();
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: "fixture-occurrence".into(),
            provider_observation_digest: "f".repeat(64),
        };
        db.bind_external_allocation("T-one", &occurrence, test_observation_timing())
            .unwrap();
        let intent = ExternalTerminationIntent {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            termination_request_digest: "1".repeat(64),
        };
        let observation = ExternalTerminalObservation {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            termination_request_digest: intent.termination_request_digest.clone(),
            terminal_state: "terminated".into(),
            provider_observation_digest: "2".repeat(64),
        };
        assert!(db.settle_external_terminal("T-one", &observation).is_err());
        assert!(db.begin_external_termination("T-one", &intent).unwrap());
        assert!(!db.begin_external_termination("T-one", &intent).unwrap());
        assert_eq!(read_guard(&db.conn).unwrap(), 1);
        let mut mismatches = Vec::new();
        let mut changed = observation.clone();
        changed.binding_hash = "3".repeat(64);
        mismatches.push(changed);
        let mut changed = observation.clone();
        changed.request_digest = "3".repeat(64);
        mismatches.push(changed);
        let mut changed = observation.clone();
        changed.occurrence_id = "other-occurrence".into();
        mismatches.push(changed);
        let mut changed = observation.clone();
        changed.termination_request_digest = "3".repeat(64);
        mismatches.push(changed);
        let mut changed = observation.clone();
        changed.terminal_state = "running".into();
        mismatches.push(changed);
        for changed in mismatches {
            assert!(db.settle_external_terminal("T-one", &changed).is_err());
            assert_eq!(read_guard(&db.conn).unwrap(), 1);
            assert_eq!(
                db.conn
                    .query_row(
                        "SELECT COUNT(*) FROM external_execution_terminal_observation",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                0
            );
        }
        drop(db);

        let db = RuntimeDb::open(&path).unwrap();
        assert!(!db.begin_external_termination("T-one", &intent).unwrap());
        db.settle_external_terminal("T-one", &observation).unwrap();
        db.settle_external_terminal("T-one", &observation).unwrap();
        assert_eq!(read_guard(&db.conn).unwrap(), 0);
        assert_eq!(
            db.external_allocation("T-one").unwrap().unwrap().phase,
            ExternalAllocationPhase::Terminated
        );
        assert!(matches!(
            db.claim_external_allocation_contact("T-one", &reservation.request_digest)
                .unwrap(),
            ExternalAllocationContactClaim::Settled(ExternalAllocationRecord {
                phase: ExternalAllocationPhase::Terminated,
                ..
            })
        ));
        let mut changed = observation;
        changed.provider_observation_digest = "3".repeat(64);
        assert!(db.settle_external_terminal("T-one", &changed).is_err());
    }

    #[test]
    fn uncertain_activation_can_be_retired_by_exact_occurrence_termination_after_restart() {
        // A provider may have accepted activation even when its response was
        // lost. Recovery cannot repeat that contact or infer its outcome, but
        // cleanup may retire the already-bound occurrence by its exact ID.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let reservation = reservation(&db, "uncertain-activation");
        reserve(&db, &reservation).unwrap();
        db.claim_external_allocation_contact(
            &reservation.placement_thread_id,
            &reservation.request_digest,
        )
        .unwrap();
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: "fixture-uncertain-activation".into(),
            provider_observation_digest: "f".repeat(64),
        };
        db.bind_external_allocation(
            &reservation.placement_thread_id,
            &occurrence,
            test_observation_timing(),
        )
        .unwrap();
        let activation = activation_intent(&reservation, &occurrence);
        assert!(
            db.begin_external_supervisor_activation(&reservation.placement_thread_id, &activation)
                .unwrap()
        );
        assert!(
            db.external_supervisor_activation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap()
                .observation
                .is_none()
        );
        drop(db);

        let db = RuntimeDb::open(&path).unwrap();
        assert!(
            !db.begin_external_supervisor_activation(&reservation.placement_thread_id, &activation)
                .unwrap()
        );
        let termination = ExternalTerminationIntent {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            termination_request_digest: "1".repeat(64),
        };
        assert!(
            db.begin_external_termination(&reservation.placement_thread_id, &termination)
                .unwrap()
        );
        assert!(
            !db.begin_external_termination(&reservation.placement_thread_id, &termination)
                .unwrap()
        );
        let terminal = ExternalTerminalObservation {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            termination_request_digest: termination.termination_request_digest.clone(),
            terminal_state: "terminated".into(),
            provider_observation_digest: "2".repeat(64),
        };
        db.settle_external_terminal(&reservation.placement_thread_id, &terminal)
            .unwrap();
        assert_eq!(
            db.external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap()
                .phase,
            ExternalAllocationPhase::Terminated
        );
        assert!(
            !db.begin_external_supervisor_activation(&reservation.placement_thread_id, &activation)
                .unwrap()
        );
        assert!(
            db.external_supervisor_activation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap()
                .observation
                .is_none()
        );
    }

    #[test]
    fn predecessor_guest_package_binding_refuses_before_reopen_decode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let retained = crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
        let mut predecessor = serde_json::to_value(&retained).unwrap();
        predecessor["document"]["schema"] = serde_json::Value::from(9);
        predecessor["document"]
            .as_object_mut()
            .unwrap()
            .remove("max_guest_package_regular_bytes");
        predecessor["document"]
            .as_object_mut()
            .unwrap()
            .remove("max_guest_package_framed_bytes");
        let predecessor = lillux::canonical_json(&predecessor).unwrap();
        db.conn
            .execute(
                "INSERT INTO external_execution_binding_generation
                 (binding_hash,capacity_owner,binding_json,retained_at_ms)
                 VALUES (?1,?2,?3,1)",
                params![retained.digest(), retained.capacity_owner(), predecessor],
            )
            .unwrap();
        db.conn
            .pragma_update(
                None,
                "application_id",
                RUNTIME_OPERATOR_APP_ID_PREFIX | (RUNTIME_OPERATOR_SCHEMA_EPOCH - 1),
            )
            .unwrap();
        drop(db);

        let error = RuntimeDb::open(&path)
            .err()
            .expect("predecessor binding must require explicit reset");
        let message = format!("{error:#}");
        assert!(message.contains("explicit no-backcompat reset"));
        assert!(message.contains(&format!(
            "stored schema_epoch={}",
            RUNTIME_OPERATOR_SCHEMA_EPOCH - 1
        )));
        assert!(!message.contains("decode retained external binding"));
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let preserved: String = conn
            .query_row(
                "SELECT binding_json FROM external_execution_binding_generation WHERE binding_hash=?1",
                [retained.digest()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(preserved, predecessor);
    }

    #[test]
    fn external_reservation_atomically_retains_exact_binding_generation() {
        let dir = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&dir.path().join("runtime.sqlite3")).unwrap();
        let reservation = reservation(&db, "one");
        let mut wrong = crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
        let mut value = serde_json::to_value(&wrong).unwrap();
        value["capacity_owner"] = serde_json::Value::String("f".repeat(64));
        wrong = serde_json::from_value(value).unwrap();
        assert!(
            db.reserve_external_allocation(&reservation, &wrong)
                .is_err()
        );
        let exact = crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
        let exact_value = serde_json::to_value(&exact).unwrap();
        for (path, changed) in [
            (
                "/document/credential_generation",
                serde_json::Value::String("f".repeat(64)),
            ),
            (
                "/document/workload/provider_declaration_id",
                serde_json::Value::String("other-provider".into()),
            ),
            (
                "/document/workload/provider_configuration_destination",
                serde_json::Value::String("other.toml".into()),
            ),
            (
                "/document/workload/runtime_manifest_hash",
                serde_json::Value::String("f".repeat(64)),
            ),
            (
                "/document/workload/runtime_selection_identity",
                serde_json::Value::String("f".repeat(64)),
            ),
            (
                "/document/workload/configuration_adapter_artifact_hash",
                serde_json::Value::String("f".repeat(64)),
            ),
            (
                "/document/workload/configuration_adapter_artifact_bytes",
                serde_json::Value::from(8192),
            ),
            (
                "/document/backend_artifact_hash",
                serde_json::Value::String("f".repeat(64)),
            ),
            (
                "/document/backend_artifact_bytes",
                serde_json::Value::from(8192),
            ),
            (
                "/document/supervisor_artifact_hash",
                serde_json::Value::String("f".repeat(64)),
            ),
            (
                "/document/supervisor_artifact_bytes",
                serde_json::Value::from(8192),
            ),
            (
                "/document/launcher_artifact_hash",
                serde_json::Value::String("f".repeat(64)),
            ),
            (
                "/document/launcher_artifact_bytes",
                serde_json::Value::from(8192),
            ),
            (
                "/document/workload/connector_protocol",
                serde_json::Value::String("other".into()),
            ),
            (
                "/document/workload/connector_artifact_hash",
                serde_json::Value::String("0".repeat(64)),
            ),
            (
                "/document/workload/connector_artifact_bytes",
                serde_json::Value::from(8192),
            ),
            (
                "/document/network_policy",
                serde_json::Value::String("other".into()),
            ),
            (
                "/document/max_workspace_bytes",
                serde_json::Value::from(2048),
            ),
            (
                "/document/max_guest_package_regular_bytes",
                serde_json::Value::from(2048),
            ),
            (
                "/document/max_guest_package_framed_bytes",
                serde_json::Value::from(4096),
            ),
            ("/document/max_active", serde_json::Value::from(2)),
            ("/document/timeout_seconds", serde_json::Value::from(61)),
        ] {
            let mut value = exact_value.clone();
            let field = value
                .pointer_mut(path)
                .expect("mutation must name an existing current-schema binding field");
            assert_ne!(*field, changed, "binding mutation is unchanged: {path}");
            *field = changed;
            let changed: crate::node_config::sections::external_execution::RetainedExternalExecutionBinding =
                serde_json::from_value(value).unwrap();
            assert!(
                changed.validate().is_err(),
                "retained field {path} escaped signed-source join"
            );
        }
        let mut value = exact_value.clone();
        value["document"]["settings"]["region"] = serde_json::Value::String("other".into());
        let changed: crate::node_config::sections::external_execution::RetainedExternalExecutionBinding =
            serde_json::from_value(value).unwrap();
        assert!(
            changed.validate().is_err(),
            "adapter-owned retained region escaped its signed settings join"
        );
        let mut wider = reservation.clone();
        wider.max_active = 2;
        assert!(db.reserve_external_allocation(&wider, &exact).is_err());
        let mut missing_owner = reservation.clone();
        missing_owner.placement_thread_id = "T-missing".into();
        let ExternalAllocationOwner::DedicatedSession(owner) = &mut missing_owner.owner else {
            unreachable!()
        };
        owner.workspace_id = "W-missing".into();
        owner.worker_instance_id = "worker-missing".into();
        assert!(
            db.reserve_external_allocation(&missing_owner, &exact)
                .is_err()
        );
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM external_execution_binding_generation",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        reserve(&db, &reservation).unwrap();
        assert!(
            db.conn
                .execute(
                    "UPDATE external_execution_binding_generation SET binding_json='{}'",
                    []
                )
                .is_err()
        );
        assert!(
            db.conn
                .execute("DELETE FROM external_execution_binding_generation", [])
                .is_err()
        );
    }

    #[test]
    fn recovery_rechecks_allocation_limits_against_retained_binding() {
        let dir = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&dir.path().join("runtime.sqlite3")).unwrap();
        let reservation = reservation(&db, "one");
        reserve(&db, &reservation).unwrap();

        // Model structural storage corruption after reservation. Startup must
        // not accept a wider allocation merely because both rows remain
        // individually well-formed and the binding identity still exists.
        db.conn
            .execute("DROP TRIGGER external_execution_transition_guard", [])
            .unwrap();
        let mut widened = reservation;
        widened.max_active = 2;
        let widened = lillux::canonical_json(&serde_json::to_value(widened).unwrap()).unwrap();
        db.conn
            .execute(
                "UPDATE external_execution_allocation SET reservation_json=?1
                 WHERE placement_thread_id='T-one'",
                [widened],
            )
            .unwrap();
        assert!(validate_current(&db.conn).is_err());
    }

    #[test]
    fn external_no_contact_cancel_settles_without_reopening_the_intent() {
        let dir = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&dir.path().join("runtime.sqlite3")).unwrap();
        let reserved = reservation(&db, "one");
        reserve(&db, &reserved).unwrap();
        db.cancel_external_allocation("T-one").unwrap();
        db.cancel_external_allocation("T-one").unwrap();
        assert!(matches!(
            db.claim_external_allocation_contact("T-one", &reserved.request_digest)
                .unwrap(),
            ExternalAllocationContactClaim::Settled(ExternalAllocationRecord {
                phase: ExternalAllocationPhase::NoContact,
                ..
            })
        ));
        assert_eq!(read_guard(&db.conn).unwrap(), 0);
        db.release_credential_profile("P-one", "worker-one")
            .unwrap();
        assert_eq!(
            reserve(&db, &reserved).unwrap().phase,
            ExternalAllocationPhase::NoContact
        );
    }

    #[test]
    fn external_contact_rechecks_workspace_readiness_in_claim_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&dir.path().join("runtime.sqlite3")).unwrap();
        let reserved = reservation(&db, "one");
        reserve(&db, &reserved).unwrap();
        db.conn
            .execute(
                "UPDATE execution_workspace SET state='orphaned' WHERE workspace_id='W-one'",
                [],
            )
            .unwrap();
        assert!(
            db.claim_external_allocation_contact("T-one", &reserved.request_digest)
                .is_err()
        );
        assert_eq!(
            db.external_allocation("T-one").unwrap().unwrap().phase,
            ExternalAllocationPhase::Reserved
        );
    }

    #[test]
    fn external_late_occurrence_identifies_cleanup_without_releasing_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&dir.path().join("runtime.sqlite3")).unwrap();
        let reserved = reservation(&db, "one");
        reserve(&db, &reserved).unwrap();
        assert!(matches!(
            db.claim_external_allocation_contact("T-one", &reserved.request_digest)
                .unwrap(),
            ExternalAllocationContactClaim::Contact(_)
        ));
        db.cancel_external_allocation("T-one").unwrap();
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reserved.binding_hash,
            request_digest: reserved.request_digest,
            occurrence_id: "exact-occurrence".into(),
            provider_observation_digest: "f".repeat(64),
        };
        db.bind_external_allocation("T-one", &occurrence, test_observation_timing())
            .unwrap();
        db.bind_external_allocation("T-one", &occurrence, test_observation_timing())
            .unwrap();
        let record = db.external_allocation("T-one").unwrap().unwrap();
        assert_eq!(record.phase, ExternalAllocationPhase::Quarantined);
        assert_eq!(record.occurrence, Some(occurrence.clone()));
        let mut wrong = occurrence;
        wrong.occurrence_id = "another-occurrence".into();
        assert!(
            db.bind_external_allocation("T-one", &wrong, test_observation_timing())
                .is_err()
        );
        assert_eq!(read_guard(&db.conn).unwrap(), 1);
    }

    #[test]
    fn external_reset_guard_does_not_decode_predecessor_execution_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(GUARD_SQL).unwrap();
        conn.execute_batch(
            "INSERT INTO external_execution_guard VALUES(1,1,1);
            CREATE TABLE unknown_execution_authority(opaque BLOB);
            INSERT INTO unknown_execution_authority VALUES(X'FF00');",
        )
        .unwrap();
        assert!(ensure_resettable(&conn, FIRST_EPOCH).is_err());
        assert!(ensure_resettable(&conn, FIRST_EPOCH + 1).is_err());
        conn.execute("UPDATE external_execution_guard SET unsettled=0", [])
            .unwrap();
        ensure_resettable(&conn, FIRST_EPOCH).unwrap();
        conn.execute("DELETE FROM external_execution_guard", [])
            .unwrap();
        assert!(ensure_resettable(&conn, FIRST_EPOCH).is_err());
        conn.execute("DROP TABLE external_execution_guard", [])
            .unwrap();
        assert!(ensure_resettable(&conn, FIRST_EPOCH).is_err());
        ensure_resettable(&conn, FIRST_EPOCH - 1).unwrap();
    }

    #[test]
    fn external_reservation_rejects_changed_or_unowned_authority() {
        let missing_claim = tempfile::tempdir().unwrap();
        let missing_claim_db =
            RuntimeDb::open(&missing_claim.path().join("runtime.sqlite3")).unwrap();
        let missing_claim_reservation = reservation(&missing_claim_db, "missing-claim");
        missing_claim_db
            .conn
            .execute(
                "DELETE FROM thread_launch_claim WHERE thread_id=?1",
                [&missing_claim_reservation.placement_thread_id],
            )
            .unwrap();
        assert!(reserve(&missing_claim_db, &missing_claim_reservation).is_err());

        let stale_owner = tempfile::tempdir().unwrap();
        let stale_owner_db = RuntimeDb::open(&stale_owner.path().join("runtime.sqlite3")).unwrap();
        let stale_owner_reservation = reservation(&stale_owner_db, "stale-owner");
        stale_owner_db
            .conn
            .execute(
                "UPDATE execution_workspace SET launch_owner='stale-owner' WHERE workspace_id=?1",
                [&stale_owner_reservation
                    .owner
                    .dedicated_session()
                    .unwrap()
                    .workspace_id],
            )
            .unwrap();
        assert!(reserve(&stale_owner_db, &stale_owner_reservation).is_err());

        let dir = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&dir.path().join("runtime.sqlite3")).unwrap();
        let reserved = reservation(&db, "one");
        let mut wrong = reserved.clone();
        wrong.schema = 1;
        assert!(reserve(&db, &wrong).is_err());
        wrong = reserved.clone();
        wrong.base_snapshot_hash = "f".repeat(64);
        assert!(reserve(&db, &wrong).is_err());
        wrong = reserved.clone();
        let ExternalAllocationOwner::DedicatedSession(owner) = &mut wrong.owner else {
            unreachable!()
        };
        owner.worker_boot_epoch += 1;
        assert!(reserve(&db, &wrong).is_err());
        reserve(&db, &reserved).unwrap();
        wrong = reserved.clone();
        wrong.binding_hash = "f".repeat(64);
        assert!(reserve(&db, &wrong).is_err());
        assert!(
            db.claim_external_allocation_contact("T-one", &"f".repeat(64))
                .is_err()
        );
        assert_eq!(
            db.external_allocation("T-one").unwrap().unwrap().phase,
            ExternalAllocationPhase::Reserved
        );
    }
}
