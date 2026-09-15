//! Complete node-owned project ingest-ignore policy.
//!
//! Conventional project exclusions are signed node data, not engine defaults.
//! RyeOS-owned identity, state, cache, and transaction paths are protected by
//! the separate non-bypassable project snapshot floor.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::ignore::{IgnoreConfig, IgnoreMatcher};
use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "ingest_ignore";
pub const POLICY_SCHEMA: u32 = 2;
pub const MAX_PATTERNS: usize = 256;
pub const MAX_PATTERN_BYTES: usize = 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct IngestIgnorePolicyDocument {
    schema: u32,
    patterns: Vec<String>,
}

/// Exact matcher compiled from one signed policy generation.
#[derive(Debug, Clone)]
pub struct CompiledIngestIgnorePolicy {
    pub schema: u32,
    pub patterns: Vec<String>,
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
    if document.patterns.len() > MAX_PATTERNS {
        bail!("ingest-ignore node policy exceeds {MAX_PATTERNS} patterns");
    }
    for pattern in &document.patterns {
        if pattern.is_empty()
            || pattern.len() > MAX_PATTERN_BYTES
            || pattern.trim() != pattern
            || pattern.chars().any(char::is_control)
        {
            bail!("ingest-ignore pattern is not bounded canonical text");
        }
    }

    let matcher = IgnoreMatcher::from_config(&IgnoreConfig {
        patterns: document.patterns.clone(),
    })
    .context("compile ingest-ignore node policy")?;
    let patterns = matcher.canonical_patterns().to_vec();
    if patterns != document.patterns {
        bail!("ingest-ignore patterns must be canonical, sorted, and unique");
    }

    Ok(CompiledIngestIgnorePolicy {
        schema: document.schema,
        patterns,
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
        assert!(
            section
                .parse(&context(), &json!({"schema": 2, "patterns": []}))
                .is_ok()
        );
    }

    #[test]
    fn compiles_exact_canonical_policy() {
        let record = parse(json!({
            "schema": 2,
            "patterns": ["*.trace", ".git/", "/generated/private/"]
        }))
        .unwrap();

        assert_eq!(
            record.patterns,
            vec![
                "*.trace".to_owned(),
                ".git/".to_owned(),
                "/generated/private/".to_owned()
            ]
        );
        assert!(record.matcher.is_ignored(".git/config"));
        assert!(record.matcher.is_ignored("run.trace"));
        assert!(record.matcher.is_ignored("generated/private/output.bin"));
        assert!(!record.matcher.is_ignored("src/main.rs"));
    }

    #[test]
    fn empty_policy_has_no_implicit_conventional_exclusions() {
        let record = parse(json!({"schema": 2, "patterns": []})).unwrap();
        assert!(record.patterns.is_empty());
        assert!(record.matcher.canonical_patterns().is_empty());
        assert!(!record.matcher.is_ignored(".git/config"));
    }

    #[test]
    fn rejects_predecessor_and_unknown_shapes() {
        assert!(parse(json!({"schema": 2})).is_err());
        assert!(
            parse(json!({
                "schema": 2,
                "patterns": [],
                "additional_patterns": []
            }))
            .is_err()
        );
        assert!(parse(json!({"schema": 1, "additional_patterns": []})).is_err());
    }

    #[test]
    fn rejects_noncanonical_duplicates_and_invalid_patterns() {
        assert!(
            parse(json!({
                "schema": 2,
                "patterns": ["z-output/", "a-output/"]
            }))
            .is_err()
        );
        assert!(
            parse(json!({
                "schema": 2,
                "patterns": ["[invalid"]
            }))
            .is_err()
        );
        assert!(
            parse(json!({
                "schema": 2,
                "patterns": ["duplicate/", "duplicate/"]
            }))
            .is_err()
        );
    }

    #[test]
    fn rejects_unbounded_pattern_count_and_size() {
        let too_many = (0..=MAX_PATTERNS)
            .map(|index| format!("generated-{index}/"))
            .collect::<Vec<_>>();
        assert!(parse(json!({"schema": 2, "patterns": too_many})).is_err());

        let too_long = "x".repeat(MAX_PATTERN_BYTES + 1);
        assert!(parse(json!({"schema": 2, "patterns": [too_long]})).is_err());
    }
}
