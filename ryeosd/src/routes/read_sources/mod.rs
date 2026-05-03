pub mod thread_detail;

use std::sync::Arc;

use serde_json::Value;

use crate::dispatch_error::RouteConfigError;
use crate::routes::raw::RawRouteSpec;
use crate::state::AppState;

/// Compile-time: validates route YAML config and returns a bound read source.
///
/// Mirrors `StreamingSource` — each implementation is a named plugin that
/// the `read` response mode delegates to.
pub trait ReadSource: Send + Sync {
    fn key(&self) -> &'static str;

    fn compile(
        &self,
        raw_route: &RawRouteSpec,
        source_config: &Value,
    ) -> Result<Arc<dyn BoundReadSource>, RouteConfigError>;
}

/// Runtime: produces a JSON response from path captures + app state.
///
/// Returns `Ok(Some(value))` for 200, `Ok(None)` for 404.
#[axum::async_trait]
pub trait BoundReadSource: Send + Sync {
    async fn fetch(
        &self,
        captures: &std::collections::HashMap<String, String>,
        state: &AppState,
    ) -> Result<Option<Value>, crate::dispatch_error::RouteDispatchError>;
}

pub struct ReadSourceRegistry {
    sources: Vec<Arc<dyn ReadSource>>,
}

impl ReadSourceRegistry {
    pub fn new() -> Self {
        Self {
            sources: Vec::new(),
        }
    }

    pub fn register(&mut self, source: Arc<dyn ReadSource>) {
        let key = source.key();
        if self.sources.iter().any(|s| s.key() == key) {
            panic!("ReadSourceRegistry: duplicate source `{key}`");
        }
        self.sources.push(source);
    }

    pub fn get(&self, key: &str) -> Option<&dyn ReadSource> {
        self.sources
            .iter()
            .find(|s| s.key() == key)
            .map(|s| s.as_ref())
    }

    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(thread_detail::ThreadDetailSource));
        r
    }
}

impl Default for ReadSourceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_register_thread_detail() {
        let r = ReadSourceRegistry::with_builtins();
        assert!(r.get("thread_detail").is_some());
        assert!(r.get("nonexistent").is_none());
    }

    #[test]
    #[should_panic(expected = "duplicate source")]
    fn duplicate_registration_panics() {
        let mut r = ReadSourceRegistry::new();
        r.register(Arc::new(thread_detail::ThreadDetailSource));
        r.register(Arc::new(thread_detail::ThreadDetailSource));
    }
}
