pub mod event_stream_mode;
pub mod execute_mode;
pub mod json_mode;
pub mod launch_mode;
pub mod static_mode;

use std::sync::Arc;

use crate::routes::compile::ResponseMode;

pub struct ResponseModeRegistry {
    static_mode: static_mode::StaticMode,
    event_stream_mode: event_stream_mode::EventStreamMode,
    modes: Vec<Arc<dyn ResponseMode>>,
}

impl Default for ResponseModeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponseModeRegistry {
    pub fn new() -> Self {
        Self {
            static_mode: static_mode::StaticMode::default(),
            event_stream_mode: event_stream_mode::EventStreamMode::default(),
            modes: Vec::new(),
        }
    }

    pub fn register(&mut self, mode: Arc<dyn ResponseMode>) {
        let key = mode.key();
        if key == self.static_mode.key() || key == self.event_stream_mode.key() {
            panic!("ResponseModeRegistry: duplicate mode `{key}`");
        }
        if self.modes.iter().any(|m| m.key() == key) {
            panic!("ResponseModeRegistry: duplicate mode `{key}`");
        }
        self.modes.push(mode);
    }

    pub fn get(&self, key: &str) -> Option<&dyn ResponseMode> {
        if key == self.static_mode.key() {
            return Some(&self.static_mode);
        }
        if key == self.event_stream_mode.key() {
            return Some(&self.event_stream_mode);
        }
        self.modes
            .iter()
            .find(|m| m.key() == key)
            .map(|m| m.as_ref())
    }

    /// Build a registry with API-only builtins (no UI extensions).
    pub fn with_api_builtins_from(
        service_descriptors: &'static [crate::registry::ServiceDescriptor],
    ) -> Self {
        let mut r = Self::new();
        r.register(Arc::new(launch_mode::LaunchMode::default()));
        r.register(Arc::new(json_mode::JsonMode {
            service_descriptors,
        }));
        r.register(Arc::new(execute_mode::ExecuteMode));
        r.register(Arc::new(launch_mode::LaunchMode::with_key("accepted")));
        r
    }

    /// Build a registry with API-only builtins using default descriptors.
    pub fn with_builtins() -> Self {
        Self::with_api_builtins_from(crate::handlers::ALL)
    }

    /// Register an additional stream source in the event_stream response mode.
    pub fn register_event_stream_source(
        &mut self,
        name: impl Into<String>,
        compiler: std::sync::Arc<dyn event_stream_mode::StreamSourceCompiler>,
    ) {
        self.event_stream_mode.register_source(name, compiler);
    }

    /// Register a static asset provider for the given source name.
    pub fn set_static_asset_provider(
        &mut self,
        source_name: impl Into<String>,
        provider: Arc<dyn static_mode::StaticAssetProvider>,
    ) {
        self.static_mode.register_provider(source_name, provider);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_register_static_event_stream_and_launch() {
        let r = ResponseModeRegistry::with_builtins();
        assert!(r.get("static").is_some());
        assert!(r.get("event_stream").is_some());
        assert!(r.get("launch").is_some());
        assert!(r.get("json").is_some(), "json mode must be registered");
        assert!(
            r.get("accepted").is_some(),
            "accepted alias must be registered"
        );
        // Unknown modes never silently resolve.
        assert!(r.get("nonexistent_mode").is_none());
    }

    #[test]
    fn accepted_alias_compiles_same_as_launch() {
        let r = ResponseModeRegistry::with_builtins();
        let accepted = r.get("accepted").expect("accepted must exist");
        let launch = r.get("launch").expect("launch must exist");
        // Both resolve to the same compile logic (same key family).
        assert_eq!(accepted.key(), "accepted");
        assert_eq!(launch.key(), "launch");
    }

    #[test]
    #[should_panic(expected = "duplicate mode")]
    fn duplicate_registration_panics() {
        let mut r = ResponseModeRegistry::new();
        r.register(Arc::new(static_mode::StaticMode::default()));
    }
}
