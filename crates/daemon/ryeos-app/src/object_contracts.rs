//! Application composition for durable CAS object contracts owned above state.

use ryeos_state::object_closure::{
    EventPayloadContractRegistration, ObjectContractRegistration, RegisteredObjectEdge,
    RegisteredObjectExpectation, RegisteredObjectLinks,
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
    ryeos_state::object_closure::install_object_contracts(
        vec![
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
        ],
        vec![
            EventPayloadContractRegistration {
                event_type: ryeos_ui_presentation_contract::move_freeze_lifecycle::MOVE_FREEZE_CANCELLED_EVENT,
                validate: validate_move_freeze_cancellation,
                links: move_freeze_cancellation_links,
            },
            EventPayloadContractRegistration {
                event_type: ryeos_ui_presentation_contract::transfer::TRANSFER_EVENT,
                validate: validate_presentation_transfer,
                links: presentation_transfer_links,
            },
            EventPayloadContractRegistration {
                event_type: ryeos_ui_presentation_contract::transfer_lifecycle::TRANSFER_LIFECYCLE_EVENT,
                validate: validate_presentation_transfer_lifecycle,
                links: presentation_transfer_lifecycle_links,
            },
            EventPayloadContractRegistration {
                event_type: ryeos_ui_presentation_contract::transfer_lifecycle_v2::TRANSFER_LIFECYCLE_V2_EVENT,
                validate: validate_presentation_transfer_lifecycle_v2,
                links: presentation_transfer_lifecycle_v2_links,
            },
        ],
    )
}

fn validate_presentation_transfer(value: &Value) -> anyhow::Result<()> {
    ryeos_ui_presentation_contract::transfer::from_event_value(value)
        .map(|_| ())
        .map_err(anyhow::Error::msg)
}

fn validate_move_freeze_cancellation(value: &Value) -> anyhow::Result<()> {
    ryeos_ui_presentation_contract::move_freeze_lifecycle::from_event_value(value)
        .map(|_| ())
        .map_err(anyhow::Error::msg)
}

fn move_freeze_cancellation_links(value: &Value) -> Result<RegisteredObjectLinks, String> {
    let record = ryeos_ui_presentation_contract::move_freeze_lifecycle::from_event_value(value)?;
    let mut links = RegisteredObjectLinks::default();
    for evidence in record.evidence_links() {
        push_kind(&mut links, &evidence.event_hash, "thread_event");
    }
    Ok(links)
}

fn presentation_transfer_links(value: &Value) -> Result<RegisteredObjectLinks, String> {
    let event = ryeos_ui_presentation_contract::transfer::from_event_value(value)?;
    let mut links = RegisteredObjectLinks::default();
    for evidence in event.payload.evidence_links() {
        push_kind(&mut links, &evidence.event_hash, "thread_event");
    }
    Ok(links)
}

fn validate_presentation_transfer_lifecycle(value: &Value) -> anyhow::Result<()> {
    ryeos_ui_presentation_contract::transfer_lifecycle::from_event_value(value)
        .map(|_| ())
        .map_err(anyhow::Error::msg)
}

fn presentation_transfer_lifecycle_links(value: &Value) -> Result<RegisteredObjectLinks, String> {
    let event = ryeos_ui_presentation_contract::transfer_lifecycle::from_event_value(value)?;
    let mut links = RegisteredObjectLinks::default();
    for evidence in event.evidence_links() {
        push_kind(&mut links, &evidence.event_hash, "thread_event");
    }
    Ok(links)
}

fn validate_presentation_transfer_lifecycle_v2(value: &Value) -> anyhow::Result<()> {
    ryeos_ui_presentation_contract::transfer_lifecycle_v2::from_event_value(value)
        .map(|_| ())
        .map_err(anyhow::Error::msg)
}

fn presentation_transfer_lifecycle_v2_links(
    value: &Value,
) -> Result<RegisteredObjectLinks, String> {
    let event = ryeos_ui_presentation_contract::transfer_lifecycle_v2::from_event_value(value)?;
    let mut links = RegisteredObjectLinks::default();
    for evidence in event.evidence_links() {
        push_kind(&mut links, &evidence.event_hash, "thread_event");
    }
    Ok(links)
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

    #[test]
    fn freeze_cancellation_links_exact_testimony_and_rejects_other_source_chain() {
        use ryeos_ui_presentation_contract::move_freeze_lifecycle::*;
        use ryeos_ui_presentation_contract::transfer::*;
        let seat = |id: &str, side: &str| TransferSeat {
            presentation_id: id.into(),
            thread_id: format!("thread-{side}"),
            chain_root_id: format!("root-{side}"),
            producer_incarnation: hash('a'),
            revision: 4,
        };
        let record = MoveFreezeCancellation::new(
            TransferIdentity {
                operation_id: "33333333-3333-4333-8333-333333333333".into(),
                source: seat("11111111-1111-4111-8111-111111111111", "source"),
                destination: seat("22222222-2222-4222-8222-222222222222", "destination"),
                export_digest: hash('b'),
            },
            MoveFreezeEvidence {
                preparation: TransferEvidence {
                    event_hash: hash('c'),
                    chain_seq: 7,
                },
                completion: TransferEvidence {
                    event_hash: hash('d'),
                    chain_seq: 11,
                },
            }
            .into(),
            MoveFreezeCancellationWriter::CurrentProducer {
                producer_incarnation: hash('e'),
                next_engine_seq: 100,
            },
        )
        .unwrap();
        let mut event = json!({"event_type":MOVE_FREEZE_CANCELLED_EVENT,"thread_id":"thread-source","chain_root_id":"root-source","chain_seq":15,"payload":record});
        validate_move_freeze_cancellation(&event).unwrap();
        let links = move_freeze_cancellation_links(&event).unwrap();
        assert_eq!(
            links.object_edges,
            vec![
                RegisteredObjectEdge {
                    hash: hash('c'),
                    expected: RegisteredObjectExpectation::Kind("thread_event")
                },
                RegisteredObjectEdge {
                    hash: hash('d'),
                    expected: RegisteredObjectExpectation::Kind("thread_event")
                },
            ]
        );
        event["thread_id"] = json!("thread-destination");
        assert!(validate_move_freeze_cancellation(&event).is_err());
        event["thread_id"] = json!("thread-source");
        let pending = MoveFreezeCancellation::new(
            record.transfer.clone(),
            MoveFreezeCancellationEvidence {
                preparation: record.freeze.preparation.clone(),
                completion: None,
            },
            record.writer.clone(),
        )
        .unwrap();
        let pending_event = json!({"event_type":MOVE_FREEZE_CANCELLED_EVENT,"thread_id":"thread-source","chain_root_id":"root-source","chain_seq":8,"payload":pending});
        validate_move_freeze_cancellation(&pending_event).unwrap();
        assert_eq!(
            move_freeze_cancellation_links(&pending_event)
                .unwrap()
                .object_edges
                .len(),
            1
        );
        event["chain_seq"] = json!(11);
        assert!(move_freeze_cancellation_links(&event).is_err());
    }

    #[test]
    fn destination_transfer_root_reaches_source_testimony_without_source_head() {
        use ryeos_ui_presentation_contract::transfer::*;
        use std::collections::BTreeMap;
        install().unwrap();
        fn seat(id: &str, name: &str) -> TransferSeat {
            TransferSeat {
                presentation_id: id.into(),
                thread_id: format!("T-{name}"),
                chain_root_id: format!("T-{name}"),
                producer_incarnation: "ab".repeat(32),
                revision: 3,
            }
        }
        fn retain(
            objects: &mut BTreeMap<String, Vec<u8>>,
            record: TransferRecord,
            seq: u64,
            previous: Option<String>,
        ) -> TransferEvidence {
            let owner = record.event_owner();
            let event = ryeos_state::objects::thread_event::NewEvent::new(
                &owner.chain_root_id,
                &owner.thread_id,
                TRANSFER_EVENT,
            )
            .chain_seq(seq)
            .thread_seq(seq)
            .prev_chain_event_hash(previous.clone())
            .prev_thread_event_hash(previous)
            .payload(
                serde_json::to_value(TransferEventPayload {
                    seq,
                    operation_id: record.transfer.operation_id.clone(),
                    payload: record,
                })
                .unwrap(),
            )
            .build();
            event.validate().unwrap();
            let bytes = lillux::canonical_json(&event.to_value())
                .unwrap()
                .into_bytes();
            let event_hash = lillux::sha256_hex(&bytes);
            objects.insert(event_hash.clone(), bytes);
            TransferEvidence {
                event_hash,
                chain_seq: seq,
            }
        }
        let export =
            json!({"mode":"copy", "data_only":"retained draft", "unrelated_hash":hash('f')});
        let identity = TransferIdentity {
            operation_id: "33333333-3333-4333-8333-333333333333".into(),
            source: seat("11111111-1111-4111-8111-111111111111", "source"),
            destination: seat("22222222-2222-4222-8222-222222222222", "destination"),
            export_digest: ryeos_ui_presentation_contract::payload_digest::seat_payload_digest(
                &export,
            )
            .unwrap(),
        };
        let reservation = json!({"destination_allocation":"retained exact allocation"});
        let import_digest =
            ryeos_ui_presentation_contract::payload_digest::seat_payload_digest(&reservation)
                .unwrap();
        let mut objects = BTreeMap::new();
        let inbound = retain(
            &mut objects,
            TransferRecord {
                schema: 1,
                transfer: identity.clone(),
                phase: TransferPhase::InboundCommitment,
            },
            0,
            None,
        );
        let preparation = retain(
            &mut objects,
            TransferRecord {
                schema: 1,
                transfer: identity.clone(),
                phase: TransferPhase::Prepared {
                    inbound: inbound.clone(),
                    freeze: None,
                    export,
                },
            },
            0,
            None,
        );
        let acceptance = retain(
            &mut objects,
            TransferRecord {
                schema: 1,
                transfer: identity.clone(),
                phase: TransferPhase::Accepted {
                    preparation: preparation.clone(),
                    reservation,
                    import_digest: import_digest.clone(),
                },
            },
            0,
            None,
        );
        let commit = retain(
            &mut objects,
            TransferRecord {
                schema: 1,
                transfer: identity.clone(),
                phase: TransferPhase::Committed {
                    preparation: preparation.clone(),
                    acceptance: acceptance.clone(),
                    import_digest: import_digest.clone(),
                },
            },
            1,
            Some(preparation.event_hash.clone()),
        );
        let activation = retain(
            &mut objects,
            TransferRecord {
                schema: 1,
                transfer: identity,
                phase: TransferPhase::Activated {
                    commit: commit.clone(),
                    import_digest,
                },
            },
            1,
            Some(acceptance.event_hash.clone()),
        );
        // No source head/ref is a root. This tests the actual closure walker,
        // not GC/release or authorized transfer publication.
        let report = ryeos_state::object_closure::collect_object_closure_from_memory(
            &objects,
            &BTreeMap::new(),
            [activation.event_hash],
            ryeos_state::object_closure::ObjectClosureLimits::default(),
        )
        .unwrap();
        assert!(report.is_complete(), "{report:?}");
        for retained in [&inbound, &preparation, &acceptance, &commit] {
            assert!(report.object_hashes.contains(&retained.event_hash));
        }
        assert!(
            !report.object_hashes.contains(&hash('f')),
            "export data is not an implicit edge"
        );
        assert!(
            !report.object_hashes.contains(&hash('d')),
            "commitments are not CAS links"
        );

        // A domain event cannot silently become an ordinary leaf when its
        // phase-owned payload is malformed, even though the envelope is valid.
        let mut malformed: Value = serde_json::from_slice(&objects[&commit.event_hash]).unwrap();
        malformed["payload"]["payload"]["transfer"]["destination"]["producer_incarnation"] =
            json!("wrong");
        assert!(presentation_transfer_links(&malformed).is_err());
        assert!(ryeos_state::object_closure::object_links(&malformed).is_err());
    }

    #[test]
    fn composed_registry_contains_all_application_contracts() {
        install().unwrap();
        assert!(
            ryeos_state::object_closure::current_event_payload_selectors()
                .contains(&ryeos_ui_presentation_contract::transfer::TRANSFER_EVENT)
        );
        assert!(
            ryeos_state::object_closure::current_event_payload_selectors().contains(
                &ryeos_ui_presentation_contract::transfer_lifecycle::TRANSFER_LIFECYCLE_EVENT
            )
        );
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
    fn lifecycle_settlement_contract_roots_transfer_testimony_and_exact_fence() {
        use ryeos_ui_presentation_contract::transfer::*;
        use ryeos_ui_presentation_contract::transfer_lifecycle::*;
        install().unwrap();
        let seat = |presentation_id: &str, side: &str| TransferSeat {
            presentation_id: presentation_id.into(),
            thread_id: format!("T-{side}"),
            chain_root_id: format!("T-{side}"),
            producer_incarnation: hash('a'),
            revision: 3,
        };
        let payload = TransferLifecycleEventPayload::new(
            TransferRecord {
                schema: 1,
                transfer: TransferIdentity {
                    operation_id: "33333333-3333-4333-8333-333333333333".into(),
                    source: seat("11111111-1111-4111-8111-111111111111", "source"),
                    destination: seat("22222222-2222-4222-8222-222222222222", "destination"),
                    export_digest: hash('b'),
                },
                phase: TransferPhase::SettlementObserved {
                    settlement: TransferEvidence {
                        event_hash: hash('c'),
                        chain_seq: 4,
                    },
                },
            },
            TransferEvidence {
                event_hash: hash('d'),
                chain_seq: 5,
            },
        )
        .unwrap();
        let event = ryeos_state::objects::thread_event::NewEvent::new(
            "T-source",
            "T-source",
            TRANSFER_LIFECYCLE_EVENT,
        )
        .chain_seq(6)
        .thread_seq(6)
        .payload(serde_json::to_value(payload).unwrap())
        .build()
        .to_value();
        let links = presentation_transfer_lifecycle_links(&event).unwrap();
        assert_eq!(links.object_edges.len(), 2);
        for (edge, expected) in links.object_edges.iter().zip([hash('c'), hash('d')]) {
            assert_eq!(edge.hash, expected);
            assert_eq!(
                edge.expected,
                RegisteredObjectExpectation::Kind("thread_event")
            );
        }
        assert!(ryeos_state::object_closure::object_links(&event).is_ok());
        let mut browser_wrapper = event;
        browser_wrapper["payload"]["seq"] = json!(1);
        assert!(ryeos_state::object_closure::object_links(&browser_wrapper).is_err());
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
