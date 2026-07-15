use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{Map, Value};

struct CacheEntries {
    values: HashMap<String, Value>,
    budget: ryeos_runtime::RuntimeJsonObjectBudget,
}

/// One execution's private node-result cache.
///
/// Cache entries are replay authority: a hit skips dispatch and therefore
/// skips billing. Keeping them in the graph process makes that authority
/// ephemeral and non-forgeable by project or host filesystem writes. A fresh
/// cache is created for every `Walker::execute`, so graph, tool, and overlay
/// changes can never inherit a prior run's result.
pub struct NodeCache {
    entries: Mutex<CacheEntries>,
}

impl NodeCache {
    pub fn new(_graph_id: &str) -> Self {
        Self {
            entries: Mutex::new(CacheEntries {
                values: HashMap::new(),
                budget: ryeos_runtime::RuntimeJsonObjectBudget::from_object(
                    &Map::new(),
                    "graph node cache",
                )
                .expect("empty graph node cache must fit rye-expr/1 limits"),
            }),
        }
    }

    pub fn lookup(&self, key: &str) -> Option<Value> {
        match self.entries.lock() {
            Ok(entries) => entries.values.get(key).cloned(),
            Err(error) => {
                tracing::error!(%error, "graph node cache lock poisoned");
                None
            }
        }
    }

    pub fn store(&self, key: &str, value: &Value) {
        if let Err(error) =
            crate::evaluation::validate_runtime_value(value, "graph node cache value")
        {
            tracing::warn!(%error, "refusing to cache an out-of-bounds graph node result");
            return;
        }
        match self.entries.lock() {
            Ok(mut entries) => {
                if let Err(error) = entries.budget.replace(key, value) {
                    tracing::warn!(%error, "refusing to exceed aggregate graph node cache bounds");
                    return;
                }
                entries.values.insert(key.to_string(), value.clone());
            }
            Err(error) => {
                tracing::error!(%error, "graph node cache lock poisoned");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cache_round_trip() {
        let cache = NodeCache::new("test-graph");
        let key = "abc123";
        let value = json!({"stdout": "hello"});

        cache.store(key, &value);

        assert_eq!(cache.lookup(key), Some(value));
    }

    #[test]
    fn cache_miss_returns_none() {
        let cache = NodeCache::new("test-graph");
        assert!(cache.lookup("nonexistent").is_none());
    }

    #[test]
    fn cache_is_execution_local() {
        let first = NodeCache::new("same-graph");
        let second = NodeCache::new("same-graph");
        first.store("key", &json!({"value": 1}));

        assert_eq!(first.lookup("key"), Some(json!({"value": 1})));
        assert!(second.lookup("key").is_none());
    }

    #[test]
    fn oversized_cache_value_is_not_retained() {
        let cache = NodeCache::new("test-graph");
        let limit = ryeos_runtime::EvaluationLimits::default().max_result_bytes;
        cache.store("oversized", &Value::String("x".repeat(limit + 1)));

        assert!(cache.lookup("oversized").is_none());
    }

    #[test]
    fn aggregate_cache_size_is_bounded() {
        let cache = NodeCache::new("test-graph");
        let value = Value::String("x".repeat(700 * 1024));
        for index in 0..8 {
            cache.store(&format!("key-{index}"), &value);
        }

        assert!(cache.lookup("key-0").is_some());
        assert!(cache.lookup("key-7").is_none());
    }
}
