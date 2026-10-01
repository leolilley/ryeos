//! Generic byte custody beneath an authenticated retained consumer root.
//! The admitted verifier owns record semantics; this owner chooses the closed
//! archive members and retains exact realization leases through the copy.

use std::{collections::BTreeMap, path::Path};

use anyhow::{Context as _, Result, ensure};
use ryeos_app::operator_external_content::product_qualification::PreparedRetainedConsumerVerifier;
use ryeos_external_execution::restoration_verifier_delivery::{
    ConsumerProductInventory, PreparedConsumerArchive,
};
use ryeos_external_execution_contract::restored_runtime_measurement::{
    CONSUMER_INPUT_RECORD_NAME, CONSUMER_VERIFIER_REMOTE_NAME, MAX_CONSUMER_INPUT_RECORD_BYTES,
    MAX_RESTORATION_VERIFIER_BYTES,
};
use ryeos_external_execution_contract::staging_package::GuestStagingEntry;
use ryeos_state::objects::ExternalContentRealizationSet;

/// No caller-selected filenames, modes, executables or supplementary entries.
/// `prepared` can only be constructed by accepted-root authentication in app.
/// This does not reserve/contact a provider or qualify the resulting archive.
pub fn prepare_retained_consumer_archive(
    state: &ryeos_app::state::AppState,
    prepared: &PreparedRetainedConsumerVerifier,
    record: &[u8],
    parent: &lillux::PinnedDirectory,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<PreparedConsumerArchive> {
    ensure!(
        !record.is_empty() && record.len() <= MAX_CONSUMER_INPUT_RECORD_BYTES,
        "consumer record exceeds its closed transport allowance"
    );
    ensure!(
        !deadline.has_elapsed(),
        "consumer delivery preparation deadline expired"
    );
    prepared
        .purpose()
        .validate_remote_consumer_coordinate(prepared.coordinate(), prepared.selection())?;
    let source = prepared
        .purpose()
        .remote_verifier_sources
        .get(&prepared.coordinate().scenario_id)
        .context("retained consumer verifier source absent")?;
    let observation = prepared.payload().regular_file_observation()?;
    ensure!(
        observation.size() == source.payload_bytes
            && observation.size() > 0
            && observation.size() <= MAX_RESTORATION_VERIFIER_BYTES
            && prepared
                .payload()
                .digest_regular_file_stable_exact(&observation)?
                == prepared.selection().verifier_artifact_hash,
        "consumer delivery executable differs from exact admitted source"
    );
    let content = prepared
        .purpose()
        .consumer_content
        .as_ref()
        .context("consumer delivery has no accepted runtime content")?;
    let realizations = ExternalContentRealizationSet::new(
        content
            .worker_literals
            .iter()
            .chain(content.environment_realizations.iter())
            .chain(std::iter::once(&content.runtime_realization))
            .cloned()
            .collect(),
    )?;
    require_materialization_budget(
        &realizations,
        &prepared.selection().archive_budget,
        record.len() as u64,
        observation.size(),
    )?;
    parent.require_owner_private_directory()?;
    let runtime = super::external_guest_inputs::RuntimeDestinationOverride {
        realization_id: &prepared.requirement().runtime_product_declaration_id,
        destination: Path::new(
            &prepared
                .requirement()
                .runtime_recipe
                .runtime_mount_destination,
        ),
    };
    let inputs = super::external_guest_inputs::prepare_retained_product_inputs(
        state,
        &realizations,
        Path::new("/workspace"),
        Some(runtime),
    )?;
    ensure!(
        !deadline.has_elapsed(),
        "consumer product redemption exceeded delivery deadline"
    );
    for (input, authority) in inputs.inputs.iter().zip(&inputs.authorities) {
        if input.kind == ryeos_external_execution_contract::GuestMountKind::Directory {
            let source =
                authority.try_clone_pinned_directory("<retained-consumer-product>".into())?;
            parent.require_disjoint_directory_tree(&source)?;
        }
    }
    let record_authority =
        lillux::sealed_memfd(c"consumer-verifier-record", record).map_err(anyhow::Error::msg)?;
    let records = ConsumerProductInventory {
        entries: vec![
            GuestStagingEntry::RegularFile {
                path: CONSUMER_INPUT_RECORD_NAME.into(),
                mode: 0o400,
                bytes: record.len() as u64,
                sha256: lillux::sha256_hex(record),
            },
            GuestStagingEntry::RegularFile {
                path: CONSUMER_VERIFIER_REMOTE_NAME.into(),
                mode: 0o500,
                bytes: observation.size(),
                sha256: prepared.selection().verifier_artifact_hash.clone(),
            },
        ],
        descriptors: BTreeMap::from([
            (CONSUMER_INPUT_RECORD_NAME.into(), record_authority),
            (
                CONSUMER_VERIFIER_REMOTE_NAME.into(),
                prepared.payload().clone(),
            ),
        ]),
    };
    let archive = inputs.prepare_consumer_archive(
        parent,
        records,
        &prepared.selection().archive_budget,
        deadline,
    )?;
    archive.require_verifier_selection(prepared.selection())?;
    Ok(archive)
}

/// Refuse impossible budgets before redeeming/copying large CAS products.
/// This is a lower bound; manifest/framing overhead is still charged exactly
/// by the existing archive writer under the same full allowance.
fn require_materialization_budget(
    realizations: &ExternalContentRealizationSet,
    budget: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerArchiveBudget,
    record_bytes: u64,
    verifier_bytes: u64,
) -> Result<()> {
    budget.validate()?;
    let mut bytes = record_bytes
        .checked_add(verifier_bytes)
        .context("consumer record byte total overflow")?;
    let mut entries = 2usize;
    for realization in realizations.iter() {
        bytes = bytes
            .checked_add(realization.total_bytes)
            .context("consumer product byte total overflow")?;
        entries = entries
            .checked_add(realization.entry_count)
            .and_then(|count| count.checked_add(1))
            .context("consumer product entry total overflow")?;
    }
    ensure!(
        bytes <= budget.maximum_regular_bytes && entries <= budget.maximum_entries,
        "consumer products exceed delivery budget before materialization"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerArchiveBudget;
    use ryeos_state::objects::{
        ExternalContentKind, ExternalContentMode, ExternalContentMountRoot,
        ExternalContentRealization,
    };

    #[test]
    fn consumer_delivery_refuses_impossible_products_before_materialization() {
        let realizations = ExternalContentRealizationSet::new(vec![ExternalContentRealization {
            id: "runtime".into(),
            kind: ExternalContentKind::File,
            mode: ExternalContentMode::Pinned,
            manifest_hash: "a".repeat(64),
            entry_count: 1,
            total_bytes: 7,
            mount_root: ExternalContentMountRoot::Project,
            mount: "vendor/runtime".into(),
        }])
        .unwrap();
        let enough = ConsumerArchiveBudget::new(4, 100, 16 * 1024).unwrap();
        require_materialization_budget(&realizations, &enough, 10, 20).unwrap();
        assert!(
            require_materialization_budget(
                &realizations,
                &ConsumerArchiveBudget::new(3, 100, 16 * 1024).unwrap(),
                10,
                20
            )
            .is_err()
        );
        assert!(
            require_materialization_budget(
                &realizations,
                &ConsumerArchiveBudget::new(4, 36, 16 * 1024).unwrap(),
                10,
                20
            )
            .is_err()
        );
        assert!(require_materialization_budget(&realizations, &enough, u64::MAX, 1).is_err());
    }
}
