//! `ExecutionParamsHandler` — claims the top-level `execution_params`
//! block on a tool/runtime item.
//!
//! `execution_params` is a per-chain-element typed list of keys this
//! element accepts as execution overrides (filtered against
//! `config_resolve`'s resolved config; see `config_resolve.rs`).
//!
//! This handler exists purely to type-validate the block at the
//! `ValidateInput` phase via the shared `parse_execution_params`
//! helper. It **does not store** the parsed list:
//! `ConfigResolveHandler` continues to read the raw value from
//! `intermediate.parsed["execution_params"]` (also via the shared
//! helper, so consumer + validator can never drift). By the time
//! `ResolveContext` runs, this validator has already enforced the
//! shape (typed `Vec<String>` — anything that isn't a string array
//! fails loud here, not silently in the consumer).
//!
//! Phase note: chosen as `ValidateInput` rather than `ResolveContext`
//! because alphabetical key order would otherwise put
//! `config_resolve` before `execution_params` within the same phase,
//! letting the consumer run before the validator.
//!
//! Cardinality: `All` — each chain element may declare its own list.

use std::path::Path;

use serde_json::Value;

use crate::error::EngineError;
use crate::runtime::{CompileContext, RuntimeHandler};

pub const KEY: &str = "execution_params";

/// Strict typed parser shared by the validator and `ConfigResolveHandler`.
///
/// Accepts only `Vec<String>`. A bare string (e.g. the classic typo
/// `execution_params: "max_steps"`), a map, or a list of non-strings
/// all surface as `EngineError::InvalidRuntimeConfig`.
///
/// `source_path` is used purely for error reporting.
pub fn parse_execution_params(
    block: &Value,
    source_path: &Path,
) -> Result<Vec<String>, EngineError> {
    serde_json::from_value::<Vec<String>>(block.clone()).map_err(|e| {
        EngineError::InvalidRuntimeConfig {
            path: source_path.display().to_string(),
            reason: format!(
                "invalid execution_params: expected list of strings (e.g. \
                 `[\"max_steps\"]`); got error: {e}"
            ),
        }
    })
}

pub struct ExecutionParamsHandler;

impl RuntimeHandler for ExecutionParamsHandler {
    fn key(&self) -> &'static str {
        KEY
    }

    fn phase(&self) -> crate::runtime::HandlerPhase {
        crate::runtime::HandlerPhase::ValidateInput
    }

    fn cardinality(&self) -> crate::runtime::HandlerCardinality {
        crate::runtime::HandlerCardinality::All
    }

    fn apply(&self, block: &Value, ctx: &mut CompileContext<'_>) -> Result<(), EngineError> {
        let intermediate = &ctx.chain[ctx.current_index];
        let _ = parse_execution_params(block, &intermediate.source_path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_resolution::ResolutionRoots;
    use crate::kind_registry::KindRegistry;
    use crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors;
    use crate::runtime::{ChainIntermediate, SpecOverrides, TemplateContext};
    use crate::trust::TrustStore;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::PathBuf;

    static NULL_PARAMS: Value = Value::Null;

    fn run(block: Value) -> Result<(), EngineError> {
        let chain = vec![ChainIntermediate {
            executor_id: "mytool".into(),
            resolved_ref: "tool:mytool".into(),
            kind: "tool".into(),
            source_path: PathBuf::from("/tmp/fake.yaml"),
            parsed: json!({ "execution_params": block.clone() }),
        }];
        let parsers = dispatcher_with_canonical_bundle_descriptors();
        let kinds = KindRegistry::empty();
        let trust = TrustStore::empty();
        let roots = ResolutionRoots { ordered: vec![] };
        let mut ctx = CompileContext {
            template_ctx: TemplateContext::new(PathBuf::from("/dev/null")),
            env: HashMap::new(),
            spec_overrides: SpecOverrides::default(),
            params: Value::Null,
            original_params: &NULL_PARAMS,
            chain: &chain,
            current_index: 0,
            roots: &roots,
            parsers: &parsers,
            kinds: &kinds,
            trust_store: &trust,
            project_root: None,
        };
        ExecutionParamsHandler.apply(&block, &mut ctx)
    }

    #[test]
    fn valid_string_list_passes() {
        run(json!(["max_steps", "max_concurrency"])).expect("valid list should pass");
    }

    #[test]
    fn empty_list_passes() {
        run(json!([])).expect("empty list should pass");
    }

    #[test]
    fn bare_string_typo_fails_loud() {
        // The classic typo: `execution_params: "max_steps"` instead
        // of `execution_params: ["max_steps"]`.
        let err = run(json!("max_steps")).unwrap_err();
        assert!(
            matches!(err, EngineError::InvalidRuntimeConfig { .. }),
            "expected InvalidRuntimeConfig, got {err:?}"
        );
    }

    #[test]
    fn list_of_non_strings_fails_loud() {
        let err = run(json!([1, 2, 3])).unwrap_err();
        assert!(
            matches!(err, EngineError::InvalidRuntimeConfig { .. }),
            "expected InvalidRuntimeConfig, got {err:?}"
        );
    }

    #[test]
    fn map_fails_loud() {
        let err = run(json!({ "max_steps": true })).unwrap_err();
        assert!(
            matches!(err, EngineError::InvalidRuntimeConfig { .. }),
            "expected InvalidRuntimeConfig, got {err:?}"
        );
    }
}
