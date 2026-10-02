//! Stored envelope validation only. A decoded row never mints live authority.
//! The complete original owner must join the opaque budget, retained-input
//! proof, scratch custody and one-use RuntimeDb permit before contact.
use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredEnvelope {
    pub version: u32,
    pub original_reservation: crate::runtime_db::ProcessResourceReservationRecord,
    pub original_reservation_id: String,
    pub capsule_hash: String,
    pub scope: ryeos_state::objects::AdmittedAccountingScope,
    pub root_chain_id: String,
    pub audit_chain_root_id: String,
    pub thread_id: String,
    pub launch_generation: String,
    pub callback_invocation_id: String,
    pub principal: String,
    pub current_site_id: String,
    pub origin_site_id: String,
    pub limits_digest: String,
    pub deadline_at_ms: i64,
    pub cleanup_allowance_ms: u64,
    pub occupancy_start: lillux::time::OccupancyCoordinate,
    pub authority: ResourceAccountingAuthority,
}

impl StoredEnvelope {
    pub fn validate(&self) -> Result<()> {
        use crate::runtime_db::ProcessResourceLaunchAuthority;
        use crate::runtime_db::process_resource_launch::TrustedResourceLaunchPhase;
        if self.version != 1
            || self.original_reservation.owner_kind != "pooled_session"
            || self.original_reservation.selections.len() != 1
        {
            bail!("unsupported first-product precontact envelope");
        }
        self.scope.validate()?;
        for text in [
            &self.callback_invocation_id,
            &self.principal,
            &self.current_site_id,
            &self.origin_site_id,
        ] {
            validate_resource_claim_coordinate(text)?;
        }
        validate_resource_claim_coordinate(&self.thread_id)?;
        validate_resource_claim_coordinate(&self.launch_generation)?;
        validate_resource_claim_coordinate(&self.root_chain_id)?;
        validate_resource_claim_coordinate(&self.audit_chain_root_id)?;
        HexDigest::new(self.capsule_hash.clone()).map_err(anyhow::Error::msg)?;
        HexDigest::new(self.limits_digest.clone()).map_err(anyhow::Error::msg)?;
        if self.original_reservation.reservation_id()? != self.original_reservation_id {
            bail!("precontact envelope contradicts its original reservation");
        }
        let custody = self
            .original_reservation
            .runtime_custody
            .as_ref()
            .context("precontact envelope lacks original custody")?;
        custody.require_retained()?;
        if custody.session_capsule_hash != self.capsule_hash {
            bail!("precontact envelope capsule differs from original custody");
        }
        if !matches!(
            &self.original_reservation.launch_authority,
            ProcessResourceLaunchAuthority::TrustedProcessGroup {
                phase: TrustedResourceLaunchPhase::Reserved,
                ..
            }
        ) {
            bail!("precontact envelope original birth is not trusted Reserved");
        }
        self.authority.validate().map_err(anyhow::Error::msg)?;
        self.occupancy_start
            .validate()
            .map_err(anyhow::Error::msg)?;
        if self.occupancy_start.contract_digest
            != self.authority.meter.clock_contract_digest.as_str()
        {
            bail!("original occupancy clock differs from exact admitted meter");
        }
        let selection = &self.original_reservation.selections[0];
        if selection.stable_id != self.authority.stable_resource_id
            || selection.class != self.authority.resource_class
            || selection.observation_contract_digest
                != self.authority.observation_contract_digest.as_str()
        {
            bail!("precontact selection/accounting authority differs");
        }
        let ResourceSpendAuthority::Bounded {
            maximum_occupancy_milliseconds,
            ..
        } = self.authority.spend
        else {
            bail!("precontact trusted funding requires bounded spend");
        };
        if self.cleanup_allowance_ms == 0
            || self.cleanup_allowance_ms >= maximum_occupancy_milliseconds
        {
            bail!("precontact cleanup allowance is not inside original finite occupancy");
        }
        Ok(())
    }

    pub fn gate_id(&self) -> Result<HexDigest> {
        self.validate()?;
        HexDigest::of_canonical_json(&serde_json::json!({
            "kind": "resource_owner_accounting_gate", "version": 2,
            "envelope": self,
        }))
        .map_err(anyhow::Error::msg)
    }

    pub fn request_digest(&self) -> Result<HexDigest> {
        HexDigest::of_canonical_json(&serde_json::json!({
            "version": 3, "owner_gate_id": self.gate_id()?,
            "original_reservation_id": self.original_reservation_id,
            "thread_id": self.thread_id, "launch_generation": self.launch_generation,
            "root_chain_id": self.root_chain_id,
            "execution_budget_id": self.scope.execution_budget_id,
            "directive_budget_id": self.scope.directive_budget_id,
            "stable_resource_id": self.authority.stable_resource_id,
            "authority_digest": self.authority.authority_digest,
        }))
        .map_err(anyhow::Error::msg)
    }

    pub fn operation_id(&self) -> Result<HexDigest> {
        HexDigest::of_canonical_json(&serde_json::json!({
            "kind": "resource_occupancy_operation", "request_digest": self.request_digest()?,
        }))
        .map_err(anyhow::Error::msg)
    }

    pub fn validate_ledger(&self, conn: &Connection) -> Result<()> {
        let snapshot = load_execution_resource_budget(
            conn,
            &self.scope.execution_budget_id,
            &self.scope.budget_authority_site_id,
            self.scope.ledger_epoch,
        )?
        .context("precontact original execution budget is absent")?;
        if snapshot.root_chain_id != self.root_chain_id
            || snapshot.limits_digest != self.limits_digest
            || snapshot.deadline_at_ms != Some(self.deadline_at_ms)
        {
            bail!("precontact original budget binding changed");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FundingPhase {
    Prepared,
    ReserveAttempted,
    Funded,
    Denied,
    ContactAttempted,
    ContactReady,
    ContactTaken,
    AttachAttempted,
    Attached,
    RefundAttempted,
    Refunded,
}

/// Original process-local funding owner. Construct BEFORE attempting any
/// financial write and retain it in the existing original start owner. No
/// Clone/serde/Drop refund, replay constructor or replacement deadline.
/// This component alone does not own physical scratch or admit execution.
pub(crate) struct OriginalFundingOwner {
    accounting: Arc<AccountingDb>,
    state_store: Arc<crate::state_store::StateStore>,
    original_budget: crate::handler_context::OriginalExecutionBudget,
    envelope: StoredEnvelope,
    gate_id: HexDigest,
    request_digest: HexDigest,
    operation_id: HexDigest,
    phase: FundingPhase,
    runtime_permit: Option<crate::runtime_db::process_resource_launch::TrustedResourceSpawnPermit>,
    intent: Option<crate::runtime_db::ProcessResourceReservationRecord>,
}

impl OriginalFundingOwner {
    pub(crate) fn prepare(
        state: &crate::state::AppState,
        inputs: &crate::persistent_session_admission::VerifiedTrustedPooledSessionInputs,
        budget: &crate::handler_context::OriginalExecutionBudget,
        allocation_scope: &ryeos_state::objects::AdmittedAccountingScope,
        original: &crate::runtime_db::ProcessResourceReservationRecord,
        thread_id: &str,
        launch_generation: &str,
    ) -> Result<Self> {
        if budget.contact_expiry().has_elapsed() {
            bail!("original funding deadline expired");
        }
        inputs.require_same_state(state)?;
        if original.selections != inputs.selected().selections() {
            bail!("funding reservation differs from same verified retained selection");
        }
        let crate::runtime_db::ProcessResourceLaunchAuthority::TrustedProcessGroup {
            cleanup_contract_digest,
            ..
        } = &original.launch_authority
        else {
            bail!("precontact funding requires trusted original reservation");
        };
        if cleanup_contract_digest != inputs.cleanup_contract_digest() {
            bail!("funding reservation cleanup differs from admitted node/product agreement");
        }
        let authorities = inputs.selected().accounting_authorities();
        if authorities.len() != 1 {
            bail!("first trusted funding product requires one accounted selection");
        }
        let authority = authorities[0]
            .as_ref()
            .context("verified selection lacks accounting authority")?
            .clone();
        let binding = budget.precontact_binding();
        if binding.scope != allocation_scope {
            bail!("new trusted pool funding differs from exact verified allocation scope");
        }
        if binding.current_site_id != state.threads.site_id() {
            bail!("original funding callback belongs to another current execution site");
        }
        let envelope = StoredEnvelope {
            version: 1,
            original_reservation: original.clone(),
            original_reservation_id: original.reservation_id()?,
            capsule_hash: inputs.capsule().content_hash()?,
            scope: binding.scope.clone(),
            root_chain_id: binding.root_chain_id.to_owned(),
            audit_chain_root_id: binding.root_chain_id.to_owned(),
            thread_id: thread_id.to_owned(),
            launch_generation: launch_generation.to_owned(),
            callback_invocation_id: binding.callback_invocation_id.to_owned(),
            principal: binding.principal.to_owned(),
            current_site_id: binding.current_site_id.to_owned(),
            origin_site_id: binding.origin_site_id.to_owned(),
            limits_digest: binding.limits_digest.to_owned(),
            deadline_at_ms: binding.deadline_at_ms,
            cleanup_allowance_ms: inputs
                .selected()
                .cleanup_allowance_ms()
                .context("verified selection lacks cleanup reserve")?,
            occupancy_start: lillux::time::occupancy_now().map_err(anyhow::Error::msg)?,
            authority,
        };
        envelope.validate()?;
        let accounting = Arc::clone(
            state
                .accounting
                .as_ref()
                .context("original funding requires actual shared ledger")?,
        );
        if envelope.scope.budget_authority_site_id != accounting.site_id
            || envelope.scope.ledger_epoch != accounting.epoch
        {
            bail!("original funding budget is not on same current financial owner");
        }
        Ok(Self {
            gate_id: envelope.gate_id()?,
            request_digest: envelope.request_digest()?,
            operation_id: envelope.operation_id()?,
            accounting,
            state_store: Arc::clone(&state.state_store),
            original_budget: budget.clone(),
            envelope,
            phase: FundingPhase::Prepared,
            runtime_permit: None,
            intent: None,
        })
    }

    fn require_contact_window(&self) -> Result<()> {
        let cleanup = lillux::time::Duration::from_millis(self.envelope.cleanup_allowance_ms);
        if self.original_budget.remaining() <= cleanup {
            bail!("original funding window reached reserved cleanup interval");
        }
        let ResourceSpendAuthority::Bounded {
            maximum_occupancy_milliseconds,
            ..
        } = self.envelope.authority.spend
        else {
            bail!("original trusted funding lost bounded authority");
        };
        let maximum_ns = maximum_occupancy_milliseconds
            .checked_mul(1_000_000)
            .context("original resource occupancy maximum overflow")?;
        let limit =
            lillux::time::OccupancyLimit::new(self.envelope.occupancy_start.clone(), maximum_ns)
                .map_err(anyhow::Error::msg)?;
        if !matches!(
            limit.window(cleanup).map_err(anyhow::Error::msg)?,
            lillux::time::OccupancyWindowState::Service { .. }
        ) {
            bail!("original occupancy window is no longer open for contact");
        }
        Ok(())
    }

    /// Atomic exact envelope + ordinary shared-account hold. A retained row
    /// can reconcile liability, but never reconstruct this live owner or fund
    /// a second contact. Any failed ACK poisons this attempt before returning.
    pub(crate) fn reserve(&mut self) -> Result<bool> {
        if self.phase != FundingPhase::Prepared {
            bail!("original funding reservation is one-use");
        }
        self.phase = FundingPhase::ReserveAttempted;
        self.require_contact_window()?;
        let now_ms = lillux::time::timestamp_millis();
        let funded =
            self.accounting
                .with_conn_until(self.original_budget.contact_expiry(), |conn| {
                    immediate_transaction(conn, "original precontact financial hold", || {
                        self.reserve_in_transaction(conn, now_ms)
                    })
                })?;
        self.phase = if funded {
            FundingPhase::Funded
        } else {
            FundingPhase::Denied
        };
        Ok(funded)
    }

    fn reserve_in_transaction(&self, conn: &Connection, now_ms: i64) -> Result<bool> {
        self.envelope.validate_ledger(conn)?;
        let existing: i64 = conn.query_row(
            "SELECT COUNT(*) FROM resource_owner_accounting_gate WHERE original_reservation_id=?1",
            [&self.envelope.original_reservation_id],
            |row| row.get(0),
        )?;
        if existing != 0 {
            bail!("original financial reservation already attempted; retain same liability");
        }
        let encoded = canonical_json_string(&serde_json::to_value(&self.envelope)?)?;
        conn.execute(
                    "INSERT INTO resource_owner_accounting_gate (
                        owner_gate_id, owner_incarnation, budget_authority_site_id, ledger_epoch,
                        execution_budget_id, directive_budget_id, root_chain_id, audit_chain_root_id,
                        owner_recovery_json, state, active_request_count, fenced_reason,
                        created_at_ms, updated_at_ms, original_reservation_id, precontact_json
                     ) VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7, NULL, 'precontact_reserved', 0, NULL, ?8, ?8, ?9, ?10)",
                    rusqlite::params![self.gate_id.as_str(), self.accounting.site_id, self.accounting.epoch_i64(),
                        self.envelope.scope.execution_budget_id, self.envelope.scope.directive_budget_id,
                        self.envelope.root_chain_id, self.envelope.audit_chain_root_id, now_ms,
                        self.envelope.original_reservation_id, encoded])?;
        let outcome = self.accounting.reserve_resource_operation_in_transaction(
            conn,
            ResourceReservationArgs {
                owner_gate_id: self.gate_id.as_str(),
                operation_id: self.operation_id.as_str(),
                request_digest: self.request_digest.as_str(),
                execution_budget_id: &self.envelope.scope.execution_budget_id,
                directive_budget_id: self.envelope.scope.directive_budget_id.as_deref(),
                root_chain_id: &self.envelope.root_chain_id,
                audit_chain_root_id: &self.envelope.audit_chain_root_id,
                thread_id: &self.envelope.thread_id,
                launch_generation: &self.envelope.launch_generation,
                owner_incarnation: None,
                authority: &self.envelope.authority,
                now_ms,
                precontact: Some(&self.envelope),
            },
        )?;
        match outcome {
            ResourceOperationReserveOutcome::Reserved { replayed: false } => Ok(true),
            ResourceOperationReserveOutcome::Denied { replayed: false } => {
                let changed = conn.execute("UPDATE resource_owner_accounting_gate SET state='precontact_denied'
                            WHERE owner_gate_id=?1 AND state='precontact_reserved' AND owner_incarnation IS NULL",
                            [self.gate_id.as_str()])?;
                if changed != 1 {
                    bail!("original precontact denial lost exact gate");
                }
                Ok(false)
            }
            _ => bail!("original financial birth cannot replay or acquire advisory authority"),
        }
    }

    /// RuntimeDb one-use permit wins first, then this SAME ledger consumes
    /// its pending phase. Neither permit can be reconstructed after an ACK
    /// loss. No process contact occurs inside this method.
    pub(crate) fn bind_contact_intent(
        &mut self,
        permit: crate::runtime_db::process_resource_launch::TrustedResourceSpawnPermit,
    ) -> Result<()> {
        if self.phase != FundingPhase::Funded {
            bail!("original funding has no unconsumed contact intent");
        }
        self.phase = FundingPhase::ContactAttempted;
        self.runtime_permit = Some(permit);
        self.require_contact_window()?;
        let mut expected = self.envelope.original_reservation.clone();
        let crate::runtime_db::ProcessResourceLaunchAuthority::TrustedProcessGroup {
            phase, ..
        } = &mut expected.launch_authority
        else {
            bail!("original trusted funding lost trusted launch authority");
        };
        *phase =
            crate::runtime_db::process_resource_launch::TrustedResourceLaunchPhase::SpawnIntent;
        if self
            .runtime_permit
            .as_ref()
            .expect("retained before validation")
            .reservation()
            != &expected
        {
            bail!("runtime contact permit differs from original funded birth");
        }
        self.intent = Some(expected);
        self.accounting.with_conn_until(self.original_budget.contact_expiry(), |conn| {
            immediate_transaction(conn, "original precontact contact intent", || {
                self.envelope.validate_ledger(conn)?;
                let encoded = canonical_json_string(&serde_json::to_value(&self.envelope)?)?;
                let changed = conn.execute(
                    "UPDATE resource_owner_accounting_gate SET state='precontact_spawn_intent', updated_at_ms=?3
                     WHERE owner_gate_id=?1 AND precontact_json=?2 AND state='precontact_reserved' AND owner_incarnation IS NULL",
                    rusqlite::params![self.gate_id.as_str(), encoded, lillux::time::timestamp_millis()])?;
                if changed != 1 { bail!("original financial spawn intent was not consumed exactly once"); }
                Ok(())
            })
        })?;
        self.phase = FundingPhase::ContactReady;
        Ok(())
    }

    pub(crate) fn take_contact_permit(
        &mut self,
    ) -> Result<crate::runtime_db::process_resource_launch::TrustedResourceSpawnPermit> {
        if self.phase != FundingPhase::ContactReady {
            bail!("original financial contact permit is unavailable");
        }
        // Poison before checking time/taking authority: a late caller cannot
        // retry using a retained runtime permit.
        self.phase = FundingPhase::ContactTaken;
        self.require_contact_window()?;
        self.runtime_permit
            .take()
            .context("original runtime contact permit was already consumed")
    }

    pub(crate) fn refund_never_born(
        &mut self,
        receipt: crate::state_store::NeverBornResourceFundingReceipt,
    ) -> Result<()> {
        if !matches!(
            self.phase,
            FundingPhase::Funded | FundingPhase::ReserveAttempted
        ) {
            bail!("contacted original funding cannot claim no-birth refund");
        }
        self.phase = FundingPhase::RefundAttempted;
        receipt.matches_original(&self.state_store, &self.envelope.original_reservation)?;
        // Refund is cleanup, so it may use the remaining ORIGINAL cleanup
        // interval, never a new monotonic clock or service-window renewal.
        self.accounting.with_conn_until(self.original_budget.contact_expiry(), |conn| {
            immediate_transaction(conn, "original authoritative no-birth refund", || {
                self.envelope.validate_ledger(conn)?;
                let encoded = canonical_json_string(&serde_json::to_value(&self.envelope)?)?;
                let changed = conn.execute(
                    "UPDATE resource_owner_accounting_gate SET state='precontact_released', updated_at_ms=?3
                     WHERE owner_gate_id=?1 AND precontact_json=?2 AND state='precontact_reserved'
                       AND owner_incarnation IS NULL AND owner_recovery_json IS NULL",
                    rusqlite::params![self.gate_id.as_str(), encoded, lillux::time::timestamp_millis()])?;
                if changed != 1 { bail!("no-birth refund lost exact original uncontacted financial owner"); }
                self.accounting.release_unissued_resource_operation_money_in_transaction(
                    conn, self.operation_id.as_str(), self.request_digest.as_str(), lillux::time::timestamp_millis())?;
                Ok(())
            })
        })?;
        self.phase = FundingPhase::Refunded;
        Ok(())
    }

    /// Attach only a real attachment-pending Engine/Lillux owner, never an
    /// ExecutionProcessIdentity DTO, numeric PID or recovered ledger row.
    /// Result binding still conveys no process release or driver retirement.
    pub(crate) fn attach_held(
        &mut self,
        held: &ryeos_engine::dispatch::SpawnedExecutionAwaitingAttachment,
    ) -> Result<(
        crate::process::ExecutionProcessIdentity,
        ryeos_accounting::ResourceOperationBinding,
    )> {
        if self.phase != FundingPhase::ContactTaken {
            bail!("original financial birth attachment is one-use");
        }
        self.phase = FundingPhase::AttachAttempted;
        self.require_contact_window()?;
        let mut identity = crate::process::execution_process_identity_from_lillux(
            held.exact_process_identity()?,
            held.scope_recovery().cloned(),
        )?;
        if identity.process_scope.is_some() {
            bail!("trusted original birth cannot acquire a local scope");
        }
        let owner = identity.owner_incarnation_digest()?;
        let recovery = canonical_json_string(&serde_json::to_value(&identity)?)?;
        let ResourceSpendAuthority::Bounded {
            maximum_occupancy_milliseconds,
            ..
        } = self.envelope.authority.spend
        else {
            bail!("original trusted funding lost bounded authority");
        };
        let binding = ryeos_accounting::ResourceOperationBinding {
            version: ryeos_accounting::RESOURCE_OPERATION_BINDING_VERSION,
            owner_gate_id: self.gate_id.clone(),
            operation_id: self.operation_id.clone(),
            request_digest: self.request_digest.clone(),
            owner_incarnation: HexDigest::new(owner.clone()).map_err(anyhow::Error::msg)?,
            stable_resource_id: self.envelope.authority.stable_resource_id.clone(),
            authority_digest: self.envelope.authority.authority_digest.clone(),
            meter_contract_digest: self.envelope.authority.meter.contract_digest.clone(),
            clock_contract_digest: self.envelope.authority.meter.clock_contract_digest.clone(),
            maximum_occupancy_milliseconds: Some(maximum_occupancy_milliseconds),
        };
        binding.validate().map_err(anyhow::Error::msg)?;
        identity.resource_settlement_authority = Some(
            self.envelope
                .original_reservation
                .launch_authority
                .settlement_authority(),
        );
        identity.resource_selections = self.envelope.original_reservation.selections.clone();
        identity.resource_operations = vec![binding.clone()];
        identity.resource_allocation_limit =
            Some(self.envelope.original_reservation.allocation_limit);
        identity.resource_occupancy_start = Some(self.envelope.occupancy_start.clone());
        identity.resource_occupancy_limit = Some(
            lillux::time::OccupancyLimit::new(
                self.envelope.occupancy_start.clone(),
                maximum_occupancy_milliseconds
                    .checked_mul(1_000_000)
                    .context("original occupancy maximum overflow")?,
            )
            .map_err(anyhow::Error::msg)?,
        );
        identity.resource_cleanup_allowance_ms = Some(self.envelope.cleanup_allowance_ms);
        crate::process::validate_execution_process_identity_shape(&identity)?;
        let intent = self
            .intent
            .as_ref()
            .context("original funded spawn intent is absent")?;
        if !intent.matches_held_process(&identity)? {
            bail!("actual held birth contradicts original host/cleanup authority");
        }
        self.accounting.with_conn_until(self.original_budget.contact_expiry(), |conn| {
            immediate_transaction(conn, "attach original precontact held birth", || {
                self.envelope.validate_ledger(conn)?;
                let encoded = canonical_json_string(&serde_json::to_value(&self.envelope)?)?;
                let changed = conn.execute(
                    "UPDATE resource_owner_accounting_gate SET owner_incarnation=?3, owner_recovery_json=?4, state='open', updated_at_ms=?5
                     WHERE owner_gate_id=?1 AND precontact_json=?2 AND state='precontact_spawn_intent' AND owner_incarnation IS NULL",
                    rusqlite::params![self.gate_id.as_str(), encoded, owner, recovery, lillux::time::timestamp_millis()])?;
                if changed != 1 { bail!("original actual held birth was not attached exactly once"); }
                let changed = conn.execute(
                    "UPDATE resource_financial_operation SET owner_incarnation=?3
                     WHERE operation_id=?1 AND request_digest=?2 AND owner_gate_id=?4 AND owner_incarnation IS NULL AND state='reserved'",
                    rusqlite::params![self.operation_id.as_str(), self.request_digest.as_str(), owner, self.gate_id.as_str()])?;
                if changed != 1 { bail!("original operation lost its exact unissued funded attachment"); }
                // First actual-process V1 reservation publication starts its
                // original namespace here, with no second monetary debit.
                self.accounting.enqueue_resource_transition(conn, self.operation_id.as_str(),
                    1, ResourceBudgetState::Reserved, None, None, lillux::time::timestamp_millis())?;
                Ok(())
            })
        })?;
        self.phase = FundingPhase::Attached;
        Ok((identity, binding))
    }
}

impl AccountingDb {
    pub(super) fn enqueue_precontact_resource_transition(
        &self,
        conn: &Connection,
        operation_id: &str,
        sequence: u32,
        state: ResourceBudgetState,
        charge: Option<i64>,
        usage: Option<HexDigest>,
        occurred_at_ms: i64,
    ) -> Result<(i64, String)> {
        if charge.is_some() || usage.is_some() {
            bail!("uncreated process cannot retain charged/driver usage evidence");
        }
        let (encoded, gate_id, request, owner, stored_state, reserved):
            (String, String, String, Option<String>, String, i64) = conn.query_row(
                "SELECT gate.precontact_json, op.owner_gate_id, op.request_digest, op.owner_incarnation,
                        op.state, op.reserved_usd_nanos
                 FROM resource_financial_operation op JOIN resource_owner_accounting_gate gate USING(owner_gate_id)
                 WHERE op.operation_id=?1", [operation_id],
                 |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?)))?;
        let envelope: StoredEnvelope = serde_json::from_str(&encoded)?;
        envelope.validate_ledger(conn)?;
        if canonical_json_string(&serde_json::to_value(&envelope)?)? != encoded
            || envelope.operation_id()?.as_str() != operation_id
            || envelope.request_digest()?.as_str() != request
            || envelope.gate_id()?.as_str() != gate_id
            || owner.is_some()
            || stored_state != state.as_str()
        {
            bail!("precontact audit differs from exact original financial reservation");
        }
        let ResourceSpendAuthority::Bounded { maximum, .. } = envelope.authority.spend else {
            bail!("precontact audit lacks bounded authority");
        };
        if reserved != maximum.as_nanos() {
            bail!("precontact audit maximum differs from original hold");
        }
        let event = ryeos_accounting::ResourcePrecontactBudgetTransitionV1 {
            version: 1,
            transition_id: ryeos_accounting::ResourcePrecontactBudgetTransitionV1::transition_id(
                operation_id,
                sequence,
            ),
            transition_sequence: sequence,
            operation_id: operation_id.to_owned(),
            budget_authority_site_id: self.site_id.clone(),
            ledger_epoch: self.epoch,
            execution_budget_id: envelope.scope.execution_budget_id,
            root_chain_id: envelope.root_chain_id,
            audit_chain_root_id: envelope.audit_chain_root_id.clone(),
            thread_id: envelope.thread_id,
            launch_generation: envelope.launch_generation,
            owner_gate_id: HexDigest::new(gate_id).map_err(anyhow::Error::msg)?,
            original_reservation_id: HexDigest::new(envelope.original_reservation_id)
                .map_err(anyhow::Error::msg)?,
            envelope_digest: HexDigest::new(lillux::sha256_hex(encoded.as_bytes()))
                .map_err(anyhow::Error::msg)?,
            capsule_hash: HexDigest::new(envelope.capsule_hash).map_err(anyhow::Error::msg)?,
            stable_resource_id: envelope.authority.stable_resource_id,
            authority_digest: envelope.authority.authority_digest,
            transition: state,
            reserved_usd_nanos: u64::try_from(reserved)?,
            occurred_at_ms,
        };
        event.validate().map_err(anyhow::Error::msg)?;
        let canonical = canonical_json_string(&serde_json::to_value(&event)?)?;
        let fingerprint = lillux::sha256_hex(canonical.as_bytes());
        conn.execute(
            "INSERT INTO accounting_audit_outbox (
                operation_id, audit_chain_root_id, transition_sequence, transition_id, transition,
                payload_fingerprint, payload, published_chain_seq, created_at_ms, lease_expires_at_ms, event_type
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,NULL,?8,NULL,'resource_precontact_budget_transition_v1')",
            rusqlite::params![operation_id, event.audit_chain_root_id, i64::from(sequence),event.transition_id,
                state.as_str(),fingerprint,canonical,occurred_at_ms])?;
        Ok((conn.last_insert_rowid(), fingerprint))
    }
}

#[cfg(all(any(test, feature = "test-support"), target_os = "linux"))]
mod tests {
    use super::*;

    /// Financial component fixture: actual callback stores/CAS/ledger budget,
    /// observed /dev/null descriptors, original private scratch and RuntimeDb
    /// reservation. It does NOT construct a VerifiedTrustedPooledSessionInputs
    /// proof or test executable/product/GPU admission. Production factory
    /// controls remain a separate required gate.
    #[cfg(test)]
    fn fixture(
        cap: &str,
    ) -> (
        tempfile::TempDir,
        crate::handler_context::budget_test_support::Fixture,
        Arc<crate::temp_dir_guard::TempDirGuard>,
        OriginalFundingOwner,
    ) {
        let root = tempfile::tempdir().unwrap();
        let (fixture, scratch, owner) = financial_fixture_at(root.path(), cap);
        (root, fixture, scratch, owner)
    }

    fn financial_fixture_at(
        root: &Path,
        cap: &str,
    ) -> (
        crate::handler_context::budget_test_support::Fixture,
        Arc<crate::temp_dir_guard::TempDirGuard>,
        OriginalFundingOwner,
    ) {
        use crate::runtime_db::process_resource_custody::*;
        let fixture = crate::handler_context::budget_test_support::build_with_spend_limit(
            root,
            lillux::time::Duration::from_secs(60),
            Some(ryeos_accounting::UsdNanos::parse_canonical(cap).unwrap()),
        )
        .unwrap();
        let budget = fixture.context.original_execution_budget().unwrap().clone();
        let binding = budget.precontact_binding();
        let accounting = Arc::clone(fixture.state.accounting.as_ref().unwrap());
        accounting
            .activate_account(
                &binding.scope.execution_budget_id,
                "execution",
                &binding.scope.execution_budget_id,
            )
            .unwrap();
        assert!(accounting.startup_verify().unwrap().hard_admission_enabled);
        let devices = lillux::CharacterDeviceSet::observe(&[lillux::CharacterDeviceSpec {
            role: "test_primary".into(),
            source: "/dev/null".into(),
            destination: "/dev/null".into(),
            access: lillux::CharacterDeviceAccess::ReadWrite,
        }])
        .unwrap();
        let occupancy_start = lillux::time::occupancy_now().unwrap();
        let meter = ryeos_accounting::ResourceMeterContract {
            version: ryeos_accounting::RESOURCE_METER_CONTRACT_VERSION,
            kind: ryeos_accounting::ResourceMeterKind::OccupancyDuration,
            clock_contract_digest: HexDigest::new(occupancy_start.contract_digest.clone()).unwrap(),
            contract_digest: HexDigest::new("0".repeat(64)).unwrap(),
        }
        .sealed()
        .unwrap();
        let tariff = ryeos_accounting::ResourceTariffDocument {
            version: ryeos_accounting::RESOURCE_TARIFF_VERSION,
            currency: ryeos_accounting::Currency::Usd,
            pricing_generation: "unit-financial-rate".into(),
            charge_class: ryeos_accounting::ResourceChargeClass::InternalAllocation,
            rate_per_million_milliseconds: ryeos_accounting::UsdNanos::parse_canonical("1")
                .unwrap(),
            billing_quantum_milliseconds: 1000,
            minimum_billable_milliseconds: 1000,
            expires_at_ms: None,
        };
        let authority = ResourceAccountingAuthority {
            version: ryeos_accounting::RESOURCE_ACCOUNTING_AUTHORITY_VERSION,
            authority_digest: HexDigest::new("0".repeat(64)).unwrap(),
            stable_resource_id: "test-char-device".into(),
            resource_class: "test-character-device".into(),
            observation_contract_digest: HexDigest::new(lillux::sha256_hex(b"unit-test-observer"))
                .unwrap(),
            meter,
            spend: ResourceSpendAuthority::Bounded {
                maximum_occupancy_milliseconds: 10000,
                maximum: tariff.charge_for_nanoseconds(10_000_000_000).unwrap(),
            },
            tariff,
        }
        .sealed()
        .unwrap();
        let pinned = fixture.state.state_store.pinned_state_authority().unwrap();
        let runtime = pinned.runtime_directory();
        let (_, scratch) = crate::temp_dir_guard::create_projectless_workspace(
            &runtime.path().join("cache"),
            "j4-financial-original",
        )
        .unwrap();
        std::fs::write(
            scratch.path().unwrap().join("original"),
            b"original private scratch",
        )
        .unwrap();
        let (parent, name) = scratch.owned_scratch_parent().unwrap();
        let original = crate::runtime_db::ProcessResourceReservationRecord {
            owner_kind: "pooled_session".into(),
            owner_coordinate: "unit-financial-original-owner".into(),
            daemon_generation_id: crate::runtime_db::daemon_generation_id().to_owned(),
            allocation_limit: 1,
            selections: vec![ryeos_engine::contracts::ExecutionResourceSelection {
                stable_id: authority.stable_resource_id.clone(),
                class: authority.resource_class.clone(),
                matched_facts: Default::default(),
                observation_contract_digest: authority.observation_contract_digest.as_str().into(),
                device_binding_digest: devices.binding_digest().into(),
                access: ryeos_engine::contracts::ExecutionResourceAccess::ExecutionRestricted,
                enforcement:
                    ryeos_engine::contracts::ExecutionResourceEnforcement::CharacterDeviceGrant,
                character_devices: devices.identities().to_vec(),
            }],
            launch_authority:
                crate::runtime_db::ProcessResourceLaunchAuthority::TrustedProcessGroup {
                    cleanup_contract_digest: "7".repeat(64),
                    host_lifetime: lillux::ProcessHostLifetime::capture_current().unwrap(),
                    phase: crate::runtime_db::TrustedResourceLaunchPhase::Reserved,
                },
            runtime_custody: Some(ProcessResourceRuntimeCustody {
                version: ProcessResourceRuntimeCustody::VERSION,
                session_capsule_hash: "1".repeat(64),
                execution_realization_hash: "2".repeat(64),
                source_binding_hash: None,
                materializations: vec![],
                workspace: ProcessWorkspaceCustody::ResourceOwnedScratch {
                    runtime_directory_identity: runtime.identity().unwrap(),
                    parent_directory_identity: parent.identity().unwrap(),
                    scratch_name: name.to_str().unwrap().into(),
                    workspace_identity: scratch.owned_scratch_root().unwrap().identity().unwrap(),
                },
                physical_state: ProcessCustodyPhysicalState::Retained,
            }),
        };
        fixture
            .state
            .state_store
            .reserve_process_resource_launch_with_scratch(&original, &scratch)
            .unwrap();
        let envelope = StoredEnvelope {
            version: 1,
            original_reservation_id: original.reservation_id().unwrap(),
            original_reservation: original,
            capsule_hash: "1".repeat(64),
            scope: binding.scope.clone(),
            root_chain_id: binding.root_chain_id.into(),
            audit_chain_root_id: binding.root_chain_id.into(),
            thread_id: fixture.thread_id.clone(),
            launch_generation: "unit-original-launch".into(),
            callback_invocation_id: binding.callback_invocation_id.into(),
            principal: binding.principal.into(),
            current_site_id: binding.current_site_id.into(),
            origin_site_id: binding.origin_site_id.into(),
            limits_digest: binding.limits_digest.into(),
            deadline_at_ms: binding.deadline_at_ms,
            cleanup_allowance_ms: 1000,
            occupancy_start,
            authority,
        };
        envelope.validate().unwrap();
        let owner = OriginalFundingOwner {
            gate_id: envelope.gate_id().unwrap(),
            request_digest: envelope.request_digest().unwrap(),
            operation_id: envelope.operation_id().unwrap(),
            state_store: Arc::clone(&fixture.state.state_store),
            accounting,
            original_budget: budget,
            envelope,
            phase: FundingPhase::Prepared,
            runtime_permit: None,
            intent: None,
        };
        (fixture, scratch, owner)
    }

    fn exact_hold(owner: &OriginalFundingOwner) -> i64 {
        let conn = owner.accounting.lock_conn().unwrap();
        conn.query_row("SELECT COALESCE(SUM(held_usd_nanos),0) FROM resource_financial_debit WHERE operation_id=?1",
            [owner.operation_id.as_str()], |row| row.get(0)).unwrap()
    }

    fn audits(owner: &OriginalFundingOwner) -> Vec<serde_json::Value> {
        let conn = owner.accounting.lock_conn().unwrap();
        let mut statement = conn.prepare("SELECT payload FROM accounting_audit_outbox WHERE operation_id=?1 ORDER BY outbox_seq").unwrap();
        statement
            .query_map([owner.operation_id.as_str()], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
            .collect()
    }

    #[cfg(test)]
    #[test]
    fn actual_precontact_reserve_has_null_birth_and_publishes_original_reservation_audit() {
        let (_root, fixture, _scratch, mut owner) = fixture("1");
        assert!(owner.reserve().unwrap());
        assert!(exact_hold(&owner) > 0);
        let audit = audits(&owner);
        assert_eq!(audit.len(), 1);
        let event: ryeos_accounting::ResourcePrecontactBudgetTransitionV1 =
            serde_json::from_value(audit[0].clone()).unwrap();
        event.validate().unwrap();
        assert!(audit[0].get("owner_incarnation").is_none());
        assert_eq!(
            event.original_reservation_id.as_str(),
            owner.envelope.original_reservation_id
        );
        let record = crate::state_store::NewEventRecord {
            event_type: ryeos_state::event_types::RESOURCE_PRECONTACT_BUDGET_TRANSITION_V1.into(),
            storage_class: "indexed".into(),
            payload: audit[0].clone(),
        };
        fixture
            .state
            .state_store
            .append_events(
                &owner.envelope.audit_chain_root_id,
                &fixture.thread_id,
                &[record],
            )
            .unwrap();
        let retained = fixture
            .state
            .state_store
            .thread_events_by_type_operation_id(
                &fixture.thread_id,
                ryeos_state::event_types::RESOURCE_PRECONTACT_BUDGET_TRANSITION_V1,
                owner.operation_id.as_str(),
            )
            .unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].payload, audit[0]);
        assert!(owner.reserve().is_err());
        assert!(
            owner
                .accounting
                .startup_verify()
                .unwrap()
                .hard_admission_enabled
        );
    }

    #[cfg(test)]
    #[test]
    fn actual_precontact_denial_has_no_hold_or_contact_and_retains_original_audit() {
        let (_root, _fixture, _scratch, mut owner) = fixture("0.0001");
        assert!(!owner.reserve().unwrap());
        assert_eq!(exact_hold(&owner), 0);
        assert!(owner.take_contact_permit().is_err());
        let audit = audits(&owner);
        assert_eq!(audit.len(), 1);
        let event: ryeos_accounting::ResourcePrecontactBudgetTransitionV1 =
            serde_json::from_value(audit[0].clone()).unwrap();
        assert_eq!(event.transition, ResourceBudgetState::ReservationDenied);
        event.validate().unwrap();
        assert!(
            owner
                .accounting
                .startup_verify()
                .unwrap()
                .hard_admission_enabled
        );
    }

    #[cfg(test)]
    #[test]
    fn actual_precontact_no_birth_refund_uses_original_physical_receipt_and_two_audits() {
        let (_root, fixture, scratch, mut owner) = fixture("1");
        assert!(owner.reserve().unwrap());
        assert!(exact_hold(&owner) > 0);
        assert!(
            owner
                .accounting
                .release_unissued_resource_operation(
                    owner.operation_id.as_str(),
                    owner.request_digest.as_str(),
                    lillux::time::timestamp_millis()
                )
                .is_err()
        );
        let receipt = fixture
            .state
            .state_store
            .remove_never_born_scratch_for_funding(&owner.envelope.original_reservation, &scratch)
            .unwrap();
        owner.refund_never_born(receipt).unwrap();
        assert_eq!(exact_hold(&owner), 0);
        assert!(
            fixture
                .state
                .state_store
                .begin_trusted_process_resource_spawn(&owner.envelope.original_reservation)
                .is_err()
        );
        let audit = audits(&owner);
        assert_eq!(audit.len(), 2);
        for (index, (payload, expected)) in audit
            .iter()
            .zip([
                ResourceBudgetState::Reserved,
                ResourceBudgetState::ReleasedUnissued,
            ])
            .enumerate()
        {
            let event: ryeos_accounting::ResourcePrecontactBudgetTransitionV1 =
                serde_json::from_value(payload.clone()).unwrap();
            event.validate().unwrap();
            assert_eq!(event.transition, expected);
            assert_eq!(event.transition_sequence, u32::try_from(index + 1).unwrap());
        }
        assert!(
            owner
                .accounting
                .startup_verify()
                .unwrap()
                .hard_admission_enabled
        );
    }

    #[cfg(test)]
    #[test]
    fn actual_precontact_intent_is_one_use_and_reopen_retains_same_unattached_hold() {
        let (root, fixture, _scratch, mut owner) = fixture("1");
        assert!(owner.reserve().unwrap());
        let hold = exact_hold(&owner);
        let permit = fixture
            .state
            .state_store
            .begin_trusted_process_resource_spawn(&owner.envelope.original_reservation)
            .unwrap();
        owner.bind_contact_intent(permit).unwrap();
        let permit = owner.take_contact_permit().unwrap();
        assert!(owner.take_contact_permit().is_err());
        let original = owner.envelope.original_reservation_id.clone();
        let operation = owner.operation_id.clone();
        drop(permit);
        drop(owner);
        drop(fixture);
        let reopened =
            AccountingDb::open_default(&root.path().join("budget-fixture-ledger")).unwrap();
        assert!(reopened.startup_verify().unwrap().hard_admission_enabled);
        let conn = reopened.lock_conn().unwrap();
        let row: (String, Option<String>) = conn.query_row(
            "SELECT state,owner_incarnation FROM resource_owner_accounting_gate WHERE original_reservation_id=?1", [original], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!(row, ("precontact_spawn_intent".into(), None));
        assert_eq!(
            conn.query_row(
                "SELECT SUM(held_usd_nanos) FROM resource_financial_debit WHERE operation_id=?1",
                [operation.as_str()],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            hold
        );
    }
    #[cfg(test)]
    #[test]
    fn actual_precontact_lost_commit_ack_reopens_same_original_hold_without_contact() {
        let (root, fixture, _scratch, mut owner) = fixture("1");
        owner.phase = FundingPhase::ReserveAttempted;
        owner.require_contact_window().unwrap();
        // SAME production financial body, actual durable ledger COMMIT, then
        // deliberately lose its caller ACK. This does not mint another owner.
        let result: Result<()> =
            owner
                .accounting
                .with_conn_until(owner.original_budget.contact_expiry(), |conn| {
                    let funded =
                        immediate_transaction(conn, "test original precontact lost ACK", || {
                            owner.reserve_in_transaction(conn, lillux::time::timestamp_millis())
                        })?;
                    assert!(funded);
                    bail!("test lost original precontact commit ACK")
                });
        assert!(result.is_err());
        let held = exact_hold(&owner);
        assert!(held > 0);
        assert_eq!(audits(&owner).len(), 1);
        assert!(owner.reserve().is_err());
        assert!(owner.take_contact_permit().is_err());
        let operation = owner.operation_id.clone();
        let original = owner.envelope.original_reservation_id.clone();
        drop(owner);
        drop(fixture);
        let reopened =
            AccountingDb::open_default(&root.path().join("budget-fixture-ledger")).unwrap();
        assert!(reopened.startup_verify().unwrap().hard_admission_enabled);
        let conn = reopened.lock_conn().unwrap();
        let row: (String, Option<String>) = conn.query_row("SELECT state,owner_incarnation FROM resource_owner_accounting_gate WHERE original_reservation_id=?1", [original], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!(row, ("precontact_reserved".into(), None));
        assert_eq!(
            conn.query_row(
                "SELECT SUM(held_usd_nanos) FROM resource_financial_debit WHERE operation_id=?1",
                [operation.as_str()],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            held
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM accounting_audit_outbox WHERE operation_id=?1",
                [operation.as_str()],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }
    pub(super) fn publication_fixture(
        root: &Path,
    ) -> crate::handler_context::budget_test_support::Fixture {
        let (fixture, scratch, mut owner) = financial_fixture_at(root, "1");
        assert!(owner.reserve().unwrap());
        let receipt = fixture
            .state
            .state_store
            .remove_never_born_scratch_for_funding(&owner.envelope.original_reservation, &scratch)
            .unwrap();
        owner.refund_never_born(receipt).unwrap();
        assert_eq!(exact_hold(&owner), 0);
        assert_eq!(audits(&owner).len(), 2);
        fixture
    }

    #[cfg(test)]
    #[test]
    fn actual_precontact_narrower_directive_denies_new_hold_under_same_original_clock() {
        let (_root, fixture, _scratch, mut owner) = fixture("2");
        let original_remaining = owner.original_budget.remaining();
        let budget =
            crate::handler_context::budget_test_support::derive_directive_callback_for_test(
                &fixture,
                "D-original-narrower",
                ryeos_accounting::UsdNanos::parse_canonical("0.001").unwrap(),
            )
            .unwrap();
        assert!(budget.remaining() <= original_remaining);
        let binding = budget.precontact_binding();
        owner.envelope.scope = binding.scope.clone();
        owner.envelope.callback_invocation_id = binding.callback_invocation_id.into();
        owner.original_budget = budget;
        owner.gate_id = owner.envelope.gate_id().unwrap();
        owner.request_digest = owner.envelope.request_digest().unwrap();
        owner.operation_id = owner.envelope.operation_id().unwrap();
        // The same actual resource maximum is $0.01; execution has $2 but
        // the authenticated directive permits only $0.001. No new pool hold.
        assert!(!owner.reserve().unwrap());
        assert_eq!(exact_hold(&owner), 0);
        assert!(owner.take_contact_permit().is_err());
        let conn = owner.accounting.lock_conn().unwrap();
        let row: (String, Option<String>) = conn.query_row("SELECT state,directive_budget_id FROM resource_financial_operation WHERE operation_id=?1", [owner.operation_id.as_str()], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!(
            row,
            (
                "reservation_denied".into(),
                Some("D-original-narrower".into())
            )
        );
        assert_eq!(
            conn.query_row(
                "SELECT SUM(held_usd_nanos) FROM budget_account",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        drop(conn);
        let event: ryeos_accounting::ResourcePrecontactBudgetTransitionV1 =
            serde_json::from_value(audits(&owner)[0].clone()).unwrap();
        assert_eq!(event.transition, ResourceBudgetState::ReservationDenied);
        assert!(
            owner
                .accounting
                .startup_verify()
                .unwrap()
                .hard_admission_enabled
        );
    }
}

#[cfg(all(any(test, feature = "test-support"), target_os = "linux"))]
pub(super) fn financial_publication_fixture(
    root: &Path,
) -> crate::handler_context::budget_test_support::Fixture {
    tests::publication_fixture(root)
}
