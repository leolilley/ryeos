use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(test)]
use std::path::Path;

use anyhow::Context;

use crate::objects::thread_snapshot::parse_canonical_timestamp;
use crate::objects::{ChainState, ChainThreadEntry, ThreadEvent, ThreadSnapshot, ThreadStatus};

#[derive(Debug)]
struct ThreadEventCursor {
    next_hash: Option<String>,
    next_sequence: u64,
}

#[derive(Debug)]
struct EventPosition {
    thread_id: String,
    chain_sequence: u64,
    thread_sequence: u64,
    timestamp: String,
    prev_chain_hash: Option<String>,
    prev_thread_hash: Option<String>,
}

#[derive(Debug)]
struct SnapshotEventPosition {
    thread_id: String,
    event_hash: String,
    chain_sequence: u64,
    thread_sequence: u64,
}

/// Validate the exact authoritative closure represented by one chain head.
///
/// Object-level validation alone is insufficient: a valid snapshot can still
/// be published under the wrong thread entry, and individually valid events
/// can form forks, gaps, or disagreeing chain/thread sequences. Repair and
/// rebuild call this before projecting any rows so corrupt CAS history never
/// becomes operational state.
fn validate_authoritative_closure(
    cas: &lillux::CasStore,
    state: &ChainState,
    check: &mut dyn FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<HashMap<String, EventPosition>> {
    check()?;
    state.validate()?;
    validate_chain_positions(state)?;

    let state_updated_at = parse_canonical_timestamp(&state.updated_at)
        .with_context(|| "invalid authoritative chain updated_at")?;
    let mut thread_cursors = BTreeMap::new();
    let mut snapshot_event_positions = Vec::new();

    for (thread_id, entry) in &state.threads {
        check()?;
        let snapshot = load_hashed_snapshot(cas, &entry.snapshot_hash)
            .with_context(|| format!("load authoritative snapshot for thread {thread_id}"))?;
        validate_stored_snapshot(state, thread_id, entry, &snapshot)
            .with_context(|| format!("invalid authoritative snapshot for thread {thread_id}"))?;
        let snapshot_updated_at = parse_canonical_timestamp(&snapshot.updated_at)
            .with_context(|| format!("invalid snapshot updated_at for thread {thread_id}"))?;
        if snapshot_updated_at > state_updated_at {
            anyhow::bail!(
                "snapshot updated_at for thread {thread_id} exceeds authoritative chain updated_at"
            );
        }
        if let Some(event_hash) = snapshot.last_event_hash.clone() {
            snapshot_event_positions.push(SnapshotEventPosition {
                thread_id: thread_id.clone(),
                event_hash,
                chain_sequence: snapshot.last_chain_seq,
                thread_sequence: snapshot.last_thread_seq,
            });
        }
        thread_cursors.insert(
            thread_id.clone(),
            ThreadEventCursor {
                next_hash: entry.last_event_hash.clone(),
                next_sequence: entry.last_thread_seq,
            },
        );
    }

    let mut current_hash = state.last_event_hash.clone();
    let mut expected_chain_sequence = state.last_chain_seq;
    let mut newer_timestamp = state_updated_at;
    let mut visited = HashSet::new();
    let mut event_positions = HashMap::new();

    while expected_chain_sequence > 0 {
        check()?;
        let event_hash = current_hash.as_deref().ok_or_else(|| {
            anyhow::anyhow!("chain event history ended before sequence {expected_chain_sequence}")
        })?;
        if !visited.insert(event_hash.to_string()) {
            anyhow::bail!("cycle in chain event history at {event_hash}");
        }

        let event = load_hashed_event(cas, event_hash)
            .with_context(|| format!("load authoritative chain event {event_hash}"))?;
        event.validate()?;
        if event.chain_root_id != state.chain_root_id {
            anyhow::bail!(
                "event {event_hash} belongs to chain {}, not {}",
                event.chain_root_id,
                state.chain_root_id
            );
        }
        if event.chain_seq != expected_chain_sequence {
            anyhow::bail!(
                "event {event_hash} chain sequence mismatch: expected {expected_chain_sequence}, got {}",
                event.chain_seq
            );
        }
        let event_timestamp = parse_canonical_timestamp(&event.ts)
            .with_context(|| format!("invalid event timestamp for {event_hash}"))?;
        if event_timestamp > newer_timestamp {
            anyhow::bail!(
                "event {event_hash} timestamp exceeds the next authoritative chain timestamp"
            );
        }
        newer_timestamp = event_timestamp;

        let cursor = thread_cursors.get_mut(&event.thread_id).ok_or_else(|| {
            anyhow::anyhow!(
                "event {event_hash} names thread {} absent from the authoritative head",
                event.thread_id
            )
        })?;
        if cursor.next_hash.as_deref() != Some(event_hash) {
            anyhow::bail!(
                "thread {} event link mismatch: expected {:?}, reached {event_hash}",
                event.thread_id,
                cursor.next_hash
            );
        }
        if event.thread_seq != cursor.next_sequence {
            anyhow::bail!(
                "event {event_hash} thread sequence mismatch for {}: expected {}, got {}",
                event.thread_id,
                cursor.next_sequence,
                event.thread_seq
            );
        }

        validate_predecessor_link(
            "chain event",
            event.chain_seq,
            event.prev_chain_event_hash.as_deref(),
        )?;
        validate_predecessor_link(
            &format!("thread {} event", event.thread_id),
            event.thread_seq,
            event.prev_thread_event_hash.as_deref(),
        )?;

        event_positions.insert(
            event_hash.to_string(),
            EventPosition {
                thread_id: event.thread_id.clone(),
                chain_sequence: event.chain_seq,
                thread_sequence: event.thread_seq,
                timestamp: event.ts.clone(),
                prev_chain_hash: event.prev_chain_event_hash.clone(),
                prev_thread_hash: event.prev_thread_event_hash.clone(),
            },
        );
        current_hash = event.prev_chain_event_hash;
        expected_chain_sequence -= 1;
        cursor.next_hash = event.prev_thread_event_hash;
        cursor.next_sequence -= 1;
    }

    if current_hash.is_some() {
        anyhow::bail!("chain event history continues before sequence one");
    }
    for (thread_id, cursor) in &thread_cursors {
        check()?;
        if cursor.next_sequence != 0 || cursor.next_hash.is_some() {
            anyhow::bail!(
                "thread {thread_id} event history is not fully represented by the chain event history"
            );
        }
    }
    for snapshot in snapshot_event_positions {
        check()?;
        let event = event_positions.get(&snapshot.event_hash).ok_or_else(|| {
            anyhow::anyhow!(
                "snapshot for thread {} references event {} outside the authoritative chain history",
                snapshot.thread_id,
                snapshot.event_hash
            )
        })?;
        if event.thread_id != snapshot.thread_id
            || event.thread_sequence != snapshot.thread_sequence
            || event.chain_sequence > snapshot.chain_sequence
        {
            anyhow::bail!(
                "snapshot event position mismatch for thread {} at {}",
                snapshot.thread_id,
                snapshot.event_hash
            );
        }
    }

    Ok(event_positions)
}

/// Validate every ChainState from genesis through `target_hash` and prove that
/// each state boundary is a legal local transition. When `expected_ancestor`
/// is present, the target must advance from that already-signed head rather
/// than replace it with a fork.
#[cfg(test)]
pub(crate) fn validate_authoritative_history(
    cas_root: &Path,
    chain_root_id: &str,
    target_hash: &str,
    expected_ancestor: Option<&str>,
) -> anyhow::Result<()> {
    let mut check = || Ok(());
    validate_authoritative_history_with_check(
        cas_root,
        chain_root_id,
        target_hash,
        expected_ancestor,
        &mut check,
    )
}

#[cfg(test)]
pub(crate) fn validate_authoritative_history_with_check(
    cas_root: &Path,
    chain_root_id: &str,
    target_hash: &str,
    expected_ancestor: Option<&str>,
    check: &mut dyn FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let root = lillux::PinnedDirectory::open(cas_root)?.ok_or_else(|| {
        anyhow::anyhow!("authoritative CAS root is absent: {}", cas_root.display())
    })?;
    let cas = lillux::CasStore::from_pinned_root(root);
    validate_authoritative_history_with_cas_and_check(
        &cas,
        chain_root_id,
        target_hash,
        expected_ancestor,
        check,
    )
}

pub(crate) fn validate_authoritative_history_with_cas(
    cas: &lillux::CasStore,
    chain_root_id: &str,
    target_hash: &str,
    expected_ancestor: Option<&str>,
) -> anyhow::Result<()> {
    let mut check = || Ok(());
    validate_authoritative_history_with_cas_and_check(
        cas,
        chain_root_id,
        target_hash,
        expected_ancestor,
        &mut check,
    )
}

pub(crate) fn validate_authoritative_history_with_cas_and_check(
    cas: &lillux::CasStore,
    chain_root_id: &str,
    target_hash: &str,
    expected_ancestor: Option<&str>,
    check: &mut dyn FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    check()?;
    let target = load_hashed_chain_state(cas, target_hash)?;
    if target.chain_root_id != chain_root_id {
        anyhow::bail!(
            "target chain root mismatch: expected {chain_root_id}, got {}",
            target.chain_root_id
        );
    }
    let event_positions = validate_authoritative_closure(cas, &target, check)?;

    let mut newest_to_oldest = Vec::new();
    let mut visited = HashSet::new();
    let mut current_hash = Some(target_hash.to_string());
    while let Some(hash) = current_hash {
        check()?;
        if !visited.insert(hash.clone()) {
            anyhow::bail!("cycle in ChainState history at {hash}");
        }
        let state = load_hashed_chain_state(cas, &hash)
            .with_context(|| format!("load historical ChainState {hash}"))?;
        state.validate()?;
        validate_chain_positions(&state)?;
        if state.chain_root_id != chain_root_id {
            anyhow::bail!(
                "historical ChainState {hash} belongs to {}, not {chain_root_id}",
                state.chain_root_id
            );
        }
        current_hash = state.prev_chain_state_hash.clone();
        newest_to_oldest.push(hash);
    }

    if let Some(expected) = expected_ancestor {
        if !newest_to_oldest.iter().any(|hash| hash == expected) {
            anyhow::bail!(
                "target ChainState {target_hash} does not advance from expected current head {expected}"
            );
        }
    }

    newest_to_oldest.reverse();
    let genesis_hash = newest_to_oldest
        .first()
        .ok_or_else(|| anyhow::anyhow!("empty ChainState history for target {target_hash}"))?;
    let mut previous = load_hashed_chain_state(cas, genesis_hash)?;
    check()?;
    validate_genesis_state(cas, &previous, &event_positions)
        .with_context(|| format!("invalid genesis ChainState {genesis_hash}"))?;
    let mut previous_hash = genesis_hash.clone();

    for next_hash in newest_to_oldest.iter().skip(1) {
        check()?;
        let next = load_hashed_chain_state(cas, next_hash)?;
        validate_chain_state_transition(
            cas,
            &previous_hash,
            &previous,
            next_hash,
            &next,
            &event_positions,
            check,
        )
        .with_context(|| format!("invalid ChainState transition {previous_hash} -> {next_hash}"))?;
        previous = next;
        previous_hash.clone_from(next_hash);
    }

    Ok(())
}

fn validate_genesis_state(
    cas: &lillux::CasStore,
    state: &ChainState,
    events: &HashMap<String, EventPosition>,
) -> anyhow::Result<()> {
    if state.prev_chain_state_hash.is_some() {
        anyhow::bail!("genesis ChainState has a predecessor");
    }
    if state.last_chain_seq != 0 || state.last_event_hash.is_some() {
        anyhow::bail!("genesis ChainState must begin before the first event");
    }
    if state.threads.len() != 1 {
        anyhow::bail!("genesis ChainState must contain only its root thread");
    }
    let entry = state
        .threads
        .get(&state.chain_root_id)
        .expect("ChainState::validate requires its root entry");
    let snapshot = load_hashed_snapshot(cas, &entry.snapshot_hash)?;
    validate_published_snapshot(state, &state.chain_root_id, entry, &snapshot, events)?;
    validate_new_thread_snapshot(&snapshot, None)?;
    Ok(())
}

fn validate_chain_state_transition(
    cas: &lillux::CasStore,
    previous_hash: &str,
    previous: &ChainState,
    next_hash: &str,
    next: &ChainState,
    events: &HashMap<String, EventPosition>,
    check: &mut dyn FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if next.prev_chain_state_hash.as_deref() != Some(previous_hash) {
        anyhow::bail!(
            "ChainState {next_hash} does not name {previous_hash} as its direct predecessor"
        );
    }
    if next.schema != previous.schema
        || next.kind != previous.kind
        || next.chain_root_id != previous.chain_root_id
    {
        anyhow::bail!("ChainState transition changed immutable chain identity");
    }

    let previous_updated_at = parse_canonical_timestamp(&previous.updated_at)?;
    let next_updated_at = parse_canonical_timestamp(&next.updated_at)?;
    if next_updated_at < previous_updated_at {
        anyhow::bail!("ChainState updated_at moved backwards");
    }
    let appended_times = validate_event_advance(
        events,
        EventHistory::Chain,
        previous.last_event_hash.as_deref(),
        previous.last_chain_seq,
        next.last_event_hash.as_deref(),
        next.last_chain_seq,
        check,
    )?;
    if let Some((oldest, newest)) = appended_times.as_ref() {
        if parse_canonical_timestamp(oldest)? < previous_updated_at {
            anyhow::bail!("new chain events precede the previous ChainState timestamp");
        }
        if parse_canonical_timestamp(newest)? > next_updated_at {
            anyhow::bail!("new chain events exceed the successor ChainState timestamp");
        }
    }

    for thread_id in previous.threads.keys() {
        check()?;
        if !next.threads.contains_key(thread_id) {
            anyhow::bail!("ChainState transition removed thread {thread_id}");
        }
    }

    for (thread_id, next_entry) in &next.threads {
        check()?;
        validate_entry_event_position(next, thread_id, next_entry, events)?;
        match previous.threads.get(thread_id) {
            None => {
                validate_event_advance(
                    events,
                    EventHistory::Thread(thread_id),
                    None,
                    0,
                    next_entry.last_event_hash.as_deref(),
                    next_entry.last_thread_seq,
                    check,
                )?;
                let snapshot = load_hashed_snapshot(cas, &next_entry.snapshot_hash)?;
                validate_published_snapshot(next, thread_id, next_entry, &snapshot, events)?;
                let continuation_source = snapshot
                    .base_project_snapshot_hash
                    .as_ref()
                    .and(snapshot.upstream_thread_id.as_deref())
                    .filter(|source| previous.threads.contains_key(*source));
                validate_new_thread_snapshot(&snapshot, continuation_source)?;
            }
            Some(previous_entry) => {
                validate_event_advance(
                    events,
                    EventHistory::Thread(thread_id),
                    previous_entry.last_event_hash.as_deref(),
                    previous_entry.last_thread_seq,
                    next_entry.last_event_hash.as_deref(),
                    next_entry.last_thread_seq,
                    check,
                )?;
                if previous_entry.snapshot_hash == next_entry.snapshot_hash {
                    if previous_entry.status != next_entry.status {
                        anyhow::bail!(
                            "thread {thread_id} changed status without publishing a new snapshot"
                        );
                    }
                } else {
                    let previous_snapshot =
                        load_hashed_snapshot(cas, &previous_entry.snapshot_hash)?;
                    let next_snapshot = load_hashed_snapshot(cas, &next_entry.snapshot_hash)?;
                    validate_published_snapshot(
                        next,
                        thread_id,
                        next_entry,
                        &next_snapshot,
                        events,
                    )?;
                    validate_historical_snapshot_transition(
                        &previous_snapshot,
                        &next_snapshot,
                        appended_times.as_ref().map(|(_, newest)| newest.as_str()),
                    )?;
                }
            }
        }
    }

    Ok(())
}

fn validate_new_thread_snapshot(
    snapshot: &ThreadSnapshot,
    continuation_source: Option<&str>,
) -> anyhow::Result<()> {
    if snapshot.status != ThreadStatus::Created {
        anyhow::bail!("new authoritative thread must begin in created status");
    }
    validate_new_thread_project_snapshots(snapshot, continuation_source)?;
    Ok(())
}

fn validate_new_thread_project_snapshots(
    snapshot: &ThreadSnapshot,
    continuation_source: Option<&str>,
) -> anyhow::Result<()> {
    if snapshot.result_project_snapshot_hash.is_some() {
        anyhow::bail!("new created snapshot cannot carry a result project snapshot hash");
    }
    if snapshot.base_project_snapshot_hash.is_some() {
        let source = continuation_source.ok_or_else(|| {
            anyhow::anyhow!(
                "new created snapshot may carry a base project snapshot hash only for a continuation successor"
            )
        })?;
        if snapshot.upstream_thread_id.as_deref() != Some(source) {
            anyhow::bail!(
                "continuation successor upstream {:?} does not match continuation source {source}",
                snapshot.upstream_thread_id
            );
        }
    }
    Ok(())
}

fn validate_published_snapshot(
    state: &ChainState,
    thread_id: &str,
    entry: &ChainThreadEntry,
    snapshot: &ThreadSnapshot,
    events: &HashMap<String, EventPosition>,
) -> anyhow::Result<()> {
    validate_stored_snapshot(state, thread_id, entry, snapshot)?;
    if snapshot.last_chain_seq != state.last_chain_seq
        || snapshot.last_thread_seq != entry.last_thread_seq
        || snapshot.last_event_hash != entry.last_event_hash
    {
        anyhow::bail!(
            "new snapshot for thread {thread_id} does not carry its exact publication position"
        );
    }
    if parse_canonical_timestamp(&snapshot.updated_at)?
        > parse_canonical_timestamp(&state.updated_at)?
    {
        anyhow::bail!("snapshot for thread {thread_id} exceeds its publishing ChainState time");
    }
    validate_snapshot_event_position(snapshot, events)
}

fn validate_snapshot_event_position(
    snapshot: &ThreadSnapshot,
    events: &HashMap<String, EventPosition>,
) -> anyhow::Result<()> {
    let Some(hash) = snapshot.last_event_hash.as_deref() else {
        return Ok(());
    };
    let event = events
        .get(hash)
        .ok_or_else(|| anyhow::anyhow!("snapshot references event {hash} outside chain history"))?;
    if event.thread_id != snapshot.thread_id
        || event.thread_sequence != snapshot.last_thread_seq
        || event.chain_sequence > snapshot.last_chain_seq
    {
        anyhow::bail!("snapshot event identity or position mismatch at {hash}");
    }
    Ok(())
}

fn validate_entry_event_position(
    state: &ChainState,
    thread_id: &str,
    entry: &ChainThreadEntry,
    events: &HashMap<String, EventPosition>,
) -> anyhow::Result<()> {
    let Some(hash) = entry.last_event_hash.as_deref() else {
        return Ok(());
    };
    let event = events.get(hash).ok_or_else(|| {
        anyhow::anyhow!("thread {thread_id} entry references event {hash} outside chain history")
    })?;
    if event.thread_id != thread_id
        || event.thread_sequence != entry.last_thread_seq
        || event.chain_sequence > state.last_chain_seq
    {
        anyhow::bail!("thread {thread_id} entry event position mismatch at {hash}");
    }
    Ok(())
}

fn validate_historical_snapshot_transition(
    previous: &ThreadSnapshot,
    next: &ThreadSnapshot,
    appended_through: Option<&str>,
) -> anyhow::Result<()> {
    if appended_through.is_none() {
        anyhow::bail!("existing thread snapshot changed without an event append");
    }
    validate_snapshot_transition_identity(previous, next)?;
    next.validate()?;

    let previous_updated_at = parse_canonical_timestamp(&previous.updated_at)?;
    let next_updated_at = parse_canonical_timestamp(&next.updated_at)?;
    if next_updated_at < previous_updated_at {
        anyhow::bail!("snapshot updated_at moved backwards");
    }
    if previous.started_at.is_none()
        && next
            .started_at
            .as_deref()
            .map(parse_canonical_timestamp)
            .transpose()?
            .is_some_and(|started_at| started_at < previous_updated_at)
    {
        anyhow::bail!("new snapshot started_at precedes its predecessor update");
    }
    let appended_through = parse_canonical_timestamp(
        appended_through.expect("snapshot changes require an appended event timestamp"),
    )?;
    if next_updated_at < appended_through {
        anyhow::bail!("snapshot updated_at precedes its publishing event append");
    }
    if next
        .finished_at
        .as_deref()
        .map(parse_canonical_timestamp)
        .transpose()?
        .is_some_and(|finished_at| {
            finished_at < previous_updated_at || finished_at < appended_through
        })
    {
        anyhow::bail!("snapshot finished_at precedes its predecessor or publishing append");
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum EventHistory<'a> {
    Chain,
    Thread(&'a str),
}

fn validate_event_advance(
    events: &HashMap<String, EventPosition>,
    history: EventHistory<'_>,
    previous_hash: Option<&str>,
    previous_sequence: u64,
    next_hash: Option<&str>,
    next_sequence: u64,
    check: &mut dyn FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<Option<(String, String)>> {
    if next_sequence < previous_sequence {
        anyhow::bail!("event sequence moved backwards");
    }
    if next_sequence == previous_sequence {
        if next_hash != previous_hash {
            anyhow::bail!("event hash changed without advancing its sequence");
        }
        return Ok(None);
    }

    let mut sequence = next_sequence;
    let mut hash = next_hash.map(str::to_string);
    let mut newest = None;
    let mut oldest = None;
    while sequence > previous_sequence {
        check()?;
        let current_hash = hash
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("event history ended before sequence {sequence}"))?;
        let event = events.get(current_hash).ok_or_else(|| {
            anyhow::anyhow!("event {current_hash} is outside the authoritative chain history")
        })?;
        let (actual_sequence, predecessor) = match history {
            EventHistory::Chain => (event.chain_sequence, event.prev_chain_hash.as_deref()),
            EventHistory::Thread(thread_id) => {
                if event.thread_id != thread_id {
                    anyhow::bail!(
                        "thread {thread_id} history crosses into thread {} at {current_hash}",
                        event.thread_id
                    );
                }
                (event.thread_sequence, event.prev_thread_hash.as_deref())
            }
        };
        if actual_sequence != sequence {
            anyhow::bail!(
                "event {current_hash} sequence mismatch: expected {sequence}, got {actual_sequence}"
            );
        }
        if newest.is_none() {
            newest = Some(event.timestamp.clone());
        }
        oldest = Some(event.timestamp.clone());
        hash = predecessor.map(str::to_string);
        sequence -= 1;
    }
    if hash.as_deref() != previous_hash {
        anyhow::bail!("event advance does not join the predecessor history");
    }
    Ok(Some((
        oldest.expect("a positive sequence advance visits an event"),
        newest.expect("a positive sequence advance visits an event"),
    )))
}

fn load_hashed_chain_state(cas: &lillux::CasStore, hash: &str) -> anyhow::Result<ChainState> {
    let value = cas
        .get_object(hash)
        .with_context(|| format!("read authoritative ChainState object {hash}"))?
        .ok_or_else(|| anyhow::anyhow!("authoritative ChainState object {hash} is absent"))?;
    let state: ChainState = serde_json::from_value(value)
        .with_context(|| format!("decode authoritative ChainState object {hash}"))?;
    let canonical = lillux::canonical_json(&state.to_value())
        .with_context(|| format!("canonicalize authoritative ChainState {hash}"))?;
    let canonical_hash = lillux::sha256_hex(canonical.as_bytes());
    if canonical_hash != hash {
        anyhow::bail!(
            "authoritative ChainState is not canonically encoded: expected {hash}, canonical {canonical_hash}"
        );
    }
    Ok(state)
}

fn validate_predecessor_link(
    label: &str,
    sequence: u64,
    predecessor: Option<&str>,
) -> anyhow::Result<()> {
    match (sequence, predecessor) {
        (1, None) | (2.., Some(_)) => Ok(()),
        (0, _) => anyhow::bail!("{label} has sequence zero"),
        (1, Some(_)) => anyhow::bail!("{label} sequence one has a predecessor"),
        (2.., None) => anyhow::bail!("{label} sequence {sequence} is missing its predecessor"),
    }
}

fn load_hashed_snapshot(cas: &lillux::CasStore, hash: &str) -> anyhow::Result<ThreadSnapshot> {
    let value = cas
        .get_object(hash)
        .with_context(|| format!("read authoritative snapshot object {hash}"))?
        .ok_or_else(|| anyhow::anyhow!("authoritative snapshot object {hash} is absent"))?;
    let snapshot: ThreadSnapshot = serde_json::from_value(value)
        .with_context(|| format!("decode authoritative snapshot object {hash}"))?;
    let canonical = lillux::canonical_json(&snapshot.to_value())
        .with_context(|| format!("canonicalize authoritative snapshot {hash}"))?;
    let canonical_hash = lillux::sha256_hex(canonical.as_bytes());
    if canonical_hash != hash {
        anyhow::bail!(
            "authoritative snapshot is not canonically encoded: expected {hash}, canonical {canonical_hash}"
        );
    }
    Ok(snapshot)
}

fn load_hashed_event(cas: &lillux::CasStore, hash: &str) -> anyhow::Result<ThreadEvent> {
    let value = cas
        .get_object(hash)
        .with_context(|| format!("read authoritative event object {hash}"))?
        .ok_or_else(|| anyhow::anyhow!("authoritative event object {hash} is absent"))?;
    let event: ThreadEvent = serde_json::from_value(value)
        .with_context(|| format!("decode authoritative event object {hash}"))?;
    let canonical = lillux::canonical_json(&event.to_value())
        .with_context(|| format!("canonicalize authoritative event {hash}"))?;
    let canonical_hash = lillux::sha256_hex(canonical.as_bytes());
    if canonical_hash != hash {
        anyhow::bail!(
            "authoritative event is not canonically encoded: expected {hash}, canonical {canonical_hash}"
        );
    }
    Ok(event)
}

/// Validate the linkage represented directly by a chain-state entry.
pub(crate) fn validate_chain_positions(state: &ChainState) -> anyhow::Result<()> {
    validate_hash_sequence_pair(
        "chain",
        state.last_chain_seq,
        state.last_event_hash.as_deref(),
    )?;
    parse_canonical_timestamp(&state.updated_at)
        .with_context(|| "invalid chain_state.updated_at")?;

    for (thread_id, entry) in &state.threads {
        validate_hash_sequence_pair(
            &format!("thread {thread_id}"),
            entry.last_thread_seq,
            entry.last_event_hash.as_deref(),
        )?;
        if entry.last_thread_seq > state.last_chain_seq {
            anyhow::bail!(
                "thread {thread_id} sequence {} exceeds chain sequence {}",
                entry.last_thread_seq,
                state.last_chain_seq
            );
        }
    }
    Ok(())
}

fn validate_hash_sequence_pair(
    label: &str,
    sequence: u64,
    last_event_hash: Option<&str>,
) -> anyhow::Result<()> {
    match (sequence, last_event_hash) {
        (0, None) | (1.., Some(_)) => Ok(()),
        (0, Some(_)) => anyhow::bail!("{label} has an event hash at sequence zero"),
        (1.., None) => anyhow::bail!("{label} is missing its event hash at sequence {sequence}"),
    }
}

/// Validate a snapshot loaded through a current chain-state entry.
///
/// A snapshot may predate later event-only appends. When it represents the
/// entry's current thread sequence, however, its event hash must agree exactly.
pub(crate) fn validate_stored_snapshot(
    chain_state: &ChainState,
    thread_id: &str,
    entry: &ChainThreadEntry,
    snapshot: &ThreadSnapshot,
) -> anyhow::Result<()> {
    snapshot.validate()?;
    if snapshot.thread_id != thread_id {
        anyhow::bail!(
            "snapshot thread_id mismatch: entry is {thread_id}, snapshot is {}",
            snapshot.thread_id
        );
    }
    if snapshot.chain_root_id != chain_state.chain_root_id {
        anyhow::bail!(
            "snapshot chain_root_id mismatch: expected {}, got {}",
            chain_state.chain_root_id,
            snapshot.chain_root_id
        );
    }
    if snapshot.status != entry.status {
        anyhow::bail!(
            "snapshot status mismatch for {thread_id}: entry is {}, snapshot is {}",
            entry.status,
            snapshot.status
        );
    }
    if snapshot.last_chain_seq > chain_state.last_chain_seq {
        anyhow::bail!(
            "snapshot chain sequence {} exceeds authoritative chain sequence {}",
            snapshot.last_chain_seq,
            chain_state.last_chain_seq
        );
    }
    if snapshot.last_thread_seq > entry.last_thread_seq {
        anyhow::bail!(
            "snapshot thread sequence {} exceeds authoritative thread sequence {}",
            snapshot.last_thread_seq,
            entry.last_thread_seq
        );
    }
    if snapshot.last_thread_seq > snapshot.last_chain_seq {
        anyhow::bail!(
            "snapshot thread sequence {} exceeds its chain sequence {}",
            snapshot.last_thread_seq,
            snapshot.last_chain_seq
        );
    }
    validate_hash_sequence_pair(
        &format!("snapshot for thread {thread_id}"),
        snapshot.last_thread_seq,
        snapshot.last_event_hash.as_deref(),
    )?;
    if snapshot.last_thread_seq == entry.last_thread_seq
        && snapshot.last_event_hash != entry.last_event_hash
    {
        anyhow::bail!(
            "snapshot last_event_hash disagrees with the authoritative thread entry at sequence {}",
            entry.last_thread_seq
        );
    }
    Ok(())
}

pub(super) fn validate_snapshot_last_event(
    snapshot: &ThreadSnapshot,
    expected_hash: &str,
    event: &ThreadEvent,
) -> anyhow::Result<()> {
    event.validate()?;
    parse_canonical_timestamp(&event.ts).with_context(|| "invalid snapshot last event ts")?;
    if snapshot.last_event_hash.as_deref() != Some(expected_hash) {
        anyhow::bail!("snapshot does not name the event supplied as its last event");
    }
    if event.chain_root_id != snapshot.chain_root_id || event.thread_id != snapshot.thread_id {
        anyhow::bail!("snapshot last event belongs to a different chain or thread");
    }
    if event.thread_seq != snapshot.last_thread_seq {
        anyhow::bail!(
            "snapshot thread sequence {} disagrees with last event sequence {}",
            snapshot.last_thread_seq,
            event.thread_seq
        );
    }
    if event.chain_seq > snapshot.last_chain_seq {
        anyhow::bail!(
            "snapshot last event chain sequence {} exceeds snapshot chain sequence {}",
            event.chain_seq,
            snapshot.last_chain_seq
        );
    }
    Ok(())
}

pub(super) fn validate_update_identity(
    update_thread_id: &str,
    snapshot: &ThreadSnapshot,
    chain_root_id: &str,
) -> anyhow::Result<()> {
    if update_thread_id != snapshot.thread_id {
        anyhow::bail!(
            "snapshot update thread_id {update_thread_id} does not match snapshot thread_id {}",
            snapshot.thread_id
        );
    }
    if snapshot.chain_root_id != chain_root_id {
        anyhow::bail!(
            "snapshot update for {update_thread_id} belongs to chain {}, not {chain_root_id}",
            snapshot.chain_root_id
        );
    }
    Ok(())
}

/// Validate fields that a new version of an existing thread cannot rewrite.
/// Timestamp normalization and event-position stamping happen separately.
pub(super) fn validate_snapshot_transition_identity(
    previous: &ThreadSnapshot,
    next: &ThreadSnapshot,
) -> anyhow::Result<()> {
    macro_rules! immutable {
        ($field:ident) => {
            if previous.$field != next.$field {
                anyhow::bail!(
                    "snapshot transition cannot change immutable field {}",
                    stringify!($field)
                );
            }
        };
    }

    immutable!(schema);
    immutable!(kind);
    immutable!(thread_id);
    immutable!(chain_root_id);
    immutable!(kind_name);
    immutable!(item_ref);
    immutable!(executor_ref);
    immutable!(launch_mode);
    // `current_site_id` is deliberately mutable: it is execution location,
    // not origin/identity, and an admitted site transfer updates it.
    immutable!(origin_site_id);
    immutable!(upstream_thread_id);
    immutable!(requested_by);
    immutable!(project_root);
    immutable!(captured_history_policy);
    immutable!(created_at);

    validate_status_transition(previous.status, next.status)?;

    match (
        previous.base_project_snapshot_hash.as_ref(),
        next.base_project_snapshot_hash.as_ref(),
    ) {
        (left, right) if left == right => {}
        (None, Some(_))
            if previous.status == ThreadStatus::Created
                && next.status == ThreadStatus::Running => {}
        _ => anyhow::bail!(
            "base_project_snapshot_hash may only be established by created -> running and is immutable afterwards"
        ),
    }

    match (
        previous.result_project_snapshot_hash.as_ref(),
        next.result_project_snapshot_hash.as_ref(),
    ) {
        (left, right) if left == right => {}
        (None, Some(_)) if next.status.is_terminal() => {}
        _ => anyhow::bail!(
            "result_project_snapshot_hash may only be established by a terminal transition and is immutable afterwards"
        ),
    }

    if previous.started_at.is_some() && previous.started_at != next.started_at {
        anyhow::bail!("snapshot transition cannot change or clear started_at once established");
    }

    Ok(())
}

fn validate_status_transition(previous: ThreadStatus, next: ThreadStatus) -> anyhow::Result<()> {
    let valid = match previous {
        ThreadStatus::Created => next == ThreadStatus::Running || next.is_terminal(),
        ThreadStatus::Running => next.is_terminal(),
        status if status.is_terminal() => false,
        _ => false,
    };
    if !valid {
        anyhow::bail!("illegal thread status transition: {previous} -> {next}");
    }
    Ok(())
}

/// Clamp caller-created event times forward so wall-clock rollback cannot make
/// an otherwise valid local append non-monotonic.
pub(super) fn normalize_event_timestamps(
    events: &mut [ThreadEvent],
    floor: &str,
) -> anyhow::Result<String> {
    let mut cursor = floor.to_string();
    parse_canonical_timestamp(&cursor).with_context(|| "invalid event timestamp floor")?;
    for event in events {
        clamp_timestamp(&mut event.ts, &cursor, "event.ts")?;
        cursor.clone_from(&event.ts);
    }
    Ok(cursor)
}

/// Normalize the mutable lifecycle timestamps of a locally created successor
/// snapshot, then validate the complete authoritative transition.
pub(super) fn normalize_and_validate_snapshot_transition(
    previous: &ThreadSnapshot,
    next: &mut ThreadSnapshot,
    append_timestamp: &str,
) -> anyhow::Result<()> {
    validate_snapshot_transition_identity(previous, next)?;

    // Parse every supplied spelling before replacing an earlier value. Invalid
    // or non-canonical wire data remains an error; only clock rollback is fixed.
    clamp_timestamp(&mut next.updated_at, &previous.updated_at, "updated_at")?;

    if let Some(started_at) = next.started_at.as_mut() {
        clamp_timestamp(started_at, &next.created_at, "started_at")?;
        if previous.started_at.is_none() {
            clamp_timestamp(started_at, &previous.updated_at, "started_at")?;
        }
        clamp_timestamp(&mut next.updated_at, started_at, "updated_at")?;
    }

    if let Some(finished_at) = next.finished_at.as_mut() {
        clamp_timestamp(finished_at, &next.created_at, "finished_at")?;
        clamp_timestamp(finished_at, &previous.updated_at, "finished_at")?;
        clamp_timestamp(finished_at, append_timestamp, "finished_at")?;
        if let Some(started_at) = next.started_at.as_deref() {
            clamp_timestamp(finished_at, started_at, "finished_at")?;
        }
        clamp_timestamp(&mut next.updated_at, finished_at, "updated_at")?;
    }

    clamp_timestamp(&mut next.updated_at, append_timestamp, "updated_at")?;
    next.validate()?;
    Ok(())
}

/// Normalize a new thread's snapshot against the chain position at which it is
/// inserted. New authoritative threads always begin in `created` state. Only
/// an explicitly identified continuation successor may already carry the
/// immutable project pin inherited at its atomic handoff.
pub(super) fn normalize_and_validate_new_thread(
    snapshot: &mut ThreadSnapshot,
    chain_root_id: &str,
    timestamp_floor: &str,
    continuation_source: Option<&str>,
) -> anyhow::Result<()> {
    if snapshot.chain_root_id != chain_root_id {
        anyhow::bail!(
            "snapshot chain_root_id mismatch: expected {chain_root_id}, got {}",
            snapshot.chain_root_id
        );
    }
    if snapshot.status != ThreadStatus::Created {
        anyhow::bail!(
            "new thread snapshot must start in created status, got {}",
            snapshot.status
        );
    }
    validate_new_thread_project_snapshots(snapshot, continuation_source)?;
    parse_canonical_timestamp(&snapshot.created_at)
        .with_context(|| "invalid new snapshot created_at")?;
    clamp_timestamp(&mut snapshot.updated_at, &snapshot.created_at, "updated_at")?;
    clamp_timestamp(&mut snapshot.updated_at, timestamp_floor, "updated_at")?;
    snapshot.validate()?;
    Ok(())
}

pub(super) fn stamp_snapshot_position(
    snapshot: &mut ThreadSnapshot,
    entry: &ChainThreadEntry,
    chain_sequence: u64,
) {
    snapshot.last_event_hash.clone_from(&entry.last_event_hash);
    snapshot.last_thread_seq = entry.last_thread_seq;
    snapshot.last_chain_seq = chain_sequence;
}

/// Produce a canonical local chain timestamp no earlier than any authoritative
/// input. Inputs are validated even when another value is later.
pub(super) fn monotonic_now<'a>(
    floors: impl IntoIterator<Item = &'a str>,
) -> anyhow::Result<String> {
    let mut result = lillux::time::iso8601_now();
    let mut result_time = parse_canonical_timestamp(&result)
        .with_context(|| "local clock produced a non-canonical timestamp")?;
    for floor in floors {
        let floor_time = parse_canonical_timestamp(floor)
            .with_context(|| format!("invalid monotonic timestamp floor {floor:?}"))?;
        if floor_time > result_time {
            result = floor.to_string();
            result_time = floor_time;
        }
    }
    Ok(result)
}

fn clamp_timestamp(value: &mut String, floor: &str, label: &str) -> anyhow::Result<()> {
    let value_time =
        parse_canonical_timestamp(value).with_context(|| format!("invalid {label} timestamp"))?;
    let floor_time = parse_canonical_timestamp(floor)
        .with_context(|| format!("invalid {label} timestamp floor"))?;
    if value_time < floor_time {
        value.clear();
        value.push_str(floor);
    }
    Ok(())
}

/// Assert that a prospective thread map preserves every untouched entry.
pub(super) fn validate_untouched_entries(
    previous: &BTreeMap<String, ChainThreadEntry>,
    next: &BTreeMap<String, ChainThreadEntry>,
    updated_threads: impl IntoIterator<Item = String>,
) -> anyhow::Result<()> {
    let mut expected = previous.clone();
    for thread_id in updated_threads {
        expected.remove(&thread_id);
    }
    for (thread_id, expected_entry) in expected {
        let actual = next
            .get(&thread_id)
            .ok_or_else(|| anyhow::anyhow!("prospective chain removed thread {thread_id}"))?;
        if actual.snapshot_hash != expected_entry.snapshot_hash
            || actual.last_event_hash != expected_entry.last_event_hash
            || actual.last_thread_seq != expected_entry.last_thread_seq
            || actual.status != expected_entry.status
        {
            anyhow::bail!("prospective chain changed untouched thread {thread_id}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::thread_event::NewEvent;
    use crate::objects::thread_snapshot::{
        CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
        CapturedThreadHistoryPolicy, ThreadHistoryRetention, ThreadSnapshotBuilder,
    };

    fn child(status: ThreadStatus, updated_at: &str) -> ThreadSnapshot {
        let mut snapshot = ThreadSnapshotBuilder::new(
            "T-child",
            "T-root",
            "agent",
            "directive:test",
            "native:test",
        )
        .created_at("2026-01-01T00:00:00Z".into())
        .updated_at(updated_at.into())
        .status(status)
        .build();
        match status {
            ThreadStatus::Running => {
                snapshot.started_at = Some(updated_at.into());
            }
            status if status.is_terminal() => {
                snapshot.finished_at = Some(updated_at.into());
            }
            _ => {}
        }
        snapshot
    }

    fn root() -> ThreadSnapshot {
        let hash = "11".repeat(32);
        ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "directive",
            "directive:test",
            "native:test",
        )
        .created_at("2026-01-01T00:00:00Z".into())
        .updated_at("2026-01-01T00:00:00Z".into())
        .captured_history_policy(Some(CapturedThreadHistoryPolicy {
            retention: ThreadHistoryRetention::Durable,
            canonical_item_ref: "directive:test".into(),
            item_content_hash: hash.clone(),
            item_signer_fingerprint: Some(hash.clone()),
            item_trust_class: CapturedItemTrustClass::Trusted,
            kind_schema_content_hash: hash,
            resolved_from: CapturedPolicyProvenance::NodeDefault {
                node_policy: CapturedNodeHistoryPolicyProvenance::MissingConfig,
            },
        }))
        .build()
    }

    fn write_value(cas_root: &Path, value: serde_json::Value) -> String {
        let canonical = lillux::canonical_json(&value).unwrap();
        let hash = lillux::sha256_hex(canonical.as_bytes());
        let path = lillux::shard_path(cas_root, "objects", &hash, ".json");
        lillux::atomic_write(&path, canonical.as_bytes()).unwrap();
        hash
    }

    fn valid_two_state_history(cas_root: &Path) -> (String, String) {
        let created = root();
        let created_hash = write_value(cas_root, created.to_value());
        let genesis = ChainState {
            schema: 1,
            kind: "chain_state".into(),
            chain_root_id: "T-root".into(),
            prev_chain_state_hash: None,
            last_event_hash: None,
            last_chain_seq: 0,
            updated_at: "2026-01-01T00:00:00Z".into(),
            threads: BTreeMap::from([(
                "T-root".into(),
                ChainThreadEntry {
                    snapshot_hash: created_hash,
                    last_event_hash: None,
                    last_thread_seq: 0,
                    status: ThreadStatus::Created,
                },
            )]),
        };
        let genesis_hash = write_value(cas_root, genesis.to_value());

        let mut event = NewEvent::new("T-root", "T-root", "thread_started")
            .chain_seq(1)
            .thread_seq(1)
            .build();
        event.ts = "2026-01-01T00:00:01Z".into();
        let event_hash = write_value(cas_root, event.to_value());

        let mut running = created;
        running.status = ThreadStatus::Running;
        running.updated_at = "2026-01-01T00:00:01Z".into();
        running.started_at = Some("2026-01-01T00:00:01Z".into());
        running.last_event_hash = Some(event_hash.clone());
        running.last_chain_seq = 1;
        running.last_thread_seq = 1;
        let running_hash = write_value(cas_root, running.to_value());
        let running_state = ChainState {
            schema: 1,
            kind: "chain_state".into(),
            chain_root_id: "T-root".into(),
            prev_chain_state_hash: Some(genesis_hash.clone()),
            last_event_hash: Some(event_hash.clone()),
            last_chain_seq: 1,
            updated_at: "2026-01-01T00:00:01Z".into(),
            threads: BTreeMap::from([(
                "T-root".into(),
                ChainThreadEntry {
                    snapshot_hash: running_hash,
                    last_event_hash: Some(event_hash),
                    last_thread_seq: 1,
                    status: ThreadStatus::Running,
                },
            )]),
        };
        let running_state_hash = write_value(cas_root, running_state.to_value());
        (genesis_hash, running_state_hash)
    }

    #[test]
    fn created_can_transition_directly_to_every_terminal_status() {
        let previous = child(ThreadStatus::Created, "2026-01-01T00:00:00Z");
        for status in [
            ThreadStatus::Completed,
            ThreadStatus::Failed,
            ThreadStatus::Cancelled,
            ThreadStatus::Killed,
            ThreadStatus::TimedOut,
            ThreadStatus::Continued,
        ] {
            let mut next = child(status, "2026-01-01T00:00:01Z");
            normalize_and_validate_snapshot_transition(
                &previous,
                &mut next,
                "2026-01-01T00:00:01Z",
            )
            .unwrap();
        }
    }

    #[test]
    fn terminal_snapshot_cannot_transition_again() {
        let previous = child(ThreadStatus::Failed, "2026-01-01T00:00:01Z");
        let mut next = child(ThreadStatus::Completed, "2026-01-01T00:00:02Z");
        assert!(normalize_and_validate_snapshot_transition(
            &previous,
            &mut next,
            "2026-01-01T00:00:02Z"
        )
        .is_err());
    }

    #[test]
    fn immutable_identity_and_update_id_are_enforced() {
        let previous = child(ThreadStatus::Created, "2026-01-01T00:00:00Z");
        let mut next = child(ThreadStatus::Running, "2026-01-01T00:00:01Z");
        next.item_ref = "directive:other".into();
        assert!(validate_snapshot_transition_identity(&previous, &next).is_err());
        assert!(validate_update_identity("T-other", &next, "T-root").is_err());
    }

    #[test]
    fn base_snapshot_is_set_once_only_on_created_to_running() {
        let previous = child(ThreadStatus::Created, "2026-01-01T00:00:00Z");
        let mut running = child(ThreadStatus::Running, "2026-01-01T00:00:01Z");
        running.base_project_snapshot_hash = Some("11".repeat(32));
        validate_snapshot_transition_identity(&previous, &running).unwrap();

        let mut terminal = child(ThreadStatus::Completed, "2026-01-01T00:00:02Z");
        terminal.started_at = running.started_at.clone();
        assert!(validate_snapshot_transition_identity(&running, &terminal).is_err());
        terminal.base_project_snapshot_hash = running.base_project_snapshot_hash.clone();
        validate_snapshot_transition_identity(&running, &terminal).unwrap();
    }

    #[test]
    fn created_continuation_may_inherit_base_snapshot() {
        let mut continuation = child(ThreadStatus::Created, "2026-01-01T00:00:00Z");
        continuation.upstream_thread_id = Some("T-root".into());
        continuation.base_project_snapshot_hash = Some("11".repeat(32));

        normalize_and_validate_new_thread(
            &mut continuation,
            "T-root",
            "2026-01-01T00:00:00Z",
            Some("T-root"),
        )
        .unwrap();

        let mut running = continuation.clone();
        running.status = ThreadStatus::Running;
        running.updated_at = "2026-01-01T00:00:01Z".into();
        running.started_at = Some("2026-01-01T00:00:01Z".into());
        validate_snapshot_transition_identity(&continuation, &running).unwrap();
    }

    #[test]
    fn created_non_continuation_cannot_carry_project_snapshot_hashes() {
        let mut fresh = child(ThreadStatus::Created, "2026-01-01T00:00:00Z");
        fresh.base_project_snapshot_hash = Some("11".repeat(32));
        let error =
            normalize_and_validate_new_thread(&mut fresh, "T-root", "2026-01-01T00:00:00Z", None)
                .unwrap_err();
        assert!(error
            .to_string()
            .contains("only for a continuation successor"));

        let mut continuation = child(ThreadStatus::Created, "2026-01-01T00:00:00Z");
        continuation.upstream_thread_id = Some("T-root".into());
        continuation.result_project_snapshot_hash = Some("22".repeat(32));
        let error = normalize_and_validate_new_thread(
            &mut continuation,
            "T-root",
            "2026-01-01T00:00:00Z",
            Some("T-root"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot carry a result"));
    }

    #[test]
    fn result_snapshot_is_set_only_by_terminal_transition() {
        let previous = child(ThreadStatus::Created, "2026-01-01T00:00:00Z");
        let mut running = child(ThreadStatus::Running, "2026-01-01T00:00:01Z");
        running.result_project_snapshot_hash = Some("22".repeat(32));
        assert!(validate_snapshot_transition_identity(&previous, &running).is_err());

        let mut terminal = child(ThreadStatus::Completed, "2026-01-01T00:00:01Z");
        terminal.result_project_snapshot_hash = Some("22".repeat(32));
        validate_snapshot_transition_identity(&previous, &terminal).unwrap();
    }

    #[test]
    fn local_clock_rollback_is_clamped_forward() {
        let previous = child(ThreadStatus::Running, "2026-01-01T00:00:10Z");
        let mut next = child(ThreadStatus::Completed, "2026-01-01T00:00:05Z");
        next.created_at.clone_from(&previous.created_at);
        next.started_at.clone_from(&previous.started_at);
        normalize_and_validate_snapshot_transition(&previous, &mut next, "2026-01-01T00:00:11Z")
            .unwrap();
        assert_eq!(next.finished_at.as_deref(), Some("2026-01-01T00:00:11Z"));
        assert_eq!(next.updated_at, "2026-01-01T00:00:11Z");
    }

    #[test]
    fn position_stamping_overrides_placeholder_linkage() {
        let mut snapshot = child(ThreadStatus::Created, "2026-01-01T00:00:00Z");
        let entry = ChainThreadEntry {
            snapshot_hash: "22".repeat(32),
            last_event_hash: Some("33".repeat(32)),
            last_thread_seq: 7,
            status: ThreadStatus::Created,
        };
        stamp_snapshot_position(&mut snapshot, &entry, 11);
        assert_eq!(snapshot.last_event_hash, entry.last_event_hash);
        assert_eq!(snapshot.last_thread_seq, 7);
        assert_eq!(snapshot.last_chain_seq, 11);
    }

    #[test]
    fn authoritative_history_accepts_legal_transition_and_exact_anchor() {
        let temp = tempfile::tempdir().unwrap();
        let (genesis_hash, target_hash) = valid_two_state_history(temp.path());
        validate_authoritative_history(temp.path(), "T-root", &target_hash, Some(&genesis_hash))
            .unwrap();
    }

    #[test]
    fn authoritative_history_rejects_a_fork_from_the_current_head() {
        let temp = tempfile::tempdir().unwrap();
        let (_, target_hash) = valid_two_state_history(temp.path());
        let fork_hash = "ff".repeat(32);
        let error =
            validate_authoritative_history(temp.path(), "T-root", &target_hash, Some(&fork_hash))
                .unwrap_err();
        assert!(error.to_string().contains("does not advance"));
    }

    #[test]
    fn authoritative_history_rejects_immutable_snapshot_rewrite() {
        let temp = tempfile::tempdir().unwrap();
        let (_, target_hash) = valid_two_state_history(temp.path());
        let cas = lillux::CasStore::new(temp.path().to_path_buf());
        let mut target = load_hashed_chain_state(&cas, &target_hash).unwrap();
        let root_entry = target.threads.get("T-root").unwrap().clone();
        let mut snapshot = load_hashed_snapshot(&cas, &root_entry.snapshot_hash).unwrap();
        snapshot.item_ref = "directive:rewritten".into();
        let rewritten_snapshot_hash = write_value(temp.path(), snapshot.to_value());
        target.threads.get_mut("T-root").unwrap().snapshot_hash = rewritten_snapshot_hash;
        let rewritten_target_hash = write_value(temp.path(), target.to_value());

        let error =
            validate_authoritative_history(temp.path(), "T-root", &rewritten_target_hash, None)
                .unwrap_err();
        assert!(format!("{error:#}").contains("immutable field item_ref"));
    }
}
