//! Application composition for durable CAS object contracts owned above state.

use ryeos_state::object_closure::{
    ObjectContractRegistration, RegisteredObjectEdge, RegisteredObjectExpectation,
    RegisteredObjectLinks,
};
use serde_json::Value;
use std::sync::OnceLock;

pub fn install() -> anyhow::Result<()> {
    static INSTALLED: OnceLock<Result<(), String>> = OnceLock::new();
    INSTALLED
        .get_or_init(|| install_once().map_err(|error| error.to_string()))
        .clone()
        .map_err(anyhow::Error::msg)
}

fn install_once() -> anyhow::Result<()> {
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
            ryeos_bundle_publication_contract::BUNDLE_SET_KIND,
            validate_set,
            set_links,
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
fn validate_materialization(value: &Value) -> anyhow::Result<()> {
    ryeos_bundle_publication_contract::PublisherMaterializationResult::from_current_value(value)
        .map(|_| ())
}
fn validate_set(value: &Value) -> anyhow::Result<()> {
    ryeos_bundle_publication_contract::BundleSet::from_current_value(value).map(|_| ())
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

    #[test]
    fn composed_registry_contains_all_application_contracts() {
        install().unwrap();
        let kinds = ryeos_state::object_closure::current_object_kinds();
        for kind in [
            ryeos_provider_contract::PROVIDER_CALL_RECORD_KIND,
            ryeos_provider_contract::LOCAL_WORKER_OBSERVATION_KIND,
            ryeos_bundle_publication_contract::PUBLISHER_MATERIALIZATION_RESULT_KIND,
            ryeos_bundle_publication_contract::BUNDLE_GENERATION_KIND,
            ryeos_bundle_publication_contract::BUNDLE_SET_KIND,
            ryeos_bundle_publication_contract::NODE_BUNDLE_SELECTION_KIND,
            ryeos_bundle_publication_contract::BUNDLE_CATALOG_SNAPSHOT_KIND,
            ryeos_bundle_publication_contract::BUNDLE_CATALOG_PUBLICATION_KIND,
        ] {
            assert!(kinds.contains(&kind), "missing {kind}");
        }
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
