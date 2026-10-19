//! Application composition for durable CAS object contracts owned above state.

use ryeos_state::object_closure::{
    EventPayloadContractRegistration, ObjectContractRegistration, RegisteredObjectEdge,
    RegisteredObjectExpectation, RegisteredObjectLinks,
};
use serde_json::Value;
use std::sync::OnceLock;

/// Outcome of the single process-wide object-contract installation.
static OBJECTS: OnceLock<Result<(), String>> = OnceLock::new();

/// Selectors of the event contracts the single process-wide event
/// installation registered, or the refusal it produced.
static EVENTS: OnceLock<Result<Vec<&'static str>, String>> = OnceLock::new();

/// Ensure the application object contracts are installed. This never touches
/// the event-contract set: that belongs to the composition root and is
/// installed separately, so calling this first cannot foreclose it.
pub fn install() -> anyhow::Result<()> {
    OBJECTS
        .get_or_init(|| install_objects().map_err(|error| error.to_string()))
        .clone()
        .map_err(anyhow::Error::msg)
}

/// Install the application object contracts together with the domain event
/// payload contracts the composition root owns. The event set installs once
/// per process: repeating the same selector set is accepted, a different one
/// is refused rather than merged or replaced.
pub fn install_with_event_contracts(
    events: Vec<EventPayloadContractRegistration>,
) -> anyhow::Result<()> {
    install()?;
    let mut requested: Vec<&'static str> = events.iter().map(|event| event.event_type).collect();
    requested.sort_unstable();
    let installed = EVENTS
        .get_or_init(|| {
            ryeos_state::object_closure::install_event_payload_contracts(events)
                .map(|()| requested.clone())
                .map_err(|error| error.to_string())
        })
        .clone()
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        installed == requested,
        "application event contracts were already installed with a different selector set"
    );
    Ok(())
}

fn install_objects() -> anyhow::Result<()> {
    ryeos_state::object_closure::install_object_contracts(vec![
        registration(
            ryeos_bundle_publication_contract::BUNDLE_CATALOG_PUBLICATION_KIND,
            validate_catalog_publication,
            catalog_publication_links,
        ),
        registration(
            ryeos_bundle_publication_contract::BUNDLE_CATALOG_SNAPSHOT_KIND,
            validate_catalog_snapshot,
            catalog_snapshot_links,
        ),
        registration(
            ryeos_bundle_publication_contract::BUNDLE_GENERATION_KIND,
            validate_generation,
            generation_links,
        ),
        registration(
            ryeos_bundle_publication_contract::BUNDLE_PAYLOAD_OWNERSHIP_KIND,
            validate_payload_ownership,
            no_links,
        ),
        registration(
            ryeos_bundle_publication_contract::BUNDLE_SET_KIND,
            validate_set,
            set_links,
        ),
        registration(
            ryeos_bundle_publication_contract::SUBSTRATE_RELEASE_KIND,
            validate_substrate_release,
            substrate_release_links,
        ),
        registration(
            ryeos_bundle_publication_contract::SUBSTRATE_BUILD_RECEIPT_KIND,
            validate_substrate_build_receipt,
            no_links,
        ),
        registration(
            ryeos_bundle_publication_contract::NODE_BUNDLE_SELECTION_KIND,
            validate_selection,
            selection_links,
        ),
        registration(
            ryeos_provider_contract::PROVIDER_CALL_RECORD_KIND,
            validate_provider_call,
            provider_call_links,
        ),
        registration(
            ryeos_provider_contract::LOCAL_WORKER_OBSERVATION_KIND,
            validate_local_worker_observation,
            local_worker_observation_links,
        ),
        registration(
            ryeos_bundle_publication_contract::PUBLISHER_MATERIALIZATION_RESULT_KIND,
            validate_materialization,
            materialization_links,
        ),
    ])
}

fn registration(
    kind: &'static str,
    validate: fn(&Value) -> anyhow::Result<()>,
    links: fn(&Value) -> Result<RegisteredObjectLinks, String>,
) -> ObjectContractRegistration {
    ObjectContractRegistration {
        kind,
        validate,
        links,
    }
}

fn validate_generation(value: &Value) -> anyhow::Result<()> {
    ryeos_bundle_publication_contract::BundleGeneration::from_current_value(value).map(|_| ())
}
fn validate_payload_ownership(value: &Value) -> anyhow::Result<()> {
    ryeos_bundle_publication_contract::BundlePayloadOwnership::from_current_value(value).map(|_| ())
}
fn validate_materialization(value: &Value) -> anyhow::Result<()> {
    ryeos_bundle_publication_contract::PublisherMaterializationResult::from_current_value(value)
        .map(|_| ())
}
fn validate_set(value: &Value) -> anyhow::Result<()> {
    ryeos_bundle_publication_contract::BundleSet::from_current_value(value).map(|_| ())
}
fn validate_substrate_release(value: &Value) -> anyhow::Result<()> {
    ryeos_bundle_publication_contract::SubstrateRelease::from_current_value(value).map(|_| ())
}
fn validate_substrate_build_receipt(value: &Value) -> anyhow::Result<()> {
    ryeos_bundle_publication_contract::SubstrateBuildReceipt::from_current_value(value).map(|_| ())
}

fn no_links(_value: &Value) -> Result<RegisteredObjectLinks, String> {
    Ok(RegisteredObjectLinks::default())
}
fn validate_selection(value: &Value) -> anyhow::Result<()> {
    ryeos_bundle_publication_contract::NodeBundleSelection::from_current_value(value).map(|_| ())
}
fn validate_catalog_snapshot(value: &Value) -> anyhow::Result<()> {
    ryeos_bundle_publication_contract::BundleCatalogSnapshot::from_current_value(value).map(|_| ())
}
fn validate_catalog_publication(value: &Value) -> anyhow::Result<()> {
    ryeos_bundle_publication_contract::BundleCatalogPublication::from_current_value(value)
        .map(|_| ())
}

fn generation_links(value: &Value) -> Result<RegisteredObjectLinks, String> {
    let object = ryeos_bundle_publication_contract::BundleGeneration::from_current_value(value)
        .map_err(|error| error.to_string())?;
    let mut links = RegisteredObjectLinks::default();
    push_kind(
        &mut links,
        &object.content_manifest_hash,
        ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
    );
    push_kind(&mut links, &object.manifest_item_hash, "item_source");
    push_kind(&mut links, &object.accepted_product_result_hash, ryeos_state::external_content::products::accepted_result::PRODUCT_BUILD_ACCEPTED_RESULT_KIND);
    push_kind(&mut links, &object.selected_product_witness, "attestation");
    push_kind(&mut links, &object.accepted_capture_result_hash, ryeos_state::external_content::products::accepted_result::PRODUCT_BUILD_ACCEPTED_RESULT_KIND);
    push_kind(
        &mut links,
        &object.selected_signed_product_witness,
        "attestation",
    );
    push_kind(
        &mut links,
        &object.publisher_materialization_result_hash,
        ryeos_bundle_publication_contract::PUBLISHER_MATERIALIZATION_RESULT_KIND,
    );
    push_kind(
        &mut links,
        &object.payload_ownership_hash,
        ryeos_bundle_publication_contract::BUNDLE_PAYLOAD_OWNERSHIP_KIND,
    );
    if let Some(hash) = &object.source_snapshot_hash {
        push_kind(&mut links, hash, "project_snapshot");
    }
    for hash in &object.qualification_evidence_hashes {
        push_kind(&mut links, hash, "attestation");
    }
    if let Some(hash) = &object.provenance_hash {
        push_kind(&mut links, hash, "attestation");
    }
    if let Some(hash) = &object.sbom_hash {
        push_kind(&mut links, hash, "attestation");
    }
    Ok(links)
}

fn materialization_links(value: &Value) -> Result<RegisteredObjectLinks, String> {
    let object =
        ryeos_bundle_publication_contract::PublisherMaterializationResult::from_current_value(
            value,
        )
        .map_err(|error| error.to_string())?;
    let mut links = RegisteredObjectLinks::default();
    push_kind(&mut links, &object.accepted_product_result_hash, ryeos_state::external_content::products::accepted_result::PRODUCT_BUILD_ACCEPTED_RESULT_KIND);
    push_kind(&mut links, &object.selected_product_witness, "attestation");
    push_kind(
        &mut links,
        &object.input_content_manifest_hash,
        ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
    );
    push_kind(
        &mut links,
        &object.output_content_manifest_hash,
        ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
    );
    push_kind(&mut links, &object.output_manifest_item_hash, "item_source");
    Ok(links)
}

fn set_links(value: &Value) -> Result<RegisteredObjectLinks, String> {
    let object = ryeos_bundle_publication_contract::BundleSet::from_current_value(value)
        .map_err(|error| error.to_string())?;
    let mut links = RegisteredObjectLinks::default();
    push_kind(
        &mut links,
        &object.substrate_release_attestation_hash,
        "attestation",
    );
    for entry in &object.entries {
        push_kind(
            &mut links,
            &entry.generation_hash,
            ryeos_bundle_publication_contract::BUNDLE_GENERATION_KIND,
        );
        push_kind(&mut links, &entry.publisher_attestation_hash, "attestation");
    }
    Ok(links)
}

fn substrate_release_links(value: &Value) -> Result<RegisteredObjectLinks, String> {
    let object = ryeos_bundle_publication_contract::SubstrateRelease::from_current_value(value)
        .map_err(|error| error.to_string())?;
    let mut links = RegisteredObjectLinks::default();
    push_kind(
        &mut links,
        &object.substrate_build_accepted_result_hash,
        ryeos_state::external_content::products::accepted_result::PRODUCT_BUILD_ACCEPTED_RESULT_KIND,
    );
    push_kind(
        &mut links,
        &object.selected_substrate_product_witness,
        "attestation",
    );
    push_kind(
        &mut links,
        &object.substrate_build_receipt_hash,
        ryeos_bundle_publication_contract::SUBSTRATE_BUILD_RECEIPT_KIND,
    );
    for hash in &object.qualification_evidence_hashes {
        push_kind(&mut links, hash, "attestation");
    }
    push_kind(
        &mut links,
        &object.core_generation_hash,
        ryeos_bundle_publication_contract::BUNDLE_GENERATION_KIND,
    );
    push_kind(
        &mut links,
        &object.core_generation_attestation_hash,
        "attestation",
    );
    Ok(links)
}

fn selection_links(value: &Value) -> Result<RegisteredObjectLinks, String> {
    let object = ryeos_bundle_publication_contract::NodeBundleSelection::from_current_value(value)
        .map_err(|error| error.to_string())?;
    let mut links = RegisteredObjectLinks::default();
    push_kind(
        &mut links,
        &object.bundle_set_hash,
        ryeos_bundle_publication_contract::BUNDLE_SET_KIND,
    );
    if let Some(hash) = &object.curated_set_attestation_hash {
        push_kind(&mut links, hash, "attestation");
    }
    // `expected_active_selection` is a compare-and-swap fence, not retention.
    Ok(links)
}

fn catalog_snapshot_links(value: &Value) -> Result<RegisteredObjectLinks, String> {
    let object =
        ryeos_bundle_publication_contract::BundleCatalogSnapshot::from_current_value(value)
            .map_err(|error| error.to_string())?;
    let mut links = RegisteredObjectLinks::default();
    for channel in &object.bundle_channels {
        push_kind(
            &mut links,
            &channel.generation_attestation_hash,
            "attestation",
        );
    }
    for channel in &object.set_channels {
        push_kind(&mut links, &channel.set_attestation_hash, "attestation");
    }
    Ok(links)
}

fn catalog_publication_links(value: &Value) -> Result<RegisteredObjectLinks, String> {
    let object =
        ryeos_bundle_publication_contract::BundleCatalogPublication::from_current_value(value)
            .map_err(|error| error.to_string())?;
    let mut links = RegisteredObjectLinks::default();
    push_kind(
        &mut links,
        &object.snapshot_hash,
        ryeos_bundle_publication_contract::BUNDLE_CATALOG_SNAPSHOT_KIND,
    );
    // The predecessor fences succession but does not retain unbounded history.
    Ok(links)
}

fn validate_provider_call(value: &Value) -> anyhow::Result<()> {
    ryeos_provider_contract::ProviderCallRecord::from_current_value(value).map(|_| ())
}
fn validate_local_worker_observation(value: &Value) -> anyhow::Result<()> {
    ryeos_provider_contract::LocalWorkerObservation::from_current_value(value).map(|_| ())
}

fn provider_call_links(value: &Value) -> Result<RegisteredObjectLinks, String> {
    let record = ryeos_provider_contract::ProviderCallRecord::from_current_value(value)
        .map_err(|error| error.to_string())?;
    let mut links = RegisteredObjectLinks::default();
    if let ryeos_provider_contract::TransportCoordinate::AdmittedLocalWorker {
        capsule_hash,
        execution_realization_hash,
        ..
    } = &record.coordinate.transport
    {
        push_kind(
            &mut links,
            capsule_hash,
            ryeos_state::objects::PERSISTENT_SESSION_CAPSULE_KIND,
        );
        push_kind(
            &mut links,
            execution_realization_hash,
            ryeos_state::objects::ADMITTED_EXECUTION_REALIZATION_KIND,
        );
    }
    if let Some(hash) = &record.first_observation.execution_identity_attestation_hash {
        push_kind(&mut links, hash, "attestation");
    }
    if let Some(hash) = &record.first_observation.admitted_execution_realization_hash {
        push_kind(
            &mut links,
            hash,
            ryeos_state::objects::ADMITTED_EXECUTION_REALIZATION_KIND,
        );
    }
    if let Some(hash) = &record.first_observation.observed_execution_realization_hash {
        push_kind(
            &mut links,
            hash,
            ryeos_state::objects::OBSERVED_EXECUTION_REALIZATION_KIND,
        );
    }
    Ok(links)
}

fn local_worker_observation_links(value: &Value) -> Result<RegisteredObjectLinks, String> {
    let observation = ryeos_provider_contract::LocalWorkerObservation::from_current_value(value)
        .map_err(|error| error.to_string())?;
    let mut links = RegisteredObjectLinks::default();
    push_kind(
        &mut links,
        &observation.capsule_hash,
        ryeos_state::objects::PERSISTENT_SESSION_CAPSULE_KIND,
    );
    push_kind(
        &mut links,
        &observation.admitted_execution_realization_hash,
        ryeos_state::objects::ADMITTED_EXECUTION_REALIZATION_KIND,
    );
    if let Some(hash) = &observation.observed_execution_realization_hash {
        push_kind(
            &mut links,
            hash,
            ryeos_state::objects::OBSERVED_EXECUTION_REALIZATION_KIND,
        );
    }
    push_kind(
        &mut links,
        &observation.execution_identity_attestation_hash,
        "attestation",
    );
    Ok(links)
}

fn push_kind(links: &mut RegisteredObjectLinks, hash: &str, kind: &'static str) {
    links.object_edges.push(RegisteredObjectEdge {
        hash: hash.to_owned(),
        expected: RegisteredObjectExpectation::Kind(kind),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn hash(byte: char) -> String {
        std::iter::repeat_n(byte, 64).collect()
    }

    fn accept_any_event(_: &Value) -> anyhow::Result<()> {
        Ok(())
    }

    fn no_event_links(_: &Value) -> Result<RegisteredObjectLinks, String> {
        Ok(RegisteredObjectLinks::default())
    }

    fn event_set(selector: &'static str) -> Vec<EventPayloadContractRegistration> {
        vec![EventPayloadContractRegistration {
            event_type: selector,
            validate: accept_any_event,
            links: no_event_links,
        }]
    }

    #[test]
    fn bare_install_leaves_event_set_open_and_a_different_set_is_refused() {
        install().unwrap();
        install_with_event_contracts(event_set("contract.first_consumer.event.v1")).unwrap();
        install().unwrap();
        install_with_event_contracts(event_set("contract.first_consumer.event.v1")).unwrap();
        let refused = install_with_event_contracts(event_set("contract.second_consumer.event.v1"))
            .unwrap_err();
        assert!(refused.to_string().contains("different selector set"));
        let installed = ryeos_state::object_closure::current_event_payload_selectors();
        assert!(installed.contains(&"contract.first_consumer.event.v1"));
        assert!(!installed.contains(&"contract.second_consumer.event.v1"));
    }

    #[test]
    fn composed_registry_contains_all_application_contracts() {
        install().unwrap();
        let kinds = ryeos_state::object_closure::current_object_kinds();
        for kind in [
            ryeos_provider_contract::PROVIDER_CALL_RECORD_KIND,
            ryeos_provider_contract::LOCAL_WORKER_OBSERVATION_KIND,
            ryeos_bundle_publication_contract::PUBLISHER_MATERIALIZATION_RESULT_KIND,
            ryeos_bundle_publication_contract::BUNDLE_GENERATION_KIND,
            ryeos_bundle_publication_contract::BUNDLE_PAYLOAD_OWNERSHIP_KIND,
            ryeos_bundle_publication_contract::BUNDLE_SET_KIND,
            ryeos_bundle_publication_contract::NODE_BUNDLE_SELECTION_KIND,
            ryeos_bundle_publication_contract::BUNDLE_CATALOG_SNAPSHOT_KIND,
            ryeos_bundle_publication_contract::BUNDLE_CATALOG_PUBLICATION_KIND,
        ] {
            assert!(kinds.contains(&kind), "missing {kind}");
        }
    }

    #[test]
    fn generation_closure_carries_the_payload_ownership_object() {
        let value = json!({
            "schema": ryeos_bundle_publication_contract::BUNDLE_GENERATION_SCHEMA,
            "kind": ryeos_bundle_publication_contract::BUNDLE_GENERATION_KIND,
            "bundle_name": "knowledge", "authored_version": "1.2.3",
            "content_manifest_hash": hash('a'), "manifest_item_hash": hash('b'),
            "target": {"kind":"portable"}, "build_profile": "release",
            "substrate_protocol": 1, "bundle_manifest_format": "ryeos.bundle-manifest/v1",
            "accepted_product_result_hash": hash('c'), "selected_product_identity": "bundle",
            "selected_product_witness": hash('d'),
            "publisher_materialization_result_hash": hash('e'),
            "accepted_capture_result_hash": hash('f'),
            "selected_signed_product_identity": "signed-bundle",
            "selected_signed_product_witness": hash('9'),
            "payload_ownership_hash": hash('8'),
            "source_snapshot_hash": null, "qualification_evidence_hashes": [],
            "provenance_hash": null, "sbom_hash": null
        });
        let links = generation_links(&value).unwrap();
        assert!(links.object_edges.iter().any(|edge| edge.hash == hash('8')
            && matches!(
                edge.expected,
                RegisteredObjectExpectation::Kind(
                    ryeos_bundle_publication_contract::BUNDLE_PAYLOAD_OWNERSHIP_KIND
                )
            )));
    }

    #[test]
    fn selection_predecessor_is_not_a_retention_edge() {
        let predecessor = hash('c');
        let value = json!({
            "schema": ryeos_bundle_publication_contract::NODE_BUNDLE_SELECTION_SCHEMA,
            "kind": ryeos_bundle_publication_contract::NODE_BUNDLE_SELECTION_KIND,
            "target_node_or_app_root_identity": "node:test",
            "substrate_image_digest": format!("sha256:{}", hash('d')),
            "substrate_protocol": 1, "bundle_set_hash": hash('a'),
            "curated_set_attestation_hash": hash('b'),
            "bundle_publication_policy_section_digest": hash('e'), "node_policy_generation_digest": hash('f'),
            "expected_active_selection": predecessor.clone(), "migration_decision": "none"
        });
        let links = selection_links(&value).unwrap();
        assert_eq!(links.object_edges.len(), 2);
        assert!(
            links
                .object_edges
                .iter()
                .all(|edge| edge.hash != predecessor)
        );
    }

    #[test]
    fn catalog_predecessor_is_not_a_retention_edge() {
        let predecessor = hash('b');
        let snapshot = hash('a');
        let value = json!({
            "schema": ryeos_bundle_publication_contract::BUNDLE_CATALOG_PUBLICATION_SCHEMA,
            "kind": ryeos_bundle_publication_contract::BUNDLE_CATALOG_PUBLICATION_KIND,
            "catalog_namespace": "official", "snapshot_hash": snapshot.clone(),
            "previous_publication_attestation_hash": predecessor.clone(), "sequence": 1
        });
        let links = catalog_publication_links(&value).unwrap();
        assert_eq!(links.object_edges.len(), 1);
        assert_eq!(links.object_edges[0].hash, snapshot);
        assert!(
            links
                .object_edges
                .iter()
                .all(|edge| edge.hash != predecessor)
        );
    }
}
