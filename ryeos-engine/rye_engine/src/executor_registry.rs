use std::collections::HashMap;

use crate::contracts::ItemMetadata;

/// Subprocess dispatch configuration for items that run as child processes.
#[derive(Debug, Clone)]
pub struct SubprocessDispatch {
    /// Optional interpreter command, e.g. `"python3"`, `"node"`
    pub interpreter: Option<String>,
}

/// Registry mapping executor IDs to subprocess dispatch configs.
///
/// The engine consults this during chain building. The executor ID comes
/// from item metadata or the kind locator's default. The engine branches
/// on executor ID, never on item kind.
#[derive(Debug)]
pub struct ExecutorRegistry {
    executors: HashMap<String, SubprocessDispatch>,
}

impl ExecutorRegistry {
    pub fn new() -> Self {
        Self {
            executors: HashMap::new(),
        }
    }

    /// Register a subprocess dispatch config.
    pub fn register(&mut self, executor_id: &str, config: SubprocessDispatch) {
        self.executors.insert(executor_id.to_owned(), config);
    }

    /// Look up an executor by ID.
    pub fn get(&self, executor_id: &str) -> Option<&SubprocessDispatch> {
        self.executors.get(executor_id)
    }

    /// Check whether an executor ID is registered.
    pub fn contains(&self, executor_id: &str) -> bool {
        self.executors.contains_key(executor_id)
    }

    /// Resolve the effective executor ID for an item.
    ///
    /// Priority:
    ///   1. Explicit `executor_id` from item metadata
    ///   2. Provided `default_executor_id` from the kind locator
    ///
    /// Returns `None` if neither provides an executor ID (the item
    /// is not directly executable, e.g. knowledge/config).
    pub fn resolve_executor_id(
        &self,
        metadata: &ItemMetadata,
        default_executor_id: Option<&str>,
    ) -> Option<String> {
        metadata
            .executor_id
            .clone()
            .or_else(|| default_executor_id.map(|s| s.to_owned()))
    }
}

impl Default for ExecutorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_lookup_subprocess() {
        let mut reg = ExecutorRegistry::new();
        reg.register(
            "@primitive_chain",
            SubprocessDispatch {
                interpreter: Some("python3".into()),
            },
        );

        assert!(reg.contains("@primitive_chain"));
        assert!(!reg.contains("nonexistent"));
    }

    #[test]
    fn resolve_executor_id_from_metadata() {
        let reg = ExecutorRegistry::new();
        let mut meta = ItemMetadata::default();
        meta.executor_id = Some("custom:my_executor".into());

        let resolved = reg.resolve_executor_id(&meta, Some("native:directive_orchestrator"));
        assert_eq!(resolved.as_deref(), Some("custom:my_executor"));
    }

    #[test]
    fn resolve_executor_id_falls_back_to_default() {
        let reg = ExecutorRegistry::new();
        let meta = ItemMetadata::default();

        let resolved = reg.resolve_executor_id(&meta, Some("native:directive_orchestrator"));
        assert_eq!(resolved.as_deref(), Some("native:directive_orchestrator"));
    }

    #[test]
    fn resolve_executor_id_none_when_no_metadata_or_default() {
        let reg = ExecutorRegistry::new();
        let meta = ItemMetadata::default();

        let resolved = reg.resolve_executor_id(&meta, None);
        assert!(resolved.is_none());
    }
}
