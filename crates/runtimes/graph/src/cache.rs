use std::io::Read;
use std::path::PathBuf;

use serde_json::Value;

pub struct NodeCache {
    pub cache_dir: PathBuf,
}

impl NodeCache {
    pub fn new(graph_id: &str) -> Self {
        let root = std::env::var("RYEOS_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("ryeos-graph-cache"));
        Self::under_root(root, graph_id)
    }

    fn under_root(root: PathBuf, graph_id: &str) -> Self {
        // A signed graph category is identity data, not a filesystem path.
        // Hash the full logical id into one stable component so absolute paths,
        // separators and `..` can never escape the configured cache root.
        let graph_component = lillux::cas::sha256_hex(graph_id.as_bytes());
        let dir = root.join(graph_component);
        Self { cache_dir: dir }
    }

    pub fn lookup(&self, key: &str) -> Option<Value> {
        let path = self.cache_dir.join(format!("{key}.json"));
        let limit = ryeos_runtime::EvaluationLimits::default().max_result_bytes;
        let file = std::fs::File::open(&path).ok()?;
        let mut content = String::new();
        if let Err(error) = file
            .take(limit.saturating_add(1) as u64)
            .read_to_string(&mut content)
        {
            tracing::warn!(
                "cache file could not be read as bounded UTF-8 (key={key}): {error}"
            );
            return None;
        }
        if content.len() > limit {
            tracing::warn!(
                "cache file exceeds rye-expr/1 result byte limit (key={key}, limit={limit})"
            );
            return None;
        }
        serde_json::from_str(&content)
            .map_err(|e| {
                tracing::warn!("cache file contains invalid JSON (key={key}): {e}");
            })
            .ok()
    }

    pub fn store(&self, key: &str, value: &Value) {
        if let Err(e) = std::fs::create_dir_all(&self.cache_dir) {
            tracing::debug!(error = %e, "failed to create cache dir");
            return;
        }
        let path = self.cache_dir.join(format!("{key}.json"));
        if let Ok(content) = serde_json::to_string(value) {
            if let Err(e) = std::fs::write(&path, content) {
                tracing::warn!("cache write failed for {key}: {e}");
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
        let tmp = tempfile::tempdir().unwrap();
        let cache = NodeCache {
            cache_dir: tmp.path().join("test-graph"),
        };
        let key = "abc123";
        let val = json!({"stdout": "hello"});
        cache.store(key, &val);
        let retrieved = cache.lookup(key).unwrap();
        assert_eq!(retrieved, val);
    }

    #[test]
    fn cache_miss_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = NodeCache {
            cache_dir: tmp.path().join("test-graph"),
        };
        assert!(cache.lookup("nonexistent").is_none());
    }

    #[test]
    fn oversized_cache_entry_is_a_bounded_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = NodeCache {
            cache_dir: tmp.path().join("test-graph"),
        };
        std::fs::create_dir_all(&cache.cache_dir).unwrap();
        let limit = ryeos_runtime::EvaluationLimits::default().max_result_bytes;
        std::fs::write(
            cache.cache_dir.join("oversized.json"),
            vec![b' '; limit + 1],
        )
        .unwrap();

        assert!(cache.lookup("oversized").is_none());
    }

    #[test]
    fn graph_id_is_an_opaque_cache_namespace_not_a_path() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = NodeCache::under_root(tmp.path().to_path_buf(), "../../outside/graph");
        assert_eq!(cache.cache_dir.parent(), Some(tmp.path()));
        assert_ne!(cache.cache_dir, tmp.path().join("../../outside/graph"));
    }
}
