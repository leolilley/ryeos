use std::path::PathBuf;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProjectResolution {
    #[default]
    None,
    Required,
    Optional,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PositionalMatcher {
    #[default]
    Any,
    CanonicalRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PositionalSlot {
    pub field: String,
    #[serde(default)]
    pub matcher: PositionalMatcher,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PositionalForm {
    #[serde(default)]
    pub slots: Vec<PositionalSlot>,
}

/// An alias definition synthesized from a verb descriptor's `aliases` field.
///
/// Aliases are routing sugar: `tokens` maps to a verb name. They have no
/// security implications — authorization is handled by the verb registry.
/// Aliases can be deprecated, renamed, or extended freely without touching
/// authorization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AliasRecord {
    /// Must be `"aliases"`. Matches parent directory, enforced by node kind schema.
    pub category: String,
    /// Must be `"aliases"`. Matches parent directory, enforced by loader.
    pub section: String,
    /// Token sequence that triggers this alias (e.g. `["sign"]` or `["bundle", "install"]`).
    pub tokens: Vec<String>,
    /// Verb name this alias routes to (must exist in `VerbRegistry`).
    pub verb: String,
    /// Human-readable description of what this alias does.
    pub description: String,
    /// Whether this alias is deprecated. Deprecated aliases still resolve
    /// but callers should warn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<bool>,
    /// If deprecated, the suggested replacement token sequence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement_tokens: Option<Vec<String>>,
    /// If deprecated, the version in which this alias will be removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed_in: Option<String>,
    /// If present, declares that a lone positional argument in the
    /// tail (e.g. the `<item_ref>` in `ryeos execute <item_ref>`)
    /// should be bound to this field name in the parameters object
    /// rather than collected into `_args`. The handler that backs the
    /// verb must accept this field name. See `arg_binder::bind_argv_with_positional_field`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub positional_field: Option<String>,
    /// Ordered alternative positional forms. Replaces one-off CLI
    /// command shims with data-driven tail binding.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub positional_forms: Vec<PositionalForm>,
    /// CLI project canonicalisation policy for this alias.
    #[serde(default)]
    pub project_resolution: ProjectResolution,
    /// Path to the YAML file that declared this record. Set by loader.
    #[serde(skip)]
    pub source_file: PathBuf,
}

pub(crate) fn validate_alias_tokens(name: &str, tokens: &[String]) -> Result<()> {
    // Tokens must be non-empty
    if tokens.is_empty() {
        bail!("alias '{}' has empty tokens list", name);
    }

    // Empty / dash-prefixed token checks apply to every token —
    // they would break tokeniser dispatch anywhere in the path.
    for token in tokens {
        if token.is_empty() {
            bail!("alias '{}' has empty token in tokens list", name);
        }
        if token.starts_with('-') {
            bail!("alias '{}' has dash-prefixed token '{}'", name, token);
        }
    }
    // The local-only reservations (`help`, `init`, `execute`)
    // only collide as the FIRST token — that's where the CLI
    // dispatcher short-circuits before talking to the daemon.
    // `remote execute` (where `execute` is a sub-token) is
    // unambiguous because the dispatcher looks at `cli.rest[0]`.
    if let Some(first) = tokens.first() {
        if first == "help" {
            bail!("alias '{}' uses reserved first token 'help'", name);
        }
        if first == "init" {
            bail!(
                "alias '{}' uses reserved first token 'init' (local-only)",
                name
            );
        }
        if first == "execute" {
            bail!(
                "alias '{}' uses reserved first token 'execute' \
                 (local escape hatch, uses item_ref directly)",
                name
            );
        }
    }

    Ok(())
}

pub(crate) fn validate_positional_forms(name: &str, forms: &[PositionalForm]) -> Result<()> {
    for (form_idx, form) in forms.iter().enumerate() {
        if form.slots.is_empty() {
            bail!("alias '{}' positional_forms[{form_idx}] has no slots", name);
        }
        for (slot_idx, slot) in form.slots.iter().enumerate() {
            if slot.field.trim().is_empty() {
                bail!(
                    "alias '{}' positional_forms[{form_idx}].slots[{slot_idx}] has empty field",
                    name
                );
            }
        }
    }

    Ok(())
}
