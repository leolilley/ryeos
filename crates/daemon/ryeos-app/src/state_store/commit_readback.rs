//! Exact committed-state readback, commit classification and event append.
use super::*;

pub(super) enum ContinuationCommitReadback {
    Exact(Box<ThreadSnapshot>),
    ProvenAbsent,
    Ambiguous(&'static str),
}

pub(super) struct ExpectedContinuationCommit<'a> {
    pub(super) source_thread_id: &'a str,
    pub(super) source_before: &'a ThreadSnapshot,
    pub(super) source_after: &'a ThreadSnapshot,
    pub(super) source_snapshot_updated: bool,
    pub(super) source_event: &'a ryeos_state::objects::ThreadEvent,
    pub(super) successor_snapshot: &'a ThreadSnapshot,
    pub(super) successor_events: &'a [ryeos_state::objects::ThreadEvent],
}

pub(super) fn same_authored_event(
    actual: &ryeos_state::objects::ThreadEvent,
    expected: &ryeos_state::objects::ThreadEvent,
) -> bool {
    actual.schema == expected.schema
        && actual.kind == expected.kind
        && actual.chain_root_id == expected.chain_root_id
        && actual.thread_id == expected.thread_id
        && actual.event_type == expected.event_type
        && actual.durability == expected.durability
        && actual.ts == expected.ts
        && actual.payload == expected.payload
}

pub(super) fn same_exact_snapshot(actual: &ThreadSnapshot, expected: &ThreadSnapshot) -> bool {
    actual.to_value() == expected.to_value()
}

pub(super) enum EventAppendCommitReadback {
    Exact(Vec<ryeos_state::objects::ThreadEvent>),
    ProvenAbsent,
    Ambiguous(&'static str),
}

pub(super) fn same_unpositioned_authored_event(
    actual: &ryeos_state::objects::ThreadEvent,
    expected: &ryeos_state::objects::ThreadEvent,
) -> bool {
    actual.schema == expected.schema
        && actual.kind == expected.kind
        && actual.chain_root_id == expected.chain_root_id
        && actual.thread_id == expected.thread_id
        && actual.event_type == expected.event_type
        && actual.durability == expected.durability
        && actual.payload == expected.payload
}

pub(super) fn read_exact_committed_event_append(
    g: &Inner,
    chain_root_id: &str,
    thread_id: &str,
    predecessor: &ryeos_state::AuthoritativeThreadAppendAnchorReadback,
    expected: &[ryeos_state::objects::ThreadEvent],
) -> Result<EventAppendCommitReadback> {
    let readback = g
        .state_db
        .read_authoritative_thread_append_history(chain_root_id, thread_id)?;
    classify_event_append_readback(readback.as_ref(), predecessor, expected)
}

pub(super) fn classify_event_append_readback(
    readback: Option<&ryeos_state::AuthoritativeThreadAppendHistoryReadback>,
    predecessor: &ryeos_state::AuthoritativeThreadAppendAnchorReadback,
    expected: &[ryeos_state::objects::ThreadEvent],
) -> Result<EventAppendCommitReadback> {
    let Some(readback) = readback else {
        return Ok(EventAppendCommitReadback::Ambiguous(
            "authoritative thread disappeared after append attempt",
        ));
    };
    if readback.anchor == *predecessor {
        return Ok(EventAppendCommitReadback::ProvenAbsent);
    }

    let event_count =
        u64::try_from(expected.len()).context("expected append batch is too large")?;
    let Some(final_chain_seq) = predecessor.chain_last_seq.checked_add(event_count) else {
        return Ok(EventAppendCommitReadback::Ambiguous(
            "expected chain sequence overflowed",
        ));
    };
    let Some(final_thread_seq) = predecessor.thread_last_seq.checked_add(event_count) else {
        return Ok(EventAppendCommitReadback::Ambiguous(
            "expected thread sequence overflowed",
        ));
    };
    if readback.anchor.chain_last_seq != final_chain_seq
        || readback.anchor.thread_last_seq != final_thread_seq
    {
        return Ok(EventAppendCommitReadback::Ambiguous(
            "authoritative append position diverged from the attempted batch",
        ));
    }

    let Some(batch_start) = readback.events.len().checked_sub(expected.len()) else {
        return Ok(EventAppendCommitReadback::Ambiguous(
            "authoritative thread history is shorter than the attempted batch",
        ));
    };
    let actual = &readback.events[batch_start..];
    let mut previous_chain_hash = predecessor.chain_last_event_hash.as_deref();
    let mut previous_thread_hash = predecessor.thread_last_event_hash.as_deref();
    for (index, ((hash, actual), expected)) in actual.iter().zip(expected).enumerate() {
        let offset = u64::try_from(index).context("append batch index exceeds u64")?;
        let expected_chain_seq = predecessor
            .chain_last_seq
            .checked_add(offset)
            .and_then(|sequence| sequence.checked_add(1));
        let expected_thread_seq = predecessor
            .thread_last_seq
            .checked_add(offset)
            .and_then(|sequence| sequence.checked_add(1));
        if Some(actual.chain_seq) != expected_chain_seq
            || Some(actual.thread_seq) != expected_thread_seq
            || actual.prev_chain_event_hash.as_deref() != previous_chain_hash
            || actual.prev_thread_event_hash.as_deref() != previous_thread_hash
            || !same_unpositioned_authored_event(actual, expected)
        {
            return Ok(EventAppendCommitReadback::Ambiguous(
                "authoritative event batch diverged from the attempted append",
            ));
        }
        previous_chain_hash = Some(hash.as_str());
        previous_thread_hash = Some(hash.as_str());
    }
    if readback.anchor.chain_last_event_hash.as_deref() != previous_chain_hash
        || readback.anchor.thread_last_event_hash.as_deref() != previous_thread_hash
    {
        return Ok(EventAppendCommitReadback::Ambiguous(
            "authoritative append anchors do not name the attempted batch tail",
        ));
    }

    Ok(EventAppendCommitReadback::Exact(
        actual.iter().map(|(_, event)| event.clone()).collect(),
    ))
}

pub(super) fn same_successor_event_batch(
    actual: &[(String, ryeos_state::objects::ThreadEvent)],
    expected: &[ryeos_state::objects::ThreadEvent],
) -> bool {
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected)
            .enumerate()
            .all(|(index, ((_hash, actual), expected))| {
                let expected_sequence = u64::try_from(index)
                    .ok()
                    .and_then(|index| index.checked_add(1));
                expected_sequence == Some(actual.thread_seq)
                    && expected_sequence == Some(expected.thread_seq)
                    && same_authored_event(actual, expected)
            })
}

pub(super) enum RootCommitReadback {
    Exact(Box<ThreadSnapshot>),
    ProvenAbsent,
    Ambiguous(&'static str),
}

pub(super) struct ExpectedRootCommit<'a> {
    pub(super) birth_snapshot: &'a ThreadSnapshot,
    pub(super) event_successor_snapshot: Option<&'a ThreadSnapshot>,
    pub(super) events: &'a [ryeos_state::objects::ThreadEvent],
}

pub(super) fn has_exact_root_event_positions(
    events: &[(String, ryeos_state::objects::ThreadEvent)],
) -> bool {
    if events.is_empty() {
        return false;
    }
    let mut previous_hash = None;
    for (index, (hash, event)) in events.iter().enumerate() {
        let expected_sequence = u64::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(1));
        if Some(event.chain_seq) != expected_sequence
            || Some(event.thread_seq) != expected_sequence
            || event.prev_chain_event_hash.as_deref() != previous_hash
            || event.prev_thread_event_hash.as_deref() != previous_hash
        {
            return false;
        }
        previous_hash = Some(hash.as_str());
    }
    true
}

pub(super) fn read_exact_committed_root(
    g: &StateStoreGuard<'_>,
    chain_root_id: &str,
    expected: ExpectedRootCommit<'_>,
) -> Result<RootCommitReadback> {
    let Some(readback) = g.state_db.read_authoritative_root_birth(chain_root_id)? else {
        return Ok(RootCommitReadback::ProvenAbsent);
    };
    if !readback.only_root_thread {
        return Ok(RootCommitReadback::Ambiguous(
            "authoritative root head contains a divergent thread set",
        ));
    }
    let Some(predecessor) = readback.predecessor else {
        return Ok(RootCommitReadback::Ambiguous(
            "authoritative root head has no predecessor genesis",
        ));
    };
    if predecessor.prev_chain_state_hash.is_some()
        || predecessor.last_event_hash.is_some()
        || predecessor.last_chain_seq != 0
        || !predecessor.only_root_thread
        || predecessor.root_last_event_hash.is_some()
        || predecessor.root_last_thread_seq != Some(0)
    {
        return Ok(RootCommitReadback::Ambiguous(
            "authoritative root predecessor is not the exact zero-event genesis",
        ));
    }
    let Some(genesis_snapshot) = predecessor.snapshot else {
        return Ok(RootCommitReadback::Ambiguous(
            "authoritative root predecessor has no root snapshot",
        ));
    };
    let expected_birth = StateDb::normalize_expected_root_birth(expected.birth_snapshot.clone())?;
    if !same_exact_snapshot(&genesis_snapshot, &expected_birth) {
        return Ok(RootCommitReadback::Ambiguous(
            "authoritative root genesis snapshot diverges from the attempted birth",
        ));
    }
    let Some(root) = readback.root else {
        return Ok(RootCommitReadback::Ambiguous(
            "authoritative root head has no root thread",
        ));
    };
    if !root.is_chain_final_event {
        return Ok(RootCommitReadback::Ambiguous(
            "authoritative root event batch is not chain-final",
        ));
    }
    let expected_events =
        StateDb::normalize_expected_root_events(expected.events.to_vec(), &predecessor.updated_at)?;
    if !same_successor_event_batch(&root.events, &expected_events) {
        return Ok(RootCommitReadback::Ambiguous(
            "authoritative root event batch diverges from the authored batch",
        ));
    }
    if !has_exact_root_event_positions(&root.events) {
        return Ok(RootCommitReadback::Ambiguous(
            "authoritative root events do not occupy the exact genesis append",
        ));
    }
    let expected_published_snapshot =
        if let Some(event_successor_snapshot) = expected.event_successor_snapshot {
            let Some((last_event_hash, last_event)) = root.events.last() else {
                return Ok(RootCommitReadback::Ambiguous(
                    "authoritative root event batch is empty",
                ));
            };
            let event_count = u64::try_from(root.events.len())
                .context("authoritative root event batch is too large")?;
            StateDb::normalize_expected_root_event_successor(
                &expected_birth,
                event_successor_snapshot.clone(),
                last_event_hash.clone(),
                event_count,
                &last_event.ts,
            )?
        } else {
            expected_birth
        };
    if !same_exact_snapshot(&root.snapshot, &expected_published_snapshot) {
        return Ok(RootCommitReadback::Ambiguous(
            "authoritative root snapshot diverges from the exact published state",
        ));
    }
    Ok(RootCommitReadback::Exact(Box::new(root.snapshot)))
}

pub(super) fn has_exact_atomic_continuation_positions(
    predecessor: &ryeos_state::AuthoritativeContinuationPredecessorReadback,
    successor_events: &[(String, ryeos_state::objects::ThreadEvent)],
    source_event: &ryeos_state::objects::ThreadEvent,
) -> bool {
    if !predecessor.source_was_present
        || !predecessor.successor_was_absent
        || successor_events.is_empty()
    {
        return false;
    }

    let mut expected_chain_seq = predecessor.last_chain_seq;
    let mut previous_chain_hash = predecessor.last_event_hash.as_deref();
    let mut previous_successor_hash = None;
    for (index, (hash, event)) in successor_events.iter().enumerate() {
        let Some(next_chain_seq) = expected_chain_seq.checked_add(1) else {
            return false;
        };
        let expected_thread_seq = u64::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(1));
        if event.chain_seq != next_chain_seq
            || Some(event.thread_seq) != expected_thread_seq
            || event.prev_chain_event_hash.as_deref() != previous_chain_hash
            || event.prev_thread_event_hash.as_deref() != previous_successor_hash
        {
            return false;
        }
        expected_chain_seq = next_chain_seq;
        previous_chain_hash = Some(hash.as_str());
        previous_successor_hash = Some(hash.as_str());
    }

    let Some(expected_source_chain_seq) = expected_chain_seq.checked_add(1) else {
        return false;
    };
    let Some(expected_source_thread_seq) = predecessor.source_last_thread_seq.checked_add(1) else {
        return false;
    };
    source_event.chain_seq == expected_source_chain_seq
        && source_event.thread_seq == expected_source_thread_seq
        && source_event.prev_chain_event_hash.as_deref() == previous_chain_hash
        && source_event.prev_thread_event_hash.as_deref()
            == predecessor.source_last_event_hash.as_deref()
}

/// Prove that the current signed head contains both halves of the exact
/// continuation attempted by the caller. The state-layer pair read holds one
/// chain lock across the source and successor, so a matching successor cannot
/// be combined with a source event from another head.
pub(super) fn read_exact_committed_continuation(
    g: &StateStoreGuard<'_>,
    chain_root_id: &str,
    expected: ExpectedContinuationCommit<'_>,
) -> Result<ContinuationCommitReadback> {
    let readback = g.state_db.read_authoritative_continuation(
        chain_root_id,
        expected.source_thread_id,
        &expected.successor_snapshot.thread_id,
    )?;
    let Some(source) = readback.source else {
        return Ok(ContinuationCommitReadback::Ambiguous(
            "authoritative continuation source is absent",
        ));
    };
    let source_snapshot = source.snapshot;
    let source_claims_transition =
        source
            .last_event
            .as_ref()
            .is_some_and(|(_source_event_hash, source_event)| {
                source_snapshot.chain_root_id == chain_root_id
                    && source_snapshot.thread_id == expected.source_thread_id
                    && source_event.chain_root_id == chain_root_id
                    && source_event.thread_id == expected.source_thread_id
                    && source_event.event_type == expected.source_event.event_type
                    && source_event.payload == expected.source_event.payload
            });
    let Some(successor) = readback.successor else {
        return if source_claims_transition {
            Ok(ContinuationCommitReadback::Ambiguous(
                "source claims the exact continuation but its successor is absent",
            ))
        } else {
            Ok(ContinuationCommitReadback::ProvenAbsent)
        };
    };
    let successor_snapshot = successor.snapshot;
    if !source_claims_transition {
        return Ok(ContinuationCommitReadback::Ambiguous(
            "authoritative successor exists without the exact source transition",
        ));
    }
    if !source.is_chain_final_event {
        return Ok(ContinuationCommitReadback::Ambiguous(
            "authoritative source relation event is not the current chain-final event",
        ));
    }
    let Some((_source_event_hash, source_event)) = source.last_event else {
        return Ok(ContinuationCommitReadback::Ambiguous(
            "authoritative successor exists but the source has no relation event",
        ));
    };
    let Some(predecessor) = readback.predecessor.as_ref() else {
        return Ok(ContinuationCommitReadback::Ambiguous(
            "authoritative continuation has no predecessor head",
        ));
    };
    let (expected_successor_events, expected_source_event) =
        StateDb::normalize_expected_continuation_events(
            expected.successor_events.to_vec(),
            expected.source_event.clone(),
            &predecessor.updated_at,
        )?;
    if !same_authored_event(&source_event, &expected_source_event) {
        return Ok(ContinuationCommitReadback::Ambiguous(
            "authoritative source relation event diverges from the authored event",
        ));
    }
    if !same_successor_event_batch(&successor.events, &expected_successor_events) {
        return Ok(ContinuationCommitReadback::Ambiguous(
            "authoritative successor event batch diverges from the authored batch",
        ));
    }
    if !has_exact_atomic_continuation_positions(predecessor, &successor.events, &source_event) {
        return Ok(ContinuationCommitReadback::Ambiguous(
            "authoritative events do not occupy one exact atomic continuation append",
        ));
    }
    let normalized_successor = StateDb::normalize_expected_continuation_successor(
        expected.successor_snapshot.clone(),
        &successor_snapshot,
        expected.source_thread_id,
        &source_event.ts,
    )?;
    if successor_snapshot.to_value() != normalized_successor.to_value() {
        return Ok(ContinuationCommitReadback::Ambiguous(
            "authoritative successor exists with divergent or advanced state",
        ));
    }
    let normalized_source = if expected.source_snapshot_updated {
        StateDb::normalize_expected_continuation_source(
            expected.source_before,
            expected.source_after.clone(),
            &source_snapshot,
            &source_event.ts,
        )?
    } else {
        expected.source_after.clone()
    };
    if source_snapshot.to_value() != normalized_source.to_value() {
        return Ok(ContinuationCommitReadback::Ambiguous(
            "authoritative source has divergent post-continuation state",
        ));
    }
    Ok(ContinuationCommitReadback::Exact(Box::new(
        successor_snapshot,
    )))
}

pub(super) fn build_continuation_snapshot(
    thread: &NewThreadRecord,
    resume: &crate::launch_metadata::ResumeContext,
) -> Result<ThreadSnapshot> {
    let (project_root, base_project_snapshot_hash) = resume
        .authoritative_project_identity()
        .context("derive continuation successor project identity")?;
    if thread.project_root.as_deref() != project_root.as_deref() {
        bail!(
            "continuation successor project root {:?} contradicts captured launch root {:?}",
            thread.project_root,
            project_root
        );
    }
    if thread.base_project_snapshot_hash.as_deref() != base_project_snapshot_hash.as_deref() {
        bail!(
            "continuation successor base snapshot {:?} contradicts captured launch snapshot {:?}",
            thread.base_project_snapshot_hash,
            base_project_snapshot_hash
        );
    }
    if thread.project_authority != resume.project_authority {
        bail!("continuation successor project authority contradicts captured launch authority");
    }
    let mut snapshot = build_snapshot(thread);
    snapshot.project_root = project_root;
    snapshot.base_project_snapshot_hash = base_project_snapshot_hash;
    snapshot.project_authority = resume.project_authority.clone();
    Ok(snapshot)
}

pub(super) fn authoritative_snapshot_for_transition(
    inner: &Inner,
    chain_root_id: &str,
    thread_id: &str,
) -> Result<ThreadSnapshot> {
    inner
        .state_db
        .read_authoritative_thread_snapshot(chain_root_id, thread_id)?
        .ok_or_else(|| {
            anyhow!(
                "authoritative snapshot missing for projected thread {thread_id} in chain {chain_root_id}"
            )
        })
}

pub(super) fn continued_snapshot_from_authoritative(
    mut snapshot: ThreadSnapshot,
    now: &str,
) -> ThreadSnapshot {
    snapshot.status = ThreadStatus::Continued;
    snapshot.updated_at = now.to_string();
    snapshot.finished_at = Some(now.to_string());
    snapshot.result = None;
    snapshot.outcome_code = Some("continued".to_string());
    snapshot.error = None;
    snapshot.budget = None;
    snapshot.artifacts.clear();
    snapshot.managed_runtime_terminal = None;
    snapshot.facets.clear();
    snapshot
}

pub(super) fn convert_events(
    events: &[NewEventRecord],
    chain_root_id: &str,
    thread_id: &str,
) -> Vec<ryeos_state::objects::ThreadEvent> {
    let now = lillux::time::iso8601_now();
    events
        .iter()
        .enumerate()
        .map(|(idx, event)| ryeos_state::objects::ThreadEvent {
            schema: ryeos_state::objects::SCHEMA_VERSION,
            kind: "thread_event".to_string(),
            chain_root_id: chain_root_id.to_string(),
            chain_seq: 0,
            thread_id: thread_id.to_string(),
            thread_seq: (idx + 1) as u64,
            event_type: event.event_type.clone(),
            durability: match event.storage_class.as_str() {
                "indexed" => ryeos_state::objects::EventDurability::Durable,
                "journal" | "journal_only" => ryeos_state::objects::EventDurability::Journal,
                "ephemeral" => ryeos_state::objects::EventDurability::Ephemeral,
                _ => ryeos_state::objects::EventDurability::Durable,
            },
            ts: now.clone(),
            prev_chain_event_hash: None,
            prev_thread_event_hash: None,
            payload: event.payload.clone(),
        })
        .collect()
}

pub(super) fn hook_failed_event(
    completed: &runtime_db::CompletedHookDispatch,
    identity: &ryeos_runtime::callback::HookDispatchIdentity,
    descriptor: ryeos_runtime::HookEvidenceDescriptor,
    failure_class: ryeos_runtime::HookFailureClass,
) -> Result<NewEventRecord> {
    let identity_bytes = lillux::canonical_json(&serde_json::json!({
        "schema": "ryeos.hook_failure_identity.v1",
        "chain_root_id": &completed.chain_root_id,
        "occurrence_thread_id": &completed.occurrence_thread_id,
        "occurrence": &identity.occurrence,
        "hook_id": &identity.hook_id,
        "context_hash": &identity.context_hash,
        "failure_class": failure_class,
    }))?;
    let failure_id = format!(
        "hook-failure:{}",
        lillux::sha256_hex(identity_bytes.as_bytes())
    );
    Ok(NewEventRecord {
        event_type: ryeos_state::event_types::HOOK_FAILED.to_string(),
        storage_class: "indexed".to_string(),
        payload: serde_json::to_value(ryeos_runtime::HookFailedPayload {
            schema_version: ryeos_runtime::HOOK_FAILURE_SCHEMA.to_string(),
            failure_id,
            dispatch_key: completed.dispatch_key.clone(),
            occurrence_thread_id: completed.occurrence_thread_id.clone(),
            hook: descriptor,
            context_hash: identity.context_hash.clone(),
            response_hash: completed.response_hash.clone(),
            failure_class,
        })?,
    })
}

pub(super) fn persisted_from_append(
    result: &ryeos_state::chain::AppendResult,
    events: &[NewEventRecord],
) -> Result<Vec<PersistedEventRecord>> {
    persisted_from_stored_events(&result.events, events)
}

pub(super) fn persisted_from_add_thread_with_events(
    result: &ryeos_state::chain::AddThreadWithEventsResult,
    events: &[NewEventRecord],
) -> Result<Vec<PersistedEventRecord>> {
    persisted_from_stored_events(&result.events, events)
}

pub(super) fn persisted_from_stored_events(
    stored_events: &[ryeos_state::objects::ThreadEvent],
    events: &[NewEventRecord],
) -> Result<Vec<PersistedEventRecord>> {
    stored_events
        .iter()
        .zip(events.iter())
        .map(|(stored, input)| {
            Ok(PersistedEventRecord {
                event_id: stored.chain_seq as i64,
                event_hash: Some(
                    ryeos_state::objects::thread_event::hash_event(stored)
                        .context("failed to canonicalize stored thread event")?,
                ),
                chain_root_id: stored.chain_root_id.clone(),
                chain_seq: stored.chain_seq as i64,
                thread_id: stored.thread_id.clone(),
                thread_seq: stored.thread_seq as i64,
                event_type: input.event_type.clone(),
                storage_class: input.storage_class.clone(),
                ts: stored.ts.clone(),
                prev_chain_event_hash: stored.prev_chain_event_hash.clone(),
                prev_thread_event_hash: stored.prev_thread_event_hash.clone(),
                payload: input.payload.clone(),
            })
        })
        .collect()
}

pub(super) fn persisted_event_from_projection_row(
    row: ryeos_state::queries::EventRow,
) -> Result<PersistedEventRecord> {
    // The projection stores the State durability vocabulary ("durable"),
    // not the App event API's storage class ("indexed"). Decode it strictly;
    // accepting API names here would hide a corrupt projection.
    let durability =
        ryeos_state::objects::EventDurability::deserialize(serde::de::value::StrDeserializer::<
            serde::de::value::Error,
        >::new(&row.durability))
        .context("invalid projected event durability")?;
    let storage_class = persisted_event_storage_class(durability)?;
    let payload: Value = serde_json::from_slice(&row.payload).with_context(|| {
        format!(
            "malformed JSON payload for event {} (chain_seq {})",
            row.event_id, row.chain_seq
        )
    })?;
    Ok(PersistedEventRecord {
        event_id: row.event_id,
        event_hash: Some(row.event_hash),
        chain_root_id: row.chain_root_id,
        chain_seq: row.chain_seq,
        thread_id: row.thread_id,
        thread_seq: row.thread_seq,
        event_type: row.event_type,
        storage_class: storage_class.to_owned(),
        ts: row.ts,
        prev_chain_event_hash: row.prev_chain_event_hash,
        prev_thread_event_hash: row.prev_thread_event_hash,
        payload,
    })
}

pub(super) fn ephemeral_record(
    chain_root_id: &str,
    thread_id: &str,
    event: &NewEventRecord,
) -> PersistedEventRecord {
    PersistedEventRecord {
        event_id: 0,
        event_hash: None,
        chain_root_id: chain_root_id.to_string(),
        chain_seq: 0,
        thread_id: thread_id.to_string(),
        thread_seq: 0,
        event_type: event.event_type.clone(),
        storage_class: event.storage_class.clone(),
        ts: lillux::time::iso8601_now(),
        prev_chain_event_hash: None,
        prev_thread_event_hash: None,
        payload: event.payload.clone(),
    }
}

/// Resolve an append that returned an error: signed history decides whether
/// the exact batch was published (continue from the readback), provably was
/// not (the original error), or cannot be told (an uncertain commit).
pub(super) fn committed_append_after_error(
    g: &Inner,
    chain_root_id: &str,
    thread_id: &str,
    predecessor: &ryeos_state::AuthoritativeThreadAppendAnchorReadback,
    attempted: &[ryeos_state::objects::ThreadEvent],
    events: &[NewEventRecord],
    error: anyhow::Error,
) -> Result<Vec<PersistedEventRecord>> {
    match read_exact_committed_event_append(g, chain_root_id, thread_id, predecessor, attempted) {
        Ok(EventAppendCommitReadback::Exact(stored_events)) => {
            tracing::error!(
                thread_id,
                chain_root_id,
                error = %error,
                "event append returned an error after publishing the exact authoritative batch; continuing from signed-head readback"
            );
            persisted_from_stored_events(&stored_events, events)
        }
        Ok(EventAppendCommitReadback::ProvenAbsent) => Err(error),
        Ok(EventAppendCommitReadback::Ambiguous(reason)) => {
            Err(anyhow::Error::new(EventAppendCommitUncertain).context(format!(
                "event append failed with ambiguous authoritative readback ({reason}); write error: {error:#}"
            )))
        }
        Err(readback_error) => Err(anyhow::Error::new(EventAppendCommitUncertain).context(format!(
            "event append failed and authoritative readback was unavailable; write error: {error:#}; readback error: {readback_error:#}"
        ))),
    }
}

pub(super) fn append_events_locked(
    g: &Inner,
    cas_mutation_guard: Option<&ryeos_state::CasMutationGuard>,
    chain_root_id: &str,
    thread_id: &str,
    events: &[NewEventRecord],
) -> Result<Vec<PersistedEventRecord>> {
    validate_artifact_event_admission(g, thread_id, events)?;
    validate_facet_event_admission(g, thread_id, events)?;
    let mut records: Vec<Option<PersistedEventRecord>> = vec![None; events.len()];
    let mut durable_events = Vec::new();
    let mut durable_thread_events = Vec::new();
    let mut durable_indices = Vec::new();
    let converted_events = convert_events(events, chain_root_id, thread_id);

    for (idx, (event, thread_event)) in events.iter().zip(converted_events).enumerate() {
        // Validate before separating ephemeral records: ephemeral events bypass
        // CAS, but must observe the same complete-event byte ceiling as every
        // durable writer.
        thread_event.validate()?;
        if event.storage_class == "ephemeral" {
            records[idx] = Some(ephemeral_record(chain_root_id, thread_id, event));
        } else {
            durable_indices.push(idx);
            durable_events.push(event.clone());
            durable_thread_events.push(thread_event);
        }
    }

    if !durable_events.is_empty() {
        let cas_mutation_guard = cas_mutation_guard.ok_or_else(|| {
            anyhow!("durable event append requires an admitted CAS mutation guard")
        })?;
        let predecessor = g
            .state_db
            .read_authoritative_thread_append_anchor(chain_root_id, thread_id)?
            .ok_or_else(|| {
                anyhow!(
                    "authoritative append predecessor missing for thread {thread_id} in chain {chain_root_id}"
                )
            })?;
        let committed = g.state_db.append_events_admitted(
            chain_root_id,
            thread_id,
            durable_thread_events.clone(),
            vec![],
            g.signer.as_ref(),
            &g.runtime_db,
            cas_mutation_guard,
        );
        let persisted = match committed {
            Ok(committed) => persisted_from_append(&committed_value(committed), &durable_events)?,
            Err(error) => committed_append_after_error(
                g,
                chain_root_id,
                thread_id,
                &predecessor,
                &durable_thread_events,
                &durable_events,
                error,
            )?,
        };
        for (idx, record) in durable_indices.into_iter().zip(persisted) {
            records[idx] = Some(record);
        }
    }

    records
        .into_iter()
        .map(|record| record.ok_or_else(|| anyhow!("append event record missing")))
        .collect()
}

pub(super) fn has_indexed_collection_events(events: &[NewEventRecord]) -> bool {
    events.iter().any(|event| {
        event.storage_class == "indexed"
            && matches!(
                event.event_type.as_str(),
                ryeos_state::event_types::ARTIFACT_PUBLISHED
                    | ryeos_state::event_types::THREAD_FACET_SET
            )
    })
}
