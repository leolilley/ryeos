//! Admission validators for events, artifacts, facets and state anchors.
use super::*;

pub(super) const LAUNCH_ATTEMPT_AUDIT_TYPES: [ryeos_runtime::RuntimeEventType; 3] = [
    ryeos_runtime::RuntimeEventType::AsLaunchedResolution,
    ryeos_runtime::RuntimeEventType::AsLaunchedRefBindings,
    ryeos_runtime::RuntimeEventType::RuntimeLaunchFacts,
];

pub(super) fn validate_launch_attempt_audit(events: &[NewEventRecord]) -> Result<()> {
    if events.len() < LAUNCH_ATTEMPT_AUDIT_TYPES.len() {
        bail!(
            "launch attempt audit must contain the {} required authority events, received {}",
            LAUNCH_ATTEMPT_AUDIT_TYPES.len(),
            events.len()
        );
    }
    for (index, (event, expected)) in events
        .iter()
        .take(LAUNCH_ATTEMPT_AUDIT_TYPES.len())
        .zip(LAUNCH_ATTEMPT_AUDIT_TYPES)
        .enumerate()
    {
        if event.event_type != expected.as_str() {
            bail!(
                "launch attempt audit event {index} must be '{}', received '{}'",
                expected.as_str(),
                event.event_type
            );
        }
        let expected_storage = expected.storage_class().as_str();
        if event.storage_class != expected_storage {
            bail!(
                "launch attempt audit event '{}' must use canonical storage class '{}', received '{}'",
                event.event_type,
                expected_storage,
                event.storage_class
            );
        }
    }
    for (index, event) in events
        .iter()
        .enumerate()
        .skip(LAUNCH_ATTEMPT_AUDIT_TYPES.len())
    {
        if event.event_type != ryeos_runtime::RuntimeEventType::LaunchAugmentationCacheHit.as_str()
        {
            bail!(
                "launch attempt audit event {index} must be a canonical launch augmentation audit, received '{}'",
                event.event_type
            );
        }
    }
    Ok(())
}

pub(super) fn validate_artifact_shape(
    artifact_type: &str,
    metadata: Option<&Value>,
) -> Result<usize> {
    if artifact_type.is_empty() || artifact_type.len() > MAX_THREAD_ARTIFACT_TYPE_BYTES {
        bail!("artifact_type must be 1..={MAX_THREAD_ARTIFACT_TYPE_BYTES} UTF-8 bytes");
    }
    let metadata_bytes = metadata
        .map(serde_json::to_vec)
        .transpose()?
        .map_or(0, |bytes| bytes.len());
    if metadata_bytes > MAX_THREAD_ARTIFACT_METADATA_BYTES {
        bail!(
            "artifact metadata is {metadata_bytes} bytes; maximum is {MAX_THREAD_ARTIFACT_METADATA_BYTES}"
        );
    }
    Ok(metadata_bytes)
}

pub(super) fn validate_new_artifact_shape(
    artifact_type: &str,
    metadata: Option<&Value>,
) -> Result<usize> {
    let null_metadata = Value::Null;
    validate_artifact_shape(artifact_type, Some(metadata.unwrap_or(&null_metadata)))
}

pub(super) fn ensure_artifact_projection_capacity(
    g: &Inner,
    thread_id: &str,
    additional_items: usize,
    additional_kind_bytes: usize,
    additional_metadata_bytes: usize,
) -> Result<()> {
    let (current_items, current_kind_bytes, current_metadata_bytes) =
        queries::thread_artifact_stats(g.state_db.projection(), thread_id)?;
    let final_items = current_items
        .checked_add(additional_items)
        .ok_or_else(|| anyhow!("thread artifact count overflow"))?;
    let final_metadata_bytes = current_metadata_bytes
        .checked_add(additional_metadata_bytes)
        .ok_or_else(|| anyhow!("thread artifact byte total overflow"))?;
    let final_kind_bytes = current_kind_bytes
        .checked_add(additional_kind_bytes)
        .ok_or_else(|| anyhow!("thread artifact kind byte total overflow"))?;
    if final_items > MAX_THREAD_ARTIFACT_ITEMS {
        bail!(
            "thread {thread_id} would have {final_items} artifacts; maximum is {MAX_THREAD_ARTIFACT_ITEMS}"
        );
    }
    if final_metadata_bytes > MAX_THREAD_ARTIFACT_METADATA_TOTAL_BYTES {
        bail!(
            "thread {thread_id} artifact metadata would total {final_metadata_bytes} bytes; maximum is {MAX_THREAD_ARTIFACT_METADATA_TOTAL_BYTES}"
        );
    }
    // JSON escaping can expand an arbitrary UTF-8 kind by at most six bytes
    // per source byte. Metadata is already stored as serialized JSON. Include
    // conservative fixed record overhead so every newly admitted collection is
    // guaranteed to fit the same response ceiling enforced by readers.
    let fixed_record_bytes = final_items
        .checked_mul(160)
        .ok_or_else(|| anyhow!("thread artifact response estimate overflow"))?;
    let estimated_response_bytes = final_kind_bytes
        .checked_mul(6)
        .and_then(|bytes| bytes.checked_add(final_metadata_bytes))
        .and_then(|bytes| bytes.checked_add(fixed_record_bytes))
        .and_then(|bytes| bytes.checked_add(2))
        .ok_or_else(|| anyhow!("thread artifact response estimate overflow"))?;
    if estimated_response_bytes > MAX_THREAD_ARTIFACT_RESPONSE_BYTES {
        bail!(
            "thread {thread_id} artifacts would exceed the {MAX_THREAD_ARTIFACT_RESPONSE_BYTES}-byte response maximum"
        );
    }
    Ok(())
}

pub(super) fn validate_state_anchor_request(params: &StateAnchorPublishParams) -> Result<()> {
    for (label, value) in [
        ("thread_id", params.thread_id.as_str()),
        ("contract", params.contract.as_str()),
        ("anchor label", params.anchor.label.as_str()),
    ] {
        if value.is_empty()
            || value.trim() != value
            || value.chars().any(char::is_control)
            || value.len() > 4 * 1024
        {
            bail!("state-anchor {label} must be a bounded, trimmed, control-free string");
        }
    }
    params.subject.validate()?;
    if !params.restore.is_object() {
        bail!("state-anchor restore contract must be an object");
    }
    if !params.anchor.runtime.is_object() {
        bail!("state-anchor runtime must be an object");
    }
    if !params.anchor.metadata.is_object() {
        bail!("state-anchor metadata must be an object");
    }
    let metadata_bytes = lillux::canonical_json(&params.anchor.metadata)
        .context("canonicalize state-anchor metadata")?
        .len();
    if metadata_bytes > MAX_STATE_ANCHOR_METADATA_BYTES {
        bail!(
            "state-anchor metadata is {metadata_bytes} bytes; maximum is {MAX_STATE_ANCHOR_METADATA_BYTES}"
        );
    }
    if params.objects.len() > ryeos_state::objects::MAX_STATE_MANIFEST_OBJECTS {
        bail!(
            "state manifest has {} objects; maximum is {}",
            params.objects.len(),
            ryeos_state::objects::MAX_STATE_MANIFEST_OBJECTS
        );
    }
    let mut names = HashSet::new();
    names.insert("restore");
    for input in &params.objects {
        for (label, value) in [
            ("object name", input.name.as_str()),
            ("object media_type", input.media_type.as_str()),
        ] {
            if value.is_empty()
                || value.trim() != value
                || value.chars().any(char::is_control)
                || value.len() > 1024
            {
                bail!("state manifest {label} must be bounded, trimmed, and control-free");
            }
        }
        if !names.insert(input.name.as_str()) {
            bail!(
                "state manifest contains duplicate object name {:?}",
                input.name
            );
        }
        validate_prefixed_digest("state manifest object digest", &input.digest)?;
    }
    validate_prefixed_digest("restore_digest", &params.restore_digest)
}

pub(super) fn persisted_event_from_authoritative(
    event_hash: String,
    event: ryeos_state::objects::ThreadEvent,
) -> Result<PersistedEventRecord> {
    let chain_seq = i64::try_from(event.chain_seq)
        .context("authoritative event chain_seq exceeds SQLite range")?;
    let thread_seq = i64::try_from(event.thread_seq)
        .context("authoritative event thread_seq exceeds SQLite range")?;
    let storage_class = persisted_event_storage_class(event.durability)?;
    Ok(PersistedEventRecord {
        event_id: chain_seq,
        event_hash: Some(event_hash),
        chain_root_id: event.chain_root_id,
        chain_seq,
        thread_id: event.thread_id,
        thread_seq,
        event_type: event.event_type,
        storage_class: storage_class.to_string(),
        ts: event.ts,
        prev_chain_event_hash: event.prev_chain_event_hash,
        prev_thread_event_hash: event.prev_thread_event_hash,
        payload: event.payload,
    })
}

pub(super) fn execution_event_candidate_matches(
    candidate: &PersistedEventRecord,
    signed: &PersistedEventRecord,
) -> bool {
    // `event_id` is a rebuildable SQLite row coordinate, not part of the
    // signed event. Every fact and braid coordinate a consumer reads must
    // agree with the authenticated object.
    candidate.event_hash == signed.event_hash
        && candidate.chain_root_id == signed.chain_root_id
        && candidate.chain_seq == signed.chain_seq
        && candidate.thread_id == signed.thread_id
        && candidate.thread_seq == signed.thread_seq
        && candidate.event_type == signed.event_type
        && candidate.storage_class == signed.storage_class
        && candidate.ts == signed.ts
        && candidate.prev_chain_event_hash == signed.prev_chain_event_hash
        && candidate.prev_thread_event_hash == signed.prev_thread_event_hash
        && candidate.payload == signed.payload
}

pub(super) fn persisted_event_storage_class(
    durability: ryeos_state::objects::EventDurability,
) -> Result<&'static str> {
    match durability {
        ryeos_state::objects::EventDurability::Durable => Ok("indexed"),
        ryeos_state::objects::EventDurability::Journal => Ok("journal"),
        ryeos_state::objects::EventDurability::Ephemeral => {
            bail!("persisted event cannot have ephemeral durability")
        }
    }
}

pub(super) fn validate_prefixed_digest(label: &str, value: &str) -> Result<()> {
    let Some(hash) = value.strip_prefix("sha256:") else {
        bail!("{label} must use the sha256:<hash> form");
    };
    if !lillux::valid_hash(hash) || hash.bytes().any(|byte| byte.is_ascii_uppercase()) {
        bail!("{label} must contain a canonical SHA-256 hash");
    }
    Ok(())
}

pub(super) fn require_prefixed_digest(label: &str, value: &str, expected_hash: &str) -> Result<()> {
    validate_prefixed_digest(label, value)?;
    if value != format!("sha256:{expected_hash}") {
        bail!("{label} does not match the canonical supplied bytes");
    }
    Ok(())
}

pub(super) fn validate_artifact_event_admission(
    g: &Inner,
    thread_id: &str,
    events: &[NewEventRecord],
) -> Result<()> {
    let mut additional_items = 0usize;
    let mut additional_kind_bytes = 0usize;
    let mut additional_metadata_bytes = 0usize;
    for event in events.iter().filter(|event| {
        event.storage_class == "indexed"
            && event.event_type == ryeos_state::event_types::ARTIFACT_PUBLISHED
    }) {
        let artifact_type = event
            .payload
            .get("artifact_type")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("artifact_published requires string artifact_type"))?;
        let metadata_bytes = validate_artifact_shape(artifact_type, event.payload.get("metadata"))?;
        additional_items = additional_items
            .checked_add(1)
            .ok_or_else(|| anyhow!("artifact batch count overflow"))?;
        additional_kind_bytes = additional_kind_bytes
            .checked_add(artifact_type.len())
            .ok_or_else(|| anyhow!("artifact batch kind byte total overflow"))?;
        additional_metadata_bytes = additional_metadata_bytes
            .checked_add(metadata_bytes)
            .ok_or_else(|| anyhow!("artifact batch byte total overflow"))?;
    }
    if additional_items > 0 {
        ensure_artifact_projection_capacity(
            g,
            thread_id,
            additional_items,
            additional_kind_bytes,
            additional_metadata_bytes,
        )?;
    }
    Ok(())
}

pub(super) fn validate_facet_event_admission(
    g: &Inner,
    thread_id: &str,
    events: &[NewEventRecord],
) -> Result<()> {
    let mut updates = HashMap::<String, usize>::new();
    for event in events.iter().filter(|event| {
        event.storage_class == "indexed"
            && event.event_type == ryeos_state::event_types::THREAD_FACET_SET
    }) {
        let key = event
            .payload
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("thread_facet_set requires string key"))?;
        let value = event
            .payload
            .get("value")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("thread_facet_set requires string value"))?;
        if key.is_empty() || key.len() > MAX_THREAD_FACET_KEY_BYTES {
            bail!("facet key must be 1..={MAX_THREAD_FACET_KEY_BYTES} UTF-8 bytes");
        }
        if value.len() > MAX_THREAD_FACET_VALUE_BYTES {
            bail!(
                "facet value is {} bytes; maximum is {MAX_THREAD_FACET_VALUE_BYTES}",
                value.len()
            );
        }
        // Multiple updates to one key in a batch project sequentially; only the
        // final value contributes to the durable facet collection.
        updates.insert(key.to_string(), value.len());
    }
    if updates.is_empty() {
        return Ok(());
    }

    let (mut final_items, mut final_content_bytes) =
        queries::thread_facet_stats(g.state_db.projection(), thread_id)?;
    for (key, value_bytes) in updates {
        match queries::thread_facet_value_bytes(g.state_db.projection(), thread_id, &key)? {
            Some(previous_value_bytes) => {
                final_content_bytes = final_content_bytes
                    .checked_sub(previous_value_bytes)
                    .and_then(|bytes| bytes.checked_add(value_bytes))
                    .ok_or_else(|| anyhow!("thread facet byte total overflow"))?;
            }
            None => {
                final_items = final_items
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("thread facet count overflow"))?;
                final_content_bytes = final_content_bytes
                    .checked_add(key.len())
                    .and_then(|bytes| bytes.checked_add(value_bytes))
                    .ok_or_else(|| anyhow!("thread facet byte total overflow"))?;
            }
        }
    }
    ensure_facet_collection_bounds(thread_id, final_items, final_content_bytes)
}

pub(super) fn ensure_facet_collection_bounds(
    thread_id: &str,
    final_items: usize,
    final_content_bytes: usize,
) -> Result<()> {
    if final_items > MAX_THREAD_FACET_ITEMS {
        bail!(
            "thread {thread_id} would have {final_items} facets; maximum is {MAX_THREAD_FACET_ITEMS}"
        );
    }
    if final_content_bytes > MAX_THREAD_FACET_CONTENT_BYTES {
        bail!(
            "thread {thread_id} facet content would total {final_content_bytes} bytes; maximum is {MAX_THREAD_FACET_CONTENT_BYTES}"
        );
    }
    let fixed_entry_bytes = final_items
        .checked_mul(8)
        .ok_or_else(|| anyhow!("thread facet response estimate overflow"))?;
    let estimated_response_bytes = final_content_bytes
        .checked_mul(6)
        .and_then(|bytes| bytes.checked_add(fixed_entry_bytes))
        .and_then(|bytes| bytes.checked_add(2))
        .ok_or_else(|| anyhow!("thread facet response estimate overflow"))?;
    if estimated_response_bytes > MAX_THREAD_FACET_RESPONSE_BYTES {
        bail!(
            "thread {thread_id} facets would exceed the {MAX_THREAD_FACET_RESPONSE_BYTES}-byte response maximum"
        );
    }
    Ok(())
}

pub(super) fn load_bounded_facets_many(
    g: &Inner,
    thread_ids: &[String],
) -> Result<Vec<queries::FacetRow>> {
    let mut seen = HashSet::new();
    let thread_ids = thread_ids
        .iter()
        .filter(|thread_id| seen.insert(thread_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if thread_ids.len() > MAX_THREAD_LIST_ENRICHMENT_THREADS {
        bail!(
            "thread-list enrichment requested {} threads; maximum is {MAX_THREAD_LIST_ENRICHMENT_THREADS}",
            thread_ids.len()
        );
    }
    let stats = queries::thread_facet_stats_many(g.state_db.projection(), &thread_ids)?;
    let mut total_items = 0usize;
    let mut total_content_bytes = 0usize;
    for (thread_id, items, content_bytes) in stats {
        ensure_facet_collection_bounds(&thread_id, items, content_bytes)?;
        total_items = total_items
            .checked_add(items)
            .ok_or_else(|| anyhow!("thread-list facet count overflow"))?;
        total_content_bytes = total_content_bytes
            .checked_add(content_bytes)
            .ok_or_else(|| anyhow!("thread-list facet byte total overflow"))?;
    }
    if total_items > MAX_THREAD_LIST_FACET_ITEMS {
        bail!(
            "thread-list facets contain {total_items} entries; maximum is {MAX_THREAD_LIST_FACET_ITEMS}"
        );
    }
    if total_content_bytes > MAX_THREAD_LIST_FACET_CONTENT_BYTES {
        bail!(
            "thread-list facet content is {total_content_bytes} bytes; maximum is {MAX_THREAD_LIST_FACET_CONTENT_BYTES}"
        );
    }
    let fixed_entry_bytes = total_items
        .checked_mul(8)
        .ok_or_else(|| anyhow!("thread-list facet response estimate overflow"))?;
    let estimated_response_bytes = total_content_bytes
        .checked_mul(6)
        .and_then(|bytes| bytes.checked_add(fixed_entry_bytes))
        .and_then(|bytes| bytes.checked_add(2))
        .ok_or_else(|| anyhow!("thread-list facet response estimate overflow"))?;
    if estimated_response_bytes > MAX_THREAD_LIST_FACET_RESPONSE_BYTES {
        bail!(
            "thread-list facets would exceed the {MAX_THREAD_LIST_FACET_RESPONSE_BYTES}-byte response maximum"
        );
    }
    queries::get_facets_many_bounded(
        g.state_db.projection(),
        &thread_ids,
        MAX_THREAD_LIST_FACET_ITEMS,
        MAX_THREAD_FACET_KEY_BYTES,
        MAX_THREAD_FACET_VALUE_BYTES,
        MAX_THREAD_LIST_FACET_CONTENT_BYTES,
    )
}
