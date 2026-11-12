//! External qualification workers, guest scratch and custody.
use super::*;

impl StateStore {
    /// Local qualification Worker rows that a previous daemon generation left
    /// `Attached`, as (placement, exact process identity). Start recovery
    /// proves each group dead and then settles exactly that row.
    pub fn prior_generation_attached_qualification_workers(
        &self,
    ) -> Result<Vec<(String, crate::process::ExecutionProcessIdentity)>> {
        let records = self
            .lock()?
            .runtime_db
            .attached_qualification_worker_processes(runtime_db::daemon_generation_id())?;
        Ok(records
            .into_iter()
            .map(|record| (record.placement_thread_id, record.process_identity))
            .collect())
    }

    /// Settle a Q directory borrow no Worker ever consumed; the caller has
    /// proved the Worker's scope slot empty.
    pub(crate) fn settle_unconsumed_directory_borrow(
        &self,
        placement: &str,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        self.lock()?
            .runtime_db
            .settle_unconsumed_directory_borrow(placement)
    }

    pub(crate) fn prior_generation_qualification_credential_custody(&self) -> Result<Vec<String>> {
        self.lock()?
            .runtime_db
            .unreleased_qualification_credential_custody(runtime_db::daemon_generation_id())
    }

    /// Q local starts a previous daemon generation recorded.
    pub(crate) fn prior_generation_qualification_local_starts(
        &self,
    ) -> Result<Vec<runtime_db::external_execution::QualificationLocalStartRecord>> {
        self.lock()?
            .runtime_db
            .prior_generation_qualification_local_starts(runtime_db::daemon_generation_id())
    }

    // Prior-generation Q settlement. The rows below were created by a daemon
    // generation that is dead, so no process holds their descriptors; the
    // `qualification_cold_settlement` pass removes their directories by
    // recorded coordinate under the node-private parents and drives the
    // ordinary transitions without process-local custody. Every wrapper
    // refuses a row of this generation: its original custody owns it.

    fn require_prior_generation(daemon_generation_id: &str) -> Result<()> {
        anyhow::ensure!(
            daemon_generation_id != runtime_db::daemon_generation_id(),
            "Q row belongs to this daemon generation; its original custody drives its cleanup"
        );
        Ok(())
    }

    /// The node-private parents of every Q directory, for the
    /// prior-generation pass and nothing else.
    pub(crate) fn qualification_custody_parents(
        &self,
    ) -> Result<runtime_db::qualification_cold_custody::QualificationCustodyParents> {
        runtime_db::qualification_cold_custody::QualificationCustodyParents::open(
            self.state_authority.runtime_directory(),
        )
    }

    pub(crate) fn prior_generation_open_qualification_guest_scratch(
        &self,
    ) -> Result<Vec<runtime_db::external_execution::QualificationGuestScratchRecord>> {
        self.lock()?
            .runtime_db
            .prior_generation_open_qualification_guest_scratch(runtime_db::daemon_generation_id())
    }

    pub(crate) fn begin_qualification_guest_scratch_cleanup_prior_generation(
        &self,
        expected: &runtime_db::external_execution::QualificationGuestScratchRecord,
    ) -> Result<runtime_db::external_execution::QualificationGuestScratchRecord> {
        Self::require_prior_generation(&expected.daemon_generation_id)?;
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .begin_qualification_guest_scratch_cleanup_with(
                expected,
                runtime_db::external_execution::ScratchCleanupOwner::PriorGeneration,
            )
    }

    pub(crate) fn record_qualification_guest_scratch_removed_prior_generation(
        &self,
        expected: &runtime_db::external_execution::QualificationGuestScratchRecord,
    ) -> Result<runtime_db::external_execution::QualificationGuestScratchRecord> {
        Self::require_prior_generation(&expected.daemon_generation_id)?;
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .record_qualification_guest_scratch_removed_with(
                expected,
                runtime_db::external_execution::ScratchCleanupOwner::PriorGeneration,
            )
    }

    pub(crate) fn close_qualification_guest_scratch_prior_generation(
        &self,
        expected: &runtime_db::external_execution::QualificationGuestScratchRecord,
    ) -> Result<runtime_db::external_execution::QualificationGuestScratchRecord> {
        Self::require_prior_generation(&expected.daemon_generation_id)?;
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .close_qualification_guest_scratch_with(
                expected,
                runtime_db::external_execution::ScratchCleanupOwner::PriorGeneration,
            )
    }

    pub(crate) fn begin_qualification_local_cleanup_prior_generation(
        &self,
        expected: &runtime_db::external_execution::QualificationLocalStartRecord,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        Self::require_prior_generation(&expected.daemon_generation_id)?;
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .begin_qualification_local_cleanup(expected)
    }

    pub(crate) fn authorize_qualification_workspace_removal_prior_generation(
        &self,
        expected: &runtime_db::external_execution::QualificationLocalStartRecord,
    ) -> Result<()> {
        Self::require_prior_generation(&expected.daemon_generation_id)?;
        self.lock()?
            .runtime_db
            .authorize_qualification_workspace_removal(expected)
    }

    pub(crate) fn record_qualification_workspace_removed_prior_generation(
        &self,
        expected: &runtime_db::external_execution::QualificationLocalStartRecord,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        Self::require_prior_generation(&expected.daemon_generation_id)?;
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .record_qualification_workspace_removed(expected)
    }

    pub(crate) fn record_qualification_state_root_removed_prior_generation(
        &self,
        expected: &runtime_db::external_execution::QualificationLocalStartRecord,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        Self::require_prior_generation(&expected.daemon_generation_id)?;
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .record_qualification_state_root_removed(expected)
    }

    pub(crate) fn close_qualification_local_start_prior_generation(
        &self,
        expected: &runtime_db::external_execution::QualificationLocalStartRecord,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        Self::require_prior_generation(&expected.daemon_generation_id)?;
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .close_qualification_local_start(expected)
    }

    pub(crate) fn qualification_worker_process(
        &self,
        placement: &str,
    ) -> Result<Option<runtime_db::external_execution::QualificationWorkerProcessRecord>> {
        self.lock()?
            .runtime_db
            .qualification_worker_process(placement)
    }

    pub(crate) fn qualification_local_start(
        &self,
        placement: &str,
    ) -> Result<Option<runtime_db::external_execution::QualificationLocalStartRecord>> {
        self.lock()?.runtime_db.qualification_local_start(placement)
    }

    pub(crate) fn qualification_guest_scratch(
        &self,
        placement: &str,
    ) -> Result<Option<runtime_db::external_execution::QualificationGuestScratchRecord>> {
        self.lock()?
            .runtime_db
            .qualification_guest_scratch(placement)
    }

    pub(crate) fn settled_unclosed_qualification_guest_scratch_placements(
        &self,
    ) -> Result<Vec<String>> {
        self.lock()?
            .runtime_db
            .settled_unclosed_qualification_guest_scratch_placements(
                runtime_db::daemon_generation_id(),
            )
    }

    pub(crate) fn completed_qualification_guest_scratch_transport(
        &self,
        placement: &str,
        coordinate: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
        qualification_operation_id: &str,
        owner_principal: &str,
        program_digest: &str,
    ) -> Result<bool> {
        // A negative readiness probe grants no proof and must remain usable
        // before allocation and during failure cleanup.
        if !self
            .lock()?
            .runtime_db
            .completed_qualification_guest_scratch_transport(
                placement,
                coordinate,
                qualification_operation_id,
                owner_principal,
                program_digest,
            )?
        {
            return Ok(false);
        }
        let guard = self.state_authority.acquire_shared_guard()?;
        self.verify_retained_external_q_fixture_custody_admitted(&guard, placement)?;
        self.lock()?
            .runtime_db
            .completed_qualification_guest_scratch_transport(
                placement,
                coordinate,
                qualification_operation_id,
                owner_principal,
                program_digest,
            )
    }

    /// Read the exact closed Q transport, local cleanup, native settlement,
    /// provider terminal and structural transcript at one retained DB view.
    /// The result grants no Worker semantic success or qualification claim.
    pub(crate) fn qualification_historical_transport_evidence_admitted(
        &self,
        guard: &ryeos_state::CasMutationGuard,
        coordinate: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
        qualification_operation_id: &str,
        owner_principal: &str,
    ) -> Result<runtime_db::external_execution::QualificationHistoricalTransportEvidence> {
        let placement = ryeos_state::external_execution::qualification_session::qualification_candidate_placement_key(
            coordinate, qualification_operation_id,
        )?;
        self.verify_retained_external_q_fixture_custody_admitted(guard, &placement)?;
        self.lock()?
            .runtime_db
            .qualification_historical_transport_evidence(
                coordinate,
                qualification_operation_id,
                owner_principal,
            )
    }

    /// No child descriptor is adopted on restart. Only the original
    /// state-owned parent and an absent entry may retire a precreation row.
    pub(crate) fn retire_absent_reserved_qualification_guest_scratch(
        &self,
        expected: &runtime_db::external_execution::QualificationGuestScratchRecord,
        parent: &lillux::PinnedDirectory,
    ) -> Result<runtime_db::external_execution::QualificationGuestScratchRecord> {
        let _permit = self.acquire_write_permit()?;
        parent.require_owner_private_directory()?;
        parent.ensure_path_binding()?;
        let original_parent = self
            .state_authority
            .runtime_directory()
            .open_child_directory(std::ffi::OsStr::new("qualification-guest-inputs"))?
            .context("Q absent-child recovery lost its original state-owned parent")?;
        anyhow::ensure!(
            parent.is_same_directory(&original_parent)?,
            "Q absent-child recovery changed its original parent"
        );
        self.lock()?
            .runtime_db
            .retire_absent_reserved_qualification_guest_scratch(expected, parent)
    }

    /// First Q scratch intent precedes filesystem Create and allocation
    /// reservation. The nonserializable accepted-root proof is checked before
    /// the transaction repeats current occurrence and measurement liveness.
    pub(crate) fn reserve_qualification_guest_scratch(
        &self,
        state: &crate::state::AppState,
        admission: crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
        program: &ryeos_state::external_execution::qualification_session::AdmittedExternalQualificationSessionProgram,
        qualification_operation_id: &str,
        parent: &lillux::PinnedDirectory,
    ) -> Result<runtime_db::external_execution::QualificationGuestScratchRecord> {
        let _permit = self.acquire_write_permit()?;
        admission.with_live_callback(|| {
        parent.require_owner_private_directory()?;
        parent.ensure_path_binding()?;
        let original_parent = self
            .state_authority
            .runtime_directory()
            .open_child_directory(std::ffi::OsStr::new("qualification-guest-inputs"))?
            .context("Q scratch lost its original state-owned parent")?;
        anyhow::ensure!(
            parent.is_same_directory(&original_parent)?,
            "Q scratch parent differs from pinned state authority"
        );
        let coordinate = admission.coordinate();
        program.validate_against_purpose(admission.purpose())?;
        anyhow::ensure!(
            admission.qualification_operation_id() == qualification_operation_id
                && admission.owner_principal() == admission.purpose().owner_fingerprint,
            "Q scratch changed its authenticated root program"
        );
        let operator_fingerprint = admission
            .owner_principal()
            .strip_prefix("fp:")
            .context("Q scratch operator principal is not canonical")?;
        anyhow::ensure!(
            crate::operator_authority::admitted_operator_authority_digest(
                state,
                operator_fingerprint,
            )? == admission.operator_grant_digest(),
            "Q scratch operator grant changed before reservation"
        );
        let placement = ryeos_state::external_execution::qualification_session::qualification_candidate_placement_key(
            coordinate,
            qualification_operation_id,
        )?;
        let root = parent.path().join(&placement);
        let g = self.lock()?;
        let accepted = authoritative_snapshot_for_transition(
            &g,
            &coordinate.accepted_root_id,
            &coordinate.accepted_root_id,
        )?;
        anyhow::ensure!(
            accepted.error.is_none()
                && accepted.thread_id == coordinate.accepted_root_id
                && accepted.chain_root_id == coordinate.accepted_root_id
                && accepted.requested_by.as_deref() == Some(admission.owner_principal())
                && accepted.project_authority == *admission.accepted_project_authority()
                && accepted.admitted_launch_capsule_hash.as_deref()
                    == Some(coordinate.accepted_capsule_hash.as_str()),
            "Q scratch changed its exact accepted root"
        );
        match admission.callback_launch_owner() {
            Some(_) => anyhow::ensure!(
                accepted.status == ThreadStatus::Running,
                "callback Q scratch requires its exact running accepted root"
            ),
            None => anyhow::ensure!(
                accepted.status == ThreadStatus::Completed && accepted.result.is_some(),
                "operator Q scratch requires its exact completed accepted root"
            ),
        }
        g.runtime_db.reserve_qualification_guest_scratch(
            coordinate,
            qualification_operation_id,
            admission.owner_principal(),
            &program.digest()?,
            &root,
            admission.callback_launch_owner(),
        )
        })
    }

    /// The original guard was retained before this one-way Created CAS.
    pub(crate) fn mark_qualification_guest_scratch_created(
        &self,
        state: &crate::state::AppState,
        expected: &runtime_db::external_execution::QualificationGuestScratchRecord,
        custody: &std::sync::Arc<
            crate::qualification_workspace_custody::QualificationGuestScratchCustody,
        >,
    ) -> Result<runtime_db::external_execution::QualificationGuestScratchRecord> {
        let _permit = self.acquire_write_permit()?;
        let retained = state
            .qualification_workspace_custody
            .borrow_guest_scratch(expected)?;
        anyhow::ensure!(
            std::sync::Arc::ptr_eq(&retained, custody)
                && custody.pinned_root()?.path() == std::path::Path::new(&expected.scratch_root),
            "Q scratch creation lost original descriptor custody"
        );
        self.lock()?
            .runtime_db
            .mark_qualification_guest_scratch_created(expected)
    }

    pub(crate) fn begin_qualification_guest_scratch_cleanup(
        &self,
        state: &crate::state::AppState,
        expected: &runtime_db::external_execution::QualificationGuestScratchRecord,
        custody: &std::sync::Arc<
            crate::qualification_workspace_custody::QualificationGuestScratchCustody,
        >,
    ) -> Result<runtime_db::external_execution::QualificationGuestScratchRecord> {
        let retained = state
            .qualification_workspace_custody
            .borrow_guest_scratch(expected)?;
        anyhow::ensure!(
            std::sync::Arc::ptr_eq(&retained, custody)
                && custody.pinned_root()?.path() == std::path::Path::new(&expected.scratch_root),
            "Q scratch cleanup lost its original descriptor"
        );
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .begin_qualification_guest_scratch_cleanup(expected)
    }

    pub(crate) fn record_qualification_guest_scratch_removed(
        &self,
        state: &crate::state::AppState,
        expected: &runtime_db::external_execution::QualificationGuestScratchRecord,
        custody: &std::sync::Arc<
            crate::qualification_workspace_custody::QualificationGuestScratchCustody,
        >,
    ) -> Result<runtime_db::external_execution::QualificationGuestScratchRecord> {
        let retained = state
            .qualification_workspace_custody
            .borrow_guest_scratch(expected)?;
        anyhow::ensure!(
            std::sync::Arc::ptr_eq(&retained, custody),
            "Q scratch removal changed original custody"
        );
        custody.guard().prove_owned_scratch_removed()?;
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .record_qualification_guest_scratch_removed(expected)
    }

    pub(crate) fn close_qualification_guest_scratch(
        &self,
        state: &crate::state::AppState,
        expected: &runtime_db::external_execution::QualificationGuestScratchRecord,
        custody: &std::sync::Arc<
            crate::qualification_workspace_custody::QualificationGuestScratchCustody,
        >,
    ) -> Result<runtime_db::external_execution::QualificationGuestScratchRecord> {
        let retained = state
            .qualification_workspace_custody
            .borrow_guest_scratch(expected)?;
        anyhow::ensure!(
            std::sync::Arc::ptr_eq(&retained, custody),
            "Q scratch close changed original custody"
        );
        custody.guard().prove_owned_scratch_removed()?;
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .close_qualification_guest_scratch(expected)
    }

    pub(crate) fn reserve_qualification_controller_canary(
        &self,
        state: &crate::state::AppState,
        expected: &runtime_db::external_execution::QualificationLocalStartRecord,
        witness: &ryeos_external_execution::guest_inputs::ExternalGuestInputExclusionWitness,
        admission:crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        let _permit = self.acquire_write_permit()?;
        admission.with_live_callback(|| {
            let g = self.lock()?;
            let proof = self.verify_external_qualification_owner_for_placement(
                state,
                &g,
                &expected.placement_thread_id,
                &admission,
            )?;
            g.runtime_db.reserve_qualification_controller_canary(
                expected,
                witness.identity_digest(),
                &proof,
            )
        })
    }

    /// Only the original owner module can construct this opaque successful
    /// captured-adapter invocation. Public deserialized bindings cannot enter.
    pub(crate) fn bind_qualification_controller_canary(
        &self,
        state: &crate::state::AppState,
        prepared: &crate::external_qualification_local::PreparedQualificationControllerCanary,
        admission:crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        let coordinate=crate::qualification_workspace_custody::QualificationWorkspaceCustodyCoordinate::from_start(prepared.record())?;
        let custody = state
            .qualification_workspace_custody
            .borrow_exact(&coordinate)?;
        let (path, observed) = custody.recheck_controller_canary()?;
        anyhow::ensure!(
            path == prepared.binding().controller_path
                && observed == prepared.binding().initial_file,
            "Q canary adapter result lost original file custody"
        );
        let _permit = self.acquire_write_permit()?;
        admission.with_live_callback(|| {
            let g = self.lock()?;
            let proof = self.verify_external_qualification_owner_for_placement(
                state,
                &g,
                &prepared.record().placement_thread_id,
                &admission,
            )?;
            g.runtime_db.bind_qualification_controller_canary(
                prepared.record(),
                prepared.binding(),
                &proof,
            )
        })
    }

    /// Caller holds the original occurrence cleanup gate through this method.
    /// Re-read the original private observation here; public JSON supplied by a
    /// caller is never accepted as terminal file custody.
    pub(crate) fn retain_qualification_controller_canary_terminal(
        &self,
        state: &crate::state::AppState,
        expected: &runtime_db::external_execution::QualificationLocalStartRecord,
        custody: &std::sync::Arc<
            crate::qualification_workspace_custody::QualificationWorkspaceCustody,
        >,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        let coordinate = crate::qualification_workspace_custody::QualificationWorkspaceCustodyCoordinate::from_start(expected)?;
        let retained = state
            .qualification_workspace_custody
            .borrow_exact(&coordinate)?;
        anyhow::ensure!(
            std::sync::Arc::ptr_eq(&retained, custody),
            "Q terminal canary changed its original owner"
        );
        let _permit = self.acquire_write_permit()?;
        let binding = expected
            .controller_canary_binding
            .as_ref()
            .context("Q terminal canary has no original binding")?;
        let (path, terminal_file) = custody.recheck_controller_canary()?;
        anyhow::ensure!(
            path == binding.controller_path,
            "Q terminal canary changed original path"
        );
        let evidence = ryeos_state::external_execution::qualification_controller_canary::QControllerCanaryEvidence {
            schema:1,binding:binding.clone(),terminal_file,
        };
        evidence.validate()?;
        self.lock()?
            .runtime_db
            .record_qualification_controller_canary_terminal(expected, &evidence)
    }

    /// A read-only check for the original Q directory immediately before an
    /// owner-gated view borrow and held Worker spawn.
    pub(crate) fn authorize_qualification_local_spawn(
        &self,
        state: &crate::state::AppState,
        placement: &str,
        admission: crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        let _permit = self.acquire_write_permit()?;
        admission.with_live_callback(|| {
            let g = self.lock()?;
            anyhow::ensure!(
                self.process_attachment_admission_open
                    .load(Ordering::Acquire),
                "qualification local spawn is fenced during daemon shutdown"
            );
            let proof = self.verify_external_qualification_owner_for_placement(
                state, &g, placement, &admission,
            )?;
            g.runtime_db
                .authorize_qualification_local_spawn(placement, &proof)
        })
    }

    pub(crate) fn authorize_qualification_worker_release(
        &self,
        state: &crate::state::AppState,
        placement: &str,
        admission: crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        admission.with_live_callback(|| {
            let g = self.lock()?;
            anyhow::ensure!(
                self.process_attachment_admission_open
                    .load(Ordering::Acquire),
                "qualification Worker release is fenced during daemon shutdown"
            );
            let proof = self.verify_external_qualification_owner_for_placement(
                state, &g, placement, &admission,
            )?;
            g.runtime_db
                .authorize_qualification_worker_release(placement, &proof)
        })
    }

    /// One durable possible-borrow intent before the original private directory
    /// is cloned for the held Q Worker. No replacement borrow is admitted.
    pub(crate) fn reserve_qualification_workspace_directory_borrow(
        &self,
        placement: &str,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        anyhow::ensure!(
            self.process_attachment_admission_open
                .load(Ordering::Acquire),
            "qualification workspace borrow is fenced during daemon shutdown"
        );
        g.runtime_db
            .reserve_qualification_workspace_directory_borrow(placement)
    }

    /// The cache path comes from the pinned runtime state authority. The
    /// returned workspace coordinate must be journaled before directory Create.
    pub(crate) fn reserve_qualification_local_start(
        &self,
        placement: &str,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        let _permit = self.acquire_write_permit()?;
        let cache = self
            .state_authority
            .runtime_directory()
            .open_or_create_child(std::ffi::OsStr::new("cache"), 0o700)?;
        cache.require_owner_private_directory()?;
        let g = self.lock()?;
        anyhow::ensure!(
            self.process_attachment_admission_open
                .load(Ordering::Acquire),
            "qualification local start is fenced during daemon shutdown"
        );
        g.runtime_db
            .reserve_qualification_local_start(placement, cache.path())
    }

    pub(crate) fn claim_qualification_local_credential_custody(
        &self,
        state: &crate::state::AppState,
        placement: &str,
        profile_id: &str,
        owner_principal: &str,
        contract_digest: &str,
        workload_family: &str,
        subject_projection_contract: &str,
        admission: crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        let _permit = self.acquire_write_permit()?;
        admission.with_live_callback(|| {
            let g = self.lock()?;
            let proof = self.verify_external_qualification_owner_for_placement(
                state, &g, placement, &admission,
            )?;
            g.runtime_db.claim_qualification_local_credential_custody(
                placement,
                profile_id,
                owner_principal,
                contract_digest,
                workload_family,
                subject_projection_contract,
                &proof,
            )
        })
    }

    pub(crate) fn require_live_qualification_local_credential_custody(
        &self,
        placement: &str,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        self.lock()?
            .runtime_db
            .require_live_qualification_local_credential_custody(placement)
    }

    pub(crate) fn release_qualification_local_credential_custody(
        &self,
        placement: &str,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .release_qualification_local_credential_custody(placement)
    }

    /// Bind the original privately materialized project. The caller holds its
    /// pinned custody; this receipt does not permit reopening it after restart.
    pub(crate) fn complete_qualification_private_workspace(
        &self,
        placement: &str,
        identity: lillux::PinnedDirectoryIdentity,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        anyhow::ensure!(
            self.process_attachment_admission_open
                .load(Ordering::Acquire),
            "qualification private workspace is fenced during daemon shutdown"
        );
        g.runtime_db
            .complete_qualification_private_workspace(placement, identity)
    }

    pub(crate) fn mark_qualification_local_cleanup_unproved(
        &self,
        placement: &str,
        reason: &str,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .mark_qualification_local_cleanup_unproved(placement, reason)
    }

    pub(super) fn checked_qualification_cleanup_custody(
        &self,
        state: &crate::state::AppState,
        placement: &str,
        custody: &Arc<crate::qualification_workspace_custody::QualificationWorkspaceCustody>,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        anyhow::ensure!(
            std::ptr::eq(self, Arc::as_ptr(&state.state_store)),
            "Q cleanup custody belongs to another StateStore"
        );
        let record = self
            .qualification_local_start(placement)?
            .context("Q cleanup has no retained local start")?;
        let coordinate =
            crate::qualification_workspace_custody::QualificationWorkspaceCustodyCoordinate::from_start(&record)?;
        anyhow::ensure!(
            custody.coordinate() == &coordinate
                && Arc::ptr_eq(
                    custody,
                    &state
                        .qualification_workspace_custody
                        .borrow_exact(&coordinate)?,
                ),
            "Q cleanup lacks original registered workspace custody"
        );
        Ok(record)
    }

    /// Fence fresh local contact. The original process-local custody is
    /// mandatory even when a prior failure was already journaled.
    pub(crate) fn begin_qualification_local_cleanup(
        &self,
        state: &crate::state::AppState,
        placement: &str,
        custody: &Arc<crate::qualification_workspace_custody::QualificationWorkspaceCustody>,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        let _permit = self.acquire_write_permit()?;
        let record = self.checked_qualification_cleanup_custody(state, placement, custody)?;
        self.lock()?
            .runtime_db
            .begin_qualification_local_cleanup(&record)
    }

    /// Close a fully created pair of original Q roots if custody registration
    /// failed before private workspace preparation. The facade holds the Q occurrence contact
    /// gate for this entire call. No descriptor is reconstructed from a path.
    /// Each physical removal follows a durable no-contact intent and precedes
    /// its corresponding retained receipt; interruption leaves an explicit
    /// cleanup phase rather than a reusable Reserved row.
    pub(crate) fn rollback_unregistered_qualification_roots(
        &self,
        state: &crate::state::AppState,
        expected: &runtime_db::external_execution::QualificationLocalStartRecord,
        workspace: &Arc<crate::temp_dir_guard::TempDirGuard>,
        state_root_parent: &lillux::PinnedDirectory,
        state_root: &lillux::PinnedDirectory,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        use std::ffi::OsStr;

        anyhow::ensure!(
            std::ptr::eq(self, Arc::as_ptr(&state.state_store))
                && expected.phase
                    == runtime_db::external_execution::QualificationLocalStartPhase::Reserved,
            "Q unregistered rollback requires its original StateStore reservation"
        );
        let current = self
            .qualification_local_start(&expected.placement_thread_id)?
            .context("Q unregistered rollback lost its start")?;
        anyhow::ensure!(
            current == *expected,
            "Q unregistered rollback changed its exact Reserved row"
        );
        let coordinate =
            crate::qualification_workspace_custody::QualificationWorkspaceCustodyCoordinate::from_start(&current)?;
        state
            .qualification_workspace_custody
            .ensure_unregistered(&current.placement_thread_id)?;
        let retained = state
            .qualification_workspace_custody
            .borrow_unregistered_rollback(&coordinate)?;
        anyhow::ensure!(
            Arc::ptr_eq(retained.workspace(), workspace)
                && retained
                    .state_root_parent()
                    .is_same_directory(state_root_parent)?
                && retained.state_root().is_same_directory(state_root)?,
            "Q rollback changed its retained original descriptor custody"
        );
        let workspace_root = Path::new(&current.workspace_root);
        let project = workspace_root.join("project");
        let state_path = Path::new(&current.state_root);
        let state_parent_path = state_path.parent().context("Q state root has no parent")?;
        let expected_state_parent = self
            .state_authority
            .runtime_directory()
            .path()
            .join("cache")
            .join("qualification-local");
        anyhow::ensure!(
            coordinate.placement() == current.placement_thread_id
                && workspace.path().as_deref() == Some(workspace_root)
                && workspace.owns_effective_path(&project)
                && workspace.owns_workspace_project_path(&project)
                && workspace.owned_scratch_root()?.path() == workspace_root
                && state_root.path() == state_path
                && state_root_parent.path() == state_parent_path
                && state_parent_path == expected_state_parent,
            "Q unregistered rollback changed its original pinned roots"
        );
        workspace.owned_scratch_root()?.ensure_path_binding()?;
        state_root_parent.ensure_path_binding()?;
        state_root.ensure_path_binding()?;
        let name = OsStr::new(&current.workspace_id);
        let child = state_root_parent
            .open_child_directory(name)?
            .context("Q original state root disappeared before rollback")?;
        anyhow::ensure!(
            child.is_same_directory(state_root)?,
            "Q unregistered state root was replaced before rollback"
        );

        // Original descriptors and no-Create ownership were checked above.
        // Release only the proven pre-contact credential fence, then use the
        // refreshed journal row for cleanup CAS; no credential-home I/O occurs.
        let released =
            self.release_qualification_local_credential_custody(&current.placement_thread_id)?;
        let current = self
            .qualification_local_start(&current.placement_thread_id)?
            .context("Q unregistered rollback lost its released credential row")?;
        anyhow::ensure!(
            current == released,
            "Q unregistered rollback changed after credential release"
        );
        {
            let _permit = self.acquire_write_permit()?;
            self.lock()?
                .runtime_db
                .begin_unregistered_qualification_precreate_rollback(&current)?;
        }
        self.resume_unregistered_qualification_rollback(state, &current.placement_thread_id)
    }

    /// Same-daemon continuation of the original no-Create rollback. A cold
    /// daemon has no retained descriptor pair and cannot infer removal from
    /// diagnostic paths or a missing current configuration entry.
    pub(crate) fn resume_unregistered_qualification_rollback(
        &self,
        state: &crate::state::AppState,
        placement: &str,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        use std::ffi::OsStr;

        anyhow::ensure!(
            std::ptr::eq(self, Arc::as_ptr(&state.state_store)),
            "Q rollback resume belongs to another StateStore"
        );
        let mut current = self
            .qualification_local_start(placement)?
            .context("Q rollback resume lost its retained start")?;
        anyhow::ensure!(
            matches!(
                current.phase,
                runtime_db::external_execution::QualificationLocalStartPhase::CleanupIntent
                    | runtime_db::external_execution::QualificationLocalStartPhase::WorkspaceRemoved
                    | runtime_db::external_execution::QualificationLocalStartPhase::StateRemoved
                    | runtime_db::external_execution::QualificationLocalStartPhase::Closed
            ) && current.workspace_identity.is_none()
                && current.worker_incarnation_digest.is_none()
                && current.directory_borrow_intent.is_none(),
            "Q rollback resume lacks its exact no-Create cleanup phase"
        );
        let coordinate =
            crate::qualification_workspace_custody::QualificationWorkspaceCustodyCoordinate::from_start(&current)?;
        state
            .qualification_workspace_custody
            .ensure_unregistered(placement)?;
        let retained = state
            .qualification_workspace_custody
            .borrow_unregistered_rollback(&coordinate)?;
        let workspace = retained.workspace();
        let parent = retained.state_root_parent();
        let root = retained.state_root();
        let workspace_id = current.workspace_id.clone();
        let name = OsStr::new(&workspace_id);
        let state_parent_path = Path::new(&current.state_root)
            .parent()
            .context("Q rollback state root has no parent")?;
        anyhow::ensure!(
            workspace.owned_scratch_root()?.path() == Path::new(&current.workspace_root)
                && root.path() == Path::new(&current.state_root)
                && parent.path() == state_parent_path
                && state_parent_path
                    == self
                        .state_authority
                        .runtime_directory()
                        .path()
                        .join("cache")
                        .join("qualification-local"),
            "Q rollback resume changed its original pinned root coordinates"
        );
        parent.ensure_path_binding()?;
        // Resume also covers interruptions after cleanup intent. The durable
        // no-borrow gate remains authoritative without a live callback.
        let released = self.release_qualification_local_credential_custody(placement)?;
        current = self
            .qualification_local_start(placement)?
            .context("Q rollback resume lost its released credential row")?;
        anyhow::ensure!(
            current == released,
            "Q rollback resume changed after credential release"
        );

        if current.phase
            == runtime_db::external_execution::QualificationLocalStartPhase::CleanupIntent
        {
            self.lock()?
                .runtime_db
                .authorize_qualification_workspace_removal(&current)?;
            workspace.remove_uncreated_workspace_now()?;
            workspace.prove_uncreated_workspace_removed()?;
            current = {
                let _permit = self.acquire_write_permit()?;
                self.lock()?
                    .runtime_db
                    .record_qualification_workspace_removed(&current)?
            };
        } else {
            workspace.prove_uncreated_workspace_removed()?;
        }

        if current.phase
            == runtime_db::external_execution::QualificationLocalStartPhase::WorkspaceRemoved
        {
            match parent.open_child_directory(name)? {
                Some(child) => {
                    anyhow::ensure!(
                        child.is_same_directory(root)?
                            && parent
                                .remove_empty_child_if_same(name, root)
                                .map(|receipt| receipt.is_some())?,
                        "Q rollback state root changed or remained nonempty"
                    );
                }
                None => {
                    // The original unlink may have succeeded before its
                    // durable CAS. The same retained parent proves absence.
                }
            }
            anyhow::ensure!(
                parent.open_child_directory(name)?.is_none(),
                "Q rollback state root reappeared before receipt"
            );
            current = {
                let _permit = self.acquire_write_permit()?;
                self.lock()?
                    .runtime_db
                    .record_qualification_state_root_removed(&current)?
            };
        } else {
            anyhow::ensure!(
                parent.open_child_directory(name)?.is_none(),
                "Q rollback state root reappeared after removal receipt"
            );
        }

        if current.phase
            == runtime_db::external_execution::QualificationLocalStartPhase::StateRemoved
        {
            let _permit = self.acquire_write_permit()?;
            current = self
                .lock()?
                .runtime_db
                .close_qualification_local_start(&current)?;
        }
        Ok(current)
    }

    /// Remove only the original private root after the exact Worker and
    /// connector have settled. No backend view or Destroy receipt is involved.
    pub(crate) fn remove_qualification_workspace_root(
        &self,
        state: &crate::state::AppState,
        expected: &runtime_db::external_execution::QualificationLocalStartRecord,
        custody: &Arc<crate::qualification_workspace_custody::QualificationWorkspaceCustody>,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        let current = self.checked_qualification_cleanup_custody(
            state,
            &expected.placement_thread_id,
            custody,
        )?;
        anyhow::ensure!(
            current == *expected,
            "Q workspace removal lost its exact journal row"
        );
        self.lock()?
            .runtime_db
            .authorize_qualification_workspace_removal(&current)?;
        custody.workspace().remove_uncreated_workspace_now()?;
        custody.workspace().prove_uncreated_workspace_removed()?;
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .record_qualification_workspace_removed(expected)
    }

    pub(crate) fn remove_qualification_state_root(
        &self,
        state: &crate::state::AppState,
        expected: &runtime_db::external_execution::QualificationLocalStartRecord,
        custody: &Arc<crate::qualification_workspace_custody::QualificationWorkspaceCustody>,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        let current = self.checked_qualification_cleanup_custody(
            state,
            &expected.placement_thread_id,
            custody,
        )?;
        anyhow::ensure!(current == *expected
            && current.phase == runtime_db::external_execution::QualificationLocalStartPhase::WorkspaceRemoved,
            "Q state root removal lost its exact workspace removal");
        custody.remove_state_root_now()?;
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .record_qualification_state_root_removed(expected)
    }

    pub(crate) fn close_qualification_local_start(
        &self,
        state: &crate::state::AppState,
        expected: &runtime_db::external_execution::QualificationLocalStartRecord,
        custody: &Arc<crate::qualification_workspace_custody::QualificationWorkspaceCustody>,
    ) -> Result<runtime_db::external_execution::QualificationLocalStartRecord> {
        let _permit = self.acquire_write_permit()?;
        let current = self.checked_qualification_cleanup_custody(
            state,
            &expected.placement_thread_id,
            custody,
        )?;
        anyhow::ensure!(
            current == *expected
                && current.phase
                    == runtime_db::external_execution::QualificationLocalStartPhase::StateRemoved,
            "Q local close lost its exact physical removal receipts"
        );
        self.lock()?
            .runtime_db
            .close_qualification_local_start(expected)
    }

    /// Attach the one Q local Worker while Lillux still holds it before
    /// release. This does not grant an ordinary dedicated session or new
    /// provider contact.
    pub(crate) fn attach_qualification_worker_process(
        &self,
        placement: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
        daemon_generation_id: &str,
        configuration: &crate::external_connector::PreparedProviderConfigurationObservation,
    ) -> Result<runtime_db::external_execution::QualificationWorkerProcessRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?.runtime_db.attach_qualification_worker_process(
            placement,
            process_identity,
            daemon_generation_id,
            configuration,
        )
    }

    /// Proof is captured outside the store lock, then the writer transaction
    /// rejoins it to the immutable attached identity. A failed proof leaves
    /// the row attached for process recovery.
    pub(crate) fn settle_qualification_worker_process(
        &self,
        placement: &str,
    ) -> Result<runtime_db::external_execution::QualificationWorkerProcessRecord> {
        let record = self
            .qualification_worker_process(placement)?
            .context("qualification Worker process is not attached")?;
        let proof = runtime_db::external_execution::QualificationWorkerDeathProof::capture(record)?;
        let _permit = self.acquire_write_permit()?;
        let settled = self
            .lock()?
            .runtime_db
            .settle_qualification_worker_process(proof)?;
        // A cgroup scope is a kernel resource that outlives its settled
        // processes; remove it now that the terminal row names it empty.
        // A PID-namespace containment has no resource to retire.
        if let Some(scope) = settled
            .process_identity
            .process_scope
            .as_ref()
            .filter(|scope| !scope.is_pid_namespace_init())
        {
            scope
                .retire_after_settlement()
                .map_err(anyhow::Error::msg)
                .context("retire the settled Q Worker's cgroup scope")?;
        }
        Ok(settled)
    }

    /// Read one exact Q framed request and observed turn. This projection is
    /// evidence only; it does not authorize a qualification claim.
    pub(crate) fn qualification_worker_command(
        &self,
        placement: &str,
    ) -> Result<Option<runtime_db::external_execution::QualificationWorkerCommandRecord>> {
        self.lock()?
            .runtime_db
            .qualification_worker_command(placement)
    }

    pub(crate) fn reserve_qualification_worker_command(
        &self,
        placement: &str,
        setup_request_digest: &str,
        request_digest: &str,
    ) -> Result<runtime_db::external_execution::QualificationWorkerCommandRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .reserve_qualification_worker_command(placement, setup_request_digest, request_digest)
    }

    /// This is the one-use pre-send boundary. A caller may send one framed
    /// Request only after this transaction succeeds, and may never resend it.
    pub(crate) fn mark_qualification_worker_setup_possible_contact(
        &self,
        state: &crate::state::AppState,
        placement: &str,
        setup_request_digest: &str,
        admission: crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        admission.with_live_callback(|| {
            let g = self.lock()?;
            let proof = self.verify_external_qualification_owner_for_placement(
                state, &g, placement, &admission,
            )?;
            g.runtime_db
                .mark_qualification_worker_setup_possible_contact(
                    placement,
                    setup_request_digest,
                    &proof,
                )
        })
    }

    pub(crate) fn mark_qualification_worker_command_possible_contact(
        &self,
        state: &crate::state::AppState,
        placement: &str,
        request_digest: &str,
        admission: crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        admission.with_live_callback(|| {
            let g = self.lock()?;
            let proof = self.verify_external_qualification_owner_for_placement(
                state, &g, placement, &admission,
            )?;
            g.runtime_db
                .mark_qualification_worker_command_possible_contact(
                    placement,
                    request_digest,
                    &proof,
                )
        })
    }

    /// Persist an exact Q turn transition before acknowledging a pushed batch.
    pub(crate) fn record_qualification_worker_turn_observation(
        &self,
        placement: &str,
        request_digest: &str,
        observation: &serde_json::Value,
    ) -> Result<runtime_db::external_execution::QualificationWorkerCommandRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .record_qualification_worker_turn_observation(placement, request_digest, observation)
    }

    pub(crate) fn record_qualification_worker_progress_delta(
        &self,
        placement: &str,
        request_digest: &str,
        delta: &serde_json::Value,
        max_frame_bytes: u32,
    ) -> Result<runtime_db::external_execution::QualificationWorkerCommandRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .record_qualification_worker_progress_delta(
                placement,
                request_digest,
                delta,
                max_frame_bytes,
            )
    }

    pub(crate) fn record_qualification_worker_setup_final(
        &self,
        placement: &str,
        setup_request_digest: &str,
        response: &serde_json::Value,
        max_frame_bytes: u32,
    ) -> Result<runtime_db::external_execution::QualificationWorkerCommandRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .record_qualification_worker_setup_final(
                placement,
                setup_request_digest,
                response,
                max_frame_bytes,
            )
    }

    pub(crate) fn mark_qualification_worker_setup_send_settled_success(
        &self,
        placement: &str,
        setup_request_digest: &str,
    ) -> Result<runtime_db::external_execution::QualificationWorkerCommandRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .mark_qualification_worker_setup_send_settled_success(placement, setup_request_digest)
    }

    pub(crate) fn record_qualification_worker_final(
        &self,
        placement: &str,
        request_digest: &str,
        response: &serde_json::Value,
        max_frame_bytes: u32,
    ) -> Result<runtime_db::external_execution::QualificationWorkerCommandRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?.runtime_db.record_qualification_worker_final(
            placement,
            request_digest,
            response,
            max_frame_bytes,
        )
    }

    pub(crate) fn mark_qualification_worker_send_settled_success(
        &self,
        placement: &str,
        request_digest: &str,
    ) -> Result<runtime_db::external_execution::QualificationWorkerCommandRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .mark_qualification_worker_send_settled_success(placement, request_digest)
    }

    pub(crate) fn mark_qualification_worker_command_outcome_unknown(
        &self,
        placement: &str,
    ) -> Result<runtime_db::external_execution::QualificationWorkerCommandRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .mark_qualification_worker_command_outcome_unknown(placement)
    }

    /// Retain one complete pushed Q Worker batch before returning its
    /// transport ACK. This local receipt is not a portable signed claim.
    pub(crate) fn record_qualification_worker_observation_batch(
        &self,
        placement: &str,
        request_digest: &str,
        batch: &serde_json::Value,
        signed_wire_max_bytes: u32,
    ) -> Result<runtime_db::external_execution::QualificationWorkerBatchAck> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .record_qualification_worker_observation_batch(
                placement,
                request_digest,
                batch,
                signed_wire_max_bytes,
            )
    }

    /// Freeze the exact death-settled Q Worker command and pushed batches as
    /// CAS objects, then bind the manifest as an immutable operator root.
    /// The returned row is structural evidence, never a qualification permit.
    pub(crate) fn retain_qualification_worker_transcript(
        &self,
        placement: &str,
    ) -> Result<runtime_db::external_execution::QualificationWorkerTranscriptRecord> {
        let permit = self.acquire_write_permit()?;
        let source = self
            .lock()?
            .runtime_db
            .qualification_worker_transcript_source(placement)?;
        // Verify the SAME original receiver stage and exact native capture CAS
        // under this publication guard before creating any transcript roots.
        self.verify_retained_external_q_fixture_custody_admitted(permit.cas_guard(), placement)?;
        let prepared =
            crate::external_qualification_transcript::prepare_qualification_worker_transcript(
                &source.owner_principal,
                &source.command,
                &source.batches,
                &source.provider_configuration,
                source.controller_canary.as_ref(),
                source.fixture_capture.as_ref(),
                source.fixture_content.as_ref(),
            )?;
        let authority = self.pinned_state_authority()?;
        authority.ensure_guard(permit.cas_guard())?;
        let cas = authority.cas_store()?;
        let mut stage = authority
            .require_recovery()?
            .begin_staged_cas_roots_admitted(
                permit.cas_guard(),
                "qualification-worker-transcript",
            )?;
        for (chunk, declared) in prepared.chunks.iter().zip(&prepared.manifest.chunks) {
            let hash = stage.store_blob_admitted(permit.cas_guard(), &cas, chunk)?;
            anyhow::ensure!(
                hash == declared.blob_hash,
                "Q transcript CAS chunk differs from its verified manifest"
            );
        }
        let manifest_hash = stage.store_object_admitted(
            permit.cas_guard(),
            &cas,
            &prepared.manifest.to_value()?,
        )?;
        anyhow::ensure!(
            manifest_hash == prepared.manifest_hash,
            "Q transcript CAS manifest differs from its canonical source"
        );
        stage.protect_object_hash_admitted(permit.cas_guard(), &manifest_hash)?;
        let record = self
            .lock()?
            .runtime_db
            .bind_qualification_worker_transcript(&source, &prepared.manifest, &manifest_hash)?;
        if let Err(error) = stage.finish_admitted(permit.cas_guard()) {
            tracing::warn!(%error, placement, "Q transcript retained while temporary CAS roots remain recoverable");
        }
        Ok(record)
    }

    /// Apply the signed native scope row as inert evidence. This does not
    /// settle the provider occurrence or publish a qualification claim.
    pub(crate) fn apply_external_qualification_scope_settlement(
        &self,
        placement: &str,
    ) -> Result<Option<runtime_db::external_execution::RetainedExternalQualificationScopeSettlement>>
    {
        use ryeos_state::external_execution::ExecutionChannelPayload;
        use ryeos_state::recovery::DurableCasPublicationKey;

        let permit = self.acquire_write_permit()?;
        let Some(plan) = self
            .lock()?
            .runtime_db
            .prepare_external_qualification_scope_settlement(placement)?
        else {
            return Ok(None);
        };
        let ExecutionChannelPayload::QualificationScopeSettled { observation } =
            &plan.evidence.settlement.frame().payload
        else {
            bail!("Q receiver lost its exact signed settlement");
        };
        let verified = match (
            plan.owner.program.fixture(),
            observation.fixture_capture.as_ref(),
            observation.fixture_content.as_ref(),
        ) {
            (None, None, None) => None,
            (Some(fixture), Some(reference), Some(content)) => {
                let bytes = content.verify_for(
                    reference,
                    fixture,
                    &plan.evidence.binding,
                    &observation.observation,
                )?;
                let key = DurableCasPublicationKey::qualification_fixture_occurrence(
                    &plan.evidence.binding.digest()?,
                    plan.evidence.completion.digest(),
                )?;
                let recovery = self.state_authority.require_recovery()?;
                let record = if let Some(record) = &plan.receiver_stage {
                    record.clone()
                } else {
                    // Only empty, expirable staging exists before the DB locator.
                    // Never store or protect roots until the chosen ID is durable.
                    let stage = recovery.begin_durable_cas_upload_admitted(
                        permit.cas_guard(),
                        &plan.owner.operator_principal,
                        "qualification-fixture-receiver",
                        &key,
                        None,
                    )?;
                    let stage_id = stage.staging_id().to_owned();
                    drop(stage);
                    self.lock()?
                        .runtime_db
                        .bind_receiver_stage(&plan, &stage_id)?
                };
                {
                    // Per-stage locking happens outside the StateStore/DB lock.
                    let mut stage = recovery.open_durable_cas_upload_admitted(
                        permit.cas_guard(),
                        &record.stage_id,
                        &plan.owner.operator_principal,
                    )?;
                    stage.ensure_publication_contract(&key, None)?;
                    if stage.admitted_target_hash().is_none() {
                        let cas = self.state_authority.cas_store()?;
                        anyhow::ensure!(
                            stage.store_blob(permit.cas_guard(), &cas, &bytes)?
                                == reference.blob_hash
                                && stage.store_object(
                                    permit.cas_guard(),
                                    &cas,
                                    &content.metadata.to_value()?,
                                )? == reference.metadata_hash,
                            "Q receiver CAS write changed its signed capture"
                        );
                        stage.retain_qualification_fixture(permit.cas_guard(), reference)?;
                    }
                }
                Some(
                    runtime_db::external_execution::verify_qualification_fixture_receiver_custody(
                        &self.state_authority,
                        permit.cas_guard(),
                        &plan,
                        &record,
                    )?,
                )
            }
            _ => bail!("Q receiver fixture presence differs from its admitted program"),
        };
        let retained = self
            .lock()?
            .runtime_db
            .finish_external_qualification_scope_settlement(&plan, verified.as_ref())?;
        Ok(Some(retained))
    }

    /// Cancellation cleanup may proceed without successful capture custody.
    /// Positive evidence publication/readback uses the unconditional retained
    /// check below, including after channel revocation.
    pub(super) fn verify_external_q_fixture_custody_admitted(
        &self,
        guard: &ryeos_state::CasMutationGuard,
        placement: &str,
    ) -> Result<()> {
        self.state_authority.ensure_guard(guard)?;
        {
            let state = self.lock()?;
            if !state
                .runtime_db
                .q_fixture_receiver_custody_required(placement)?
            {
                return Ok(());
            }
        }
        self.verify_retained_external_q_fixture_custody_admitted(guard, placement)
    }

    /// Authenticate the original stage and exact CAS bytes for positive
    /// retained evidence. Revocation cannot replace or waive that custody.
    /// Null/non-Q selections have no fixture; their callers check their exact
    /// historical source separately. No provider contact is authorized here.
    pub(super) fn verify_retained_external_q_fixture_custody_admitted(
        &self,
        guard: &ryeos_state::CasMutationGuard,
        placement: &str,
    ) -> Result<()> {
        self.state_authority.ensure_guard(guard)?;
        let Some(plan) = self
            .lock()?
            .runtime_db
            .retained_q_fixture_receiver_plan(placement)?
        else {
            return Ok(());
        };
        let record = plan
            .receiver_stage
            .as_ref()
            .context("Q fixture receiver lost its original durable stage")?;
        runtime_db::external_execution::verify_qualification_fixture_receiver_custody(
            &self.state_authority,
            guard,
            &plan,
            record,
        )?;
        Ok(())
    }
}
