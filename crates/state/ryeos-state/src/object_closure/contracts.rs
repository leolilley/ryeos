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
    fn leaf() -> Self {
        Self {
            object_edges: Vec::new(),
            blob_hashes: Vec::new(),
            large_object_hashes: Vec::new(),
        }
    }

    fn finish(mut self) -> Self {
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
    "admitted_launch_capsule",
    "attestation",
    "bundle_event",
    "chain_state",
    "execution_identity",
    "external_content_manifest",
    "external_large_content_manifest",
    "graph_node_effect_record",
    "item_source",
    "project_file",
    "project_snapshot",
    "project_snapshot_policy",
    "project_tree",
    "provider_call_effect_record",
    "source_manifest",
    "state_manifest",
    "thread_event",
    "thread_snapshot",
];

const CURRENT_OBJECT_CONTRACTS: &[ObjectContract] = &[
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
        kind: crate::objects::EXECUTION_IDENTITY_KIND,
        validate: validate_execution_identity,
        links: links_leaf,
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
        kind: crate::objects::GRAPH_NODE_EFFECT_RECORD_KIND,
        validate: validate_graph_node_effect_record,
        links: links_leaf,
    },
    ObjectContract {
        kind: "item_source",
        validate: validate_item_source,
        links: links_item_source,
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
        kind: crate::objects::PROVIDER_CALL_EFFECT_RECORD_KIND,
        validate: validate_provider_call_effect_record,
        links: links_leaf,
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
        return Ok(None);
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
        return Ok(None);
    };
    (contract.links)(value).map(|links| Some(links.finish()))
}

fn validate_admitted_launch_capsule(value: &Value) -> anyhow::Result<()> {
    crate::objects::AdmittedLaunchCapsule::from_current_value(value.clone()).map(|_| ())
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

fn validate_external_large_content_manifest(value: &Value) -> anyhow::Result<()> {
    crate::objects::ExternalLargeContentManifestObject::from_value(value).map(|_| ())
}

fn validate_graph_node_effect_record(value: &Value) -> anyhow::Result<()> {
    crate::objects::GraphNodeEffectRecord::from_current_value(value).map(|_| ())
}

fn validate_item_source(value: &Value) -> anyhow::Result<()> {
    crate::objects::ItemSource::from_value(value).map(|_| ())
}

fn validate_project_file(value: &Value) -> anyhow::Result<()> {
    crate::objects::ProjectFile::from_value(value).map(|_| ())
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

fn validate_provider_call_effect_record(value: &Value) -> anyhow::Result<()> {
    crate::objects::ProviderCallEffectRecord::from_current_value(value).map(|_| ())
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

fn links_leaf(_value: &Value) -> Result<ContractLinks, String> {
    Ok(ContractLinks::leaf())
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
    }
    for hash in super::external_realization_manifest_hashes(value)? {
        super::push_typed_hash(
            &hash,
            ExpectedObject::OneOf(EXTERNAL_MANIFEST_KINDS),
            None,
            &mut links.object_edges,
        )?;
    }

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
    Ok(links)
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
        let anchor = crate::objects::StateAnchorMilestoneV2::from_value(
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
        super::push_optional_hash(entry, "target_blob", &mut links.blob_hashes)?;
    }
    Ok(links)
}

fn links_external_large_content_manifest(value: &Value) -> Result<ContractLinks, String> {
    let manifest = crate::objects::ExternalLargeContentManifestObject::from_value(value)
        .map_err(|error| format!("invalid external large-content manifest: {error:#}"))?;
    Ok(ContractLinks {
        object_edges: Vec::new(),
        blob_hashes: Vec::new(),
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
    fn inventory_and_registry_are_sorted_and_identical() {
        let registry = CURRENT_OBJECT_CONTRACTS
            .iter()
            .map(|contract| contract.kind)
            .collect::<Vec<_>>();
        assert!(registry.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(registry, CURRENT_OBJECT_KINDS);
    }
}
