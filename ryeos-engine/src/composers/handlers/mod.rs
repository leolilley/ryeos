//! Native composer handlers — the only `with_builtins()` left for
//! composition. Symmetric to `parsers::handlers`: handlers are
//! registered by string ID at boot, and kind schemas name a handler
//! ID via their `composer:` field. The `ComposerRegistry` is then
//! built data-drivenly by walking kind schemas (see
//! `super::ComposerRegistry::from_kinds`).

pub mod extends_chain;
pub mod graph_permissions;
pub mod identity;

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::resolution::{KindComposedView, ResolutionError, ResolvedAncestor};

pub use extends_chain::ExtendsChainComposer;
pub use graph_permissions::GraphPermissionsComposer;
pub use identity::IdentityComposer;

/// Trait implemented by per-kind composer handlers.
///
/// Mirrors `crate::parsers::handlers::NativeParserHandler` exactly:
/// `validate_config` is run at boot time so misconfigured composer
/// blocks fail loud before first compose; `compose` runs daemon-side
/// after all resolution steps have built the `ResolvedAncestor` chain.
/// The `config` blob is the kind schema's `composer_config` field
/// (already validated at boot).
pub trait KindComposer: Send + Sync {
    /// Validate `composer_config` ahead of time. Returning `Err`
    /// surfaces in `boot_validation` so a misconfigured kind fails
    /// loud at boot rather than at first compose.
    fn validate_config(&self, config: &Value) -> Result<(), String>;

    /// Run the composer. `config` is the kind schema's
    /// `composer_config`; `root_parsed` and `ancestor_parsed` are the
    /// per-item parser outputs already produced by the parser
    /// dispatcher.
    fn compose(
        &self,
        config: &Value,
        root: &ResolvedAncestor,
        root_parsed: &Value,
        ancestors: &[ResolvedAncestor],
        ancestor_parsed: &[Value],
    ) -> Result<KindComposedView, ResolutionError>;
}

/// Registry of native composer handlers indexed by handler ID
/// (e.g. `"rye/core/extends_chain"`).
///
/// Symmetric to `NativeParserHandlerRegistry`. `with_builtins()`
/// registers exactly the handlers shipped with the engine; tests can
/// use `new()` to start empty and inject mocks. The data path
/// (`ComposerRegistry::from_kinds`) walks loaded kind schemas and
/// looks up each kind's declared handler ID here.
pub struct NativeComposerHandlerRegistry {
    handlers: HashMap<String, Arc<dyn KindComposer>>,
}

impl NativeComposerHandlerRegistry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    /// Register the in-process composer handlers shipped with the
    /// engine. Symmetric to `NativeParserHandlerRegistry::with_builtins`.
    pub fn with_builtins() -> Self {
        let mut reg = Self::new();
        reg.register(extends_chain::HANDLER_ID, Arc::new(ExtendsChainComposer));
        reg.register(graph_permissions::HANDLER_ID, Arc::new(GraphPermissionsComposer));
        reg.register(identity::HANDLER_ID, Arc::new(IdentityComposer));
        reg
    }

    pub fn register(&mut self, handler_id: &str, composer: Arc<dyn KindComposer>) {
        self.handlers.insert(handler_id.to_owned(), composer);
    }

    pub fn get(&self, handler_id: &str) -> Option<Arc<dyn KindComposer>> {
        self.handlers.get(handler_id).cloned()
    }

    pub fn contains(&self, handler_id: &str) -> bool {
        self.handlers.contains_key(handler_id)
    }

    pub fn handler_ids(&self) -> impl Iterator<Item = &str> {
        self.handlers.keys().map(|s| s.as_str())
    }
}

impl Default for NativeComposerHandlerRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

impl std::fmt::Debug for NativeComposerHandlerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeComposerHandlerRegistry")
            .field("handlers", &self.handlers.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_builtins_registers_all_handlers() {
        let reg = NativeComposerHandlerRegistry::with_builtins();
        assert!(reg.contains(extends_chain::HANDLER_ID));
        assert!(reg.contains(graph_permissions::HANDLER_ID));
        assert!(reg.contains(identity::HANDLER_ID));
        assert!(reg.get(extends_chain::HANDLER_ID).is_some());
        assert!(reg.get(graph_permissions::HANDLER_ID).is_some());
        assert!(reg.get(identity::HANDLER_ID).is_some());
        assert!(reg.get("missing").is_none());
    }

    #[test]
    fn handler_ids_lists_registered() {
        let reg = NativeComposerHandlerRegistry::with_builtins();
        let mut ids: Vec<&str> = reg.handler_ids().collect();
        ids.sort();
        assert_eq!(ids, vec![
            "rye/core/extends_chain",
            "rye/core/graph_permissions",
            "rye/core/identity",
        ]);
    }
}
