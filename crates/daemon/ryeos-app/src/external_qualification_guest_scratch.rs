//! Precontact Q guest-input scratch custody. This prepares an empty original
//! pinned parent for the executor; it does not reserve a provider allocation,
//! contact a backend, or assert qualification success.

use std::ffi::OsStr;
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate;
use ryeos_state::object_closure::ObjectClosureLimits;

use crate::handler_context::HandlerContext;
use crate::qualification_workspace_custody::QualificationGuestScratchCustody;
use crate::state::AppState;
use crate::temp_dir_guard::TempDirGuard;

/// One process-local original descriptor. The registry separately retains
/// this exact custody after the caller drops the value, including on errors.
/// A future execution owner must join it to the durable scratch row before
/// materializing guest inputs or reserving the Q allocation.
pub struct PreparedQualificationGuestScratch {
    placement_thread_id: String,
    custody: Arc<QualificationGuestScratchCustody>,
}

impl PreparedQualificationGuestScratch {
    pub fn placement(&self) -> &str {
        &self.placement_thread_id
    }

    pub fn pinned_root(&self) -> Result<&lillux::PinnedDirectory> {
        self.custody.pinned_root()
    }
}

/// Reserve one durable scratch intent before creating any guest-input child.
/// The accepted CAS root, retained product, current signed Worker profile,
/// signed placement binding and live restored occurrence are rechecked first.
/// A lost response never admits another child or a replacement descriptor.
pub fn prepare_original_qualification_guest_scratch(
    state: &AppState,
    context: &HandlerContext,
    coordinate: &ConsumerRuntimeVerificationCoordinate,
    qualification_operation_id: &str,
    limits: ObjectClosureLimits,
) -> Result<PreparedQualificationGuestScratch> {
    crate::hosted_operation::with_qualification_occurrence_contact(
        qualification_operation_id,
        || {
            prepare_original_qualification_guest_scratch_under_gate(
                state,
                context,
                coordinate,
                qualification_operation_id,
                limits,
            )
        },
    )
}

fn prepare_original_qualification_guest_scratch_under_gate(
    state: &AppState,
    context: &HandlerContext,
    coordinate: &ConsumerRuntimeVerificationCoordinate,
    qualification_operation_id: &str,
    limits: ObjectClosureLimits,
) -> Result<PreparedQualificationGuestScratch> {
    state
        .controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)?;
    let root = crate::operator_external_content::product_qualification::prepare_external_qualification_root(
        state,
        context,
        coordinate,
        qualification_operation_id,
        limits,
    )?;
    crate::external_placement::preflight_external_qualification_guest(state, &root)?;
    let placement = ryeos_state::external_execution::qualification_session::qualification_candidate_placement_key(
        coordinate,
        qualification_operation_id,
    )?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let admission =
        crate::operator_external_content::product_qualification::authenticate_consumer_root(
            state,
            &authority,
            &guard,
            limits,
            context,
            coordinate,
            qualification_operation_id,
        )?;
    ensure!(
        admission.purpose() == root.purpose()
            && admission.accepted_project_authority() == &root.capsule().project_authority,
        "Q scratch preparation changed its accepted root"
    );
    authority.ensure_guard(&guard)?;
    drop(guard);
    let parent = state.state_store.qualification_guest_scratch_parent()?;
    let expected_generation = &root
        .purpose()
        .consumer_content
        .as_ref()
        .context("Q scratch has no accepted Worker content")?
        .definitions
        .bundle_generation_identity;
    let reserved = state.engine.with_checked_bundle_generation(|generation| {
        ensure!(
            generation.request_engine_generation_identity() == expected_generation,
            "Q scratch changed signed Bundle generation before its intent"
        );
        state.state_store.reserve_qualification_guest_scratch(
            state,
            admission,
            root.program(),
            qualification_operation_id,
            &parent,
        )
    })?;
    ensure!(
        reserved.placement_thread_id == placement
            && reserved.scratch_root == parent.path().join(&placement).to_string_lossy(),
        "Q scratch reservation changed its pinned private child"
    );
    let child = parent.create_child(OsStr::new(&placement), 0o700)?;
    let guard = Arc::new(TempDirGuard::new_pinned(
        parent,
        std::ffi::OsString::from(&placement),
        child,
    ));
    guard
        .owned_scratch_root()?
        .require_owner_private_directory()?;
    let custody = Arc::new(QualificationGuestScratchCustody::new(&reserved, guard)?);
    state
        .qualification_workspace_custody
        .retain_guest_scratch(Arc::clone(&custody))?;
    let created = state
        .state_store
        .mark_qualification_guest_scratch_created(state, &reserved, &custody)?;
    ensure!(
        created.placement_thread_id == placement
            && created.phase
                == crate::runtime_db::external_execution::QualificationGuestScratchPhase::Created,
        "Q guest scratch creation lost its exact durable owner"
    );
    Ok(PreparedQualificationGuestScratch {
        placement_thread_id: placement,
        custody,
    })
}
