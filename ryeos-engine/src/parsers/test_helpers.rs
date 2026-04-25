//! Test-only seam for building a `ParserDispatcher` without going
//! through disk I/O. Production code MUST load `ParserRegistry` via
//! `ParserRegistry::load_base` from real bundle YAML on disk.
//!
//! Configs intentionally mirror the live bundle
//! (`ryeos-bundles/core/.ai/parsers/...`) so unit tests don't drift
//! from production semantics. This module is `pub(crate)` and gated
//! `#[cfg(test)]` — no out-of-crate caller can reach it.

use serde_json::json;

use super::descriptor::ParserDescriptor;
use super::dispatcher::ParserDispatcher;

fn mk(executor_id: &str, parser_config: serde_json::Value) -> ParserDescriptor {
    ParserDescriptor {
        version: "1.0.0".into(),
        category: None,
        description: None,
        executor_id: executor_id.into(),
        parser_api_version: 1,
        parser_config,
        output_schema: crate::contracts::ValueShape::any_mapping(),
    }
}

/// All five canonical bundle parser descriptors:
///   * `parser:rye/core/python/ast`
///   * `parser:rye/core/yaml/yaml`
///   * `parser:rye/core/markdown/directive`
///   * `parser:rye/core/markdown/frontmatter`
///   * `parser:rye/core/javascript/javascript`
pub(crate) fn dispatcher_with_canonical_bundle_descriptors() -> ParserDispatcher {
    let entries = vec![
        (
            "parser:rye/core/python/ast".to_string(),
            mk(
                "native:parser_regex_kv",
                json!({
                    "patterns": [{
                        "regex": r#"(?m)^(__\w+__)\s*=\s*"([^"]+)""#,
                        "key_group": 1,
                        "value_group": 2
                    }]
                }),
            ),
        ),
        (
            "parser:rye/core/yaml/yaml".to_string(),
            mk(
                "native:parser_yaml_document",
                json!({ "require_mapping": true }),
            ),
        ),
        (
            "parser:rye/core/markdown/directive".to_string(),
            mk(
                "native:parser_yaml_header_document",
                json!({
                    "require_header": true,
                    "body_field": "body",
                    "forms": [
                        { "kind": "frontmatter", "delimiter": "---" },
                        { "kind": "fenced_block", "language": "yaml" }
                    ]
                }),
            ),
        ),
        (
            "parser:rye/core/markdown/frontmatter".to_string(),
            mk(
                "native:parser_yaml_header_document",
                json!({
                    "require_header": false,
                    "body_field": null,
                    "forms": [
                        { "kind": "fenced_block", "language": "yaml" }
                    ]
                }),
            ),
        ),
        (
            "parser:rye/core/javascript/javascript".to_string(),
            mk(
                "native:parser_regex_kv",
                json!({
                    "patterns": [{
                        "regex": r#"(?m)^const\s+(__\w+__)\s*=\s*"([^"]+)""#,
                        "key_group": 1,
                        "value_group": 2
                    }]
                }),
            ),
        ),
    ];
    ParserDispatcher::from_descriptors(entries)
}
