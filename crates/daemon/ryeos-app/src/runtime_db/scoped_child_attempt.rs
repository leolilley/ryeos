//! One-shot daemon-owned scoped child attempts.
//!
//! This journal is not a process launcher. In particular, recovery can inspect
//! an uncertain attempt and retire its exact scope, but can never reopen the
//! attempt or mint another launch permission.

use super::*;

pub(super) const JOURNAL_SQL: &str = r#"
CREATE TABLE scoped_child_attempt (
    attempt_id TEXT PRIMARY KEY,
    owner_thread_id TEXT NOT NULL,
    launch_owner TEXT NOT NULL UNIQUE,
    recipe_digest TEXT NOT NULL,
    recipe_generation TEXT NOT NULL,
    scenario_digest TEXT NOT NULL,
    scope_allocation TEXT NOT NULL,
    scope_recovery TEXT,
    process_identity TEXT,
    mount_preparation_evidence TEXT,
    natural_empty_receipt_digest TEXT,
    observation_object_hash TEXT,
    recovery_death_evidence_digest TEXT,
    retirement_evidence_digest TEXT,
    phase TEXT NOT NULL CHECK (phase IN
        ('reserved','unbound_discard_pending','scope_bound','process_attached','release_permitted','natural_scope_empty','bound_retirement_pending','bound_death_proven','retired')),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK ((phase IN ('reserved','unbound_discard_pending') AND scope_recovery IS NULL AND process_identity IS NULL AND mount_preparation_evidence IS NULL)
        OR (phase='scope_bound' AND scope_recovery IS NOT NULL AND process_identity IS NULL AND mount_preparation_evidence IS NULL)
        OR (phase IN ('process_attached','release_permitted','natural_scope_empty')
            AND scope_recovery IS NOT NULL AND process_identity IS NOT NULL AND mount_preparation_evidence IS NOT NULL)
        OR (phase IN ('bound_retirement_pending','bound_death_proven') AND scope_recovery IS NOT NULL)
        OR (phase='retired' AND (scope_recovery IS NOT NULL OR process_identity IS NULL))),
    CHECK ((process_identity IS NULL) = (mount_preparation_evidence IS NULL)),
    CHECK ((phase IN ('natural_scope_empty','bound_retirement_pending','bound_death_proven','retired') OR natural_empty_receipt_digest IS NULL)
        AND ((natural_empty_receipt_digest IS NULL AND observation_object_hash IS NULL)
             OR (natural_empty_receipt_digest IS NOT NULL AND observation_object_hash IS NOT NULL))
        AND (natural_empty_receipt_digest IS NULL OR (scope_recovery IS NOT NULL AND process_identity IS NOT NULL))
        AND (phase='retired' OR retirement_evidence_digest IS NULL)
        AND (recovery_death_evidence_digest IS NULL OR (scope_recovery IS NOT NULL
            AND phase IN ('bound_death_proven','retired')))
        AND (phase NOT IN ('bound_death_proven','retired') OR scope_recovery IS NULL
            OR recovery_death_evidence_digest IS NOT NULL))
);
CREATE TABLE scoped_child_input_operation (
    attempt_id TEXT NOT NULL REFERENCES scoped_child_attempt(attempt_id),
    sequence INTEGER NOT NULL CHECK (sequence >= 0 AND sequence <= 512),
    kind TEXT NOT NULL CHECK (kind IN ('write','close')),
    payload_digest TEXT NOT NULL,
    byte_count INTEGER NOT NULL CHECK (byte_count >= 0 AND byte_count <= 65536),
    phase TEXT NOT NULL CHECK (phase IN ('reserved','delivered')),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (attempt_id, sequence),
    CHECK ((kind='write' AND byte_count > 0) OR (kind='close' AND byte_count=0))
);
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopedChildInputKind {
    Write,
    Close,
}

impl ScopedChildInputKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Close => "close",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopedChildInputReservation {
    Reserved,
    AlreadyDelivered,
}

/// One ordered local input delivery coordinate. `payload_digest` attests the
/// callback bytes, not guest consumption of those bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedChildInputOperation {
    pub sequence: u32,
    pub kind: ScopedChildInputKind,
    pub payload_digest: String,
    pub byte_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopedChildPhase {
    Reserved,
    UnboundDiscardPending,
    ScopeBound,
    ProcessAttached,
    ReleasePermitted,
    NaturalScopeEmpty,
    BoundRetirementPending,
    BoundDeathProven,
    Retired,
}

impl ScopedChildPhase {
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "reserved" => Self::Reserved,
            "unbound_discard_pending" => Self::UnboundDiscardPending,
            "scope_bound" => Self::ScopeBound,
            "process_attached" => Self::ProcessAttached,
            "release_permitted" => Self::ReleasePermitted,
            "natural_scope_empty" => Self::NaturalScopeEmpty,
            "bound_retirement_pending" => Self::BoundRetirementPending,
            "bound_death_proven" => Self::BoundDeathProven,
            "retired" => Self::Retired,
            _ => bail!("invalid scoped child attempt phase"),
        })
    }
}

/// Immutable journal coordinate. The exact `LaunchOwner`, not a thread id or
/// a claimed generation supplied by a Tool, is the owner fence. Trusted
/// verifier admission belongs to the service grant, outside this journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewScopedChildAttempt {
    pub attempt_id: String,
    pub owner: LaunchOwner,
    pub recipe_digest: String,
    pub recipe_generation: String,
    pub scenario_digest: String,
    pub scope_allocation: lillux::ProcessScopeAllocation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedChildAttemptRecord {
    pub initial: NewScopedChildAttempt,
    pub scope_recovery: Option<lillux::ProcessScopeRecovery>,
    pub process_identity: Option<ExecutionProcessIdentity>,
    /// Exact pre-release plan/target-view join committed with process attachment.
    pub mount_preparation_evidence: Option<ScopedChildMountPreparationEvidence>,
    /// Digest of daemon-authored natural scope-empty testimony, not Tool text.
    pub natural_empty_receipt_digest: Option<String>,
    /// CAS object containing the complete bounded observed result and exact
    /// compiled isolation provenance. This is a GC root even after retirement.
    pub observation_object_hash: Option<String>,
    /// Exact Lillux termination/empty proof persisted before scope removal.
    pub recovery_death_evidence_digest: Option<String>,
    /// Digest of independently retained cleanup/retirement evidence.
    pub retirement_evidence_digest: Option<String>,
    pub phase: ScopedChildPhase,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedChildMountPreparationEvidence {
    pub schema: u32,
    pub plan_digest: String,
    pub expected: lillux::LinuxSandboxMountPreparationCommitments,
    pub observed: lillux::LinuxSandboxMountPreparationReceipt,
    /// Daemon-sealed content at exact prepared namespace destinations. This
    /// cannot be reconstructed from the writable workspace after release.
    pub prepared_immutable_sha256: std::collections::BTreeMap<String, String>,
}

impl ScopedChildMountPreparationEvidence {
    fn validate_for(&self, identity: &ExecutionProcessIdentity) -> Result<()> {
        anyhow::ensure!(
            self.schema == 2,
            "scoped child mount evidence schema is unsupported"
        );
        anyhow::ensure!(
            self.prepared_immutable_sha256.len()
                <= ryeos_state::external_content::products::producer_recipe::MAX_PRODUCER_PREPARED_IMMUTABLE_FILES,
            "scoped child immutable file evidence exceeds signed bound"
        );
        for (destination, digest) in &self.prepared_immutable_sha256 {
            let path = std::path::Path::new(destination);
            let parent = path.parent().ok_or_else(|| anyhow!("immutable destination has no parent"))?;
            let root = std::path::Path::new("/ryeos/producer-prepared");
            anyhow::ensure!(
                parent.parent() == Some(root)
                    && parent.file_name().and_then(|name| name.to_str()).is_some_and(|id| {
                        ryeos_state::external_content::products::producer_recipe::prepared_directory_mount_destination(id)
                            .is_ok_and(|expected| expected == parent)
                    })
                    && path.file_name().and_then(|name| name.to_str()).is_some_and(|leaf| {
                        ryeos_state::external_content::products::producer_recipe::ProducerPreparedImmutableFile {
                            prepared_directory_id: parent.file_name().unwrap().to_string_lossy().into_owned(),
                            leaf_name: leaf.to_owned(),
                            maximum_bytes: 1,
                            expected_sha256: digest.clone(),
                        }.validate().is_ok()
                    }),
                "scoped child immutable destination is not canonical"
            );
            require_hex_digest("scoped child immutable content", digest)?;
        }
        let digest = self
            .plan_digest
            .strip_prefix("sha256:")
            .ok_or_else(|| anyhow!("scoped child mount evidence lacks a plan digest"))?;
        require_hex_digest("scoped child plan", digest)?;
        anyhow::ensure!(
            identity.target_pid > 0
                && i64::from(self.observed.owned_child_pid) == identity.target_pid
                && self.observed.matches_commitments(&self.expected),
            "scoped child mount evidence contradicts held process or compiled plan"
        );
        Ok(())
    }

    pub fn validate_observation_plan(
        &self,
        plan_digest: &str,
        identity: &ExecutionProcessIdentity,
    ) -> Result<()> {
        self.validate_for(identity)?;
        anyhow::ensure!(
            self.plan_digest == plan_digest,
            "observed isolation plan differs from retained held mount preparation"
        );
        Ok(())
    }
}

/// In-memory daemon testimony, constructed only by consuming the Lillux
/// process owner through its natural-exit path. This is not a serialized
/// capability. The verifier's signed Tool terminal is authored *after* this
/// observation and must bind this receipt digest; requiring that terminal
/// here would create a circular dependency.
#[derive(Debug, Clone)]
pub struct ScopedChildNaturalEmptyReceipt {
    attempt_id: String,
    owner: LaunchOwner,
    recipe_digest: String,
    recipe_generation: String,
    scenario_digest: String,
    recovery: lillux::ProcessScopeRecovery,
    process_identity: ExecutionProcessIdentity,
    natural_result_success: bool,
    natural_result_exit_code: i32,
    natural_result_timed_out: bool,
    natural_result_stdout_digest: String,
    natural_result_stderr_digest: String,
}

impl ScopedChildNaturalEmptyReceipt {
    pub fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    /// Recompute a retained receipt digest from the exact journal coordinate
    /// and recorded result fields. This is validation only: it does not mint
    /// natural-exit testimony or authorize a journal transition.
    pub fn verify_recorded_digest(
        record: &ScopedChildAttemptRecord,
        receipt_digest: &str,
        subprocess_success: bool,
        exit_code: i32,
        timed_out: bool,
        stdout_digest: &str,
        stderr_digest: &str,
    ) -> Result<()> {
        require_hex_digest("natural empty receipt", receipt_digest)?;
        require_hex_digest("scoped child stdout", stdout_digest)?;
        require_hex_digest("scoped child stderr", stderr_digest)?;
        let receipt = Self {
            attempt_id: record.initial.attempt_id.clone(),
            owner: record.initial.owner.clone(),
            recipe_digest: record.initial.recipe_digest.clone(),
            recipe_generation: record.initial.recipe_generation.clone(),
            scenario_digest: record.initial.scenario_digest.clone(),
            recovery: record
                .scope_recovery
                .clone()
                .ok_or_else(|| anyhow!("recorded scoped child has no scope recovery"))?,
            process_identity: record
                .process_identity
                .clone()
                .ok_or_else(|| anyhow!("recorded scoped child has no process identity"))?,
            natural_result_success: subprocess_success,
            natural_result_exit_code: exit_code,
            natural_result_timed_out: timed_out,
            natural_result_stdout_digest: stdout_digest.to_owned(),
            natural_result_stderr_digest: stderr_digest.to_owned(),
        };
        if receipt.digest()? != receipt_digest {
            bail!("recorded scoped child result contradicts natural-empty receipt");
        }
        Ok(())
    }

    /// Recheck the retained scope outside the StateStore mutex before its
    /// final short journal CAS. Natural exit already proved this once; a
    /// failed fresh observation cannot become success testimony.
    pub fn recheck_scope_empty(&self) -> Result<()> {
        self.recovery
            .wait_empty(self.recovery.control_timeout())
            .map_err(anyhow::Error::msg)
    }

    /// A failed natural wait returns the still-owned process for ordinary
    /// abort/retry; it never produces a receipt from an exit status alone.
    pub fn observe(
        running: lillux::RunningProcess,
        record: &ScopedChildAttemptRecord,
        timeout: std::time::Duration,
    ) -> std::result::Result<(lillux::SubprocessResult, Self), lillux::RunningProcess> {
        let Some(recovery) = record.scope_recovery.as_ref() else {
            return Err(running);
        };
        let Some(identity) = record.process_identity.as_ref() else {
            return Err(running);
        };
        if record.phase != ScopedChildPhase::ReleasePermitted
            || running.scope_recovery() != Some(recovery)
            || i64::from(running.pid) != identity.target_pid
            || running.pgid != identity.group_leader_pid
            || identity.process_scope.as_ref() != Some(recovery)
        {
            return Err(running);
        }
        let result = running.wait_for_natural_exit(timeout)?;
        let receipt = Self {
            attempt_id: record.initial.attempt_id.clone(),
            owner: record.initial.owner.clone(),
            recipe_digest: record.initial.recipe_digest.clone(),
            recipe_generation: record.initial.recipe_generation.clone(),
            scenario_digest: record.initial.scenario_digest.clone(),
            recovery: recovery.clone(),
            process_identity: identity.clone(),
            natural_result_success: result.success,
            natural_result_exit_code: result.exit_code,
            natural_result_timed_out: result.timed_out,
            natural_result_stdout_digest: lillux::sha256_hex(result.stdout.as_bytes()),
            natural_result_stderr_digest: lillux::sha256_hex(result.stderr.as_bytes()),
        };
        Ok((result, receipt))
    }

    pub fn digest(&self) -> Result<String> {
        let canonical = lillux::canonical_json(&serde_json::json!({
            "kind": "scoped_child_natural_empty_v1",
            "attempt_id": self.attempt_id,
            "owner": self.owner,
            "recipe_digest": self.recipe_digest,
            "recipe_generation": self.recipe_generation,
            "scenario_digest": self.scenario_digest,
            "scope_recovery": self.recovery,
            "process_identity": self.process_identity,
            "natural_exit": true,
            "natural_wait_empty": true,
            "natural_result_success": self.natural_result_success,
            "natural_result_exit_code": self.natural_result_exit_code,
            "natural_result_timed_out": self.natural_result_timed_out,
            "natural_result_stdout_digest": self.natural_result_stdout_digest,
            "natural_result_stderr_digest": self.natural_result_stderr_digest,
        }))?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }
}

fn require_hex_digest(label: &str, digest: &str) -> Result<()> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("{label} must be a lowercase SHA-256 digest");
    }
    Ok(())
}

impl NewScopedChildAttempt {
    fn validate(&self) -> Result<()> {
        validate_bounded_runtime_text("scoped child attempt id", &self.attempt_id, 128)?;
        validate_runtime_thread_id(&self.owner.thread_id)?;
        validate_bounded_runtime_text(
            "scoped child launch nonce",
            &self.owner.unpredictable_nonce,
            256,
        )?;
        validate_bounded_runtime_text(
            "scoped child daemon generation",
            &self.owner.daemon_generation_id,
            256,
        )?;
        if self.owner.monotonic_launch_epoch == 0 {
            bail!("scoped child launch owner has no epoch");
        }
        require_hex_digest("scoped child recipe", &self.recipe_digest)?;
        require_hex_digest("scoped child scenario", &self.scenario_digest)?;
        validate_bounded_runtime_text(
            "scoped child recipe generation",
            &self.recipe_generation,
            256,
        )?;
        self.scope_allocation
            .validate()
            .map_err(anyhow::Error::msg)?;
        Ok(())
    }
}

impl RuntimeDb {
    /// Point-read an exact delivered input operation without requiring its
    /// process or pipe to remain live. This is an acknowledgement lookup, not
    /// authority to reserve or contact the target after observation. An
    /// existing reserved operation remains contact-uncertain and cannot be
    /// acknowledged or replayed.
    pub fn delivered_scoped_child_input_operation(
        &self,
        attempt_id: &str,
        owner: &LaunchOwner,
        operation: &ScopedChildInputOperation,
    ) -> Result<bool> {
        require_hex_digest("scoped child input payload", &operation.payload_digest)?;
        let encoded_owner = lillux::canonical_json(&serde_json::to_value(owner)?)?;
        let retained_owner: Option<String> = self
            .conn
            .query_row(
                "SELECT launch_owner FROM scoped_child_attempt WHERE attempt_id=?1",
                [attempt_id],
                |row| row.get(0),
            )
            .optional()?;
        if retained_owner.as_deref() != Some(encoded_owner.as_str()) {
            bail!("scoped child input acknowledgement has no exact attempt owner");
        }
        let retained: Option<(String, String, u32, String)> = self
            .conn
            .query_row(
                "SELECT kind, payload_digest, byte_count, phase
                 FROM scoped_child_input_operation WHERE attempt_id=?1 AND sequence=?2",
                params![attempt_id, operation.sequence],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((kind, digest, byte_count, phase)) = retained else {
            return Ok(false);
        };
        if kind != operation.kind.as_str()
            || digest != operation.payload_digest
            || byte_count != operation.byte_count
        {
            bail!("scoped child input acknowledgement differs from retained operation");
        }
        if phase != "delivered" {
            bail!("scoped child input acknowledgement is contact-uncertain");
        }
        Ok(true)
    }

    /// Refuse interactive natural observation until the exact owner has a
    /// complete ordered input journal ending in a delivered half-close.
    /// Input EOF is still not process or whole-scope settlement.
    pub fn assert_scoped_child_input_closed(
        &self,
        attempt_id: &str,
        owner: &LaunchOwner,
    ) -> Result<()> {
        let encoded_owner = lillux::canonical_json(&serde_json::to_value(owner)?)?;
        let retained: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT launch_owner, phase FROM scoped_child_attempt WHERE attempt_id=?1",
                [attempt_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((retained_owner, attempt_phase)) = retained else {
            bail!("scoped child input attempt is absent before observation");
        };
        if retained_owner != encoded_owner || attempt_phase != "release_permitted" {
            bail!("scoped child input owner or phase differs before observation");
        }
        let mut statement = self.conn.prepare(
            "SELECT sequence, kind, phase FROM scoped_child_input_operation
             WHERE attempt_id=?1 ORDER BY sequence",
        )?;
        let mut rows = statement.query([attempt_id])?;
        let mut expected = 0_u32;
        let mut last_was_close = false;
        while let Some(row) = rows.next()? {
            let sequence: u32 = row.get(0)?;
            let kind: String = row.get(1)?;
            let phase: String = row.get(2)?;
            if sequence != expected || phase != "delivered" || last_was_close {
                bail!("scoped child input is incomplete or out of order");
            }
            expected = expected
                .checked_add(1)
                .ok_or_else(|| anyhow!("scoped child input sequence exhausted"))?;
            if kind == "close" {
                last_was_close = true;
            } else if kind != "write" {
                bail!("scoped child input has invalid operation kind");
            }
        }
        if !last_was_close {
            bail!("scoped child input has no delivered close");
        }
        Ok(())
    }

    /// Reserve one exact ordered input operation before any channel write or
    /// close. A pending operation is contact-uncertain: neither it nor any
    /// later sequence may be sent after an ambiguous response or restart.
    pub fn reserve_scoped_child_input_operation(
        &self,
        attempt_id: &str,
        owner: &LaunchOwner,
        operation: &ScopedChildInputOperation,
        source: &ryeos_state::external_content::products::producer_recipe::ProducerStdinSource,
    ) -> Result<ScopedChildInputReservation> {
        use ryeos_state::external_content::products::producer_recipe::ProducerStdinSource;

        let ProducerStdinSource::InteractiveVerifierChannel {
            maximum_frame_bytes,
            maximum_total_bytes,
            maximum_frames,
        } = source
        else {
            bail!("scoped child input requires signed interactive authority");
        };
        require_hex_digest("scoped child input payload", &operation.payload_digest)?;
        if owner.daemon_generation_id != daemon_generation_id()
            || operation.sequence > *maximum_frames
            || (operation.kind == ScopedChildInputKind::Write
                && (operation.byte_count == 0
                    || operation.byte_count > *maximum_frame_bytes
                    || operation.sequence >= *maximum_frames))
            || (operation.kind == ScopedChildInputKind::Close
                && (operation.byte_count != 0
                    || operation.payload_digest != lillux::sha256_hex(b"")))
        {
            bail!("scoped child input exceeds signed bounds or current owner");
        }
        let encoded_owner = lillux::canonical_json(&serde_json::to_value(owner)?)?;
        let tx = self.conn.unchecked_transaction()?;
        let admitted: Option<(String, String, String)> = tx
            .query_row(
                "SELECT launch_owner, phase, owner_thread_id FROM scoped_child_attempt WHERE attempt_id=?1",
                [attempt_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((retained_owner, phase, thread_id)) = admitted else {
            bail!("scoped child input attempt is absent");
        };
        if retained_owner != encoded_owner || thread_id != owner.thread_id {
            bail!("scoped child input owner differs from retained attempt");
        }
        let mut statement = tx.prepare(
            "SELECT sequence, kind, payload_digest, byte_count, phase
             FROM scoped_child_input_operation WHERE attempt_id=?1 ORDER BY sequence",
        )?;
        let mut rows = statement.query([attempt_id])?;
        let mut next_sequence = 0_u32;
        let mut total_bytes = 0_u64;
        let mut pending = false;
        let mut closed = false;
        while let Some(row) = rows.next()? {
            let sequence: u32 = row.get(0)?;
            let kind: String = row.get(1)?;
            let digest: String = row.get(2)?;
            let byte_count: u32 = row.get(3)?;
            let retained_phase: String = row.get(4)?;
            if sequence != next_sequence {
                bail!("scoped child input journal is not contiguous");
            }
            next_sequence = next_sequence
                .checked_add(1)
                .ok_or_else(|| anyhow!("scoped child input sequence exhausted"))?;
            if kind == "write" {
                total_bytes = total_bytes
                    .checked_add(u64::from(byte_count))
                    .ok_or_else(|| anyhow!("scoped child input total overflowed"))?;
            } else if kind == "close" {
                closed = true;
            } else {
                bail!("scoped child input journal has invalid operation kind");
            }
            if retained_phase == "reserved" {
                pending = true;
            } else if retained_phase != "delivered" {
                bail!("scoped child input journal has invalid phase");
            }
            if sequence == operation.sequence {
                if kind != operation.kind.as_str()
                    || digest != operation.payload_digest
                    || byte_count != operation.byte_count
                {
                    bail!("scoped child input replay differs from retained operation");
                }
                if retained_phase == "delivered" {
                    return Ok(ScopedChildInputReservation::AlreadyDelivered);
                }
                bail!("scoped child input delivery is uncertain");
            }
        }
        drop(rows);
        drop(statement);
        if phase != "release_permitted" || pending || closed || operation.sequence != next_sequence
        {
            bail!("scoped child input is not the next live operation");
        }
        if operation.kind == ScopedChildInputKind::Write
            && total_bytes
                .checked_add(u64::from(operation.byte_count))
                .is_none_or(|sum| sum > *maximum_total_bytes)
        {
            bail!("scoped child input exceeds signed total bytes");
        }
        let owner_live: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM thread_runtime r JOIN thread_launch_claim c ON c.thread_id=r.thread_id
                 WHERE r.thread_id=?1 AND r.stop_requested_at_ms IS NULL
                   AND c.claimed_by=?2 AND c.claim_id=?3)",
            params![owner.thread_id, encoded_owner, owner.unpredictable_nonce],
            |row| row.get(0),
        )?;
        if !owner_live {
            bail!("scoped child input root has stopped or lost its launch claim");
        }
        let now = lillux::time::timestamp_millis();
        tx.execute(
            "INSERT INTO scoped_child_input_operation
             (attempt_id, sequence, kind, payload_digest, byte_count, phase, created_at_ms, updated_at_ms)
             VALUES (?1,?2,?3,?4,?5,'reserved',?6,?6)",
            params![attempt_id, operation.sequence, operation.kind.as_str(),
                operation.payload_digest, operation.byte_count, now],
        )?;
        tx.commit()?;
        Ok(ScopedChildInputReservation::Reserved)
    }

    /// Record only a completed local Lillux write/close. This does not claim
    /// that the guest read or applied the bytes. A failed or partial write
    /// leaves the reservation uncertain and blocks every later operation.
    pub fn complete_scoped_child_input_operation(
        &self,
        attempt_id: &str,
        owner: &LaunchOwner,
        operation: &ScopedChildInputOperation,
    ) -> Result<()> {
        require_hex_digest("scoped child input payload", &operation.payload_digest)?;
        if owner.daemon_generation_id != daemon_generation_id() {
            bail!("scoped child input completion names another daemon generation");
        }
        let encoded_owner = lillux::canonical_json(&serde_json::to_value(owner)?)?;
        let changed = self.conn.execute(
            "UPDATE scoped_child_input_operation SET phase='delivered', updated_at_ms=?6
             WHERE attempt_id=?1 AND sequence=?2 AND kind=?3 AND payload_digest=?4
               AND byte_count=?5 AND phase='reserved'
               AND EXISTS (
                   SELECT 1 FROM scoped_child_attempt a
                   JOIN thread_runtime r ON r.thread_id=a.owner_thread_id
                   JOIN thread_launch_claim c ON c.thread_id=r.thread_id
                   WHERE a.attempt_id=?1 AND a.launch_owner=?7
                     AND r.stop_requested_at_ms IS NULL
                     AND c.claimed_by=?7 AND c.claim_id=?8)",
            params![
                attempt_id,
                operation.sequence,
                operation.kind.as_str(),
                operation.payload_digest,
                operation.byte_count,
                lillux::time::timestamp_millis(),
                encoded_owner,
                owner.unpredictable_nonce
            ],
        )?;
        if changed != 1 {
            bail!("scoped child input completion lost its exact live pending authority");
        }
        Ok(())
    }

    /// Commit the attempt before asking Lillux to allocate a scope. A repeated
    /// request, including an exact repeat after lost ACK, never authorizes a
    /// second allocation or launch.
    pub fn reserve_scoped_child_attempt(&self, initial: &NewScopedChildAttempt) -> Result<()> {
        initial.validate()?;
        if initial.owner.daemon_generation_id != daemon_generation_id() {
            bail!("scoped child attempt names another daemon generation");
        }
        let owner = lillux::canonical_json(&serde_json::to_value(&initial.owner)?)?;
        let allocation = lillux::canonical_json(&serde_json::to_value(&initial.scope_allocation)?)?;
        let now = lillux::time::timestamp_millis();
        let lifetime = initial
            .scope_allocation
            .host_lifetime()
            .map_err(anyhow::Error::msg)?;
        let tx = self.conn.unchecked_transaction()?;
        // A retained child pins the exact host occurrence. The fence is not
        // allowed to disappear or move while that attempt remains unsettled,
        // even if a later caller presents an otherwise valid launch claim.
        validate_unsettled_scoped_child_lifetime(&tx)?;
        if let Some(incumbent) = read_scope_lifetime_fence(&tx)? {
            if incumbent != lifetime {
                if !incumbent.has_ended().map_err(anyhow::Error::msg)?
                    || self.unsettled_process_scope_count(None)? != 0
                {
                    bail!("scoped child reservation crosses an unsettled host-lifetime fence");
                }
            }
        }
        let encoded_lifetime = lillux::canonical_json(&serde_json::to_value(&lifetime)?)?;
        tx.execute(
            "UPDATE execution_lifetime_fence SET host_lifetime=?1 WHERE singleton=1",
            [&encoded_lifetime],
        )?;
        let changed = tx.execute(
            "INSERT INTO scoped_child_attempt
             (attempt_id, owner_thread_id, launch_owner, recipe_digest, recipe_generation,
              scenario_digest, scope_allocation, phase, created_at_ms, updated_at_ms)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, 'reserved', ?8, ?8
             WHERE EXISTS (SELECT 1 FROM thread_runtime WHERE thread_id=?2 AND stop_requested_at_ms IS NULL)
               AND EXISTS (SELECT 1 FROM thread_launch_claim
                            WHERE thread_id=?2 AND claimed_by=?3 AND claim_id=?9)",
            params![initial.attempt_id, initial.owner.thread_id, owner, initial.recipe_digest,
                initial.recipe_generation, initial.scenario_digest, allocation, now,
                initial.owner.unpredictable_nonce],
        )?;
        if changed != 1 {
            bail!("scoped child attempt lacks its exact admitted launch owner");
        }
        tx.commit()?;
        Ok(())
    }

    /// One-shot binding of the exact allocated scope. Restart does not retry
    /// allocation: a reserved row is cleanup/reconciliation work only.
    pub fn bind_scoped_child_scope(
        &self,
        attempt_id: &str,
        recovery: &lillux::ProcessScopeRecovery,
    ) -> Result<()> {
        let record = self
            .get_scoped_child_attempt(attempt_id)?
            .ok_or_else(|| anyhow!("unknown scoped child attempt"))?;
        if record.phase != ScopedChildPhase::Reserved
            || record.initial.owner.daemon_generation_id != daemon_generation_id()
            || !recovery.matches_allocation(&record.initial.scope_allocation)
        {
            bail!("scoped child binding lost its original unbound allocation");
        }
        let encoded = lillux::canonical_json(&serde_json::to_value(recovery)?)?;
        let owner = lillux::canonical_json(&serde_json::to_value(&record.initial.owner)?)?;
        let changed = self.conn.execute(
            "UPDATE scoped_child_attempt SET scope_recovery=?2, phase='scope_bound', updated_at_ms=?3
             WHERE attempt_id=?1 AND phase='reserved' AND scope_recovery IS NULL AND process_identity IS NULL
               AND EXISTS (SELECT 1 FROM thread_launch_claim WHERE thread_id=?4 AND claimed_by=?5)
               AND EXISTS (SELECT 1 FROM thread_runtime WHERE thread_id=?4 AND stop_requested_at_ms IS NULL)",
            params![attempt_id, encoded, lillux::time::timestamp_millis(), record.initial.owner.thread_id, owner],
        )?;
        if changed != 1 {
            bail!("scoped child scope binding lost its one-shot CAS");
        }
        Ok(())
    }

    /// Attach a held process before target release. The process's own scoped
    /// identity must match the original allocation's exact recovery record.
    pub fn attach_scoped_child_process(
        &self,
        attempt_id: &str,
        identity: &ExecutionProcessIdentity,
        mount_evidence: &ScopedChildMountPreparationEvidence,
    ) -> Result<()> {
        validate_execution_process_identity_shape(identity)?;
        mount_evidence.validate_for(identity)?;
        let record = self
            .get_scoped_child_attempt(attempt_id)?
            .ok_or_else(|| anyhow!("unknown scoped child attempt"))?;
        if record.phase != ScopedChildPhase::ScopeBound
            || record.initial.owner.daemon_generation_id != daemon_generation_id()
            || identity.process_scope.as_ref() != record.scope_recovery.as_ref()
        {
            bail!("held scoped child identity contradicts its bound scope");
        }
        let encoded = lillux::canonical_json(&serde_json::to_value(identity)?)?;
        let encoded_mount = lillux::canonical_json(&serde_json::to_value(mount_evidence)?)?;
        let recovery = lillux::canonical_json(&serde_json::to_value(
            record.scope_recovery.as_ref().unwrap(),
        )?)?;
        let owner = lillux::canonical_json(&serde_json::to_value(&record.initial.owner)?)?;
        let changed = self.conn.execute(
            "UPDATE scoped_child_attempt SET process_identity=?3, mount_preparation_evidence=?4,
                    phase='process_attached', updated_at_ms=?5
             WHERE attempt_id=?1 AND phase='scope_bound' AND scope_recovery=?2
               AND process_identity IS NULL AND mount_preparation_evidence IS NULL
               AND EXISTS (SELECT 1 FROM thread_launch_claim WHERE thread_id=?6 AND claimed_by=?7)
               AND EXISTS (SELECT 1 FROM thread_runtime WHERE thread_id=?6 AND stop_requested_at_ms IS NULL)",
            params![attempt_id, recovery, encoded, encoded_mount, lillux::time::timestamp_millis(), record.initial.owner.thread_id, owner],
        )?;
        if changed != 1 {
            bail!("held scoped child attachment lost its one-shot CAS");
        }
        Ok(())
    }

    /// Irreversible pre-release cut. Once committed, recovery must assume the
    /// target may have run even if the caller never received an ACK.
    pub fn permit_scoped_child_release(
        &self,
        attempt_id: &str,
        identity: &ExecutionProcessIdentity,
    ) -> Result<()> {
        let record = self
            .get_scoped_child_attempt(attempt_id)?
            .ok_or_else(|| anyhow!("unknown scoped child attempt"))?;
        if record.phase != ScopedChildPhase::ProcessAttached
            || record.initial.owner.daemon_generation_id != daemon_generation_id()
            || record.process_identity.as_ref() != Some(identity)
        {
            bail!("scoped child release lost its exact held process");
        }
        let encoded = lillux::canonical_json(&serde_json::to_value(identity)?)?;
        let owner = lillux::canonical_json(&serde_json::to_value(&record.initial.owner)?)?;
        let changed = self.conn.execute(
            "UPDATE scoped_child_attempt SET phase='release_permitted', updated_at_ms=?3
             WHERE attempt_id=?1 AND phase='process_attached' AND process_identity=?2
               AND EXISTS (SELECT 1 FROM thread_launch_claim WHERE thread_id=?4 AND claimed_by=?5)
               AND EXISTS (SELECT 1 FROM thread_runtime WHERE thread_id=?4 AND stop_requested_at_ms IS NULL)",
            params![attempt_id, encoded, lillux::time::timestamp_millis(), record.initial.owner.thread_id, owner],
        )?;
        if changed != 1 {
            bail!("scoped child release permission was already consumed");
        }
        Ok(())
    }

    /// Persist daemon-observed natural emptiness before the verifier signs its
    /// terminal. This is separate from cleanup-only retirement. The signed
    /// verifier terminal must subsequently bind this exact receipt digest;
    /// the receipt alone does not qualify a runtime.
    pub fn record_scoped_child_natural_empty(
        &self,
        receipt: &ScopedChildNaturalEmptyReceipt,
        observation_object_hash: &str,
    ) -> Result<()> {
        require_hex_digest("scoped child observation object", observation_object_hash)?;
        let attempt_id = &receipt.attempt_id;
        let recovery = &receipt.recovery;
        let record = self
            .get_scoped_child_attempt(attempt_id)?
            .ok_or_else(|| anyhow!("unknown scoped child attempt"))?;
        if record.phase != ScopedChildPhase::ReleasePermitted
            || record.initial.owner.daemon_generation_id != daemon_generation_id()
            || record.scope_recovery.as_ref() != Some(recovery)
            || record.process_identity.as_ref() != Some(&receipt.process_identity)
            || record.initial.owner != receipt.owner
            || record.initial.recipe_digest != receipt.recipe_digest
            || record.initial.recipe_generation != receipt.recipe_generation
            || record.initial.scenario_digest != receipt.scenario_digest
        {
            bail!("natural scope-empty observation lacks exact released producer scope");
        }
        // This is a fresh observation of the retained exact scope, not an
        // inference from receipt fields or a caller-supplied digest.
        // The potentially blocking wait ran before taking StateStore's global
        // mutex. At this CAS, make a fresh bounded kernel observation instead
        // of blocking unrelated thread reads for the full control timeout.
        if !recovery
            .is_empty(std::time::Duration::from_millis(100))
            .map_err(anyhow::Error::msg)?
        {
            bail!("scoped child is no longer empty at natural receipt commit");
        }
        let receipt_digest = receipt.digest()?;
        let encoded_recovery = lillux::canonical_json(&serde_json::to_value(recovery)?)?;
        let owner = lillux::canonical_json(&serde_json::to_value(&record.initial.owner)?)?;
        let changed = self.conn.execute(
            "UPDATE scoped_child_attempt SET natural_empty_receipt_digest=?3, observation_object_hash=?4,
                    phase='natural_scope_empty', updated_at_ms=?5
             WHERE attempt_id=?1 AND phase='release_permitted' AND scope_recovery=?2
               AND natural_empty_receipt_digest IS NULL AND observation_object_hash IS NULL
               AND EXISTS (SELECT 1 FROM thread_launch_claim WHERE thread_id=?6 AND claimed_by=?7)
               AND EXISTS (SELECT 1 FROM thread_runtime WHERE thread_id=?6 AND stop_requested_at_ms IS NULL)",
            params![attempt_id, encoded_recovery, receipt_digest, observation_object_hash,
                lillux::time::timestamp_millis(), record.initial.owner.thread_id, owner],
        )?;
        if changed != 1 {
            bail!("natural scope-empty observation lost its one-shot CAS");
        }
        Ok(())
    }

    /// Exact verifier-requested abort of a released attempt. Unlike generic
    /// retirement, this cannot win after natural observation has committed;
    /// its single CAS fences the two outcomes against each other.
    pub fn claim_released_scoped_child_abort(
        &self,
        attempt_id: &str,
        recovery: &lillux::ProcessScopeRecovery,
    ) -> Result<()> {
        let encoded_recovery = lillux::canonical_json(&serde_json::to_value(recovery)?)?;
        let changed = self.conn.execute(
            "UPDATE scoped_child_attempt SET phase='bound_retirement_pending', updated_at_ms=?3
             WHERE attempt_id=?1 AND phase='release_permitted' AND scope_recovery=?2
               AND natural_empty_receipt_digest IS NULL AND observation_object_hash IS NULL
               AND retirement_evidence_digest IS NULL",
            params![
                attempt_id,
                encoded_recovery,
                lillux::time::timestamp_millis()
            ],
        )?;
        if changed != 1 {
            bail!("scoped abort lost its exact released-attempt retirement CAS");
        }
        Ok(())
    }

    /// Irreversibly fence attachment and release before retiring the bound
    /// scope. Recovery from this phase may only finish cleanup.
    pub fn claim_bound_scoped_child_retirement(
        &self,
        attempt_id: &str,
        recovery: &lillux::ProcessScopeRecovery,
    ) -> Result<()> {
        let record = self
            .get_scoped_child_attempt(attempt_id)?
            .ok_or_else(|| anyhow!("unknown scoped child attempt"))?;
        if !matches!(
            record.phase,
            ScopedChildPhase::ScopeBound
                | ScopedChildPhase::ProcessAttached
                | ScopedChildPhase::ReleasePermitted
                | ScopedChildPhase::NaturalScopeEmpty
        ) || record.scope_recovery.as_ref() != Some(recovery)
        {
            bail!("scoped child retirement lacks its exact unsettled scope");
        }
        let encoded_recovery = lillux::canonical_json(&serde_json::to_value(recovery)?)?;
        let changed = self.conn.execute(
            "UPDATE scoped_child_attempt SET phase='bound_retirement_pending', updated_at_ms=?3
             WHERE attempt_id=?1 AND scope_recovery=?2 AND phase=?4 AND retirement_evidence_digest IS NULL",
            params![attempt_id, encoded_recovery, lillux::time::timestamp_millis(), match record.phase {
                ScopedChildPhase::ScopeBound => "scope_bound",
                ScopedChildPhase::ProcessAttached => "process_attached",
                ScopedChildPhase::ReleasePermitted => "release_permitted",
                ScopedChildPhase::NaturalScopeEmpty => "natural_scope_empty",
                _ => unreachable!(),
            }],
        )?;
        if changed != 1 {
            bail!("scoped child retirement claim lost its one-shot CAS");
        }
        Ok(())
    }

    /// Persist a successful exact Lillux termination/empty observation before
    /// removing the scope. This cut is cleanup testimony, never natural-exit
    /// or qualification testimony. Repeated recovery from this phase skips
    /// termination and only completes idempotent retirement.
    pub(crate) fn record_bound_scoped_child_death(
        &self,
        attempt_id: &str,
        recovery: &lillux::ProcessScopeRecovery,
    ) -> Result<()> {
        let record = self
            .get_scoped_child_attempt(attempt_id)?
            .ok_or_else(|| anyhow!("unknown scoped child attempt"))?;
        if record.phase != ScopedChildPhase::BoundRetirementPending
            || record.scope_recovery.as_ref() != Some(recovery)
        {
            bail!("scoped child death proof lacks its exact pending scope");
        }
        let operation = lillux::canonical_json(&serde_json::json!({
            "kind": "lillux_scoped_child_terminate_and_wait_v1",
            "attempt_id": attempt_id,
            "recovery": recovery,
        }))?;
        let evidence_digest = lillux::sha256_hex(operation.as_bytes());
        let encoded_recovery = lillux::canonical_json(&serde_json::to_value(recovery)?)?;
        let changed = self.conn.execute(
            "UPDATE scoped_child_attempt
                SET recovery_death_evidence_digest=?3, phase='bound_death_proven', updated_at_ms=?4
              WHERE attempt_id=?1 AND phase='bound_retirement_pending' AND scope_recovery=?2
                AND recovery_death_evidence_digest IS NULL AND retirement_evidence_digest IS NULL",
            params![
                attempt_id,
                encoded_recovery,
                evidence_digest,
                lillux::time::timestamp_millis()
            ],
        )?;
        if changed != 1 {
            bail!("scoped child death proof lost its one-shot CAS");
        }
        Ok(())
    }

    /// Commit retirement after StateStore completed Lillux's exact
    /// `retire_after_settlement` outside its global mutex. The death-proof
    /// phase forbids any further launch or release while this CAS is pending.
    pub(crate) fn complete_bound_scoped_child_retirement(
        &self,
        attempt_id: &str,
        recovery: &lillux::ProcessScopeRecovery,
    ) -> Result<()> {
        let record = self
            .get_scoped_child_attempt(attempt_id)?
            .ok_or_else(|| anyhow!("unknown scoped child attempt"))?;
        if record.phase != ScopedChildPhase::BoundDeathProven
            || record.recovery_death_evidence_digest.is_none()
        {
            bail!("bound scoped child lacks durable death proof");
        }
        if record.scope_recovery.as_ref() != Some(recovery) {
            bail!("bound retirement changed its exact recovery scope");
        }
        let operation = lillux::canonical_json(&serde_json::json!({
            "kind": "lillux_retire_after_settlement", "attempt_id": attempt_id,
            "recovery": recovery,
        }))?;
        let evidence_digest = lillux::sha256_hex(operation.as_bytes());
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE scoped_child_attempt SET retirement_evidence_digest=?2, phase='retired', updated_at_ms=?3
             WHERE attempt_id=?1 AND phase='bound_death_proven'
               AND recovery_death_evidence_digest IS NOT NULL AND retirement_evidence_digest IS NULL",
            params![attempt_id, evidence_digest, lillux::time::timestamp_millis()],
        )?;
        if changed != 1 {
            bail!("bound scoped child retirement lost its one-shot CAS");
        }
        if self.unsettled_process_scope_count(None)? == 0 {
            tx.execute(
                "UPDATE execution_lifetime_fence SET host_lifetime=NULL WHERE singleton=1",
                [],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Fence the old creator before checking the unbound allocation. A live
    /// launch claim must first be cleared by its existing daemon lifecycle;
    /// this journal does not terminate or supersede that creator.
    pub fn claim_unbound_scoped_child_discard(&self, attempt_id: &str) -> Result<()> {
        let record = self
            .get_scoped_child_attempt(attempt_id)?
            .ok_or_else(|| anyhow!("unknown scoped child attempt"))?;
        if record.phase != ScopedChildPhase::Reserved {
            bail!("unbound scoped child discard lacks its exact reservation");
        }
        let changed = self.conn.execute(
            "UPDATE scoped_child_attempt SET phase='unbound_discard_pending', updated_at_ms=?2
             WHERE attempt_id=?1 AND phase='reserved' AND scope_recovery IS NULL
               AND process_identity IS NULL AND retirement_evidence_digest IS NULL
               AND NOT EXISTS (SELECT 1 FROM thread_launch_claim WHERE thread_id=?3)",
            params![
                attempt_id,
                lillux::time::timestamp_millis(),
                record.initial.owner.thread_id
            ],
        )?;
        if changed != 1 {
            bail!("unbound scoped child discard requires the old creator to be fenced");
        }
        Ok(())
    }

    /// Reconcile the exact unbound slot using Lillux itself. A failed or
    /// interrupted discard leaves the pending row for retry, never for launch.
    pub fn complete_unbound_scoped_child_discard(&self, attempt_id: &str) -> Result<()> {
        let record = self
            .get_scoped_child_attempt(attempt_id)?
            .ok_or_else(|| anyhow!("unknown scoped child attempt"))?;
        if record.phase != ScopedChildPhase::UnboundDiscardPending {
            bail!("unbound scoped child allocation is not pending discard");
        }
        record
            .initial
            .scope_allocation
            .discard_unlaunched()
            .map_err(anyhow::Error::msg)?;
        let operation = lillux::canonical_json(&serde_json::json!({
            "kind": "lillux_discard_unlaunched",
            "attempt_id": attempt_id,
            "allocation": record.initial.scope_allocation,
        }))?;
        let evidence_digest = lillux::sha256_hex(operation.as_bytes());
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE scoped_child_attempt SET retirement_evidence_digest=?2, phase='retired', updated_at_ms=?3
             WHERE attempt_id=?1 AND phase='unbound_discard_pending'
               AND scope_recovery IS NULL AND process_identity IS NULL
               AND retirement_evidence_digest IS NULL",
            params![attempt_id, evidence_digest, lillux::time::timestamp_millis()],
        )?;
        if changed != 1 {
            bail!("unbound scoped child discard lost its one-shot CAS");
        }
        if self.unsettled_process_scope_count(None)? == 0 {
            tx.execute(
                "UPDATE execution_lifetime_fence SET host_lifetime=NULL WHERE singleton=1",
                [],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Point lookup is recovery evidence, never executable launch authority.
    pub fn get_scoped_child_attempt(
        &self,
        attempt_id: &str,
    ) -> Result<Option<ScopedChildAttemptRecord>> {
        read_scoped_child_attempt(&self.conn, attempt_id)
    }

    /// Exact root-owner point lookup for lost START acknowledgements. The
    /// owner is UNIQUE; this never selects a new signed recipe or attempt.
    pub fn get_scoped_child_attempt_for_owner(
        &self,
        owner: &LaunchOwner,
    ) -> Result<Option<ScopedChildAttemptRecord>> {
        let encoded = lillux::canonical_json(&serde_json::to_value(owner)?)?;
        let attempt_id: Option<String> = self
            .conn
            .query_row(
                "SELECT attempt_id FROM scoped_child_attempt WHERE owner_thread_id=?1 AND launch_owner=?2",
                params![owner.thread_id, encoded],
                |row| row.get(0),
            )
            .optional()?;
        attempt_id
            .as_deref()
            .map(|id| read_scoped_child_attempt(&self.conn, id))
            .transpose()
            .map(|record| record.flatten())
    }

    pub fn unsettled_scoped_child_attempt_ids(&self) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare("SELECT attempt_id FROM scoped_child_attempt WHERE phase!='retired' ORDER BY created_at_ms, attempt_id")?;
        let ids = statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(ids)
    }

    pub fn has_unsettled_scoped_child_for_thread(&self, thread_id: &str) -> Result<bool> {
        let found: i64 = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM scoped_child_attempt
             WHERE owner_thread_id=?1 AND phase!='retired')",
            [thread_id],
            |row| row.get(0),
        )?;
        Ok(found != 0)
    }

    /// Retain exact observed-result CAS objects through scope retirement and
    /// thread history pruning. A durable pointer must never be swept first.
    pub fn scoped_child_observation_cas_roots(&self) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare(
            "SELECT observation_object_hash FROM scoped_child_attempt
             WHERE observation_object_hash IS NOT NULL ORDER BY attempt_id",
        )?;
        let roots = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for root in &roots {
            require_hex_digest("scoped child observation CAS root", root)?;
        }
        Ok(roots)
    }
}

fn read_scoped_child_attempt(
    conn: &Connection,
    attempt_id: &str,
) -> Result<Option<ScopedChildAttemptRecord>> {
    let row: Option<(
        String,
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        i64,
        i64,
    )> = conn
        .query_row(
            "SELECT owner_thread_id, launch_owner, recipe_digest, recipe_generation,
                    scenario_digest, scope_allocation, scope_recovery, process_identity,
                    mount_preparation_evidence,
                    natural_empty_receipt_digest, observation_object_hash, recovery_death_evidence_digest,
                    retirement_evidence_digest,
                    phase, created_at_ms, updated_at_ms
               FROM scoped_child_attempt WHERE attempt_id=?1",
            [attempt_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                    row.get(14)?,
                    row.get(15)?,
                ))
            },
        )
        .optional()?;
    let Some((
        owner_thread_id,
        owner_json,
        recipe_digest,
        recipe_generation,
        scenario_digest,
        allocation_json,
        recovery_json,
        identity_json,
        mount_evidence_json,
        natural_empty_receipt_digest,
        observation_object_hash,
        recovery_death_evidence_digest,
        retirement_evidence_digest,
        phase,
        created_at_ms,
        updated_at_ms,
    )) = row
    else {
        return Ok(None);
    };
    let owner: LaunchOwner = serde_json::from_str(&owner_json)?;
    let allocation: lillux::ProcessScopeAllocation = serde_json::from_str(&allocation_json)?;
    let recovery: Option<lillux::ProcessScopeRecovery> = recovery_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?;
    let identity: Option<ExecutionProcessIdentity> = identity_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?;
    let mount_evidence: Option<ScopedChildMountPreparationEvidence> = mount_evidence_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?;
    let initial = NewScopedChildAttempt {
        attempt_id: attempt_id.to_owned(),
        owner,
        recipe_digest,
        recipe_generation,
        scenario_digest,
        scope_allocation: allocation,
    };
    initial.validate()?;
    let phase = ScopedChildPhase::parse(&phase)?;
    if owner_thread_id != initial.owner.thread_id
        || lillux::canonical_json(&serde_json::to_value(&initial.owner)?)? != owner_json
        || lillux::canonical_json(&serde_json::to_value(&initial.scope_allocation)?)?
            != allocation_json
        || recovery_json.as_ref().is_some_and(|json| {
            lillux::canonical_json(&serde_json::to_value(recovery.as_ref().unwrap()).unwrap())
                .ok()
                .as_ref()
                != Some(json)
        })
        || identity_json.as_ref().is_some_and(|json| {
            lillux::canonical_json(&serde_json::to_value(identity.as_ref().unwrap()).unwrap())
                .ok()
                .as_ref()
                != Some(json)
        })
        || mount_evidence_json.as_ref().is_some_and(|json| {
            lillux::canonical_json(&serde_json::to_value(mount_evidence.as_ref().unwrap()).unwrap())
                .ok()
                .as_ref()
                != Some(json)
        })
        || mount_evidence.as_ref().is_some_and(|evidence| {
            identity
                .as_ref()
                .is_none_or(|identity| evidence.validate_for(identity).is_err())
        })
        || identity.is_some() != mount_evidence.is_some()
        || recovery
            .as_ref()
            .is_some_and(|value| !value.matches_allocation(&initial.scope_allocation))
        || identity.as_ref().is_some_and(|value| {
            value.process_scope.as_ref() != recovery.as_ref()
                || validate_execution_process_identity_shape(value).is_err()
        })
        || natural_empty_receipt_digest
            .as_ref()
            .is_some_and(|digest| require_hex_digest("natural empty receipt", digest).is_err())
        || observation_object_hash.as_ref().is_some_and(|digest| {
            require_hex_digest("scoped child observation object", digest).is_err()
        })
        || natural_empty_receipt_digest.is_some() != observation_object_hash.is_some()
        || retirement_evidence_digest
            .as_ref()
            .is_some_and(|digest| require_hex_digest("retirement evidence", digest).is_err())
        || recovery_death_evidence_digest
            .as_ref()
            .is_some_and(|digest| require_hex_digest("recovery death evidence", digest).is_err())
        || (natural_empty_receipt_digest.is_some() && (recovery.is_none() || identity.is_none()))
        || created_at_ms <= 0
        || updated_at_ms < created_at_ms
    {
        bail!("retained scoped child attempt is not canonical");
    }
    let shape_ok = match phase {
        ScopedChildPhase::Reserved | ScopedChildPhase::UnboundDiscardPending => {
            recovery.is_none() && identity.is_none() && mount_evidence.is_none()
        }
        ScopedChildPhase::ScopeBound => {
            recovery.is_some() && identity.is_none() && mount_evidence.is_none()
        }
        ScopedChildPhase::BoundRetirementPending | ScopedChildPhase::BoundDeathProven => {
            recovery.is_some()
        }
        ScopedChildPhase::Retired => recovery.is_some() || identity.is_none(),
        _ => recovery.is_some() && identity.is_some() && mount_evidence.is_some(),
    };
    if !shape_ok {
        bail!("retained scoped child attempt has invalid phase fields");
    }
    if (matches!(phase, ScopedChildPhase::NaturalScopeEmpty)
        && natural_empty_receipt_digest.is_none())
        || (matches!(phase, ScopedChildPhase::Retired) != retirement_evidence_digest.is_some())
        || (matches!(phase, ScopedChildPhase::Retired)
            && recovery.is_some()
            && recovery_death_evidence_digest.is_none())
        || (matches!(phase, ScopedChildPhase::BoundDeathProven)
            && recovery_death_evidence_digest.is_none())
        || (recovery_death_evidence_digest.is_some()
            && !matches!(
                phase,
                ScopedChildPhase::BoundDeathProven | ScopedChildPhase::Retired
            ))
        || (natural_empty_receipt_digest.is_some()
            && !matches!(
                phase,
                ScopedChildPhase::NaturalScopeEmpty
                    | ScopedChildPhase::BoundRetirementPending
                    | ScopedChildPhase::BoundDeathProven
                    | ScopedChildPhase::Retired
            ))
    {
        bail!("retained scoped child evidence contradicts its phase");
    }
    Ok(Some(ScopedChildAttemptRecord {
        initial,
        scope_recovery: recovery,
        process_identity: identity,
        mount_preparation_evidence: mount_evidence,
        natural_empty_receipt_digest,
        observation_object_hash,
        recovery_death_evidence_digest,
        retirement_evidence_digest,
        phase,
        created_at_ms,
        updated_at_ms,
    }))
}

pub(super) fn validate_current(conn: &Connection) -> Result<()> {
    let ids = {
        let mut statement =
            conn.prepare("SELECT attempt_id FROM scoped_child_attempt ORDER BY attempt_id")?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for id in ids {
        read_scoped_child_attempt(conn, &id)?
            .ok_or_else(|| anyhow!("scoped child attempt disappeared during validation"))?;
    }
    validate_unsettled_scoped_child_lifetime(conn)?;
    Ok(())
}

fn validate_unsettled_scoped_child_lifetime(conn: &Connection) -> Result<()> {
    let fence = read_scope_lifetime_fence(conn)?;
    let mut statement = conn.prepare(
        "SELECT attempt_id FROM scoped_child_attempt WHERE phase!='retired' ORDER BY attempt_id",
    )?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        let attempt = read_scoped_child_attempt(conn, &id)?
            .ok_or_else(|| anyhow!("unsettled scoped child attempt disappeared"))?;
        let lifetime = attempt
            .initial
            .scope_allocation
            .host_lifetime()
            .map_err(anyhow::Error::msg)?;
        if fence.as_ref() != Some(&lifetime) {
            bail!("unsettled scoped child attempt contradicts host-lifetime fence");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (
        NewScopedChildAttempt,
        lillux::ProcessScopeRecovery,
        ExecutionProcessIdentity,
    ) {
        let recovery: lillux::ProcessScopeRecovery = serde_json::from_value(serde_json::json!({
            "version": 4, "control_timeout": {"secs": 1, "nanos": 0},
            "configuration": {"version": 3, "backend": {
                "implementation": "linux_cgroup_v2", "parent": "/fixture/delegation"
            }},
            "backend": {"implementation": "linux_cgroup_v2",
                "boot_id": "00000000-0000-4000-8000-000000000000",
                "parent": {"containing_device": 1, "inode": 2},
                "directory": {"containing_device": 1, "inode": 3},
                "name": "producer-one"}
        }))
        .unwrap();
        let mut planned = serde_json::to_value(&recovery).unwrap();
        planned["version"] = 2.into();
        planned["backend"]
            .as_object_mut()
            .unwrap()
            .remove("directory");
        let scope_allocation = serde_json::from_value(planned).unwrap();
        let initial = NewScopedChildAttempt {
            attempt_id: "producer-one".to_owned(),
            owner: LaunchOwner {
                thread_id: "T-owner".to_owned(),
                monotonic_launch_epoch: 1,
                unpredictable_nonce: "claim-one".to_owned(),
                daemon_generation_id: daemon_generation_id().to_owned(),
            },
            recipe_digest: "a".repeat(64),
            recipe_generation: "signed-generation-one".to_owned(),
            scenario_digest: "b".repeat(64),
            scope_allocation,
        };
        let identity = ExecutionProcessIdentity {
            schema_version: PROCESS_IDENTITY_SCHEMA_VERSION,
            boot_id: "00000000-0000-4000-8000-000000000000".to_owned(),
            target_pid: 123,
            target_start_time_ticks: 10,
            group_leader_pid: 123,
            group_leader_start_time_ticks: 10,
            process_scope: Some(recovery.clone()),
            resource_selections: Vec::new(),
            resource_operations: Vec::new(),
            resource_allocation_limit: None,
            resource_occupancy_start: None,
            resource_occupancy_limit: None,
            resource_cleanup_allowance_ms: None,
        };
        (initial, recovery, identity)
    }

    fn seed_owner(db: &RuntimeDb, owner: &LaunchOwner) {
        db.insert_thread_runtime(&owner.thread_id, &owner.thread_id)
            .unwrap();
        let encoded = lillux::canonical_json(&serde_json::to_value(owner).unwrap()).unwrap();
        db.conn.execute(
            "INSERT INTO thread_launch_claim(thread_id,claim_id,claimed_at_ms,lease_expires_at_ms,claimed_by,rearm_resume_budget_if_stale)
             VALUES (?1,?2,1,9223372036854775807,?3,0)",
            params![owner.thread_id, owner.unpredictable_nonce, encoded],
        ).unwrap();
    }

    fn mount_evidence(identity: &ExecutionProcessIdentity) -> ScopedChildMountPreparationEvidence {
        ScopedChildMountPreparationEvidence {
            schema: 2,
            plan_digest: format!("sha256:{}", "c".repeat(64)),
            expected: lillux::LinuxSandboxMountPreparationCommitments {
                schema: 1,
                mount_count: 1,
                destination_access_sha256: [7; 32],
            },
            observed: lillux::LinuxSandboxMountPreparationReceipt {
                schema: 1,
                owned_child_pid: identity.target_pid as u32,
                mount_count: 1,
                destination_access_sha256: [7; 32],
            },
            prepared_immutable_sha256: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn mount_evidence_requires_exact_observed_plan_digest() {
        let (_, _, identity) = fixture();
        let evidence = mount_evidence(&identity);
        evidence.validate_observation_plan(&evidence.plan_digest, &identity).unwrap();
        assert!(evidence.validate_observation_plan(
            &format!("sha256:{}", "d".repeat(64)),
            &identity,
        ).is_err());
        let mut wrong = evidence.clone();
        wrong.prepared_immutable_sha256.insert(
            "/ryeos/producer-prepared/codex-home/../config.toml".into(),
            "a".repeat(64),
        );
        assert!(wrong.validate_for(&identity).is_err());
        wrong.prepared_immutable_sha256.clear();
        wrong.prepared_immutable_sha256.insert(
            "/ryeos/producer-prepared/codex-home/config.toml".into(),
            "A".repeat(64),
        );
        assert!(wrong.validate_for(&identity).is_err());
        wrong.prepared_immutable_sha256.insert(
            "/ryeos/producer-prepared/codex-home/config.toml".into(),
            "a".repeat(64),
        );
        wrong.validate_for(&identity).unwrap();
    }

    #[test]
    fn duplicate_changed_and_wrong_owner_never_reopen_attempt() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let (initial, recovery, identity) = fixture();
        assert!(db.reserve_scoped_child_attempt(&initial).is_err());
        seed_owner(&db, &initial.owner);
        db.reserve_scoped_child_attempt(&initial).unwrap();
        assert!(
            db.has_unsettled_scoped_child_for_thread(&initial.owner.thread_id)
                .unwrap()
        );
        assert_eq!(
            db.get_scoped_child_attempt_for_owner(&initial.owner)
                .unwrap()
                .unwrap()
                .initial,
            initial
        );
        let mut another_owner = initial.owner.clone();
        another_owner.unpredictable_nonce = "other-owner".to_owned();
        assert!(
            db.get_scoped_child_attempt_for_owner(&another_owner)
                .unwrap()
                .is_none()
        );
        assert!(db.reserve_scoped_child_attempt(&initial).is_err());
        let mut changed = initial.clone();
        changed.attempt_id = "producer-two".to_owned();
        changed.scenario_digest = "c".repeat(64);
        assert!(db.reserve_scoped_child_attempt(&changed).is_err());
        let mut false_owner = changed.clone();
        false_owner.owner.unpredictable_nonce = "another-claim".to_owned();
        assert!(db.reserve_scoped_child_attempt(&false_owner).is_err());
        let mut wrong_recovery = serde_json::to_value(&recovery).unwrap();
        wrong_recovery["backend"]["name"] = "another-scope".into();
        let wrong_recovery = serde_json::from_value(wrong_recovery).unwrap();
        assert!(
            db.bind_scoped_child_scope(&initial.attempt_id, &wrong_recovery)
                .is_err()
        );
        db.bind_scoped_child_scope(&initial.attempt_id, &recovery)
            .unwrap();
        assert!(
            db.bind_scoped_child_scope(&initial.attempt_id, &recovery)
                .is_err()
        );
        let mut wrong = identity.clone();
        wrong.target_pid = 999;
        wrong.process_scope = None;
        let mut wrong_mount = mount_evidence(&identity);
        wrong_mount.observed.destination_access_sha256[0] ^= 1;
        assert!(db.attach_scoped_child_process(&initial.attempt_id, &identity, &wrong_mount).is_err());
        assert!(
            db.attach_scoped_child_process(&initial.attempt_id, &wrong, &mount_evidence(&identity))
                .is_err()
        );
        db.attach_scoped_child_process(&initial.attempt_id, &identity, &mount_evidence(&identity))
            .unwrap();
        assert!(
            db.attach_scoped_child_process(&initial.attempt_id, &identity, &mount_evidence(&identity))
                .is_err()
        );
        assert!(
            db.permit_scoped_child_release(&initial.attempt_id, &wrong)
                .is_err()
        );
        db.permit_scoped_child_release(&initial.attempt_id, &identity)
            .unwrap();
        assert!(
            db.permit_scoped_child_release(&initial.attempt_id, &identity)
                .is_err()
        );
        assert_eq!(
            db.get_scoped_child_attempt(&initial.attempt_id)
                .unwrap()
                .unwrap()
                .phase,
            ScopedChildPhase::ReleasePermitted
        );
    }

    #[test]
    fn scoped_input_pending_blocks_later_contact_and_delivered_replays_exactly() {
        use ryeos_state::external_content::products::producer_recipe::ProducerStdinSource;

        let db = RuntimeDb::new_in_memory().unwrap();
        let (initial, recovery, identity) = fixture();
        seed_owner(&db, &initial.owner);
        db.reserve_scoped_child_attempt(&initial).unwrap();
        db.bind_scoped_child_scope(&initial.attempt_id, &recovery)
            .unwrap();
        db.attach_scoped_child_process(&initial.attempt_id, &identity, &mount_evidence(&identity))
            .unwrap();
        db.permit_scoped_child_release(&initial.attempt_id, &identity)
            .unwrap();
        let limits = ProducerStdinSource::InteractiveVerifierChannel {
            maximum_frame_bytes: 4,
            maximum_total_bytes: 7,
            maximum_frames: 4,
        };
        let first = ScopedChildInputOperation {
            sequence: 0,
            kind: ScopedChildInputKind::Write,
            payload_digest: lillux::sha256_hex(b"abcd"),
            byte_count: 4,
        };
        assert!(
            !db.delivered_scoped_child_input_operation(&initial.attempt_id, &initial.owner, &first)
                .unwrap()
        );
        let second = ScopedChildInputOperation {
            sequence: 1,
            kind: ScopedChildInputKind::Write,
            payload_digest: lillux::sha256_hex(b"ef"),
            byte_count: 2,
        };
        assert_eq!(
            db.reserve_scoped_child_input_operation(
                &initial.attempt_id,
                &initial.owner,
                &first,
                &limits,
            )
            .unwrap(),
            ScopedChildInputReservation::Reserved
        );
        assert!(
            db.reserve_scoped_child_input_operation(
                &initial.attempt_id,
                &initial.owner,
                &first,
                &limits,
            )
            .is_err()
        );
        assert!(
            db.delivered_scoped_child_input_operation(&initial.attempt_id, &initial.owner, &first)
                .is_err()
        );
        assert!(
            db.reserve_scoped_child_input_operation(
                &initial.attempt_id,
                &initial.owner,
                &second,
                &limits,
            )
            .is_err()
        );
        let premature_close = ScopedChildInputOperation {
            sequence: 1,
            kind: ScopedChildInputKind::Close,
            payload_digest: lillux::sha256_hex(b""),
            byte_count: 0,
        };
        assert!(
            db.reserve_scoped_child_input_operation(
                &initial.attempt_id,
                &initial.owner,
                &premature_close,
                &limits,
            )
            .is_err()
        );
        for mismatch in [
            ScopedChildInputOperation {
                payload_digest: lillux::sha256_hex(b"wxyz"),
                ..first.clone()
            },
            ScopedChildInputOperation {
                byte_count: 3,
                ..first.clone()
            },
            ScopedChildInputOperation {
                kind: ScopedChildInputKind::Close,
                ..first.clone()
            },
            ScopedChildInputOperation {
                sequence: 1,
                ..first.clone()
            },
        ] {
            assert!(
                db.complete_scoped_child_input_operation(
                    &initial.attempt_id,
                    &initial.owner,
                    &mismatch,
                )
                .is_err()
            );
        }
        let mut wrong_owner = initial.owner.clone();
        wrong_owner.unpredictable_nonce = "wrong-owner".to_owned();
        assert!(
            db.complete_scoped_child_input_operation(&initial.attempt_id, &wrong_owner, &first)
                .is_err()
        );
        db.complete_scoped_child_input_operation(&initial.attempt_id, &initial.owner, &first)
            .unwrap();
        assert!(
            db.delivered_scoped_child_input_operation(&initial.attempt_id, &initial.owner, &first)
                .unwrap()
        );
        assert!(
            db.delivered_scoped_child_input_operation(&initial.attempt_id, &wrong_owner, &first)
                .is_err()
        );
        assert_eq!(
            db.reserve_scoped_child_input_operation(
                &initial.attempt_id,
                &initial.owner,
                &first,
                &limits,
            )
            .unwrap(),
            ScopedChildInputReservation::AlreadyDelivered
        );
        let changed = ScopedChildInputOperation {
            payload_digest: lillux::sha256_hex(b"wxyz"),
            ..first.clone()
        };
        assert!(
            db.delivered_scoped_child_input_operation(
                &initial.attempt_id,
                &initial.owner,
                &changed
            )
            .is_err()
        );
        assert!(
            db.reserve_scoped_child_input_operation(
                &initial.attempt_id,
                &initial.owner,
                &changed,
                &limits,
            )
            .is_err()
        );
        let over_total = ScopedChildInputOperation {
            payload_digest: lillux::sha256_hex(b"efgh"),
            byte_count: 4,
            ..second.clone()
        };
        assert!(
            db.reserve_scoped_child_input_operation(
                &initial.attempt_id,
                &initial.owner,
                &over_total,
                &limits,
            )
            .is_err()
        );
        assert_eq!(
            db.reserve_scoped_child_input_operation(
                &initial.attempt_id,
                &initial.owner,
                &second,
                &limits,
            )
            .unwrap(),
            ScopedChildInputReservation::Reserved
        );
        db.complete_scoped_child_input_operation(&initial.attempt_id, &initial.owner, &second)
            .unwrap();
        let close = ScopedChildInputOperation {
            sequence: 2,
            kind: ScopedChildInputKind::Close,
            payload_digest: lillux::sha256_hex(b""),
            byte_count: 0,
        };
        assert!(
            db.assert_scoped_child_input_closed(&initial.attempt_id, &initial.owner)
                .is_err()
        );
        db.reserve_scoped_child_input_operation(
            &initial.attempt_id,
            &initial.owner,
            &close,
            &limits,
        )
        .unwrap();
        assert!(
            db.assert_scoped_child_input_closed(&initial.attempt_id, &initial.owner)
                .is_err()
        );
        db.complete_scoped_child_input_operation(&initial.attempt_id, &initial.owner, &close)
            .unwrap();
        db.assert_scoped_child_input_closed(&initial.attempt_id, &initial.owner)
            .unwrap();
        assert!(
            db.assert_scoped_child_input_closed(&initial.attempt_id, &wrong_owner)
                .is_err()
        );
        assert_eq!(
            db.reserve_scoped_child_input_operation(
                &initial.attempt_id,
                &initial.owner,
                &close,
                &limits,
            )
            .unwrap(),
            ScopedChildInputReservation::AlreadyDelivered
        );
        let later = ScopedChildInputOperation {
            sequence: 3,
            payload_digest: lillux::sha256_hex(b"g"),
            byte_count: 1,
            ..second
        };
        assert!(
            db.reserve_scoped_child_input_operation(
                &initial.attempt_id,
                &initial.owner,
                &later,
                &limits,
            )
            .is_err()
        );
    }

    #[test]
    fn scoped_input_pending_survives_reopen_and_stop_blocks_new_reservation() {
        use ryeos_state::external_content::products::producer_recipe::ProducerStdinSource;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let (initial, recovery, identity) = fixture();
        let limits = ProducerStdinSource::InteractiveVerifierChannel {
            maximum_frame_bytes: 4,
            maximum_total_bytes: 8,
            maximum_frames: 2,
        };
        let first = ScopedChildInputOperation {
            sequence: 0,
            kind: ScopedChildInputKind::Write,
            payload_digest: lillux::sha256_hex(b"abcd"),
            byte_count: 4,
        };
        {
            let db = RuntimeDb::open(&path).unwrap();
            seed_owner(&db, &initial.owner);
            db.reserve_scoped_child_attempt(&initial).unwrap();
            db.bind_scoped_child_scope(&initial.attempt_id, &recovery)
                .unwrap();
            db.attach_scoped_child_process(&initial.attempt_id, &identity, &mount_evidence(&identity))
                .unwrap();
            db.permit_scoped_child_release(&initial.attempt_id, &identity)
                .unwrap();
            db.reserve_scoped_child_input_operation(
                &initial.attempt_id,
                &initial.owner,
                &first,
                &limits,
            )
            .unwrap();
        }
        let db = RuntimeDb::open_existing_current(&path).unwrap();
        assert!(
            db.reserve_scoped_child_input_operation(
                &initial.attempt_id,
                &initial.owner,
                &first,
                &limits,
            )
            .is_err()
        );
        let second = ScopedChildInputOperation {
            sequence: 1,
            ..first.clone()
        };
        assert!(
            db.reserve_scoped_child_input_operation(
                &initial.attempt_id,
                &initial.owner,
                &second,
                &limits,
            )
            .is_err()
        );
        db.complete_scoped_child_input_operation(&initial.attempt_id, &initial.owner, &first)
            .unwrap();
        drop(db);
        let db = RuntimeDb::open_existing_current(&path).unwrap();
        assert_eq!(
            db.reserve_scoped_child_input_operation(
                &initial.attempt_id,
                &initial.owner,
                &first,
                &limits,
            )
            .unwrap(),
            ScopedChildInputReservation::AlreadyDelivered
        );
        assert_eq!(
            db.reserve_scoped_child_input_operation(
                &initial.attempt_id,
                &initial.owner,
                &second,
                &limits,
            )
            .unwrap(),
            ScopedChildInputReservation::Reserved
        );
        db.conn
            .execute(
                "UPDATE thread_runtime SET stop_requested_at_ms=1 WHERE thread_id=?1",
                [&initial.owner.thread_id],
            )
            .unwrap();
        assert!(
            db.delivered_scoped_child_input_operation(&initial.attempt_id, &initial.owner, &first)
                .unwrap()
        );
        assert!(
            db.complete_scoped_child_input_operation(&initial.attempt_id, &initial.owner, &second)
                .is_err()
        );
        assert!(
            db.delivered_scoped_child_input_operation(&initial.attempt_id, &initial.owner, &second)
                .is_err()
        );
        assert!(
            db.reserve_scoped_child_input_operation(
                &initial.attempt_id,
                &initial.owner,
                &second,
                &limits,
            )
            .is_err()
        );
    }

    #[test]
    fn crash_cut_remains_uncertain_and_cleanup_only_without_natural_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("runtime.sqlite3");
        let (initial, recovery, identity) = fixture();
        {
            let db = RuntimeDb::open(&path).unwrap();
            seed_owner(&db, &initial.owner);
            db.reserve_scoped_child_attempt(&initial).unwrap();
            db.bind_scoped_child_scope(&initial.attempt_id, &recovery)
                .unwrap();
            db.attach_scoped_child_process(&initial.attempt_id, &identity, &mount_evidence(&identity))
                .unwrap();
            db.permit_scoped_child_release(&initial.attempt_id, &identity)
                .unwrap();
        }
        let db = RuntimeDb::open_existing_current(&path).unwrap();
        let retained = db
            .get_scoped_child_attempt(&initial.attempt_id)
            .unwrap()
            .unwrap();
        assert_eq!(retained.phase, ScopedChildPhase::ReleasePermitted);
        assert_eq!(
            retained.mount_preparation_evidence,
            Some(mount_evidence(&identity))
        );
        assert!(retained.natural_empty_receipt_digest.is_none());
        assert_eq!(
            db.unsettled_scoped_child_attempt_ids().unwrap(),
            vec![initial.attempt_id.clone()]
        );
        assert!(db.reserve_scoped_child_attempt(&initial).is_err());
        assert!(
            db.permit_scoped_child_release(&initial.attempt_id, &identity)
                .is_err()
        );
        db.claim_bound_scoped_child_retirement(&initial.attempt_id, &recovery)
            .unwrap();
        assert_eq!(
            db.get_scoped_child_attempt(&initial.attempt_id)
                .unwrap()
                .unwrap()
                .phase,
            ScopedChildPhase::BoundRetirementPending
        );
        assert!(
            db.complete_bound_scoped_child_retirement(&initial.attempt_id, &recovery)
                .is_err()
        );
        db.record_bound_scoped_child_death(&initial.attempt_id, &recovery)
            .unwrap();
        recovery.retire_after_settlement().unwrap();
        db.complete_bound_scoped_child_retirement(&initial.attempt_id, &recovery)
            .unwrap();
        let retired = db
            .get_scoped_child_attempt(&initial.attempt_id)
            .unwrap()
            .unwrap();
        assert_eq!(retired.phase, ScopedChildPhase::Retired);
        assert!(retired.natural_empty_receipt_digest.is_none());
        assert!(db.unsettled_scoped_child_attempt_ids().unwrap().is_empty());
        assert!(db.reserve_scoped_child_attempt(&initial).is_err());
    }

    #[test]
    fn exact_abort_retirement_fences_later_natural_observation() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let (initial, recovery, identity) = fixture();
        seed_owner(&db, &initial.owner);
        db.reserve_scoped_child_attempt(&initial).unwrap();
        db.bind_scoped_child_scope(&initial.attempt_id, &recovery)
            .unwrap();
        db.attach_scoped_child_process(&initial.attempt_id, &identity, &mount_evidence(&identity))
            .unwrap();
        db.permit_scoped_child_release(&initial.attempt_id, &identity)
            .unwrap();
        db.claim_released_scoped_child_abort(&initial.attempt_id, &recovery)
            .unwrap();
        let receipt = ScopedChildNaturalEmptyReceipt {
            attempt_id: initial.attempt_id.clone(),
            owner: initial.owner.clone(),
            recipe_digest: initial.recipe_digest.clone(),
            recipe_generation: initial.recipe_generation.clone(),
            scenario_digest: initial.scenario_digest.clone(),
            recovery: recovery.clone(),
            process_identity: identity,
            natural_result_success: true,
            natural_result_exit_code: 0,
            natural_result_timed_out: false,
            natural_result_stdout_digest: "d".repeat(64),
            natural_result_stderr_digest: "e".repeat(64),
        };
        assert!(
            db.record_scoped_child_natural_empty(&receipt, &"c".repeat(64))
                .is_err()
        );
        let retained = db
            .get_scoped_child_attempt(&initial.attempt_id)
            .unwrap()
            .unwrap();
        assert_eq!(retained.phase, ScopedChildPhase::BoundRetirementPending);
        assert!(retained.observation_object_hash.is_none());
        assert!(retained.natural_empty_receipt_digest.is_none());
    }

    #[test]
    fn death_proof_is_exact_durable_and_never_reopens_release_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("runtime.sqlite3");
        let (initial, recovery, identity) = fixture();
        {
            let db = RuntimeDb::open(&path).unwrap();
            seed_owner(&db, &initial.owner);
            db.reserve_scoped_child_attempt(&initial).unwrap();
            db.bind_scoped_child_scope(&initial.attempt_id, &recovery)
                .unwrap();
            db.attach_scoped_child_process(&initial.attempt_id, &identity, &mount_evidence(&identity))
                .unwrap();
            db.permit_scoped_child_release(&initial.attempt_id, &identity)
                .unwrap();
            db.claim_bound_scoped_child_retirement(&initial.attempt_id, &recovery)
                .unwrap();
            assert!(
                db.complete_bound_scoped_child_retirement(&initial.attempt_id, &recovery)
                    .is_err()
            );
            let mut wrong = serde_json::to_value(&recovery).unwrap();
            wrong["backend"]["name"] = "not-producer-one".into();
            let wrong = serde_json::from_value(wrong).unwrap();
            assert!(
                db.record_bound_scoped_child_death(&initial.attempt_id, &wrong)
                    .is_err()
            );
            assert_eq!(
                db.get_scoped_child_attempt(&initial.attempt_id)
                    .unwrap()
                    .unwrap()
                    .phase,
                ScopedChildPhase::BoundRetirementPending
            );
            db.record_bound_scoped_child_death(&initial.attempt_id, &recovery)
                .unwrap();
        }
        let db = RuntimeDb::open_existing_current(&path).unwrap();
        let retained = db
            .get_scoped_child_attempt(&initial.attempt_id)
            .unwrap()
            .unwrap();
        assert_eq!(retained.phase, ScopedChildPhase::BoundDeathProven);
        assert!(retained.recovery_death_evidence_digest.is_some());
        assert!(retained.natural_empty_receipt_digest.is_none());
        assert!(
            db.record_bound_scoped_child_death(&initial.attempt_id, &recovery)
                .is_err()
        );
        assert!(
            db.permit_scoped_child_release(&initial.attempt_id, &identity)
                .is_err()
        );
        recovery.retire_after_settlement().unwrap();
        db.complete_bound_scoped_child_retirement(&initial.attempt_id, &recovery)
            .unwrap();
        assert_eq!(
            db.get_scoped_child_attempt(&initial.attempt_id)
                .unwrap()
                .unwrap()
                .phase,
            ScopedChildPhase::Retired
        );
        assert!(db.unsettled_scoped_child_attempt_ids().unwrap().is_empty());
    }

    #[test]
    fn natural_empty_is_distinct_from_cleanup_and_bound_to_exact_scope() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let (initial, recovery, identity) = fixture();
        seed_owner(&db, &initial.owner);
        db.reserve_scoped_child_attempt(&initial).unwrap();
        db.bind_scoped_child_scope(&initial.attempt_id, &recovery)
            .unwrap();
        // Test-only construction exercises journal binding. Production can
        // obtain this private-field type only through `observe`.
        let receipt = ScopedChildNaturalEmptyReceipt {
            attempt_id: initial.attempt_id.clone(),
            owner: initial.owner.clone(),
            recipe_digest: initial.recipe_digest.clone(),
            recipe_generation: initial.recipe_generation.clone(),
            scenario_digest: initial.scenario_digest.clone(),
            recovery: recovery.clone(),
            process_identity: identity.clone(),
            natural_result_success: true,
            natural_result_exit_code: 0,
            natural_result_timed_out: false,
            natural_result_stdout_digest: "d".repeat(64),
            natural_result_stderr_digest: "e".repeat(64),
        };
        assert!(
            db.record_scoped_child_natural_empty(&receipt, &"c".repeat(64))
                .is_err()
        );
        db.attach_scoped_child_process(&initial.attempt_id, &identity, &mount_evidence(&identity))
            .unwrap();
        db.permit_scoped_child_release(&initial.attempt_id, &identity)
            .unwrap();
        let mut wrong = ScopedChildNaturalEmptyReceipt { ..receipt.clone() };
        let mut wrong_recovery = serde_json::to_value(&recovery).unwrap();
        wrong_recovery["backend"]["directory"]["inode"] = 99.into();
        wrong.recovery = serde_json::from_value(wrong_recovery).unwrap();
        assert!(
            db.record_scoped_child_natural_empty(&wrong, &"c".repeat(64))
                .is_err()
        );
        let mut wrong_recipe = receipt.clone();
        wrong_recipe.recipe_digest = "c".repeat(64);
        assert!(
            db.record_scoped_child_natural_empty(&wrong_recipe, &"c".repeat(64))
                .is_err()
        );
        db.record_scoped_child_natural_empty(&receipt, &"c".repeat(64))
            .unwrap();
        assert!(
            db.claim_released_scoped_child_abort(&initial.attempt_id, &recovery)
                .is_err()
        );
        assert!(
            db.record_scoped_child_natural_empty(&receipt, &"d".repeat(64))
                .is_err()
        );
        db.claim_bound_scoped_child_retirement(&initial.attempt_id, &recovery)
            .unwrap();
        db.record_bound_scoped_child_death(&initial.attempt_id, &recovery)
            .unwrap();
        recovery.retire_after_settlement().unwrap();
        db.complete_bound_scoped_child_retirement(&initial.attempt_id, &recovery)
            .unwrap();
        let retained = db
            .get_scoped_child_attempt(&initial.attempt_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            retained.natural_empty_receipt_digest.as_deref(),
            Some(receipt.digest().unwrap().as_str())
        );
        ScopedChildNaturalEmptyReceipt::verify_recorded_digest(
            &retained,
            &receipt.digest().unwrap(),
            true,
            0,
            false,
            &"d".repeat(64),
            &"e".repeat(64),
        )
        .unwrap();
        assert!(
            ScopedChildNaturalEmptyReceipt::verify_recorded_digest(
                &retained,
                &receipt.digest().unwrap(),
                false,
                0,
                false,
                &"d".repeat(64),
                &"e".repeat(64),
            )
            .is_err()
        );
        assert_eq!(
            retained.observation_object_hash.as_deref(),
            Some("c".repeat(64).as_str())
        );
        assert_eq!(
            db.scoped_child_observation_cas_roots().unwrap(),
            vec!["c".repeat(64)]
        );
        assert_eq!(retained.phase, ScopedChildPhase::Retired);
        assert!(
            !db.has_unsettled_scoped_child_for_thread(&initial.owner.thread_id)
                .unwrap()
        );
    }

    #[test]
    fn unbound_attempt_requires_exact_discard_and_retires_without_process_claim() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let (initial, recovery, _) = fixture();
        seed_owner(&db, &initial.owner);
        db.reserve_scoped_child_attempt(&initial).unwrap();
        assert!(read_scope_lifetime_fence(&db.conn).unwrap().is_some());
        assert!(
            db.claim_unbound_scoped_child_discard(&initial.attempt_id)
                .is_err()
        );
        db.conn
            .execute(
                "DELETE FROM thread_launch_claim WHERE thread_id=?1",
                [&initial.owner.thread_id],
            )
            .unwrap();
        db.claim_unbound_scoped_child_discard(&initial.attempt_id)
            .unwrap();
        assert_eq!(
            db.get_scoped_child_attempt(&initial.attempt_id)
                .unwrap()
                .unwrap()
                .phase,
            ScopedChildPhase::UnboundDiscardPending
        );
        assert!(
            db.bind_scoped_child_scope(&initial.attempt_id, &recovery)
                .is_err()
        );
        db.complete_unbound_scoped_child_discard(&initial.attempt_id)
            .unwrap();
        let retained = db
            .get_scoped_child_attempt(&initial.attempt_id)
            .unwrap()
            .unwrap();
        assert_eq!(retained.phase, ScopedChildPhase::Retired);
        assert!(retained.scope_recovery.is_none());
        assert!(retained.process_identity.is_none());
        assert!(retained.natural_empty_receipt_digest.is_none());
        assert!(
            db.bind_scoped_child_scope(&initial.attempt_id, &recovery)
                .is_err()
        );
        assert!(
            db.complete_unbound_scoped_child_discard(&initial.attempt_id)
                .is_err()
        );
        assert!(read_scope_lifetime_fence(&db.conn).unwrap().is_none());
    }

    #[test]
    fn startup_rejects_corrupt_retained_scoped_child_row() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("runtime.sqlite3");
        let (initial, _, _) = fixture();
        {
            let db = RuntimeDb::open(&path).unwrap();
            seed_owner(&db, &initial.owner);
            db.reserve_scoped_child_attempt(&initial).unwrap();
            db.conn.execute("UPDATE scoped_child_attempt SET recipe_digest='not-a-digest' WHERE attempt_id=?1", [&initial.attempt_id]).unwrap();
        }
        assert!(RuntimeDb::open_existing_current(&path).is_err());
    }

    #[test]
    fn restart_rejects_mount_evidence_that_no_longer_matches_held_process() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("runtime.sqlite3");
        let (initial, recovery, identity) = fixture();
        {
            let db = RuntimeDb::open(&path).unwrap();
            seed_owner(&db, &initial.owner);
            db.reserve_scoped_child_attempt(&initial).unwrap();
            db.bind_scoped_child_scope(&initial.attempt_id, &recovery).unwrap();
            db.attach_scoped_child_process(&initial.attempt_id, &identity, &mount_evidence(&identity)).unwrap();
            let mut tampered = mount_evidence(&identity);
            tampered.observed.destination_access_sha256[0] ^= 1;
            let encoded = lillux::canonical_json(&serde_json::to_value(tampered).unwrap()).unwrap();
            db.conn.execute(
                "UPDATE scoped_child_attempt SET mount_preparation_evidence=?2 WHERE attempt_id=?1",
                params![initial.attempt_id, encoded],
            ).unwrap();
        }
        assert!(RuntimeDb::open_existing_current(&path).is_err());
    }

    #[test]
    fn unsettled_attempt_requires_exact_host_lifetime_on_reopen_and_reserve() {
        for corrupt_fence in [None, Some("different_lifetime")] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("runtime.sqlite3");
            let (initial, _, _) = fixture();
            {
                let db = RuntimeDb::open(&path).unwrap();
                seed_owner(&db, &initial.owner);
                db.reserve_scoped_child_attempt(&initial).unwrap();
                let replacement = corrupt_fence.map(|_| {
                    let mut lifetime =
                        serde_json::to_value(initial.scope_allocation.host_lifetime().unwrap())
                            .unwrap();
                    lifetime["backend"]["boot_id"] = "11111111-1111-4111-8111-111111111111".into();
                    lillux::canonical_json(&lifetime).unwrap()
                });
                db.conn
                    .execute(
                        "UPDATE execution_lifetime_fence SET host_lifetime=?1 WHERE singleton=1",
                        [replacement],
                    )
                    .unwrap();
                let mut second = initial.clone();
                second.attempt_id = "producer-two".into();
                second.owner.thread_id = "T-other".into();
                second.owner.unpredictable_nonce = "claim-other".into();
                let mut allocation = serde_json::to_value(&second.scope_allocation).unwrap();
                allocation["backend"]["name"] = "producer-two".into();
                second.scope_allocation = serde_json::from_value(allocation).unwrap();
                seed_owner(&db, &second.owner);
                assert!(db.reserve_scoped_child_attempt(&second).is_err());
            }
            assert!(RuntimeDb::open_existing_current(&path).is_err());
        }
    }

    #[test]
    fn unbound_retirement_cannot_carry_natural_empty_claim() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let (initial, _, _) = fixture();
        seed_owner(&db, &initial.owner);
        db.reserve_scoped_child_attempt(&initial).unwrap();
        db.conn
            .execute(
                "DELETE FROM thread_launch_claim WHERE thread_id=?1",
                [&initial.owner.thread_id],
            )
            .unwrap();
        db.claim_unbound_scoped_child_discard(&initial.attempt_id)
            .unwrap();
        db.complete_unbound_scoped_child_discard(&initial.attempt_id)
            .unwrap();
        assert!(
            db.conn
                .execute(
                    "UPDATE scoped_child_attempt SET natural_empty_receipt_digest=?2 WHERE attempt_id=?1",
                    params![initial.attempt_id, "a".repeat(64)],
                )
                .is_err()
        );
    }

    #[test]
    fn retiring_one_scoped_child_preserves_shared_host_lifetime_fence() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let (first, _, _) = fixture();
        let mut second = first.clone();
        second.attempt_id = "producer-two".to_owned();
        second.owner.thread_id = "T-other".to_owned();
        second.owner.unpredictable_nonce = "claim-other".to_owned();
        let mut allocation = serde_json::to_value(&second.scope_allocation).unwrap();
        allocation["backend"]["name"] = "producer-two".into();
        second.scope_allocation = serde_json::from_value(allocation).unwrap();
        seed_owner(&db, &first.owner);
        seed_owner(&db, &second.owner);
        db.reserve_scoped_child_attempt(&first).unwrap();
        db.reserve_scoped_child_attempt(&second).unwrap();
        db.conn
            .execute(
                "DELETE FROM thread_launch_claim WHERE thread_id=?1",
                [&first.owner.thread_id],
            )
            .unwrap();
        db.claim_unbound_scoped_child_discard(&first.attempt_id)
            .unwrap();
        db.complete_unbound_scoped_child_discard(&first.attempt_id)
            .unwrap();
        assert!(read_scope_lifetime_fence(&db.conn).unwrap().is_some());
        assert_eq!(db.unsettled_process_scope_count(None).unwrap(), 1);
        db.conn
            .execute(
                "DELETE FROM thread_launch_claim WHERE thread_id=?1",
                [&second.owner.thread_id],
            )
            .unwrap();
        db.claim_unbound_scoped_child_discard(&second.attempt_id)
            .unwrap();
        db.complete_unbound_scoped_child_discard(&second.attempt_id)
            .unwrap();
        assert!(read_scope_lifetime_fence(&db.conn).unwrap().is_none());
    }

    #[test]
    fn ended_old_lifetime_cannot_be_replaced_while_attempt_is_unsettled() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let (first, _, _) = fixture();
        seed_owner(&db, &first.owner);
        db.reserve_scoped_child_attempt(&first).unwrap();
        let mut second = first.clone();
        second.attempt_id = "producer-other-lifetime".to_owned();
        second.owner.thread_id = "T-other".to_owned();
        second.owner.unpredictable_nonce = "claim-other".to_owned();
        let mut allocation = serde_json::to_value(&second.scope_allocation).unwrap();
        allocation["backend"]["boot_id"] = "11111111-1111-4111-8111-111111111111".into();
        allocation["backend"]["name"] = "producer-other-lifetime".into();
        second.scope_allocation = serde_json::from_value(allocation).unwrap();
        seed_owner(&db, &second.owner);
        assert!(db.reserve_scoped_child_attempt(&second).is_err());
        assert_eq!(
            db.unsettled_scoped_child_attempt_ids().unwrap(),
            vec![first.attempt_id]
        );
    }
}
