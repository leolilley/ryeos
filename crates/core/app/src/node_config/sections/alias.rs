use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_config::{NodeConfigSection, SectionRecord, SectionSourcePolicy};

/// A parsed alias definition loaded from `.ai/node/aliases/<name>.yaml`.
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
    /// Path to the YAML file that declared this record. Set by loader.
    #[serde(skip)]
    pub source_file: PathBuf,
}

pub struct AliasSection;

impl NodeConfigSection for AliasSection {
    /// Aliases define routing convenience. Bundles can contribute
    /// aliases (they're pure routing, no security implications).
    fn source_policy(&self) -> SectionSourcePolicy {
        SectionSourcePolicy::EffectiveBundleRootsAndState
    }

    fn parse(&self, name: &str, body: &Value) -> Result<Box<dyn SectionRecord>> {
        let record: AliasRecord =
            serde_json::from_value(body.clone()).context("failed to parse alias record")?;

        // Section must be "aliases"
        if record.section != "aliases" {
            bail!(
                "alias record declares section '{}' but must be 'aliases'",
                record.section
            );
        }

        // Category must be "aliases"
        if record.category != "aliases" {
            bail!(
                "alias record declares category '{}' but must be 'aliases'",
                record.category
            );
        }

        // Tokens must be non-empty
        if record.tokens.is_empty() {
            bail!("alias '{}' has empty tokens list", name);
        }

        // Empty / dash-prefixed token checks apply to every token —
        // they would break tokeniser dispatch anywhere in the path.
        for token in &record.tokens {
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
        if let Some(first) = record.tokens.first() {
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

        Ok(Box::new(record))
    }
}

impl SectionRecord for AliasRecord {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_body() -> Value {
        serde_json::json!({
            "category": "aliases",
            "section": "aliases",
            "tokens": ["sign"],
            "verb": "sign",
            "description": "Sign an item"
        })
    }

    #[test]
    fn valid_alias_parses() {
        let section = AliasSection;
        let result = section.parse("sign", &valid_body());
        assert!(result.is_ok());
        let boxed = result.unwrap();
        let record = boxed.as_any().downcast_ref::<AliasRecord>().unwrap();
        assert_eq!(record.tokens, vec!["sign"]);
        assert_eq!(record.verb, "sign");
    }

    #[test]
    fn multi_token_alias_parses() {
        let section = AliasSection;
        let body = serde_json::json!({
            "category": "aliases",
            "section": "aliases",
            "tokens": ["bundle", "install"],
            "verb": "bundle-install",
            "description": "Install a bundle"
        });
        let result = section.parse("bundle-install", &body);
        assert!(result.is_ok());
        let boxed = result.unwrap();
        let record = boxed.as_any().downcast_ref::<AliasRecord>().unwrap();
        assert_eq!(record.tokens, vec!["bundle", "install"]);
        assert_eq!(record.verb, "bundle-install");
    }

    #[test]
    fn deprecated_alias_parses() {
        let section = AliasSection;
        let body = serde_json::json!({
            "category": "aliases",
            "section": "aliases",
            "tokens": ["sig"],
            "verb": "sign",
            "description": "Sign (deprecated)",
            "deprecated": true,
            "replacement_tokens": ["sign"],
            "removed_in": "0.4.0"
        });
        let result = section.parse("sig", &body);
        assert!(result.is_ok());
        let binding = result.unwrap();
        let record = binding.as_any().downcast_ref::<AliasRecord>().unwrap();
        assert_eq!(record.deprecated, Some(true));
        assert_eq!(record.replacement_tokens, Some(vec!["sign".to_string()]));
        assert_eq!(record.removed_in.as_deref(), Some("0.4.0"));
    }

    #[test]
    fn wrong_section_rejected() {
        let section = AliasSection;
        let mut body = valid_body();
        body["section"] = serde_json::json!("verbs");
        let result = section.parse("sign", &body);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("must be 'aliases'"), "got: {msg}");
    }

    #[test]
    fn wrong_category_rejected() {
        let section = AliasSection;
        let mut body = valid_body();
        body["category"] = serde_json::json!("verbs");
        let result = section.parse("sign", &body);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("must be 'aliases'"), "got: {msg}");
    }

    #[test]
    fn surface_field_rejected_as_unknown() {
        // `surface` was removed — it must be rejected by deny_unknown_fields.
        let section = AliasSection;
        let mut body = valid_body();
        body["surface"] = serde_json::json!("cli");
        let result = section.parse("sign", &body);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("unknown field"), "got: {msg}");
    }

    #[test]
    fn empty_tokens_rejected() {
        let section = AliasSection;
        let mut body = valid_body();
        body["tokens"] = serde_json::json!([]);
        let result = section.parse("sign", &body);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("empty tokens list"), "got: {msg}");
    }

    #[test]
    fn reserved_first_token_init_rejected() {
        let section = AliasSection;
        let mut body = valid_body();
        body["tokens"] = serde_json::json!(["init"]);
        let result = section.parse("init-alias", &body);
        assert!(result.is_err());
        assert!(format!("{:#}", result.unwrap_err()).contains("reserved first token 'init'"));
    }

    #[test]
    fn reserved_first_token_help_rejected() {
        let section = AliasSection;
        let mut body = valid_body();
        body["tokens"] = serde_json::json!(["help"]);
        let result = section.parse("help-alias", &body);
        assert!(result.is_err());
        assert!(format!("{:#}", result.unwrap_err()).contains("reserved first token 'help'"));
    }

    #[test]
    fn reserved_first_token_execute_rejected() {
        let section = AliasSection;
        let mut body = valid_body();
        body["tokens"] = serde_json::json!(["execute"]);
        let result = section.parse("execute-alias", &body);
        assert!(result.is_err());
        assert!(format!("{:#}", result.unwrap_err()).contains("reserved first token 'execute'"));
    }

    /// Regression: reserved tokens are only collisions as the FIRST
    /// token. `["remote", "execute"]` is fine because the CLI
    /// dispatcher's local escape hatch only fires when `cli.rest[0]`
    /// is `"execute"`.
    #[test]
    fn reserved_token_as_subtoken_accepted() {
        let section = AliasSection;
        let mut body = valid_body();
        body["tokens"] = serde_json::json!(["remote", "execute"]);
        section
            .parse("remote-execute", &body)
            .expect("'execute' as a sub-token must be accepted");

        body["tokens"] = serde_json::json!(["foo", "help"]);
        section
            .parse("foo-help", &body)
            .expect("'help' as a sub-token must be accepted");

        body["tokens"] = serde_json::json!(["something", "init"]);
        section
            .parse("something-init", &body)
            .expect("'init' as a sub-token must be accepted");
    }

    #[test]
    fn unknown_field_rejected() {
        let section = AliasSection;
        let mut body = valid_body();
        body["bogus"] = serde_json::json!("nope");
        let result = section.parse("sign", &body);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("unknown field"), "got: {msg}");
    }

    #[test]
    fn source_policy_is_effective_bundle_roots_and_state() {
        let section = AliasSection;
        assert_eq!(
            section.source_policy(),
            SectionSourcePolicy::EffectiveBundleRootsAndState
        );
    }
}
