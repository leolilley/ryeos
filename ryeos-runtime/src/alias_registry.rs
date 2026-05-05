//! Alias registry — surface-aware routing sugar, decoupled from security.
//!
//! Maps `(surface, tokens)` to verb names. Each surface (cli, api, etc.)
//! defines its own token vocabulary. Pure UX/routing convenience: can be
//! deprecated, renamed, or extended freely without touching authorization.
//!
//! The alias registry is validated at startup against the verb registry:
//! every alias must reference a known verb. This catches configuration
//! drift early (fail-closed).

use std::collections::{BTreeMap, BTreeSet};

/// A single alias definition: a `(surface, tokens)` pair that routes to a verb.
#[derive(Debug, Clone)]
pub struct AliasDef {
    /// Which surface this alias is for (e.g. "cli").
    pub surface: String,
    /// Token sequence that triggers this alias (e.g. `["sign"]` or `["bundle", "install"]`).
    pub tokens: Vec<String>,
    /// Verb name this alias routes to (must exist in `VerbRegistry`).
    pub verb: String,
    /// Whether this alias is deprecated. Deprecated aliases still resolve
    /// but callers should warn.
    pub deprecated: bool,
    /// If deprecated, the suggested replacement token sequence.
    pub replacement_tokens: Option<Vec<String>>,
    /// If deprecated, the version in which this alias will be removed.
    pub removed_in: Option<String>,
}

/// Composite key for the alias index: (surface, tokens).
type AliasKey = (String, Vec<String>);

/// Registry of aliases (routing convenience).
///
/// Keyed by `(surface, tokens)` so different surfaces can have
/// independent vocabularies for the same verbs.
#[derive(Debug, Clone)]
pub struct AliasRegistry {
    /// `(surface, tokens)` → alias definition.
    alias_index: BTreeMap<AliasKey, AliasDef>,
    /// Set of `(surface, tokens)` that are deprecated.
    deprecated: BTreeSet<AliasKey>,
}

/// Errors from alias registry operations.
#[derive(Debug, thiserror::Error)]
pub enum AliasRegistryError {
    #[error("duplicate alias (surface={surface}, tokens={tokens:?}): first routes to '{first}', second routes to '{second}'")]
    DuplicateAlias {
        surface: String,
        tokens: Vec<String>,
        first: String,
        second: String,
    },
    #[error("alias (surface={surface}, tokens={tokens:?}) references unknown verb '{verb}'")]
    UnknownVerb {
        surface: String,
        tokens: Vec<String>,
        verb: String,
    },
}

impl AliasRegistry {
    /// Build the alias registry from alias definitions.
    ///
    /// Validates that `(surface, tokens)` pairs are unique. Verb existence
    /// validation is done separately by the daemon startup (which
    /// has access to both registries).
    pub fn from_records(records: &[AliasDef]) -> Result<Self, AliasRegistryError> {
        let mut alias_index: BTreeMap<AliasKey, AliasDef> = BTreeMap::new();
        let mut deprecated: BTreeSet<AliasKey> = BTreeSet::new();

        for def in records {
            let key = (def.surface.clone(), def.tokens.clone());

            if let Some(existing) = alias_index.get(&key) {
                return Err(AliasRegistryError::DuplicateAlias {
                    surface: def.surface.clone(),
                    tokens: def.tokens.clone(),
                    first: existing.verb.clone(),
                    second: def.verb.clone(),
                });
            }

            if def.deprecated {
                deprecated.insert(key.clone());
            }

            alias_index.insert(key, def.clone());
        }

        Ok(Self {
            alias_index,
            deprecated,
        })
    }

    /// Resolve a token sequence on a specific surface to a verb name.
    ///
    /// Returns `None` if no alias matches on that surface.
    pub fn resolve_tokens(&self, surface: &str, tokens: &[String]) -> Option<&str> {
        let key = (surface.to_string(), tokens.to_vec());
        self.alias_index.get(&key).map(|def| def.verb.as_str())
    }

    /// Resolve a token sequence to an alias definition.
    pub fn get_alias(&self, surface: &str, tokens: &[String]) -> Option<&AliasDef> {
        let key = (surface.to_string(), tokens.to_vec());
        self.alias_index.get(&key)
    }

    /// Match an argv against aliases on a surface using longest-prefix matching.
    ///
    /// Tries from longest to shortest prefix. Returns `(verb_name, tokens_consumed)`.
    /// E.g. argv `["bundle", "install", "extra"]` on surface "cli" matches
    /// `["bundle", "install"]` → `("bundle-install", 2)`.
    pub fn match_argv(&self, surface: &str, argv: &[String]) -> Option<(String, usize)> {
        for len in (1..=argv.len()).rev() {
            let prefix = &argv[0..len];
            if let Some(verb) = self.resolve_tokens(surface, prefix) {
                return Some((verb.to_string(), len));
            }
        }
        None
    }

    /// Check if a token sequence is deprecated on a surface.
    pub fn is_deprecated(&self, surface: &str, tokens: &[String]) -> bool {
        let key = (surface.to_string(), tokens.to_vec());
        self.deprecated.contains(&key)
    }

    /// Get all aliases for a given verb on a surface.
    pub fn aliases_for_verb(&self, surface: &str, verb: &str) -> Vec<&AliasDef> {
        self.alias_index
            .values()
            .filter(|def| def.surface == surface && def.verb == verb)
            .collect()
    }

    /// Return all alias definitions across all surfaces.
    pub fn all_aliases(&self) -> Vec<&AliasDef> {
        self.alias_index.values().collect()
    }

    /// Return all non-deprecated alias definitions.
    pub fn active_aliases(&self) -> Vec<&AliasDef> {
        self.alias_index
            .values()
            .filter(|def| !def.deprecated)
            .collect()
    }

    /// Validate that every alias references a verb that exists in the given registry.
    ///
    /// Called once at daemon startup after both registries are built.
    /// Returns the first unknown-verb alias as a structured error.
    pub fn validate_all_verbs_known(
        &self,
        verb_registry: &crate::verb_registry::VerbRegistry,
    ) -> Result<(), AliasRegistryError> {
        for def in self.alias_index.values() {
            if !verb_registry.has_verb(&def.verb) {
                return Err(AliasRegistryError::UnknownVerb {
                    surface: def.surface.clone(),
                    tokens: def.tokens.clone(),
                    verb: def.verb.clone(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_alias_defs() -> Vec<AliasDef> {
        vec![
            AliasDef {
                surface: "cli".into(),
                tokens: vec!["sign".into()],
                verb: "sign".into(),
                deprecated: false,
                replacement_tokens: None,
                removed_in: None,
            },
            AliasDef {
                surface: "cli".into(),
                tokens: vec!["s".into()],
                verb: "sign".into(),
                deprecated: false,
                replacement_tokens: None,
                removed_in: None,
            },
            AliasDef {
                surface: "cli".into(),
                tokens: vec!["fetch".into()],
                verb: "fetch".into(),
                deprecated: false,
                replacement_tokens: None,
                removed_in: None,
            },
            AliasDef {
                surface: "cli".into(),
                tokens: vec!["f".into()],
                verb: "fetch".into(),
                deprecated: false,
                replacement_tokens: None,
                removed_in: None,
            },
            AliasDef {
                surface: "cli".into(),
                tokens: vec!["bundle".into(), "install".into()],
                verb: "bundle-install".into(),
                deprecated: false,
                replacement_tokens: None,
                removed_in: None,
            },
            AliasDef {
                surface: "cli".into(),
                tokens: vec!["sig".into()],
                verb: "sign".into(),
                deprecated: true,
                replacement_tokens: Some(vec!["sign".into()]),
                removed_in: Some("0.4.0".into()),
            },
        ]
    }

    fn test_registry() -> AliasRegistry {
        AliasRegistry::from_records(&test_alias_defs()).unwrap()
    }

    #[test]
    fn resolve_single_token() {
        let reg = test_registry();
        assert_eq!(reg.resolve_tokens("cli", &["sign".to_string()]), Some("sign"));
        assert_eq!(reg.resolve_tokens("cli", &["fetch".to_string()]), Some("fetch"));
    }

    #[test]
    fn resolve_short_form() {
        let reg = test_registry();
        assert_eq!(reg.resolve_tokens("cli", &["s".to_string()]), Some("sign"));
        assert_eq!(reg.resolve_tokens("cli", &["f".to_string()]), Some("fetch"));
    }

    #[test]
    fn resolve_multi_token() {
        let reg = test_registry();
        assert_eq!(
            reg.resolve_tokens("cli", &["bundle".to_string(), "install".to_string()]),
            Some("bundle-install")
        );
    }

    #[test]
    fn resolve_unknown_returns_none() {
        let reg = test_registry();
        assert_eq!(reg.resolve_tokens("cli", &["nonexistent".to_string()]), None);
    }

    #[test]
    fn resolve_wrong_surface_returns_none() {
        let reg = test_registry();
        assert_eq!(reg.resolve_tokens("api", &["sign".to_string()]), None);
    }

    #[test]
    fn match_argv_longest_prefix() {
        let reg = test_registry();
        let (verb, consumed) = reg.match_argv(
            "cli",
            &["bundle".to_string(), "install".to_string(), "extra".to_string()],
        ).unwrap();
        assert_eq!(verb, "bundle-install");
        assert_eq!(consumed, 2);
    }

    #[test]
    fn match_argv_single_token() {
        let reg = test_registry();
        let (verb, consumed) = reg.match_argv(
            "cli",
            &["sign".to_string(), "extra".to_string()],
        ).unwrap();
        assert_eq!(verb, "sign");
        assert_eq!(consumed, 1);
    }

    #[test]
    fn match_argv_no_match_returns_none() {
        let reg = test_registry();
        assert_eq!(reg.match_argv("cli", &["xyz".to_string()]), None);
    }

    #[test]
    fn match_argv_exact_match() {
        let reg = test_registry();
        let (verb, consumed) = reg.match_argv(
            "cli",
            &["bundle".to_string(), "install".to_string()],
        ).unwrap();
        assert_eq!(verb, "bundle-install");
        assert_eq!(consumed, 2);
    }

    #[test]
    fn deprecated_alias_still_resolves() {
        let reg = test_registry();
        assert_eq!(reg.resolve_tokens("cli", &["sig".to_string()]), Some("sign"));
        assert!(reg.is_deprecated("cli", &["sig".to_string()]));
    }

    #[test]
    fn non_deprecated_not_flagged() {
        let reg = test_registry();
        assert!(!reg.is_deprecated("cli", &["sign".to_string()]));
    }

    #[test]
    fn aliases_for_verb_on_surface() {
        let reg = test_registry();
        let mut names: Vec<&str> = reg.aliases_for_verb("cli", "sign")
            .iter()
            .map(|a| a.tokens[0].as_str())
            .collect();
        names.sort();
        assert_eq!(names, vec!["s", "sig", "sign"]);
    }

    #[test]
    fn aliases_for_verb_wrong_surface_empty() {
        let reg = test_registry();
        assert!(reg.aliases_for_verb("api", "sign").is_empty());
    }

    #[test]
    fn active_aliases_excludes_deprecated() {
        let reg = test_registry();
        let active = reg.active_aliases();
        let deprecated_count = active.iter().filter(|a| a.deprecated).count();
        assert_eq!(deprecated_count, 0);
        // 6 total, 1 deprecated → 5 active
        assert_eq!(active.len(), 5);
    }

    #[test]
    fn duplicate_tokens_same_surface_error() {
        let result = AliasRegistry::from_records(&[
            AliasDef {
                surface: "cli".into(),
                tokens: vec!["sign".into()],
                verb: "sign".into(),
                deprecated: false,
                replacement_tokens: None,
                removed_in: None,
            },
            AliasDef {
                surface: "cli".into(),
                tokens: vec!["sign".into()],
                verb: "sign-alt".into(),
                deprecated: false,
                replacement_tokens: None,
                removed_in: None,
            },
        ]);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("duplicate alias"), "got: {msg}");
    }

    #[test]
    fn same_tokens_different_surface_ok() {
        let result = AliasRegistry::from_records(&[
            AliasDef {
                surface: "cli".into(),
                tokens: vec!["sign".into()],
                verb: "sign".into(),
                deprecated: false,
                replacement_tokens: None,
                removed_in: None,
            },
            AliasDef {
                surface: "api".into(),
                tokens: vec!["sign".into()],
                verb: "sign".into(),
                deprecated: false,
                replacement_tokens: None,
                removed_in: None,
            },
        ]);
        assert!(result.is_ok());
    }

    #[test]
    fn empty_records_ok() {
        let reg = AliasRegistry::from_records(&[]).unwrap();
        assert!(reg.all_aliases().is_empty());
    }

    #[test]
    fn get_alias_returns_def() {
        let reg = test_registry();
        let alias = reg.get_alias("cli", &["sig".to_string()]).unwrap();
        assert_eq!(alias.surface, "cli");
        assert_eq!(alias.verb, "sign");
        assert!(alias.deprecated);
        assert_eq!(alias.replacement_tokens, Some(vec!["sign".to_string()]));
        assert_eq!(alias.removed_in.as_deref(), Some("0.4.0"));
    }

    #[test]
    fn validate_all_verbs_known_passes() {
        use crate::verb_registry::{VerbDef, VerbRegistry};
        let reg = test_registry();
        let verbs = VerbRegistry::from_records(&[
            VerbDef { name: "sign".into(), execute: None },
            VerbDef { name: "fetch".into(), execute: None },
            VerbDef { name: "bundle-install".into(), execute: None },
        ]).unwrap();
        assert!(reg.validate_all_verbs_known(&verbs).is_ok());
    }

    #[test]
    fn validate_all_verbs_known_rejects_unknown() {
        use crate::verb_registry::{VerbDef, VerbRegistry};
        let reg = test_registry();
        // No verbs registered — all aliases should fail
        let verbs = VerbRegistry::from_records(&[]).unwrap();
        let result = reg.validate_all_verbs_known(&verbs);
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("unknown verb"), "got: {msg}");
    }
}
