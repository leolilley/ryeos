//! Strict parser descriptor — what a `parser` kind YAML deserializes to.
//!
//! Parsers are their own kind. The kind identity is implicit in where
//! the file lives (under the `parser` kind's `location.directory`,
//! typically `.ai/parsers/ryeos/core/...`) — there is no discriminator
//! field on the descriptor. The boot-time `ParserRegistry` loader uses
//! the raw signed-YAML loader (same shape as the `KindRegistry`'s
//! loader) so the cycle of "you need a parser to load parsers" is
//! broken at the bootstrap layer.
//!
//! `parser_api_version` pins to `1` for now; bumping it is a deliberate
//! breaking change that will require descriptor authors to opt in.

use serde::{Deserialize, Serialize};

use crate::contracts::ValueShape;

/// Signed parser-result caching contract. Disabled is the compatibility and
/// safety default: a parser must opt in explicitly before results can outlive
/// one handler invocation.
///
/// `content_addressed` asserts that the handler is deterministic and has no
/// externally visible side effects for an exact parser configuration,
/// signature-stripped input, and source-path string. The dispatcher binds all
/// of those inputs, plus the verified parser/handler registry fingerprint, in
/// its cache key.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ParserCachePolicy {
    #[default]
    Disabled,
    ContentAddressed,
}

impl ParserCachePolicy {
    pub fn is_disabled(&self) -> bool {
        matches!(self, Self::Disabled)
    }

    pub fn is_content_addressed(&self) -> bool {
        matches!(self, Self::ContentAddressed)
    }
}

/// Strictly typed parser descriptor (top-level fields of a parser
/// kind YAML).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParserDescriptor {
    pub version: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Canonical handler ref, e.g. `"handler:ryeos/core/yaml-document"`.
    pub handler: String,
    pub parser_api_version: u32,
    /// Opaque-to-the-engine config blob; the native handler validates
    /// and consumes it.
    #[serde(default)]
    pub parser_config: serde_json::Value,
    /// Successful parse results may be shared only when this signed policy
    /// explicitly opts into content-addressed deterministic caching.
    #[serde(default, skip_serializing_if = "ParserCachePolicy::is_disabled")]
    pub cache: ParserCachePolicy,
    /// Lower-bound declared shape of this parser's output `Value`.
    /// Required. The boot validator checks this shape for
    /// compatibility/no-contradiction with each consuming kind's final
    /// `composed_value_contract`; concrete descriptor instances are
    /// still validated by preflight and post-composition checks.
    pub output_schema: ValueShape,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor_yaml(cache: &str) -> String {
        format!(
            r#"
version: "1.0.0"
handler: "handler:test/parser"
parser_api_version: 1
parser_config: {{}}
{cache}output_schema:
  root_type: mapping
  required: {{}}
"#
        )
    }

    #[test]
    fn omitted_cache_policy_is_disabled() {
        let descriptor: ParserDescriptor = serde_yaml::from_str(&descriptor_yaml("")).unwrap();
        assert_eq!(descriptor.cache, ParserCachePolicy::Disabled);
    }

    #[test]
    fn content_addressed_cache_policy_is_explicit() {
        let descriptor: ParserDescriptor =
            serde_yaml::from_str(&descriptor_yaml("cache:\n  mode: content_addressed\n")).unwrap();
        assert_eq!(descriptor.cache, ParserCachePolicy::ContentAddressed);
    }

    #[test]
    fn unknown_cache_policy_is_rejected() {
        let error = serde_yaml::from_str::<ParserDescriptor>(&descriptor_yaml(
            "cache:\n  mode: best_effort\n",
        ))
        .unwrap_err();
        assert!(error.to_string().contains("unknown variant"));
    }
}
