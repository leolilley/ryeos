//! Descriptor custody for one qualification-only external guest.
//!
//! This reproduces the accepted root's immutable project and product inputs.
//! It grants no allocation, provider contact, channel or qualification claim.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use ryeos_app::external_qualification_invocation::QualificationInvocation;
use ryeos_app::operator_external_content::product_qualification::PreparedExternalQualificationRoot;
use ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority;
use ryeos_external_execution_contract::{
    EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA, ExternalGuestInputProjection, GuestBaseSnapshotInput,
    GuestMountAccess, GuestMountContentAuthority, GuestMountInput, GuestMountKind, GuestMountRole,
};
use ryeos_state::external_content::qualification_purpose::QualificationLaunchPurpose;
use ryeos_state::external_execution::admission::ExternalCandidateGuestEnvironment;
use ryeos_state::external_execution::qualification_session::AdmittedExternalQualificationSessionProgram;
use ryeos_state::objects::{
    AdmittedLaunchCapsule, EnvironmentAuthority, ExecutionProjectAuthority,
    ExternalContentRealizationSet, PinnedProjectRealization, SessionProcessEnvironmentValue,
    canonical_value_digest, runtime_view_mount_destination,
};

use super::external_guest_inputs::{
    PreparedProductInputs, RuntimeDestinationOverride, prepare_retained_product_inputs,
};

/// The authenticated accepted root and its complete one-use guest input
/// descriptors remain together until the app placement owner consumes them.
/// This value has no reservation or provider-contact authority.
pub struct PreparedExternalQualificationGuest {
    root: PreparedExternalQualificationRoot,
    inputs: ExternalGuestInputAuthority,
}

impl PreparedExternalQualificationGuest {
    pub fn into_parts(
        self,
    ) -> (
        PreparedExternalQualificationRoot,
        ExternalGuestInputAuthority,
    ) {
        (self.root, self.inputs)
    }
}

/// Transfer the authenticated root and its original descriptor lifelines to
/// the app's one-use qualification allocation owner. Reservation repeats the
/// accepted-root and signed-placement checks before writing durable state.
pub fn reserve_external_qualification_guest<'a>(
    state: &'a ryeos_app::state::AppState,
    context: &ryeos_app::handler_context::HandlerContext,
    coordinate: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
    qualification_operation_id: &str,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    prepared: PreparedExternalQualificationGuest,
) -> Result<ryeos_app::external_placement::ExternalQualificationStart<'a>> {
    reserve_external_qualification_guest_for_invocation(
        state,
        &QualificationInvocation::operator(context),
        coordinate,
        qualification_operation_id,
        limits,
        prepared,
    )
}

/// Preserve the actual invoking lane through the app's owned startup lifeline.
/// The app reauthenticates this identity before reservation and first contact.
pub fn reserve_external_qualification_guest_for_invocation<'a>(
    state: &'a ryeos_app::state::AppState,
    invocation: &QualificationInvocation,
    coordinate: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
    qualification_operation_id: &str,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    prepared: PreparedExternalQualificationGuest,
) -> Result<ryeos_app::external_placement::ExternalQualificationStart<'a>> {
    let (root, inputs) = prepared.into_parts();
    ryeos_app::external_placement::prepare_external_qualification_start_for_invocation(
        state,
        invocation,
        coordinate,
        qualification_operation_id,
        limits,
        root,
        inputs,
    )
}

pub fn prepare_external_qualification_guest(
    state: &ryeos_app::state::AppState,
    context: &ryeos_app::handler_context::HandlerContext,
    coordinate: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
    qualification_operation_id: &str,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    private_parent: &lillux::PinnedDirectory,
) -> Result<PreparedExternalQualificationGuest> {
    prepare_external_qualification_guest_for_invocation(
        state,
        &QualificationInvocation::operator(context),
        coordinate,
        qualification_operation_id,
        limits,
        private_parent,
    )
}

/// Authenticate retained Q inputs using the real operator or callback lane.
/// Invocation identity selects authentication, never replacement input bytes.
pub fn prepare_external_qualification_guest_for_invocation(
    state: &ryeos_app::state::AppState,
    invocation: &QualificationInvocation,
    coordinate: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
    qualification_operation_id: &str,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    private_parent: &lillux::PinnedDirectory,
) -> Result<PreparedExternalQualificationGuest> {
    let root = ryeos_app::operator_external_content::product_qualification::prepare_external_qualification_root_for_invocation(
        state,
        invocation,
        coordinate,
        qualification_operation_id,
        limits,
    )?;
    ryeos_app::external_placement::preflight_external_qualification_guest(state, &root)?;
    let inputs = prepare_external_qualification_inputs(
        state,
        root.capsule(),
        root.purpose(),
        root.program(),
        private_parent,
    )?;
    Ok(PreparedExternalQualificationGuest { root, inputs })
}

/// Caller authenticates the accepted root, sealed purpose, current occurrence,
/// profile and product witness before supplying them. This function repeats
/// their immutable data agreement, then retains every descriptor and cache
/// lease until the eventual one-shot activation consumes the returned owner.
/// `private_parent` is a fresh one-use directory; an existing scratch tree is
/// ambiguous and must not be adopted or overwritten after process loss.
pub fn prepare_external_qualification_inputs(
    state: &ryeos_app::state::AppState,
    capsule: &AdmittedLaunchCapsule,
    purpose: &QualificationLaunchPurpose,
    program: &AdmittedExternalQualificationSessionProgram,
    private_parent: &lillux::PinnedDirectory,
) -> Result<ExternalGuestInputAuthority> {
    private_parent.require_owner_private_directory()?;
    private_parent.ensure_path_binding()?;
    ensure!(
        private_parent.entries_no_follow_bounded(1)?.is_empty(),
        "qualification guest input workspace is not fresh"
    );
    program.validate_against_purpose(purpose)?;
    let sealed =
        ryeos_app::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
            capsule,
        )?;
    ensure!(
        sealed.qualification_purpose() == Some(purpose),
        "qualification guest inputs differ from the accepted sealed root"
    );
    let ExecutionProjectAuthority::PinnedGeneration {
        snapshot_hash,
        realization: PinnedProjectRealization::ReadOnly,
        workspace_outputs: None,
        environment: EnvironmentAuthority::None,
        ..
    } = &capsule.project_authority
    else {
        anyhow::bail!("qualification guest requires output-free read-only pinned authority");
    };
    let content = purpose
        .consumer_content
        .as_ref()
        .context("qualification guest has no accepted consumer content")?;
    let realizations = ExternalContentRealizationSet::new(
        content
            .worker_literals
            .iter()
            .chain(content.environment_realizations.iter())
            .chain(std::iter::once(&content.runtime_realization))
            .cloned()
            .collect(),
    )?;
    let PreparedProductInputs {
        mut inputs,
        mut authorities,
        manifest_authorities,
        leases,
        destinations,
    } = prepare_retained_product_inputs(
        state,
        &realizations,
        Path::new("/workspace"),
        Some(RuntimeDestinationOverride {
            realization_id: &program.requirement.runtime_product_declaration_id,
            destination: Path::new(&program.requirement.runtime_recipe.runtime_mount_destination),
        }),
    )?;
    ensure!(
        destinations
            == ExternalCandidateGuestEnvironment::expected_destinations(
                &program.requirement,
                &realizations
            )?,
        "qualification guest product destinations differ from the accepted runtime recipe"
    );
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let base =
        ryeos_project_capture::prepare_project_snapshot_transfer(&authority, &guard, snapshot_hash)
            .context("prepare accepted qualification root snapshot")?;
    authority.ensure_guard(&guard)?;
    drop(guard);
    let base_descriptor = base.descriptor();
    private_parent.require_disjoint_directory_tree(
        &base_descriptor.try_clone_pinned_directory("<qualification-base>".into())?,
    )?;
    for (input, source) in inputs.iter().zip(&authorities) {
        if input.kind == GuestMountKind::Directory {
            private_parent.require_disjoint_directory_tree(
                &source.try_clone_pinned_directory("<qualification-product>".into())?,
            )?;
        }
    }
    let mut lifelines: Vec<Box<dyn Send + Sync>> = leases
        .into_iter()
        .map(|lease| Box::new(lease) as Box<dyn Send + Sync>)
        .collect();
    // Runtime-view directories are fresh, private guest scratch. They cannot
    // alias the accepted base or any retained product and never confer output
    // publication authority.
    if content.process_environment.values().any(|value| {
        matches!(
            value,
            SessionProcessEnvironmentValue::RuntimeViewDirectory { .. }
        )
    }) {
        let scratch = private_parent
            .create_child(OsStr::new("runtime-view"), 0o700)?
            .into_inherited_descriptor_path()?;
        for (name, value) in &content.process_environment {
            let SessionProcessEnvironmentValue::RuntimeViewDirectory { relative_path } = value
            else {
                continue;
            };
            let mut relative = PathBuf::from("scratch");
            if relative_path != "." {
                relative.push(relative_path);
            }
            let source = scratch.open_or_create_private_directory_descendant(&relative)?;
            let destination = runtime_view_mount_destination(name)?
                .to_str()
                .context("qualification scratch destination is not UTF-8")?
                .to_owned();
            let binding_hash = canonical_value_digest(&serde_json::json!({
                "domain": "ryeos.qualification-guest-private-scratch.v1",
                "purpose": program.qualification_purpose_digest,
                "snapshot": snapshot_hash,
                "environment_name": name,
                "relative_path": relative,
            }))?;
            inputs.push(GuestMountInput {
                role: GuestMountRole::PrivateScratch,
                authority_id: format!("scratch-{name}"),
                descriptor: source.inherited_descriptor().map_err(anyhow::Error::msg)?,
                destination,
                kind: GuestMountKind::Directory,
                access: GuestMountAccess::PrivateWritable,
                normalized_mode: None,
                content_authority: GuestMountContentAuthority::PrivateScratch { binding_hash },
                bytes: 0,
            });
            authorities.push(source);
        }
        lifelines.push(Box::new(scratch));
    }
    let logical = ExternalCandidateGuestEnvironment::derive(
        &program.requirement.runtime_recipe,
        &content.process_environment,
        &content.executable_search,
        &destinations,
    )?;
    let projection = ExternalGuestInputProjection {
        workspace_view: ryeos_external_execution_contract::GuestWorkspaceView::FullSnapshot,
        schema: EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
        base_snapshot: GuestBaseSnapshotInput {
            descriptor: base_descriptor
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?,
            snapshot_hash: snapshot_hash.clone(),
            closure_digest: base.closure_digest().to_owned(),
            object_count: base.object_count(),
            blob_count: base.blob_count(),
            total_bytes: base.total_bytes(),
        },
        workspace_outputs: None,
        inputs,
        executable_search: logical.executable_search,
        environment: logical.environment,
    };
    program.validate_guest_inputs(&projection)?;
    private_parent.ensure_path_binding()?;
    lifelines.push(Box::new(private_parent.try_clone()?));
    lifelines.push(Box::new(base));
    ExternalGuestInputAuthority::new(
        projection,
        base_descriptor,
        None,
        authorities,
        manifest_authorities,
        lifelines,
    )
}
