//! A count-bounded least-recently-used map shared by process-local caches.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::Mutex;

#[derive(Default)]
struct BoundedLruState<K, V> {
    entries: HashMap<K, V>,
    order: VecDeque<K>,
}

/// Count-bounded LRU. Deliberately count- rather than
/// byte-bounded: entries retain full source closures, so the cap trades a
/// bounded number of large entries for eviction simplicity on a
/// single-operator node.
#[derive(Default)]
pub struct BoundedLru<K, V> {
    state: Mutex<BoundedLruState<K, V>>,
    max_entries: usize,
}

impl<K: Clone + Eq + Hash, V: Clone> BoundedLru<K, V> {
    pub fn with_capacity(max_entries: usize) -> Self {
        Self {
            state: Mutex::new(BoundedLruState {
                entries: HashMap::new(),
                order: VecDeque::new(),
            }),
            max_entries,
        }
    }

    pub fn get(&self, key: &K) -> Option<V> {
        let mut state = self.state.lock().ok()?;
        let value = state.entries.get(key)?.clone();
        if let Some(position) = state.order.iter().position(|entry| entry == key) {
            state.order.remove(position);
        }
        state.order.push_back(key.clone());
        Some(value)
    }

    pub fn insert(&self, key: K, value: V) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if let Some(position) = state.order.iter().position(|entry| entry == &key) {
            state.order.remove(position);
        }
        state.entries.insert(key.clone(), value);
        state.order.push_back(key);
        while state.entries.len() > self.max_entries {
            let Some(evicted) = state.order.pop_front() else {
                break;
            };
            state.entries.remove(&evicted);
        }
    }

    pub fn remove(&self, key: &K) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.entries.remove(key);
        if let Some(position) = state.order.iter().position(|entry| entry == key) {
            state.order.remove(position);
        }
    }
}
