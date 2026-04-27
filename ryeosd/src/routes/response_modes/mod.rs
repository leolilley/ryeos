pub mod event_stream_mode;
pub mod static_mode;

use std::sync::Arc;

use crate::routes::compile::ResponseMode;

pub struct ResponseModeRegistry {
    modes: Vec<Arc<dyn ResponseMode>>,
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
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_register_static_and_event_stream() {
        let r = ResponseModeRegistry::with_builtins();
        assert!(r.get("static").is_some());
        assert!(r.get("event_stream").is_some());
        assert!(r.get("tool_response").is_none());
    }

    #[test]
    #[should_panic(expected = "duplicate mode")]
    fn duplicate_registration_panics() {
        let mut r = ResponseModeRegistry::new();
        r.register(Arc::new(static_mode::StaticMode));
        r.register(Arc::new(static_mode::StaticMode));
    }
}
