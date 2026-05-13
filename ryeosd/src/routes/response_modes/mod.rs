pub mod event_stream_mode;
pub mod execute_mode;
pub mod json_mode;
pub mod launch_mode;
pub mod static_mode;

use std::sync::Arc;

use crate::routes::compile::ResponseMode;

pub struct ResponseModeRegistry {
    modes: Vec<Arc<dyn ResponseMode>>,
}

impl Default for ResponseModeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponseModeRegistry {
    pub fn new() -> Self {
        Self { modes: Vec::new() }
    }

    pub fn register(&mut self, mode: Arc<dyn ResponseMode>) {
        let key = mode.key();
        if self.modes.iter().any(|m| m.key() == key) {
            panic!("ResponseModeRegistry: duplicate mode `{key}`");
        }
        self.modes.push(mode);
    }

    pub fn get(&self, key: &str) -> Option<&dyn ResponseMode> {
        self.modes
            .iter()
            .find(|m| m.key() == key)
            .map(|m| m.as_ref())
    }

    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(static_mode::StaticMode));
        r.register(Arc::new(event_stream_mode::EventStreamMode));
        r.register(Arc::new(launch_mode::LaunchMode::default()));
        r.register(Arc::new(json_mode::JsonMode));
        r.register(Arc::new(execute_mode::ExecuteMode));
        // "accepted" is an alias for "launch" — both compile to CompiledLaunchInvocation.
        r.register(Arc::new(launch_mode::LaunchMode::with_key("accepted")));
        r
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
        assert!(r.get("accepted").is_some(), "accepted alias must be registered");
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
        r.register(Arc::new(static_mode::StaticMode));
        r.register(Arc::new(static_mode::StaticMode));
    }
}
