//! Single current-object contract registry for CAS closure traversal.
//!
//! A registry entry owns both the authoritative current decoder and every
//! outbound edge class. Adding a durable CAS object writer therefore requires
//! one entry rather than coordinated edits to independent kind switches.

use anyhow::Context as _;
use serde_json::Value;

use super::{ExpectedObject, HistoryGraph, ObjectEdge};

pub(super) struct ContractLinks {
    pub(super) object_edges: Vec<ObjectEdge>,
    pub(super) blob_hashes: Vec<String>,
    pub(super) large_object_hashes: Vec<String>,
}

impl ContractLinks {
    pub(super) fn leaf() -> Self {
        Self {
            object_edges: Vec::new(),
            blob_hashes: Vec::new(),
            large_object_hashes: Vec::new(),
        }
    }

    pub(super) fn finish(mut self) -> Self {
        self.object_edges.sort_by(|left, right| {
            (&left.hash, &left.expected, &left.history_graph).cmp(&(
                &right.hash,
                &right.expected,
                &right.history_graph,
            ))
        });
        self.object_edges.dedup_by(|left, right| {
            left.hash == right.hash
                && left.expected == right.expected
                && left.history_graph == right.history_graph
        });
        self.blob_hashes.sort();
        self.blob_hashes.dedup();
        self.large_object_hashes.sort();
        self.large_object_hashes.dedup();
        self
    }
}

struct ObjectContract {
    kind: &'static str,
    validate: fn(&Value) -> anyhow::Result<()>,
    links: fn(&Value) -> Result<ContractLinks, String>,
}

pub(super) const CURRENT_OBJECT_KINDS: &[&str] = &[
    "accounting_allowance_transfer",
    "admitted_execution_realization",
    "admitted_launch_capsule",
    "attestation",
    "bundle_event",
    "chain_state",
    "dispatch_effect_record",
    "execution_identity",
    "external_content_activation",
    "external_content_binding",
    "external_content_manifest",
    "external_large_content_manifest",
    "item_source",
    "observed_execution_realization",
    "persistent_session_capsule",
    "placement_runtime_seed",
    "placement_transfer_manifest",
    "product_build_accepted_result",
    "project_file",
    "project_snapshot",
    "project_snapshot_policy",
    "project_tree",
    "ryeos.effective_source_binding",
    "ryeos.source_closure_manifest",
    "source_manifest",
    "state_manifest",
    "thread_event",
    "thread_snapshot",
    "workspace_output_capture",
];

const CURRENT_OBJECT_CONTRACTS: &[ObjectContract] = &[
    ObjectContract {
        kind: crate::objects::ACCOUNTING_ALLOWANCE_TRANSFER_KIND,
        validate: validate_accounting_allowance_transfer,
        links: links_leaf,
    },
    ObjectContract {
        kind: crate::objects::ADMITTED_EXECUTION_REALIZATION_KIND,
        validate: validate_admitted_execution_realization,
        links: links_admitted_execution_realization,
    },
    ObjectContract {
        kind: "admitted_launch_capsule",
        validate: validate_admitted_launch_capsule,
        links: links_admitted_launch_capsule,
    },
    ObjectContract {
        kind: "attestation",
        validate: validate_attestation,
        links: links_attestation,
    },
    ObjectContract {
        kind: "bundle_event",
        validate: validate_bundle_event,
        links: links_bundle_event,
    },
    ObjectContract {
        kind: "chain_state",
        validate: validate_chain_state,
        links: links_chain_state,
    },
    ObjectContract {
        kind: crate::objects::EFFECT_RECORD_KIND,
        validate: validate_dispatch_effect_record,
        links: links_dispatch_effect_record,
    },
    ObjectContract {
        kind: crate::objects::EXECUTION_IDENTITY_KIND,
        validate: validate_execution_identity,
        links: links_leaf,
    },
    ObjectContract {
        kind: crate::objects::EXTERNAL_CONTENT_ACTIVATION_KIND,
        validate: validate_external_content_activation,
        links: links_external_content_activation,
    },
    ObjectContract {
        kind: crate::objects::EXTERNAL_CONTENT_BINDING_KIND,
        validate: validate_external_content_binding,
        links: links_external_content_binding,
    },
    ObjectContract {
        kind: crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
        validate: validate_external_content_manifest,
        links: links_external_content_manifest,
    },
    ObjectContract {
        kind: crate::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
        validate: validate_external_large_content_manifest,
        links: links_external_large_content_manifest,
    },
    ObjectContract {
        kind: "item_source",
        validate: validate_item_source,
        links: links_item_source,
    },
    ObjectContract {
        kind: crate::objects::OBSERVED_EXECUTION_REALIZATION_KIND,
        validate: validate_observed_execution_realization,
        links: links_observed_execution_realization,
    },
    ObjectContract {
        kind: crate::objects::PERSISTENT_SESSION_CAPSULE_KIND,
        validate: validate_persistent_session_capsule,
        links: links_persistent_session_capsule,
    },
    ObjectContract {
        kind: crate::objects::PLACEMENT_RUNTIME_SEED_KIND,
        validate: validate_placement_runtime_seed,
        links: links_placement_runtime_seed,
    },
    ObjectContract {
        kind: crate::objects::PLACEMENT_TRANSFER_MANIFEST_KIND,
        validate: validate_placement_transfer_manifest,
        links: links_placement_transfer_manifest,
    },
    ObjectContract {
        kind:
            crate::external_content::products::accepted_result::PRODUCT_BUILD_ACCEPTED_RESULT_KIND,
        validate: validate_product_build_accepted_result,
        links: links_product_build_accepted_result,
    },
    ObjectContract {
        kind: "project_file",
        validate: validate_project_file,
        links: links_project_file,
    },
    ObjectContract {
        kind: "project_snapshot",
        validate: validate_project_snapshot,
        links: links_project_snapshot,
    },
    ObjectContract {
        kind: "project_snapshot_policy",
        validate: validate_project_snapshot_policy,
        links: links_leaf,
    },
    ObjectContract {
        kind: "project_tree",
        validate: validate_project_tree,
        links: links_project_tree,
    },
    ObjectContract {
        kind: crate::objects::EFFECTIVE_SOURCE_BINDING_KIND,
        validate: validate_effective_source_binding,
        links: links_effective_source_binding,
    },
    ObjectContract {
        kind: crate::objects::SOURCE_CLOSURE_MANIFEST_KIND,
        validate: validate_source_closure_manifest,
        links: links_source_closure_manifest,
    },
    ObjectContract {
        kind: "source_manifest",
        validate: validate_source_manifest,
        links: links_source_manifest,
    },
    ObjectContract {
        kind: crate::objects::STATE_MANIFEST_KIND,
        validate: validate_state_manifest,
        links: links_state_manifest,
    },
    ObjectContract {
        kind: "thread_event",
        validate: validate_thread_event,
        links: links_thread_event,
    },
    ObjectContract {
        kind: "thread_snapshot",
        validate: validate_thread_snapshot,
        links: links_thread_snapshot,
    },
    ObjectContract {
        kind: crate::objects::WORKSPACE_OUTPUT_CAPTURE_KIND,
        validate: validate_workspace_output_capture,
        links: links_workspace_output_capture,
    },
];

fn contract(kind: &str) -> Option<&'static ObjectContract> {
    CURRENT_OBJECT_CONTRACTS
        .binary_search_by_key(&kind, |contract| contract.kind)
        .ok()
        .map(|index| &CURRENT_OBJECT_CONTRACTS[index])
}

pub(super) fn decode(value: &Value) -> anyhow::Result<Option<ContractLinks>> {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing object kind"))?;
    let Some(contract) = contract(kind) else {
        return super::decode_registered(value);
    };
    (contract.validate)(value).with_context(|| format!("invalid {kind} object"))?;
    let links = (contract.links)(value)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("invalid {kind} object links"))?;
    Ok(Some(links.finish()))
}

/// Project edges for diagnostics and focused contract tests without claiming
/// that the enclosing object is admissible. Closure traversal uses `decode`,
/// which validates and projects atomically through one registry lookup.
pub(super) fn links(value: &Value) -> Result<Option<ContractLinks>, String> {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing object kind".to_string())?;
    let Some(contract) = contract(kind) else {
        return super::links_registered(value);
    };
    (contract.links)(value).map(|links| Some(links.finish()))
}

fn validate_admitted_launch_capsule(value: &Value) -> anyhow::Result<()> {
    crate::objects::AdmittedLaunchCapsule::from_current_value(value.clone()).map(|_| ())
}

fn validate_accounting_allowance_transfer(value: &Value) -> anyhow::Result<()> {
    let transfer: crate::objects::AccountingAllowanceTransfer =
        serde_json::from_value(value.clone())?;
    transfer.validate()
}

fn validate_admitted_execution_realization(value: &Value) -> anyhow::Result<()> {
    crate::objects::AdmittedExecutionRealization::from_current_value(value).map(|_| ())
}

fn validate_attestation(value: &Value) -> anyhow::Result<()> {
    crate::objects::Attestation::from_value(value).map(|_| ())
}

fn validate_bundle_event(value: &Value) -> anyhow::Result<()> {
    let object = serde_json::from_value::<crate::objects::BundleEventObject>(value.clone())?;
    object.validate()
}

fn validate_chain_state(value: &Value) -> anyhow::Result<()> {
    let object = serde_json::from_value::<crate::objects::ChainState>(value.clone())?;
    object.validate()
}

fn validate_execution_identity(value: &Value) -> anyhow::Result<()> {
    crate::objects::ExecutionIdentity::from_current_value(value).map(|_| ())
}

fn validate_external_content_manifest(value: &Value) -> anyhow::Result<()> {
    crate::objects::ExternalContentManifestObject::from_value(value).map(|_| ())
}

fn validate_external_content_activation(value: &Value) -> anyhow::Result<()> {
    crate::objects::ExternalContentActivationReceipt::from_value(value).map(|_| ())
}

fn validate_external_content_binding(value: &Value) -> anyhow::Result<()> {
    crate::objects::ExternalContentBinding::from_value(value).map(|_| ())
}

fn validate_external_large_content_manifest(value: &Value) -> anyhow::Result<()> {
    crate::objects::ExternalLargeContentManifestObject::from_value(value).map(|_| ())
}

fn validate_dispatch_effect_record(value: &Value) -> anyhow::Result<()> {
    crate::objects::DispatchEffectRecord::from_current_value(value).map(|_| ())
}

fn validate_effective_source_binding(value: &Value) -> anyhow::Result<()> {
    crate::objects::EffectiveSourceBinding::from_value(value).map(|_| ())
}

fn validate_source_closure_manifest(value: &Value) -> anyhow::Result<()> {
    crate::objects::SourceClosureManifest::from_value(value).map(|_| ())
}

fn validate_item_source(value: &Value) -> anyhow::Result<()> {
    crate::objects::ItemSource::from_value(value).map(|_| ())
}

fn validate_observed_execution_realization(value: &Value) -> anyhow::Result<()> {
    crate::objects::ObservedExecutionRealization::from_current_value(value).map(|_| ())
}

fn validate_project_file(value: &Value) -> anyhow::Result<()> {
    crate::objects::ProjectFile::from_value(value).map(|_| ())
}

fn validate_product_build_accepted_result(value: &Value) -> anyhow::Result<()> {
    crate::external_content::products::accepted_result::ProductBuildAcceptedResult::from_value(
        value,
    )
    .map(|_| ())
}

fn links_product_build_accepted_result(value: &Value) -> Result<ContractLinks, String> {
    let result =
        crate::external_content::products::accepted_result::ProductBuildAcceptedResult::from_value(
            value,
        )
        .map_err(|error| format!("invalid accepted product build result: {error:#}"))?;
    let mut links = ContractLinks::leaf();
    for product in result.products {
        super::push_typed_hash(
            &product.witness_hash,
            ExpectedObject::Kind("attestation"),
            None,
            &mut links.object_edges,
        )?;
        if let Some(hash) = product.qualification_hash {
            super::push_typed_hash(
                &hash,
                ExpectedObject::Kind("attestation"),
                None,
                &mut links.object_edges,
            )?;
        }
    }
    Ok(links)
}

fn validate_project_snapshot(value: &Value) -> anyhow::Result<()> {
    crate::objects::ProjectSnapshot::from_value(value).map(|_| ())
}

fn validate_project_snapshot_policy(value: &Value) -> anyhow::Result<()> {
    crate::objects::ProjectSnapshotPolicy::from_value(value).map(|_| ())
}

fn validate_project_tree(value: &Value) -> anyhow::Result<()> {
    crate::objects::ProjectTree::from_value(value).map(|_| ())
}

fn validate_persistent_session_capsule(value: &Value) -> anyhow::Result<()> {
    crate::objects::AdmittedPersistentSessionCapsule::from_current_value(value).map(|_| ())
}

fn validate_placement_runtime_seed(value: &Value) -> anyhow::Result<()> {
    crate::objects::PlacementRuntimeSeed::from_current_value(value.clone()).map(|_| ())
}

fn validate_placement_transfer_manifest(value: &Value) -> anyhow::Result<()> {
    crate::objects::PlacementTransferManifest::from_current_value(value.clone()).map(|_| ())
}

fn validate_source_manifest(value: &Value) -> anyhow::Result<()> {
    crate::objects::SourceManifest::from_value(value).map(|_| ())
}

fn validate_state_manifest(value: &Value) -> anyhow::Result<()> {
    crate::objects::StateManifest::from_current_value(value.clone()).map(|_| ())
}

fn validate_thread_event(value: &Value) -> anyhow::Result<()> {
    let object = serde_json::from_value::<crate::objects::ThreadEvent>(value.clone())?;
    object.validate()
}

fn validate_thread_snapshot(value: &Value) -> anyhow::Result<()> {
    crate::objects::ThreadSnapshot::from_current_value(value.clone()).map(|_| ())
}

fn validate_workspace_output_capture(value: &Value) -> anyhow::Result<()> {
    crate::objects::WorkspaceOutputCapture::from_value(value).map(|_| ())
}

fn links_leaf(_value: &Value) -> Result<ContractLinks, String> {
    Ok(ContractLinks::leaf())
}

fn links_effective_source_binding(value: &Value) -> Result<ContractLinks, String> {
    let mut links = ContractLinks::leaf();
    super::push_required_object_edge(
        value,
        "content_manifest_hash",
        ExpectedObject::Kind(crate::objects::SOURCE_CLOSURE_MANIFEST_KIND),
        None,
        &mut links.object_edges,
    )?;
    Ok(links)
}

fn links_source_closure_manifest(value: &Value) -> Result<ContractLinks, String> {
    let manifest = crate::objects::SourceClosureManifest::from_value(value)
        .map_err(|error| error.to_string())?;
    let mut links = ContractLinks::leaf();
    links.blob_hashes = manifest.blob_hashes();
    Ok(links)
}

fn links_dispatch_effect_record(value: &Value) -> Result<ContractLinks, String> {
    let mut links = ContractLinks::leaf();
    let answer: ryeos_effect_contract::DispatchEffectAnswer = serde_json::from_value(
        value
            .get("answer")
            .cloned()
            .ok_or_else(|| "dispatch_effect_record missing answer".to_owned())?,
    )
    .map_err(|error| format!("invalid dispatch effect answer: {error}"))?;
    answer.validate().map_err(|error| error.to_string())?;
    if let ryeos_effect_contract::DispatchEffectAnswer::Retained {
        retained_result, ..
    } = answer
    {
        match retained_result {
            ryeos_effect_contract::RetainedEffectResult::ProductBuildAcceptedResult {
                object_hash,
            } => {
                super::push_typed_hash(
                    &object_hash,
                    ExpectedObject::Kind(crate::external_content::products::accepted_result::PRODUCT_BUILD_ACCEPTED_RESULT_KIND),
                    None, &mut links.object_edges,
                )?;
            }
        }
    }
    super::push_required_object_edge(
        value,
        "admission_evidence_hash",
        ExpectedObject::Any,
        None,
        &mut links.object_edges,
    )?;
    let observation = value
        .get("first_observation")
        .ok_or_else(|| "dispatch_effect_record missing first_observation".to_string())?;
    push_execution_observation_edges(observation, &mut links)?;
    Ok(links)
}

fn links_admitted_execution_realization(value: &Value) -> Result<ContractLinks, String> {
    let realization = crate::objects::AdmittedExecutionRealization::from_current_value(value)
        .map_err(|error| error.to_string())?;
    let mut links = ContractLinks::leaf();
    super::push_typed_hash(
        &realization.substrate_identity_hash,
        ExpectedObject::Kind(crate::objects::EXECUTION_IDENTITY_KIND),
        None,
        &mut links.object_edges,
    )?;
    super::push_typed_hash(
        &realization.substrate_attestation_hash,
        ExpectedObject::Kind("attestation"),
        None,
        &mut links.object_edges,
    )?;
    push_execution_component_edges(&realization.components, &mut links)?;
    Ok(links)
}

fn links_observed_execution_realization(value: &Value) -> Result<ContractLinks, String> {
    let realization = crate::objects::ObservedExecutionRealization::from_current_value(value)
        .map_err(|error| error.to_string())?;
    let mut links = ContractLinks::leaf();
    super::push_typed_hash(
        &realization.admitted_realization_hash,
        ExpectedObject::Kind(crate::objects::ADMITTED_EXECUTION_REALIZATION_KIND),
        None,
        &mut links.object_edges,
    )?;
    push_execution_component_edges(&realization.components, &mut links)?;
    Ok(links)
}

fn push_execution_component_edges(
    components: &[crate::objects::ExecutionComponentReference],
    links: &mut ContractLinks,
) -> Result<(), String> {
    for component in components {
        match &component.material {
            crate::objects::ExecutionComponentStorage::CasObject {
                hash,
                expected_kind,
            } => {
                let expected = contract(expected_kind)
                    .map(|contract| ExpectedObject::Kind(contract.kind))
                    .ok_or_else(|| {
                        format!(
                            "execution component `{}` expects unsupported object kind `{expected_kind}`",
                            component.role
                        )
                    })?;
                super::push_typed_hash(hash, expected, None, &mut links.object_edges)?;
            }
            crate::objects::ExecutionComponentStorage::CasBlob { hash } => {
                links.blob_hashes.push(hash.clone());
            }
            crate::objects::ExecutionComponentStorage::LargeObject { hash, .. } => {
                links.large_object_hashes.push(hash.clone());
            }
        }
    }
    Ok(())
}

fn push_execution_observation_edges(
    observation: &Value,
    links: &mut ContractLinks,
) -> Result<(), String> {
    super::push_optional_object_edge(
        observation,
        "execution_identity_attestation_hash",
        ExpectedObject::Kind("attestation"),
        None,
        &mut links.object_edges,
    )?;
    super::push_optional_object_edge(
        observation,
        "admitted_execution_realization_hash",
        ExpectedObject::Kind(crate::objects::ADMITTED_EXECUTION_REALIZATION_KIND),
        None,
        &mut links.object_edges,
    )?;
    super::push_optional_object_edge(
        observation,
        "observed_execution_realization_hash",
        ExpectedObject::Kind(crate::objects::OBSERVED_EXECUTION_REALIZATION_KIND),
        None,
        &mut links.object_edges,
    )?;
    Ok(())
}

fn links_attestation(value: &Value) -> Result<ContractLinks, String> {
    let mut links = ContractLinks::leaf();
    super::push_required_object_edge(
        value,
        "subject_hash",
        ExpectedObject::Any,
        None,
        &mut links.object_edges,
    )?;
    let attestation = crate::objects::Attestation::from_value(value)
        .map_err(|error| format!("invalid attestation links: {error}"))?;
    if attestation.claim
        == crate::external_content::products::qualification::PRODUCT_QUALIFICATION_CLAIM
        && attestation.policy
            == crate::external_content::products::qualification::PRODUCT_QUALIFICATION_ATTESTATION_POLICY
    {
        let evidence = crate::external_content::products::qualification::ProductQualificationEvidence::from_attestation(&attestation)
            .map_err(|error| format!("invalid product qualification evidence links: {error}"))?;
        for hash in evidence
            .owning_attestation_hashes()
            .map_err(|error| format!("invalid product qualification proof links: {error}"))?
        {
            super::push_typed_hash(
                &hash,
                ExpectedObject::Kind("attestation"),
                None,
                &mut links.object_edges,
            )?;
        }
        for verifier in evidence.execution_verifiers() {
            super::push_typed_hash(
                &verifier.execution_realization_hash,
                ExpectedObject::Kind(crate::objects::ADMITTED_EXECUTION_REALIZATION_KIND),
                None,
                &mut links.object_edges,
            )?;
        }
    }
    Ok(links)
}

fn links_placement_runtime_seed(value: &Value) -> Result<ContractLinks, String> {
    let seed = crate::objects::PlacementRuntimeSeed::from_current_value(value.clone())
        .map_err(|error| error.to_string())?;
    let mut links = ContractLinks::leaf();
    super::push_typed_hash(
        &seed.target_launch_capsule_hash,
        ExpectedObject::Kind("admitted_launch_capsule"),
        None,
        &mut links.object_edges,
    )?;
    links.blob_hashes.push(seed.launch_metadata_blob_hash);
    Ok(links)
}

fn links_placement_transfer_manifest(value: &Value) -> Result<ContractLinks, String> {
    let manifest = crate::objects::PlacementTransferManifest::from_current_value(value.clone())
        .map_err(|error| error.to_string())?;
    let mut links = ContractLinks::leaf();
    for (hash, expected) in [
        (
            &manifest.source_chain_head_hash,
            ExpectedObject::Kind("chain_state"),
        ),
        (
            &manifest.checkpoint_manifest_hash,
            ExpectedObject::Kind(crate::objects::STATE_MANIFEST_KIND),
        ),
        (
            &manifest.project_candidate_snapshot_hash,
            ExpectedObject::Kind("project_snapshot"),
        ),
        (
            &manifest.source_launch_capsule_hash,
            ExpectedObject::Kind("admitted_launch_capsule"),
        ),
    ] {
        super::push_typed_hash(hash, expected, None, &mut links.object_edges)?;
    }
    Ok(links)
}

fn links_external_content_activation(value: &Value) -> Result<ContractLinks, String> {
    let receipt = crate::objects::ExternalContentActivationReceipt::from_value(value)
        .map_err(|error| error.to_string())?;
    let mut links = ContractLinks::leaf();
    for component in receipt.components {
        super::push_typed_hash(
            &component.binding_hash,
            ExpectedObject::Kind(crate::objects::EXTERNAL_CONTENT_BINDING_KIND),
            None,
            &mut links.object_edges,
        )?;
    }
    Ok(links)
}

fn links_chain_state(value: &Value) -> Result<ContractLinks, String> {
    let mut links = ContractLinks::leaf();
    super::push_optional_object_edge(
        value,
        "prev_chain_state_hash",
        ExpectedObject::Kind("chain_state"),
        Some(HistoryGraph::ChainStatePredecessors),
        &mut links.object_edges,
    )?;
    super::push_optional_object_edge(
        value,
        "last_event_hash",
        ExpectedObject::Kind("thread_event"),
        None,
        &mut links.object_edges,
    )?;
    let threads = value
        .get("threads")
        .and_then(Value::as_object)
        .ok_or_else(|| "chain_state missing threads object".to_string())?;
    for entry in threads.values() {
        super::push_required_object_edge(
            entry,
            "snapshot_hash",
            ExpectedObject::Kind("thread_snapshot"),
            None,
            &mut links.object_edges,
        )?;
        super::push_optional_object_edge(
            entry,
            "last_event_hash",
            ExpectedObject::Kind("thread_event"),
            None,
            &mut links.object_edges,
        )?;
    }
    Ok(links)
}

fn links_thread_snapshot(value: &Value) -> Result<ContractLinks, String> {
    let mut links = ContractLinks::leaf();
    super::push_optional_object_edge(
        value,
        "result_workspace_output_capture_hash",
        ExpectedObject::Kind(crate::objects::WORKSPACE_OUTPUT_CAPTURE_KIND),
        None,
        &mut links.object_edges,
    )?;
    for field in ["base_project_snapshot_hash", "result_project_snapshot_hash"] {
        super::push_optional_object_edge(
            value,
            field,
            ExpectedObject::Kind("project_snapshot"),
            None,
            &mut links.object_edges,
        )?;
    }
    super::push_optional_object_edge(
        value,
        "last_event_hash",
        ExpectedObject::Kind("thread_event"),
        None,
        &mut links.object_edges,
    )?;
    super::push_optional_object_edge(
        value,
        "admitted_launch_capsule_hash",
        ExpectedObject::Kind("admitted_launch_capsule"),
        None,
        &mut links.object_edges,
    )?;
    Ok(links)
}

pub(super) const EXTERNAL_MANIFEST_KINDS: &[&str] = &[
    crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
    crate::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
];

fn links_admitted_launch_capsule(value: &Value) -> Result<ContractLinks, String> {
    let mut links = ContractLinks::leaf();
    super::push_optional_object_edge(
        value,
        "source_binding_hash",
        ExpectedObject::Kind(crate::objects::EFFECTIVE_SOURCE_BINDING_KIND),
        None,
        &mut links.object_edges,
    )?;
    super::push_required_object_edge(
        value,
        "execution_realization_hash",
        ExpectedObject::Kind(crate::objects::ADMITTED_EXECUTION_REALIZATION_KIND),
        None,
        &mut links.object_edges,
    )?;
    let project_authority = value
        .get("project_authority")
        .and_then(Value::as_object)
        .ok_or_else(|| "admitted_launch_capsule missing project_authority object".to_string())?;
    if project_authority.get("kind").and_then(Value::as_str) == Some("pinned_generation") {
        let authority = Value::Object(project_authority.clone());
        for field in ["base_snapshot_hash", "snapshot_hash"] {
            super::push_required_object_edge(
                &authority,
                field,
                ExpectedObject::Kind("project_snapshot"),
                None,
                &mut links.object_edges,
            )?;
        }
        if let Some(workspace_outputs) = authority
            .get("workspace_outputs")
            .and_then(Value::as_object)
        {
            super::push_optional_object_edge(
                &Value::Object(workspace_outputs.clone()),
                "capture_hash",
                ExpectedObject::Kind(crate::objects::WORKSPACE_OUTPUT_CAPTURE_KIND),
                None,
                &mut links.object_edges,
            )?;
        }
    }
    for hash in super::external_realization_manifest_hashes(value)? {
        super::push_typed_hash(
            &hash,
            ExpectedObject::OneOf(EXTERNAL_MANIFEST_KINDS),
            None,
            &mut links.object_edges,
        )?;
    }
    push_retained_product_proof_edges(
        value.pointer("/sealed_invocation/resolution_output"),
        &mut links,
    )?;

    let execution_closure = value
        .get("execution_closure")
        .and_then(Value::as_object)
        .ok_or_else(|| "admitted_launch_capsule missing execution_closure object".to_string())?;
    if execution_closure.get("driver").and_then(Value::as_str) == Some("managed_runtime") {
        super::push_required_hash(
            &Value::Object(execution_closure.clone()),
            "executor_blob_hash",
            &mut links.blob_hashes,
        )?;
    }
    let command = execution_closure.get("command").and_then(Value::as_object);
    if command
        .and_then(|command| command.get("authority"))
        .and_then(Value::as_str)
        == Some("content_addressed")
    {
        super::push_required_hash(
            &Value::Object(command.cloned().expect("checked direct command")),
            "executable_blob_hash",
            &mut links.blob_hashes,
        )?;
    }
    if command
        .and_then(|command| command.get("authority"))
        .and_then(Value::as_str)
        == Some("realization_member")
    {
        super::push_required_object_edge(
            &Value::Object(command.cloned().expect("checked direct command")),
            "realization_manifest_hash",
            ExpectedObject::OneOf(EXTERNAL_MANIFEST_KINDS),
            None,
            &mut links.object_edges,
        )?;
    }
    if let Some(sessions) = execution_closure
        .get("prepared_runtime_launch")
        .and_then(|launch| launch.get("admitted_sessions"))
    {
        let sessions = sessions
            .as_object()
            .ok_or_else(|| "admitted launch sessions must be an object".to_owned())?;
        for hash in sessions.values() {
            let hash = hash
                .as_str()
                .ok_or_else(|| "admitted launch session hash must be a string".to_owned())?;
            super::push_typed_hash(
                hash,
                ExpectedObject::Kind(crate::objects::PERSISTENT_SESSION_CAPSULE_KIND),
                None,
                &mut links.object_edges,
            )?;
        }
    }
    if let Some(dependencies) = execution_closure
        .get("prepared_runtime_launch")
        .and_then(|launch| launch.get("content_dependencies"))
    {
        let dependencies = dependencies
            .as_object()
            .ok_or_else(|| "admitted launch content dependencies must be an object".to_owned())?;
        for dependency in dependencies.values() {
            let resolution = dependency
                .get("resolution")
                .ok_or_else(|| "content dependency is missing its resolution".to_owned())?;
            push_retained_product_proof_edges(Some(resolution), &mut links)?;
            for hash in super::retained_resolution_external_realization_manifest_hashes(resolution)?
            {
                super::push_typed_hash(
                    &hash,
                    ExpectedObject::OneOf(EXTERNAL_MANIFEST_KINDS),
                    None,
                    &mut links.object_edges,
                )?;
            }
        }
    }
    push_evidence_attachment_event_edges(
        execution_closure
            .get("prepared_runtime_launch")
            .and_then(|launch| launch.get("evidence_attachments")),
        &mut links,
    )?;
    Ok(links)
}

fn links_persistent_session_capsule(value: &Value) -> Result<ContractLinks, String> {
    let mut links = ContractLinks::leaf();
    super::push_optional_object_edge(
        value,
        "source_binding_hash",
        ExpectedObject::Kind(crate::objects::EFFECTIVE_SOURCE_BINDING_KIND),
        None,
        &mut links.object_edges,
    )?;
    super::push_required_object_edge(
        value,
        "execution_realization_hash",
        ExpectedObject::Kind(crate::objects::ADMITTED_EXECUTION_REALIZATION_KIND),
        None,
        &mut links.object_edges,
    )?;
    for hash in super::persistent_session_external_realization_manifest_hashes(value)? {
        super::push_typed_hash(
            &hash,
            ExpectedObject::OneOf(EXTERNAL_MANIFEST_KINDS),
            None,
            &mut links.object_edges,
        )?;
    }
    match value.get("retained_product_selections") {
        Some(Value::Null) => {}
        Some(selections) => {
            for hash in super::retained_product_selection_proof_hashes(selections)? {
                super::push_typed_hash(
                    &hash,
                    ExpectedObject::Kind("attestation"),
                    None,
                    &mut links.object_edges,
                )?;
            }
        }
        None => {
            return Err("persistent-session capsule missing retained product selections".into());
        }
    }
    push_evidence_attachment_event_edges(
        value.pointer("/exact_program/evidence_attachments"),
        &mut links,
    )?;
    let execution_closure = value
        .get("execution_closure")
        .and_then(Value::as_object)
        .ok_or_else(|| "persistent_session_capsule missing execution_closure object".to_owned())?;
    let command = execution_closure
        .get("command")
        .and_then(Value::as_object)
        .ok_or_else(|| "persistent_session_capsule missing command object".to_owned())?;
    if command.get("authority").and_then(Value::as_str) == Some("content_addressed") {
        super::push_required_hash(
            &Value::Object(command.clone()),
            "executable_blob_hash",
            &mut links.blob_hashes,
        )?;
    }
    if command.get("authority").and_then(Value::as_str) == Some("realization_member") {
        super::push_required_object_edge(
            &Value::Object(command.clone()),
            "realization_manifest_hash",
            ExpectedObject::OneOf(EXTERNAL_MANIFEST_KINDS),
            None,
            &mut links.object_edges,
        )?;
    }
    Ok(links)
}

fn push_retained_product_proof_edges(
    resolution: Option<&Value>,
    links: &mut ContractLinks,
) -> Result<(), String> {
    let Some(resolution) = resolution else {
        return Ok(());
    };
    for hash in super::retained_resolution_product_proof_hashes(resolution)? {
        super::push_typed_hash(
            &hash,
            ExpectedObject::Kind("attestation"),
            None,
            &mut links.object_edges,
        )?;
    }
    Ok(())
}

fn push_evidence_attachment_event_edges(
    attachments: Option<&Value>,
    links: &mut ContractLinks,
) -> Result<(), String> {
    let Some(attachments) = attachments else {
        return Ok(());
    };
    let attachments = attachments
        .as_array()
        .ok_or_else(|| "evidence_attachments must be an array".to_owned())?;
    for attachment in attachments {
        super::push_required_object_edge(
            attachment,
            "event_hash",
            ExpectedObject::Kind(crate::objects::BUNDLE_EVENT_KIND),
            None,
            &mut links.object_edges,
        )?;
    }
    Ok(())
}

fn links_thread_event(value: &Value) -> Result<ContractLinks, String> {
    let mut links = ContractLinks::leaf();
    super::push_optional_object_edge(
        value,
        "prev_chain_event_hash",
        ExpectedObject::Kind("thread_event"),
        Some(HistoryGraph::ThreadEventChainPredecessors),
        &mut links.object_edges,
    )?;
    super::push_optional_object_edge(
        value,
        "prev_thread_event_hash",
        ExpectedObject::Kind("thread_event"),
        Some(HistoryGraph::ThreadEventThreadPredecessors),
        &mut links.object_edges,
    )?;
    if value.get("event_type").and_then(Value::as_str) == Some("milestone")
        && value.pointer("/payload/kind").and_then(Value::as_str) == Some("state_anchor")
    {
        let anchor = crate::objects::StateAnchorMilestone::from_value(
            value
                .get("payload")
                .cloned()
                .ok_or_else(|| "state_anchor milestone is missing payload".to_string())?,
        )
        .map_err(|error| {
            format!("state_anchor milestone violates the current contract: {error:#}")
        })?;
        let manifest_hash = anchor
            .payload
            .manifest_ref
            .strip_prefix("cas:")
            .ok_or_else(|| "state_anchor manifest_ref must use the cas:<hash> form".to_string())?
            .to_string();
        super::push_typed_hash(
            &manifest_hash,
            ExpectedObject::Kind(crate::objects::STATE_MANIFEST_KIND),
            None,
            &mut links.object_edges,
        )?;
    }
    if value.get("event_type").and_then(Value::as_str) == Some("thread_continued")
        && value.pointer("/payload/remote_adoption").is_some()
    {
        let remote: crate::objects::RemoteContinuationAuthority = serde_json::from_value(
            value
                .pointer("/payload/remote_adoption")
                .cloned()
                .ok_or_else(|| "remote continuation authority disappeared".to_string())?,
        )
        .map_err(|error| format!("invalid remote continuation authority: {error}"))?;
        remote
            .validate()
            .map_err(|error| format!("invalid remote continuation authority: {error}"))?;
        for (hash, expected) in [
            (
                &remote.preflight_attestation_hash,
                ExpectedObject::Kind("attestation"),
            ),
            (
                &remote.checkpoint_manifest_hash,
                ExpectedObject::Kind(crate::objects::STATE_MANIFEST_KIND),
            ),
            (
                &remote.target_placement_attestation_hash,
                ExpectedObject::Kind("attestation"),
            ),
            (
                &remote.chain_writer_grant_hash,
                ExpectedObject::Kind("attestation"),
            ),
            (
                &remote.target_launch_capsule_hash,
                ExpectedObject::Kind("admitted_launch_capsule"),
            ),
            (
                &remote.target_runtime_seed_hash,
                ExpectedObject::Kind(crate::objects::PLACEMENT_RUNTIME_SEED_KIND),
            ),
        ] {
            super::push_typed_hash(hash, expected, None, &mut links.object_edges)?;
        }
        if let Some(hash) = &remote.follow_delivery_reservation_attestation_hash {
            super::push_typed_hash(
                hash,
                ExpectedObject::Kind("attestation"),
                None,
                &mut links.object_edges,
            )?;
        }
        if let Some(hash) = &remote.source_accounting_transfer_hash {
            super::push_typed_hash(
                hash,
                ExpectedObject::Kind(crate::objects::ACCOUNTING_ALLOWANCE_TRANSFER_KIND),
                None,
                &mut links.object_edges,
            )?;
        }
    }
    Ok(links)
}

fn links_bundle_event(value: &Value) -> Result<ContractLinks, String> {
    let mut links = ContractLinks::leaf();
    super::push_optional_object_edge(
        value,
        "prev_chain_event_hash",
        ExpectedObject::Kind("bundle_event"),
        Some(HistoryGraph::BundleEventPredecessors),
        &mut links.object_edges,
    )?;
    let attachments = value
        .get("attachments")
        .map(|value| {
            value
                .as_array()
                .ok_or_else(|| "bundle_event attachments is not an array".to_string())
        })
        .transpose()?
        .cloned()
        .unwrap_or_default();
    for attachment in &attachments {
        super::push_required_hash(attachment, "blob_hash", &mut links.blob_hashes)?;
    }
    Ok(links)
}

fn links_state_manifest(value: &Value) -> Result<ContractLinks, String> {
    let mut links = ContractLinks::leaf();
    let restore = value
        .get("restore")
        .ok_or_else(|| "state_manifest missing restore object".to_string())?;
    super::push_required_hash(restore, "blob_hash", &mut links.blob_hashes)?;
    let objects = value
        .get("objects")
        .and_then(Value::as_array)
        .ok_or_else(|| "state_manifest missing objects array".to_string())?;
    for object in objects {
        super::push_required_hash(object, "blob_hash", &mut links.blob_hashes)?;
    }
    Ok(links)
}

fn links_external_content_manifest(value: &Value) -> Result<ContractLinks, String> {
    let mut links = ContractLinks::leaf();
    let entries = value
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| "external_content_manifest missing entries array".to_string())?;
    for entry in entries {
        super::push_optional_hash(entry, "blob_hash", &mut links.blob_hashes)?;
    }
    Ok(links)
}

fn links_external_content_binding(value: &Value) -> Result<ContractLinks, String> {
    let binding = crate::objects::ExternalContentBinding::from_value(value)
        .map_err(|error| format!("invalid external-content binding: {error:#}"))?;
    let mut links = ContractLinks::leaf();
    if binding.state == crate::objects::ExternalContentBindingState::Active {
        let expected = match binding.manifest_kind.as_str() {
            crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND => {
                ExpectedObject::Kind(crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND)
            }
            crate::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND => {
                ExpectedObject::Kind(crate::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND)
            }
            _ => return Err("external-content binding has unsupported manifest kind".to_owned()),
        };
        links.object_edges.push(ObjectEdge {
            hash: binding.manifest_hash.clone(),
            expected,
            history_graph: None,
        });
        if let Some(source_closure) = binding.consumer.source_closure() {
            links.object_edges.push(ObjectEdge {
                hash: source_closure.binding_hash.clone(),
                expected: ExpectedObject::Kind(crate::objects::EFFECTIVE_SOURCE_BINDING_KIND),
                history_graph: None,
            });
        }
    }
    Ok(links)
}

fn links_external_large_content_manifest(value: &Value) -> Result<ContractLinks, String> {
    let manifest = crate::objects::ExternalLargeContentManifestObject::from_value(value)
        .map_err(|error| format!("invalid external large-content manifest: {error:#}"))?;
    Ok(ContractLinks {
        object_edges: Vec::new(),
        blob_hashes: manifest.referenced_blobs(),
        large_object_hashes: manifest.referenced_large_objects(),
    })
}

fn links_project_snapshot(value: &Value) -> Result<ContractLinks, String> {
    let mut links = ContractLinks::leaf();
    super::push_required_object_edge(
        value,
        "project_tree_hash",
        ExpectedObject::Kind("project_tree"),
        None,
        &mut links.object_edges,
    )?;
    super::push_required_object_edge(
        value,
        "effective_policy_hash",
        ExpectedObject::Kind("project_snapshot_policy"),
        None,
        &mut links.object_edges,
    )?;
    let parents = value
        .get("parent_hashes")
        .and_then(Value::as_array)
        .ok_or_else(|| "project_snapshot missing parent_hashes array".to_string())?;
    for parent in parents {
        let hash = parent
            .as_str()
            .ok_or_else(|| "project_snapshot parent_hashes contains non-string".to_string())?;
        super::push_typed_hash(
            hash,
            ExpectedObject::Kind("project_snapshot"),
            Some(HistoryGraph::ProjectSnapshotParents),
            &mut links.object_edges,
        )?;
    }
    Ok(links)
}

fn links_workspace_output_capture(value: &Value) -> Result<ContractLinks, String> {
    let capture = crate::objects::WorkspaceOutputCapture::from_value(value)
        .map_err(|error| format!("invalid workspace output capture: {error:#}"))?;
    let mut links = ContractLinks::leaf();
    super::push_typed_hash(
        &capture.result_project_snapshot_hash,
        ExpectedObject::Kind("project_snapshot"),
        None,
        &mut links.object_edges,
    )?;
    for (_, state) in capture.outputs {
        let crate::objects::WorkspaceOutputCaptureState::Captured {
            manifest_kind,
            manifest_hash,
        } = state
        else {
            continue;
        };
        let expected = match manifest_kind.as_str() {
            crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND => {
                ExpectedObject::Kind(crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND)
            }
            crate::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND => {
                ExpectedObject::Kind(crate::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND)
            }
            _ => return Err("workspace output capture has unsupported manifest kind".to_owned()),
        };
        super::push_typed_hash(&manifest_hash, expected, None, &mut links.object_edges)?;
    }
    Ok(links)
}

fn links_source_manifest(value: &Value) -> Result<ContractLinks, String> {
    let mut links = ContractLinks::leaf();
    let hashes = value
        .get("item_source_hashes")
        .and_then(Value::as_object)
        .ok_or_else(|| "source_manifest missing item_source_hashes object".to_string())?;
    for (item_ref, hash) in hashes {
        let hash = hash
            .as_str()
            .ok_or_else(|| "source_manifest item_source_hashes contains non-string".to_string())?;
        super::push_typed_hash(
            hash,
            ExpectedObject::ItemSource {
                item_ref: item_ref.clone(),
            },
            None,
            &mut links.object_edges,
        )?;
    }
    Ok(links)
}

fn links_project_tree(value: &Value) -> Result<ContractLinks, String> {
    let mut links = ContractLinks::leaf();
    let hashes = value
        .get("files")
        .and_then(Value::as_object)
        .ok_or_else(|| "project_tree missing files object".to_string())?;
    for hash in hashes.values() {
        let hash = hash
            .as_str()
            .ok_or_else(|| "project_tree files contains non-string".to_string())?;
        super::push_typed_hash(
            hash,
            ExpectedObject::Kind("project_file"),
            None,
            &mut links.object_edges,
        )?;
    }
    Ok(links)
}

fn links_project_file(value: &Value) -> Result<ContractLinks, String> {
    let mut links = ContractLinks::leaf();
    super::push_required_hash(value, "blob_hash", &mut links.blob_hashes)?;
    Ok(links)
}

fn links_item_source(value: &Value) -> Result<ContractLinks, String> {
    let mut links = ContractLinks::leaf();
    super::push_required_hash(value, "content_blob_hash", &mut links.blob_hashes)?;
    Ok(links)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_product_result_owns_only_exact_attestations() {
        use crate::external_content::products::accepted_result::*;
        let result = ProductBuildAcceptedResult {
            schema: PRODUCT_BUILD_ACCEPTED_RESULT_SCHEMA.into(),
            kind: PRODUCT_BUILD_ACCEPTED_RESULT_KIND.into(),
            owner_principal: format!("fp:{}", "1".repeat(64)),
            producer_ref: "graph:example/build".into(),
            producer_project_snapshot_hash: "2".repeat(64),
            producer_effective_definition_digest: "3".repeat(64),
            producer_parameters_digest: "4".repeat(64),
            producer_partition_identity: "5".repeat(64),
            products: vec![ProductBuildAcceptedProduct {
                product_name: "runtime".into(),
                witness_hash: "6".repeat(64),
                qualification_hash: Some("7".repeat(64)),
            }],
        };
        let value = result.to_value().unwrap();
        let links = links_product_build_accepted_result(&value).unwrap();
        assert_eq!(links.object_edges.len(), 2);
        assert_eq!(
            links
                .object_edges
                .iter()
                .map(|edge| edge.hash.as_str())
                .collect::<Vec<_>>(),
            vec![
                result.products[0].witness_hash.as_str(),
                result.products[0].qualification_hash.as_deref().unwrap()
            ]
        );
        assert!(
            links
                .object_edges
                .iter()
                .all(|edge| edge.expected == ExpectedObject::Kind("attestation")
                    && edge.history_graph.is_none())
        );
        assert!(links.blob_hashes.is_empty() && links.large_object_hashes.is_empty());
        let mut malformed = value;
        malformed["products"][0]["witness_hash"] = serde_json::json!("not-a-hash");
        assert!(links_product_build_accepted_result(&malformed).is_err());
    }

    #[test]
    fn inventory_and_registry_are_sorted_and_identical() {
        let registry = CURRENT_OBJECT_CONTRACTS
            .iter()
            .map(|contract| contract.kind)
            .collect::<Vec<_>>();
        assert!(registry.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(registry, CURRENT_OBJECT_KINDS);
    }

    #[test]
    fn dispatch_effect_record_roots_record_level_admission_evidence() {
        let evidence = "a".repeat(64);
        let links = links_dispatch_effect_record(&serde_json::json!({
            "admission_evidence_hash": evidence,
            "answer": {"envelope":"bare", "result":null},
            "first_observation": {}
        }))
        .unwrap();

        assert_eq!(links.object_edges.len(), 1);
        assert_eq!(links.object_edges[0].hash, "a".repeat(64));
    }

    #[test]
    fn dispatch_effect_retained_result_is_a_typed_edge_not_arbitrary_result_json() {
        let retained = "a".repeat(64);
        let arbitrary = "b".repeat(64);
        let admission = "c".repeat(64);
        let mut value = serde_json::json!({
            "admission_evidence_hash": admission,
            "first_observation": {},
            "answer": {"envelope":"retained", "result":{"object_hash":arbitrary},
                "retained_result":{"kind":"product_build_accepted_result", "object_hash":retained}},
        });
        let links = links_dispatch_effect_record(&value).unwrap();
        assert_eq!(links.object_edges.len(), 2);
        assert!(links.object_edges.iter().any(|edge| edge.hash == retained
            && edge.expected == ExpectedObject::Kind("product_build_accepted_result")));
        assert!(!links.object_edges.iter().any(|edge| edge.hash == arbitrary));
        value["answer"] = serde_json::json!({"envelope":"bare", "result":{"retained_result":{"kind":"product_build_accepted_result", "object_hash":retained}}});
        assert_eq!(
            links_dispatch_effect_record(&value)
                .unwrap()
                .object_edges
                .len(),
            1
        );
        value["answer"] = serde_json::json!({"envelope":"retained", "result":null,
            "retained_result":{"kind":"product_build_accepted_result", "object_hash":"invalid"}});
        assert!(links_dispatch_effect_record(&value).is_err());
    }

    #[test]
    fn declarative_project_external_binding_roots_only_its_external_manifest() {
        let consumer = crate::objects::ExternalContentConsumerAuthority::pinned_project(
            "tool:project/build".to_owned(),
            "b".repeat(64),
            "c".repeat(64),
            "d".repeat(64),
            None,
        )
        .unwrap();
        let binding = crate::objects::ExternalContentBinding::active(
            "a".repeat(64),
            crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
            consumer,
            "2".repeat(64),
            "3".repeat(64),
            "4".repeat(64),
        )
        .unwrap();
        let links = links_external_content_binding(&binding.to_value().unwrap()).unwrap();
        assert_eq!(links.object_edges.len(), 1);
        assert_eq!(links.object_edges[0].hash, "a".repeat(64));
        assert_eq!(
            links.object_edges[0].expected,
            ExpectedObject::Kind(crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND)
        );
    }

    #[test]
    fn project_external_binding_roots_its_manifest_and_source_authority() {
        let source_binding = "e".repeat(64);
        let consumer = crate::objects::ExternalContentConsumerAuthority::pinned_project(
            "tool:project/build".to_owned(),
            "b".repeat(64),
            "c".repeat(64),
            "d".repeat(64),
            Some(crate::objects::EffectiveSourceClosureProjection {
                schema: crate::objects::EFFECTIVE_SOURCE_BINDING_SCHEMA,
                binding_hash: source_binding.clone(),
                content_manifest_hash: "f".repeat(64),
                owner_key: "1".repeat(64),
                file_count: 1,
                total_bytes: 1,
            }),
        )
        .unwrap();
        let binding = crate::objects::ExternalContentBinding::active(
            "a".repeat(64),
            crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
            consumer,
            "2".repeat(64),
            "3".repeat(64),
            "4".repeat(64),
        )
        .unwrap();
        let links = links_external_content_binding(&binding.to_value().unwrap()).unwrap();
        assert!(links.object_edges.iter().any(|edge| {
            edge.hash == "a".repeat(64)
                && edge.expected
                    == ExpectedObject::Kind(crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND)
        }));
        assert!(links.object_edges.iter().any(|edge| {
            edge.hash == source_binding
                && edge.expected
                    == ExpectedObject::Kind(crate::objects::EFFECTIVE_SOURCE_BINDING_KIND)
        }));
        assert_eq!(links.object_edges.len(), 2);
    }

    #[test]
    fn workspace_output_capture_roots_only_result_snapshot_and_captured_manifests() {
        use crate::external_content::products::{ProductBounds, ProductStorage};

        let result_snapshot = "a".repeat(64);
        let content_manifest = "b".repeat(64);
        let large_manifest = "c".repeat(64);
        let bounds = || ProductBounds {
            maximum_entries: 8,
            maximum_depth: 4,
            maximum_file_bytes: 1_024,
            maximum_total_bytes: 4_096,
        };
        let mut partition = crate::objects::WorkspaceOutputPartition {
            schema: crate::objects::WORKSPACE_OUTPUT_PARTITION_SCHEMA.to_owned(),
            recipe_binding: "product_recipe".to_owned(),
            recipe_ref: "config:fixtures/products".to_owned(),
            recipe_raw_content_digest: "f".repeat(64),
            declarations_hash: "1".repeat(64),
            project_snapshot_policy_hash: "2".repeat(64),
            roots: [
                ("absent", ProductStorage::Content),
                ("content", ProductStorage::Content),
                ("empty", ProductStorage::LargeContent),
                ("large", ProductStorage::LargeContent),
            ]
            .into_iter()
            .map(|(name, storage)| crate::objects::WorkspaceOutputRoot {
                name: name.to_owned(),
                path: format!("products/{name}"),
                storage,
                declared_bounds: bounds(),
                effective_bounds: bounds(),
            })
            .collect(),
            products: Vec::new(),
            partition_identity: String::new(),
            capture_policy_digest: "3".repeat(64),
        };
        partition.partition_identity = partition.derived_partition_identity().unwrap();
        let capture = crate::objects::WorkspaceOutputCapture {
            schema: crate::objects::WORKSPACE_OUTPUT_CAPTURE_SCHEMA.to_owned(),
            kind: crate::objects::WORKSPACE_OUTPUT_CAPTURE_KIND.to_owned(),
            producer_chain_root_id: "T-root".to_owned(),
            producer_thread_id: "T-producer".to_owned(),
            admitted_launch_capsule_hash: "d".repeat(64),
            base_project_snapshot_hash: "e".repeat(64),
            result_project_snapshot_hash: result_snapshot.clone(),
            partition,
            outputs: std::collections::BTreeMap::from([
                (
                    "absent".to_owned(),
                    crate::objects::WorkspaceOutputCaptureState::Absent,
                ),
                (
                    "content".to_owned(),
                    crate::objects::WorkspaceOutputCaptureState::Captured {
                        manifest_kind: crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
                        manifest_hash: content_manifest.clone(),
                    },
                ),
                (
                    "empty".to_owned(),
                    crate::objects::WorkspaceOutputCaptureState::EmptyDirectory,
                ),
                (
                    "large".to_owned(),
                    crate::objects::WorkspaceOutputCaptureState::Captured {
                        manifest_kind: crate::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
                            .to_owned(),
                        manifest_hash: large_manifest.clone(),
                    },
                ),
            ]),
        };

        let links = links_workspace_output_capture(&capture.to_value().unwrap()).unwrap();
        assert_eq!(links.object_edges.len(), 3);
        assert!(links.object_edges.iter().any(|edge| {
            edge.hash == result_snapshot
                && edge.expected == ExpectedObject::Kind("project_snapshot")
        }));
        assert!(links.object_edges.iter().any(|edge| {
            edge.hash == content_manifest
                && edge.expected
                    == ExpectedObject::Kind(crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND)
        }));
        assert!(links.object_edges.iter().any(|edge| {
            edge.hash == large_manifest
                && edge.expected
                    == ExpectedObject::Kind(crate::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND)
        }));
        for non_owning in [
            &capture.admitted_launch_capsule_hash,
            &capture.base_project_snapshot_hash,
            &capture.partition.recipe_raw_content_digest,
            &capture.partition.declarations_hash,
            &capture.partition.partition_identity,
            &capture.partition.capture_policy_digest,
            &capture.partition.project_snapshot_policy_hash,
        ] {
            assert!(
                links
                    .object_edges
                    .iter()
                    .all(|edge| &edge.hash != non_owning)
            );
        }
    }

    #[test]
    fn terminal_thread_snapshot_owns_the_result_output_capture() {
        let capture_hash = "a".repeat(64);
        let links = links_thread_snapshot(&serde_json::json!({
            "base_project_snapshot_hash": null,
            "result_project_snapshot_hash": null,
            "result_workspace_output_capture_hash": capture_hash,
            "last_event_hash": null,
            "admitted_launch_capsule_hash": null,
        }))
        .unwrap();
        assert_eq!(links.object_edges.len(), 1);
        assert_eq!(links.object_edges[0].hash, capture_hash);
        assert_eq!(
            links.object_edges[0].expected,
            ExpectedObject::Kind(crate::objects::WORKSPACE_OUTPUT_CAPTURE_KIND)
        );
    }

    #[test]
    fn placement_transfer_roots_its_project_candidate() {
        let candidate = "7".repeat(64);
        let source_capsule = "5".repeat(64);
        let manifest = crate::objects::PlacementTransferManifest::new(
            "1".repeat(64),
            "owner".into(),
            "T-root".into(),
            "site:a".into(),
            "site:a".into(),
            "site:b".into(),
            "T-source".into(),
            "T-target".into(),
            "2".repeat(64),
            "3".repeat(64),
            "4".repeat(64),
            candidate.clone(),
            source_capsule.clone(),
        )
        .unwrap();

        let links = links_placement_transfer_manifest(&manifest.to_value().unwrap()).unwrap();
        assert!(links.object_edges.iter().any(|edge| {
            edge.hash == candidate && edge.expected == ExpectedObject::Kind("project_snapshot")
        }));
        assert!(links.object_edges.iter().any(|edge| {
            edge.hash == source_capsule
                && edge.expected == ExpectedObject::Kind("admitted_launch_capsule")
        }));
        assert_eq!(links.object_edges.len(), 4);
        assert!(links.blob_hashes.is_empty());
    }

    #[test]
    fn activation_receipt_roots_every_component_binding() {
        let receipt = crate::objects::ExternalContentActivationReceipt::new(
            "config:fixture/activation".to_owned(),
            "a".repeat(64),
            "worker:fixture/hosted".to_owned(),
            "b".repeat(64),
            "c".repeat(64),
            "d".repeat(64),
            vec![crate::objects::ExternalContentActivationComponentReceipt {
                id: "runtime".to_owned(),
                binding_hash: "3".repeat(64),
            }],
            "4".repeat(64),
        )
        .unwrap();
        let links = links_external_content_activation(&receipt.to_value().unwrap()).unwrap();
        assert_eq!(links.object_edges.len(), 1);
        assert_eq!(links.object_edges[0].hash, "3".repeat(64));
    }

    #[test]
    fn evidence_attachment_binding_roots_its_exact_bundle_event() {
        let event_hash = "9".repeat(64);
        let mut links = ContractLinks::leaf();
        push_evidence_attachment_event_edges(
            Some(&serde_json::json!([{
                "binding_id": "observations",
                "event_hash": event_hash,
            }])),
            &mut links,
        )
        .unwrap();

        assert_eq!(links.object_edges.len(), 1);
        assert_eq!(links.object_edges[0].hash, "9".repeat(64));
        assert_eq!(
            links.object_edges[0].expected,
            ExpectedObject::Kind(crate::objects::BUNDLE_EVENT_KIND)
        );
    }
}
