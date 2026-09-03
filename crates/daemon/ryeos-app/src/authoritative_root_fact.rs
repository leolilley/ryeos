//! Bounded acceleration for exact facts in an authoritative root event chain.
//!
//! The cache is derived only from complete/incremental root replay. Its Bloom
//! filter proves absence only; possible hits outside the bounded exact cache
//! fall back to complete replay. Mutable projections never grant authority.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, OnceLock, Weak};

use anyhow::{Result, anyhow, bail};
use serde_json::Value;

use crate::state::AppState;
use crate::state_store::NewEventRecord;

const CACHE_ROOTS: usize = 16;
const RECENT_FACTS: usize = 512;
const BLOOM_WORDS: usize = 131_072;
const BLOOM_HASHES: usize = 6;
const CACHED_PAYLOAD_BYTES: usize = 4 * 1024;

fn operation_lock(root_thread_id: &str) -> Arc<std::sync::Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Weak<std::sync::Mutex<()>>>>> =
        OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .expect("authoritative root fact lock map poisoned");
    locks.retain(|_, lock| lock.strong_count() != 0);
    if let Some(lock) = locks.get(root_thread_id).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(std::sync::Mutex::new(()));
    locks.insert(root_thread_id.to_owned(), Arc::downgrade(&lock));
    lock
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct FactKey {
    event_type: String,
    operation_id: String,
}

#[derive(Clone)]
struct CachedFact {
    count: u8,
    payload_digest: String,
    payload: Option<Value>,
    first_chain_seq: i64,
    last_chain_seq: i64,
    /// True only after a complete replay, or cache-proven absence followed by
    /// replay of the entire unseen tail, counted this exact key.
    complete: bool,
}

struct ReplayIndex {
    initialized: bool,
    verified_through: Option<i64>,
    bloom: Vec<u64>,
    recent: HashMap<FactKey, CachedFact>,
    recent_order: VecDeque<FactKey>,
    last_used: u64,
}

impl Default for ReplayIndex {
    fn default() -> Self {
        Self {
            initialized: false,
            verified_through: None,
            bloom: vec![0; BLOOM_WORDS],
            recent: HashMap::new(),
            recent_order: VecDeque::new(),
            last_used: 0,
        }
    }
}

impl ReplayIndex {
    fn bloom_positions(key: &FactKey) -> [usize; BLOOM_HASHES] {
        let digest =
            lillux::sha256_hex(format!("{}\0{}", key.event_type, key.operation_id).as_bytes());
        std::array::from_fn(|index| {
            let offset = index * 8;
            let word =
                u32::from_str_radix(&digest[offset..offset + 8], 16).expect("sha256 hex chunk");
            usize::try_from(word).expect("u32 fits usize") % (BLOOM_WORDS * u64::BITS as usize)
        })
    }

    fn bloom_may_contain(&self, key: &FactKey) -> bool {
        Self::bloom_positions(key).into_iter().all(|position| {
            self.bloom[position / u64::BITS as usize] & (1_u64 << (position % u64::BITS as usize))
                != 0
        })
    }

    fn bloom_insert(&mut self, key: &FactKey) {
        for position in Self::bloom_positions(key) {
            self.bloom[position / u64::BITS as usize] |= 1_u64 << (position % u64::BITS as usize);
        }
    }

    fn remember_exact(&mut self, key: FactKey, fact: CachedFact) {
        if let std::collections::hash_map::Entry::Occupied(mut entry) =
            self.recent.entry(key.clone())
        {
            entry.insert(fact);
            return;
        }
        while self.recent.len() >= RECENT_FACTS {
            if let Some(oldest) = self.recent_order.pop_front() {
                self.recent.remove(&oldest);
            }
        }
        self.recent_order.push_back(key.clone());
        self.recent.insert(key, fact);
    }

    fn observe(&mut self, event_type: &str, payload: &Value, chain_seq: i64) -> Result<()> {
        let Some(operation_id) = payload.get("operation_id").and_then(Value::as_str) else {
            return Ok(());
        };
        let key = FactKey {
            event_type: event_type.to_owned(),
            operation_id: operation_id.to_owned(),
        };
        self.bloom_insert(&key);
        let canonical = lillux::canonical_json(payload)?;
        let payload_digest = lillux::sha256_hex(canonical.as_bytes());
        let cached_payload = (canonical.len() <= CACHED_PAYLOAD_BYTES).then(|| payload.clone());
        if let Some(existing) = self.recent.get_mut(&key) {
            existing.count = existing.count.saturating_add(1);
            existing.first_chain_seq = existing.first_chain_seq.min(chain_seq);
            existing.last_chain_seq = existing.last_chain_seq.max(chain_seq);
            if existing.payload_digest != payload_digest {
                existing.payload = None;
            }
            return Ok(());
        }
        self.remember_exact(
            key,
            CachedFact {
                count: 1,
                payload_digest,
                payload: cached_payload,
                first_chain_seq: chain_seq,
                last_chain_seq: chain_seq,
                complete: false,
            },
        );
        Ok(())
    }
}

#[derive(Default)]
struct ReplayCache {
    clock: u64,
    roots: HashMap<String, ReplayIndex>,
}

/// Exact result of authoritative root replay for one event/operation identity.
#[derive(Clone, Debug)]
pub struct RootFactLookup {
    pub count: u8,
    pub payload_digest: Option<String>,
    pub payload: Option<Value>,
    pub first_chain_seq: Option<i64>,
    pub last_chain_seq: Option<i64>,
}

fn replay_cache() -> &'static std::sync::Mutex<ReplayCache> {
    static CACHE: OnceLock<std::sync::Mutex<ReplayCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn take_index(root_thread_id: &str) -> ReplayIndex {
    replay_cache()
        .lock()
        .expect("authoritative root replay cache poisoned")
        .roots
        .remove(root_thread_id)
        .unwrap_or_default()
}

fn put_index(root_thread_id: &str, mut index: ReplayIndex) {
    let mut cache = replay_cache()
        .lock()
        .expect("authoritative root replay cache poisoned");
    cache.clock = cache.clock.wrapping_add(1);
    index.last_used = cache.clock;
    while cache.roots.len() >= CACHE_ROOTS {
        let Some(oldest) = cache
            .roots
            .iter()
            .min_by_key(|(_, candidate)| candidate.last_used)
            .map(|(root, _)| root.clone())
        else {
            break;
        };
        cache.roots.remove(&oldest);
    }
    cache.roots.insert(root_thread_id.to_owned(), index);
}

fn scan_tail(
    state: &AppState,
    chain_root_id: &str,
    root_thread_id: &str,
    index: &mut ReplayIndex,
    requested: &FactKey,
) -> Result<RootFactLookup> {
    let mut after = index.verified_through;
    let mut matching_count = 0_u8;
    let mut matching_digest = None;
    let mut matching_payload = None;
    let mut first_chain_seq = None;
    let mut last_chain_seq = None;
    loop {
        let page = state.state_store.replay_events(
            chain_root_id,
            Some(root_thread_id),
            after,
            1024,
            8 * 1024 * 1024,
        )?;
        for event in &page.events {
            if event.chain_root_id != chain_root_id || event.thread_id != root_thread_id {
                bail!("authoritative root replay returned an event outside the requested root");
            }
            if event.event_type == requested.event_type
                && event.payload.get("operation_id").and_then(Value::as_str)
                    == Some(requested.operation_id.as_str())
            {
                let canonical = lillux::canonical_json(&event.payload)?;
                matching_count = matching_count.saturating_add(1);
                matching_digest = Some(lillux::sha256_hex(canonical.as_bytes()));
                matching_payload = Some(event.payload.clone());
                first_chain_seq.get_or_insert(event.chain_seq);
                last_chain_seq = Some(event.chain_seq);
            }
            index.observe(&event.event_type, &event.payload, event.chain_seq)?;
        }
        if let Some(last) = page.events.last() {
            after = Some(last.chain_seq);
            index.verified_through = after;
        }
        if !page.has_more {
            break;
        }
    }
    index.initialized = true;
    Ok(RootFactLookup {
        count: matching_count,
        payload_digest: matching_digest,
        payload: matching_payload,
        first_chain_seq,
        last_chain_seq,
    })
}

fn replay_exact(
    state: &AppState,
    chain_root_id: &str,
    root_thread_id: &str,
    requested: &FactKey,
) -> Result<RootFactLookup> {
    let mut after = None;
    let mut lookup = RootFactLookup {
        count: 0,
        payload_digest: None,
        payload: None,
        first_chain_seq: None,
        last_chain_seq: None,
    };
    loop {
        let page = state.state_store.replay_events(
            chain_root_id,
            Some(root_thread_id),
            after,
            1024,
            8 * 1024 * 1024,
        )?;
        for event in &page.events {
            if event.chain_root_id != chain_root_id || event.thread_id != root_thread_id {
                bail!("authoritative root replay returned an event outside the requested root");
            }
            if event.event_type != requested.event_type
                || event.payload.get("operation_id").and_then(Value::as_str)
                    != Some(requested.operation_id.as_str())
            {
                continue;
            }
            let canonical = lillux::canonical_json(&event.payload)?;
            lookup.count = lookup.count.saturating_add(1);
            lookup.payload_digest = Some(lillux::sha256_hex(canonical.as_bytes()));
            lookup.payload = Some(event.payload.clone());
            lookup.first_chain_seq.get_or_insert(event.chain_seq);
            lookup.last_chain_seq = Some(event.chain_seq);
        }
        after = page.events.last().map(|event| event.chain_seq);
        if !page.has_more {
            return Ok(lookup);
        }
    }
}

fn lookup_under_lock(
    state: &AppState,
    chain_root_id: &str,
    root_thread_id: &str,
    event_type: &str,
    operation_id: &str,
) -> Result<RootFactLookup> {
    let key = FactKey {
        event_type: event_type.to_owned(),
        operation_id: operation_id.to_owned(),
    };
    let mut index = take_index(root_thread_id);
    let initialized = index.initialized;
    let exact_before = index.recent.get(&key).filter(|fact| fact.complete).cloned();
    let may_contain_before = index.bloom_may_contain(&key);
    let tail = scan_tail(state, chain_root_id, root_thread_id, &mut index, &key);
    let mut lookup = match tail {
        Ok(tail) if !initialized => tail,
        Ok(tail) => match exact_before {
            Some(mut exact) => {
                exact.count = exact.count.saturating_add(tail.count);
                if tail.count != 0 {
                    if tail.payload_digest.as_ref() != Some(&exact.payload_digest) {
                        exact.payload = None;
                    }
                    exact.payload_digest = tail
                        .payload_digest
                        .expect("matching replay has a payload digest");
                    exact.payload = tail.payload;
                    exact.first_chain_seq = exact.first_chain_seq.min(
                        tail.first_chain_seq
                            .expect("matching replay has a chain sequence"),
                    );
                    exact.last_chain_seq = exact.last_chain_seq.max(
                        tail.last_chain_seq
                            .expect("matching replay has a chain sequence"),
                    );
                }
                RootFactLookup {
                    count: exact.count,
                    payload_digest: Some(exact.payload_digest),
                    payload: exact.payload,
                    first_chain_seq: Some(exact.first_chain_seq),
                    last_chain_seq: Some(exact.last_chain_seq),
                }
            }
            None if !may_contain_before => tail,
            None => replay_exact(state, chain_root_id, root_thread_id, &key)?,
        },
        Err(error) => return Err(error),
    };
    if lookup.count != 0 {
        let cached_payload = lookup
            .payload
            .as_ref()
            .map(lillux::canonical_json)
            .transpose()?
            .filter(|canonical| canonical.len() <= CACHED_PAYLOAD_BYTES)
            .and(lookup.payload.clone());
        index.bloom_insert(&key);
        index.remember_exact(
            key,
            CachedFact {
                count: lookup.count,
                payload_digest: lookup
                    .payload_digest
                    .clone()
                    .expect("present fact has a payload digest"),
                payload: cached_payload,
                first_chain_seq: lookup
                    .first_chain_seq
                    .expect("present fact has a first chain sequence"),
                last_chain_seq: lookup
                    .last_chain_seq
                    .expect("present fact has a last chain sequence"),
                complete: true,
            },
        );
    }
    put_index(root_thread_id, index);
    if lookup.count > 1 {
        lookup.payload = None;
    }
    Ok(lookup)
}

/// Look up exact root testimony while serializing against same-root appends.
pub fn lookup(
    state: &AppState,
    root_thread_id: &str,
    event_type: &str,
    operation_id: &str,
) -> Result<RootFactLookup> {
    let lock = operation_lock(root_thread_id);
    let _guard = lock
        .lock()
        .map_err(|_| anyhow!("authoritative root fact lock poisoned"))?;
    let thread = state
        .state_store
        .get_thread(root_thread_id)?
        .ok_or_else(|| anyhow!("hosted execution root thread disappeared"))?;
    let mut lookup = lookup_under_lock(
        state,
        &thread.chain_root_id,
        &thread.thread_id,
        event_type,
        operation_id,
    )?;
    if lookup.count == 1 && lookup.payload.is_none() {
        lookup = replay_exact(
            state,
            &thread.chain_root_id,
            &thread.thread_id,
            &FactKey {
                event_type: event_type.to_owned(),
                operation_id: operation_id.to_owned(),
            },
        )?;
    }
    Ok(lookup)
}

/// Append one canonical root fact exactly once under the same replay/append
/// lock. An operation-id retry must carry byte-equivalent canonical testimony.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppendOnceOutcome {
    Appended,
    AlreadyPresent,
}

pub fn append_once(
    state: &AppState,
    root_thread_id: &str,
    event_type: &str,
    operation_id: &str,
    payload: Value,
) -> Result<()> {
    append_once_with_followups(
        state,
        root_thread_id,
        event_type,
        operation_id,
        payload,
        &[],
    )
    .map(|_| ())
}

/// Append one exact recovery fact to a successor that has been durably
/// created but is intentionally not runnable yet. This is the narrow form
/// used when external evidence must settle a local waiter before that
/// successor can start. Ordinary live-root testimony continues to use
/// [`append_once`].
pub fn append_once_to_created_thread(
    state: &AppState,
    thread_id: &str,
    event_type: &str,
    operation_id: &str,
    mut payload: Value,
) -> Result<AppendOnceOutcome> {
    let lock = operation_lock(thread_id);
    let _guard = lock
        .lock()
        .map_err(|_| anyhow!("authoritative root fact lock poisoned"))?;
    let thread = state
        .state_store
        .get_thread(thread_id)?
        .ok_or_else(|| anyhow!("recovery fact thread disappeared"))?;
    payload
        .as_object_mut()
        .ok_or_else(|| anyhow!("authoritative recovery fact payload is not an object"))?
        .insert(
            "operation_id".to_owned(),
            Value::String(operation_id.to_owned()),
        );
    let fact = lookup_under_lock(
        state,
        &thread.chain_root_id,
        &thread.thread_id,
        event_type,
        operation_id,
    )?;
    if fact.count > 1 {
        bail!("recovery fact operation is duplicated in the authoritative chain");
    }
    let expected_digest = ryeos_state::objects::canonical_value_digest(&payload)?;
    if let Some(existing_digest) = fact.payload_digest {
        if existing_digest != expected_digest {
            bail!("recovery fact operation id is bound to contradictory canonical testimony");
        }
        return Ok(AppendOnceOutcome::AlreadyPresent);
    }
    if thread.status != ryeos_state::objects::ThreadStatus::Created.as_str() {
        bail!("recovery fact target is not an unstarted successor");
    }
    state.state_store.append_events(
        &thread.chain_root_id,
        &thread.thread_id,
        &[NewEventRecord {
            event_type: event_type.to_owned(),
            storage_class: "indexed".to_owned(),
            payload,
        }],
    )?;
    Ok(AppendOnceOutcome::Appended)
}

/// Atomically append a canonical idempotence fact and its associated events.
/// Follow-ups are emitted only for the first authoritative append.
pub fn append_once_with_followups(
    state: &AppState,
    root_thread_id: &str,
    event_type: &str,
    operation_id: &str,
    mut payload: Value,
    followups: &[NewEventRecord],
) -> Result<AppendOnceOutcome> {
    let lock = operation_lock(root_thread_id);
    let _guard = lock
        .lock()
        .map_err(|_| anyhow!("authoritative root fact lock poisoned"))?;
    let thread = state
        .state_store
        .get_thread(root_thread_id)?
        .ok_or_else(|| anyhow!("hosted execution root thread disappeared"))?;
    payload
        .as_object_mut()
        .ok_or_else(|| anyhow!("authoritative root fact payload is not an object"))?
        .insert(
            "operation_id".to_owned(),
            Value::String(operation_id.to_owned()),
        );
    let fact = lookup_under_lock(
        state,
        &thread.chain_root_id,
        &thread.thread_id,
        event_type,
        operation_id,
    )?;
    if fact.count > 1 {
        bail!("root fact operation is duplicated in the authoritative chain");
    }
    let expected_digest = ryeos_state::objects::canonical_value_digest(&payload)?;
    if let Some(existing_digest) = fact.payload_digest {
        if existing_digest != expected_digest {
            bail!("root fact operation id is bound to contradictory canonical testimony");
        }
        return Ok(AppendOnceOutcome::AlreadyPresent);
    }
    let mut events = Vec::with_capacity(1 + followups.len());
    events.push(NewEventRecord {
        event_type: event_type.to_owned(),
        storage_class: "indexed".to_owned(),
        payload,
    });
    events.extend_from_slice(followups);
    state
        .threads
        .append_thread_events(&thread.chain_root_id, &thread.thread_id, &events)?
        .ok_or_else(|| anyhow!("hosted execution root is no longer running"))?;
    Ok(AppendOnceOutcome::Appended)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replay_index_is_bounded_and_incidental_hits_are_not_authority() {
        let mut index = ReplayIndex::default();
        let key = FactKey {
            event_type: "hosted.test".to_owned(),
            operation_id: "a".repeat(64),
        };
        assert!(!index.bloom_may_contain(&key));

        let payload = json!({"operation_id":key.operation_id,"value":"observed"});
        index.observe(&key.event_type, &payload, 7).unwrap();
        assert!(index.bloom_may_contain(&key));
        assert!(!index.recent.get(&key).unwrap().complete);

        let canonical = lillux::canonical_json(&payload).unwrap();
        index.remember_exact(
            key.clone(),
            CachedFact {
                count: 1,
                payload_digest: lillux::sha256_hex(canonical.as_bytes()),
                payload: Some(payload),
                first_chain_seq: 7,
                last_chain_seq: 7,
                complete: true,
            },
        );
        index
            .observe(
                &key.event_type,
                &json!({"operation_id":key.operation_id,"value":"duplicate"}),
                9,
            )
            .unwrap();
        let duplicate = index.recent.get(&key).unwrap();
        assert_eq!(duplicate.count, 2);
        assert!(duplicate.complete);
        assert!(duplicate.payload.is_none());

        for ordinal in 0..=RECENT_FACTS {
            index
                .observe(
                    "hosted.bounded",
                    &json!({"operation_id":format!("{ordinal:064x}"),"ordinal":ordinal}),
                    i64::try_from(ordinal).unwrap() + 10,
                )
                .unwrap();
        }
        assert_eq!(index.recent.len(), RECENT_FACTS);
        assert_eq!(index.recent_order.len(), RECENT_FACTS);
    }
}
