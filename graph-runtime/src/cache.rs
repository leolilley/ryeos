use std::path::PathBuf;

use serde_json::Value;

pub struct NodeCache {
    pub cache_dir: PathBuf,
}

impl NodeCache {
    pub fn new(graph_id: &str) -> Self {
        let dir = std::env::var("RYE_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("rye-graph-cache"))
            .join(graph_id);
        Self { cache_dir: dir }
    }

    pub fn lookup(&self, key: &str) -> Option<Value> {
        let path = self.cache_dir.join(format!("{key}.json"));
        let content = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    pub fn store(&self, key: &str, value: &Value) {
        if let Err(e) = std::fs::create_dir_all(&self.cache_dir) {
            tracing::debug!(error = %e, "failed to create cache dir");
            return;
        }
        let path = self.cache_dir.join(format!("{key}.json"));
        if let Ok(content) = serde_json::to_string(value) {
            let _ = std::fs::write(&path, content);
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
}
