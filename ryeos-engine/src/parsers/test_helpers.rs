//! Test-only seam for building a `ParserDispatcher` without going
//! through descriptor disk I/O. Production code MUST load
//! `ParserRegistry` via `ParserRegistry::load_base` from real bundle
//! YAML on disk.
//!
//! Configs intentionally mirror the live bundle
//! (`ryeos-bundles/core/.ai/parsers/...`) so unit tests don't drift
//! from production semantics. The `HandlerRegistry` is loaded from
//! `ryeos-bundles/core/` so dispatch routes through the real signed
//! handler binaries — there is NO native-handler fallback in the
//! dispatcher and NO empty-registry shortcut here.

use serde_json::json;

use super::descriptor::ParserDescriptor;
use super::dispatcher::ParserDispatcher;

fn mk(handler: &str, parser_config: serde_json::Value) -> ParserDescriptor {
    ParserDescriptor {
        version: "1.0.0".into(),
        category: None,
        description: None,
        handler: handler.into(),
        parser_api_version: 1,
        parser_config,
        output_schema: crate::contracts::ValueShape::any_mapping(),
    }
}

/// All five canonical bundle parser descriptors:
///   * `parser:ryeos/core/python/ast`
///   * `parser:ryeos/core/yaml/yaml`
///   * `parser:ryeos/core/markdown/directive`
///   * `parser:ryeos/core/markdown/frontmatter`
///   * `parser:ryeos/core/javascript/javascript`
pub(crate) fn dispatcher_with_canonical_bundle_descriptors() -> ParserDispatcher {
    let entries = vec![
        (
            "parser:ryeos/core/python/ast".to_string(),
            mk(
                "handler:ryeos/core/regex-kv",
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
            "parser:ryeos/core/yaml/yaml".to_string(),
            mk(
                "handler:ryeos/core/yaml-document",
                json!({ "require_mapping": true }),
            ),
        ),
        (
            "parser:ryeos/core/markdown/directive".to_string(),
            mk(
                "handler:ryeos/core/yaml-header-document",
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
            "parser:ryeos/core/markdown/frontmatter".to_string(),
            mk(
                "handler:ryeos/core/yaml-header-document",
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
            "parser:ryeos/core/javascript/javascript".to_string(),
            mk(
                "handler:ryeos/core/regex-kv",
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
    crate::test_support::build_parser_dispatcher_from_roots(entries)
}
