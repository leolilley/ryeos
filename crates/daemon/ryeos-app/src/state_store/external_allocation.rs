//! External allocation, lease and owner verification.
use super::*;

/// Ephemeral handoff from authoritative born-thread/CAS verification to the
/// allocation transaction. Not caller-deserializable and not a second ledger.
pub(crate) struct VerifiedExternalDirectOwner {
    pub(super) thread_id: String,
    pub(super) chain_root_id: String,
    pub(super) capsule_hash: String,
    pub(super) snapshot_hash: String,
    pub(super) endpoint_binding_hash: String,
    pub(super) program_digest: String,
    pub(super) launch_owner: runtime_db::LaunchOwner,
}

impl VerifiedExternalDirectOwner {
    pub(crate) fn matches(
        &self,
        reservation: &runtime_db::external_execution::ExternalAllocationReservation,
    ) -> bool {
        self.thread_id == reservation.placement_thread_id
            && self.capsule_hash == reservation.admitted_capsule_hash
            && self.snapshot_hash == reservation.base_snapshot_hash
            && self.endpoint_binding_hash == reservation.binding_hash
            && matches!(&reservation.owner,
                runtime_db::external_execution::ExternalAllocationOwner::DirectThread { chain_root_id, launch_owner, program }
                if chain_root_id == &self.chain_root_id && launch_owner == &self.launch_owner
                    && program.digest().ok().as_ref() == Some(&self.program_digest))
    }
}

/// Ephemeral bridge from an authenticated accepted consumer root and the
/// current restored occurrence to one exact qualification-only allocation.
/// It is neither serializable nor renewable without accepted-root admission.
pub(crate) struct VerifiedExternalQualificationOwner {
    pub(super) reservation: runtime_db::external_execution::ExternalAllocationReservation,
    pub(super) callback_launch_owner: Option<String>,
    pub(super) canary_request: Option<ryeos_state::external_content::products::qualification::ProductQualificationConsumerWorkerRequest>,
}

impl VerifiedExternalQualificationOwner {
    pub(crate) fn canary_request(&self) -> Option<&ryeos_state::external_content::products::qualification::ProductQualificationConsumerWorkerRequest>{
        self.canary_request.as_ref()
    }
    pub(crate) fn callback_launch_owner(&self) -> Option<&str> {
        self.callback_launch_owner.as_deref()
    }

    pub(crate) fn matches(
        &self,
        reservation: &runtime_db::external_execution::ExternalAllocationReservation,
    ) -> bool {
        matches!(
            &reservation.owner,
            runtime_db::external_execution::ExternalAllocationOwner::QualificationCandidate(_)
        ) && self.reservation == *reservation
    }
}

impl StateStore {
    pub(crate) fn unsettled_external_allocation_placements(&self) -> Result<Vec<String>> {
        self.lock()?
            .runtime_db
            .unsettled_external_allocation_placements()
    }

    pub(crate) fn recoverable_external_cleanup_placements(&self) -> Result<Vec<String>> {
        self.lock()?
            .runtime_db
            .recoverable_external_cleanup_placements()
    }

    pub(crate) fn retained_external_binding(
        &self,
        binding_hash: &str,
    ) -> Result<
        Option<crate::node_config::sections::external_execution::RetainedExternalExecutionBinding>,
    > {
        self.lock()?
            .runtime_db
            .retained_external_binding(binding_hash)
    }

    pub(crate) fn reserve_external_allocation(
        &self,
        reservation: &runtime_db::external_execution::ExternalAllocationReservation,
        binding: &crate::node_config::sections::external_execution::RetainedExternalExecutionBinding,
    ) -> Result<runtime_db::external_execution::ExternalAllocationRecord> {
        self.reserve_external_allocation_inner(reservation, binding, None)
    }

    /// Reserve a placed session's allocation together with its admitted
    /// lease's charge against the node lease budget, in one transaction and
    /// before any provider contact. A replay of the exact reservation reserves
    /// nothing again.
    pub(crate) fn reserve_external_lease_allocation(
        &self,
        reservation: &runtime_db::external_execution::ExternalAllocationReservation,
        binding: &crate::node_config::sections::external_execution::RetainedExternalExecutionBinding,
        lease: &runtime_db::external_execution::ExternalLeaseCharge,
    ) -> Result<runtime_db::external_execution::ExternalAllocationRecord> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        g.runtime_db
            .reserve_external_lease_allocation(reservation, binding, lease)
    }

    /// The lease a placed session still holds, if any.
    pub(crate) fn held_external_lease(
        &self,
        placement: &str,
    ) -> Result<Option<runtime_db::external_execution::HeldExternalLease>> {
        self.lock()?.runtime_db.held_external_lease(placement)
    }

    /// Placements holding a lease reserved for a worker of another daemon
    /// incarnation than `current_generation`.
    pub(crate) fn predecessor_external_lease_placements(
        &self,
        current_generation: &str,
    ) -> Result<Vec<String>> {
        self.lock()?
            .runtime_db
            .predecessor_external_lease_placements(current_generation)
    }

    /// The lease record of a placed session's remote lease, if it has one.
    pub(crate) fn external_lease_record(
        &self,
        placement: &str,
    ) -> Result<Option<runtime_db::external_execution::ExternalLeaseRecord>> {
        self.lock()?.runtime_db.external_lease_record(placement)
    }

    /// Reserved to activated: bind the lease's identity and ready deadline.
    pub(crate) fn activate_external_lease_record(
        &self,
        placement: &str,
        activation: &runtime_db::external_execution::ExternalLeaseActivation,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?.runtime_db.activate_external_lease_record(
            placement,
            activation,
            lillux::time::timestamp_millis(),
        )
    }

    /// Hold the guest's first frame, un-journalled, on an activated record.
    pub(crate) fn hold_external_lease_ready_frame(
        &self,
        placement: &str,
        frame: &[u8],
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?.runtime_db.hold_external_lease_ready_frame(
            placement,
            frame,
            lillux::time::timestamp_millis(),
        )
    }

    /// Hold the guest's second frame, un-journalled, on an activated record.
    pub(crate) fn hold_external_lease_report_frame(
        &self,
        placement: &str,
        frame: &[u8],
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?.runtime_db.hold_external_lease_report_frame(
            placement,
            frame,
            lillux::time::timestamp_millis(),
        )
    }

    /// Journal the held Ready and admit the lease, in one transaction.
    pub(crate) fn admit_external_lease_with_ready(
        &self,
        placement: &str,
        admission: &runtime_db::external_execution::ExternalLeaseAdmission,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?.runtime_db.admit_external_lease_with_ready(
            placement,
            admission,
            lillux::time::timestamp_millis(),
        )
    }

    /// Refuse the lease, terminally, and cancel its allocation, in one
    /// transaction.
    pub(crate) fn refuse_external_lease_and_cancel(
        &self,
        placement: &str,
        reason: &str,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?.runtime_db.refuse_external_lease_and_cancel(
            placement,
            reason,
            lillux::time::timestamp_millis(),
        )
    }

    /// Refuse the lease and cancel its allocation, in one transaction, only if
    /// it is still activated; returns whether it was refused.
    pub(crate) fn refuse_activated_external_lease_and_cancel(
        &self,
        placement: &str,
        reason: &str,
    ) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .refuse_activated_external_lease_and_cancel(
                placement,
                reason,
                lillux::time::timestamp_millis(),
            )
    }

    /// Activated lease records whose ready deadline is at or before `now_ms`.
    pub(crate) fn overdue_external_lease_records(
        &self,
        now_ms: i64,
    ) -> Result<Vec<runtime_db::external_execution::ExternalLeaseRecord>> {
        self.lock()?
            .runtime_db
            .overdue_external_lease_records(now_ms)
    }

    /// Unrefused lease records of leases held for a worker of another daemon
    /// incarnation than `current_generation`.
    pub(crate) fn predecessor_external_lease_records(
        &self,
        current_generation: &str,
    ) -> Result<Vec<runtime_db::external_execution::ExternalLeaseRecord>> {
        self.lock()?
            .runtime_db
            .predecessor_external_lease_records(current_generation)
    }

    /// First direct reservation consumes evidence from the sole full-capsule
    /// compiler. Retained JSON alone cannot produce this non-serializable token.
    pub(crate) fn reserve_external_direct_allocation(
        &self,
        reservation: &runtime_db::external_execution::ExternalAllocationReservation,
        binding: &crate::node_config::sections::external_execution::RetainedExternalExecutionBinding,
        compiled: crate::thread_lifecycle::CompiledExternalDirectProgram,
    ) -> Result<runtime_db::external_execution::ExternalAllocationRecord> {
        compiled.verify_reservation(reservation)?;
        self.reserve_external_allocation_inner(reservation, binding, Some(&compiled))
    }

    /// First qualification-only reservation consumes the accepted-root token.
    /// Contacted allocations recover through the ordinary retained journal;
    /// they never acquire another fresh proof or allocation opportunity.
    pub(crate) fn reserve_external_qualification_allocation(
        &self,
        state: &crate::state::AppState,
        reservation: &runtime_db::external_execution::ExternalAllocationReservation,
        binding: &crate::node_config::sections::external_execution::RetainedExternalExecutionBinding,
        admission: crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
    ) -> Result<runtime_db::external_execution::ExternalAllocationRecord> {
        let _permit = self.acquire_write_permit()?;
        admission.with_live_callback(|| {
            let g = self.lock()?;
            anyhow::ensure!(
                g.runtime_db
                    .external_allocation(&reservation.placement_thread_id)?
                    .is_none(),
                "qualification first reservation cannot replace retained input lifelines"
            );
            let scratch = g
                .runtime_db
                .qualification_guest_scratch(&reservation.placement_thread_id)?
                .context("qualification first reservation has no original guest scratch")?;
            anyhow::ensure!(
                scratch.phase
                    == runtime_db::external_execution::QualificationGuestScratchPhase::Created,
                "qualification first reservation has no Created guest scratch"
            );
            let original = state
                .qualification_workspace_custody
                .borrow_guest_scratch(&scratch)?;
            let pinned = original.pinned_root()?;
            pinned.require_owner_private_directory()?;
            pinned.ensure_path_binding()?;
            let proof = self.verify_external_qualification_owner(
                state,
                &g,
                reservation,
                binding,
                &admission,
            )?;
            g.runtime_db
                .reserve_external_qualification_allocation_with_verified_owner(
                    reservation,
                    binding,
                    &proof,
                )
        })
    }

    /// The only Q-path that may cross Reserved to ContactPending. The caller
    /// must also hold the occurrence contact gate and match the installed
    /// signed binding; the runtime transaction repeats live occurrence and
    /// owner-measurement checks before its one-contact CAS.
    pub(crate) fn claim_external_qualification_allocation_contact(
        &self,
        state: &crate::state::AppState,
        placement: &str,
        request_digest: &str,
        binding: &crate::node_config::sections::external_execution::RetainedExternalExecutionBinding,
        admission: crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
    ) -> Result<runtime_db::external_execution::ExternalAllocationContactClaim> {
        let _permit = self.acquire_write_permit()?;
        admission.with_live_callback(|| {
            let g = self.lock()?;
            let record = g
                .runtime_db
                .external_allocation(placement)?
                .context("external qualification allocation was not reserved")?;
            anyhow::ensure!(
                record.phase == runtime_db::external_execution::ExternalAllocationPhase::Reserved
                    && record.reservation.request_digest == request_digest,
                "qualification first contact changed its reserved request"
            );
            let proof = self.verify_external_qualification_owner(
                state,
                &g,
                &record.reservation,
                binding,
                &admission,
            )?;
            g.runtime_db
                .claim_external_qualification_allocation_contact_with_verified_owner(
                    placement,
                    request_digest,
                    &proof,
                )
        })
    }

    pub(super) fn reserve_external_allocation_inner(
        &self,
        reservation: &runtime_db::external_execution::ExternalAllocationReservation,
        binding: &crate::node_config::sections::external_execution::RetainedExternalExecutionBinding,
        compiled: Option<&crate::thread_lifecycle::CompiledExternalDirectProgram>,
    ) -> Result<runtime_db::external_execution::ExternalAllocationRecord> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let existing = g
            .runtime_db
            .external_allocation(&reservation.placement_thread_id)?;
        let proof = match &existing {
            Some(existing)
                if existing.phase
                    != runtime_db::external_execution::ExternalAllocationPhase::Reserved =>
            {
                anyhow::ensure!(
                    existing.reservation == *reservation,
                    "external allocation replay changed its exact reservation"
                );
                // This is the existing occurrence's observation/cleanup path,
                // not fresh admission. Rotation, stop or terminal history may
                // invalidate a live proof without erasing original ownership.
                None
            }
            _ => self.verify_external_direct_owner(&g, reservation)?,
        };
        if existing.is_none() && proof.is_some() {
            compiled
                .context("first direct allocation requires one-use compiler evidence")?
                .verify_reservation(reservation)?;
        }
        g.runtime_db
            .reserve_external_allocation_with_verified_owner(reservation, binding, proof.as_ref())
    }

    /// Caller holds the mutation permit and StateStore lock until the runtime
    /// transaction has compared the current launch claim and stop state.
    pub(super) fn verify_external_direct_owner(
        &self,
        inner: &Inner,
        reservation: &runtime_db::external_execution::ExternalAllocationReservation,
    ) -> Result<Option<VerifiedExternalDirectOwner>> {
        use runtime_db::external_execution::ExternalAllocationOwner;
        let ExternalAllocationOwner::DirectThread {
            chain_root_id,
            launch_owner,
            program,
        } = &reservation.owner
        else {
            return Ok(None);
        };
        reservation.validate()?;
        let snapshot = authoritative_snapshot_for_transition(
            inner,
            chain_root_id,
            &reservation.placement_thread_id,
        )?;
        anyhow::ensure!(
            snapshot.thread_id == reservation.placement_thread_id
                && snapshot.chain_root_id == *chain_root_id
                && !snapshot.status.is_terminal(),
            "external direct allocation requires its exact nonterminal born thread"
        );
        let capsule_hash = snapshot
            .admitted_launch_capsule_hash
            .as_deref()
            .context("external direct born thread has no admitted capsule")?;
        anyhow::ensure!(
            capsule_hash == reservation.admitted_capsule_hash,
            "external direct allocation changed its born capsule"
        );
        let capsule = load_admitted_launch_capsule(
            &self.state_authority,
            &self.bundle_admission_trust,
            capsule_hash,
        )?;
        anyhow::ensure!(
            capsule.project_authority == snapshot.project_authority
                && capsule.executor_ref == snapshot.executor_ref
                && matches!(
                    &capsule.execution_closure,
                    ryeos_state::objects::AdmittedExecutionClosure::DirectItemExecutor { .. }
                ),
            "external direct allocation has no exact ordinary executable capsule"
        );
        let endpoint =
            crate::thread_lifecycle::validate_retained_external_direct_program(&capsule, program)?;
        anyhow::ensure!(
            endpoint.binding_digest == reservation.binding_hash,
            "external direct allocation changed its sealed endpoint binding"
        );
        let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            snapshot_hash,
            realization: ryeos_state::objects::PinnedProjectRealization::ReadOnly,
            environment: ryeos_state::objects::EnvironmentAuthority::None,
            ..
        } = &capsule.project_authority
        else {
            bail!("external direct allocation requires an immutable pinned execution generation");
        };
        anyhow::ensure!(
            snapshot_hash == &reservation.base_snapshot_hash,
            "external direct allocation changed its immutable execution generation"
        );
        let claim = inner
            .runtime_db
            .get_launch_claim(&reservation.placement_thread_id)?
            .context("external direct allocation has no current launch claim")?;
        anyhow::ensure!(
            &claim.owner == launch_owner,
            "external direct allocation changed its current launch owner"
        );
        Ok(Some(VerifiedExternalDirectOwner {
            thread_id: snapshot.thread_id,
            chain_root_id: snapshot.chain_root_id,
            capsule_hash: capsule_hash.to_owned(),
            snapshot_hash: snapshot_hash.clone(),
            endpoint_binding_hash: endpoint.binding_digest,
            program_digest: program.digest()?,
            launch_owner: claim.owner,
        }))
    }

    /// Authenticate the immutable accepted root against the current
    /// operator-owned restored occurrence before a qualification-only Worker
    /// allocation may be reserved or first contacted. The allocation journal
    /// repeats the mutable occurrence/prerequisite checks in its transaction.
    pub(super) fn verify_external_qualification_owner(
        &self,
        state: &crate::state::AppState,
        inner: &Inner,
        reservation: &runtime_db::external_execution::ExternalAllocationReservation,
        binding: &crate::node_config::sections::external_execution::RetainedExternalExecutionBinding,
        admission: &crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
    ) -> Result<VerifiedExternalQualificationOwner> {
        use runtime_db::external_execution::ExternalAllocationOwner;
        let ExternalAllocationOwner::QualificationCandidate(owner) = &reservation.owner else {
            bail!("external qualification proof requires a qualification candidate owner");
        };
        reservation.validate()?;
        anyhow::ensure!(
            owner.coordinate == *admission.coordinate()
                && owner.qualification_operation_id == admission.qualification_operation_id()
                && owner.operator_principal == admission.owner_principal()
                && owner.operator_principal == admission.purpose().owner_fingerprint
                && reservation.admitted_capsule_hash == owner.coordinate.accepted_capsule_hash,
            "external qualification owner differs from authenticated accepted root"
        );
        let operator_fingerprint = owner
            .operator_principal
            .strip_prefix("fp:")
            .context("external qualification operator principal is not canonical")?;
        anyhow::ensure!(
            crate::operator_authority::admitted_operator_authority_digest(
                state,
                operator_fingerprint,
            )? == admission.operator_grant_digest(),
            "external qualification operator grant changed before first contact"
        );
        let root = authoritative_snapshot_for_transition(
            inner,
            &owner.coordinate.accepted_root_id,
            &owner.coordinate.accepted_root_id,
        )?;
        anyhow::ensure!(
            root.error.is_none()
                && root.thread_id == owner.coordinate.accepted_root_id
                && root.chain_root_id == owner.coordinate.accepted_root_id
                && root.requested_by.as_deref() == Some(owner.operator_principal.as_str())
                && root.project_authority == *admission.accepted_project_authority()
                && root.admitted_launch_capsule_hash.as_deref()
                    == Some(owner.coordinate.accepted_capsule_hash.as_str()),
            "external qualification candidate changed its exact accepted root"
        );
        match admission.callback_launch_owner() {
            Some(_) => anyhow::ensure!(
                root.status == ThreadStatus::Running,
                "callback qualification candidate requires its exact running accepted root"
            ),
            None => anyhow::ensure!(
                root.status == ThreadStatus::Completed && root.result.is_some(),
                "operator qualification candidate requires its exact completed accepted root"
            ),
        }
        let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            snapshot_hash,
            realization: ryeos_state::objects::PinnedProjectRealization::ReadOnly,
            workspace_outputs: None,
            environment: ryeos_state::objects::EnvironmentAuthority::None,
            ..
        } = admission.accepted_project_authority()
        else {
            bail!("external qualification candidate requires an immutable read-only root snapshot");
        };
        anyhow::ensure!(
            snapshot_hash == &reservation.base_snapshot_hash,
            "external qualification candidate changed the accepted root snapshot"
        );
        owner
            .program
            .validate_against_purpose(admission.purpose())?;
        let mut installed = state
            .node_config
            .external_execution
            .iter()
            .filter(|candidate| candidate.digest() == binding.digest());
        let current = installed
            .next()
            .context("external qualification placement binding is no longer installed")?;
        anyhow::ensure!(
            installed.next().is_none(),
            "external qualification placement binding is installed more than once"
        );
        current.check_qualification_program(&owner.program, admission.purpose())?;
        binding.check_qualification_program(&owner.program, admission.purpose())?;
        anyhow::ensure!(
            binding.digest() == reservation.binding_hash
                && binding.capacity_owner() == reservation.capacity_owner,
            "external qualification candidate changed its retained signed placement"
        );
        let qualified = inner
            .runtime_db
            .snapshot_qualification_operation(&owner.qualification_operation_id)?
            .context("external qualification candidate lost its restored occurrence")?;
        anyhow::ensure!(
            qualified.phase
                == runtime_db::runtime_snapshot_qualification::SnapshotQualificationPhase::OccurrenceBound
                && qualified.intent.owner_principal == owner.operator_principal
                && qualified.occurrence.as_ref() == Some(&owner.qualification_occurrence),
            "external qualification candidate changed its operator-owned occurrence"
        );
        let snapshot = inner
            .runtime_db
            .runtime_snapshot_operation(&qualified.intent.snapshot_operation_id)?
            .context("external qualification candidate lost its measured snapshot source")?;
        anyhow::ensure!(
            snapshot.intent.guest_runtime_manifest_hash
                == binding.backend_contract().guest_runtime_manifest_hash,
            "external qualification placement changed the measured guest-owner runtime"
        );
        inner
            .runtime_db
            .observed_consumer_verification_prerequisite(
                &owner.qualification_operation_id,
                &owner.operator_principal,
                &owner.coordinate,
            )?;
        let canary_request = admission
            .purpose()
            .policy_source
            .policy
            .producer_scenarios
            .get(&owner.coordinate.scenario_id)
            .context("Q proof lost its signed scenario")?
            .consumer_worker_request
            .as_ref()
            .filter(|request| request.controller_canary.is_some())
            .cloned();
        Ok(VerifiedExternalQualificationOwner {
            reservation: reservation.clone(),
            callback_launch_owner: admission.callback_launch_owner().map(str::to_owned),
            canary_request,
        })
    }

    pub(super) fn verify_external_qualification_owner_for_placement(
        &self,
        state: &crate::state::AppState,
        inner: &Inner,
        placement: &str,
        admission: &crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
    ) -> Result<VerifiedExternalQualificationOwner> {
        let record = inner
            .runtime_db
            .external_allocation(placement)?
            .context("Q executable mutation lost its allocation")?;
        let binding = inner
            .runtime_db
            .retained_external_binding(&record.reservation.binding_hash)?
            .context("Q executable mutation lost its retained signed binding")?;
        self.verify_external_qualification_owner(
            state,
            inner,
            &record.reservation,
            &binding,
            admission,
        )
    }

    pub(crate) fn claim_external_allocation_contact(
        &self,
        placement: &str,
        request_digest: &str,
    ) -> Result<runtime_db::external_execution::ExternalAllocationContactClaim> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let record = g
            .runtime_db
            .external_allocation(placement)?
            .context("external allocation was not reserved")?;
        let proof =
            if record.phase == runtime_db::external_execution::ExternalAllocationPhase::Reserved {
                self.verify_external_direct_owner(&g, &record.reservation)?
            } else {
                // Already-contacted work retains the original owner for cleanup;
                // a rotated or absent live claim is not permission to relaunch it.
                None
            };
        g.runtime_db
            .claim_external_allocation_contact_with_verified_owner(
                placement,
                request_digest,
                proof.as_ref(),
            )
    }

    /// Content-only retention under the existing write/CAS guard owner. The
    /// caller must keep its assembly guard until this transaction commits.
    pub fn retain_external_candidate_import(
        &self,
        placement: &str,
        imported: &ryeos_state::external_execution::export::ImportedCandidateContent,
    ) -> Result<()> {
        let permit = self.acquire_write_permit()?;
        let binding = self
            .lock()?
            .runtime_db
            .external_execution_channel(placement)?;
        // Hash potentially large reused content outside the global state lock
        // and SQLite write transaction. The CAS guard remains held throughout;
        // the short commit rechecks immutable channel/export coordinates.
        let verified =
            imported.validate_retention(&self.state_authority, permit.cas_guard(), &binding)?;
        let g = self.lock()?;
        g.runtime_db
            .retain_external_candidate_import(placement, &verified, &self.state_authority)
    }

    /// Reconcile one exact sealed external export from the controller's
    /// immutable signed transcript. The write permit retains the same CAS
    /// guard across reconstruction, closure verification, durable rooting and
    /// the atomic application commit. Reconciliation may repeat only
    /// content-addressed CAS writes; it never replays candidate execution.
    pub(crate) fn reconcile_external_candidate_import(
        &self,
        placement: &str,
        seal_sequence: u64,
        seal_digest: &str,
    ) -> Result<()> {
        use ryeos_state::external_execution::export::{
            CandidateExportAssembler, validate_retained_candidate_coordinates,
        };

        let permit = self.acquire_write_permit()?;
        let claim = self.lock()?.runtime_db.claim_external_candidate_import(
            placement,
            seal_sequence,
            seal_digest,
        )?;
        match claim {
            runtime_db::external_execution::ExternalCandidateImportClaim::AlreadyApplied(
                retained,
            ) => {
                if retained.seal_sequence != seal_sequence || retained.seal_digest != seal_digest {
                    bail!("retained external import changed its exact seal");
                }
                validate_retained_candidate_coordinates(
                    &self.state_authority,
                    permit.cas_guard(),
                    &retained.binding,
                    &retained.candidate_snapshot_hash,
                    retained.candidate_output_capture_hash.as_deref(),
                    &retained.completion_request_digest,
                    &retained.writer_exclusion_evidence_hash,
                )?;
                Ok(())
            }
            runtime_db::external_execution::ExternalCandidateImportClaim::Reconcile(plan) => {
                let mut assembler = CandidateExportAssembler::new(
                    &self.state_authority,
                    permit.cas_guard(),
                    plan.binding.clone(),
                    &plan.quiesce,
                )?;
                let mut imported = None;
                for (index, frame) in plan.export_frames.iter().enumerate() {
                    let result = assembler.accept(frame)?;
                    if index + 1 == plan.export_frames.len() {
                        imported = result;
                    } else if result.is_some() {
                        bail!("external candidate export sealed before its retained terminator");
                    }
                }
                let imported = imported.context(
                    "external candidate export retained no completed candidate at its seal",
                )?;
                let verified = imported.validate_retention(
                    &self.state_authority,
                    permit.cas_guard(),
                    &plan.binding,
                )?;
                let g = self.lock()?;
                g.runtime_db.finish_external_candidate_import(
                    placement,
                    &plan,
                    &verified,
                    &self.state_authority,
                )
            }
        }
    }

    pub(crate) fn recoverable_external_candidate_imports(
        &self,
        placement: Option<&str>,
    ) -> Result<Vec<runtime_db::external_execution::ExternalCandidateImportTarget>> {
        self.lock()?
            .runtime_db
            .recoverable_external_candidate_imports(placement)
    }

    pub fn external_execution_blob_roots(&self) -> Result<Vec<String>> {
        let state = self.lock()?;
        let mut roots = state.runtime_db.external_execution_blob_roots()?;
        roots.extend(state.runtime_db.restored_verifier_blob_roots()?);
        roots.sort();
        roots.dedup();
        Ok(roots)
    }

    pub fn record_external_execution_revocation(
        &self,
        placement: &str,
        wire: &[u8],
    ) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .record_external_execution_revocation(placement, wire)
    }

    /// A relay must serialize this sticky gate with actual dispatch. This
    /// read by itself is not a release permit and does not settle cleanup.
    pub fn external_execution_revoked(&self, placement: &str) -> Result<bool> {
        self.lock()?
            .runtime_db
            .external_execution_revoked(placement)
    }
}

impl StateStore {
    /// The journal rows behind the operator's external-execution status,
    /// read under one store lock so they agree with each other.
    pub(crate) fn external_execution_status_rows(
        &self,
    ) -> Result<crate::external_execution_status::StatusRows> {
        let g = self.lock()?;
        Ok(crate::external_execution_status::StatusRows {
            allocations: g.runtime_db.unsettled_external_allocations()?,
            local_starts: g.runtime_db.open_qualification_local_starts()?,
            profiles: g.runtime_db.held_credential_profiles()?,
        })
    }
}
