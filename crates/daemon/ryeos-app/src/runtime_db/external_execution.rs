//! Subordinate external allocation ownership, attached to a dedicated session.
//!
//! This is not a second session scheduler. Its key is the existing placement
//! thread. The local worker remains an ordinary local process. Persist contact
//! intention before calling an external allocator; an ambiguous call consumes
//! capacity indefinitely until an independently qualified reconciliation path
//! settles it. No TTL, local process death or caller boolean is cleanup proof.

use super::*;
use anyhow::ensure;

mod channel;

pub(super) const FIRST_EPOCH: u32 = 40;
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
    evidence_blob_hash TEXT NOT NULL,
    completion_request_digest TEXT NOT NULL,
    export_frame_digest TEXT NOT NULL
);

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
"#;

/// No secrets, URLs or arbitrary commands are accepted at this boundary.
/// The protected adapter binding supplies them, not a project/worker request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalAllocationReservation {
    pub schema: u32,
    pub placement_thread_id: String,
    pub admitted_capsule_hash: String,
    pub workspace_id: String,
    pub worker_instance_id: String,
    pub worker_boot_epoch: u64,
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
}

impl ExternalAllocationReservation {
    pub fn validate(&self) -> Result<()> {
        if self.schema != 2
            || !(1..=64).contains(&self.max_active)
            || !(1..=3600).contains(&self.timeout_seconds)
            || self.contact_deadline_ms <= 0
            || self.worker_boot_epoch == 0
            || self.worker_boot_epoch > i64::MAX as u64
        {
            bail!("external allocation reservation is outside its versioned bounds");
        }
        validate_bounded_runtime_text("external placement", &self.placement_thread_id, 256)?;
        validate_bounded_runtime_text("external workspace", &self.workspace_id, 256)?;
        validate_bounded_runtime_text("external worker", &self.worker_instance_id, 256)?;
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
        Ok(())
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
        if reservation_json.len() > 8192
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
    match record.phase {
        ExternalAllocationPhase::ContactedNoOccurrence => ensure!(
            no_occurrence.is_some() && termination.is_none() && terminal.is_none(),
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
            require_session_owner(conn, &record.reservation)?;
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
        validate_lifecycle_evidence(conn, &record)?;
    }
    channel::validate_channels(conn)?;
    Ok(())
}

fn require_session_owner(
    conn: &Connection,
    reservation: &ExternalAllocationReservation,
) -> Result<()> {
    let matched: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM dedicated_session s JOIN credential_profile p
           ON p.profile_id=s.credential_profile_id
         JOIN execution_workspace w ON w.workspace_id=s.workspace_id
         WHERE s.placement_thread_id=?1 AND s.admitted_capsule_hash=?2 AND s.workspace_id=?3
           AND s.worker_instance_id=?4 AND s.worker_boot_epoch=?5
           AND w.thread_id=s.placement_thread_id AND w.base_snapshot=?6
           AND w.launch_owner='dedicated_worker_session'
           AND p.credential_generation=s.credential_generation
           AND p.lock_owner=s.worker_instance_id)",
        params![
            reservation.placement_thread_id,
            reservation.admitted_capsule_hash,
            reservation.workspace_id,
            reservation.worker_instance_id,
            i64::try_from(reservation.worker_boot_epoch)?,
            reservation.base_snapshot_hash
        ],
        |row| row.get(0),
    )?;
    if !matched {
        bail!("external allocation has no exact locked dedicated-session owner");
    }
    Ok(())
}

fn require_launch_ready_session(conn: &Connection, placement: &str) -> Result<()> {
    let admitted: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM dedicated_session s
            JOIN execution_workspace w ON w.workspace_id=s.workspace_id
          WHERE s.placement_thread_id=?1 AND s.state='admitted' AND s.send_boundary='none'
            AND w.thread_id=s.placement_thread_id
            AND w.launch_owner='dedicated_worker_session' AND w.state='ready')",
        [placement],
        |row| row.get(0),
    )?;
    if !admitted {
        bail!("external allocation requires an unreleased admitted session");
    }
    Ok(())
}

impl RuntimeDb {
    pub fn external_allocation(&self, placement: &str) -> Result<Option<ExternalAllocationRecord>> {
        read(&self.conn, placement)
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
        require_session_owner(&tx, reservation)?;
        require_launch_ready_session(&tx, &reservation.placement_thread_id)?;
        if read_guard(&tx)? >= 256 {
            bail!("node external allocation ceiling reached");
        }
        let now = i64::try_from(lillux::time::timestamp_millis())?;
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
        require_session_owner(&tx, &record.reservation)?;
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
        require_launch_ready_session(&tx, placement)?;
        let now = i64::try_from(lillux::time::timestamp_millis())?;
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
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read(&tx, placement)?.context("external allocation is absent")?;
        occurrence.validate(&record.reservation)?;
        if let Some(prior) = &record.occurrence {
            if prior != occurrence {
                bail!("external allocation occurrence changed");
            }
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
            ensure!(
                revoked,
                "external termination requires durable channel revocation"
            );
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

    pub(crate) fn external_termination_intent(
        &self,
        placement: &str,
    ) -> Result<Option<ExternalTerminationIntent>> {
        let record = read(&self.conn, placement)?.context("external allocation is absent")?;
        let intent: Option<ExternalTerminationIntent> = read_canonical_evidence(
            &self.conn,
            "external_execution_termination_intent",
            "intent_json",
            placement,
        )?;
        if let Some(intent) = &intent {
            intent.validate(
                &record.reservation,
                record
                    .occurrence
                    .as_ref()
                    .context("external termination intent has no occurrence")?,
            )?;
        }
        Ok(intent)
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
mod tests {
    use super::*;

    pub(super) fn reservation(db: &RuntimeDb, suffix: &str) -> ExternalAllocationReservation {
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
        db.conn
            .execute(
                "INSERT INTO execution_workspace(workspace_id,thread_id,launch_owner,backend_id,
             base_snapshot,root_path,state,created_at_ms,updated_at_ms)
             VALUES(?1,?2,'dedicated_worker_session','fixture',?3,'/fixture','ready',1,1)",
                params![workspace, placement, "b".repeat(64)],
            )
            .unwrap();
        let binding = crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
        ExternalAllocationReservation {
            schema: 2,
            placement_thread_id: placement,
            admitted_capsule_hash: "a".repeat(64),
            workspace_id: workspace,
            worker_instance_id: worker,
            worker_boot_epoch: 1,
            base_snapshot_hash: "b".repeat(64),
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
        db.bind_external_allocation("T-one", &occurrence).unwrap();
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
        db.bind_external_allocation("T-one", &occurrence).unwrap();
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
        for (field, changed) in [
            (
                "credential_generation",
                serde_json::Value::String("f".repeat(64)),
            ),
            (
                "runtime_manifest_hash",
                serde_json::Value::String("f".repeat(64)),
            ),
            (
                "runtime_selection_identity",
                serde_json::Value::String("f".repeat(64)),
            ),
            (
                "backend_artifact_hash",
                serde_json::Value::String("f".repeat(64)),
            ),
            ("region", serde_json::Value::String("other".into())),
            ("network_policy", serde_json::Value::String("other".into())),
            ("max_workspace_bytes", serde_json::Value::from(2048)),
            ("max_active", serde_json::Value::from(2)),
            ("timeout_seconds", serde_json::Value::from(61)),
        ] {
            let mut value = exact_value.clone();
            value["document"][field] = changed;
            let changed: crate::node_config::sections::external_execution::RetainedExternalExecutionBinding =
                serde_json::from_value(value).unwrap();
            assert!(
                changed.validate().is_err(),
                "retained field {field} escaped signed-source join"
            );
        }
        let mut wider = reservation.clone();
        wider.max_active = 2;
        assert!(db.reserve_external_allocation(&wider, &exact).is_err());
        let mut missing_owner = reservation.clone();
        missing_owner.placement_thread_id = "T-missing".into();
        missing_owner.workspace_id = "W-missing".into();
        missing_owner.worker_instance_id = "worker-missing".into();
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
        db.bind_external_allocation("T-one", &occurrence).unwrap();
        db.bind_external_allocation("T-one", &occurrence).unwrap();
        let record = db.external_allocation("T-one").unwrap().unwrap();
        assert_eq!(record.phase, ExternalAllocationPhase::Quarantined);
        assert_eq!(record.occurrence, Some(occurrence.clone()));
        let mut wrong = occurrence;
        wrong.occurrence_id = "another-occurrence".into();
        assert!(db.bind_external_allocation("T-one", &wrong).is_err());
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
        wrong.worker_boot_epoch += 1;
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
