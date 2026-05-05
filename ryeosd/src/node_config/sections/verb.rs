use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_config::{NodeConfigSection, SectionRecord, SectionSourcePolicy};

/// A parsed verb definition loaded from `.ai/node/verbs/<name>.yaml`.
///
/// Verbs are security-canonical: their name appears in capability strings
/// (`rye.<verb>.<kind>.<subject>`), and their `execute` field defines what
/// runs when the verb is dispatched. Token routing (CLI aliases) lives in
/// the separate `aliases` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerbRecord {
    /// Must be `"verbs"`. Matches parent directory, enforced by node kind schema.
    pub category: String,
    /// Must be `"verbs"`. Matches parent directory, enforced by loader.
    pub section: String,
    /// Verb name. Must match filename stem, enforced by section handler.
    /// Syntax: `^[a-z][a-z0-9-]*$`
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Canonical ref to execute when this verb is dispatched.
    /// `None` for abstract verbs like `execute` (generic dispatcher).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execute: Option<String>,
    /// Path to the YAML file that declared this record. Set by loader.
    #[serde(skip)]
    pub source_file: std::path::PathBuf,
}

pub struct VerbSection;

impl NodeConfigSection for VerbSection {
    /// Verbs define capability gates. Bundles can contribute
    /// verbs (they're routing, not privilege escalation — without `implies`,
    /// each verb is independent and appears explicitly in capability strings).
    fn source_policy(&self) -> SectionSourcePolicy {
        SectionSourcePolicy::EffectiveBundleRootsAndState
    }

    fn parse(&self, name: &str, body: &Value) -> Result<Box<dyn SectionRecord>> {
        let record: VerbRecord = serde_json::from_value(body.clone())
            .context("failed to parse verb record")?;

        // Name must match filename
        if record.name != name {
            bail!(
                "verb record declares name '{}' but filename is '{}'",
                record.name,
                name
            );
        }

        // Section must be "verbs"
        if record.section != "verbs" {
            bail!(
                "verb record declares section '{}' but must be 'verbs'",
                record.section
            );
        }

        // Category must be "verbs"
        if record.category != "verbs" {
            bail!(
                "verb record declares category '{}' but must be 'verbs'",
                record.category
            );
        }

        // Validate verb name syntax
        validate_verb_name(&record.name)?;

        // Validate execute ref if present
        if let Some(ref execute_ref) = record.execute {
            ryeos_engine::canonical_ref::CanonicalRef::parse(execute_ref).with_context(|| {
                format!("invalid execute ref '{}' in verb record", execute_ref)
            })?;
        }

        Ok(Box::new(record))
    }
}

impl SectionRecord for VerbRecord {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Validate that a verb name matches `^[a-z][a-z0-9-]*$`.
fn validate_verb_name(name: &str) -> Result<()> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new("^[a-z][a-z0-9-]*$").unwrap());

    if !re.is_match(name) {
        bail!(
            "invalid verb name '{}': must match ^[a-z][a-z0-9-]*$",
            name
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_body() -> Value {
        serde_json::json!({
            "category": "verbs",
            "section": "verbs",
            "name": "sign",
            "description": "Sign an item",
            "execute": "tool:rye/core/sign"
        })
    }

    #[test]
    fn valid_verb_parses() {
        let section = VerbSection;
        let result = section.parse("sign", &valid_body());
        assert!(result.is_ok());
        let boxed = result.unwrap();
        let record = boxed.as_any().downcast_ref::<VerbRecord>().unwrap();
        assert_eq!(record.name, "sign");
        assert_eq!(record.execute.as_deref(), Some("tool:rye/core/sign"));
    }

    #[test]
    fn abstract_verb_no_execute() {
        let section = VerbSection;
        let body = serde_json::json!({
            "category": "verbs",
            "section": "verbs",
            "name": "execute",
            "description": "Execute an item"
        });
        let result = section.parse("execute", &body);
        assert!(result.is_ok());
        let binding = result.unwrap();
        let record = binding.as_any().downcast_ref::<VerbRecord>().unwrap();
        assert!(record.execute.is_none());
    }

    #[test]
    fn name_mismatch_rejected() {
        let section = VerbSection;
        let result = section.parse("fetch", &valid_body());
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("declares name 'sign' but filename is 'fetch'"), "got: {msg}");
    }

    #[test]
    fn wrong_section_rejected() {
        let section = VerbSection;
        let mut body = valid_body();
        body["section"] = serde_json::json!("routes");
        let result = section.parse("sign", &body);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("must be 'verbs'"), "got: {msg}");
    }

    #[test]
    fn wrong_category_rejected() {
        let section = VerbSection;
        let mut body = valid_body();
        body["category"] = serde_json::json!("routes");
        let result = section.parse("sign", &body);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("must be 'verbs'"), "got: {msg}");
    }

    #[test]
    fn unknown_field_rejected() {
        let section = VerbSection;
        let mut body = valid_body();
        body["bogus"] = serde_json::json!("nope");
        let result = section.parse("sign", &body);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("unknown field"), "got: {msg}");
    }

    #[test]
    fn invalid_execute_ref_rejected() {
        let section = VerbSection;
        let mut body = valid_body();
        body["execute"] = serde_json::json!("not-a-valid-ref");
        let result = section.parse("sign", &body);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("invalid execute ref"), "got: {msg}");
    }

    #[test]
    fn invalid_verb_name_rejected() {
        let result = validate_verb_name("EXECUTE");
        assert!(result.is_err());
        let result = validate_verb_name("has space");
        assert!(result.is_err());
        let result = validate_verb_name("123");
        assert!(result.is_err());
        let result = validate_verb_name("a_b");
        assert!(result.is_err());
    }

    #[test]
    fn valid_verb_names_accepted() {
        assert!(validate_verb_name("execute").is_ok());
        assert!(validate_verb_name("fetch").is_ok());
        assert!(validate_verb_name("sign").is_ok());
        assert!(validate_verb_name("bundle-install").is_ok());
        assert!(validate_verb_name("a").is_ok());
    }

    #[test]
    fn source_policy_is_effective_bundle_roots_and_state() {
        let section = VerbSection;
        assert_eq!(section.source_policy(), SectionSourcePolicy::EffectiveBundleRootsAndState);
    }
}
