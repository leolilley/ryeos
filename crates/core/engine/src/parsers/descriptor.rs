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
    /// Declared shape of this parser's output `Value`. Required.
    /// The boot validator checks every consuming kind's
    /// `composed_value_contract` against this shape; making it
    /// mandatory removes the silent-skip class of bugs where a parser
    /// without a declaration would never be type-checked.
    pub output_schema: ValueShape,
}
