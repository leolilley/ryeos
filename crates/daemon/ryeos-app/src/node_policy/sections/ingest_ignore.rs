//! Node-owned additions to the non-bypassable ingest-ignore floor.
//!
//! Policy authors may only add exclusions. The built-in floor remains an
//! engine/state invariant and is compiled into the effective matcher here; it
//! is never copied into operator-authored policy or made removable.

use anyhow::{Context as _, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::ignore::{IgnoreConfig, IgnoreMatcher};
use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "ingest_ignore";
pub const POLICY_SCHEMA: u32 = 1;
pub const MAX_OPERATOR_PATTERNS: usize = 256;
pub const MAX_PATTERN_BYTES: usize = 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct IngestIgnorePolicyDocument {
    schema: u32,
    additional_patterns: Vec<String>,
}

/// Compiled node policy. `additional_patterns` is canonical policy identity;
/// `effective_config` and `matcher` include the immutable built-in floor.
#[derive(Debug, Clone)]
pub struct CompiledIngestIgnorePolicy {
    pub schema: u32,
    pub additional_patterns: Vec<String>,
    pub effective_config: IgnoreConfig,
    pub matcher: IgnoreMatcher,
}

pub struct IngestIgnorePolicySection;

impl TypedNodePolicy for CompiledIngestIgnorePolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

impl NodePolicySection for IngestIgnorePolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let document: IngestIgnorePolicyDocument =
            serde_json::from_value(body.clone()).context("parse ingest-ignore node policy")?;
        Ok(Arc::new(compile_policy(document)?))
    }
}

fn compile_policy(
    document: IngestIgnorePolicyDocument,
) -> anyhow::Result<CompiledIngestIgnorePolicy> {
    if document.schema != POLICY_SCHEMA {
        bail!("ingest-ignore node policy schema is not current");
    }
    if document.additional_patterns.len() > MAX_OPERATOR_PATTERNS {
        bail!("ingest-ignore node policy exceeds {MAX_OPERATOR_PATTERNS} operator patterns");
    }
    for pattern in &document.additional_patterns {
        if pattern.is_empty()
            || pattern.len() > MAX_PATTERN_BYTES
            || pattern.trim() != pattern
            || pattern.chars().any(char::is_control)
        {
            bail!("ingest-ignore operator pattern is not bounded canonical text");
        }
    }

    let additions_matcher = IgnoreMatcher::from_config(&IgnoreConfig {
        patterns: document.additional_patterns.clone(),
    })
    .context("compile ingest-ignore operator additions")?;
    let canonical_additions = additions_matcher.canonical_patterns().to_vec();
    if canonical_additions != document.additional_patterns {
        bail!("ingest-ignore operator patterns must be canonical, sorted, and unique");
    }

    let builtins = crate::ignore::matcher_from_builtins()
        .canonical_patterns()
        .to_vec();
    let builtin_set = builtins
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(duplicate) = canonical_additions
        .iter()
        .find(|pattern| builtin_set.contains(pattern.as_str()))
    {
        bail!(
            "ingest-ignore operator pattern `{duplicate}` duplicates the immutable built-in floor"
        );
    }

    let mut effective_patterns = builtins;
    effective_patterns.extend(canonical_additions.iter().cloned());
    effective_patterns.sort();
    effective_patterns.dedup();
    let effective_config = IgnoreConfig {
        patterns: effective_patterns,
    };
    let matcher = IgnoreMatcher::from_config(&effective_config)
        .context("compile effective ingest-ignore node policy")?;

    Ok(CompiledIngestIgnorePolicy {
        schema: document.schema,
        additional_patterns: canonical_additions,
        effective_config,
        matcher,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context() -> NodePolicyContext {
        NodePolicyContext {
            section: SECTION_NAME.to_owned(),
            source_file: format!("/node/policies/{SECTION_NAME}.yaml").into(),
            signer_fingerprint: "ab".repeat(32),
        }
    }

    fn parse(body: Value) -> anyhow::Result<CompiledIngestIgnorePolicy> {
        let parsed = IngestIgnorePolicySection.parse(&context(), &body)?;
        Ok(parsed
            .as_any()
            .downcast_ref::<CompiledIngestIgnorePolicy>()
            .expect("ingest-ignore compiler returned wrong type")
            .clone())
    }

    #[test]
    fn section_is_registered_policy_authority() {
        let section = IngestIgnorePolicySection;
        assert_eq!(section.name(), SECTION_NAME);
        assert!(section
            .parse(&context(), &json!({"schema": 1, "additional_patterns": []}))
            .is_ok());
    }

    #[test]
    fn compiles_builtin_floor_and_canonical_operator_additions() {
        let record = parse(json!({
            "schema": 1,
            "additional_patterns": ["*.trace", "/generated/private/"]
        }))
        .unwrap();

        assert_eq!(
            record.additional_patterns,
            vec!["*.trace".to_owned(), "/generated/private/".to_owned()]
        );
        assert!(record.matcher.is_ignored(".git/config"));
        assert!(record.matcher.is_ignored("run.trace"));
        assert!(record.matcher.is_ignored("generated/private/output.bin"));
        assert!(!record.matcher.is_ignored("src/main.rs"));
        assert_eq!(
            record.effective_config.patterns.as_slice(),
            record.matcher.canonical_patterns()
        );
    }

    #[test]
    fn explicit_empty_policy_compiles_exact_builtin_floor() {
        let record = parse(json!({"schema": 1, "additional_patterns": []})).unwrap();
        assert!(record.additional_patterns.is_empty());
        assert_eq!(
            record.matcher.canonical_patterns(),
            crate::ignore::matcher_from_builtins().canonical_patterns()
        );
    }

    #[test]
    fn rejects_unknown_shape_and_schema() {
        assert!(parse(json!({"schema": 1})).is_err());
        assert!(
            parse(json!({
                "schema": 1,
                "additional_patterns": [],
                "patterns": []
            }))
            .is_err()
        );
        assert!(parse(json!({"schema": 2, "additional_patterns": []})).is_err());
    }

    #[test]
    fn rejects_noncanonical_duplicates_and_invalid_patterns() {
        assert!(
            parse(json!({
                "schema": 1,
                "additional_patterns": ["z-output/", "a-output/"]
            }))
            .is_err()
        );
        assert!(
            parse(json!({
                "schema": 1,
                "additional_patterns": [".git/"]
            }))
            .unwrap_err()
            .to_string()
            .contains("built-in floor")
        );
        assert!(
            parse(json!({
                "schema": 1,
                "additional_patterns": ["[invalid"]
            }))
            .is_err()
        );
        assert!(
            parse(json!({
                "schema": 1,
                "additional_patterns": ["duplicate/", "duplicate/"]
            }))
            .is_err()
        );
    }

    #[test]
    fn rejects_unbounded_pattern_count_and_size() {
        let too_many = (0..=MAX_OPERATOR_PATTERNS)
            .map(|index| format!("generated-{index}/"))
            .collect::<Vec<_>>();
        assert!(parse(json!({"schema": 1, "additional_patterns": too_many})).is_err());

        let too_long = "x".repeat(MAX_PATTERN_BYTES + 1);
        assert!(parse(json!({"schema": 1, "additional_patterns": [too_long]})).is_err());
    }
}
use std::sync::Arc;
