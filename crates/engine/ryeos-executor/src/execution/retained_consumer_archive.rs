//! Private ConsumerRuntime input preparation from an authenticated retained root.
//!
//! This redeems accepted CAS source and realization content into a bounded
//! archive. It neither claims an attempt nor authorizes provider contact.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use ryeos_app::operator_external_content::product_qualification::PreparedRetainedConsumerVerifier;
use ryeos_external_execution::restoration_verifier_delivery::{
    PreparedConsumerArchive, prepare_consumer_delivery_records,
};
use ryeos_state::external_content::qualification_consumer_record::ConsumerInputRecord;
use ryeos_state::external_execution::admission::ExternalCandidateGuestEnvironment;
use ryeos_state::objects::{
    ExternalContentRealizationSet, MAX_SOURCE_BINDING_BYTES, MAX_SOURCE_FILE_BYTES,
    MAX_SOURCE_MANIFEST_BYTES, SourceExecutionPolicyIdentity, SourceLogicalBinding,
    SourceRootIdentity, SourceSpaceIdentity,
};
use ryeos_state::source_verification::VerifiedAdmittedSourceRecords;

use super::external_guest_inputs::{
    PreparedProductInputs, RuntimeDestinationOverride, prepare_retained_product_inputs,
};

/// Private byte custody for a later independently admitted consumer attempt.
/// Keeping the product inputs here retains every cache-generation lease until
/// the archive's private copy and any subsequent owner handoff are complete.
pub(crate) struct PreparedRetainedConsumerArchive {
    record: ConsumerInputRecord,
    archive: PreparedConsumerArchive,
    _products: PreparedProductInputs,
}

impl PreparedRetainedConsumerArchive {
    pub(crate) fn record(&self) -> &ConsumerInputRecord {
        &self.record
    }

    pub(crate) fn archive(&self) -> &PreparedConsumerArchive {
        &self.archive
    }
}

/// The admitted input must come from the daemon's authenticated born-root
/// lookup. This operation accepts no caller-authored purpose, requirement,
/// verifier path or realization set.
pub(crate) fn prepare_retained_consumer_archive(
    state: &ryeos_app::state::AppState,
    admitted: &PreparedRetainedConsumerVerifier,
    record: &ConsumerInputRecord,
    private_parent: &lillux::PinnedDirectory,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<PreparedRetainedConsumerArchive> {
    private_parent.require_owner_private_directory()?;
    let purpose = admitted.purpose();
    purpose.validate_remote_consumer_coordinate(admitted.coordinate(), admitted.selection())?;
    let content = purpose
        .consumer_content
        .as_ref()
        .context("accepted consumer purpose has no retained content")?;
    let qualified_use = content
        .qualification_use
        .as_ref()
        .context("accepted consumer purpose has no admitted use")?;
    let requirement = retained_worker_requirement(state, content)?;
    ensure!(
        requirement.qualification_requirement_digest()? == qualified_use.requirement_digest,
        "retained Worker requirement differs from admitted consumer use"
    );
    record.validate()?;
    ensure!(
        record.purpose == *purpose
            && record.coordinate == *admitted.coordinate()
            && record.selection == *admitted.selection()
            && record.requirement == requirement,
        "consumer record differs from authenticated retained use"
    );
    record.canonical_bytes()?;

    // Accepted consumer content has already excluded overlapping realization
    // identities. The set constructor gives the guest the same sorted order.
    let realizations = ExternalContentRealizationSet::new(
        content
            .worker_literals
            .iter()
            .chain(content.environment_realizations.iter())
            .chain(std::iter::once(&content.runtime_realization))
            .cloned()
            .collect(),
    )?;
    let destinations =
        ExternalCandidateGuestEnvironment::expected_destinations(&requirement, &realizations)?;
    let products = prepare_retained_product_inputs(
        state,
        &realizations,
        Path::new("/workspace"),
        Some(RuntimeDestinationOverride {
            realization_id: &requirement.runtime_product_declaration_id,
            destination: Path::new(&requirement.runtime_recipe.runtime_mount_destination),
        }),
    )?;
    ensure!(
        products.destinations == destinations,
        "retained consumer product destinations differ from admitted recipe"
    );
    let records = prepare_consumer_delivery_records(record, admitted.payload())?;
    let archive = products.prepare_consumer_archive(
        private_parent,
        records,
        &record.selection.archive_budget,
        deadline,
    )?;
    archive.require_consumer_input_record(record)?;
    archive.require_verifier_selection(&record.selection)?;
    Ok(PreparedRetainedConsumerArchive {
        record: record.clone(),
        archive,
        _products: products,
    })
}

/// Recompile the full recipe from the accepted Worker's retained CAS closure.
/// A profile hash alone is insufficient: the resulting requirement must also
/// rejoin the exact admitted use before it enters the input record.
fn retained_worker_requirement(
    state: &ryeos_app::state::AppState,
    content: &ryeos_state::external_content::products::qualification::ProductQualificationConsumerContentIdentity,
) -> Result<ryeos_state::external_execution::admission::ExternalCandidateRequirement> {
    let projection = &content.worker_source;
    projection.validate()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let binding = cas
        .get_object_bounded(&projection.binding_hash, MAX_SOURCE_BINDING_BYTES as u64)?
        .context("retained Worker source binding is absent")?;
    let manifest = cas
        .get_object_bounded(
            &projection.content_manifest_hash,
            MAX_SOURCE_MANIFEST_BYTES as u64,
        )?
        .context("retained Worker source manifest is absent")?;
    let binding_bytes = lillux::canonical_json(&binding)?;
    let manifest_bytes = lillux::canonical_json(&manifest)?;
    let records = VerifiedAdmittedSourceRecords::from_canonical_bytes(
        &projection.binding_hash,
        &projection.content_manifest_hash,
        binding_bytes.as_bytes(),
        manifest_bytes.as_bytes(),
    )?;
    records.validate_projection(projection)?;
    let owner = &records.binding().owner;
    ensure!(
        owner.item_kind == "worker"
            && owner.canonical_ref == content.definitions.worker.canonical_ref
            && owner.root_raw_content_digest == content.definitions.worker.raw_content_digest
            && owner.signer_fingerprint == content.definitions.worker.publisher_fingerprint
            && matches!(&owner.source_space, SourceSpaceIdentity::Bundle)
            && matches!(&owner.source_root, SourceRootIdentity::Bundle { .. })
            && matches!(
                &records.binding().execution_policy,
                SourceExecutionPolicyIdentity::Worker { .. }
            ),
        "retained Worker source differs from accepted signed definition"
    );
    let SourceLogicalBinding::Worker { entry, .. } = &records.binding().logical_binding else {
        anyhow::bail!("retained consumer source has no Worker entry");
    };
    let mut source_files = BTreeMap::new();
    for file in &records.manifest().entries {
        let bytes = cas
            .get_blob_bounded(&file.blob_hash, MAX_SOURCE_FILE_BYTES)?
            .context("retained Worker source blob is absent")?;
        ensure!(
            bytes.len() as u64 == file.size,
            "retained Worker source blob changed length"
        );
        ensure!(
            source_files.insert(file.path.clone(), bytes).is_none(),
            "retained Worker source contains duplicate paths"
        );
    }
    authority.ensure_guard(&guard)?;
    let profile_bytes = source_files
        .get(entry)
        .context("retained Worker profile entry is absent")?;
    let profile = ryeos_engine::structured_session_profile::compile(profile_bytes, &source_files)?;
    profile.validate()?;
    ensure!(
        profile.profile_hash == content.worker_profile_hash,
        "retained Worker profile differs from accepted consumer content"
    );
    profile
        .external_candidate_requirement()?
        .context("retained Worker profile has no external candidate requirement")
}
